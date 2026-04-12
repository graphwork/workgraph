use chrono::{Duration, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};

/// Configuration for structural cycle iteration.
/// Only present on the cycle header task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CycleConfig {
    /// Hard cap on cycle iterations
    pub max_iterations: u32,
    /// Condition that must be true to iterate (None = always, up to max_iterations)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<LoopGuard>,
    /// Time delay before re-activation (e.g., "30s", "5m", "1h")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<String>,
    /// When true, agents cannot signal convergence — all iterations MUST run
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_converge: bool,
    /// When true (default), if any cycle member fails, restart the entire cycle
    /// from the header instead of dead-ending. Set to false to preserve legacy behavior.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub restart_on_failure: bool,
    /// Maximum number of failure-triggered restarts per cycle lifetime.
    /// Prevents infinite failure loops. Defaults to 3 when restart_on_failure is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failure_restarts: Option<u32>,
}

fn is_false(b: &bool) -> bool {
    !b
}

fn is_true(b: &bool) -> bool {
    *b
}

fn default_true() -> bool {
    true
}

/// Guard condition for a loop edge
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LoopGuard {
    /// Loop if a specific task has this status
    TaskStatus { task: String, status: Status },
    /// Loop if iteration count < N (redundant with max_iterations but explicit)
    IterationLessThan(u32),
    /// Always loop (up to max_iterations)
    Always,
}

/// Parse a human-readable duration string like "30s", "5m", "1h", "24h" into seconds.
/// Returns None if the string is not a valid duration.
pub fn parse_delay(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Use char boundary to avoid panic on multi-byte UTF-8
    let last_char = s.chars().last()?;
    let split_pos = s.len() - last_char.len_utf8();
    let num_part = &s[..split_pos];
    let num: u64 = num_part.parse().ok()?;
    let unit = last_char;
    match unit {
        's' => Some(num),
        'm' => num.checked_mul(60),
        'h' => num.checked_mul(3600),
        'd' => num.checked_mul(86400),
        _ => None,
    }
}

/// A log entry for tracking progress/notes on a task
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The user who created this log entry (from `current_user()`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub message: String,
}

/// Cost/time estimate for a task
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Estimate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// Wait condition for `wg wait` — specifies what a Waiting task is waiting for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum WaitCondition {
    /// Wait for a task to reach a specific status
    TaskStatus { task_id: String, status: Status },
    /// Wait for a duration to elapse (resume_after is ISO 8601 timestamp computed at wait time)
    Timer { resume_after: String },
    /// Wait for a human to send a message on the task
    HumanInput,
    /// Wait for any message on the task (from any source)
    Message,
    /// Wait for a file to change (mtime check)
    FileChanged { path: String, mtime_at_wait: u64 },
}

/// Composite wait specification: AND (All) or OR (Any) of conditions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "mode", content = "conditions")]
pub enum WaitSpec {
    /// All conditions must be true
    All(Vec<WaitCondition>),
    /// Any condition being true is sufficient
    Any(Vec<WaitCondition>),
}

/// Task status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Open,
    InProgress,
    Waiting,
    Done,
    Blocked,
    Failed,
    Abandoned,
    PendingValidation,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Open => write!(f, "open"),
            Status::InProgress => write!(f, "in-progress"),
            Status::Waiting => write!(f, "waiting"),
            Status::Done => write!(f, "done"),
            Status::Blocked => write!(f, "blocked"),
            Status::Failed => write!(f, "failed"),
            Status::Abandoned => write!(f, "abandoned"),
            Status::PendingValidation => write!(f, "pending-validation"),
        }
    }
}

/// Custom deserializer that maps legacy "pending-review" status to Done.
impl<'de> serde::Deserialize<'de> for Status {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "open" => Ok(Status::Open),
            "in-progress" => Ok(Status::InProgress),
            "waiting" => Ok(Status::Waiting),
            "done" => Ok(Status::Done),
            "blocked" => Ok(Status::Blocked),
            "failed" => Ok(Status::Failed),
            "abandoned" => Ok(Status::Abandoned),
            "pending-validation" => Ok(Status::PendingValidation),
            // Migration: pending-review is treated as done
            "pending-review" => Ok(Status::Done),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &[
                    "open",
                    "in-progress",
                    "waiting",
                    "done",
                    "blocked",
                    "failed",
                    "abandoned",
                    "pending-validation",
                ],
            )),
        }
    }
}

impl Status {
    /// Whether this status is terminal — the task will not progress further
    /// without explicit intervention (retry, reopen, etc.).
    /// Terminal statuses should not block dependent tasks.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Status::Done | Status::Failed | Status::Abandoned)
    }
}

/// Task priority levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    Critical,
    High,
    #[default]
    Normal,
    Low,
    Idle,
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Priority::Critical => write!(f, "critical"),
            Priority::High => write!(f, "high"),
            Priority::Normal => write!(f, "normal"),
            Priority::Low => write!(f, "low"),
            Priority::Idle => write!(f, "idle"),
        }
    }
}

/// A task node.
///
/// A task in the workgraph with dependencies, status, and execution metadata.
///
/// Custom `Deserialize` handles migration from the old `identity` field
/// (`{"role_id": "...", "motivation_id": "..."}`) to the new `agent` field
/// (content-hash string).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Task {
    pub id: String,
    pub title: String,
    /// Detailed description of the task (body, acceptance criteria, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Status,
    /// Task priority level (critical, high, normal, low, idle)
    #[serde(default)]
    pub priority: Priority,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<Estimate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocks")]
    pub before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocked_by")]
    pub after: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Required skills/capabilities for this task
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Input files/context paths needed for this task
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// Expected output paths/artifacts
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliverables: Vec<String>,
    /// Actual produced artifacts (paths/references)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    /// Shell command to execute for this task (optional, for wg exec)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    /// Per-task timeout duration string (e.g., "30m", "4h"). Takes priority over
    /// executor config and coordinator config in timeout resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Task is not ready until this timestamp (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_before: Option<String>,
    /// Timestamp when the task was created (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Timestamp when the task status changed to InProgress (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Timestamp when the task status changed to Done (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Progress log entries
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<LogEntry>,
    /// Number of times this task has been retried after failure
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retry_count: u32,
    /// Maximum number of retries allowed (None = unlimited)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// Reason for failure or abandonment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// Preferred model for this task (haiku, sonnet, opus)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider override for this task (anthropic, openai, openrouter, local)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Named endpoint for this task (matches a name in [llm_endpoints])
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Verification criteria - if set, task requires review before done
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    /// Verification timeout override for this specific task (e.g., "15m", "900s")
    /// Takes priority over global WG_VERIFY_TIMEOUT and coordinator defaults
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_timeout: Option<String>,
    /// Agent assigned to this task (content-hash of an Agent in the agency)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Current cycle iteration (0 = first run, incremented on each re-activation)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub loop_iteration: u32,
    /// Timestamp when the most recent cycle iteration completed (before re-activation).
    /// Preserved across cycle resets so timing displays can show "last iteration completed X ago".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_iteration_completed_at: Option<String>,
    /// Number of failure-triggered cycle restarts consumed (on cycle config owner only)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cycle_failure_restarts: u32,
    /// Configuration for structural cycle iteration (only on cycle header tasks)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_config: Option<CycleConfig>,
    /// Task is not ready until this timestamp (ISO 8601 / RFC 3339).
    /// Set by loop edges with a delay — prevents immediate dispatch after re-activation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_after: Option<String>,
    /// When true, the task is paused and will not be dispatched by the coordinator.
    /// The task retains its status and loop state; `wg resume` clears this flag.
    #[serde(default, skip_serializing_if = "is_bool_false")]
    pub paused: bool,
    /// Visibility zone for trace exports. Controls what crosses organizational boundaries.
    /// Values: "internal" (default, org-only), "public" (sanitized sharing),
    /// "peer" (richer view for credentialed peers).
    #[serde(
        default = "default_visibility",
        skip_serializing_if = "is_default_visibility"
    )]
    pub visibility: String,
    /// Context scope for prompt assembly: clean, task, graph, full
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_scope: Option<String>,
    /// Execution weight tier controlling agent tool access:
    /// - "shell": no LLM, run task.exec command directly
    /// - "bare": LLM with wg CLI only, --system-prompt path
    /// - "light": LLM with read-only file access (Read, Glob, Grep, WebFetch)
    /// - "full" (default): full Claude Code session with all tools
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_mode: Option<String>,
    /// Token usage and cost data extracted from agent output.log
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    /// Claude session ID for resume/resurrection (populated from stream.jsonl Init events)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Wait condition set by `wg wait` — coordinator checks and resumes when met
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_condition: Option<WaitSpec>,
    /// Checkpoint summary written by agent before parking via `wg wait`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Number of times this task has been requeued via failed-dependency triage
    #[serde(default, skip_serializing_if = "is_zero")]
    pub triage_count: u32,
    /// Number of times this task has been resurrected (Done → Open) due to messages
    #[serde(default, skip_serializing_if = "is_zero")]
    pub resurrection_count: u32,
    /// Timestamp of last resurrection (for cooldown enforcement)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_resurrected_at: Option<String>,
    /// Validation mode: "none" (default/backward-compat), "integrated", or "external"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<String>,
    /// Commands to run during validation
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_commands: Vec<String>,
    /// If true, validator rejects when no test files were modified
    #[serde(default, skip_serializing_if = "is_bool_false")]
    pub test_required: bool,
    /// Number of times this task has been rejected by validation
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rejection_count: u32,
    /// Maximum rejections before task fails (default 3)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rejections: Option<u32>,
    /// Number of consecutive verify command failures (circuit breaker counter)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub verify_failures: u32,
    /// Number of consecutive spawn failures (spawn circuit breaker counter)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub spawn_failures: u32,
    /// Models already tried for this task (for tier escalation on retry).
    /// Each entry is the model ID string that was used for a failed attempt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tried_models: Vec<String>,
    /// Tasks that this task was replaced by (set on abandon with --superseded-by)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub superseded_by: Vec<String>,
    /// Task that this task replaces (set on new tasks created as replacements)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// When true, task was created with --no-place and should skip automatic placement.
    /// The assignment step will not include placement (dependency edge) decisions.
    #[serde(default, skip_serializing_if = "is_bool_false")]
    pub unplaced: bool,
    /// Placement hint: place near these tasks (IDs). Used by the assignment step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub place_near: Vec<String>,
    /// Placement hint: place before these tasks (IDs). Used by the assignment step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub place_before: Vec<String>,
    /// When true, task was created with --independent and has no implicit dependency
    /// on the task that created it. Explicit --after deps are still honored.
    #[serde(default, skip_serializing_if = "is_bool_false")]
    pub independent: bool,
    /// Iteration tracking: which iteration round this task is (0 = not an iteration)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub iteration_round: u32,
    /// Iteration tracking: ID of the original task this iterates from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration_anchor: Option<String>,
    /// Iteration tracking: ID of the immediate prior iteration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration_parent: Option<String>,
    /// Iteration configuration (max_retries, propagation, retry_strategy)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration_config: Option<crate::agency::IterationConfig>,
    /// Cron schedule expression (e.g., "0 2 * * *" for daily at 2am)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron_schedule: Option<String>,
    /// Whether this task has cron scheduling enabled
    #[serde(default, skip_serializing_if = "is_bool_false")]
    pub cron_enabled: bool,
    /// Timestamp of last cron trigger (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_cron_fire: Option<String>,
    /// Timestamp of next scheduled cron trigger (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cron_fire: Option<String>,
}

