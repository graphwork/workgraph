//! Zero-output agent detection and circuit-breaking respawn.
//!
//! Detects agents whose API call never returns (0 bytes written to stream files
//! for extended periods), kills them, and manages respawn with per-task circuit
//! breakers and global API-down detection.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, modify_graph};
use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};
use workgraph::stream_event;

use crate::commands::{graph_path, is_process_alive};

/// Threshold after which a zero-output agent is considered a zombie and killed.
const ZERO_OUTPUT_KILL_THRESHOLD: Duration = Duration::from_secs(5 * 60);

/// Maximum consecutive zero-output respawns per task before circuit-breaking.
const MAX_ZERO_OUTPUT_RESPAWNS: u32 = 2;

/// Tag applied to tasks that are circuit-broken due to repeated zero-output failures.
const CIRCUIT_BROKEN_TAG: &str = "zero-output-circuit-broken";

/// Fraction of alive agents with zero output that triggers global API-down detection.
const GLOBAL_OUTAGE_RATIO: f64 = 0.5;

/// Minimum alive agents before global outage detection kicks in.
const GLOBAL_OUTAGE_MIN_AGENTS: usize = 2;

/// Maximum backoff duration for global spawn pause.
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Initial backoff duration for global spawn pause.
const INITIAL_BACKOFF: Duration = Duration::from_secs(60);

/// Result of a zero-output detection sweep.
#[derive(Debug, Default)]
pub struct ZeroOutputSweepResult {
    /// Agents detected as zero-output zombies and killed.
    pub killed: Vec<ZeroOutputKill>,
    /// Tasks that hit the per-task circuit breaker.
    pub circuit_broken_tasks: Vec<String>,
    /// Whether global API-down was detected.
    pub global_outage_detected: bool,
}

/// Details of a killed zero-output agent.
#[derive(Debug)]
pub struct ZeroOutputKill {
    pub agent_id: String,
    pub task_id: String,
    pub pid: u32,
    pub age_secs: u64,
}

/// Tracks per-task zero-output respawn counts.
///
/// Persisted as a JSON file in the service directory so state survives daemon restarts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ZeroOutputState {
    /// Consecutive zero-output spawn count per task ID.
    pub task_respawn_counts: HashMap<String, u32>,
    /// Global backoff state.
    #[serde(default)]
    pub global_backoff: Option<GlobalBackoffState>,
}

/// Persistent global backoff state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobalBackoffState {
    /// When the current backoff period expires (ISO 8601).
    pub resume_after: String,
    /// Current backoff duration in seconds.
    pub backoff_secs: u64,
    /// Whether a probe agent has been dispatched.
    pub probe_dispatched: bool,
}

impl ZeroOutputState {
    fn state_path(dir: &Path) -> std::path::PathBuf {
        dir.join("service").join("zero_output_state.json")
    }

    pub fn load(dir: &Path) -> Self {
        let path = Self::state_path(dir);
        if path.exists()
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(state) = serde_json::from_str(&content)
        {
            return state;
        }
        Self::default()
    }

