//! Dead-agent triage: detection, LLM-based assessment, and verdict application.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::io::{Read as IoRead, Seek, SeekFrom};
use std::path::Path;
use std::process;

use workgraph::agency;
use workgraph::config::Config;
use workgraph::graph::{evaluate_cycle_iteration, parse_token_usage, LogEntry, Status, Task};
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};

use crate::commands::is_process_alive;

/// Reason an agent was detected as dead
enum DeadReason {
    /// Process is no longer running
    ProcessExited,
}

/// Check if an agent should be considered dead
fn detect_dead_reason(agent: &AgentEntry) -> Option<DeadReason> {
    if !agent.is_alive() {
        return None;
    }

    // Process not running is the only signal — heartbeat is no longer used for detection
    if !is_process_alive(agent.pid) {
        return Some(DeadReason::ProcessExited);
    }

    None
}

/// Clean up dead agents (process exited)
/// Returns list of cleaned up agent IDs
pub(crate) fn cleanup_dead_agents(dir: &Path, graph_path: &Path) -> Result<Vec<String>> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Find agents that are dead: process gone
    let dead: Vec<_> = locked_registry
        .agents
        .values()
        .filter_map(|a| {
            detect_dead_reason(a).map(|reason| {
                (
                    a.id.clone(),
                    a.task_id.clone(),
                    a.pid,
                    a.output_file.clone(),
                    reason,
                )
            })
        })
        .collect();

    // Auto-bump heartbeat for agents whose process is still alive
    for agent in locked_registry.agents.values_mut() {
        if agent.is_alive() && is_process_alive(agent.pid) {
            agent.last_heartbeat = Utc::now().to_rfc3339();
        }
    }

    if dead.is_empty() {
        locked_registry.save_ref()?;
        return Ok(vec![]);
    }

    // Mark these agents as dead in registry
    for (agent_id, _, _, _, _) in &dead {
        if let Some(agent) = locked_registry.get_agent_mut(agent_id) {
            agent.status = AgentStatus::Dead;
        }
    }
    locked_registry.save_ref()?;

    // Load config for triage settings
    let config = Config::load_or_default(dir);

    // Unclaim their tasks (if still in progress - agent may have completed or failed them already)
    let mut graph = load_graph(graph_path).context("Failed to load graph")?;
    let mut tasks_modified = false;
    let mut tasks_completed_by_triage: Vec<String> = Vec::new();

    for (agent_id, task_id, pid, output_file, reason) in &dead {
        if let Some(task) = graph.get_task_mut(task_id) {
            // Only unclaim if task is still in progress (agent didn't finish it properly)
            if task.status == Status::InProgress {
                if config.agency.auto_triage {
                    // Run synchronous triage to assess progress
                    match run_triage(&config, task, output_file) {
                        Ok(verdict) => {
                            let is_done = verdict.verdict == "done";
                            apply_triage_verdict(task, &verdict, agent_id, *pid);
                            eprintln!(
                                "[coordinator] Triage for '{}': verdict={}, reason={}",
                                task_id, verdict.verdict, verdict.reason
                            );
                            if is_done && task.status == Status::Done {
                                tasks_completed_by_triage.push(task_id.clone());
                            }
                        }
                        Err(e) => {
                            // Triage failed, fall back to restart behavior
                            eprintln!(
                                "[coordinator] Triage failed for '{}': {}, falling back to restart",
                                task_id, e
                            );
                            task.status = Status::Open;
                            task.assigned = None;
                            task.log.push(LogEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                actor: Some("triage".to_string()),
                                message: format!(
                                    "Triage failed ({}), task reset: agent '{}' (PID {}) process exited",
                                    e, agent_id, pid
                                ),
                            });
                        }
                    }
                } else {
                    // Existing behavior: simple unclaim
                    task.status = Status::Open;
                    task.assigned = None;
                    let reason_msg = match reason {
                        DeadReason::ProcessExited => format!(
                            "Task unclaimed: agent '{}' (PID {}) process exited",
                            agent_id, pid
                        ),
                    };
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: None,
                        message: reason_msg,
                    });
                }
                tasks_modified = true;
            }
        }
    }

    // Extract token usage from output.log for dead agents' tasks
    for (_agent_id, task_id, _pid, output_file, _reason) in &dead {
        if let Some(task) = graph.get_task_mut(task_id) {
            if task.token_usage.is_none() {
                let output_path = std::path::Path::new(output_file);
                let abs_path = if output_path.is_absolute() {
                    output_path.to_path_buf()
                } else {
                    dir.parent().unwrap_or(dir).join(output_path)
                };
                if let Some(usage) = parse_token_usage(&abs_path) {
                    task.token_usage = Some(usage);
                    tasks_modified = true;
                }
            }
        }
    }

    // Evaluate structural cycle iterations for tasks triaged as done
    if !tasks_completed_by_triage.is_empty() {
        let cycle_analysis = graph.compute_cycle_analysis();
        for task_id in &tasks_completed_by_triage {
            evaluate_cycle_iteration(&mut graph, task_id, &cycle_analysis);
        }
    }

    if tasks_modified {
        save_graph(&graph, graph_path).context("Failed to save graph")?;
    }

    // Capture output for completed/failed tasks whose agents just died.
    // done.rs already captures output, but fail.rs does not,
    // and the agent may have completed without triggering capture (e.g. wrapper
    // script marked it done but output capture wasn't invoked). This is a
    // best-effort safety net.
    let graph = load_graph(graph_path).context("Failed to reload graph for output capture")?;
    for (_agent_id, task_id, _pid, _output_file, _reason) in &dead {
        if let Some(task) = graph.get_task(task_id)
            && matches!(task.status, Status::Done | Status::Failed)
        {
            let output_dir = dir.join("output").join(task_id);
            if !output_dir.exists() {
                if let Err(e) = agency::capture_task_output(dir, task) {
                    eprintln!(
                        "[coordinator] Warning: output capture failed for '{}': {}",
                        task_id, e
                    );
                } else {
                    eprintln!(
                        "[coordinator] Captured output for completed task '{}'",
                        task_id
                    );
                }
            }
        }
    }

    Ok(dead.into_iter().map(|(id, _, _, _, _)| id).collect())
}

