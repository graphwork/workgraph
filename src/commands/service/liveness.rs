//! Liveness detection: sleep-aware stuck agent handling.
//!
//! Phase 1: SleepTracker using CLOCK_MONOTONIC for sleep detection.
//!          Stream staleness tracking per agent.
//!
//! Phase 2: Stuck triage after 2 consecutive stale ticks.
//!          Verdicts: wait, kill-done, kill-restart.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;

use workgraph::config::Config;
use workgraph::graph::{LogEntry, Status};
use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};
use workgraph::stream_event::{self, StreamEvent};

use crate::commands::{graph_path, is_process_alive};

use super::triage::read_truncated_log;

// ── Monotonic clock ─────────────────────────────────────────────────────

/// Read CLOCK_MONOTONIC directly via libc. This clock pauses during system
/// sleep, unlike CLOCK_BOOTTIME. By comparing it against wall-clock time
/// we can detect sleep gaps.
#[cfg(unix)]
fn monotonic_secs() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1_000_000_000.0
}

#[cfg(not(unix))]
fn monotonic_secs() -> f64 {
    // Fallback: no sleep detection on non-Unix
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn wall_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ── SleepTracker ────────────────────────────────────────────────────────

/// Tracks monotonic vs wall-clock drift to detect system sleep, and
/// per-agent stream staleness for stuck detection.
pub struct SleepTracker {
    last_tick_wall: f64,
    last_tick_mono: f64,
    /// Grace period: skip stuck checks until this instant (after wake).
    wake_grace_until: Option<Instant>,
    /// Consecutive stale ticks per agent ID.
    agent_stale_ticks: HashMap<String, u32>,
    /// Last known stream event timestamp (ms) per agent ID.
    agent_last_event_ms: HashMap<String, i64>,
}

impl SleepTracker {
    pub fn new() -> Self {
        Self {
            last_tick_wall: wall_secs(),
            last_tick_mono: monotonic_secs(),
            wake_grace_until: None,
            agent_stale_ticks: HashMap::new(),
            agent_last_event_ms: HashMap::new(),
        }
    }

    /// Called at the start of each coordinator tick. Detects sleep gaps
    /// and manages the grace period. Returns the detected sleep gap in
    /// seconds (0.0 if no sleep detected).
    pub fn tick(&mut self, config: &Config) -> f64 {
        let now_wall = wall_secs();
        let now_mono = monotonic_secs();

        let wall_elapsed = now_wall - self.last_tick_wall;
        let mono_elapsed = now_mono - self.last_tick_mono;
        let sleep_gap = wall_elapsed - mono_elapsed;

        let threshold = config.agent.sleep_gap_threshold.unwrap_or(30) as f64;

        if sleep_gap > threshold {
            eprintln!(
                "[liveness] Sleep detected: gap={:.1}s (wall={:.1}s, mono={:.1}s)",
                sleep_gap, wall_elapsed, mono_elapsed
            );
            let grace_secs = config.agent.wake_grace_period.unwrap_or(120);
            self.wake_grace_until =
                Some(Instant::now() + std::time::Duration::from_secs(grace_secs));
            // Reset all stale counters — agents may need time to reconnect.
            self.agent_stale_ticks.clear();
        }

        self.last_tick_wall = now_wall;
        self.last_tick_mono = now_mono;

        sleep_gap.max(0.0)
    }

    /// Returns true if we are within the post-wake grace period.
    pub fn in_grace_period(&self) -> bool {
        self.wake_grace_until
            .map(|deadline| Instant::now() < deadline)
            .unwrap_or(false)
    }

    /// Clean up tracking state for agents that are no longer alive.
    pub fn prune_dead_agents(&mut self, alive_agent_ids: &[&str]) {
        self.agent_stale_ticks
            .retain(|id, _| alive_agent_ids.contains(&id.as_str()));
        self.agent_last_event_ms
            .retain(|id, _| alive_agent_ids.contains(&id.as_str()));
    }
}

// ── Stream staleness checking ───────────────────────────────────────────

/// Check the last event timestamp from an agent's stream file.
fn last_stream_event_ms(agent: &AgentEntry) -> Option<i64> {
    let output_path = std::path::Path::new(&agent.output_file);
    let agent_dir = output_path.parent()?;

    // Try unified stream.jsonl first
    let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);
    if stream_path.exists() {
        if let Ok((events, _)) = stream_event::read_stream_events(&stream_path, 0) {
            return events.last().map(|e| e.timestamp_ms());
        }
    }

    // Try raw_stream.jsonl (Claude CLI)
    let raw_path = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
    if raw_path.exists() {
        if let Ok((events, _)) = stream_event::translate_claude_stream(&raw_path, 0) {
            return events.last().map(|e| e.timestamp_ms());
        }
    }

    None
}