    pub fn save(&self, dir: &Path) {
        let path = Self::state_path(dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, content);
        }
    }

    /// Record a zero-output kill for a task. Returns true if the task is now
    /// circuit-broken (exceeded max respawns).
    pub fn record_zero_output_kill(&mut self, task_id: &str) -> bool {
        let count = self
            .task_respawn_counts
            .entry(task_id.to_string())
            .or_insert(0);
        *count += 1;
        *count > MAX_ZERO_OUTPUT_RESPAWNS
    }

    /// Reset the respawn counter for a task (e.g., when it produces output successfully).
    pub fn reset_task(&mut self, task_id: &str) {
        self.task_respawn_counts.remove(task_id);
    }

    /// Check if a task is circuit-broken.
    pub fn is_circuit_broken(&self, task_id: &str) -> bool {
        self.task_respawn_counts
            .get(task_id)
            .map(|c| *c > MAX_ZERO_OUTPUT_RESPAWNS)
            .unwrap_or(false)
    }

    /// Activate global backoff with exponential increase.
    pub fn activate_global_backoff(&mut self) {
        let backoff_secs = match &self.global_backoff {
            Some(existing) => {
                // Double the backoff, capped at MAX_BACKOFF
                (existing.backoff_secs * 2).min(MAX_BACKOFF.as_secs())
            }
            None => INITIAL_BACKOFF.as_secs(),
        };

        let resume_after = Utc::now() + chrono::Duration::seconds(backoff_secs as i64);
        self.global_backoff = Some(GlobalBackoffState {
            resume_after: resume_after.to_rfc3339(),
            backoff_secs,
            probe_dispatched: false,
        });
    }

    /// Check if global spawning is paused due to backoff.
    pub fn is_spawn_paused(&self) -> bool {
        if let Some(ref backoff) = self.global_backoff
            && let Ok(resume) = backoff.resume_after.parse::<DateTime<Utc>>()
        {
            return Utc::now() < resume;
        }
        false
    }

    /// Clear global backoff (probe succeeded, API is back).
    pub fn clear_global_backoff(&mut self) {
        self.global_backoff = None;
    }

    /// Mark that a probe agent has been dispatched during backoff.
    pub fn mark_probe_dispatched(&mut self) {
        if let Some(ref mut backoff) = self.global_backoff {
            backoff.probe_dispatched = true;
        }
    }

    /// Check if a probe has already been dispatched in the current backoff period.
    pub fn is_probe_dispatched(&self) -> bool {
        self.global_backoff
            .as_ref()
            .map(|b| b.probe_dispatched)
            .unwrap_or(false)
    }
}

/// Check whether an agent has zero output (no stream events written).
///
/// Returns `Some(age_secs)` if the agent has zero output, has been alive
/// longer than the kill threshold, and has no active child processes.
/// Returns `None` otherwise.
fn check_zero_output(agent: &AgentEntry) -> Option<u64> {
    if !agent.is_alive() {
        return None;
    }

    let output_path = std::path::Path::new(&agent.output_file);
    let agent_dir = output_path.parent()?;

    // Check raw_stream.jsonl size (Claude CLI agents)
    let raw_path = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
    let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);

    let has_output = (raw_path.exists() && file_has_content(&raw_path))
        || (stream_path.exists() && file_has_content(&stream_path));

    if has_output {
        return None;
    }

    // Agent has zero output — check how old it is
    let started = DateTime::parse_from_rfc3339(&agent.started_at).ok()?;
    let age = (Utc::now() - started.with_timezone(&Utc)).num_seconds();
    if age < 0 {
        return None;
    }

    let age_secs = age as u64;
    if age_secs >= ZERO_OUTPUT_KILL_THRESHOLD.as_secs() {
        // Don't flag as zero-output if agent has active child processes —
        // it may be waiting on a subprocess (e.g., slow model API startup,
        // compilation, or sub-agent initialization)
        if workgraph::service::has_active_children(agent.pid) {
            return None;
        }
        Some(age_secs)
    } else {
        None
    }
}