// ---------------------------------------------------------------------------
// Dead-agent triage
// ---------------------------------------------------------------------------

/// Triage verdict returned by the LLM
#[derive(Debug, serde::Deserialize)]
struct TriageVerdict {
    /// One of "done", "continue", "restart"
    verdict: String,
    /// Brief explanation of the verdict
    #[serde(default)]
    reason: String,
    /// Summary of work accomplished (used for "continue" context)
    #[serde(default)]
    summary: String,
}

/// Read the last `max_bytes` of a file, prepending a truncation notice if needed.
fn read_truncated_log(path: &str, max_bytes: usize) -> String {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return "(output log not found or unreadable)".to_string(),
    };

    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(_) => return "(could not read output log metadata)".to_string(),
    };

    let file_size = metadata.len() as usize;
    if file_size == 0 {
        return "(output log is empty)".to_string();
    }

    let mut file = file;
    if file_size <= max_bytes {
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_ok() {
            return buf;
        }
        return "(could not read output log)".to_string();
    }

    // Seek to file_size - max_bytes and read from there
    let skip = file_size - max_bytes;
    if file.seek(SeekFrom::Start(skip as u64)).is_err() {
        return "(could not seek in output log)".to_string();
    }
    let mut buf = vec![0u8; max_bytes];
    match file.read_exact(&mut buf) {
        Ok(_) => {
            // Find the first newline after the seek point to avoid partial lines
            let start = buf
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[start..]).to_string();
            format!("[... {} bytes truncated ...]\n{}", skip + start, text)
        }
        Err(_) => "(could not read output log tail)".to_string(),
    }
}