/// Check whether the last stream event is a ToolStart without a matching ToolEnd.
/// Returns the tool name if so.
fn last_in_progress_tool(agent: &AgentEntry) -> Option<String> {
    let output_path = std::path::Path::new(&agent.output_file);
    let agent_dir = output_path.parent()?;

    let events = {
        let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);
        if stream_path.exists() {
            stream_event::read_stream_events(&stream_path, 0)
                .ok()
                .map(|(e, _)| e)
        } else {
            let raw_path = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
            if raw_path.exists() {
                stream_event::translate_claude_stream(&raw_path, 0)
                    .ok()
                    .map(|(e, _)| e)
            } else {
                None
            }
        }
    }?;

    // Walk events to find unmatched ToolStart
    let mut in_progress_tool: Option<String> = None;
    for event in &events {
        match event {
            StreamEvent::ToolStart { name, .. } => {
                in_progress_tool = Some(name.clone());
            }
            StreamEvent::ToolEnd { .. } => {
                in_progress_tool = None;
            }
            _ => {}
        }
    }
    in_progress_tool
}

// ── Stuck agent detection (called from coordinator tick) ────────────────

/// Check all alive agents for staleness. Updates internal tracking state.
/// Returns a list of (agent_id, result) for agents that triggered triage.
pub fn check_stuck_agents(
    tracker: &mut SleepTracker,
    registry: &AgentRegistry,
    config: &Config,
) -> Vec<(String, u64, Option<String>)> {
    if tracker.in_grace_period() {
        eprintln!("[liveness] Within wake grace period, skipping stuck checks");
        return vec![];
    }

    let stale_threshold_ms =
        (config.agent.stale_threshold.unwrap_or(10) as i64) * 60 * 1000;
    let tick_threshold = config.agent.stale_tick_threshold.unwrap_or(2);
    let now_ms = stream_event::now_ms();

    let mut triage_targets = vec![];

    // Collect alive agent IDs for pruning
    let alive_agents: Vec<&AgentEntry> = registry
        .agents
        .values()
        .filter(|a| a.is_alive() && is_process_alive(a.pid))
        .collect();
    let alive_ids: Vec<&str> = alive_agents.iter().map(|a| a.id.as_str()).collect();
    tracker.prune_dead_agents(&alive_ids);

    for agent in &alive_agents {
        let last_event_ms = last_stream_event_ms(agent);

        if let Some(ts) = last_event_ms {
            // Check if this is newer than what we last saw
            let prev = tracker.agent_last_event_ms.get(&agent.id).copied();
            tracker
                .agent_last_event_ms
                .insert(agent.id.clone(), ts);

            if prev.map(|p| ts > p).unwrap_or(false) {
                // Agent produced new events since last check — reset stale counter
                tracker.agent_stale_ticks.remove(&agent.id);
                continue;
            }

            // Check absolute staleness
            let stale_ms = now_ms - ts;
            if stale_ms <= stale_threshold_ms {
                // Not stale yet
                continue;
            }

            // Check for in-progress tool (extend window)
            if let Some(tool_name) = last_in_progress_tool(agent) {
                eprintln!(
                    "[liveness] Agent {} has in-progress tool '{}', extending window",
                    agent.id, tool_name
                );
                // Don't increment stale counter for in-progress tools
                continue;
            }

            // Increment stale counter
            let count = tracker
                .agent_stale_ticks
                .entry(agent.id.clone())
                .or_insert(0);
            *count += 1;

            if *count >= tick_threshold {
                let stale_secs = (stale_ms / 1000) as u64;
                let last_tool = last_in_progress_tool(agent);
                eprintln!(
                    "[liveness] Agent {} stale for {}s ({} consecutive ticks) — triggering triage",
                    agent.id, stale_secs, count
                );
                triage_targets.push((agent.id.clone(), stale_secs, last_tool));
            } else {
                eprintln!(
                    "[liveness] Agent {} stale tick {}/{} ({}s since last event)",
                    agent.id,
                    count,
                    tick_threshold,
                    stale_ms / 1000
                );
            }
        }
        // If no stream events at all, we can't determine staleness — skip.
    }

    triage_targets
}

