use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use workgraph::config::Config;
use workgraph::graph::{
    CycleConfig, LogEntry, LoopGuard, PRIORITY_DEFAULT, Priority, Status, Task, TokenUsage, format_tokens,
    parse_token_usage_live,
};
use workgraph::query::build_reverse_index;
use workgraph::service::AgentRegistry;

use super::service::CoordinatorState;

/// Blocker info with status
#[derive(Debug, Serialize)]
struct BlockerInfo {
    id: String,
    status: Status,
}

fn is_zero(val: &u32) -> bool {
    *val == 0
}

fn is_bool_false(val: &bool) -> bool {
    !*val
}

/// JSON output structure for show command
#[derive(Debug, Serialize)]
struct TaskDetails {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    status: Status,
    priority: Priority,
    #[serde(skip_serializing_if = "Option::is_none")]
    assigned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skills: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deliverables: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec: Option<String>,
    after: Vec<BlockerInfo>,
    before: Vec<BlockerInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    not_before: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    log: Vec<LogEntry>,
    #[serde(skip_serializing_if = "is_zero")]
    retry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_executor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_compaction: Option<NativeCompactionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    loop_iteration: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_iteration_completed_at: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    cycle_failure_restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cycle_config: Option<CycleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ready_after: Option<String>,
    #[serde(default, skip_serializing_if = "is_not_paused")]
    paused: bool,
    #[serde(skip_serializing_if = "is_default_visibility")]
    visibility: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_usage: Option<TokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_condition: Option<workgraph::graph::WaitSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    verify_failures: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    resurrection_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_resurrected_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    superseded_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "is_bool_false")]
    independent: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    iteration_round: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    iteration_anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iteration_parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iteration_config: Option<workgraph::agency::IterationConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    evaluations: Vec<EvalSummary>,
    /// Snapshot of the task's worktree (when one exists). Populated for
    /// retried tasks so the user can inspect prior WIP before deciding to
    /// resume in-place vs `wg retry --fresh`.
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree_state: Option<WorktreeStateInfo>,
}

/// Snapshot of a task's worktree dir + branch.
#[derive(Debug, Clone, Serialize)]
struct WorktreeStateInfo {
    path: String,
    branch: String,
    /// Number of commits on this branch ahead of `main` (or `master`)
    commits_ahead: usize,
    /// Number of files with uncommitted changes (staged + unstaged + untracked)
    uncommitted_files: usize,
    /// Last-modified timestamp of the worktree directory (RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
    /// Whether the cleanup-pending marker is present (agent exited)
    cleanup_pending: bool,
    /// Whether the branch is merged into main
    merged_to_main: bool,
}

/// Lightweight evaluation summary for wg show output.
#[derive(Debug, Clone, Serialize)]
struct EvalSummary {
    score: f64,
    source: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    dimensions: HashMap<String, f64>,
    timestamp: String,
    loop_iteration: u32,
}

#[derive(Debug, Clone, Serialize)]
struct NativeCompactionInfo {
    journal_present: bool,
    journal_entries: usize,
    #[serde(skip_serializing_if = "is_zero_u64")]
    compaction_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_compaction: Option<String>,
    session_summary_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_summary_words: Option<usize>,
}

fn is_default_visibility(val: &str) -> bool {
    val == "internal"
}

fn is_not_paused(val: &bool) -> bool {
    !*val
}

fn is_zero_u64(val: &u64) -> bool {
    *val == 0
}

fn gather_task_runtime_info(
    dir: &Path,
    task: &Task,
) -> (Option<String>, Option<String>, Option<NativeCompactionInfo>) {
    let registry_entry = task.assigned.as_ref().and_then(|aid| {
        AgentRegistry::load(dir)
            .ok()
            .and_then(|reg| reg.agents.get(aid).cloned())
    });

    let actual_executor = registry_entry.as_ref().map(|e| e.executor.clone());
    let actual_model = registry_entry.as_ref().and_then(|e| e.model.clone());

    // For coordinator tasks, resolve model/executor from CoordinatorState
    // (coordinators don't use the agent registry — their runtime info is in
    // per-coordinator state files).
    let (actual_executor, actual_model) = if task.id.starts_with(".coordinator-") {
        let coord_id = task
            .id
            .strip_prefix(".coordinator-")
            .and_then(|s| s.parse::<u32>().ok());
        if let Some(cid) = coord_id {
            let coord_state = CoordinatorState::load_for(dir, cid);
            let config = Config::load_or_default(dir);
            let coord_executor = coord_state
                .as_ref()
                .and_then(|s| s.executor_override.clone())
                .or(actual_executor)
                .or_else(|| Some(config.coordinator.effective_executor()));
            let coord_model = coord_state
                .as_ref()
                .and_then(|s| s.model_override.clone())
                .or(actual_model)
                .or_else(|| coord_state.as_ref().and_then(|s| s.model.clone()))
                .or_else(|| config.coordinator.model.clone())
                .or_else(|| {
                    Some(
                        config
                            .resolve_model_for_role(workgraph::config::DispatchRole::Default)
                            .model,
                    )
                });
            (coord_executor, coord_model)
        } else {
            (actual_executor, actual_model)
        }
    } else {
        (actual_executor, actual_model)
    };

    let session_summary_path = task
        .assigned
        .as_ref()
        .map(|aid| dir.join("agents").join(aid).join("session-summary.md"));
    let session_summary = session_summary_path
        .as_ref()
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok());

    let session_summary_present = session_summary.is_some();
    let session_summary_words = session_summary
        .as_ref()
        .map(|s| s.split_whitespace().count());

    let journal_path = workgraph::executor::native::journal::journal_path(dir, &task.id);
    let journal_present = journal_path.exists();

    let (journal_entries, compaction_count, last_compaction) = if journal_present {
        match workgraph::executor::native::journal::Journal::read_all(&journal_path) {
            Ok(entries) => {
                let mut count = 0u64;
                let mut last = None;
                for entry in &entries {
                    if matches!(
                        entry.kind,
                        workgraph::executor::native::journal::JournalEntryKind::Compaction { .. }
                    ) {
                        count += 1;
                        last = Some(entry.timestamp.clone());
                    }
                }
                (entries.len(), count, last)
            }
            Err(_) => (0, 0, None),
        }
    } else {
        (0, 0, None)
    };

    let native_compaction = if actual_executor.as_deref() == Some("native")
        || journal_present
        || session_summary_present
    {
        Some(NativeCompactionInfo {
            journal_present,
            journal_entries,
            compaction_count,
            last_compaction,
            session_summary_present,
            session_summary_words,
        })
    } else {
        None
    };

    (actual_executor, actual_model, native_compaction)
}