/// Build the triage prompt for the LLM.
fn build_triage_prompt(task: &Task, log_content: &str) -> String {
    let task_title = &task.title;
    let task_desc = task.description.as_deref().unwrap_or("(no description)");
    let task_id = &task.id;

    format!(
        r#"You are a triage system for a software development task coordinator.

An agent was working on a task but its process died unexpectedly (OOM, crash, SIGKILL).
Examine the agent's output log below and determine how much progress was made.

## Task Information
- **ID:** {task_id}
- **Title:** {task_title}
- **Description:** {task_desc}

## Agent Output Log
```
{log_content}
```

## Instructions
Based on the output log, respond with ONLY a JSON object (no markdown fences, no commentary):

{{
  "verdict": "<done|continue|restart>",
  "reason": "<one-sentence explanation>",
  "summary": "<what was accomplished, including specific files changed or artifacts produced>"
}}

Verdicts:
- **"done"**: The work appears complete — code was written, tests pass, the agent just didn't call the completion command before dying.
- **"continue"**: Significant progress was made (files created/modified, partial implementation) — a new agent should pick up where this one left off.
- **"restart"**: Little or no meaningful progress — a fresh start is appropriate.

Be conservative: only use "done" if the output clearly shows the task was finished. When in doubt between "continue" and "restart", prefer "continue" if any artifacts were created."#
    )
}

/// Run the triage LLM call synchronously. Returns a parsed TriageVerdict.
fn run_triage(config: &Config, task: &Task, output_file: &str) -> Result<TriageVerdict> {
    let max_log_bytes = config.agency.triage_max_log_bytes.unwrap_or(50_000);
    let timeout_secs = config.agency.triage_timeout.unwrap_or(30);
    let model = config.agency.triage_model.as_deref().unwrap_or("haiku");

    let log_content = read_truncated_log(output_file, max_log_bytes);
    let prompt = build_triage_prompt(task, &log_content);

    // Use `timeout` to wrap the claude call
    let output = process::Command::new("timeout")
        .arg(format!("{}s", timeout_secs))
        .arg("claude")
        .arg("--model")
        .arg(model)
        .arg("--print")
        .arg("--dangerously-skip-permissions")
        .arg(&prompt)
        .output()
        .context("Failed to run claude CLI for triage")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Triage claude call failed (exit {:?}): {}",
            output.status.code(),
            stderr.chars().take(200).collect::<String>()
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout);

    // Parse JSON verdict from output
    let json_str = extract_triage_json(&raw)
        .ok_or_else(|| anyhow::anyhow!("No valid JSON found in triage output"))?;

    let verdict: TriageVerdict = serde_json::from_str(&json_str)
        .with_context(|| format!("Failed to parse triage JSON: {}", json_str))?;

    // Validate verdict value
    match verdict.verdict.as_str() {
        "done" | "continue" | "restart" => Ok(verdict),
        other => anyhow::bail!(
            "Invalid triage verdict '{}', expected done/continue/restart",
            other
        ),
    }
}