// ── Stuck triage (Phase 2) ──────────────────────────────────────────────

/// Verdict from the stuck-alive triage LLM call.
#[derive(Debug, serde::Deserialize)]
struct StuckTriageVerdict {
    /// One of "wait", "kill-done", "kill-restart"
    verdict: String,
    #[serde(default)]
    reason: String,
}

/// Build the stuck-alive triage prompt.
fn build_stuck_triage_prompt(
    task_id: &str,
    task_title: &str,
    log_content: &str,
    stale_duration_secs: u64,
    last_tool: Option<&str>,
) -> String {
    let tool_info = last_tool
        .map(|t| format!("- **Last in-progress tool:** {}", t))
        .unwrap_or_default();

    format!(
        r#"You are a triage system for a software development task coordinator.

An agent is STILL RUNNING (PID alive) but has produced NO stream events for {stale_duration_secs} seconds.
This may indicate the agent is stuck (broken connection post-sleep, hung process) or doing legitimate long-running work.

## Task Information
- **ID:** {task_id}
- **Title:** {task_title}
{tool_info}

## Agent Output Log (last 50KB)
```
{log_content}
```

## Instructions
Based on the output log and staleness duration, respond with ONLY a JSON object (no markdown fences):

{{
  "verdict": "<wait|kill-done|kill-restart>",
  "reason": "<one-sentence explanation>"
}}

Verdicts:
- **"wait"**: Agent likely still working (long build, large file operation). Reset stale counter, check again later.
- **"kill-done"**: Agent appears to have finished but hung on cleanup/exit. Kill it and mark task Done.
- **"kill-restart"**: Agent is truly stuck (broken connection, infinite loop). Kill it and restart the task.

Be conservative: prefer "wait" if the log shows recent meaningful progress or a long-running operation is plausible."#
    )
}

/// Extract JSON from potentially noisy LLM output (reuses triage pattern).
fn extract_json(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

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

    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if start <= end {
                let candidate = &trimmed[start..=end];
                if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                    return Some(candidate.to_string());
                }
            }
        }
    }

    None
}

/// Run stuck-alive triage for a single agent. Returns the verdict.
fn run_stuck_triage(
    config: &Config,
    task_id: &str,
    task_title: &str,
    output_file: &str,
    stale_duration_secs: u64,
    last_tool: Option<&str>,
) -> Result<StuckTriageVerdict> {
    let max_log_bytes = config.agency.triage_max_log_bytes.unwrap_or(50_000);
    let timeout_secs = config.agency.triage_timeout.unwrap_or(30);
    let log_content = read_truncated_log(output_file, max_log_bytes);
    let prompt = build_stuck_triage_prompt(
        task_id,
        task_title,
        &log_content,
        stale_duration_secs,
        last_tool,
    );

    let result = workgraph::service::llm::run_lightweight_llm_call(
        config,
        workgraph::config::DispatchRole::Triage,
        &prompt,
        timeout_secs,
    )
    .context("Stuck triage LLM call failed")?;

    let json_str = extract_json(&result.text)
        .ok_or_else(|| anyhow::anyhow!("No valid JSON found in stuck triage output"))?;

    let verdict: StuckTriageVerdict = serde_json::from_str(&json_str)
        .with_context(|| format!("Failed to parse stuck triage JSON: {}", json_str))?;

    match verdict.verdict.as_str() {
        "wait" | "kill-done" | "kill-restart" => Ok(verdict),
        other => anyhow::bail!(
            "Invalid stuck triage verdict '{}', expected wait/kill-done/kill-restart",
            other
        ),
    }
}