/// Returns `true` if the task ID represents a system-generated task.
/// System tasks use a `.` prefix (e.g. `.evaluate-foo`, `.assign-foo`).
pub fn is_system_task(task_id: &str) -> bool {
    task_id.starts_with('.')
}

/// Returns `true` if the task ID represents a user board (`.user-*`).
pub fn is_user_board(task_id: &str) -> bool {
    task_id.starts_with(".user-")
}

/// Resolve a user board alias like `.user-erik` to the active `.user-erik-N`.
/// Returns the original ID if it's already fully qualified or not a user board alias.
pub fn resolve_user_board_alias(graph: &WorkGraph, id: &str) -> String {
    if !id.starts_with(".user-") {
        return id.to_string();
    }
    let suffix = &id[".user-".len()..];
    // If suffix already ends with -N (numeric), it's fully qualified
    if suffix
        .rsplit('-')
        .next()
        .is_some_and(|s| s.parse::<u32>().is_ok())
    {
        return id.to_string();
    }
    // Find highest active .user-{handle}-N
    let prefix = format!("{}-", id);
    graph
        .tasks()
        .filter(|t| t.id.starts_with(&prefix))
        .filter(|t| !t.status.is_terminal())
        .filter_map(|t| {
            t.id.rsplit('-')
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .map(|n| (n, t.id.clone()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, id)| id)
        .unwrap_or_else(|| id.to_string())
}

/// Create a user board task for the given handle and sequence number.
/// Returns the task ID and the fully constructed Task.
pub fn create_user_board_task(handle: &str, seq: u32) -> Task {
    let task_id = format!(".user-{}-{}", handle, seq);
    Task {
        id: task_id,
        title: format!("User board: {}", handle),
        description: Some(format!(
            "User board for {} — persistent conversation surface.",
            handle
        )),
        status: Status::InProgress,
        tags: vec!["user-board".to_string()],
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Task::default()
    }
}

/// Find the next available sequence number for a user board handle.
/// Scans existing `.user-{handle}-N` tasks and returns max(N) + 1, or 0 if none exist.
pub fn next_user_board_seq(graph: &WorkGraph, handle: &str) -> u32 {
    let prefix = format!(".user-{}-", handle);
    graph
        .tasks()
        .filter(|t| t.id.starts_with(&prefix))
        .filter_map(|t| t.id.rsplit('-').next().and_then(|n| n.parse::<u32>().ok()))
        .max()
        .map(|n| n + 1)
        .unwrap_or(0)
}

/// Extract the handle portion from a user board task ID.
/// E.g., `.user-erik-0` → `Some("erik")`, `.user-alice-bob-3` → `Some("alice-bob")`.
/// Returns `None` if the ID doesn't match the `.user-{handle}-{N}` pattern.
pub fn user_board_handle(task_id: &str) -> Option<&str> {
    let rest = task_id.strip_prefix(".user-")?;
    // The last `-N` segment is the sequence number
    let last_dash = rest.rfind('-')?;
    let seq_part = &rest[last_dash + 1..];
    if seq_part.parse::<u32>().is_ok() {
        Some(&rest[..last_dash])
    } else {
        None
    }
}

/// Extract the sequence number from a user board task ID.
/// E.g., `.user-erik-0` → `Some(0)`.
pub fn user_board_seq(task_id: &str) -> Option<u32> {
    let rest = task_id.strip_prefix(".user-")?;
    rest.rsplit('-').next().and_then(|s| s.parse::<u32>().ok())
}

/// Token usage and cost data from a Claude CLI agent run.
/// Extracted from the final `type=result` line in the agent's output.log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Total cost in USD
    #[serde(default, skip_serializing_if = "is_f64_zero")]
    pub cost_usd: f64,
    /// Input tokens sent this turn (non-cached portion; what you pay full price for)
    #[serde(default, skip_serializing_if = "is_u64_zero")]
    pub input_tokens: u64,
    /// Output tokens
    #[serde(default, skip_serializing_if = "is_u64_zero")]
    pub output_tokens: u64,
    /// Tokens served from cache (already paid at discount)
    #[serde(default, skip_serializing_if = "is_u64_zero")]
    pub cache_read_input_tokens: u64,
    /// Tokens newly cached this turn (paid at premium)
    #[serde(default, skip_serializing_if = "is_u64_zero")]
    pub cache_creation_input_tokens: u64,
}

fn is_f64_zero(val: &f64) -> bool {
    *val == 0.0
}

fn is_u64_zero(val: &u64) -> bool {
    *val == 0
}

impl TokenUsage {
    /// Total input tokens (uncached + cache read + cache creation)
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }

    /// Total tokens (all input + output)
    pub fn total_tokens(&self) -> u64 {
        self.total_input() + self.output_tokens
    }

    /// Accumulate another TokenUsage into this one (component-wise addition).
    pub fn accumulate(&mut self, other: &TokenUsage) {
        self.cost_usd += other.cost_usd;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

/// Parse token usage data from a Claude CLI output.log file.
///
/// Reads the file from the end, looking for the last JSON line with `"type":"result"`.
/// Returns `None` if the file doesn't exist, is empty, or has no result line.
///
/// Supports both Claude CLI format (`"usage": {...}`, `"total_cost_usd": X`)
/// and native executor format (`"total_usage": {...}`).
pub fn parse_token_usage(output_log_path: &std::path::Path) -> Option<TokenUsage> {
    let content = std::fs::read_to_string(output_log_path).ok()?;

    // Find the last line that parses as JSON with type=result
    for line in content.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line).ok()?;
        if val.get("type").and_then(|v| v.as_str()) != Some("result") {
            continue;
        }

        let cost_usd = val
            .get("total_cost_usd")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        // Claude CLI uses "usage", native executor uses "total_usage"
        let usage = val.get("usage").or_else(|| val.get("total_usage"));

        let input_tokens = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .and_then(|u| {
                u.get("cache_read_input_tokens")
                    .or_else(|| u.get("cacheReadInputTokens"))
            })
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_creation = usage
            .and_then(|u| {
                u.get("cache_creation_input_tokens")
                    .or_else(|| u.get("cacheCreationInputTokens"))
            })
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        return Some(TokenUsage {
            cost_usd,
            input_tokens,
            output_tokens,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
        });
    }

    None
}

