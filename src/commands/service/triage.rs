//! Dead-agent triage: detection, LLM-based assessment, and verdict application.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::io::{Read as IoRead, Seek, SeekFrom};
use std::path::Path;

use workgraph::agency;
use workgraph::config::Config;
use workgraph::graph::{
    LogEntry, Status, Task, evaluate_cycle_iteration, parse_token_usage, parse_wg_tokens,
};
use workgraph::parser::{load_graph, modify_graph};
use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};
use workgraph::stream_event::{self, StreamEvent};

use crate::commands::is_process_alive;

/// Extract session_id from an agent's stream files (stream.jsonl or raw_stream.jsonl).
fn extract_session_id(agent: &AgentEntry) -> Option<String> {
    let output_path = std::path::Path::new(&agent.output_file);
    let agent_dir = output_path.parent()?;

    // Try unified stream.jsonl first
    let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);
    if stream_path.exists()
        && let Ok((events, _)) = stream_event::read_stream_events(&stream_path, 0)
    {
        for event in &events {
            if let StreamEvent::Init {
                session_id: Some(sid),
                ..
            } = event
            {
                return Some(sid.clone());
            }
        }
    }

    // Try raw_stream.jsonl (Claude CLI)
    let raw_path = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
    if raw_path.exists()
        && let Ok((events, _)) = stream_event::translate_claude_stream(&raw_path, 0)
    {
        for event in &events {
            if let StreamEvent::Init {
                session_id: Some(sid),
                ..
            } = event
            {
                return Some(sid.clone());
            }
        }
    }

    None
}

/// Default grace period value, used by tests.
/// Production code reads from `config.agent.reaper_grace_seconds`.
#[cfg(test)]
const DEFAULT_REAPER_GRACE_PERIOD_SECS: i64 = 30;

/// Reason an agent was detected as dead
enum DeadReason {
    /// Process is no longer running
    ProcessExited,
    /// PID exists but belongs to a different process (PID reuse after daemon restart)
    PidReused,
}

/// Check stream file activity for an agent. Returns the timestamp of the last
/// stream event (if any stream file exists), and whether the stream is stale.
///
/// Staleness threshold: 5 minutes with no stream events while PID is alive.
const STREAM_STALE_THRESHOLD_MS: i64 = 5 * 60 * 1000;

fn check_stream_liveness(agent: &AgentEntry) -> Option<i64> {
    let output_path = std::path::Path::new(&agent.output_file);
    let agent_dir = output_path.parent()?;

    // Try unified stream.jsonl first (native executor, amplifier/shell bookends)
    let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);
    if stream_path.exists()
        && let Ok((events, _)) = stream_event::read_stream_events(&stream_path, 0)
    {
        return events.last().map(|e| e.timestamp_ms());
    }

    // Try raw_stream.jsonl (Claude CLI)
    let raw_path = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
    if raw_path.exists()
        && let Ok((events, _)) = stream_event::translate_claude_stream(&raw_path, 0)
    {
        return events.last().map(|e| e.timestamp_ms());
    }

    None
}

/// Check if an agent should be considered dead.
///
/// `grace_period_secs` is the minimum uptime before a dead PID is acted on.
/// This avoids race conditions where the coordinator registers a PID but the
/// process hasn't fully started yet.
fn detect_dead_reason(agent: &AgentEntry, grace_period_secs: i64) -> Option<DeadReason> {
    if !agent.is_alive() {
        return None;
    }

    // Grace period: don't reap agents that were started very recently.
    // The coordinator may register a PID before the process is fully up,
    // so give it grace_period_secs before treating a missing PID as dead.
    if let Some(uptime) = agent.uptime_secs()
        && uptime < grace_period_secs
    {
        return None;
    }

    // Process not running is the only signal — heartbeat is no longer used for detection
    if !is_process_alive(agent.pid) {
        return Some(DeadReason::ProcessExited);
    }

    // PID exists but might belong to a different process (PID reuse).
    // This happens when the daemon restarts after a crash and old PIDs have been
    // recycled by the OS. We verify by comparing the actual process start time
    // against the agent's registered start time.
    if let Ok(agent_start) = agent.started_at.parse::<chrono::DateTime<chrono::Utc>>()
        && !workgraph::service::verify_process_identity(agent.pid, agent_start.timestamp())
    {
        return Some(DeadReason::PidReused);
    }

    None
}