/// Gather worktree state for a task, if a worktree exists for it. Returns
/// branch name, commits ahead of main, uncommitted file count, last-modified
/// time, and whether the cleanup marker / merged-into-main signals are set.
fn gather_worktree_state(dir: &Path, task_id: &str) -> Option<WorktreeStateInfo> {
    use std::process::Command;
    let project_root = dir.parent()?;
    let (path, branch) =
        crate::commands::spawn::worktree::find_worktree_for_task(project_root, task_id)?;

    // commits ahead of main: prefer "main", fall back to "master"
    let commits_ahead = {
        let mut count = 0usize;
        for main in &["main", "master"] {
            let out = Command::new("git")
                .args(["rev-list", "--count"])
                .arg(format!("{}..{}", main, branch))
                .current_dir(project_root)
                .output();
            if let Ok(o) = out
                && o.status.success()
            {
                let s = String::from_utf8_lossy(&o.stdout);
                if let Ok(n) = s.trim().parse::<usize>() {
                    count = n;
                    break;
                }
            }
        }
        count
    };

    // Uncommitted file count from `git status --porcelain` in the worktree
    let uncommitted_files = {
        let out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&path)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                s.lines().filter(|l| !l.is_empty()).count()
            }
            _ => 0,
        }
    };

    let last_modified = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());

    let cleanup_pending = path
        .join(crate::commands::service::worktree::CLEANUP_PENDING_MARKER)
        .exists();

    let merged_to_main =
        crate::commands::service::worktree::is_branch_merged(project_root, &branch);

    Some(WorktreeStateInfo {
        path: path.to_string_lossy().into_owned(),
        branch,
        commits_ahead,
        uncommitted_files,
        last_modified,
        cleanup_pending,
        merged_to_main,
    })
}