/// Parse token usage from an agent output.log, including mid-run data.
///
/// First tries to find a `type=result` line (completed runs). If none exists,
/// sums up per-turn usage from either:
/// - Claude CLI format: `type=assistant` with `message.usage`
/// - Native executor format: `type=turn` with top-level `usage`
///
/// Returns `None` if the file doesn't exist or has no usable data.
pub fn parse_token_usage_live(output_log_path: &std::path::Path) -> Option<TokenUsage> {
    // Try the fast path first: completed result line
    if let Some(usage) = parse_token_usage(output_log_path) {
        return Some(usage);
    }

    // Fall back: sum per-turn usage from assistant/turn messages
    let content = std::fs::read_to_string(output_log_path).ok()?;

    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_cache_creation = 0u64;
    let mut found_any = false;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        // Quick check before full parse — match Claude CLI "assistant" or native "turn"
        if !line.contains("\"type\":\"assistant\"") && !line.contains("\"type\":\"turn\"") {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let event_type = val.get("type").and_then(|v| v.as_str());

        // Claude CLI: usage nested under message.usage
        // Native executor: usage at top level
        let usage = match event_type {
            Some("assistant") => val.get("message").and_then(|m| m.get("usage")),
            Some("turn") => val.get("usage"),
            _ => continue,
        };
        if let Some(usage) = usage {
            found_any = true;
            total_input += usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            total_output += usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            total_cache_read += usage
                .get("cache_read_input_tokens")
                .or_else(|| usage.get("cacheReadInputTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            total_cache_creation += usage
                .get("cache_creation_input_tokens")
                .or_else(|| usage.get("cacheCreationInputTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }
    }

    if found_any {
        Some(TokenUsage {
            cost_usd: 0.0, // Per-turn messages don't include cumulative cost
            input_tokens: total_input,
            output_tokens: total_output,
            cache_read_input_tokens: total_cache_read,
            cache_creation_input_tokens: total_cache_creation,
        })
    } else {
        None
    }
}

/// Parse token usage from `__WG_TOKENS__:` lines in an output log.
///
/// Eval agents (`.evaluate-*`, `.flip-*`) emit `__WG_TOKENS__:{json}` to stderr
/// during `wg evaluate run`. This function extracts and sums those lines.
/// Returns `None` if the file doesn't exist or has no `__WG_TOKENS__` lines.
pub fn parse_wg_tokens(output_log_path: &std::path::Path) -> Option<TokenUsage> {
    let content = std::fs::read_to_string(output_log_path).ok()?;

    let mut total = TokenUsage {
        cost_usd: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    let mut found_any = false;

    for line in content.lines() {
        if let Some(json) = line.strip_prefix("__WG_TOKENS__:")
            && let Ok(usage) = serde_json::from_str::<TokenUsage>(json.trim())
        {
            found_any = true;
            total.accumulate(&usage);
        }
    }

    if found_any { Some(total) } else { None }
}

/// Format token usage in compact slash notation: in/out or in/out/val
/// Input = total input (uncached + cache_read + cache_creation).
/// Validation shown only when > 0.
/// `usage` is the work task's token usage, `validation` is the optional assign+eval token usage.
pub fn format_token_display(
    usage: Option<&TokenUsage>,
    agency_usage: Option<&TokenUsage>,
) -> Option<String> {
    let has_work = usage.is_some();
    let has_agency = agency_usage.is_some_and(|a| a.input_tokens + a.output_tokens > 0);

    if !has_work && !has_agency {
        return None;
    }

    let mut s = String::new();

    if let Some(u) = usage {
        // With prompt caching, `input_tokens` only counts tokens outside any cache
        // block (typically 1-3 per turn). The actual novel input is better represented
        // by `input_tokens + cache_creation_input_tokens` (content newly written to cache).
        let novel_in = u.input_tokens + u.cache_creation_input_tokens;
        s.push_str(&format!(
            "→{} ←{}",
            format_tokens(novel_in),
            format_tokens(u.output_tokens)
        ));
        if u.cache_read_input_tokens > 0 {
            // ◎ disk/circle symbol for cached tokens (read from existing cache)
            s.push_str(&format!(" ◎{}", format_tokens(u.cache_read_input_tokens)));
        }
    }

    if let Some(a) = agency_usage {
        let novel_in = a.input_tokens;
        let novel_out = a.output_tokens;
        let total = novel_in + novel_out;
        if total > 0 {
            // § agency overhead (sum of input + output)
            s.push_str(&format!(" §{}", format_tokens(total)));
        }
    }

    if s.is_empty() { None } else { Some(s) }
}

/// Format a token count in a human-readable abbreviated form (e.g., "11k", "1.2M").
pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let m = tokens as f64 / 1_000_000.0;
        if m >= 10.0 {
            format!("{:.0}M", m)
        } else {
            format!("{:.1}M", m)
        }
    } else if tokens >= 1_000 {
        let k = tokens as f64 / 1_000.0;
        if k >= 10.0 {
            format!("{:.0}k", k)
        } else {
            format!("{:.1}k", k)
        }
    } else {
        format!("{}", tokens)
    }
}

fn default_visibility() -> String {
    "internal".to_string()
}

/// Deserialize loops_to accepting both old string format and array format.
fn deserialize_loops_to<'de, D>(deserializer: D) -> Result<Vec<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct LoopsToVisitor;

    impl<'de> de::Visitor<'de> for LoopsToVisitor {
        type Value = Vec<serde_json::Value>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string or array for loops_to")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![serde_json::Value::String(v.to_string())])
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(vec![serde_json::Value::String(v)])
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(vec![])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(vec![])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(val) = seq.next_element()? {
                values.push(val);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(LoopsToVisitor)
}

fn is_default_visibility(val: &str) -> bool {
    val == "internal"
}

/// Legacy identity format: `{"role_id": "...", "motivation_id": "..."}`.
/// Used for migrating old JSONL data that stored identity inline on tasks.
#[derive(Deserialize)]
struct LegacyIdentity {
    role_id: String,
    motivation_id: String,
}

/// Helper struct for deserializing Task with migration from old `identity` field.
#[derive(Deserialize)]
struct TaskHelper {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: Status,
    #[serde(default)]
    priority: Option<Priority>,
    #[serde(default)]
    assigned: Option<String>,
    #[serde(default)]
    estimate: Option<Estimate>,
    #[serde(default, alias = "blocks")]
    before: Vec<String>,
    #[serde(default, alias = "blocked_by")]
    after: Vec<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    deliverables: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    exec: Option<String>,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    not_before: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
    #[serde(default)]
    log: Vec<LogEntry>,
    #[serde(default)]
    retry_count: u32,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    failure_reason: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    verify: Option<String>,
    #[serde(default)]
    verify_timeout: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    /// Deprecated: silently ignored on deserialization for backward compatibility.
    /// Accepts both old string format ("loops_to": "b") and array format ("loops_to": ["b"]).
    #[serde(default, deserialize_with = "deserialize_loops_to")]
    #[allow(dead_code)]
    loops_to: Vec<serde_json::Value>,
    #[serde(default)]
    loop_iteration: u32,
    #[serde(default)]
    last_iteration_completed_at: Option<String>,
    #[serde(default)]
    cycle_failure_restarts: u32,
    #[serde(default)]
    cycle_config: Option<CycleConfig>,
    #[serde(default)]
    ready_after: Option<String>,
    #[serde(default)]
    paused: bool,
    #[serde(default = "default_visibility")]
    visibility: String,
    #[serde(default)]
    context_scope: Option<String>,
    #[serde(default)]
    exec_mode: Option<String>,
    #[serde(default)]
    token_usage: Option<TokenUsage>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    wait_condition: Option<WaitSpec>,
    #[serde(default)]
    checkpoint: Option<String>,
    #[serde(default)]
    triage_count: u32,
    #[serde(default)]
    resurrection_count: u32,
    #[serde(default)]
    last_resurrected_at: Option<String>,
    #[serde(default)]
    validation: Option<String>,
    #[serde(default)]
    validation_commands: Vec<String>,
    #[serde(default)]
    test_required: bool,
    #[serde(default)]
    rejection_count: u32,
    #[serde(default)]
    max_rejections: Option<u32>,
    #[serde(default)]
    verify_failures: u32,
    #[serde(default)]
    spawn_failures: u32,
    #[serde(default)]
    tried_models: Vec<String>,
    #[serde(default)]
    superseded_by: Vec<String>,
    #[serde(default)]
    supersedes: Option<String>,
    #[serde(default)]
    unplaced: bool,
    #[serde(default)]
    place_near: Vec<String>,
    #[serde(default)]
    place_before: Vec<String>,
    /// Old format: inline identity object. Migrated to `agent` hash on read.
    #[serde(default)]
    identity: Option<LegacyIdentity>,
    /// Cron schedule expression (e.g., "0 2 * * *" for daily at 2am)
    #[serde(default)]
    cron_schedule: Option<String>,
    /// Whether this task has cron scheduling enabled
    #[serde(default)]
    cron_enabled: bool,
    /// Timestamp of last cron trigger (ISO 8601 / RFC 3339)
    #[serde(default)]
    last_cron_fire: Option<String>,
    /// Timestamp of next scheduled cron trigger (ISO 8601 / RFC 3339)
    #[serde(default)]
    next_cron_fire: Option<String>,
}

impl<'de> Deserialize<'de> for Task {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let helper = TaskHelper::deserialize(deserializer)?;

        // Migrate: if old `identity` field present and no `agent`, compute hash
        let agent = match (helper.agent, helper.identity) {
            (Some(a), _) => Some(a),
            (None, Some(legacy)) => Some(crate::agency::content_hash_agent(
                &legacy.role_id,
                &legacy.motivation_id,
            )),
            (None, None) => None,
        };

        Ok(Task {
            id: helper.id,
            title: helper.title,
            description: helper.description,
            status: helper.status,
            priority: helper.priority.unwrap_or_default(),
            assigned: helper.assigned,
            estimate: helper.estimate,
            before: helper.before,
            after: helper.after,
            requires: helper.requires,
            tags: helper.tags,
            skills: helper.skills,
            inputs: helper.inputs,
            deliverables: helper.deliverables,
            artifacts: helper.artifacts,
            exec: helper.exec,
            timeout: helper.timeout,
            not_before: helper.not_before,
            created_at: helper.created_at,
            started_at: helper.started_at,
            completed_at: helper.completed_at,
            log: helper.log,
            retry_count: helper.retry_count,
            max_retries: helper.max_retries,
            failure_reason: helper.failure_reason,
            model: helper.model,
            provider: helper.provider,
            endpoint: helper.endpoint,
            verify: helper.verify,
            verify_timeout: helper.verify_timeout,
            agent,
            loop_iteration: helper.loop_iteration,
            last_iteration_completed_at: helper.last_iteration_completed_at,
            cycle_failure_restarts: helper.cycle_failure_restarts,
            cycle_config: helper.cycle_config,
            ready_after: helper.ready_after,
            paused: helper.paused,
            visibility: helper.visibility,
            context_scope: helper.context_scope,
            exec_mode: helper.exec_mode,
            token_usage: helper.token_usage,
            session_id: helper.session_id,
            wait_condition: helper.wait_condition,
            checkpoint: helper.checkpoint,
            triage_count: helper.triage_count,
            resurrection_count: helper.resurrection_count,
            last_resurrected_at: helper.last_resurrected_at,
            validation: helper.validation,
            validation_commands: helper.validation_commands,
            test_required: helper.test_required,
            rejection_count: helper.rejection_count,
            max_rejections: helper.max_rejections,
            verify_failures: helper.verify_failures,
            spawn_failures: helper.spawn_failures,
            tried_models: helper.tried_models,
            superseded_by: helper.superseded_by,
            supersedes: helper.supersedes,
            unplaced: helper.unplaced,
            place_near: helper.place_near,
            place_before: helper.place_before,
            independent: false,
            iteration_round: 0,
            iteration_anchor: None,
            iteration_parent: None,
            iteration_config: None,
            cron_schedule: helper.cron_schedule,
            cron_enabled: helper.cron_enabled,
            last_cron_fire: helper.last_cron_fire,
            next_cron_fire: helper.next_cron_fire,
        })
    }
}

fn is_zero(val: &u32) -> bool {
    *val == 0
}

fn is_bool_false(val: &bool) -> bool {
    !*val
}

/// Trust level for an agent
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TrustLevel {
    /// Fully verified (human admin, proven agent)
    Verified,
    /// Provisionally trusted (new agent, limited permissions)
    #[default]
    Provisional,
    /// Unknown trust (external agent, needs verification)
    Unknown,
}

/// A resource (budget, compute, etc.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// A node in the work graph (task or resource)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[allow(clippy::large_enum_variant)]
pub enum Node {
    Task(Task),
    Resource(Resource),
}

impl Node {
    pub fn id(&self) -> &str {
        match self {
            Node::Task(t) => &t.id,
            Node::Resource(r) => &r.id,
        }
    }
}

/// A detected cycle (strongly connected component) in the task graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedCycle {
    /// All task IDs in this cycle's SCC.
    pub members: Vec<String>,
    /// The entry point / loop header task ID.
    pub header: String,
    /// Is this a reducible cycle (single entry point)?
    pub reducible: bool,
}

/// Cached cycle analysis derived from the graph's after edges.
/// Never serialized — recomputed lazily on access.
#[derive(Debug, Clone, Default)]
pub struct CycleAnalysis {
    /// Non-trivial SCCs (cycles).
    pub cycles: Vec<DetectedCycle>,
    /// Which cycle each task belongs to (task_id → index into cycles).
    pub task_to_cycle: HashMap<String, usize>,
    /// Back-edges: (predecessor_id, header_id) pairs within cycles.
    pub back_edges: HashSet<(String, String)>,
}

impl CycleAnalysis {
    /// Compute cycle analysis from a WorkGraph's after edges.
    pub fn from_graph(graph: &WorkGraph) -> Self {
        use crate::cycle::NamedGraph;

        // Filter out system scaffolding tasks (.assign-*, .flip-*, .evaluate-*,
        // .place-*) from cycle analysis. These are auto-generated by the
        // coordinator pipeline and add external dependency edges to cycle
        // members, which causes Havlak's algorithm to misclassify user-defined
        // cycles as IRREDUCIBLE (multiple entry points).
        fn is_system_scaffolding(id: &str) -> bool {
            id.starts_with(".assign-")
                || id.starts_with(".flip-")
                || id.starts_with(".evaluate-")
                || id.starts_with(".place-")
        }

        // Sort tasks by ID for deterministic node numbering and adjacency
        // list ordering. Back-edge detection via DFS is sensitive to
        // successor order; non-deterministic HashMap iteration previously
        // caused different back-edge sets across runs.
        let mut sorted_tasks: Vec<&Task> = graph.tasks().collect();
        sorted_tasks.sort_by(|a, b| a.id.cmp(&b.id));

        let mut named = NamedGraph::new();
        for task in &sorted_tasks {
            if !is_system_scaffolding(&task.id) {
                named.add_node(&task.id);
            }
        }
        for task in &sorted_tasks {
            if is_system_scaffolding(&task.id) {
                continue;
            }
            for dep_id in &task.after {
                if !is_system_scaffolding(dep_id) && graph.get_task(dep_id).is_some() {
                    named.add_edge(dep_id, &task.id);
                }
            }
        }

        let metadata = named.analyze_cycles();
        let mut cycles = Vec::new();
        let mut task_to_cycle = HashMap::new();
        let mut back_edges = HashSet::new();

        for (idx, meta) in metadata.iter().enumerate() {
            let members: Vec<String> = meta
                .members
                .iter()
                .map(|&nid| named.get_name(nid).to_string())
                .collect();
            let havlak_header = named.get_name(meta.header).to_string();

            // The effective header (cycle entry point) is determined by
            // Havlak's DFS algorithm on the sorted task graph. Task IDs
            // are sorted alphabetically for deterministic back-edge
            // detection. The cycle_config field is iteration metadata
            // (max_iterations, restart_on_failure) and does not influence
            // header selection — the graph structure determines execution
            // order.
            let effective_header = havlak_header.clone();

            for member in &members {
                task_to_cycle.insert(member.clone(), idx);
            }

            // Use Havlak's back-edges directly. These are edges from
            // DFS descendants back to ancestors within the SCC.
            for &(src, tgt) in &meta.back_edges {
                back_edges.insert((
                    named.get_name(src).to_string(),
                    named.get_name(tgt).to_string(),
                ));
            }
            cycles.push(DetectedCycle {
                members,
                header: effective_header,
                reducible: meta.reducible,
            });
        }

        CycleAnalysis {
            cycles,
            task_to_cycle,
            back_edges,
        }
    }
}

/// The work graph: a directed task graph with dependency edges and optional loop edges.
///
/// Tasks depend on other tasks via `after`/`blocks` edges. Resources are
/// consumed by tasks via `requires` edges. The graph is persisted as JSONL
/// (one node per line) and supports concurrent readers via atomic writes.
#[derive(Debug, Clone, Default)]
pub struct WorkGraph {
    nodes: HashMap<String, Node>,
    /// Cached cycle analysis. Lazily computed; invalidated on structural mutations.
    cycle_analysis: Option<CycleAnalysis>,
}

impl WorkGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            cycle_analysis: None,
        }
    }

    /// Insert a node (task or resource) into the graph.
    pub fn add_node(&mut self, node: Node) {
        self.cycle_analysis = None;
        self.nodes.insert(node.id().to_string(), node);
    }

    /// Look up a node by ID.
    pub fn get_node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Look up a task by ID, returning `None` if the node is a resource.
    pub fn get_task(&self, id: &str) -> Option<&Task> {
        match self.nodes.get(id) {
            Some(Node::Task(t)) => Some(t),
            _ => None,
        }
    }

    /// Look up a task by ID (mutable), returning `None` if the node is a resource.
    pub fn get_task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.cycle_analysis = None;
        match self.nodes.get_mut(id) {
            Some(Node::Task(t)) => Some(t),
            _ => None,
        }
    }

    /// Look up a task by ID, returning an error with did-you-mean suggestions if not found.
    pub fn get_task_or_err(&self, id: &str) -> anyhow::Result<&Task> {
        self.get_task(id)
            .ok_or_else(|| self.task_not_found_error(id))
    }

    /// Look up a task by ID (mutable), returning an error with did-you-mean suggestions if not found.
    pub fn get_task_mut_or_err(&mut self, id: &str) -> anyhow::Result<&mut Task> {
        self.cycle_analysis = None;
        let err = self.task_not_found_error(id);
        self.nodes
            .get_mut(id)
            .and_then(|n| match n {
                Node::Task(t) => Some(t),
                _ => None,
            })
            .ok_or(err)
    }

    /// Build a "Task not found" error, suggesting similar task IDs if any exist.
    fn task_not_found_error(&self, id: &str) -> anyhow::Error {
        let suggestion = self
            .tasks()
            .map(|t| t.id.as_str())
            .filter(|candidate| is_similar(id, candidate))
            .min_by_key(|candidate| levenshtein(id, candidate))
            .map(|s| s.to_string());

        match suggestion {
            Some(s) => anyhow::anyhow!("Task '{}' not found. Did you mean '{}'?", id, s),
            None => anyhow::anyhow!("Task '{}' not found", id),
        }
    }

    /// Look up a resource by ID, returning `None` if the node is a task.
    pub fn get_resource(&self, id: &str) -> Option<&Resource> {
        match self.nodes.get(id) {
            Some(Node::Resource(r)) => Some(r),
            _ => None,
        }
    }

    /// Iterate over all nodes (tasks and resources) in the graph.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// Iterate over all tasks in the graph, skipping resource nodes.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.nodes.values().filter_map(|n| match n {
            Node::Task(t) => Some(t),
            _ => None,
        })
    }

    /// Iterate over all resources in the graph, skipping task nodes.
    pub fn resources(&self) -> impl Iterator<Item = &Resource> {
        self.nodes.values().filter_map(|n| match n {
            Node::Resource(r) => Some(r),
            _ => None,
        })
    }

    /// Remove a node by ID, returning the removed node if it existed.
    ///
    /// Also cleans up all references to the removed node from other tasks
    /// (`after`, `blocks`, `requires`).
    pub fn remove_node(&mut self, id: &str) -> Option<Node> {
        self.cycle_analysis = None;
        let removed = self.nodes.remove(id);
        if removed.is_some() {
            for node in self.nodes.values_mut() {
                if let Node::Task(task) = node {
                    task.after.retain(|dep| dep != id);
                    task.before.retain(|dep| dep != id);
                    task.requires.retain(|dep| dep != id);
                }
            }
        }
        removed
    }

    /// Return the total number of nodes (tasks + resources) in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Return true if the graph contains no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Invalidate cached cycle analysis. Called by structural mutations.
    pub fn invalidate_cycle_cache(&mut self) {
        self.cycle_analysis = None;
    }

    /// Compute cycle analysis without caching (for use with immutable references).
    pub fn compute_cycle_analysis(&self) -> CycleAnalysis {
        CycleAnalysis::from_graph(self)
    }

    /// Get or compute cached cycle analysis.
    pub fn get_cycle_analysis(&mut self) -> &CycleAnalysis {
        if self.cycle_analysis.is_none() {
            self.cycle_analysis = Some(CycleAnalysis::from_graph(self));
        }
        self.cycle_analysis.as_ref().unwrap()
    }

    /// Compute the depth of a task by walking its `after` dependency chain.
    ///
    /// Depth is the length of the longest path from any root task (a task with
    /// no `after` dependencies) to this task. A root task has depth 0, its
    /// direct dependents have depth 1, etc.
    ///
    /// Returns 0 for unknown task IDs or tasks with no dependencies.
    pub fn task_depth(&self, task_id: &str) -> u32 {
        let mut memo: HashMap<String, u32> = HashMap::new();
        self.task_depth_inner(task_id, &mut memo, &mut HashSet::new())
    }

    fn task_depth_inner(
        &self,
        task_id: &str,
        memo: &mut HashMap<String, u32>,
        visiting: &mut HashSet<String>,
    ) -> u32 {
        if let Some(&cached) = memo.get(task_id) {
            return cached;
        }

        // Cycle detection: if we're already visiting this node, return 0
        if !visiting.insert(task_id.to_string()) {
            return 0;
        }

        let depth = match self.get_task(task_id) {
            Some(task) if !task.after.is_empty() => {
                let max_parent_depth = task
                    .after
                    .iter()
                    .map(|parent_id| self.task_depth_inner(parent_id, memo, visiting))
                    .max()
                    .unwrap_or(0);
                max_parent_depth + 1
            }
            _ => 0,
        };

        visiting.remove(task_id);
        memo.insert(task_id.to_string(), depth);
        depth
    }
}