/// Clean up dead agents (process exited)
/// Returns list of cleaned up agent IDs
pub(crate) fn cleanup_dead_agents(dir: &Path, graph_path: &Path) -> Result<Vec<String>> {
    let config = Config::load_or_default(dir);
    let grace_secs = config.agent.reaper_grace_seconds as i64;

    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Find agents that are dead: process gone
    let dead: Vec<_> = locked_registry
        .agents
        .values()
        .filter_map(|a| {
            detect_dead_reason(a, grace_secs).map(|reason| {
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

    // Auto-bump heartbeat for agents whose process is still alive.
    // Also check stream file activity for more precise liveness tracking.
    for agent in locked_registry.agents.values_mut() {
        if agent.is_alive() && is_process_alive(agent.pid) {
            agent.last_heartbeat = Utc::now().to_rfc3339();

            // Check stream for staleness warning (PID alive but no stream activity)
            if let Some(last_event_ms) = check_stream_liveness(agent) {
                let now_ms = stream_event::now_ms();
                if now_ms - last_event_ms > STREAM_STALE_THRESHOLD_MS {
                    eprintln!(
                        "[triage] WARNING: Agent {} (task {}) PID alive but stream stale for {}s",
                        agent.id,
                        agent.task_id,
                        (now_ms - last_event_ms) / 1000
                    );
                }
            }
        }
    }

    if dead.is_empty() {
        locked_registry.save_ref()?;
        return Ok(vec![]);
    }

    // Mark these agents as dead in registry
    let now = Utc::now().to_rfc3339();
    for (agent_id, _, _, _, _) in &dead {
        if let Some(agent) = locked_registry.get_agent_mut(agent_id) {
            agent.status = AgentStatus::Dead;
            if agent.completed_at.is_none() {
                agent.completed_at = Some(now.clone());
            }
        }
    }
    locked_registry.save_ref()?;

    // Load config for triage settings (already loaded above as `config`)

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
                                user: Some(workgraph::current_user()),
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
                        DeadReason::PidReused => format!(
                            "Task unclaimed: agent '{}' (PID {}) dead (PID reused by different process)",
                            agent_id, pid
                        ),
                    };
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: None,
                        user: Some(workgraph::current_user()),
                        message: reason_msg,
                    });
                }
                tasks_modified = true;
            }
        }
    }

    // Extract token usage and session_id from dead agents' stream files
    for (agent_id, task_id, _pid, output_file, _reason) in &dead {
        // Extract session_id from stream events
        if let Some(agent) = locked_registry.get_agent(agent_id)
            && let Some(sid) = extract_session_id(agent)
            && let Some(task) = graph.get_task_mut(task_id)
            && task.session_id.is_none()
        {
            task.session_id = Some(sid);
            tasks_modified = true;
        }

        // Extract token usage from output.log
        if let Some(task) = graph.get_task_mut(task_id)
            && task.token_usage.is_none()
        {
            let output_path = std::path::Path::new(output_file);
            let abs_path = if output_path.is_absolute() {
                output_path.to_path_buf()
            } else {
                dir.parent().unwrap_or(dir).join(output_path)
            };
            if let Some(usage) = parse_token_usage(&abs_path) {
                task.token_usage = Some(usage);
                tasks_modified = true;
            } else if let Some(usage) = parse_wg_tokens(&abs_path) {
                task.token_usage = Some(usage);
                tasks_modified = true;
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
        // Write back atomically via modify_graph. Since we already have the mutated graph,
        // we replace all task states from our local copy.
        modify_graph(graph_path, |fresh_graph| {
            // Replay mutations: for each task we modified, update the fresh graph
            for tid in &tasks_completed_by_triage {
                if let Some(local) = graph.get_task(tid)
                    && let Some(fresh) = fresh_graph.get_task_mut(tid)
                {
                    fresh.status = local.status;
                    fresh.completed_at = local.completed_at.clone();
                    fresh.failure_reason = local.failure_reason.clone();
                    fresh.retry_count = local.retry_count;
                    fresh.log = local.log.clone();
                    fresh.session_id = local.session_id.clone();
                    fresh.token_usage = local.token_usage.clone();
                }
            }
            // Also replay mutations for other dead-agent tasks (unclaim, triage-fail, token/session)
            for (_, task_id, _, _, _) in &dead {
                if !tasks_completed_by_triage.contains(task_id)
                    && let Some(local) = graph.get_task(task_id)
                    && let Some(fresh) = fresh_graph.get_task_mut(task_id)
                {
                    fresh.status = local.status;
                    fresh.assigned = local.assigned.clone();
                    fresh.log = local.log.clone();
                    fresh.session_id = local.session_id.clone();
                    fresh.token_usage = local.token_usage.clone();
                }
            }
            true
        })
        .context("Failed to save graph")?;
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

    // Clean up worktrees for dead agents (agent isolation).
    // Read metadata.json from each dead agent's output directory to find
    // worktree_path and worktree_branch, then recover commits and remove.
    let project_root = dir.parent().unwrap_or(dir);
    for (agent_id, _task_id, _pid, output_file, _reason) in &dead {
        let output_path = std::path::Path::new(output_file);
        let agent_dir = if output_path.is_absolute() {
            output_path.parent().map(|p| p.to_path_buf())
        } else {
            output_path.parent().map(|p| project_root.join(p))
        };
        let agent_dir = match agent_dir {
            Some(d) => d,
            None => continue,
        };

        let metadata_path = agent_dir.join("metadata.json");
        if let Ok(metadata_str) = fs::read_to_string(&metadata_path)
            && let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&metadata_str)
            && let (Some(wt_path_str), Some(wt_branch)) = (
                metadata.get("worktree_path").and_then(|v| v.as_str()),
                metadata.get("worktree_branch").and_then(|v| v.as_str()),
            )
        {
            let wt_path = Path::new(wt_path_str);
            if wt_path.exists() {
                eprintln!(
                    "[triage] Cleaning up worktree for dead agent {}: {:?}",
                    agent_id, wt_path
                );
                super::worktree::cleanup_dead_agent_worktree(
                    project_root,
                    wt_path,
                    wt_branch,
                    agent_id,
                );
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
pub(super) fn read_truncated_log(path: &str, max_bytes: usize) -> String {
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
    let log_content = read_truncated_log(output_file, max_log_bytes);
    let prompt = build_triage_prompt(task, &log_content);

    let result = workgraph::service::llm::run_lightweight_llm_call(
        config,
        workgraph::config::DispatchRole::Triage,
        &prompt,
        timeout_secs,
    )
    .context("Triage LLM call failed")?;

    // Parse JSON verdict from output
    let json_str = extract_triage_json(&result.text)
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
                user: Some(workgraph::current_user()),
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
                    user: Some(workgraph::current_user()),
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
                user: Some(workgraph::current_user()),
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
                    user: Some(workgraph::current_user()),
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
                user: Some(workgraph::current_user()),
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

    /// Verify that for bare-mode agent logs (no Claude CLI `type=result` line),
    /// `parse_wg_tokens` extracts token usage where `parse_token_usage` returns None.
    /// This is the fallback chain used in `cleanup_dead_agents` for `.flip-*`,
    /// `.evaluate-*`, and `.assign-*` tasks.
    #[test]
    fn test_triage_token_extraction_fallback_to_wg_tokens() {
        use workgraph::graph::{parse_token_usage, parse_wg_tokens};

        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("output.log");

        // Bare-mode agent output: no Claude CLI JSON, only __WG_TOKENS__ lines
        std::fs::write(
            &log_path,
            "FLIP Phase 1: Inferring prompt from output...\n\
             FLIP Phase 2: Comparing prompts...\n\
             __WG_TOKENS__:{\"cost_usd\":0.05,\"input_tokens\":300,\"output_tokens\":100,\"cache_read_input_tokens\":50,\"cache_creation_input_tokens\":0}\n",
        )
        .unwrap();

        // parse_token_usage should return None (no type=result line)
        assert!(parse_token_usage(&log_path).is_none());

        // parse_wg_tokens should succeed (the fallback path)
        let usage = parse_wg_tokens(&log_path).unwrap();
        assert!((usage.cost_usd - 0.05).abs() < f64::EPSILON);
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 100);
        assert_eq!(usage.cache_read_input_tokens, 50);
    }

    // -----------------------------------------------------------------------
    // Dead agent reaper: grace period and PID-based detection
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_dead_reason_skips_recently_started_agent() {
        // An agent started just now with a non-existent PID should NOT be
        // detected as dead — the grace period protects against startup races.
        let agent = AgentEntry {
            id: "agent-1".to_string(),
            pid: 999999999,
            task_id: "task-1".to_string(),
            executor: "test".to_string(),
            started_at: Utc::now().to_rfc3339(), // just started
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: "/tmp/test.log".to_string(),
            model: None,
            completed_at: None,
        };

        assert!(
            detect_dead_reason(&agent, DEFAULT_REAPER_GRACE_PERIOD_SECS).is_none(),
            "Agent within grace period should not be detected as dead"
        );
    }

    #[test]
    fn test_detect_dead_reason_detects_old_dead_agent() {
        // An agent started long ago with a non-existent PID should be detected
        // as dead — past the grace period.
        let old_start = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        let agent = AgentEntry {
            id: "agent-1".to_string(),
            pid: 999999999,
            task_id: "task-1".to_string(),
            executor: "test".to_string(),
            started_at: old_start.clone(),
            last_heartbeat: old_start,
            status: AgentStatus::Working,
            output_file: "/tmp/test.log".to_string(),
            model: None,
            completed_at: None,
        };

        let reason = detect_dead_reason(&agent, DEFAULT_REAPER_GRACE_PERIOD_SECS);
        assert!(
            reason.is_some(),
            "Agent past grace period with dead PID should be detected"
        );
        assert!(
            matches!(reason.unwrap(), DeadReason::ProcessExited),
            "Reason should be ProcessExited for non-existent PID"
        );
    }

    #[test]
    fn test_detect_dead_reason_zero_grace_period() {
        // With grace period = 0, even a freshly started agent with a dead PID
        // should be detected immediately.
        let agent = AgentEntry {
            id: "agent-1".to_string(),
            pid: 999999999,
            task_id: "task-1".to_string(),
            executor: "test".to_string(),
            started_at: Utc::now().to_rfc3339(), // just started
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: "/tmp/test.log".to_string(),
            model: None,
            completed_at: None,
        };

        let reason = detect_dead_reason(&agent, 0);
        assert!(
            reason.is_some(),
            "Grace period 0 should detect dead PID immediately"
        );
        assert!(
            matches!(reason.unwrap(), DeadReason::ProcessExited),
            "Reason should be ProcessExited"
        );
    }

    #[test]
    fn test_detect_dead_reason_ignores_non_alive_agent() {
        // An agent already marked dead in the registry should not be re-detected.
        let agent = AgentEntry {
            id: "agent-1".to_string(),
            pid: 999999999,
            task_id: "task-1".to_string(),
            executor: "test".to_string(),
            started_at: "2020-01-01T00:00:00Z".to_string(),
            last_heartbeat: "2020-01-01T00:00:00Z".to_string(),
            status: AgentStatus::Dead,
            output_file: "/tmp/test.log".to_string(),
            model: None,
            completed_at: None,
        };

        assert!(
            detect_dead_reason(&agent, DEFAULT_REAPER_GRACE_PERIOD_SECS).is_none(),
            "Already-dead agent should not be re-detected"
        );
    }

    #[test]
    fn test_detect_dead_reason_alive_process() {
        // An agent with the current process PID should not be detected as dead.
        let agent = AgentEntry {
            id: "agent-1".to_string(),
            pid: std::process::id(),
            task_id: "task-1".to_string(),
            executor: "test".to_string(),
            started_at: "2020-01-01T00:00:00Z".to_string(), // old start, past grace
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: "/tmp/test.log".to_string(),
            model: None,
            completed_at: None,
        };

        // On Linux, verify_process_identity may detect PID reuse since our
        // actual start time doesn't match 2020. On non-Linux this falls
        // through to None. Either way the process IS alive, so ProcessExited
        // should never fire.
        let reason = detect_dead_reason(&agent, DEFAULT_REAPER_GRACE_PERIOD_SECS);
        if let Some(ref r) = reason {
            // Only PidReused is acceptable here (on Linux where /proc is available)
            assert!(
                matches!(r, DeadReason::PidReused),
                "Alive process should not be detected as ProcessExited"
            );
        }
    }

    #[test]
    fn test_dead_agent_reaper_unclaims_task() {
        // End-to-end: a dead PID past grace period triggers task unclaim via cleanup_dead_agents.
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        let gpath = wg_dir.join("graph.jsonl");

        // Write config with reaper_grace_seconds = 0 so detection is immediate
        let config_dir = wg_dir;
        fs::create_dir_all(config_dir).ok();
        fs::write(
            config_dir.join("config.toml"),
            "[agent]\nreaper_grace_seconds = 0\n",
        )
        .unwrap();

        // Create an in-progress task assigned to agent-1
        let mut graph = workgraph::graph::WorkGraph::new();
        let task = Task {
            id: "task-1".to_string(),
            title: "Test Task".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            ..Default::default()
        };
        graph.add_node(workgraph::graph::Node::Task(task));
        workgraph::parser::save_graph(&graph, &gpath).unwrap();

        // Register an agent with a dead PID
        let mut registry = AgentRegistry::new();
        let agent_id = registry.register_agent(999999999, "task-1", "test", "/tmp/output.log");
        registry.save(wg_dir).unwrap();

        // Run the reaper
        let cleaned = cleanup_dead_agents(wg_dir, &gpath).unwrap();
        assert_eq!(cleaned.len(), 1, "Should detect one dead agent");
        assert_eq!(cleaned[0], agent_id);

        // Verify task was unclaimed
        let graph = workgraph::parser::load_graph(&gpath).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert_eq!(task.status, Status::Open, "Task should be reset to Open");
        assert!(task.assigned.is_none(), "Task should be unassigned");

        // Verify log entry was created
        assert!(
            task.log.iter().any(|l| l.message.contains("process exited")
                || l.message.contains("dead")
                || l.message.contains("unclaimed")
                || l.message.contains("Triage")),
            "Task should have a log entry about the dead agent: {:?}",
            task.log
        );

        // Verify agent is marked dead in registry
        let registry = AgentRegistry::load(wg_dir).unwrap();
        let agent = registry.get_agent(&agent_id).unwrap();
        assert_eq!(
            agent.status,
            AgentStatus::Dead,
            "Agent should be marked dead in registry"
        );
    }

    #[test]
    fn test_dead_agent_reaper_grace_period_prevents_unclaim() {
        // A dead PID within the grace period should NOT trigger unclaim.
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        let gpath = wg_dir.join("graph.jsonl");

        // No config override → default 30s grace period applies.

        // Create an in-progress task assigned to agent-1
        let mut graph = workgraph::graph::WorkGraph::new();
        let task = Task {
            id: "task-1".to_string(),
            title: "Test Task".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            ..Default::default()
        };
        graph.add_node(workgraph::graph::Node::Task(task));
        workgraph::parser::save_graph(&graph, &gpath).unwrap();

        // Register an agent with a dead PID but FRESH start time (within grace period)
        let mut registry = AgentRegistry::new();
        let _agent_id = registry.register_agent(999999999, "task-1", "test", "/tmp/output.log");
        // started_at is already "now" from register_agent, which is within grace period
        registry.save(wg_dir).unwrap();

        // Run the reaper
        let cleaned = cleanup_dead_agents(wg_dir, &gpath).unwrap();
        assert!(
            cleaned.is_empty(),
            "Should NOT detect dead agent within grace period"
        );

        // Verify task is still in-progress
        let graph = workgraph::parser::load_graph(&gpath).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert_eq!(
            task.status,
            Status::InProgress,
            "Task should remain InProgress during grace period"
        );
        assert_eq!(
            task.assigned.as_deref(),
            Some("agent-1"),
            "Task should remain assigned during grace period"
        );
    }
}