/// Check if a file exists and has more than 0 bytes of content.
fn file_has_content(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// Run the zero-output detection sweep.
///
/// This should be called from the coordinator tick, after the liveness cleanup.
/// It:
/// 1. Identifies agents with zero output past the threshold
/// 2. Kills them and updates registry + graph
/// 3. Tracks per-task circuit breaker state
/// 4. Detects global API-down conditions
pub fn sweep_zero_output_agents(dir: &Path) -> ZeroOutputSweepResult {
    let mut result = ZeroOutputSweepResult::default();
    let mut state = ZeroOutputState::load(dir);

    // Load registry to find alive agents
    let registry = match AgentRegistry::load(dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[zero-output] Failed to load registry: {}", e);
            return result;
        }
    };

    let alive_agents: Vec<&AgentEntry> = registry
        .agents
        .values()
        .filter(|a| a.is_alive() && is_process_alive(a.pid))
        .collect();

    if alive_agents.is_empty() {
        // No alive agents — clear any global backoff since there's nothing to measure
        if state.global_backoff.is_some() {
            state.clear_global_backoff();
            state.save(dir);
        }
        return result;
    }

    // Identify zero-output agents
    let mut zero_output_agents: Vec<(&AgentEntry, u64)> = Vec::new();
    for agent in &alive_agents {
        if let Some(age_secs) = check_zero_output(agent) {
            zero_output_agents.push((agent, age_secs));
        }
    }

    // Global API-down detection: if >=50% of alive agents have zero output
    if alive_agents.len() >= GLOBAL_OUTAGE_MIN_AGENTS {
        let zero_ratio = zero_output_agents.len() as f64 / alive_agents.len() as f64;
        if zero_ratio >= GLOBAL_OUTAGE_RATIO {
            result.global_outage_detected = true;
            state.activate_global_backoff();
            eprintln!(
                "[zero-output] GLOBAL OUTAGE DETECTED: {}/{} agents have zero output ({}%). \
                 Pausing spawns with {}s backoff.",
                zero_output_agents.len(),
                alive_agents.len(),
                (zero_ratio * 100.0) as u32,
                state
                    .global_backoff
                    .as_ref()
                    .map(|b| b.backoff_secs)
                    .unwrap_or(0)
            );
        }
    }

    // If no agents are zero-output zombies past threshold, we're done
    if zero_output_agents.is_empty() {
        // If we had a global backoff but agents are now producing output, clear it
        if state.global_backoff.is_some() && !state.is_spawn_paused() {
            state.clear_global_backoff();
            eprintln!("[zero-output] Global backoff cleared — agents producing output.");
        }
        state.save(dir);
        return result;
    }

    // Kill zero-output agents and update state
    let graph_path = graph_path(dir);

    // Collect kill targets before modifying registry
    let kill_targets: Vec<(String, String, u32, u64)> = zero_output_agents
        .iter()
        .map(|(a, age)| (a.id.clone(), a.task_id.clone(), a.pid, *age))
        .collect();

    // Kill processes
    for (agent_id, task_id, pid, age_secs) in &kill_targets {
        eprintln!(
            "[zero-output] Killing zero-output agent {} (task {}, PID {}, alive {}s)",
            agent_id, task_id, pid, age_secs
        );
        if let Err(e) = workgraph::service::kill_process_force(*pid) {
            eprintln!(
                "[zero-output] Failed to kill PID {} (agent {}): {}",
                pid, agent_id, e
            );
        }
    }

    // Update registry: mark killed agents as Dead
    if let Ok(mut locked_registry) = AgentRegistry::load_locked(dir) {
        let now = Utc::now().to_rfc3339();
        for (agent_id, _, _, _) in &kill_targets {
            if let Some(agent) = locked_registry.get_agent_mut(agent_id) {
                agent.status = AgentStatus::Dead;
                if agent.completed_at.is_none() {
                    agent.completed_at = Some(now.clone());
                }
            }
        }
        if let Err(e) = locked_registry.save_ref() {
            eprintln!("[zero-output] Failed to save registry: {}", e);
        }
    }

    // Update graph: handle per-task circuit breaking and task reset
    if let Ok(mut graph) = load_graph(&graph_path) {
        let mut graph_modified = false;

        for (agent_id, task_id, pid, age_secs) in &kill_targets {
            let is_circuit_broken = state.record_zero_output_kill(task_id);

            if let Some(task) = graph.get_task_mut(task_id) {
                if task.status != Status::InProgress {
                    continue;
                }

                if is_circuit_broken {
                    // Circuit-broken: mark incomplete for evaluator review (not auto-fail)
                    task.status = Status::Incomplete;
                    task.assigned = None;
                    if !task.tags.contains(&CIRCUIT_BROKEN_TAG.to_string()) {
                        task.tags.push(CIRCUIT_BROKEN_TAG.to_string());
                    }
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: Some("zero-output-detector".to_string()),
                        user: Some(workgraph::current_user()),
                        message: format!(
                            "Circuit breaker tripped: agent '{}' (PID {}) killed after {}s \
                             with zero output. Max respawns ({}) exceeded — marked incomplete for evaluator review.",
                            agent_id, pid, age_secs, MAX_ZERO_OUTPUT_RESPAWNS
                        ),
                    });
                    result.circuit_broken_tasks.push(task_id.clone());
                    eprintln!(
                        "[zero-output] CIRCUIT BREAKER: Task '{}' marked incomplete after {} zero-output spawns",
                        task_id,
                        state
                            .task_respawn_counts
                            .get(task_id.as_str())
                            .unwrap_or(&0)
                    );
                } else {
                    // Not yet circuit-broken: reset task for respawn
                    task.status = Status::Open;
                    task.assigned = None;
                    task.retry_count += 1;
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: Some("zero-output-detector".to_string()),
                        user: Some(workgraph::current_user()),
                        message: format!(
                            "Zero-output agent '{}' (PID {}) killed after {}s. \
                             Task reset for respawn (attempt {}/{}).",
                            agent_id,
                            pid,
                            age_secs,
                            state
                                .task_respawn_counts
                                .get(task_id.as_str())
                                .unwrap_or(&1),
                            MAX_ZERO_OUTPUT_RESPAWNS
                        ),
                    });
                }
                graph_modified = true;
            }

            result.killed.push(ZeroOutputKill {
                agent_id: agent_id.clone(),
                task_id: task_id.clone(),
                pid: *pid,
                age_secs: *age_secs,
            });
        }

        if graph_modified
            && let Err(e) = modify_graph(&graph_path, |fresh_graph| {
                // Replay mutations from our local graph
                for kill in &result.killed {
                    if let Some(local) = graph.get_task(&kill.task_id)
                        && let Some(fresh) = fresh_graph.get_task_mut(&kill.task_id)
                    {
                        fresh.status = local.status;
                        fresh.assigned = local.assigned.clone();
                        fresh.retry_count = local.retry_count;
                        fresh.failure_reason = local.failure_reason.clone();
                        fresh.log = local.log.clone();
                    }
                }
                true
            })
        {
            eprintln!("[zero-output] Failed to save graph: {}", e);
        }
    }

    state.save(dir);
    result
}