/// Evaluate a guard condition against the current graph state.
fn evaluate_guard(guard: &Option<LoopGuard>, graph: &WorkGraph) -> bool {
    match guard {
        None | Some(LoopGuard::Always) => true,
        // IterationLessThan is checked by callers where iteration count is available.
        Some(LoopGuard::IterationLessThan(_)) => true,
        Some(LoopGuard::TaskStatus { task, status }) => graph
            .get_task(task)
            .map(|t| t.status == *status)
            .unwrap_or(false),
    }
}

/// Evaluate structural cycle iteration after a task transitions to Done.
///
/// Two modes:
/// 1. **SCC cycle**: The completed task is part of a structural cycle detected via
///    `CycleAnalysis`. If ALL cycle members are Done, evaluates iteration.
/// 2. **Implicit cycle**: The completed task has `cycle_config` but is NOT in an SCC
///    (e.g., created with `--max-iterations` + `--after` without explicit back-edges).
///    Treats the task and its `after` deps as a virtual cycle.
///
/// In both cases, checks:
/// - Convergence tag on the config owner
/// - `max_iterations` limit
/// - Guard condition
/// - If iterating: re-opens all cycle members, increments `loop_iteration`,
///   optionally sets `ready_after` if delay is configured.
///
/// Returns the list of task IDs that were re-activated.
pub fn evaluate_cycle_iteration(
    graph: &mut WorkGraph,
    completed_task_id: &str,
    cycle_analysis: &CycleAnalysis,
) -> Vec<String> {
    // Determine cycle members and config owner.
    // Mode 1: SCC-detected cycle
    if let Some(&cycle_idx) = cycle_analysis.task_to_cycle.get(completed_task_id) {
        let cycle = &cycle_analysis.cycles[cycle_idx];

        // Find the cycle member with CycleConfig
        let (config_owner_id, cycle_config) = {
            let mut found = None;
            for member_id in &cycle.members {
                if let Some(task) = graph.get_task(member_id)
                    && let Some(ref config) = task.cycle_config
                {
                    found = Some((member_id.clone(), config.clone()));
                    break;
                }
            }
            match found {
                Some(pair) => pair,
                None => return vec![], // No config = no cycle iteration
            }
        };

        return reactivate_cycle(graph, &cycle.members, &config_owner_id, &cycle_config);
    }

    // Mode 2: Implicit cycle — completed task has cycle_config but no SCC back-edge.
    // This handles `wg add B --after A --max-iterations 3` where no explicit
    // back-edge was created. Treat B + its after deps as the cycle members.
    if let Some(task) = graph.get_task(completed_task_id)
        && let Some(ref config) = task.cycle_config
    {
        let config = config.clone();
        let mut members: Vec<String> = task.after.clone();
        let config_owner_id = completed_task_id.to_string();
        if !members.contains(&config_owner_id) {
            members.push(config_owner_id.clone());
        }

        return reactivate_cycle(graph, &members, &config_owner_id, &config);
    }

    vec![]
}