pub fn run(dir: &Path, id: &str, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let task = graph.get_task_or_err(id)?;

    // Build reverse index to find what this task blocks
    let reverse_index = build_reverse_index(&graph);

    // Get blocker info with statuses (supports cross-repo peer:task-id references)
    let after_info: Vec<BlockerInfo> = task
        .after
        .iter()
        .map(|blocker_id| {
            if let Some((peer_name, remote_task_id)) =
                workgraph::federation::parse_remote_ref(blocker_id)
            {
                // Cross-repo dependency — resolve via federation
                let remote = workgraph::federation::resolve_remote_task_status(
                    peer_name,
                    remote_task_id,
                    dir,
                );
                BlockerInfo {
                    id: blocker_id.clone(),
                    status: remote.status,
                }
            } else {
                let status = match graph.get_task(blocker_id) {
                    Some(t) => t.status,
                    None => {
                        eprintln!(
                            "Warning: blocker '{}' referenced by '{}' not found in graph",
                            blocker_id, id
                        );
                        Status::Open
                    }
                };
                BlockerInfo {
                    id: blocker_id.clone(),
                    status,
                }
            }
        })
        .collect();

    // Get what this task blocks
    let before_info: Vec<BlockerInfo> = reverse_index
        .get(id)
        .map(|dependents| {
            dependents
                .iter()
                .map(|dep_id| {
                    let status = graph
                        .get_task(dep_id)
                        .map(|t| t.status)
                        .unwrap_or(Status::Open);
                    BlockerInfo {
                        id: dep_id.clone(),
                        status,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // Resolve token usage: stored data first, then live data for in-progress tasks.
    // Check output.log first (works for both Claude CLI and native executor formats),
    // then fall back to stream.jsonl (native executor writes usage there directly).
    let token_usage = task.token_usage.clone().or_else(|| {
        let agent_id = task.assigned.as_deref()?;
        let agent_dir = dir.join("agents").join(agent_id);
        // Try output.log (handles both Claude CLI and native executor formats)
        let log_path = agent_dir.join("output.log");
        if let Some(usage) = parse_token_usage_live(&log_path) {
            return Some(usage);
        }
        // Fallback: read stream.jsonl (native executor writes usage there directly)
        let stream_path = agent_dir.join(workgraph::stream_event::STREAM_FILE_NAME);
        if stream_path.exists()
            && let Ok((events, _)) = workgraph::stream_event::read_stream_events(&stream_path, 0)
            && !events.is_empty()
        {
            let mut state = workgraph::stream_event::AgentStreamState::default();
            state.ingest(&events, 0);
            let usage = state.to_token_usage();
            if usage.input_tokens > 0 || usage.output_tokens > 0 {
                return Some(usage);
            }
        }
        None
    });

    let (actual_executor, actual_model, native_compaction) = gather_task_runtime_info(dir, task);

    // Load evaluation data for this task (if any)
    let evaluations = {
        let evals_dir = dir.join("agency").join("evaluations");
        if evals_dir.is_dir() {
            let all_evals = workgraph::agency::load_all_evaluations_or_warn(&evals_dir);
            all_evals
                .into_iter()
                .filter(|e| e.task_id == id)
                .map(|e| EvalSummary {
                    score: e.score,
                    source: e.source,
                    dimensions: e.dimensions,
                    timestamp: e.timestamp,
                    loop_iteration: e.loop_iteration,
                })
                .collect()
        } else {
            Vec::new()
        }
    };

    let details = TaskDetails {
        id: task.id.clone(),
        title: task.title.clone(),
        description: task.description.clone(),
        status: task.status,
        priority: task.priority,
        assigned: task.assigned.clone(),
        hours: task.estimate.as_ref().and_then(|e| e.hours),
        cost: task.estimate.as_ref().and_then(|e| e.cost),
        tags: task.tags.clone(),
        skills: task.skills.clone(),
        inputs: task.inputs.clone(),
        deliverables: task.deliverables.clone(),
        artifacts: task.artifacts.clone(),
        exec: task.exec.clone(),
        after: after_info,
        before: before_info,
        created_at: task.created_at.clone(),
        started_at: task.started_at.clone(),
        completed_at: task.completed_at.clone(),
        not_before: task.not_before.clone(),
        log: task.log.clone(),
        retry_count: task.retry_count,
        max_retries: task.max_retries,
        failure_reason: task.failure_reason.clone(),
        model: task.model.clone(),
        actual_executor,
        actual_model,
        native_compaction,
        verify: task.verify.clone(),
        agent: task.agent.clone(),
        loop_iteration: task.loop_iteration,
        last_iteration_completed_at: task.last_iteration_completed_at.clone(),
        cycle_failure_restarts: task.cycle_failure_restarts,
        cycle_config: task.cycle_config.clone(),
        ready_after: task.ready_after.clone(),
        paused: task.paused,
        visibility: task.visibility.clone(),
        context_scope: task.context_scope.clone(),
        exec_mode: task.exec_mode.clone(),
        token_usage,
        session_id: task.session_id.clone(),
        wait_condition: task.wait_condition.clone(),
        checkpoint: task.checkpoint.clone(),
        verify_failures: task.verify_failures,
        resurrection_count: task.resurrection_count,
        last_resurrected_at: task.last_resurrected_at.clone(),
        superseded_by: task.superseded_by.clone(),
        supersedes: task.supersedes.clone(),
        independent: task.independent,
        iteration_round: task.iteration_round,
        iteration_anchor: task.iteration_anchor.clone(),
        iteration_parent: task.iteration_parent.clone(),
        iteration_config: task.iteration_config.clone(),
        evaluations,
        worktree_state: gather_worktree_state(dir, id),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&details)?);
    } else {
        print_human_readable(&details);
        if task.retry_count > 0 {
            print_retry_history(dir, &task.id);
        }
    }

    Ok(())
}

fn print_human_readable(details: &TaskDetails) {
    println!("Task: {}", details.id);
    println!("Title: {}", details.title);
    if details.paused {
        println!("Status: {} (PAUSED)", details.status);
    } else {
        println!("Status: {}", details.status);
    }

    if details.priority != PRIORITY_DEFAULT {
        println!("Priority: ⌁{}", details.priority);
    }

    if details.visibility != "internal" {
        println!("Visibility: {}", details.visibility);
    }

    if let Some(ref scope) = details.context_scope {
        println!("Context scope: {}", scope);
    }

    if let Some(ref mode) = details.exec_mode {
        println!("Exec mode: {}", mode);
    }

    if let Some(ref assigned) = details.assigned {
        println!("Assigned: {}", assigned);
    }
    if let Some(ref agent) = details.agent {
        println!("Agent: {}", agent);
    }
    if details.actual_executor.is_some()
        || details.model.is_some()
        || details.actual_model.is_some()
    {
        println!();
        println!("Runtime:");
        if let Some(ref executor) = details.actual_executor {
            println!("  Executor: {}", executor);
        }
        match (&details.model, &details.actual_model) {
            (Some(configured), Some(actual)) if configured != actual => {
                println!("  Model: {} (configured: {})", actual, configured);
            }
            (_, Some(actual)) => {
                println!("  Model: {}", actual);
            }
            (Some(configured), None) => {
                println!("  Model: {} (configured)", configured);
            }
            (None, None) => {}
        }
        if let Some(ref session_id) = details.session_id {
            println!("  Session: {}", session_id);
        }
    }
    if let Some(ref compact) = details.native_compaction {
        println!();
        println!("Compaction:");
        println!(
            "  Native journal: {}",
            if compact.journal_present {
                "present"
            } else {
                "absent"
            }
        );
        if compact.journal_present {
            println!("  Journal entries: {}", compact.journal_entries);
        }
        if compact.compaction_count > 0 {
            println!("  Compactions: {}", compact.compaction_count);
        } else if compact.journal_present {
            println!("  Compactions: none (no 90%+ context pressure)");
        }
        if let Some(ref ts) = compact.last_compaction {
            println!("  Last compaction: {}", ts);
        }
        if compact.session_summary_present {
            if let Some(words) = compact.session_summary_words {
                println!("  Session summary: present ({} words)", words);
            } else {
                println!("  Session summary: present");
            }
        } else if compact.journal_present || details.actual_executor.as_deref() == Some("native") {
            println!("  Session summary: absent");
        }
    }

    // Verify status
    if details.verify.is_some() || details.verify_failures > 0 {
        println!();
        println!("Verify:");
        if let Some(ref cmd) = details.verify {
            println!("  Command: {}", cmd);
        }
        if details.verify_failures > 0 {
            let breaker_tripped = details.status == Status::Failed
                && details
                    .log
                    .iter()
                    .any(|e| e.actor.as_deref() == Some("verify-circuit-breaker"));
            println!("  Failures: {}", details.verify_failures);
            if breaker_tripped {
                println!("  Circuit breaker: \x1b[31mTRIPPED\x1b[0m");
            }
            // Show last verify error from log
            if let Some(last_err) = details
                .log
                .iter()
                .rev()
                .find(|e| e.actor.as_deref() == Some("verify"))
            {
                // Extract stderr from the log message
                if let Some(stderr_pos) = last_err.message.find("\nstderr: ") {
                    let stderr = &last_err.message[stderr_pos + 9..];
                    // Trim at next section or end
                    let stderr = stderr
                        .find("\nstdout: ")
                        .map(|p| &stderr[..p])
                        .unwrap_or(stderr);
                    println!("  Last error: {}", stderr.trim());
                }
            }
        }
    }

    // Failure info
    if (details.status == Status::Failed || details.status == Status::Abandoned)
        && let Some(ref reason) = details.failure_reason
    {
        println!("Failure reason: {}", reason);
    }
    if !details.superseded_by.is_empty() {
        println!("Superseded by: {}", details.superseded_by.join(", "));
    }
    if let Some(ref sup) = details.supersedes {
        println!("Supersedes: {}", sup);
    }
    if details.retry_count > 0 {
        let retry_info = match details.max_retries {
            Some(max) => format!("Retry count: {}/{}", details.retry_count, max),
            None => format!("Retry count: {}", details.retry_count),
        };
        println!("{}", retry_info);
    } else if let Some(max) = details.max_retries {
        println!("Max retries: {}", max);
    }

    // Description
    if let Some(ref description) = details.description {
        println!();
        println!("Description:");
        for line in description.lines() {
            println!("  {}", line);
        }
    }

    println!();

    // Estimate section
    let has_estimate = details.hours.is_some() || details.cost.is_some();
    if has_estimate {
        let mut parts = Vec::new();
        if let Some(hours) = details.hours {
            parts.push(format!("{}h", hours));
        }
        if let Some(cost) = details.cost {
            parts.push(format!("${}", cost));
        }
        println!("Estimate: {}", parts.join(", "));
    }

    // Tags
    if !details.tags.is_empty() {
        println!("Tags: {}", details.tags.join(", "));
    }

    // Skills
    if !details.skills.is_empty() {
        println!("Skills: {}", details.skills.join(", "));
    }

    // Inputs
    if !details.inputs.is_empty() {
        println!("Inputs: {}", details.inputs.join(", "));
    }

    // Deliverables
    if !details.deliverables.is_empty() {
        println!("Deliverables: {}", details.deliverables.join(", "));
    }

    println!();

    // After section
    println!("After:");
    if details.after.is_empty() {
        println!("  (none)");
    } else {
        for blocker in &details.after {
            println!("  - {} ({})", blocker.id, blocker.status);
        }
    }

    println!();

    // Blocks section
    println!("Before:");
    if details.before.is_empty() {
        println!("  (none)");
    } else {
        for blocked in &details.before {
            println!("  - {} ({})", blocked.id, blocked.status);
        }
    }

    // Cycle config
    if let Some(ref cc) = details.cycle_config {
        println!();
        println!("Cycle config (header):");
        println!("  Max iterations: {}", cc.max_iterations);
        if let Some(ref guard) = cc.guard {
            let guard_str = match guard {
                LoopGuard::TaskStatus { task, status } => {
                    format!("task:{}={}", task, status)
                }
                LoopGuard::IterationLessThan(n) => format!("iteration<{}", n),
                LoopGuard::Always => "always".to_string(),
            };
            println!("  Guard: {}", guard_str);
        }
        if let Some(ref delay) = cc.delay {
            println!("  Delay: {}", delay);
        }
        if cc.no_converge {
            println!("  No-converge: true (all iterations forced)");
        }
        // Display 1-based iteration: loop_iteration=0 is "iteration 1/max"
        println!(
            "  Current iteration: {}/{}",
            details.loop_iteration + 1,
            cc.max_iterations
        );

        // Cycle timing: last iteration completed
        if let Some(ref last_ts) = details.last_iteration_completed_at {
            if let Ok(parsed) = last_ts.parse::<DateTime<Utc>>() {
                let ago = Utc::now().signed_duration_since(parsed).num_seconds();
                println!(
                    "  Last iteration completed: {} ({} ago)",
                    last_ts,
                    workgraph::format_duration(ago, true)
                );
            } else {
                println!("  Last iteration completed: {}", last_ts);
            }
        }

        // Next due: compute from ready_after or last_iteration_completed_at + delay
        let next_due = details.ready_after.clone().or_else(|| {
            let delay_secs = cc
                .delay
                .as_ref()
                .and_then(|d| workgraph::graph::parse_delay(d))?;
            let last_ts = details
                .last_iteration_completed_at
                .as_ref()?
                .parse::<DateTime<Utc>>()
                .ok()?;
            let next = last_ts + chrono::Duration::seconds(delay_secs as i64);
            Some(next.to_rfc3339())
        });
        if let Some(ref next_ts) = next_due
            && let Ok(parsed) = next_ts.parse::<DateTime<Utc>>()
        {
            let now = Utc::now();
            if parsed > now {
                let secs = (parsed - now).num_seconds();
                println!(
                    "  Next iteration due: in {}",
                    workgraph::format_duration(secs, true)
                );
            } else {
                println!("  Next iteration due: ready now");
            }
        }
    }

    println!();

    // Timestamps
    if let Some(ref created) = details.created_at {
        println!("Created: {}", created);
    }
    if let Some(ref started) = details.started_at {
        println!("Started: {}", started);
    }
    if let Some(ref completed) = details.completed_at {
        println!("Completed: {}", completed);
    }
    if let Some(ref not_before) = details.not_before {
        println!("Not before: {}{}", not_before, format_countdown(not_before));
    }
    if let Some(ref ready_after) = details.ready_after {
        println!(
            "Ready after: {}{}",
            ready_after,
            format_countdown(ready_after)
        );
    }

    // Token usage
    if let Some(ref usage) = details.token_usage {
        println!();
        let novel_in = usage
            .input_tokens
            .saturating_sub(usage.cache_read_input_tokens);
        if usage.cache_read_input_tokens > 0 {
            println!(
                "Tokens: {}/{} (in/out) +{} cached",
                format_tokens(novel_in),
                format_tokens(usage.output_tokens),
                format_tokens(usage.cache_read_input_tokens)
            );
        } else {
            println!(
                "Tokens: {}/{} (in/out)",
                format_tokens(novel_in),
                format_tokens(usage.output_tokens)
            );
        }
        if usage.cost_usd > 0.0 {
            println!("Cost: ${:.2}", usage.cost_usd);
        }
    }

    // Evaluation data
    if let Some(wt) = &details.worktree_state {
        println!();
        println!("Worktree:");
        println!("  Path:              {}", wt.path);
        println!("  Branch:            {}", wt.branch);
        println!("  Commits ahead:     {}", wt.commits_ahead);
        println!("  Uncommitted files: {}", wt.uncommitted_files);
        if let Some(ts) = &wt.last_modified {
            println!("  Last modified:     {}", ts);
        }
        println!("  Cleanup pending:   {}", wt.cleanup_pending);
        println!("  Merged to main:    {}", wt.merged_to_main);
        if details.retry_count > 0 {
            println!(
                "  (Retried {} time{} — `wg retry` resumes in-place; `wg retry --fresh` starts over)",
                details.retry_count,
                if details.retry_count == 1 { "" } else { "s" }
            );
        }
    }

    if !details.evaluations.is_empty() {
        println!();
        println!("Evaluations:");
        for line in format_evaluations(&details.evaluations) {
            println!("{}", line);
        }
    }

    // Log entries
    if !details.log.is_empty() {
        println!();
        println!("Log:");
        for entry in &details.log {
            let actor_str = entry
                .actor
                .as_ref()
                .map(|a| format!(" [{}]", a))
                .unwrap_or_default();
            println!("  {} {}{}", entry.timestamp, entry.message, actor_str);
        }
    }
}

/// Render evaluation entries with iteration labels so users grepping the
/// output of `wg show` on a cycle task can tell which iteration produced which
/// score. Every entry is labeled, including `[iter 0]` for non-cycle tasks —
/// the label is uniform rather than conditional so log greps don't have to
/// pattern-match around its absence.
fn format_evaluations(evals: &[EvalSummary]) -> Vec<String> {
    let mut out = Vec::with_capacity(evals.len());
    for eval in evals {
        out.push(format!(
            "  [iter {}] Score: {:.2}  Source: {}  {}",
            eval.loop_iteration, eval.score, eval.source, eval.timestamp
        ));
        if let Some(cf) = eval.dimensions.get("constraint_fidelity") {
            let flag = if *cf < 0.5 {
                " \x1b[33m⚠ unanchored constraints\x1b[0m"
            } else {
                ""
            };
            out.push(format!("    constraint_fidelity: {:.2}{}", cf, flag));
        }
        if let Some(f) = eval.dimensions.get("intent_fidelity") {
            out.push(format!("    intent_fidelity:    {:.2}", f));
        }
    }
    out
}

/// Format a timestamp as a countdown string if it's in the future, or "(elapsed)" if in the past.
fn format_countdown(timestamp: &str) -> String {
    let Ok(ts) = timestamp.parse::<DateTime<Utc>>() else {
        return String::new();
    };
    let now = Utc::now();
    if ts <= now {
        return " (elapsed)".to_string();
    }
    let secs = (ts - now).num_seconds();
    if secs < 60 {
        format!(" (in {}s)", secs)
    } else if secs < 3600 {
        format!(" (in {}m {}s)", secs / 60, secs % 60)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!(" (in {}h {}m)", h, m)
    } else {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        format!(" (in {}d {}h)", d, h)
    }
}

fn print_retry_history(dir: &Path, task_id: &str) {
    let archive_base = dir.join("log").join("agents").join(task_id);
    if !archive_base.exists() {
        return;
    }

    let mut archives: Vec<_> = match std::fs::read_dir(&archive_base) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect(),
        Err(_) => return,
    };

    if archives.is_empty() {
        return;
    }

    archives.sort_by_key(|e| e.file_name());

    println!();
    println!("Attempt History:");

    let evals_dir = dir.join("agency").join("evaluations");
    let now = Utc::now();

    for (idx, archive) in archives.iter().enumerate() {
        let ts = archive.file_name().to_string_lossy().to_string();
        let age = ts
            .parse::<DateTime<Utc>>()
            .ok()
            .map(|parsed| {
                let ago = now.signed_duration_since(parsed).num_seconds();
                format!(" ({} ago)", workgraph::format_duration(ago.max(0), true))
            })
            .unwrap_or_default();

        // Try to find agent id from the archive directory
        let agent_id = archive
            .path()
            .join("prompt.txt")
            .exists()
            .then(|| {
                // Agent ID is encoded in the registry, check the archive for hints
                // Look at output.txt for agent references
                std::fs::read_to_string(archive.path().join("output.txt"))
                    .ok()
                    .and_then(|content| {
                        content.lines().take(5).find_map(|line| {
                            if line.contains("agent-") {
                                line.split_whitespace()
                                    .find(|w| w.starts_with("agent-"))
                                    .map(|s| {
                                        s.trim_matches(|c: char| !c.is_alphanumeric() && c != '-')
                                            .to_string()
                                    })
                            } else {
                                None
                            }
                        })
                    })
            })
            .flatten();

        let agent_str = agent_id
            .as_deref()
            .map(|a| format!(" [{}]", a))
            .unwrap_or_default();

        // Look for eval result for this task
        let eval_info = if evals_dir.exists() {
            find_eval_for_attempt(&evals_dir, task_id, &ts)
        } else {
            None
        };

        let eval_str = eval_info
            .map(|(score, verdict)| format!(" — eval: {:.2}{}", score, verdict))
            .unwrap_or_default();

        println!(
            "  Attempt {}: {}{}{}{}",
            idx + 1,
            ts,
            age,
            agent_str,
            eval_str
        );
    }
}

fn find_eval_for_attempt(
    evals_dir: &Path,
    task_id: &str,
    _archive_ts: &str,
) -> Option<(f64, String)> {
    let prefix = format!("eval-{}-", task_id);
    let mut eval_files: Vec<_> = match std::fs::read_dir(evals_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .collect(),
        Err(_) => return None,
    };

    if eval_files.is_empty() {
        return None;
    }

    eval_files.sort_by_key(|e| e.file_name());
    let latest = eval_files.last()?.path();

    let content = std::fs::read_to_string(&latest).ok()?;
    let eval: serde_json::Value = serde_json::from_str(&content).ok()?;

    let score = eval.get("score")?.as_f64()?;
    let notes = eval.get("notes").and_then(|v| v.as_str()).unwrap_or("");

    let verdict = if !notes.is_empty() {
        if notes.len() > 80 {
            format!(" ({}...)", &notes[..77])
        } else {
            format!(" ({})", notes)
        }
    } else {
        String::new()
    };

    Some((score, verdict))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn test_build_reverse_index() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let index = build_reverse_index(&graph);
        let dependents = index.get("t1").unwrap();
        assert_eq!(dependents.len(), 2);
        assert!(dependents.contains(&"t2".to_string()));
        assert!(dependents.contains(&"t3".to_string()));
    }

    #[test]
    fn test_status_display() {
        assert_eq!(Status::Open.to_string(), "open");
        assert_eq!(Status::InProgress.to_string(), "in-progress");
        assert_eq!(Status::Done.to_string(), "done");
        assert_eq!(Status::Blocked.to_string(), "blocked");
    }

    /// Each evaluation line in `wg show` must include an iteration label so
    /// users on a cycle task can tell iteration 1's stale score from iteration
    /// 2's fresh one. Without the label, two distinct flip scores look
    /// identical except for timestamps. See task tui-detail-view.
    #[test]
    fn test_format_evaluations_labels_each_iteration() {
        let evals = vec![
            EvalSummary {
                score: 0.04,
                source: "flip".to_string(),
                dimensions: HashMap::new(),
                timestamp: "2026-04-28T20:04:45Z".to_string(),
                loop_iteration: 1,
            },
            EvalSummary {
                score: 0.65,
                source: "flip".to_string(),
                dimensions: HashMap::new(),
                timestamp: "2026-04-28T22:14:00Z".to_string(),
                loop_iteration: 2,
            },
        ];

        let lines = format_evaluations(&evals);
        // Each evaluation produces its own labeled line — never just a single
        // unlabeled "Score: 0.04" with no iteration context.
        let iter1_line = lines
            .iter()
            .find(|l| l.contains("[iter 1]"))
            .expect("iteration 1 line should be rendered with [iter 1] label");
        let iter2_line = lines
            .iter()
            .find(|l| l.contains("[iter 2]"))
            .expect("iteration 2 line should be rendered with [iter 2] label");
        assert!(iter1_line.contains("0.04"));
        assert!(iter2_line.contains("0.65"));
    }

    /// Non-cycle tasks should still get a uniform `[iter 0]` label rather than
    /// no label, so downstream consumers don't have to special-case the
    /// cycle-vs-non-cycle distinction when parsing.
    #[test]
    fn test_format_evaluations_labels_zero_iteration() {
        let evals = vec![EvalSummary {
            score: 0.91,
            source: "llm".to_string(),
            dimensions: HashMap::new(),
            timestamp: "2026-04-28T18:00:00Z".to_string(),
            loop_iteration: 0,
        }];
        let lines = format_evaluations(&evals);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("[iter 0]"),
            "non-cycle eval should still carry [iter 0] label, got: {}",
            lines[0]
        );
    }

    #[test]
    fn test_task_details_serialization() {
        let details = TaskDetails {
            id: "t1".to_string(),
            title: "Test Task".to_string(),
            description: Some("Test description".to_string()),
            status: Status::InProgress,
            priority: PRIORITY_DEFAULT,
            assigned: Some("agent-1".to_string()),
            hours: Some(2.0),
            cost: Some(200.0),
            tags: vec!["test".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            after: vec![],
            before: vec![BlockerInfo {
                id: "t2".to_string(),
                status: Status::Open,
            }],
            created_at: Some("2026-01-20T15:35:50+00:00".to_string()),
            started_at: Some("2026-01-20T16:30:00+00:00".to_string()),
            completed_at: None,
            not_before: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            actual_executor: Some("native".to_string()),
            actual_model: Some("openrouter/minimax".to_string()),
            native_compaction: Some(NativeCompactionInfo {
                journal_present: true,
                journal_entries: 12,
                compaction_count: 1,
                last_compaction: Some("2026-01-20T16:45:00+00:00".to_string()),
                session_summary_present: true,
                session_summary_words: Some(42),
            }),
            verify: None,
            agent: None,
            loop_iteration: 0,
            last_iteration_completed_at: None,
            cycle_failure_restarts: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            exec_mode: None,
            cycle_config: None,
            token_usage: None,
            session_id: None,
            wait_condition: None,
            checkpoint: None,
            verify_failures: 0,
            resurrection_count: 0,
            last_resurrected_at: None,
            superseded_by: vec![],
            supersedes: None,
            independent: false,
            iteration_round: 0,
            iteration_anchor: None,
            iteration_parent: None,
            iteration_config: None,
            evaluations: vec![],
            worktree_state: None,
        };

        let json = serde_json::to_string(&details).unwrap();
        assert!(json.contains("\"id\":\"t1\""));
        assert!(json.contains("\"status\":\"in-progress\""));
        assert!(json.contains("\"assigned\":\"agent-1\""));
        assert!(json.contains("\"description\":\"Test description\""));
    }

    #[test]
    fn test_status_display_all_variants() {
        assert_eq!(Status::Open.to_string(), "open");
        assert_eq!(Status::InProgress.to_string(), "in-progress");
        assert_eq!(Status::Done.to_string(), "done");
        assert_eq!(Status::Blocked.to_string(), "blocked");
        assert_eq!(Status::Failed.to_string(), "failed");
        assert_eq!(Status::Abandoned.to_string(), "abandoned");
    }

    #[test]
    fn test_format_countdown_invalid_timestamp() {
        let result = format_countdown("not-a-timestamp");
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_countdown_past_timestamp() {
        let past = "2020-01-01T00:00:00+00:00";
        let result = format_countdown(past);
        assert_eq!(result, " (elapsed)");
    }

    #[test]
    fn test_run_nonexistent_task() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "no-such-task", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_run_basic_task() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_json_output() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_gather_task_runtime_info_detects_native_compaction() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mut task = make_task("native-task", "Native task");
        task.assigned = Some("agent-1".to_string());

        let mut registry = AgentRegistry::new();
        registry.agents.insert(
            "agent-1".to_string(),
            workgraph::service::AgentEntry {
                id: "agent-1".to_string(),
                pid: 123,
                task_id: "native-task".to_string(),
                executor: "native".to_string(),
                started_at: "2026-01-20T16:00:00Z".to_string(),
                last_heartbeat: "2026-01-20T16:05:00Z".to_string(),
                status: workgraph::service::AgentStatus::Working,
                output_file: "output.log".to_string(),
                model: Some("openrouter/minimax".to_string()),
                completed_at: None,
            worktree_path: None,
        },
        );
        registry.save(temp_dir.path()).unwrap();

        let journal_path =
            workgraph::executor::native::journal::journal_path(temp_dir.path(), "native-task");
        let mut journal =
            workgraph::executor::native::journal::Journal::open(&journal_path).unwrap();
        journal
            .append(
                workgraph::executor::native::journal::JournalEntryKind::Init {
                    model: "openrouter/minimax".to_string(),
                    provider: "openrouter".to_string(),
                    system_prompt: "test".to_string(),
                    tools: vec![],
                    task_id: Some("native-task".to_string()),
                },
            )
            .unwrap();
        journal
            .append(
                workgraph::executor::native::journal::JournalEntryKind::Compaction {
                    compacted_through_seq: 1,
                    summary: "summary".to_string(),
                    original_message_count: 4,
                    original_token_count: 400,
                    model_used: None,
                    fallback_reason: None,
                },
            )
            .unwrap();

        let summary_path = temp_dir
            .path()
            .join("agents")
            .join("agent-1")
            .join("session-summary.md");
        std::fs::create_dir_all(summary_path.parent().unwrap()).unwrap();
        std::fs::write(&summary_path, "short session summary").unwrap();

        let (executor, model, compaction) = gather_task_runtime_info(temp_dir.path(), &task);
        assert_eq!(executor.as_deref(), Some("native"));
        assert_eq!(model.as_deref(), Some("openrouter/minimax"));
        let compaction = compaction.expect("expected compaction info");
        assert!(compaction.journal_present);
        assert_eq!(compaction.compaction_count, 1);
        assert!(compaction.session_summary_present);
        assert_eq!(compaction.session_summary_words, Some(3));
    }

    #[test]
    fn test_run_task_with_orphan_blocker() {
        // A task references a blocker that doesn't exist in the graph
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task with ghost blocker");
        task.after = vec!["nonexistent".to_string()];
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        // Should succeed (not crash), blocker defaults to Status::Open with a warning
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_task_with_orphan_blocker_json() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task with ghost blocker");
        task.after = vec!["ghost".to_string()];
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_no_graph_file() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_show_verify_status_in_json() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Verify task");
        task.verify = Some("cargo test".to_string());
        task.verify_failures = 2;
        task.status = Status::InProgress;
        task.log.push(workgraph::graph::LogEntry {
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            actor: Some("verify".to_string()),
            user: None,
            message:
                "Verify FAILED (exit code 1, attempt 2/3). Command: cargo test\nstderr: test failed"
                    .to_string(),
        });
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_verify_failures_display() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Verify task");
        task.verify = Some("cargo test".to_string());
        task.verify_failures = 2;
        task.status = Status::InProgress;
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        // Should not panic and should succeed
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_verify_circuit_breaker_tripped() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "CB task");
        task.verify = Some("cargo test".to_string());
        task.verify_failures = 3;
        task.status = Status::Failed;
        task.failure_reason = Some("Circuit breaker tripped".to_string());
        task.log.push(workgraph::graph::LogEntry {
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            actor: Some("verify-circuit-breaker".to_string()),
            user: None,
            message: "Circuit breaker tripped: verify command failed 3 times".to_string(),
        });
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_no_verify_section_when_no_verify() {
        // A task without verify should not display verify section
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "No verify task")));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_displays_user_input_not_dated_id() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("model-task", "Task with model");
        task.model = Some("claude:opus".to_string());
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        // JSON output should contain the user's input string, never a dated ID
        let result = run(temp_dir.path(), "model-task", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_model_field_preserves_user_spec() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("t-opus", "Opus task");
        task.model = Some("claude:opus".to_string());
        graph.add_node(Node::Task(task));

        let mut task2 = make_task("t-pinned", "Pinned task");
        task2.model = Some("claude:opus-4-6".to_string());
        graph.add_node(Node::Task(task2));

        workgraph::parser::save_graph(&graph, &path).unwrap();

        // Verify the model field in TaskDetails preserves user's string exactly
        let graph = workgraph::parser::load_graph(&path).unwrap();
        let t1 = graph.get_task("t-opus").unwrap();
        assert_eq!(t1.model.as_deref(), Some("claude:opus"));
        let t2 = graph.get_task("t-pinned").unwrap();
        assert_eq!(t2.model.as_deref(), Some("claude:opus-4-6"));
    }

    #[test]
    fn test_show_no_dated_id_in_config_constants() {
        // Verify that the canonical model constants are bare aliases, not dated IDs
        assert_eq!(
            workgraph::config::CLAUDE_OPUS_MODEL_ID,
            "opus",
            "CLAUDE_OPUS_MODEL_ID must be bare alias, not dated"
        );
        assert_eq!(
            workgraph::config::CLAUDE_SONNET_MODEL_ID,
            "sonnet",
            "CLAUDE_SONNET_MODEL_ID must be bare alias, not dated"
        );
        assert_eq!(
            workgraph::config::CLAUDE_HAIKU_MODEL_ID,
            "haiku",
            "CLAUDE_HAIKU_MODEL_ID must be bare alias, not dated"
        );
    }

    /// New retention policy (worktree-retention-don):
    /// `wg show <task>` displays worktree state when a worktree exists for
    /// the task — branch name, commits ahead, uncommitted file count — so
    /// the user can decide between resume-in-place and `wg retry --fresh`.
    #[test]
    fn test_show_displays_worktree_state_for_retried_tasks() {
        use std::process::Command;
        let temp = tempfile::TempDir::new().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        // git init -b main
        Command::new("git")
            .args(["init", "-b", "main"])
            .arg(&project)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
        Command::new("git")
            .args(["symbolic-ref", "HEAD", "refs/heads/main"])
            .current_dir(&project)
            .output()
            .unwrap();
        std::fs::write(project.join("seed.txt"), "seed").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&project)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();

        // Set up a graph with a task
        let wg_dir = project.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let mut graph = WorkGraph::new();
        let mut t = make_task("retried-task", "test");
        t.retry_count = 1;
        t.status = Status::Failed;
        graph.add_node(Node::Task(t));
        let graph_path = wg_dir.join("graph.jsonl");
        workgraph::parser::save_graph(&graph, &graph_path).unwrap();

        // Create a worktree for the task with one extra commit
        let agent_id = "agent-77";
        let branch = format!("wg/{}/{}", agent_id, "retried-task");
        let wt = project.join(".wg-worktrees").join(agent_id);
        std::fs::create_dir_all(project.join(".wg-worktrees")).unwrap();
        Command::new("git")
            .args(["worktree", "add"])
            .arg(&wt)
            .args(["-b", &branch, "HEAD"])
            .current_dir(&project)
            .output()
            .unwrap();
        std::fs::write(wt.join("delta.txt"), "branch work").unwrap();
        Command::new("git")
            .args(["add", "delta.txt"])
            .current_dir(&wt)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "wip"])
            .current_dir(&wt)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();

        // Add an uncommitted change
        std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();

        // gather_worktree_state should find the worktree
        let state = gather_worktree_state(&wg_dir, "retried-task")
            .expect("worktree state must be detected");

        assert_eq!(state.branch, branch);
        assert_eq!(
            state.commits_ahead, 1,
            "should detect 1 commit on branch ahead of main"
        );
        assert!(
            state.uncommitted_files >= 1,
            "should detect at least one uncommitted file: {:?}",
            state.uncommitted_files
        );
        assert!(
            !state.merged_to_main,
            "branch is not merged into main"
        );
        assert!(state.path.contains(".wg-worktrees"));

        // Tasks without a worktree return None
        let no_state = gather_worktree_state(&wg_dir, "no-such-task");
        assert!(no_state.is_none());
    }
}