/// Check if spawning should be paused due to global API-down backoff.
///
/// Called from the coordinator tick before spawning new agents.
/// Returns `true` if spawning should be paused.
pub fn should_pause_spawning(dir: &Path) -> bool {
    let state = ZeroOutputState::load(dir);
    state.is_spawn_paused()
}

/// Reset the zero-output counter for a task that has produced output.
///
/// Should be called when an agent successfully starts producing output,
/// so that the circuit breaker resets.
pub fn reset_task_counter(dir: &Path, task_id: &str) {
    let mut state = ZeroOutputState::load(dir);
    if state.task_respawn_counts.contains_key(task_id) {
        state.reset_task(task_id);
        state.save(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_zero_output_state_save_load() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("service")).unwrap();

        let mut state = ZeroOutputState::default();
        state.task_respawn_counts.insert("task-1".into(), 1);
        state.save(dir);

        let loaded = ZeroOutputState::load(dir);
        assert_eq!(loaded.task_respawn_counts.get("task-1"), Some(&1));
    }

    #[test]
    fn test_circuit_breaker_trip() {
        let mut state = ZeroOutputState::default();

        // First kill — not tripped
        assert!(!state.record_zero_output_kill("task-1"));
        assert_eq!(state.task_respawn_counts["task-1"], 1);

        // Second kill — not tripped
        assert!(!state.record_zero_output_kill("task-1"));
        assert_eq!(state.task_respawn_counts["task-1"], 2);

        // Third kill — tripped (count > MAX_ZERO_OUTPUT_RESPAWNS which is 2)
        assert!(state.record_zero_output_kill("task-1"));
        assert_eq!(state.task_respawn_counts["task-1"], 3);
    }

    #[test]
    fn test_circuit_breaker_independent_tasks() {
        let mut state = ZeroOutputState::default();

        assert!(!state.record_zero_output_kill("task-a"));
        assert!(!state.record_zero_output_kill("task-a"));
        assert!(state.record_zero_output_kill("task-a")); // tripped

        // task-b is independent
        assert!(!state.record_zero_output_kill("task-b"));
        assert!(!state.is_circuit_broken("task-b"));
        assert!(state.is_circuit_broken("task-a"));
    }

    #[test]
    fn test_reset_task() {
        let mut state = ZeroOutputState::default();
        state.record_zero_output_kill("task-1");
        assert!(!state.is_circuit_broken("task-1"));

        state.reset_task("task-1");
        assert!(!state.is_circuit_broken("task-1"));
        assert!(!state.task_respawn_counts.contains_key("task-1"));
    }

    #[test]
    fn test_global_backoff_activation() {
        let mut state = ZeroOutputState::default();
        assert!(!state.is_spawn_paused());

        state.activate_global_backoff();
        assert!(state.is_spawn_paused());
        assert_eq!(
            state.global_backoff.as_ref().unwrap().backoff_secs,
            INITIAL_BACKOFF.as_secs()
        );
    }

    #[test]
    fn test_global_backoff_exponential() {
        let mut state = ZeroOutputState::default();

        state.activate_global_backoff();
        assert_eq!(
            state.global_backoff.as_ref().unwrap().backoff_secs,
            60 // INITIAL_BACKOFF
        );

        state.activate_global_backoff();
        assert_eq!(state.global_backoff.as_ref().unwrap().backoff_secs, 120);

        state.activate_global_backoff();
        assert_eq!(state.global_backoff.as_ref().unwrap().backoff_secs, 240);

        // Keep doubling until MAX_BACKOFF
        state.activate_global_backoff();
        assert_eq!(state.global_backoff.as_ref().unwrap().backoff_secs, 480);

        state.activate_global_backoff();
        assert_eq!(state.global_backoff.as_ref().unwrap().backoff_secs, 900); // MAX_BACKOFF = 15 min
    }

    #[test]
    fn test_global_backoff_clear() {
        let mut state = ZeroOutputState::default();
        state.activate_global_backoff();
        assert!(state.is_spawn_paused());

        state.clear_global_backoff();
        assert!(!state.is_spawn_paused());
    }

    #[test]
    fn test_probe_dispatch_tracking() {
        let mut state = ZeroOutputState::default();
        assert!(!state.is_probe_dispatched());

        state.activate_global_backoff();
        assert!(!state.is_probe_dispatched());

        state.mark_probe_dispatched();
        assert!(state.is_probe_dispatched());
    }

    #[test]
    fn test_file_has_content() {
        let temp = TempDir::new().unwrap();

        let empty_file = temp.path().join("empty.jsonl");
        std::fs::write(&empty_file, "").unwrap();
        assert!(!file_has_content(&empty_file));

        let content_file = temp.path().join("content.jsonl");
        std::fs::write(&content_file, "{\"type\":\"init\"}\n").unwrap();
        assert!(file_has_content(&content_file));

        let missing = temp.path().join("missing.jsonl");
        assert!(!file_has_content(&missing));
    }

    #[test]
    fn test_check_zero_output_dead_agent() {
        let agent = AgentEntry {
            id: "agent-1".into(),
            pid: 99999,
            task_id: "task-1".into(),
            executor: "claude".into(),
            started_at: "2020-01-01T00:00:00Z".into(),
            last_heartbeat: "2020-01-01T00:00:00Z".into(),
            status: AgentStatus::Dead,
            output_file: "/nonexistent/output.log".into(),
            model: None,
            completed_at: None,
        };
        // Dead agents should be ignored
        assert!(check_zero_output(&agent).is_none());
    }

    #[test]
    fn test_check_zero_output_with_content() {
        let temp = TempDir::new().unwrap();
        let agent_dir = temp.path();

        // Create raw_stream.jsonl with content
        let raw_stream = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
        std::fs::write(&raw_stream, "{\"type\":\"content\"}\n").unwrap();

        let output_file = agent_dir.join("output.log");
        std::fs::write(&output_file, "").unwrap();

        let agent = AgentEntry {
            id: "agent-1".into(),
            pid: 99999,
            task_id: "task-1".into(),
            executor: "claude".into(),
            started_at: "2020-01-01T00:00:00Z".into(), // Very old
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: output_file.to_str().unwrap().into(),
            model: None,
            completed_at: None,
        };
        // Has content, so should return None
        assert!(check_zero_output(&agent).is_none());
    }

    #[test]
    fn test_check_zero_output_empty_stream_young() {
        let temp = TempDir::new().unwrap();
        let agent_dir = temp.path();

        // Create empty raw_stream.jsonl
        let raw_stream = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
        std::fs::write(&raw_stream, "").unwrap();

        let output_file = agent_dir.join("output.log");
        std::fs::write(&output_file, "").unwrap();

        let agent = AgentEntry {
            id: "agent-1".into(),
            pid: 99999,
            task_id: "task-1".into(),
            executor: "claude".into(),
            started_at: Utc::now().to_rfc3339(), // Just started
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: output_file.to_str().unwrap().into(),
            model: None,
            completed_at: None,
        };
        // Too young, should return None
        assert!(check_zero_output(&agent).is_none());
    }

    #[test]
    fn test_check_zero_output_empty_stream_old() {
        let temp = TempDir::new().unwrap();
        let agent_dir = temp.path();

        // Create empty raw_stream.jsonl
        let raw_stream = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);
        std::fs::write(&raw_stream, "").unwrap();

        let output_file = agent_dir.join("output.log");
        std::fs::write(&output_file, "").unwrap();

        let agent = AgentEntry {
            id: "agent-1".into(),
            pid: 99999,
            task_id: "task-1".into(),
            executor: "claude".into(),
            started_at: "2020-01-01T00:00:00Z".into(), // Very old
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: output_file.to_str().unwrap().into(),
            model: None,
            completed_at: None,
        };
        // Old with zero output, should return Some
        let result = check_zero_output(&agent);
        assert!(result.is_some());
        assert!(result.unwrap() > ZERO_OUTPUT_KILL_THRESHOLD.as_secs());
    }

    #[test]
    fn test_sweep_empty_registry() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("service")).unwrap();

        // Create empty registry
        let registry = AgentRegistry::new();
        registry.save(dir).unwrap();

        let result = sweep_zero_output_agents(dir);
        assert!(result.killed.is_empty());
        assert!(result.circuit_broken_tasks.is_empty());
        assert!(!result.global_outage_detected);
    }

    #[test]
    fn test_zero_output_state_persistence() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("service")).unwrap();

        let mut state = ZeroOutputState::default();
        state.record_zero_output_kill("task-a");
        state.activate_global_backoff();
        state.save(dir);

        let loaded = ZeroOutputState::load(dir);
        assert_eq!(loaded.task_respawn_counts.get("task-a"), Some(&1));
        assert!(loaded.global_backoff.is_some());
        assert_eq!(loaded.global_backoff.unwrap().backoff_secs, 60);
    }

    #[test]
    fn test_should_pause_spawning() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("service")).unwrap();

        // Initially not paused
        assert!(!should_pause_spawning(dir));

        // Activate backoff
        let mut state = ZeroOutputState::default();
        state.activate_global_backoff();
        state.save(dir);

        assert!(should_pause_spawning(dir));
    }

    #[test]
    fn test_reset_task_counter_fn() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        std::fs::create_dir_all(dir.join("service")).unwrap();

        let mut state = ZeroOutputState::default();
        state.record_zero_output_kill("task-x");
        state.save(dir);

        reset_task_counter(dir, "task-x");

        let loaded = ZeroOutputState::load(dir);
        assert!(!loaded.task_respawn_counts.contains_key("task-x"));
    }
}
