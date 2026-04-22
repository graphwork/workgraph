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
use workgraph::profile;
use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};
use workgraph::service::{ProviderErrorKind, ProviderHealth, classify_error, extract_provider_id};
use workgraph::stream_event::{self, StreamEvent};

use crate::commands::is_process_alive;
use workgraph::metrics::log_metrics_summary;

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

/// Extract token usage from an agent's stream.jsonl file.
///
/// Used as a fallback when output.log doesn't contain parseable token data
/// (e.g., native executor writes usage to stream.jsonl directly).
fn parse_token_usage_from_stream(
    agent_id: &str,
    dir: &Path,
) -> Option<workgraph::graph::TokenUsage> {
    let agent_dir = dir.join("agents").join(agent_id);
    let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);
    if !stream_path.exists() {
        return None;
    }

    let (events, _) = stream_event::read_stream_events(&stream_path, 0).ok()?;
    if events.is_empty() {
        return None;
    }

    let mut state = stream_event::AgentStreamState::default();
    state.ingest(&events, 0);

    let usage = state.to_token_usage();
    // Only return if there's actual usage data
    if usage.input_tokens > 0 || usage.output_tokens > 0 {
        Some(usage)
    } else {
        None
    }
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
    /// Agent has not sent a heartbeat within the configured timeout
    HeartbeatTimeout,
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
fn detect_dead_reason(
    agent: &AgentEntry,
    grace_period_secs: i64,
    heartbeat_timeout_secs: i64,
) -> Option<DeadReason> {
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

    // Check for heartbeat timeout, but first check stream activity as a positive
    // liveness signal. If the agent's stream has recent events, it's actively working
    // even if the registry heartbeat is stale (e.g., wrapper heartbeat loop hasn't
    // kicked in yet).
    if let Ok(last_hb) = agent
        .last_heartbeat
        .parse::<chrono::DateTime<chrono::Utc>>()
    {
        let now = chrono::Utc::now();
        let since_heartbeat = (now - last_hb).num_seconds();
        if since_heartbeat > heartbeat_timeout_secs {
            // Before declaring timeout, check stream file for recent activity.
            // If the stream has events newer than the last heartbeat AND those
            // events are within the timeout window, the agent is actively working.
            // We require events to be newer than the heartbeat to avoid Init
            // bookend events (written once at spawn) from indefinitely extending
            // the timeout window.
            if let Some(last_event_ms) = check_stream_liveness(agent) {
                let hb_ms = last_hb.timestamp_millis();
                let now_ms = now.timestamp_millis();
                let since_event_secs = (now_ms - last_event_ms) / 1000;
                if last_event_ms > hb_ms && since_event_secs <= heartbeat_timeout_secs {
                    // Stream has activity newer than last heartbeat — agent is alive
                    return None;
                }
            }
            return Some(DeadReason::HeartbeatTimeout);
        }
    }

    None
}