/// Shared logic: check conditions and re-open cycle members.
fn reactivate_cycle(
    graph: &mut WorkGraph,
    members: &[String],
    config_owner_id: &str,
    cycle_config: &CycleConfig,
) -> Vec<String> {
    // If the cycle header (config owner) is archived, suppress the entire cycle.
    // An archived header means the cycle was intentionally retired — no further
    // iterations should occur, even if non-archived members complete afterward.
    if let Some(owner) = graph.get_task(config_owner_id)
        && owner.tags.contains(&"archived".to_string())
    {
        return vec![];
    }

    // Check if ALL members are terminal (Done or Abandoned).
    // Abandoned and archived-Done are terminal — they won't produce more work.
    let mut has_done_member = false;
    for member_id in members {
        match graph.get_task(member_id) {
            Some(t) if t.status == Status::Done => {
                // Archived Done members are permanently terminal (like Abandoned)
                if !t.tags.contains(&"archived".to_string()) {
                    has_done_member = true;
                }
            }
            Some(t) if t.status == Status::Abandoned => {
                // Abandoned is terminal — don't wait for it
            }
            _ => return vec![], // Not terminal yet
        }
    }
    // If ALL members are abandoned/archived, don't iterate — there's no work to redo
    if !has_done_member {
        return vec![];
    }

    // Check convergence tag on ANY cycle member — but only if no external guard
    // is set and no_converge is false. When a guard is present, the guard is
    // authoritative over convergence. When no_converge is set, convergence
    // signals are always ignored.
    //
    // Any member can signal convergence (not just the config owner). Since we
    // only reach this point after ALL members are terminal, the current
    // iteration has already completed — convergence only prevents the NEXT
    // iteration from starting.
    let guard_is_set =
        cycle_config.guard.is_some() && !matches!(cycle_config.guard, Some(LoopGuard::Always));

    if !guard_is_set && !cycle_config.no_converge {
        let any_converged = members.iter().any(|mid| {
            graph
                .get_task(mid)
                .map(|t| t.tags.contains(&"converged".to_string()))
                .unwrap_or(false)
        });
        if any_converged {
            return vec![];
        }
    }

    // Check max_iterations — use the NEXT iteration value so that
    // max_iterations=N yields exactly N total runs (iterations 0..N-1).
    let current_iter = graph
        .get_task(config_owner_id)
        .map(|t| t.loop_iteration)
        .unwrap_or(0);
    let new_iteration = current_iter + 1;
    if cycle_config.max_iterations > 0 && new_iteration >= cycle_config.max_iterations {
        return vec![];
    }

    // Check guard condition
    if !evaluate_guard(&cycle_config.guard, graph) {
        return vec![];
    }
    if let Some(LoopGuard::IterationLessThan(n)) = &cycle_config.guard
        && new_iteration >= *n
    {
        return vec![];
    }

    // All checks passed — re-open Done members (skip Abandoned ones)
    let ready_after = cycle_config
        .delay
        .as_ref()
        .and_then(|d| match parse_delay(d) {
            Some(secs) if secs <= i64::MAX as u64 => {
                Some((Utc::now() + Duration::seconds(secs as i64)).to_rfc3339())
            }
            _ => None,
        });

    let mut reactivated = Vec::new();

    for member_id in members {
        if let Some(task) = graph.get_task_mut(member_id) {
            // Abandoned or archived members stay as-is — they opted out of future iterations
            if task.status == Status::Abandoned {
                continue;
            }
            if task.tags.contains(&"archived".to_string()) {
                continue;
            }
            // Preserve completed_at as last_iteration_completed_at before clearing
            if task.completed_at.is_some() {
                task.last_iteration_completed_at = task.completed_at.clone();
            }
            task.status = Status::Open;
            task.assigned = None;
            task.started_at = None;
            task.completed_at = None;
            task.triage_count = 0;
            task.loop_iteration = new_iteration;
            if *member_id == config_owner_id {
                task.ready_after = ready_after.clone();
            }

            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                user: Some(crate::current_user()),
                message: if cycle_config.max_iterations == 0 {
                    format!(
                        "Re-activated by cycle iteration (iteration {}/unlimited)",
                        new_iteration
                    )
                } else {
                    format!(
                        "Re-activated by cycle iteration (iteration {}/{})",
                        new_iteration, cycle_config.max_iterations
                    )
                },
            });

            reactivated.push(member_id.clone());
        }
    }

    reactivated
}

/// Scan all detected cycles and reactivate any where all members are Done.
///
/// Unlike `evaluate_cycle_iteration` (which is triggered by a single task
/// completion), this function proactively checks every cycle in the graph.
/// It is intended to be called by the coordinator as a safety net — if a
/// `wg done` call reactivated the cycle, there is nothing left to do;
/// if the reactivation was missed (race, crash, etc.), this catches it.
///
/// Returns the list of task IDs that were re-activated across all cycles.
pub fn evaluate_all_cycle_iterations(
    graph: &mut WorkGraph,
    cycle_analysis: &CycleAnalysis,
) -> Vec<String> {
    let mut all_reactivated = Vec::new();

    for cycle in &cycle_analysis.cycles {
        // Find the cycle member with CycleConfig
        let found = {
            let mut result = None;
            for member_id in &cycle.members {
                if let Some(task) = graph.get_task(member_id)
                    && let Some(ref config) = task.cycle_config
                {
                    result = Some((member_id.clone(), config.clone()));
                    break;
                }
            }
            result
        };

        let Some((config_owner_id, cycle_config)) = found else {
            continue; // No config = no cycle iteration
        };

        let reactivated = reactivate_cycle(graph, &cycle.members, &config_owner_id, &cycle_config);
        all_reactivated.extend(reactivated);
    }

    all_reactivated
}

/// Evaluate whether a failed task should trigger a cycle restart.
///
/// When a task in a cycle fails and `restart_on_failure` is true (the default),
/// this resets all cycle members to Open so the cycle retries from the top.
/// The `loop_iteration` is NOT incremented (the failed iteration is retried).
/// The `cycle_failure_restarts` counter on the config owner IS incremented.
///
/// Returns the list of task IDs that were re-activated, or empty if no restart.
pub fn evaluate_cycle_on_failure(
    graph: &mut WorkGraph,
    failed_task_id: &str,
    cycle_analysis: &CycleAnalysis,
) -> Vec<String> {
    // Mode 1: SCC-detected cycle
    if let Some(&cycle_idx) = cycle_analysis.task_to_cycle.get(failed_task_id) {
        let cycle = &cycle_analysis.cycles[cycle_idx];

        let (config_owner_id, cycle_config) = {
            let mut found = None;
            for member_id in &cycle.members {
                if let Some(task) = graph.get_task(member_id)
                    && let Some(ref config) = task.cycle_config
                {
                    found = Some((member_id.clone(), config.clone()));
                    break;
                }
            }
            match found {
                Some(pair) => pair,
                None => return vec![],
            }
        };

        return reactivate_cycle_on_failure(
            graph,
            &cycle.members,
            &config_owner_id,
            &cycle_config,
            failed_task_id,
        );
    }

    // Mode 2: Implicit cycle — failed task has cycle_config
    if let Some(task) = graph.get_task(failed_task_id)
        && let Some(ref config) = task.cycle_config
    {
        let config = config.clone();
        let mut members: Vec<String> = task.after.clone();
        let config_owner_id = failed_task_id.to_string();
        if !members.contains(&config_owner_id) {
            members.push(config_owner_id.clone());
        }

        return reactivate_cycle_on_failure(
            graph,
            &members,
            &config_owner_id,
            &config,
            failed_task_id,
        );
    }

    // Mode 3: Failed task is a member of an implicit cycle (config owner is different)
    for (id, node) in &graph.nodes {
        if let Node::Task(task) = node
            && task.cycle_config.is_some()
            && task.after.contains(&failed_task_id.to_string())
        {
            let config = task.cycle_config.as_ref().unwrap().clone();
            let config_owner_id = id.clone();
            let mut members = task.after.clone();
            if !members.contains(&config_owner_id) {
                members.push(config_owner_id.clone());
            }

            return reactivate_cycle_on_failure(
                graph,
                &members,
                &config_owner_id,
                &config,
                failed_task_id,
            );
        }
    }

    vec![]
}

/// Failure-triggered cycle restart: re-open all members when a cycle member fails.
fn reactivate_cycle_on_failure(
    graph: &mut WorkGraph,
    members: &[String],
    config_owner_id: &str,
    cycle_config: &CycleConfig,
    failed_task_id: &str,
) -> Vec<String> {
    if !cycle_config.restart_on_failure {
        return vec![];
    }

    // Verify the failed task is actually Failed
    if let Some(task) = graph.get_task(failed_task_id) {
        if task.status != Status::Failed {
            return vec![];
        }
    } else {
        return vec![];
    }

    // Check max_failure_restarts
    let failure_restarts = graph
        .get_task(config_owner_id)
        .map(|t| t.cycle_failure_restarts)
        .unwrap_or(0);
    let max_failure_restarts = cycle_config.max_failure_restarts.unwrap_or(3);
    if failure_restarts >= max_failure_restarts {
        if let Some(task) = graph.get_task_mut(config_owner_id) {
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                user: Some(crate::current_user()),
                message: format!(
                    "Cycle failure restart budget exhausted ({}/{}). Task '{}' failed — cycle dead-ended.",
                    failure_restarts, max_failure_restarts, failed_task_id
                ),
            });
        }
        return vec![];
    }

    // Collect failure info before mutating
    let failure_reason = graph
        .get_task(failed_task_id)
        .and_then(|t| t.failure_reason.clone());

    // Compute delay
    let ready_after = cycle_config.delay.as_ref().and_then(|d| {
        parse_delay(d).and_then(|secs| {
            if secs <= i64::MAX as u64 {
                Some((Utc::now() + Duration::seconds(secs as i64)).to_rfc3339())
            } else {
                None
            }
        })
    });

    let current_iter = graph
        .get_task(config_owner_id)
        .map(|t| t.loop_iteration)
        .unwrap_or(0);
    let new_failure_restarts = failure_restarts + 1;

    let failure_info = match &failure_reason {
        Some(r) => format!("{}: {}", failed_task_id, r),
        None => failed_task_id.to_string(),
    };

    let mut reactivated = Vec::new();

    for member_id in members {
        if let Some(task) = graph.get_task_mut(member_id) {
            task.status = Status::Open;
            task.assigned = None;
            task.started_at = None;
            task.completed_at = None;
            task.failure_reason = None;
            task.triage_count = 0;
            // loop_iteration stays the same — this is a retry of the same iteration
            if *member_id == config_owner_id {
                task.ready_after = ready_after.clone();
                task.cycle_failure_restarts = new_failure_restarts;
                task.tags.retain(|t| t != "converged");
            }

            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                user: Some(crate::current_user()),
                message: format!(
                    "Cycle failure restart {}/{} (iteration {}). Failed: [{}]",
                    new_failure_restarts, max_failure_restarts, current_iter, failure_info
                ),
            });

            reactivated.push(member_id.clone());
        }
    }

    reactivated
}

/// Scan all cycles and reactivate any where a member is Failed and restart_on_failure is true.
pub fn evaluate_all_cycle_failure_restarts(
    graph: &mut WorkGraph,
    cycle_analysis: &CycleAnalysis,
) -> Vec<String> {
    let mut all_reactivated = Vec::new();

    for cycle in &cycle_analysis.cycles {
        let found = {
            let mut result = None;
            for member_id in &cycle.members {
                if let Some(task) = graph.get_task(member_id)
                    && let Some(ref config) = task.cycle_config
                {
                    result = Some((member_id.clone(), config.clone()));
                    break;
                }
            }
            result
        };

        let Some((config_owner_id, cycle_config)) = found else {
            continue;
        };

        if !cycle_config.restart_on_failure {
            continue;
        }

        let failed_member = cycle.members.iter().find(|id| {
            graph
                .get_task(id.as_str())
                .map(|t| t.status == Status::Failed)
                .unwrap_or(false)
        });

        if let Some(failed_id) = failed_member {
            let failed_id = failed_id.clone();
            let reactivated = reactivate_cycle_on_failure(
                graph,
                &cycle.members,
                &config_owner_id,
                &cycle_config,
                &failed_id,
            );
            all_reactivated.extend(reactivated);
        }
    }

    all_reactivated
}