/// Extract a JSON object from potentially noisy LLM output.
fn extract_triage_json(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    // Strip markdown code fences
    if trimmed.starts_with("```") {
        let inner = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
            return Some(inner.to_string());
        }
    }

    // Find first { to last }
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
        && start <= end
    {
        let candidate = &trimmed[start..=end];
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

/// Apply a triage verdict to a task.
fn apply_triage_verdict(task: &mut Task, verdict: &TriageVerdict, agent_id: &str, pid: u32) {
    match verdict.verdict.as_str() {
        "done" => {
            task.status = Status::Done;
            task.completed_at = Some(Utc::now().to_rfc3339());
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("triage".to_string()),
                message: format!(
                    "Triage: work complete (agent '{}' PID {} died) — {}",
                    agent_id, pid, verdict.reason
                ),
            });
        }
        "continue" => {
            // Check max_retries before allowing continue
            if let Some(max) = task.max_retries
                && task.retry_count >= max
            {
                task.status = Status::Failed;
                task.failure_reason = Some(format!(
                    "Max retries exceeded ({}/{}): {}",
                    task.retry_count, max, verdict.reason
                ));
                task.assigned = None;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("triage".to_string()),
                    message: format!(
                        "Triage: wanted continue but max retries exceeded ({}/{}) — failing task",
                        task.retry_count, max
                    ),
                });
                return;
            }

            task.status = Status::Open;
            task.assigned = None;
            task.retry_count += 1;

            // Replace (not append) recovery context to prevent unbounded description growth
            let recovery_context = format!(
                "\n\n## Previous Attempt Recovery\n\
                 A previous agent worked on this task but died before completing.\n\n\
                 **What was accomplished:** {}\n\n\
                 Continue from where the previous agent left off. Do NOT redo completed work.\n\
                 Check existing artifacts before starting.",
                verdict.summary
            );
            if let Some(ref mut desc) = task.description {
                // Strip any existing recovery section before adding new one
                if let Some(pos) = desc.find("\n\n## Previous Attempt Recovery") {
                    desc.truncate(pos);
                }
                desc.push_str(&recovery_context);
            } else {
                task.description = Some(recovery_context.trim_start().to_string());
            }

            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("triage".to_string()),
                message: format!(
                    "Triage: continuing (agent '{}' PID {} died) — {}",
                    agent_id, pid, verdict.reason
                ),
            });
        }
        _ => {
            // "restart" or anything else: same as existing behavior
            // Check max_retries before allowing restart
            if let Some(max) = task.max_retries
                && task.retry_count >= max
            {
                task.status = Status::Failed;
                task.failure_reason = Some(format!(
                    "Max retries exceeded ({}/{}): {}",
                    task.retry_count, max, verdict.reason
                ));
                task.assigned = None;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("triage".to_string()),
                    message: format!(
                        "Triage: wanted restart but max retries exceeded ({}/{}) — failing task",
                        task.retry_count, max
                    ),
                });
                return;
            }

            task.status = Status::Open;
            task.assigned = None;
            task.retry_count += 1;
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("triage".to_string()),
                message: format!(
                    "Triage: restarting (agent '{}' PID {} died) — {}",
                    agent_id, pid, verdict.reason
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::Task;

    #[test]
    fn test_read_truncated_log_missing_file() {
        let result = read_truncated_log("/nonexistent/path/output.log", 50000);
        assert!(result.contains("not found"));
    }

    #[test]
    fn test_read_truncated_log_small_file() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("output.log");
        fs::write(&log_path, "hello world\nline 2\n").unwrap();
        let result = read_truncated_log(log_path.to_str().unwrap(), 50000);
        assert_eq!(result, "hello world\nline 2\n");
    }

    #[test]
    fn test_read_truncated_log_large_file() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("output.log");
        // Write 200 bytes, read last 100
        let content = "a".repeat(100) + "\n" + &"b".repeat(99);
        fs::write(&log_path, &content).unwrap();
        let result = read_truncated_log(log_path.to_str().unwrap(), 100);
        assert!(result.contains("[... "));
        assert!(result.contains("bytes truncated"));
        // Should contain the tail portion
        assert!(result.contains("bbb"));
    }

    #[test]
    fn test_read_truncated_log_empty_file() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("output.log");
        fs::write(&log_path, "").unwrap();
        let result = read_truncated_log(log_path.to_str().unwrap(), 50000);
        assert!(result.contains("empty"));
    }

    #[test]
    fn test_build_triage_prompt() {
        let task = Task {
            id: "test-task".to_string(),
            title: "Fix the bug".to_string(),
            description: Some("There is a bug in foo.rs".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            ..Default::default()
        };
        let prompt = build_triage_prompt(&task, "some log output");
        assert!(prompt.contains("test-task"));
        assert!(prompt.contains("Fix the bug"));
        assert!(prompt.contains("some log output"));
        assert!(prompt.contains("done"));
        assert!(prompt.contains("continue"));
        assert!(prompt.contains("restart"));
    }

    #[test]
    fn test_extract_triage_json_plain() {
        let input = r#"{"verdict": "done", "reason": "work complete", "summary": "all done"}"#;
        let result = extract_triage_json(input).unwrap();
        let parsed: TriageVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.verdict, "done");
    }

    #[test]
    fn test_extract_triage_json_with_fences() {
        let input = "```json\n{\"verdict\": \"continue\", \"reason\": \"partial\", \"summary\": \"half done\"}\n```";
        let result = extract_triage_json(input).unwrap();
        let parsed: TriageVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.verdict, "continue");
    }

    #[test]
    fn test_extract_triage_json_with_surrounding_text() {
        let input = "Here is my analysis:\n{\"verdict\": \"restart\", \"reason\": \"no progress\", \"summary\": \"\"}\nDone.";
        let result = extract_triage_json(input).unwrap();
        let parsed: TriageVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.verdict, "restart");
    }

    #[test]
    fn test_extract_triage_json_garbage() {
        assert!(extract_triage_json("no json here").is_none());
    }

    #[test]
    fn test_extract_triage_json_inverted_braces_no_panic() {
        // If } appears before { in the text, should return None, not panic
        assert!(extract_triage_json("some text } then { more text").is_none());
    }

    #[test]
    fn test_apply_triage_verdict_done() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            ..Default::default()
        };
        let verdict = TriageVerdict {
            verdict: "done".to_string(),
            reason: "work complete".to_string(),
            summary: "all files written".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Done);
        assert!(task.completed_at.is_some());
        assert!(task.log.last().unwrap().message.contains("work complete"));
    }

    #[test]
    fn test_apply_triage_verdict_done_verified() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            verify: Some("Check tests pass".to_string()),
            ..Default::default()
        };
        let verdict = TriageVerdict {
            verdict: "done".to_string(),
            reason: "tests pass".to_string(),
            summary: "implementation complete".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_apply_triage_verdict_continue() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: Some("Original description".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            ..Default::default()
        };
        let verdict = TriageVerdict {
            verdict: "continue".to_string(),
            reason: "partial progress".to_string(),
            summary: "Created foo.rs and bar.rs".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert_eq!(task.retry_count, 1);
        assert!(
            task.description
                .as_ref()
                .unwrap()
                .contains("Previous Attempt Recovery")
        );
        assert!(
            task.description
                .as_ref()
                .unwrap()
                .contains("Created foo.rs and bar.rs")
        );
    }

    #[test]
    fn test_apply_triage_verdict_restart() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            ..Default::default()
        };
        let verdict = TriageVerdict {
            verdict: "restart".to_string(),
            reason: "no progress".to_string(),
            summary: "".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert_eq!(task.retry_count, 1);
        // Description should NOT have recovery context for restart
        assert!(task.description.is_none());
    }

    #[test]
    fn test_apply_triage_verdict_continue_max_retries_exceeded() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: Some("Original".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            retry_count: 3,
            max_retries: Some(3),
            ..Default::default()
        };
        let verdict = TriageVerdict {
            verdict: "continue".to_string(),
            reason: "needs more work".to_string(),
            summary: "partial progress".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Failed);
        assert!(task.assigned.is_none());
        assert_eq!(task.retry_count, 3); // not incremented
        assert!(
            task.failure_reason
                .as_ref()
                .unwrap()
                .contains("Max retries exceeded")
        );
    }

    #[test]
    fn test_apply_triage_verdict_restart_max_retries_exceeded() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            retry_count: 2,
            max_retries: Some(2),
            ..Default::default()
        };
        let verdict = TriageVerdict {
            verdict: "restart".to_string(),
            reason: "no progress".to_string(),
            summary: "".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Failed);
        assert!(task.assigned.is_none());
        assert_eq!(task.retry_count, 2); // not incremented
        assert!(
            task.failure_reason
                .as_ref()
                .unwrap()
                .contains("Max retries exceeded")
        );
    }
}