/// Clean up dead agents (process exited)
/// Returns list of cleaned up agent IDs
pub(crate) fn cleanup_dead_agents(dir: &Path, graph_path: &Path) -> Result<Vec<String>> {
    let config = Config::load_or_default(dir);
    let grace_secs = config.agent.reaper_grace_seconds as i64;
    let heartbeat_timeout_secs = (config.agent.heartbeat_timeout * 60) as i64; // Config is in minutes

    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Find agents that are dead: process gone
    let dead: Vec<_> = locked_registry
        .agents
        .values()
        .filter_map(|a| {
            detect_dead_reason(a, grace_secs, heartbeat_timeout_secs).map(|reason| {
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

    // Check stream file activity for more precise liveness tracking.
    // NOTE: Heartbeats are only updated when agents actively check in,
    // not auto-bumped just for having a live process.
    for agent in locked_registry.agents.values_mut() {
        if agent.is_alive() && is_process_alive(agent.pid) {
            // Do not auto-bump heartbeat - only agents should update their own heartbeat

            // Check stream for staleness warning (PID alive but no stream activity)
            if let Some(last_event_ms) = check_stream_liveness(agent) {
                let now_ms = stream_event::now_ms();
                if now_ms - last_event_ms > STREAM_STALE_THRESHOLD_MS {
                    // Suppress warning if agent has active child processes
                    // (e.g., waiting on cargo build, wg commands, sub-agents)
                    if workgraph::service::has_active_children(agent.pid) {
                        eprintln!(
                            "[triage] Agent {} (task {}) stream stale for {}s but has active child processes — not stuck",
                            agent.id,
                            agent.task_id,
                            (now_ms - last_event_ms) / 1000
                        );
                    } else {
                        eprintln!(
                            "[triage] WARNING: Agent {} (task {}) PID alive but stream stale for {}s (no child processes)",
                            agent.id,
                            agent.task_id,
                            (now_ms - last_event_ms) / 1000
                        );
                    }
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
                            apply_triage_verdict(task, &verdict, agent_id, *pid, dir, &config);
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
                            task.retry_count += 1;
                            try_escalate_model(task, dir, &config);
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
                    task.retry_count += 1;
                    try_escalate_model(task, dir, &config);
                    let reason_msg = match reason {
                        DeadReason::ProcessExited => format!(
                            "Task unclaimed: agent '{}' (PID {}) process exited",
                            agent_id, pid
                        ),
                        DeadReason::PidReused => format!(
                            "Task unclaimed: agent '{}' (PID {}) dead (PID reused by different process)",
                            agent_id, pid
                        ),
                        DeadReason::HeartbeatTimeout => format!(
                            "Task unclaimed: agent '{}' (PID {}) timed out (no heartbeat)",
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

        // Extract token usage from output.log or stream.jsonl
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
            } else if let Some(usage) = parse_token_usage_from_stream(agent_id, dir) {
                // Fallback: read stream.jsonl (native executor writes usage there directly)
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
                    fresh.model = local.model.clone();
                    fresh.tried_models = local.tried_models.clone();
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
                    fresh.model = local.model.clone();
                    fresh.tried_models = local.tried_models.clone();
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
        match validate_and_parse_agent_metadata(&metadata_path, agent_id) {
            Ok(Some((wt_path_str, _wt_branch))) => {
                // Worktree cleanup REMOVED. Worktrees are preserved
                // for forensic inspection and manual archival. Agent
                // work (uncommitted changes, artifacts) must not be
                // auto-destroyed.
                let wt_path = Path::new(&wt_path_str);
                if wt_path.exists() {
                    eprintln!(
                        "[triage] Dead agent {} has worktree at {:?} (preserved — use `wg worktree archive` to clean up manually)",
                        agent_id, wt_path
                    );
                }
            }
            Ok(None) => {
                eprintln!(
                    "[triage] No valid worktree metadata found for dead agent {}, skipping worktree cleanup",
                    agent_id
                );
            }
            Err(e) => {
                eprintln!(
                    "[triage] Failed to parse metadata for dead agent {}: {}",
                    agent_id, e
                );

                // Attempt fallback cleanup by scanning for agent worktrees
                if let Err(fallback_err) = attempt_fallback_worktree_cleanup(project_root, agent_id)
                {
                    eprintln!(
                        "[triage] Fallback worktree cleanup also failed for agent {}: {}",
                        agent_id, fallback_err
                    );
                }
            }
        }
    }

    // Provider health tracking: analyze dead agents for provider failure patterns
    if !dead.is_empty() {
        if let Err(e) = track_provider_health(dir, &dead, &locked_registry, &config) {
            eprintln!(
                "[coordinator] Warning: provider health tracking failed: {}",
                e
            );
        }

        // Log metrics summary when dead agents are cleaned
        eprintln!(
            "[triage] Dead agent cleanup completed for {} agents",
            dead.len()
        );
        log_metrics_summary();
    }

    Ok(dead.into_iter().map(|(id, _, _, _, _)| id).collect())
}

/// Track provider health based on dead agent failures
fn track_provider_health(
    dir: &Path,
    dead: &[(String, String, u32, String, DeadReason)],
    locked_registry: &workgraph::service::LockedRegistry,
    config: &Config,
) -> Result<()> {
    // Load current provider health state
    let mut provider_health = ProviderHealth::load(dir)?;

    // Load graph to get task failure information
    let graph = load_graph(super::super::graph_path(dir))?;

    // Track failures for each dead agent
    for (agent_id, task_id, _pid, output_file, _reason) in dead {
        // Get agent information from registry
        let agent = match locked_registry.get_agent(agent_id) {
            Some(a) => a,
            None => {
                eprintln!(
                    "[provider-health] Warning: agent {} not found in registry",
                    agent_id
                );
                continue;
            }
        };

        // Extract provider ID from agent executor and model
        let provider_id = extract_provider_id(&agent.executor, agent.model.as_deref());

        // Get task to check for failure information
        let task = match graph.get_task(task_id) {
            Some(t) => t,
            None => {
                eprintln!("[provider-health] Warning: task {} not found", task_id);
                continue;
            }
        };

        // Extract error information
        let (exit_code, stderr) = extract_error_info(&task.failure_reason, output_file);

        // Classify the error
        let error_kind = classify_error(exit_code, &stderr);

        match error_kind {
            ProviderErrorKind::FatalProvider => {
                // This is a provider-level failure - track it
                eprintln!(
                    "[provider-health] Fatal provider error for '{}': {} (exit: {:?}, stderr: {})",
                    provider_id,
                    task.failure_reason.as_deref().unwrap_or("unknown"),
                    exit_code,
                    stderr.chars().take(100).collect::<String>()
                );

                provider_health.record_failure(
                    &provider_id,
                    error_kind,
                    task.failure_reason
                        .as_deref()
                        .unwrap_or("unknown error")
                        .to_string(),
                );
            }
            ProviderErrorKind::Transient | ProviderErrorKind::FatalTask => {
                // For successful completion or non-provider errors, record success to reset counters
                // But only if the task actually completed successfully
                if task.status == workgraph::graph::Status::Done {
                    provider_health.record_success(&provider_id);
                }
                // For transient/task errors, don't count against provider health
            }
        }
    }

    // Check if any providers should be paused and apply pause logic
    let paused_providers = provider_health.check_and_apply_pauses(
        config.coordinator.provider_failure_threshold,
        &config.coordinator.on_provider_failure,
    );

    // Log any providers that were paused
    for provider_id in &paused_providers {
        eprintln!(
            "[provider-health] Provider '{}' paused due to consecutive failures",
            provider_id
        );
    }

    // If service was paused, log the reason
    if provider_health.service_paused {
        eprintln!(
            "[provider-health] Service paused: {}",
            provider_health
                .pause_reason
                .as_deref()
                .unwrap_or("unknown reason")
        );
    }

    // Save updated provider health state
    provider_health.save(dir)?;

    Ok(())
}

/// Extract error information from task failure reason and output file
fn extract_error_info(failure_reason: &Option<String>, output_file: &str) -> (Option<i32>, String) {
    let mut exit_code = None;
    let mut stderr = String::new();

    // Try to parse exit code from failure reason
    if let Some(reason) = failure_reason {
        // Pattern: "Agent exited with code 124"
        if let Some(code_str) = reason.strip_prefix("Agent exited with code ")
            && let Ok(code) = code_str.trim().parse::<i32>()
        {
            exit_code = Some(code);
        }
        stderr = reason.clone();
    }

    // Try to read more detailed error information from output file
    if let Ok(content) = std::fs::read_to_string(output_file) {
        // Look for error patterns in the output file
        let lines = content.lines().collect::<Vec<_>>();

        // Take the last few lines which often contain the error
        let error_lines: Vec<&str> = lines.iter().rev().take(10).cloned().collect();
        let combined_stderr = error_lines.join("\n");

        // If we found more detailed error info, use it
        if !combined_stderr.trim().is_empty() && combined_stderr.len() > stderr.len() {
            stderr = combined_stderr;
        }
    }

    (exit_code, stderr)
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

/// Try to escalate the task's model to the next candidate in the ranked tier list.
/// Records the current model in `tried_models` and sets the new model on the task.
/// For static profiles (or when no profile is active), this is a no-op.
fn try_escalate_model(task: &mut Task, dir: &Path, config: &Config) {
    // Build a temporary tried list that includes the current model for lookup,
    // but only persist it if escalation actually succeeds.
    let mut tried_with_current = task.tried_models.clone();
    if let Some(ref current) = task.model
        && !tried_with_current.contains(current)
    {
        tried_with_current.push(current.clone());
    }

    if let Some(result) = profile::escalate_model(
        dir,
        config.profile.as_deref(),
        task.model.as_deref(),
        &tried_with_current,
        config.coordinator.max_escalation_depth,
    ) {
        // Commit the tried list now that escalation succeeded
        task.tried_models = tried_with_current;
        let old_model = task.model.as_deref().unwrap_or("(default)").to_string();
        task.model = Some(result.model.clone());
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: Some("escalation".to_string()),
            user: Some(workgraph::current_user()),
            message: format!(
                "Retrying with model {} ({}) after {} failed",
                result.model, result.reason, old_model,
            ),
        });
        eprintln!(
            "[coordinator] Model escalation for '{}': {} → {} ({})",
            task.id, old_model, result.model, result.reason,
        );
    }
}

/// Apply a triage verdict to a task.
fn apply_triage_verdict(
    task: &mut Task,
    verdict: &TriageVerdict,
    agent_id: &str,
    pid: u32,
    dir: &Path,
    config: &Config,
) {
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

            // Attempt model escalation (rotate to next model in ranked tier list)
            try_escalate_model(task, dir, config);

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

            // Attempt model escalation (rotate to next model in ranked tier list)
            try_escalate_model(task, dir, config);

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

/// Validates and parses agent metadata.json file with enhanced error handling.
/// Returns Ok(Some((worktree_path, worktree_branch))) on success,
/// Ok(None) if no valid worktree metadata is found, or Err for validation errors.
fn validate_and_parse_agent_metadata(
    metadata_path: &Path,
    agent_id: &str,
) -> Result<Option<(String, String)>> {
    // Check if metadata file exists
    if !metadata_path.exists() {
        eprintln!(
            "[triage] No metadata.json found for agent {} at {:?}",
            agent_id, metadata_path
        );
        return Ok(None);
    }

    // Read metadata file with detailed error context
    let metadata_str = fs::read_to_string(metadata_path).with_context(|| {
        format!(
            "Failed to read metadata.json for agent {} at {:?}",
            agent_id, metadata_path
        )
    })?;

    // Validate that the file is not empty
    if metadata_str.trim().is_empty() {
        eprintln!(
            "[triage] metadata.json for agent {} is empty at {:?}",
            agent_id, metadata_path
        );
        return Ok(None);
    }

    // Parse JSON with enhanced error reporting
    let metadata: serde_json::Value = serde_json::from_str(&metadata_str).with_context(|| {
        format!(
            "Failed to parse metadata.json for agent {} at {:?}. Content: {}",
            agent_id,
            metadata_path,
            metadata_str.chars().take(200).collect::<String>() + "..."
        )
    })?;

    // Validate required fields exist and are valid
    let wt_path_str = metadata
        .get("worktree_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let wt_branch = metadata
        .get("worktree_branch")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    match (wt_path_str, wt_branch) {
        (Some(path), Some(branch)) if !path.trim().is_empty() && !branch.trim().is_empty() => {
            eprintln!(
                "[triage] Successfully parsed metadata for agent {}: worktree={}, branch={}",
                agent_id, path, branch
            );
            Ok(Some((path, branch)))
        }
        (path_opt, branch_opt) => {
            eprintln!(
                "[triage] Invalid or missing worktree metadata for agent {}: path={:?}, branch={:?}",
                agent_id, path_opt, branch_opt
            );
            Ok(None)
        }
    }
}

/// Previously attempted fallback worktree cleanup for dead agents.
/// Now a no-op: worktrees are preserved for forensic inspection and
/// manual archival. Agent work (uncommitted changes, in-progress code,
/// artifacts) must never be automatically destroyed.
fn attempt_fallback_worktree_cleanup(_project_root: &Path, agent_id: &str) -> Result<()> {
    eprintln!(
        "[triage] Agent {} worktree preserved (automatic cleanup disabled — use `wg worktree archive` to clean up manually)",
        agent_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::Task;

    /// Helper: call apply_triage_verdict with a dummy dir and default config
    /// (no profile set → no escalation).
    fn apply_verdict_no_escalation(
        task: &mut Task,
        verdict: &TriageVerdict,
        agent_id: &str,
        pid: u32,
    ) {
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        apply_triage_verdict(task, verdict, agent_id, pid, tmp.path(), &config);
    }

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
        apply_verdict_no_escalation(&mut task, &verdict, "agent-1", 1234);
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
            ..Default::default()
        };
        let verdict = TriageVerdict {
            verdict: "done".to_string(),
            reason: "tests pass".to_string(),
            summary: "implementation complete".to_string(),
        };
        apply_verdict_no_escalation(&mut task, &verdict, "agent-1", 1234);
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
        apply_verdict_no_escalation(&mut task, &verdict, "agent-1", 1234);
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
        apply_verdict_no_escalation(&mut task, &verdict, "agent-1", 1234);
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
        apply_verdict_no_escalation(&mut task, &verdict, "agent-1", 1234);
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
        apply_verdict_no_escalation(&mut task, &verdict, "agent-1", 1234);
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
            detect_dead_reason(&agent, DEFAULT_REAPER_GRACE_PERIOD_SECS, 60).is_none(),
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

        let reason = detect_dead_reason(&agent, DEFAULT_REAPER_GRACE_PERIOD_SECS, 60);
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

        let reason = detect_dead_reason(&agent, 0, 60);
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
            detect_dead_reason(&agent, DEFAULT_REAPER_GRACE_PERIOD_SECS, 60).is_none(),
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
        let reason = detect_dead_reason(&agent, DEFAULT_REAPER_GRACE_PERIOD_SECS, 60);
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

    // -----------------------------------------------------------------------
    // Model escalation integration with triage verdicts
    // -----------------------------------------------------------------------

    /// Helper: write a ranked tiers file to the temp dir so escalation can find it.
    fn write_ranked_tiers_for_escalation(dir: &std::path::Path) {
        use workgraph::model_benchmarks::{RankedModel, RankedTiers};
        let ranked = RankedTiers {
            fast: vec![],
            standard: vec![
                RankedModel {
                    id: "vendor/std-a".to_string(),
                    name: "Std A".to_string(),
                    popularity_score: 90.0,
                    benchmark_score: 80.0,
                    composite_score: 85.0,
                    tier: "standard".to_string(),
                    input_per_mtok: None,
                    output_per_mtok: None,
                    context_window: None,
                    supports_tools: true,
                    is_curated: true,
                },
                RankedModel {
                    id: "vendor/std-b".to_string(),
                    name: "Std B".to_string(),
                    popularity_score: 70.0,
                    benchmark_score: 60.0,
                    composite_score: 65.0,
                    tier: "standard".to_string(),
                    input_per_mtok: None,
                    output_per_mtok: None,
                    context_window: None,
                    supports_tools: true,
                    is_curated: true,
                },
            ],
            premium: vec![RankedModel {
                id: "vendor/prem-a".to_string(),
                name: "Prem A".to_string(),
                popularity_score: 95.0,
                benchmark_score: 90.0,
                composite_score: 92.0,
                tier: "premium".to_string(),
                input_per_mtok: None,
                output_per_mtok: None,
                context_window: None,
                supports_tools: true,
                is_curated: true,
            }],
        };
        let path = dir.join("profile_ranked_tiers.json");
        let json = serde_json::to_string(&ranked).unwrap();
        std::fs::write(path, json).unwrap();
    }

    #[test]
    fn test_triage_restart_escalates_model() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers_for_escalation(tmp.path());

        let mut config = Config::default();
        config.profile = Some("openrouter".to_string());
        config.coordinator.max_escalation_depth = 3;

        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            model: Some("openrouter:vendor/std-a".to_string()),
            ..Default::default()
        };

        let verdict = TriageVerdict {
            verdict: "restart".to_string(),
            reason: "no progress".to_string(),
            summary: "".to_string(),
        };

        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234, tmp.path(), &config);

        assert_eq!(task.status, Status::Open);
        assert_eq!(task.retry_count, 1);
        // Model should have been escalated from std-a to std-b
        assert_eq!(task.model.as_deref(), Some("openrouter:vendor/std-b"));
        // std-a should be in tried_models
        assert!(
            task.tried_models
                .contains(&"openrouter:vendor/std-a".to_string())
        );
        // Log should mention escalation
        assert!(
            task.log
                .iter()
                .any(|l| l.message.contains("Retrying with model")
                    && l.message.contains("vendor/std-b")),
            "Log should contain escalation message"
        );
    }

    #[test]
    fn test_triage_continue_escalates_model() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers_for_escalation(tmp.path());

        let mut config = Config::default();
        config.profile = Some("openrouter".to_string());

        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: Some("Original".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            model: Some("openrouter:vendor/std-a".to_string()),
            ..Default::default()
        };

        let verdict = TriageVerdict {
            verdict: "continue".to_string(),
            reason: "partial".to_string(),
            summary: "half done".to_string(),
        };

        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234, tmp.path(), &config);

        assert_eq!(task.status, Status::Open);
        // Model should be escalated
        assert_eq!(task.model.as_deref(), Some("openrouter:vendor/std-b"));
    }

    #[test]
    fn test_triage_no_escalation_for_static_profile() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers_for_escalation(tmp.path());

        let mut config = Config::default();
        config.profile = Some("anthropic".to_string());

        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            model: Some("claude:sonnet".to_string()),
            ..Default::default()
        };

        let verdict = TriageVerdict {
            verdict: "restart".to_string(),
            reason: "no progress".to_string(),
            summary: "".to_string(),
        };

        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234, tmp.path(), &config);

        // Model should NOT change for static profiles
        assert_eq!(task.model.as_deref(), Some("claude:sonnet"));
        assert!(task.tried_models.is_empty());
    }

    #[test]
    fn test_triage_escalation_across_tiers() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers_for_escalation(tmp.path());

        let mut config = Config::default();
        config.profile = Some("openrouter".to_string());
        config.coordinator.max_escalation_depth = 3;

        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            model: Some("openrouter:vendor/std-a".to_string()),
            tried_models: vec![
                "openrouter:vendor/std-a".to_string(),
                "openrouter:vendor/std-b".to_string(),
            ],
            ..Default::default()
        };

        let verdict = TriageVerdict {
            verdict: "restart".to_string(),
            reason: "still failing".to_string(),
            summary: "".to_string(),
        };

        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234, tmp.path(), &config);

        // All standard models exhausted → should escalate to premium
        assert_eq!(task.model.as_deref(), Some("openrouter:vendor/prem-a"));
        assert!(
            task.log
                .iter()
                .any(|l| l.message.contains("escalated to premium-class")),
            "Log should mention tier escalation"
        );
    }

    #[test]
    fn test_validate_metadata_valid_file() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("metadata.json");

        let metadata_content = r#"{
            "worktree_path": "/path/to/worktree",
            "worktree_branch": "wg/agent-123/task-456",
            "other_field": "value"
        }"#;

        fs::write(&metadata_path, metadata_content).unwrap();

        let result = validate_and_parse_agent_metadata(&metadata_path, "agent-123").unwrap();
        assert!(result.is_some());

        let (path, branch) = result.unwrap();
        assert_eq!(path, "/path/to/worktree");
        assert_eq!(branch, "wg/agent-123/task-456");
    }

    #[test]
    fn test_validate_metadata_missing_file() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("nonexistent.json");

        let result = validate_and_parse_agent_metadata(&metadata_path, "agent-123").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_validate_metadata_invalid_json() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("metadata.json");

        fs::write(&metadata_path, "invalid json content").unwrap();

        let result = validate_and_parse_agent_metadata(&metadata_path, "agent-123");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to parse metadata.json")
        );
    }

    #[test]
    fn test_validate_metadata_missing_fields() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("metadata.json");

        let metadata_content = r#"{
            "some_other_field": "value"
        }"#;

        fs::write(&metadata_path, metadata_content).unwrap();

        let result = validate_and_parse_agent_metadata(&metadata_path, "agent-123").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_validate_metadata_empty_fields() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("metadata.json");

        let metadata_content = r#"{
            "worktree_path": "",
            "worktree_branch": "   "
        }"#;

        fs::write(&metadata_path, metadata_content).unwrap();

        let result = validate_and_parse_agent_metadata(&metadata_path, "agent-123").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_validate_metadata_empty_file() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("metadata.json");

        fs::write(&metadata_path, "").unwrap();

        let result = validate_and_parse_agent_metadata(&metadata_path, "agent-123").unwrap();
        assert!(result.is_none());
    }
}