/// Compute Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Check if two task IDs are similar enough to suggest.
/// Returns true if one is a prefix of the other, or Levenshtein distance <= 2.
fn is_similar(query: &str, candidate: &str) -> bool {
    if candidate.starts_with(query) || query.starts_with(candidate) {
        return true;
    }
    levenshtein(query, candidate) <= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn test_status_is_terminal() {
        assert!(!Status::Open.is_terminal());
        assert!(!Status::InProgress.is_terminal());
        assert!(!Status::Blocked.is_terminal());
        assert!(Status::Done.is_terminal());
        assert!(Status::Failed.is_terminal());
        assert!(Status::Abandoned.is_terminal());
        assert!(!Status::PendingValidation.is_terminal());
    }

    #[test]
    fn test_workgraph_new_is_empty() {
        let graph = WorkGraph::new();
        assert!(graph.is_empty());
        assert_eq!(graph.len(), 0);
    }

    #[test]
    fn test_add_and_get_task() {
        let mut graph = WorkGraph::new();
        let task = make_task("api-design", "Design API");
        graph.add_node(Node::Task(task));

        assert_eq!(graph.len(), 1);
        let retrieved = graph.get_task("api-design").unwrap();
        assert_eq!(retrieved.title, "Design API");
    }

    #[test]
    fn test_get_nonexistent_returns_none() {
        let graph = WorkGraph::new();
        assert!(graph.get_node("nonexistent").is_none());
        assert!(graph.get_task("nonexistent").is_none());
    }

    #[test]
    fn test_remove_node() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        assert_eq!(graph.len(), 1);

        let removed = graph.remove_node("t1");
        assert!(removed.is_some());
        assert!(graph.is_empty());
    }

    #[test]
    fn test_remove_node_cleans_up_references() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));

        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        t2.before = vec!["t1".to_string()];
        t2.requires = vec!["t1".to_string()];
        graph.add_node(Node::Task(t2));

        graph.remove_node("t1");

        let t2 = graph.get_task("t2").unwrap();
        assert!(t2.after.is_empty(), "after should be cleaned");
        assert!(t2.before.is_empty(), "blocks should be cleaned");
        assert!(t2.requires.is_empty(), "requires should be cleaned");
    }

    #[test]
    fn test_tasks_iterator() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        graph.add_node(Node::Task(make_task("t2", "Task 2")));

        let tasks: Vec<_> = graph.tasks().collect();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn test_task_with_blocks() {
        let mut graph = WorkGraph::new();
        let mut task1 = make_task("api-design", "Design API");
        task1.before = vec!["api-impl".to_string()];

        let mut task2 = make_task("api-impl", "Implement API");
        task2.after = vec!["api-design".to_string()];

        graph.add_node(Node::Task(task1));
        graph.add_node(Node::Task(task2));

        let design = graph.get_task("api-design").unwrap();
        assert_eq!(design.before, vec!["api-impl"]);

        let impl_task = graph.get_task("api-impl").unwrap();
        assert_eq!(impl_task.after, vec!["api-design"]);
    }

    #[test]
    fn test_task_serialization() {
        let task = make_task("t1", "Test task");
        let json = serde_json::to_string(&Node::Task(task)).unwrap();
        assert!(json.contains("\"kind\":\"task\""));
        assert!(json.contains("\"id\":\"t1\""));
    }

    #[test]
    fn test_task_deserialization() {
        let json = r#"{"id":"t1","kind":"task","title":"Test","status":"open"}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.id, "t1");
                assert_eq!(t.title, "Test");
                assert_eq!(t.status, Status::Open);
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_status_serialization() {
        assert_eq!(
            serde_json::to_string(&Status::InProgress).unwrap(),
            "\"in-progress\""
        );
    }

    #[test]
    fn test_timestamp_fields_serialization() {
        let mut task = make_task("t1", "Test task");
        task.created_at = Some("2024-01-15T10:30:00Z".to_string());
        task.started_at = Some("2024-01-15T11:00:00Z".to_string());
        task.completed_at = Some("2024-01-15T12:00:00Z".to_string());

        let json = serde_json::to_string(&Node::Task(task)).unwrap();
        assert!(json.contains("\"created_at\":\"2024-01-15T10:30:00Z\""));
        assert!(json.contains("\"started_at\":\"2024-01-15T11:00:00Z\""));
        assert!(json.contains("\"completed_at\":\"2024-01-15T12:00:00Z\""));

        // Verify deserialization
        let node: Node = serde_json::from_str(&json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.created_at, Some("2024-01-15T10:30:00Z".to_string()));
                assert_eq!(t.started_at, Some("2024-01-15T11:00:00Z".to_string()));
                assert_eq!(t.completed_at, Some("2024-01-15T12:00:00Z".to_string()));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_timestamp_fields_omitted_when_none() {
        let task = make_task("t1", "Test task");
        let json = serde_json::to_string(&Node::Task(task)).unwrap();

        // Verify timestamps are not included when None
        assert!(!json.contains("created_at"));
        assert!(!json.contains("started_at"));
        assert!(!json.contains("completed_at"));
    }

    #[test]
    fn test_deliverables_serialization() {
        let mut task = make_task("t1", "Build feature");
        task.deliverables = vec!["src/feature.rs".to_string(), "docs/feature.md".to_string()];

        let json = serde_json::to_string(&Node::Task(task)).unwrap();
        assert!(json.contains("\"deliverables\""));
        assert!(json.contains("src/feature.rs"));
        assert!(json.contains("docs/feature.md"));

        // Verify deserialization
        let node: Node = serde_json::from_str(&json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.deliverables.len(), 2);
                assert!(t.deliverables.contains(&"src/feature.rs".to_string()));
                assert!(t.deliverables.contains(&"docs/feature.md".to_string()));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_deliverables_omitted_when_empty() {
        let task = make_task("t1", "Test task");
        let json = serde_json::to_string(&Node::Task(task)).unwrap();

        // Verify deliverables not included when empty
        assert!(!json.contains("deliverables"));
    }

    #[test]
    fn test_deserialize_with_agent_field() {
        let json = r#"{"id":"t1","kind":"task","title":"Test","status":"open","agent":"abc123"}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.agent, Some("abc123".to_string()));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_deserialize_legacy_identity_migrates_to_agent() {
        // Old format had identity: {role_id, motivation_id} inline on the task
        let json = r#"{"id":"t1","kind":"task","title":"Test","status":"open","identity":{"role_id":"role-abc","motivation_id":"mot-xyz"}}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        match node {
            Node::Task(t) => {
                // Should be migrated to agent hash
                let expected = crate::agency::content_hash_agent("role-abc", "mot-xyz");
                assert_eq!(t.agent, Some(expected));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_deserialize_agent_field_takes_precedence_over_legacy_identity() {
        // If both agent and identity are present, agent wins
        let json = r#"{"id":"t1","kind":"task","title":"Test","status":"open","agent":"explicit-hash","identity":{"role_id":"role-abc","motivation_id":"mot-xyz"}}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.agent, Some("explicit-hash".to_string()));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_serialize_does_not_emit_identity_field() {
        let mut task = make_task("t1", "Test task");
        task.agent = Some("abc123".to_string());
        let json = serde_json::to_string(&Node::Task(task)).unwrap();
        // New format only has "agent", never "identity"
        assert!(json.contains("\"agent\":\"abc123\""));
        assert!(!json.contains("\"identity\""));
    }

    // ── parse_delay tests ──────────────────────────────────────────

    #[test]
    fn test_parse_delay_seconds() {
        assert_eq!(parse_delay("30s"), Some(30));
        assert_eq!(parse_delay("1s"), Some(1));
    }

    #[test]
    fn test_parse_delay_minutes() {
        assert_eq!(parse_delay("5m"), Some(300));
        assert_eq!(parse_delay("1m"), Some(60));
    }

    #[test]
    fn test_parse_delay_hours() {
        assert_eq!(parse_delay("2h"), Some(7200));
        assert_eq!(parse_delay("1h"), Some(3600));
    }

    #[test]
    fn test_parse_delay_days() {
        assert_eq!(parse_delay("1d"), Some(86400));
        assert_eq!(parse_delay("7d"), Some(604800));
    }

    #[test]
    fn test_parse_delay_empty_string() {
        assert_eq!(parse_delay(""), None);
    }

    #[test]
    fn test_parse_delay_whitespace_only() {
        assert_eq!(parse_delay("   "), None);
    }

    #[test]
    fn test_parse_delay_whitespace_around_value() {
        assert_eq!(parse_delay("  10s  "), Some(10));
        assert_eq!(parse_delay("\t5m\t"), Some(300));
    }

    #[test]
    fn test_parse_delay_invalid_unit() {
        assert_eq!(parse_delay("10x"), None);
        assert_eq!(parse_delay("5w"), None);
        assert_eq!(parse_delay("3y"), None);
    }

    #[test]
    fn test_parse_delay_missing_numeric_prefix() {
        assert_eq!(parse_delay("s"), None);
        assert_eq!(parse_delay("m"), None);
        assert_eq!(parse_delay("h"), None);
        assert_eq!(parse_delay("d"), None);
    }

    #[test]
    fn test_parse_delay_zero_duration() {
        assert_eq!(parse_delay("0s"), Some(0));
        assert_eq!(parse_delay("0m"), Some(0));
        assert_eq!(parse_delay("0h"), Some(0));
        assert_eq!(parse_delay("0d"), Some(0));
    }

    #[test]
    fn test_parse_delay_large_values() {
        assert_eq!(parse_delay("999999s"), Some(999999));
        assert_eq!(parse_delay("100000m"), Some(6_000_000));
    }

    #[test]
    fn test_parse_delay_overflow_returns_none() {
        // u64::MAX / 86400 < 213_503_982_334_601, so this day value overflows
        // The function returns None on overflow instead of panicking
        assert_eq!(parse_delay("213503982334602d"), None);
        assert_eq!(parse_delay("999999999999999999h"), None);
        assert_eq!(parse_delay("999999999999999999m"), None);
    }

    #[test]
    fn test_parse_delay_fractional_number() {
        // parse::<u64> fails on fractional input
        assert_eq!(parse_delay("1.5s"), None);
        assert_eq!(parse_delay("2.0m"), None);
    }

    #[test]
    fn test_parse_delay_negative_number() {
        assert_eq!(parse_delay("-5s"), None);
    }

    #[test]
    fn test_parse_delay_no_unit_just_number() {
        // Last char is a digit, not a valid unit
        assert_eq!(parse_delay("10"), None);
    }

    #[test]
    fn test_parse_delay_multibyte_utf8_no_panic() {
        // Multi-byte UTF-8 unit should return None, not panic
        assert_eq!(parse_delay("30🎯"), None);
        assert_eq!(parse_delay("5ñ"), None);
        assert_eq!(parse_delay("10日"), None);
    }

    #[test]
    fn test_levenshtein() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("food", "foo"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn test_is_similar() {
        // Prefix matches
        assert!(is_similar("api", "api-design"));
        assert!(is_similar("api-design", "api"));

        // Edit distance <= 2
        assert!(is_similar("foo", "food"));
        assert!(is_similar("foo", "boo"));
        assert!(is_similar("abc", "axc"));

        // Too far apart
        assert!(!is_similar("abc", "xyz"));
        assert!(!is_similar("hello", "world"));
    }

    #[test]
    fn test_get_task_or_err_found() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("api-design", "Design API")));

        let task = graph.get_task_or_err("api-design").unwrap();
        assert_eq!(task.title, "Design API");
    }

    #[test]
    fn test_get_task_or_err_not_found_with_suggestion() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("api-design", "Design API")));

        let err = graph.get_task_or_err("api-desgin").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "should say not found: {}", msg);
        assert!(
            msg.contains("Did you mean 'api-design'?"),
            "should suggest api-design: {}",
            msg
        );
    }

    #[test]
    fn test_get_task_or_err_not_found_no_suggestion() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("api-design", "Design API")));

        let err = graph.get_task_or_err("zzz-totally-different").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "should say not found: {}", msg);
        assert!(
            !msg.contains("Did you mean"),
            "should not suggest anything: {}",
            msg
        );
    }

    #[test]
    fn test_get_task_mut_or_err_found() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("build-ui", "Build UI")));

        let task = graph.get_task_mut_or_err("build-ui").unwrap();
        task.title = "Build UI v2".to_string();

        assert_eq!(graph.get_task("build-ui").unwrap().title, "Build UI v2");
    }

    #[test]
    fn test_get_task_or_err_prefix_suggestion() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("api-design", "Design API")));
        graph.add_node(Node::Task(make_task("build-ui", "Build UI")));

        let err = graph.get_task_or_err("api").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Did you mean 'api-design'?"),
            "should suggest prefix match: {}",
            msg
        );
    }

    #[test]
    fn test_format_tokens_small() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn test_format_tokens_thousands() {
        assert_eq!(format_tokens(1000), "1.0k");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(11228), "11k");
        assert_eq!(format_tokens(99999), "100k");
    }

    #[test]
    fn test_format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(1_500_000), "1.5M");
        assert_eq!(format_tokens(10_000_000), "10M");
    }

    #[test]
    fn test_format_token_display() {
        // Usage with cache tokens — total input = 4600 + 100000 + 5000 = 109600
        let usage = TokenUsage {
            cost_usd: 0.0,
            input_tokens: 4600,
            output_tokens: 3900,
            cache_read_input_tokens: 100_000,
            cache_creation_input_tokens: 5_000,
        };
        // Aggregated agency usage (assign + eval combined, novel only)
        let agency = TokenUsage {
            cost_usd: 0.0,
            input_tokens: 1300, // 500 assign + 800 eval novel input
            output_tokens: 600, // 200 assign + 400 eval novel output
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        // →novel_in ←out ◎cached ☀agency_total
        // novel_in = input_tokens(4600) + cache_creation(5000) = 9600
        // cached = cache_read(100000)
        assert_eq!(
            format_token_display(Some(&usage), Some(&agency)),
            Some("→9.6k ←3.9k ◎100k §1.9k".to_string())
        );
        assert_eq!(
            format_token_display(Some(&usage), None),
            Some("→9.6k ←3.9k ◎100k".to_string())
        );
        // Only agency, no task usage
        assert_eq!(
            format_token_display(None, Some(&agency)),
            Some(" §1.9k".to_string())
        );
        assert_eq!(format_token_display(None, None), None);

        // Zero agency tokens should not show § section
        let zero_val = TokenUsage {
            cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        assert_eq!(
            format_token_display(Some(&usage), Some(&zero_val)),
            Some("→9.6k ←3.9k ◎100k".to_string())
        );
        assert_eq!(format_token_display(None, Some(&zero_val)), None);
    }

    #[test]
    fn test_token_usage_total() {
        let usage = TokenUsage {
            cost_usd: 1.0,
            input_tokens: 100,
            output_tokens: 200,
            cache_read_input_tokens: 300,
            cache_creation_input_tokens: 400,
        };
        assert_eq!(usage.total_input(), 800);
        assert_eq!(usage.total_tokens(), 1000);
    }

    #[test]
    fn test_parse_token_usage_valid() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(
            &log_path,
            r#"{"type":"assistant","content":"hello"}
{"type":"result","total_cost_usd":2.18,"usage":{"input_tokens":55,"output_tokens":11228,"cache_read_input_tokens":3096388,"cache_creation_input_tokens":6204}}
"#,
        )
        .unwrap();

        let usage = parse_token_usage(&log_path).unwrap();
        assert_eq!(usage.cost_usd, 2.18);
        assert_eq!(usage.input_tokens, 55);
        assert_eq!(usage.output_tokens, 11228);
        assert_eq!(usage.cache_read_input_tokens, 3096388);
        assert_eq!(usage.cache_creation_input_tokens, 6204);
    }

    #[test]
    fn test_parse_token_usage_camel_case_keys() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(
            &log_path,
            r#"{"type":"result","total_cost_usd":0.5,"usage":{"input_tokens":10,"output_tokens":100,"cacheReadInputTokens":5000,"cacheCreationInputTokens":200}}
"#,
        )
        .unwrap();

        let usage = parse_token_usage(&log_path).unwrap();
        assert_eq!(usage.cache_read_input_tokens, 5000);
        assert_eq!(usage.cache_creation_input_tokens, 200);
    }

    #[test]
    fn test_parse_token_usage_missing_file() {
        let result = parse_token_usage(std::path::Path::new("/nonexistent/output.log"));
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_token_usage_no_result_line() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(&log_path, r#"{"type":"assistant","content":"hello"}"#).unwrap();

        assert!(parse_token_usage(&log_path).is_none());
    }

    #[test]
    fn test_parse_token_usage_live_sums_assistant_messages() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        // Simulate an in-progress run with two assistant messages (no result line)
        std::fs::write(
            &log_path,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":50,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}}
{"type":"tool_result","content":"ok"}
{"type":"assistant","message":{"usage":{"input_tokens":15,"output_tokens":30,"cache_read_input_tokens":200,"cache_creation_input_tokens":10}}}
"#,
        )
        .unwrap();

        let usage = parse_token_usage_live(&log_path).unwrap();
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.output_tokens, 80);
        assert_eq!(usage.cache_read_input_tokens, 300);
        assert_eq!(usage.cache_creation_input_tokens, 30);
        assert_eq!(usage.cost_usd, 0.0); // no cost in per-turn messages
    }

    #[test]
    fn test_parse_token_usage_live_prefers_result_line() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        // If a result line exists, it should be preferred over summing assistant messages
        std::fs::write(
            &log_path,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":50}}}
{"type":"result","total_cost_usd":1.5,"usage":{"input_tokens":100,"output_tokens":200}}
"#,
        )
        .unwrap();

        let usage = parse_token_usage_live(&log_path).unwrap();
        assert_eq!(usage.cost_usd, 1.5);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 200);
    }

    #[test]
    fn test_parse_token_usage_live_missing_file() {
        let result = parse_token_usage_live(std::path::Path::new("/nonexistent/output.log"));
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_token_usage_native_executor_result() {
        // Native executor writes "total_usage" instead of "usage" in result lines
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(
            &log_path,
            r#"{"type":"turn","turn":1,"role":"assistant","content":[],"usage":{"input_tokens":500,"output_tokens":100}}
{"type":"result","final_text":"Done.","turns":2,"total_usage":{"input_tokens":1234,"output_tokens":567,"cache_read_input_tokens":800,"cache_creation_input_tokens":50}}
"#,
        )
        .unwrap();

        let usage = parse_token_usage(&log_path).unwrap();
        assert_eq!(usage.input_tokens, 1234);
        assert_eq!(usage.output_tokens, 567);
        assert_eq!(usage.cache_read_input_tokens, 800);
        assert_eq!(usage.cache_creation_input_tokens, 50);
        assert_eq!(usage.cost_usd, 0.0); // native executor doesn't track cost
    }

    #[test]
    fn test_parse_token_usage_live_native_turn_format() {
        // Native executor writes type=turn with top-level usage (not message.usage)
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(
            &log_path,
            r#"{"type":"turn","turn":1,"role":"assistant","content":[],"usage":{"input_tokens":500,"output_tokens":100,"cache_read_input_tokens":200}}
{"type":"turn","turn":2,"role":"assistant","content":[],"usage":{"input_tokens":600,"output_tokens":150,"cache_creation_input_tokens":50}}
"#,
        )
        .unwrap();

        let usage = parse_token_usage_live(&log_path).unwrap();
        assert_eq!(usage.input_tokens, 1100);
        assert_eq!(usage.output_tokens, 250);
        assert_eq!(usage.cache_read_input_tokens, 200);
        assert_eq!(usage.cache_creation_input_tokens, 50);
    }

    #[test]
    fn test_parse_token_usage_live_native_result_preferred() {
        // If native executor has both turn and result lines, result should be used
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(
            &log_path,
            r#"{"type":"turn","turn":1,"role":"assistant","content":[],"usage":{"input_tokens":500,"output_tokens":100}}
{"type":"result","final_text":"Done.","turns":1,"total_usage":{"input_tokens":500,"output_tokens":100}}
"#,
        )
        .unwrap();

        let usage = parse_token_usage_live(&log_path).unwrap();
        // Should use the result line, not sum of turns
        assert_eq!(usage.input_tokens, 500);
        assert_eq!(usage.output_tokens, 100);
    }

    #[test]
    fn test_parse_wg_tokens_single_line() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(
            &log_path,
            "FLIP Phase 1: Inferring prompt from output...\nFLIP Phase 2: Comparing prompts...\n__WG_TOKENS__:{\"cost_usd\":0.12,\"input_tokens\":500,\"output_tokens\":200,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":50}\n",
        ).unwrap();

        let usage = parse_wg_tokens(&log_path).unwrap();
        assert!((usage.cost_usd - 0.12).abs() < f64::EPSILON);
        assert_eq!(usage.input_tokens, 500);
        assert_eq!(usage.output_tokens, 200);
        assert_eq!(usage.cache_read_input_tokens, 100);
        assert_eq!(usage.cache_creation_input_tokens, 50);
    }

    #[test]
    fn test_parse_wg_tokens_multiple_lines_accumulate() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(
            &log_path,
            "__WG_TOKENS__:{\"cost_usd\":0.1,\"input_tokens\":100,\"output_tokens\":50}\n__WG_TOKENS__:{\"cost_usd\":0.2,\"input_tokens\":200,\"output_tokens\":80}\n",
        ).unwrap();

        let usage = parse_wg_tokens(&log_path).unwrap();
        assert!((usage.cost_usd - 0.3).abs() < f64::EPSILON);
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 130);
    }

    #[test]
    fn test_parse_wg_tokens_missing_file() {
        let result = parse_wg_tokens(std::path::Path::new("/nonexistent/output.log"));
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_wg_tokens_no_tokens_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("output.log");
        std::fs::write(&log_path, "Some eval output\nAnother line\n").unwrap();
        assert!(parse_wg_tokens(&log_path).is_none());
    }

    #[test]
    fn test_token_usage_serialization_skips_zeros() {
        let usage = TokenUsage {
            cost_usd: 1.5,
            input_tokens: 0,
            output_tokens: 100,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains("cost_usd"));
        assert!(json.contains("output_tokens"));
        assert!(!json.contains("input_tokens"));
        assert!(!json.contains("cache_read"));
        assert!(!json.contains("cache_creation"));
    }

    #[test]
    fn test_task_with_token_usage_roundtrip() {
        let mut task = make_task("t1", "Test");
        task.token_usage = Some(TokenUsage {
            cost_usd: 0.5,
            input_tokens: 10,
            output_tokens: 200,
            cache_read_input_tokens: 5000,
            cache_creation_input_tokens: 100,
        });

        // Serialize as a Node (JSONL format used by parser)
        let node = Node::Task(task);
        let json = serde_json::to_string(&node).unwrap();
        let node2: Node = serde_json::from_str(&json).unwrap();

        if let Node::Task(t) = node2 {
            let usage = t.token_usage.as_ref().unwrap();
            assert_eq!(usage.cost_usd, 0.5);
            assert_eq!(usage.output_tokens, 200);
            assert_eq!(usage.total_tokens(), 5310);
        } else {
            panic!("Expected Task node");
        }
    }

    #[test]
    fn test_reactivate_cycle_skips_archived_members() {
        let mut graph = WorkGraph::new();

        // Two cycle members: one archived, one normal Done
        let mut archived = make_task("coord-a", "Archived coordinator");
        archived.status = Status::Done;
        archived.tags = vec!["archived".to_string()];
        archived.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let mut normal = make_task("coord-b", "Normal member");
        normal.status = Status::Done;
        normal.after = vec!["coord-a".to_string()];

        graph.add_node(Node::Task(archived));
        graph.add_node(Node::Task(normal));

        let members = vec!["coord-a".to_string(), "coord-b".to_string()];
        let config = CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        };

        let reactivated = reactivate_cycle(&mut graph, &members, "coord-a", &config);

        // Archived header suppresses the entire cycle — nothing reactivated
        assert_eq!(reactivated.len(), 0);

        // Both members should still be Done
        let archived = graph.get_task("coord-a").unwrap();
        assert_eq!(archived.status, Status::Done);

        let normal = graph.get_task("coord-b").unwrap();
        assert_eq!(normal.status, Status::Done);
    }

    #[test]
    fn test_reactivate_cycle_all_archived_does_not_iterate() {
        let mut graph = WorkGraph::new();

        let mut archived = make_task("coord-only", "Solo archived coordinator");
        archived.status = Status::Done;
        archived.tags = vec!["archived".to_string()];
        archived.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        graph.add_node(Node::Task(archived));

        let members = vec!["coord-only".to_string()];
        let config = CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        };

        let reactivated = reactivate_cycle(&mut graph, &members, "coord-only", &config);

        // No reactivation — the only member is archived
        assert!(reactivated.is_empty());
    }

    // ---- User board helper tests ----

    #[test]
    fn test_is_user_board() {
        assert!(is_user_board(".user-erik-0"));
        assert!(is_user_board(".user-alice-bob-3"));
        assert!(is_user_board(".user-x"));
        assert!(!is_user_board("user-erik-0"));
        assert!(!is_user_board(".coordinator-0"));
        assert!(!is_user_board("some-task"));
    }

    #[test]
    fn test_user_board_handle() {
        assert_eq!(user_board_handle(".user-erik-0"), Some("erik"));
        assert_eq!(user_board_handle(".user-erik-5"), Some("erik"));
        assert_eq!(user_board_handle(".user-alice-bob-3"), Some("alice-bob"));
        assert_eq!(user_board_handle(".user-x"), None); // no -N suffix
        assert_eq!(user_board_handle("not-a-board"), None);
    }

    #[test]
    fn test_user_board_seq() {
        assert_eq!(user_board_seq(".user-erik-0"), Some(0));
        assert_eq!(user_board_seq(".user-erik-42"), Some(42));
        assert_eq!(user_board_seq(".user-alice-bob-3"), Some(3));
        assert_eq!(user_board_seq("not-a-board"), None);
    }

    #[test]
    fn test_resolve_user_board_alias_no_match() {
        let graph = WorkGraph::new();
        // No tasks in graph — alias returns original
        assert_eq!(resolve_user_board_alias(&graph, ".user-erik"), ".user-erik");
    }

    #[test]
    fn test_resolve_user_board_alias_finds_active() {
        let mut graph = WorkGraph::new();
        let mut t0 = make_task(".user-erik-0", "Board 0");
        t0.status = Status::Done;
        let mut t1 = make_task(".user-erik-1", "Board 1");
        t1.status = Status::InProgress;
        graph.add_node(Node::Task(t0));
        graph.add_node(Node::Task(t1));

        assert_eq!(
            resolve_user_board_alias(&graph, ".user-erik"),
            ".user-erik-1"
        );
    }

    #[test]
    fn test_resolve_user_board_alias_skips_terminal() {
        let mut graph = WorkGraph::new();
        let mut t0 = make_task(".user-erik-0", "Board 0");
        t0.status = Status::Done;
        let mut t1 = make_task(".user-erik-1", "Board 1");
        t1.status = Status::Done;
        graph.add_node(Node::Task(t0));
        graph.add_node(Node::Task(t1));

        // All boards are terminal — no active board found
        assert_eq!(resolve_user_board_alias(&graph, ".user-erik"), ".user-erik");
    }

    #[test]
    fn test_resolve_user_board_alias_fully_qualified_passthrough() {
        let graph = WorkGraph::new();
        // Already has -N suffix — passthrough
        assert_eq!(
            resolve_user_board_alias(&graph, ".user-erik-0"),
            ".user-erik-0"
        );
    }

    #[test]
    fn test_resolve_user_board_alias_non_user_board_passthrough() {
        let graph = WorkGraph::new();
        assert_eq!(resolve_user_board_alias(&graph, "my-task"), "my-task");
    }

    #[test]
    fn test_next_user_board_seq_empty() {
        let graph = WorkGraph::new();
        assert_eq!(next_user_board_seq(&graph, "erik"), 0);
    }

    #[test]
    fn test_next_user_board_seq_existing() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(".user-erik-0", "Board 0")));
        graph.add_node(Node::Task(make_task(".user-erik-1", "Board 1")));
        assert_eq!(next_user_board_seq(&graph, "erik"), 2);
    }

    #[test]
    fn test_create_user_board_task() {
        let task = create_user_board_task("erik", 0);
        assert_eq!(task.id, ".user-erik-0");
        assert_eq!(task.status, Status::InProgress);
        assert!(task.tags.contains(&"user-board".to_string()));
        assert!(task.assigned.is_none());
        assert!(task.agent.is_none());
        assert!(task.created_at.is_some());
        assert!(task.started_at.is_some());
    }

    #[test]
    fn test_create_user_board_task_seq_increment() {
        let task = create_user_board_task("alice", 5);
        assert_eq!(task.id, ".user-alice-5");
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_cycle_analysis_excludes_system_scaffolding() {
        // System scaffolding tasks (.assign-*, .flip-*, etc.) should be
        // excluded from cycle analysis so they don't cause false IRREDUCIBLE
        // classifications by adding external entry points to user cycles.
        let mut graph = WorkGraph::new();

        // Create a simple 3-task cycle: a -> b -> c -> a
        let mut task_a = make_task("task-a", "Task A");
        task_a.after = vec!["task-c".into(), ".assign-task-a".into()];
        task_a.cycle_config = Some(CycleConfig {
            max_iterations: 3,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let mut task_b = make_task("task-b", "Task B");
        task_b.after = vec!["task-a".into(), ".assign-task-b".into()];

        let mut task_c = make_task("task-c", "Task C");
        task_c.after = vec!["task-b".into(), ".assign-task-c".into()];

        // Create system scaffolding tasks
        let assign_a = make_task(".assign-task-a", "Assign task-a");
        let assign_b = make_task(".assign-task-b", "Assign task-b");
        let assign_c = make_task(".assign-task-c", "Assign task-c");
        let flip_a = make_task(".flip-task-a", "FLIP task-a");

        graph.add_node(Node::Task(task_a));
        graph.add_node(Node::Task(task_b));
        graph.add_node(Node::Task(task_c));
        graph.add_node(Node::Task(assign_a));
        graph.add_node(Node::Task(assign_b));
        graph.add_node(Node::Task(assign_c));
        graph.add_node(Node::Task(flip_a));

        let analysis = graph.compute_cycle_analysis();

        // Should detect exactly one cycle with members {task-a, task-b, task-c}
        assert_eq!(analysis.cycles.len(), 1);
        let cycle = &analysis.cycles[0];
        assert_eq!(cycle.members.len(), 3);
        assert!(cycle.members.contains(&"task-a".to_string()));
        assert!(cycle.members.contains(&"task-b".to_string()));
        assert!(cycle.members.contains(&"task-c".to_string()));

        // The cycle should be REDUCIBLE (single entry point) since .assign-*
        // tasks are filtered out and don't create external entry points
        assert!(
            cycle.reducible,
            "Cycle should be REDUCIBLE when system scaffolding is excluded"
        );

        // System tasks should NOT appear in task_to_cycle mapping
        assert!(analysis.task_to_cycle.get(".assign-task-a").is_none());
        assert!(analysis.task_to_cycle.get(".assign-task-b").is_none());
        assert!(analysis.task_to_cycle.get(".assign-task-c").is_none());
        assert!(analysis.task_to_cycle.get(".flip-task-a").is_none());
    }

    #[test]
    fn test_priority() {
        // Test Priority enum default values
        assert_eq!(Priority::default(), Priority::Normal);

        // Test Priority ordering (PartialOrd implementation)
        assert!(Priority::Critical < Priority::High);
        assert!(Priority::High < Priority::Normal);
        assert!(Priority::Normal < Priority::Low);
        assert!(Priority::Low < Priority::Idle);

        // Test Priority display
        assert_eq!(format!("{}", Priority::Critical), "critical");
        assert_eq!(format!("{}", Priority::High), "high");
        assert_eq!(format!("{}", Priority::Normal), "normal");
        assert_eq!(format!("{}", Priority::Low), "low");
        assert_eq!(format!("{}", Priority::Idle), "idle");

        // Test Task with priority field
        let task = Task {
            id: "test-task".to_string(),
            title: "Test Task".to_string(),
            priority: Priority::High,
            ..Task::default()
        };
        assert_eq!(task.priority, Priority::High);

        // Test Task default priority is Normal
        let default_task = Task::default();
        assert_eq!(default_task.priority, Priority::Normal);

        // Test serialization/deserialization compatibility
        use serde_json;

        // Test serialization
        let json_str = serde_json::to_string(&Priority::Critical).unwrap();
        assert_eq!(json_str, "\"critical\"");

        // Test deserialization
        let priority: Priority = serde_json::from_str("\"high\"").unwrap();
        assert_eq!(priority, Priority::High);

        // Test backward compatibility - missing priority field should default to Normal
        let json_without_priority = r#"{"id": "test", "title": "Test", "status": "open"}"#;

        // This should deserialize successfully with default priority
        #[derive(serde::Deserialize)]
        struct TestTask {
            #[serde(rename = "id")]
            _id: String,
            #[serde(rename = "title")]
            _title: String,
            #[serde(default)]
            priority: Priority,
        }

        let parsed: TestTask = serde_json::from_str(json_without_priority).unwrap();
        assert_eq!(parsed.priority, Priority::Normal);
    }
}