/// Handle stuck agents: run triage and apply verdicts.
/// Called from the coordinator tick after `check_stuck_agents` identifies targets.
pub fn handle_stuck_agents(
    tracker: &mut SleepTracker,
    dir: &Path,
    triage_targets: Vec<(String, u64, Option<String>)>,
    config: &Config,
) -> Result<()> {
    if triage_targets.is_empty() || !config.agency.auto_triage {
        return Ok(());
    }

    let gp = graph_path(dir);
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    workgraph::parser::mutate_graph(&gp, |graph| -> Result<()> {

    for (agent_id, stale_secs, last_tool) in &triage_targets {
        let (task_id, _task_title, pid, output_file) = {
            let agent = match locked_registry.get_agent(agent_id) {
                Some(a) => a,
                None => continue,
            };
            // Re-verify the agent is still alive
            if !agent.is_alive() || !is_process_alive(agent.pid) {
                continue;
            }
            (
                agent.task_id.clone(),
                String::new(), // will fill from graph
                agent.pid,
                agent.output_file.clone(),
            )
        };

        let task_title_resolved = graph
            .get_task(&task_id)
            .map(|t| t.title.clone())
            .unwrap_or_else(|| task_id.clone());

        // Check task is still InProgress before triaging
        if let Some(task) = graph.get_task(&task_id) {
            if task.status != Status::InProgress {
                continue;
            }
        } else {
            continue;
        }

        eprintln!(
            "[liveness] Running stuck triage for agent {} (task '{}', stale {}s)",
            agent_id, task_id, stale_secs
        );

        match run_stuck_triage(
            config,
            &task_id,
            &task_title_resolved,
            &output_file,
            *stale_secs,
            last_tool.as_deref(),
        ) {
            Ok(verdict) => {
                eprintln!(
                    "[liveness] Stuck triage verdict for '{}': {} — {}",
                    task_id, verdict.verdict, verdict.reason
                );

                match verdict.verdict.as_str() {
                    "wait" => {
                        // Reset stale counter, will check again next tick
                        tracker.agent_stale_ticks.remove(agent_id);
                        if let Some(task) = graph.get_task_mut(&task_id) {
                            task.log.push(LogEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                actor: Some("liveness".to_string()),
                                message: format!(
                                    "Stuck triage: wait (stale {}s) — {}",
                                    stale_secs, verdict.reason
                                ),
                            });
                        }
                    }
                    "kill-done" => {
                        // Kill agent, mark task Done
                        eprintln!(
                            "[liveness] Killing stuck agent {} (PID {}) — verdict: kill-done",
                            agent_id, pid
                        );
                        let _ = workgraph::service::kill_process_graceful(pid, 5);

                        if let Some(agent) = locked_registry.get_agent_mut(agent_id) {
                            agent.status = AgentStatus::Dead;
                            if agent.completed_at.is_none() {
                                agent.completed_at = Some(Utc::now().to_rfc3339());
                            }
                        }

                        if let Some(task) = graph.get_task_mut(&task_id) {
                            task.status = Status::Done;
                            task.completed_at = Some(Utc::now().to_rfc3339());
                            task.log.push(LogEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                actor: Some("liveness".to_string()),
                                message: format!(
                                    "Stuck triage: kill-done (agent '{}' PID {}, stale {}s) — {}",
                                    agent_id, pid, stale_secs, verdict.reason
                                ),
                            });
                        }
                        tracker.agent_stale_ticks.remove(agent_id);
                    }
                    "kill-restart" => {
                        // Kill agent, reset task to Open for reassignment
                        eprintln!(
                            "[liveness] Killing stuck agent {} (PID {}) — verdict: kill-restart",
                            agent_id, pid
                        );
                        let _ = workgraph::service::kill_process_graceful(pid, 5);

                        if let Some(agent) = locked_registry.get_agent_mut(agent_id) {
                            agent.status = AgentStatus::Dead;
                            if agent.completed_at.is_none() {
                                agent.completed_at = Some(Utc::now().to_rfc3339());
                            }
                        }

                        if let Some(task) = graph.get_task_mut(&task_id) {
                            task.status = Status::Open;
                            task.assigned = None;
                            task.session_id = None;
                            task.retry_count += 1;
                            task.log.push(LogEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                actor: Some("liveness".to_string()),
                                message: format!(
                                    "Stuck triage: kill-restart (agent '{}' PID {}, stale {}s) — {}",
                                    agent_id, pid, stale_secs, verdict.reason
                                ),
                            });
                        }
                        tracker.agent_stale_ticks.remove(agent_id);
                    }
                    _ => {} // validated above, shouldn't reach
                }
            }
            Err(e) => {
                eprintln!(
                    "[liveness] Stuck triage failed for agent {} (task '{}'): {}",
                    agent_id, task_id, e
                );
                // Don't reset counter — will try again next tick
            }
        }
    }

    Ok(())
    }).context("Failed to save graph after stuck triage")?;

    locked_registry.save_ref()?;

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sleep_tracker_new() {
        let tracker = SleepTracker::new();
        assert!(tracker.agent_stale_ticks.is_empty());
        assert!(tracker.wake_grace_until.is_none());
        assert!(!tracker.in_grace_period());
    }

    #[test]
    fn test_sleep_tracker_tick_no_sleep() {
        let mut tracker = SleepTracker::new();
        let config = Config::default();
        // Immediate tick — no sleep gap
        let gap = tracker.tick(&config);
        // Gap should be very small (< 1 second)
        assert!(gap < 1.0, "Expected small gap, got {}", gap);
        assert!(!tracker.in_grace_period());
    }

    #[test]
    fn test_sleep_tracker_grace_period() {
        let mut tracker = SleepTracker::new();
        // Manually set grace period
        tracker.wake_grace_until =
            Some(Instant::now() + std::time::Duration::from_secs(120));
        assert!(tracker.in_grace_period());

        // Expired grace period
        tracker.wake_grace_until =
            Some(Instant::now() - std::time::Duration::from_secs(1));
        assert!(!tracker.in_grace_period());
    }

    #[test]
    fn test_sleep_tracker_prune_dead_agents() {
        let mut tracker = SleepTracker::new();
        tracker.agent_stale_ticks.insert("agent-1".to_string(), 1);
        tracker.agent_stale_ticks.insert("agent-2".to_string(), 2);
        tracker.agent_last_event_ms.insert("agent-1".to_string(), 100);
        tracker.agent_last_event_ms.insert("agent-2".to_string(), 200);

        tracker.prune_dead_agents(&["agent-1"]);

        assert!(tracker.agent_stale_ticks.contains_key("agent-1"));
        assert!(!tracker.agent_stale_ticks.contains_key("agent-2"));
        assert!(tracker.agent_last_event_ms.contains_key("agent-1"));
        assert!(!tracker.agent_last_event_ms.contains_key("agent-2"));
    }

    #[test]
    fn test_build_stuck_triage_prompt_contains_info() {
        let prompt = build_stuck_triage_prompt(
            "task-1",
            "Fix the bug",
            "some log output",
            600,
            Some("Bash"),
        );
        assert!(prompt.contains("task-1"));
        assert!(prompt.contains("Fix the bug"));
        assert!(prompt.contains("600"));
        assert!(prompt.contains("Bash"));
        assert!(prompt.contains("some log output"));
        assert!(prompt.contains("wait"));
        assert!(prompt.contains("kill-done"));
        assert!(prompt.contains("kill-restart"));
    }

    #[test]
    fn test_build_stuck_triage_prompt_no_tool() {
        let prompt = build_stuck_triage_prompt(
            "task-2",
            "Add feature",
            "log text",
            300,
            None,
        );
        assert!(prompt.contains("task-2"));
        assert!(!prompt.contains("Last in-progress tool"));
    }

    #[test]
    fn test_extract_json_plain() {
        let input = r#"{"verdict": "wait", "reason": "still building"}"#;
        let result = extract_json(input).unwrap();
        let parsed: StuckTriageVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.verdict, "wait");
    }

    #[test]
    fn test_extract_json_with_fences() {
        let input =
            "```json\n{\"verdict\": \"kill-done\", \"reason\": \"finished\"}\n```";
        let result = extract_json(input).unwrap();
        let parsed: StuckTriageVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.verdict, "kill-done");
    }

    #[test]
    fn test_extract_json_garbage() {
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn test_monotonic_secs_increases() {
        let t1 = monotonic_secs();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = monotonic_secs();
        assert!(t2 > t1);
    }

    #[test]
    fn test_sleep_detection_simulated() {
        // Simulate a sleep gap by manipulating the tracker's stored timestamps
        let mut tracker = SleepTracker::new();
        let config = Config::default();

        // Pretend the last tick was 60 seconds ago in wall time but only
        // a fraction of a second in mono time. We do this by pushing
        // last_tick_wall back.
        tracker.last_tick_wall = wall_secs() - 60.0;
        // last_tick_mono stays at current mono (as if mono didn't advance during sleep)

        let gap = tracker.tick(&config);
        // The gap should be approximately 60 seconds
        assert!(gap > 50.0, "Expected large gap, got {}", gap);
        assert!(
            tracker.in_grace_period(),
            "Should be in grace period after sleep detection"
        );
    }
}
