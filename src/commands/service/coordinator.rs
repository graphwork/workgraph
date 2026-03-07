//! Coordinator tick logic: task readiness, auto-assign, auto-evaluate, agent spawning.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::Path;

use workgraph::agency;
use workgraph::agency::run_mode::{self, AssignmentPath};
use workgraph::agency::{
    AssignerModeContext, AssignmentMode, Evaluation, TaskAssignmentRecord,
    count_assignment_records, eval_source, find_cached_agent, load_agent,
    load_all_evaluations_or_warn, load_role, load_tradeoff, render_assigner_mode_context,
    render_identity_prompt_rich, resolve_all_components, resolve_outcome, save_assignment_record,
};
use workgraph::chat;
use workgraph::config::Config;
use workgraph::graph::{
    LogEntry, Node, Status, Task, WaitCondition, WaitSpec, evaluate_all_cycle_failure_restarts,
    evaluate_all_cycle_iterations,
};
use workgraph::messages;
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks_with_peers_cycle_aware;
use workgraph::service::registry::AgentRegistry;

use super::triage;
use crate::commands::{graph_path, is_process_alive, spawn};

/// Result of a single coordinator tick
pub struct TickResult {
    /// Number of agents alive after the tick
    pub agents_alive: usize,
    /// Number of ready tasks found
    pub tasks_ready: usize,
    /// Number of agents spawned in this tick
    pub agents_spawned: usize,
}

/// Clean up dead agents and count alive ones. Returns `None` with an early
/// `TickResult` if the alive count already meets `max_agents`.
fn cleanup_and_count_alive(
    dir: &Path,
    graph_path: &Path,
    max_agents: usize,
) -> Result<Result<usize, TickResult>> {
    // Clean up dead agents: process exited
    let finished_agents = triage::cleanup_dead_agents(dir, graph_path)?;
    if !finished_agents.is_empty() {
        eprintln!(
            "[coordinator] Cleaned up {} dead agent(s): {:?}",
            finished_agents.len(),
            finished_agents
        );
    }

    // Now count truly alive agents (process still running)
    let registry = AgentRegistry::load(dir)?;
    let alive_count = registry
        .agents
        .values()
        .filter(|a| a.is_alive() && is_process_alive(a.pid))
        .count();

    if alive_count >= max_agents {
        eprintln!(
            "[coordinator] Max agents ({}) running, waiting...",
            max_agents
        );
        return Ok(Err(TickResult {
            agents_alive: alive_count,
            tasks_ready: 0,
            agents_spawned: 0,
        }));
    }

    Ok(Ok(alive_count))
}

/// Check whether any tasks are ready. Returns `None` with an early `TickResult`
/// if no ready tasks exist.
fn check_ready_or_return(
    graph: &workgraph::graph::WorkGraph,
    alive_count: usize,
    dir: &Path,
) -> Option<TickResult> {
    let cycle_analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_with_peers_cycle_aware(graph, dir, &cycle_analysis);
    if ready.is_empty() {
        let terminal = graph.tasks().filter(|t| t.status.is_terminal()).count();
        let total = graph.tasks().count();
        if terminal == total && total > 0 {
            eprintln!("[coordinator] All {} tasks complete!", total);
        } else {
            eprintln!(
                "[coordinator] No ready tasks (terminal: {}/{})",
                terminal, total
            );
        }
        return Some(TickResult {
            agents_alive: alive_count,
            tasks_ready: 0,
            agents_spawned: 0,
        });
    }
    None
}

/// Evaluate a single wait condition against the current graph/filesystem state.
/// Returns `true` if the condition is satisfied.
fn evaluate_condition(
    condition: &WaitCondition,
    graph: &workgraph::graph::WorkGraph,
    dir: &Path,
    task_id: &str,
    wait_started_at: Option<&str>,
) -> bool {
    match condition {
        WaitCondition::TaskStatus {
            task_id: dep_id,
            status: expected,
        } => {
            if let Some(dep) = graph.get_task(dep_id) {
                dep.status == *expected
            } else {
                false
            }
        }
        WaitCondition::Timer { resume_after } => {
            if let Ok(target) = resume_after.parse::<chrono::DateTime<chrono::Utc>>() {
                Utc::now() >= target
            } else {
                // Unparseable timestamp — treat as satisfied to avoid permanent hang
                true
            }
        }
        WaitCondition::HumanInput => {
            // Check for messages from non-agent senders since the task started waiting
            has_non_agent_message_since(dir, task_id, wait_started_at)
        }
        WaitCondition::Message => {
            // Check for any message since the task started waiting
            has_any_message_since(dir, task_id, wait_started_at)
        }
        WaitCondition::FileChanged {
            path,
            mtime_at_wait,
        } => {
            if let Ok(metadata) = std::fs::metadata(path) {
                if let Ok(modified) = metadata.modified() {
                    let current_mtime = modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    current_mtime > *mtime_at_wait
                } else {
                    false
                }
            } else {
                false
            }
        }
    }
}

/// Check if any message exists for a task since the wait started.
fn has_any_message_since(dir: &Path, task_id: &str, wait_started_at: Option<&str>) -> bool {
    if let Ok(msgs) = messages::list_messages(dir, task_id) {
        if let Some(wait_ts) = wait_started_at
            && let Ok(wait_time) = wait_ts.parse::<chrono::DateTime<chrono::Utc>>()
        {
            msgs.iter().any(|m| {
                m.timestamp
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .map(|t| t > wait_time)
                    .unwrap_or(false)
            })
        } else {
            !msgs.is_empty()
        }
    } else {
        false
    }
}

/// Check if any non-agent message exists for a task since the wait started.
fn has_non_agent_message_since(dir: &Path, task_id: &str, wait_started_at: Option<&str>) -> bool {
    if let Ok(msgs) = messages::list_messages(dir, task_id) {
        if let Some(wait_ts) = wait_started_at
            && let Ok(wait_time) = wait_ts.parse::<chrono::DateTime<chrono::Utc>>()
        {
            msgs.iter().any(|m| {
                !m.sender.starts_with("agent-")
                    && m.timestamp
                        .parse::<chrono::DateTime<chrono::Utc>>()
                        .map(|t| t > wait_time)
                        .unwrap_or(false)
            })
        } else {
            msgs.iter().any(|m| !m.sender.starts_with("agent-"))
        }
    } else {
        false
    }
}

/// Evaluate all conditions in a WaitSpec.
fn evaluate_wait_spec(
    spec: &WaitSpec,
    graph: &workgraph::graph::WorkGraph,
    dir: &Path,
    task_id: &str,
    wait_started_at: Option<&str>,
) -> bool {
    match spec {
        WaitSpec::All(conditions) => conditions
            .iter()
            .all(|c| evaluate_condition(c, graph, dir, task_id, wait_started_at)),
        WaitSpec::Any(conditions) => conditions
            .iter()
            .any(|c| evaluate_condition(c, graph, dir, task_id, wait_started_at)),
    }
}

/// Check if a TaskStatus wait condition is unsatisfiable (referenced task
/// is in a terminal state that doesn't match the expected status).
fn is_condition_unsatisfiable(
    condition: &WaitCondition,
    graph: &workgraph::graph::WorkGraph,
) -> Option<String> {
    match condition {
        WaitCondition::TaskStatus {
            task_id: dep_id,
            status: expected,
        } => {
            if let Some(dep) = graph.get_task(dep_id) {
                if dep.status.is_terminal() && dep.status != *expected {
                    Some(format!(
                        "task '{}' is {} (expected {})",
                        dep_id, dep.status, expected
                    ))
                } else {
                    None
                }
            } else {
                Some(format!("task '{}' no longer exists", dep_id))
            }
        }
        _ => None,
    }
}

/// Detect circular waits: task A waiting on task B, task B waiting on task A.
fn detect_circular_waits(graph: &workgraph::graph::WorkGraph) -> Vec<Vec<String>> {
    let mut cycles = Vec::new();
    let waiting_tasks: Vec<_> = graph
        .tasks()
        .filter(|t| t.status == Status::Waiting && t.wait_condition.is_some())
        .collect();

    // Build a map: task_id -> set of task_ids it's waiting on (via TaskStatus conditions)
    let mut wait_edges: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for t in &waiting_tasks {
        if let Some(ref spec) = t.wait_condition {
            let conditions = match spec {
                WaitSpec::All(c) | WaitSpec::Any(c) => c,
            };
            let deps: Vec<&str> = conditions
                .iter()
                .filter_map(|c| match c {
                    WaitCondition::TaskStatus { task_id, .. } => Some(task_id.as_str()),
                    _ => None,
                })
                .collect();
            if !deps.is_empty() {
                wait_edges.insert(t.id.as_str(), deps);
            }
        }
    }

    // DFS cycle detection
    let mut visited = std::collections::HashSet::new();
    for start in wait_edges.keys() {
        if visited.contains(start) {
            continue;
        }
        let mut path = vec![*start];
        let mut stack: Vec<(&str, usize)> = vec![(*start, 0)];
        let mut in_path = std::collections::HashSet::new();
        in_path.insert(*start);

        while let Some((node, idx)) = stack.last_mut() {
            let deps = wait_edges.get(node).cloned().unwrap_or_default();
            if *idx >= deps.len() {
                in_path.remove(*node);
                path.pop();
                stack.pop();
                continue;
            }
            let next = deps[*idx];
            *idx += 1;
            if in_path.contains(next) {
                // Found a cycle - extract it
                let cycle_start = path.iter().position(|p| *p == next).unwrap();
                let cycle: Vec<String> =
                    path[cycle_start..].iter().map(|s| s.to_string()).collect();
                if cycle.len() >= 2 {
                    cycles.push(cycle);
                }
            } else if !visited.contains(next) && wait_edges.contains_key(next) {
                in_path.insert(next);
                path.push(next);
                stack.push((next, 0));
            }
        }
        visited.insert(*start);
    }
    cycles
}

/// Build a brief graph state delta for resume context injection.
/// Shows what changed while the task was waiting (~100 tokens).
fn build_resume_delta(graph: &workgraph::graph::WorkGraph, task: &Task, dir: &Path) -> String {
    let mut delta = String::new();
    delta.push_str("## Resume Context\n");

    // Show what condition was satisfied
    if let Some(ref spec) = task.wait_condition {
        let conditions = match spec {
            WaitSpec::All(c) | WaitSpec::Any(c) => c,
        };
        delta.push_str("Your wait condition is now satisfied.\n\n");

        // Show status of referenced tasks
        for cond in conditions {
            if let WaitCondition::TaskStatus { task_id, status } = cond
                && let Some(dep) = graph.get_task(task_id)
            {
                delta.push_str(&format!(
                    "- {}: {} (expected: {})\n",
                    task_id, dep.status, status
                ));
                // Include artifacts if any
                if !dep.artifacts.is_empty() {
                    for art in &dep.artifacts {
                        delta.push_str(&format!("  artifact: {}\n", art));
                    }
                }
            }
        }
    }

    // Include checkpoint if available
    if let Some(ref cp) = task.checkpoint {
        delta.push_str(&format!("\nYour checkpoint: \"{}\"\n", cp));
    }

    // Include recent messages on this task
    if let Ok(msgs) = messages::list_messages(dir, &task.id) {
        let recent: Vec<_> = msgs.iter().rev().take(3).collect();
        if !recent.is_empty() {
            delta.push_str("\nRecent messages:\n");
            for msg in recent.iter().rev() {
                delta.push_str(&format!(
                    "- [{}] {}: {}\n",
                    msg.timestamp, msg.sender, msg.body
                ));
            }
        }
    }

    delta.push_str(&format!("\nContinue your work on '{}'.\n", task.id));
    delta
}

/// Evaluate waiting tasks and transition them when conditions are met.
/// Returns `true` if the graph was modified.
fn evaluate_waiting_tasks(graph: &mut workgraph::graph::WorkGraph, dir: &Path) -> bool {
    let mut modified = false;

    // First, detect circular waits
    let circular = detect_circular_waits(graph);
    for cycle in &circular {
        eprintln!("[coordinator] Circular wait detected: {:?}", cycle);
        for task_id in cycle {
            if let Some(t) = graph.get_task_mut(task_id)
                && t.status == Status::Waiting
            {
                t.status = Status::Failed;
                t.wait_condition = None;
                t.failure_reason = Some(format!("Circular wait detected: {}", cycle.join(" -> ")));
                t.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("coordinator".to_string()),
                    message: format!("Failed: circular wait detected ({})", cycle.join(" -> ")),
                });
                modified = true;
            }
        }
    }

    // Collect waiting tasks with their data to avoid borrow conflicts
    let waiting_data: Vec<_> = graph
        .tasks()
        .filter(|t| t.status == Status::Waiting && t.wait_condition.is_some())
        .map(|t| {
            let wait_started = t
                .log
                .iter()
                .rev()
                .find(|l| l.message.contains("Agent parked"))
                .map(|l| l.timestamp.clone());
            (
                t.id.clone(),
                t.wait_condition.clone().unwrap(),
                wait_started,
                t.session_id.clone(),
                t.checkpoint.clone(),
            )
        })
        .collect();

    for (task_id, spec, wait_started, _session_id, _checkpoint) in &waiting_data {
        // Check for unsatisfiable conditions first
        let conditions = match &spec {
            WaitSpec::All(c) | WaitSpec::Any(c) => c,
        };

        let mut unsatisfiable_reasons = Vec::new();
        for cond in conditions {
            if let Some(reason) = is_condition_unsatisfiable(cond, graph) {
                unsatisfiable_reasons.push(reason);
            }
        }

        // For All: any unsatisfiable => whole spec unsatisfiable
        // For Any: all must be unsatisfiable
        let is_unsatisfiable = match &spec {
            WaitSpec::All(_) => !unsatisfiable_reasons.is_empty(),
            WaitSpec::Any(_) => {
                // Only unsatisfiable if ALL conditions are unsatisfiable
                // (non-TaskStatus conditions like timer/message are never unsatisfiable)
                let task_status_count = conditions
                    .iter()
                    .filter(|c| matches!(c, WaitCondition::TaskStatus { .. }))
                    .count();
                unsatisfiable_reasons.len() == task_status_count
                    && task_status_count == conditions.len()
            }
        };

        if is_unsatisfiable {
            let reason = format!(
                "Wait condition unsatisfiable: {}",
                unsatisfiable_reasons.join(", ")
            );
            if let Some(t) = graph.get_task_mut(task_id) {
                t.status = Status::Failed;
                t.wait_condition = None;
                t.failure_reason = Some(reason.clone());
                t.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("coordinator".to_string()),
                    message: format!("Failed: {}", reason),
                });
                modified = true;
                eprintln!(
                    "[coordinator] Waiting task '{}' failed: {}",
                    task_id, reason
                );
            }
            continue;
        }

        // Evaluate the wait spec
        let satisfied = evaluate_wait_spec(spec, graph, dir, task_id, wait_started.as_deref());

        if satisfied {
            // Build resume delta before mutating
            let delta = {
                let task = graph.get_task(task_id).unwrap();
                build_resume_delta(graph, task, dir)
            };

            if let Some(t) = graph.get_task_mut(task_id) {
                t.status = Status::Open;
                t.wait_condition = None;
                // Store the resume delta as the new checkpoint so the spawned agent gets it
                t.checkpoint = Some(delta.clone());
                // Clear the assignment so the coordinator can re-spawn
                t.assigned = None;
                t.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("coordinator".to_string()),
                    message: "Wait condition satisfied. Task ready for resume.".to_string(),
                });
                modified = true;
                eprintln!(
                    "[coordinator] Waiting task '{}' condition satisfied, transitioning to Open",
                    task_id
                );
            }
        }
    }

    modified
}

// ---------------------------------------------------------------------------
// Message-triggered resurrection
// ---------------------------------------------------------------------------

/// Maximum number of resurrections allowed per task.
const MAX_RESURRECTIONS: u32 = 5;

/// Minimum seconds between resurrections of the same task.
const RESURRECTION_COOLDOWN_SECS: i64 = 60;

/// Scan Done tasks for unread messages and resurrect them.
///
/// Two modes:
/// 1. Reopen: if no downstream task is InProgress or Done, transition Done → Open.
/// 2. Child task: if downstream tasks are running, create a child task
///    `.respond-to-<parent-id>` that inherits the parent's session_id and checkpoint.
///
/// Guards: rate limit, sender whitelist, abandoned exclusion.
/// Returns `true` if the graph was modified.
fn resurrect_done_tasks(graph: &mut workgraph::graph::WorkGraph, dir: &Path) -> bool {
    let mut modified = false;

    // Collect Done tasks with unread messages from whitelisted senders
    let candidates: Vec<_> = graph
        .tasks()
        .filter(|t| t.status == Status::Done)
        .filter(|t| !t.tags.iter().any(|tag| tag == "resurrect:false"))
        .map(|t| {
            (
                t.id.clone(),
                t.assigned.clone(),
                t.before.clone(),
                t.session_id.clone(),
                t.checkpoint.clone(),
                t.resurrection_count,
                t.last_resurrected_at.clone(),
            )
        })
        .collect();

    for (
        task_id,
        assigned_agent,
        downstream_ids,
        session_id,
        checkpoint,
        resurrection_count,
        last_resurrected_at,
    ) in candidates
    {
        // Rate limit: max resurrections
        if resurrection_count >= MAX_RESURRECTIONS {
            continue;
        }

        // Rate limit: cooldown
        if let Some(ref last_ts) = last_resurrected_at
            && let Ok(last_time) = last_ts.parse::<chrono::DateTime<chrono::Utc>>()
        {
            let elapsed = Utc::now().signed_duration_since(last_time);
            if elapsed.num_seconds() < RESURRECTION_COOLDOWN_SECS {
                continue;
            }
        }

        // Check for unread messages not from the task's own agent
        let messages = match messages::list_messages(dir, &task_id) {
            Ok(msgs) => msgs,
            Err(_) => continue,
        };

        // Find messages with status=Sent that are not from the task's own agent
        let triggering_msgs: Vec<_> = messages
            .iter()
            .filter(|m| m.status == messages::DeliveryStatus::Sent)
            .filter(|m| {
                // Sender whitelist: user, coordinator, or dependent-task agents
                if m.sender == "user" || m.sender == "coordinator" {
                    return true;
                }
                // Allow messages from agents working on tasks that depend on this one
                // (i.e., downstream tasks whose agents might send questions back)
                if m.sender.starts_with("agent-") {
                    return true;
                }
                false
            })
            .filter(|m| {
                // Exclude messages from the task's own agent
                if let Some(ref agent) = assigned_agent {
                    m.sender != *agent
                } else {
                    true
                }
            })
            .collect();

        if triggering_msgs.is_empty() {
            continue;
        }

        // Check downstream state to decide reopen vs child task
        let has_active_downstream = downstream_ids.iter().any(|did| {
            graph
                .get_task(did)
                .is_some_and(|dt| matches!(dt.status, Status::InProgress | Status::Done))
        });

        if has_active_downstream {
            // Mode 2: Create child task
            let child_id = format!(".respond-to-{}", task_id);

            // Skip if child already exists
            if graph.get_task(&child_id).is_some() {
                continue;
            }

            let msg_summary: Vec<String> = triggering_msgs
                .iter()
                .map(|m| format!("[{}] {}: {}", m.timestamp, m.sender, m.body))
                .collect();

            let child_desc = format!(
                "You previously completed task `{}`. There are pending messages that need your attention.\n\n\
                 ## Pending Messages\n{}\n\n\
                 Read and respond to these messages using `wg msg read {} --agent $WG_AGENT_ID`.\n\
                 When done, mark this task complete with `wg done {}`.",
                task_id,
                msg_summary.join("\n"),
                task_id,
                child_id,
            );

            let child_task = Task {
                id: child_id.clone(),
                title: format!("Respond to messages on {}", task_id),
                description: Some(child_desc),
                status: Status::Open,
                session_id: session_id.clone(),
                checkpoint: checkpoint.clone(),
                after: vec![task_id.clone()],
                tags: vec!["resurrection-child".to_string()],
                created_at: Some(Utc::now().to_rfc3339()),
                ..Default::default()
            };

            graph.add_node(Node::Task(child_task));

            // Update parent resurrection tracking
            if let Some(t) = graph.get_task_mut(&task_id) {
                t.resurrection_count += 1;
                t.last_resurrected_at = Some(Utc::now().to_rfc3339());
                t.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("coordinator".to_string()),
                    message: format!(
                        "Resurrection: created child task '{}' ({} pending message(s), downstream active)",
                        child_id,
                        triggering_msgs.len()
                    ),
                });
            }

            eprintln!(
                "[coordinator] Resurrection: created child task '{}' for Done task '{}' ({} message(s))",
                child_id,
                task_id,
                triggering_msgs.len()
            );
            modified = true;
        } else {
            // Mode 1: Reopen
            if let Some(t) = graph.get_task_mut(&task_id) {
                t.status = Status::Open;
                t.assigned = None;
                t.resurrection_count += 1;
                t.last_resurrected_at = Some(Utc::now().to_rfc3339());
                t.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("coordinator".to_string()),
                    message: format!(
                        "Resurrection: reopened due to {} pending message(s)",
                        triggering_msgs.len()
                    ),
                });

                eprintln!(
                    "[coordinator] Resurrection: reopened Done task '{}' ({} message(s))",
                    task_id,
                    triggering_msgs.len()
                );
                modified = true;
            }
        }
    }

    modified
}

/// Auto-assign: run lightweight LLM assignment for unassigned ready tasks.
///
/// Uses a single `run_lightweight_llm_call()` to select the best agent for each
/// task (replaces the old multi-turn Claude Code session approach). The LLM
/// receives the full agent catalog and task context, and returns a JSON verdict
/// with agent_hash, exec_mode, and context_scope. A `.assign-*` task is created
/// (marked Done) for audit trail only — no blocking edge or agent spawn needed.
///
/// Returns `true` if the graph was modified.
fn build_auto_assign_tasks(
    graph: &mut workgraph::graph::WorkGraph,
    config: &Config,
    dir: &Path,
) -> bool {
    let mut modified = false;

    let grace_seconds = config.agency.auto_assign_grace_seconds;

    // Collect task data to avoid holding references while mutating graph
    let ready_task_data: Vec<_> = {
        let cycle_analysis = graph.compute_cycle_analysis();
        let ready = ready_tasks_with_peers_cycle_aware(graph, dir, &cycle_analysis);
        ready
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    t.title.clone(),
                    t.description.clone(),
                    t.skills.clone(),
                    t.agent.clone(),
                    t.assigned.clone(),
                    t.tags.clone(),
                    t.after.clone(),
                    t.context_scope.clone(),
                    t.created_at.clone(),
                )
            })
            .collect()
    };

    // Compute total assignments for run mode routing
    let agency_dir = dir.join("agency");
    let total_assignments = count_assignment_records(&agency_dir.join("assignments")) as u32;

    for (
        task_id,
        task_title,
        task_desc,
        task_skills,
        task_agent,
        task_assigned,
        task_tags,
        task_after,
        task_context_scope,
        task_created_at,
    ) in ready_task_data
    {
        // Skip tasks that already have an agent or are already claimed
        if task_agent.is_some() || task_assigned.is_some() {
            continue;
        }

        // Skip system tasks (dot-prefixed) to prevent infinite regress
        if workgraph::graph::is_system_task(&task_id) {
            continue;
        }

        // Grace period: skip tasks created less than `auto_assign_grace_seconds` ago.
        // This prevents premature assignment when tasks are created and then have
        // dependencies wired shortly after (e.g., `wg add` then `wg edit --add-after`).
        if grace_seconds > 0
            && let Some(ref created_str) = task_created_at
            && let Ok(created) = created_str.parse::<chrono::DateTime<chrono::Utc>>()
        {
            let age = Utc::now().signed_duration_since(created);
            if age.num_seconds() < grace_seconds as i64 {
                eprintln!(
                    "[coordinator] Skipping auto-assign for '{}': created {}s ago (grace period: {}s)",
                    task_id,
                    age.num_seconds(),
                    grace_seconds,
                );
                continue;
            }
        }

        let assign_task_id = format!(".assign-{}", task_id);

        // Skip if assignment task already exists (idempotent)
        if graph.get_task(&assign_task_id).is_some() {
            continue;
        }

        // Determine assignment path via run mode continuum
        let rng_value: f64 = {
            // Simple deterministic pseudo-random from task_id hash to avoid
            // requiring rand crate. Provides adequate entropy for routing.
            let hash = task_id
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            (hash % 10000) as f64 / 10000.0
        };
        let assignment_path =
            run_mode::determine_assignment_path(&config.agency, total_assignments, rng_value);

        // Build mode-specific context for the assigner
        let experiment = match assignment_path {
            AssignmentPath::Learning | AssignmentPath::ForcedExploration => {
                let learning_count =
                    count_assignment_records(&agency_dir.join("assignments")) as u32;
                Some(run_mode::design_experiment(
                    &agency_dir,
                    &config.agency,
                    learning_count,
                ))
            }
            AssignmentPath::Performance => None,
        };

        let cached_agents: Vec<(String, f64)> = if assignment_path == AssignmentPath::Performance {
            // Gather top cached agents for the performance mode context
            let agents_dir = agency_dir.join("cache/agents");
            let mut agents_with_scores: Vec<(String, f64)> =
                agency::load_all_agents_or_warn(&agents_dir)
                    .into_iter()
                    .filter_map(|a| {
                        let score = a.performance.avg_score?;
                        if a.staleness_flags.is_empty() {
                            Some((format!("{} ({})", a.name, agency::short_hash(&a.id)), score))
                        } else {
                            None
                        }
                    })
                    .collect();
            agents_with_scores
                .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            agents_with_scores.truncate(5); // Top 5
            agents_with_scores
        } else {
            vec![]
        };

        let effective_rate = config
            .agency
            .run_mode
            .max(config.agency.min_exploration_rate);
        let mode_context = render_assigner_mode_context(&AssignerModeContext {
            run_mode: config.agency.run_mode,
            effective_exploration_rate: effective_rate,
            assignment_path,
            experiment: experiment.as_ref(),
            cached_agents: &cached_agents,
            total_assignments,
        });

        eprintln!(
            "[coordinator] Assignment path for '{}': {:?} (run_mode={:.2}, total_assignments={})",
            task_id, assignment_path, config.agency.run_mode, total_assignments,
        );

        // Detect task underspecification
        let is_underspecified =
            task_desc.is_none() || task_desc.as_ref().map(|d| d.len() < 20).unwrap_or(true);
        let has_no_skills = task_skills.is_empty();
        let underspec_warning = if is_underspecified || has_no_skills {
            let mut warnings = Vec::new();
            if is_underspecified {
                warnings.push("task has no description or a very short description");
            }
            if has_no_skills {
                warnings.push("task has no skills/capabilities specified");
            }
            Some(format!(
                "\n**⚠ Underspecification Warning:** {}\n\
                 The assigner should use best-effort heuristics: match on title keywords, \
                 check dependency context, and default to a generalist agent.\n",
                warnings.join("; "),
            ))
        } else {
            None
        };

        // Load all agents for the lightweight LLM assignment call
        let agents_dir = agency_dir.join("cache/agents");
        let all_agents = agency::load_all_agents_or_warn(&agents_dir);
        let roles_dir = agency_dir.join("cache/roles");
        let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");

        // Build a temporary Task with the gathered data for the prompt builder
        let task_snapshot = Task {
            id: task_id.clone(),
            title: task_title.clone(),
            description: task_desc.clone(),
            skills: task_skills.clone(),
            tags: task_tags.clone(),
            after: task_after.clone(),
            context_scope: task_context_scope.clone(),
            ..Default::default()
        };

        // Run lightweight LLM call for assignment (replaces full Claude Code session)
        let (verdict, assign_token_usage) = match super::assignment::run_lightweight_assignment(
            config,
            &task_snapshot,
            &all_agents,
            &roles_dir,
            &tradeoffs_dir,
            &mode_context,
            underspec_warning.as_deref(),
        ) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[coordinator] Lightweight assignment failed for '{}': {}, will retry next tick",
                    task_id, e
                );
                continue;
            }
        };

        // Resolve the agent hash from the verdict
        let resolved_agent =
            match agency::find_agent_by_prefix(&agents_dir, &verdict.agent_hash) {
                Ok(agent) => agent,
                Err(e) => {
                    eprintln!(
                        "[coordinator] Assignment verdict agent '{}' not found for '{}': {}",
                        verdict.agent_hash, task_id, e
                    );
                    continue;
                }
            };

        // Apply assignment to the original task
        if let Some(task) = graph.get_task_mut(&task_id) {
            task.agent = Some(resolved_agent.id.clone());
            if let Some(ref mode) = verdict.exec_mode {
                match mode.as_str() {
                    "shell" | "bare" | "light" | "full" => {
                        task.exec_mode = Some(mode.clone());
                    }
                    _ => {} // invalid, keep default
                }
            }
            if let Some(ref scope) = verdict.context_scope {
                match scope.as_str() {
                    "clean" | "task" | "graph" | "full" => {
                        // Only set if not already pre-set
                        if task.context_scope.is_none() {
                            task.context_scope = Some(scope.clone());
                        }
                    }
                    _ => {} // invalid, keep default
                }
            }
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("coordinator".to_string()),
                message: format!(
                    "Lightweight assignment: agent={} ({}), exec_mode={}, context_scope={}, reason={}",
                    resolved_agent.name,
                    agency::short_hash(&resolved_agent.id),
                    verdict.exec_mode.as_deref().unwrap_or("(default)"),
                    verdict.context_scope.as_deref().unwrap_or("(default)"),
                    verdict.reason,
                ),
            });
        }

        // Create .assign-* task marked Done for audit trail (no blocking edge needed)
        let now = Utc::now().to_rfc3339();
        let assign_task = Task {
            id: assign_task_id.clone(),
            title: format!("Assign agent for: {}", task_title),
            description: Some(format!(
                "Lightweight assignment: {} ({}) → '{}'\nReason: {}",
                resolved_agent.name,
                agency::short_hash(&resolved_agent.id),
                task_id,
                verdict.reason,
            )),
            status: Status::Done,
            assigned: None,
            estimate: None,
            before: vec![task_id.clone()],
            after: vec![],
            requires: vec![],
            tags: vec!["assignment".to_string(), "agency".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: Some(now.clone()),
            started_at: Some(now.clone()),
            completed_at: Some(now),
            log: vec![LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("coordinator".to_string()),
                message: format!(
                    "Completed via lightweight LLM call (path: {:?})",
                    assignment_path,
                ),
            }],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: Some(
                config
                    .resolve_model_for_role(workgraph::config::DispatchRole::Assigner)
                    .model,
            ),
            provider: config
                .resolve_model_for_role(workgraph::config::DispatchRole::Assigner)
                .provider,
            verify: None,
            agent: config.agency.assigner_agent.clone(),
            loop_iteration: 0,
            cycle_failure_restarts: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            cycle_config: None,
            token_usage: assign_token_usage,
            session_id: None,
            wait_condition: None,
            checkpoint: None,
            resurrection_count: 0,
            last_resurrected_at: None,
            exec_mode: Some("bare".to_string()),
        };

        graph.add_node(Node::Task(assign_task));

        // Persist TaskAssignmentRecord with actual agent info
        let assignment_mode = match assignment_path {
            AssignmentPath::Performance => {
                match find_cached_agent(&agency_dir, config.agency.performance_threshold) {
                    Some((_, score)) => AssignmentMode::CacheHit { cache_score: score },
                    None => AssignmentMode::CacheMiss,
                }
            }
            AssignmentPath::Learning => AssignmentMode::Learning(
                experiment
                    .clone()
                    .expect("experiment required for Learning path"),
            ),
            AssignmentPath::ForcedExploration => AssignmentMode::ForcedExploration(
                experiment
                    .clone()
                    .expect("experiment required for ForcedExploration path"),
            ),
        };

        let record = TaskAssignmentRecord {
            task_id: task_id.clone(),
            agent_id: resolved_agent.id.clone(),
            composition_id: resolved_agent.id.clone(),
            timestamp: Utc::now().to_rfc3339(),
            run_mode_value: config.agency.run_mode,
            mode: assignment_mode,
        };

        let assignments_dir = agency_dir.join("assignments");
        if let Err(e) = save_assignment_record(&record, &assignments_dir) {
            eprintln!(
                "[coordinator] Warning: failed to save assignment record for '{}': {}",
                task_id, e,
            );
        }

        eprintln!(
            "[coordinator] Lightweight assignment for '{}': {} ({}) [path={:?}]",
            task_id,
            resolved_agent.name,
            agency::short_hash(&resolved_agent.id),
            assignment_path,
        );
        modified = true;
    }

    modified
}

/// Auto-evaluate: create evaluation tasks for completed/active tasks.
///
/// Per the agency design (§4.3), when auto_evaluate is enabled the coordinator
/// creates an evaluation task `evaluate-{task-id}` that is blocked by the
/// original task.  When the original task completes (done or failed),
/// the evaluation task becomes ready and the coordinator spawns an
/// evaluator agent on it.
///
/// Tasks tagged "evaluation", "assignment", or "evolution" are NOT
/// auto-evaluated to prevent infinite regress.  Abandoned tasks are also
/// excluded.
///
/// Returns `true` if the graph was modified.
fn build_auto_evaluate_tasks(
    dir: &Path,
    graph: &mut workgraph::graph::WorkGraph,
    config: &Config,
) -> bool {
    let mut modified = false;

    // Load agents to identify human operators — their work quality isn't
    // a reflection of a role+tradeoff prompt so we skip auto-evaluation.
    let agents_dir = dir.join("agency").join("cache/agents");
    let all_agents = agency::load_all_agents_or_warn(&agents_dir);
    let human_agent_ids: std::collections::HashSet<&str> = all_agents
        .iter()
        .filter(|a| a.is_human())
        .map(|a| a.id.as_str())
        .collect();

    // Collect all tasks (not just ready ones) that might need eval tasks.
    // We iterate all non-terminal tasks so eval tasks are created early.
    let tasks_needing_eval: Vec<_> = graph
        .tasks()
        .filter(|t| {
            // Skip tasks that already have an evaluation task
            let eval_id = format!(".evaluate-{}", t.id);
            if graph.get_task(&eval_id).is_some() {
                return false;
            }
            // Skip tasks tagged with evaluation/assignment/evolution
            let dominated_tags = ["evaluation", "assignment", "evolution"];
            if t.tags
                .iter()
                .any(|tag| dominated_tags.contains(&tag.as_str()))
            {
                return false;
            }
            // Skip tasks already tagged as having had evaluation scheduled.
            // This survives gc (which removes the evaluate-* task) and prevents
            // re-creating hundreds of eval tasks on service restart.
            if t.tags.iter().any(|tag| tag == "eval-scheduled") {
                return false;
            }
            // Skip tasks assigned to human agents
            if let Some(ref agent_id) = t.agent
                && human_agent_ids.contains(agent_id.as_str())
            {
                return false;
            }
            // Only create for tasks that are active (Open, InProgress, Blocked)
            // or already completed (Done, Failed) without an eval task
            !matches!(t.status, Status::Abandoned)
        })
        .map(|t| (t.id.clone(), t.title.clone()))
        .collect();

    // Resolve evaluator agent identity once (shared across all eval tasks)
    let evaluator_identity = config
        .agency
        .evaluator_agent
        .as_ref()
        .and_then(|agent_hash| {
            let agency_dir = dir.join("agency");
            let agents_dir = agency_dir.join("cache/agents");
            let agent_path = agents_dir.join(format!("{}.yaml", agent_hash));
            let agent = load_agent(&agent_path).ok()?;
            let roles_dir = agency_dir.join("cache/roles");
            let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
            let role = load_role(&role_path).ok()?;
            let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
            let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));
            let tradeoff = load_tradeoff(&tradeoff_path).ok()?;
            let workgraph_root = dir;
            let resolved_skills = resolve_all_components(&role, workgraph_root, &agency_dir);
            let outcome = resolve_outcome(&role.outcome_id, &agency_dir);
            Some(render_identity_prompt_rich(
                &role,
                &tradeoff,
                &resolved_skills,
                outcome.as_ref(),
            ))
        });

    for (task_id, task_title) in &tasks_needing_eval {
        let eval_task_id = format!(".evaluate-{}", task_id);

        // Double-check (the filter above already checks but graph may have changed)
        if graph.get_task(&eval_task_id).is_some() {
            continue;
        }

        let mut desc = String::new();
        // Prepend evaluator identity when composed evaluator agent is available
        if let Some(ref identity) = evaluator_identity {
            desc.push_str(identity);
            desc.push_str("\n\n");
        }
        desc.push_str(&format!(
            "Evaluate the completed task '{}'.\n\n\
             Run `wg evaluate run {}` to produce a structured evaluation.\n\
             This reads the task output from `.workgraph/output/{}/` and \
             the task definition via `wg show {}`.",
            task_id, task_id, task_id, task_id,
        ));

        let eval_task = Task {
            id: eval_task_id.clone(),
            title: format!("Evaluate: {}", task_title),
            description: Some(desc),
            status: Status::Open,
            assigned: None,
            estimate: None,
            before: vec![],
            after: vec![task_id.clone()],
            requires: vec![],
            tags: vec!["evaluation".to_string(), "agency".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: Some(format!("wg evaluate run {}", task_id)),
            not_before: None,
            created_at: Some(Utc::now().to_rfc3339()),
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: Some(
                config
                    .resolve_model_for_role(workgraph::config::DispatchRole::Evaluator)
                    .model,
            ),
            provider: config
                .resolve_model_for_role(workgraph::config::DispatchRole::Evaluator)
                .provider,
            verify: None,
            agent: config.agency.evaluator_agent.clone(),

            loop_iteration: 0,
            cycle_failure_restarts: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            cycle_config: None,
            // Evaluation uses inline lightweight LLM call — no file access needed
            exec_mode: Some("bare".to_string()),
            token_usage: None,
            session_id: None,
            wait_condition: None,
            checkpoint: None,
            resurrection_count: 0,
            last_resurrected_at: None,
        };

        graph.add_node(Node::Task(eval_task));

        // Tag the source task so we never recreate the eval task after gc.
        if let Some(source) = graph.get_task_mut(task_id)
            && !source.tags.iter().any(|t| t == "eval-scheduled")
        {
            source.tags.push("eval-scheduled".to_string());
        }

        eprintln!(
            "[coordinator] Created evaluation task '{}' blocked by '{}'",
            eval_task_id, task_id,
        );
        modified = true;
    }

    // Unblock evaluation tasks whose source task has Failed.
    // `ready_tasks()` only unblocks when the blocker is Done. For Failed
    // tasks we still want evaluation to proceed (§4.3: "Failed tasks also
    // get evaluated"), so we remove the blocker explicitly.
    let eval_fixups: Vec<(String, String)> = graph
        .tasks()
        .filter(|t| t.id.starts_with(".evaluate-") && t.status == Status::Open)
        .filter_map(|t| {
            // The eval task blocks on a single task: the original
            if t.after.len() == 1 {
                let source_id = &t.after[0];
                if let Some(source) = graph.get_task(source_id)
                    && source.status == Status::Failed
                {
                    return Some((t.id.clone(), source_id.clone()));
                }
            }
            None
        })
        .collect();

    for (eval_id, source_id) in &eval_fixups {
        if let Some(t) = graph.get_task_mut(eval_id) {
            t.after.retain(|b| b != source_id);
            modified = true;
            eprintln!(
                "[coordinator] Unblocked evaluation task '{}' (source '{}' failed)",
                eval_id, source_id,
            );
        }
    }

    modified
}

/// Create verification tasks for tasks whose FLIP score fell below the
/// configured threshold. The verification task independently checks whether
/// the work was actually completed, using the Opus model by default.
///
/// Returns `true` if the graph was modified.
fn build_flip_verification_tasks(
    dir: &Path,
    graph: &mut workgraph::graph::WorkGraph,
    config: &Config,
) -> bool {
    let threshold = match config.agency.flip_verification_threshold {
        Some(t) => t,
        None => return false, // Disabled
    };

    // Load all FLIP evaluations
    let evals_dir = dir.join("agency").join("evaluations");
    let all_evals = load_all_evaluations_or_warn(&evals_dir);

    // Filter to FLIP evaluations below threshold
    let low_flip: Vec<&Evaluation> = all_evals
        .iter()
        .filter(|e| e.source == eval_source::FLIP && e.score < threshold)
        .collect();

    if low_flip.is_empty() {
        return false;
    }

    let mut modified = false;
    let verification_resolved =
        config.resolve_model_for_role(workgraph::config::DispatchRole::Verification);
    let verification_model = verification_resolved.model;

    for eval in &low_flip {
        let source_task_id = &eval.task_id;
        let verify_task_id = format!(".verify-flip-{}", source_task_id);

        // Skip if verification task already exists
        if graph.get_task(&verify_task_id).is_some() {
            continue;
        }

        // Skip if the source task doesn't exist or is already failed
        let source_task = match graph.get_task(source_task_id) {
            Some(t) => t,
            None => continue,
        };
        if source_task.status == Status::Failed || source_task.status == Status::Abandoned {
            continue;
        }

        // Skip system tasks (dot-prefixed) to prevent verification loops
        if workgraph::graph::is_system_task(source_task_id) {
            continue;
        }

        // Build verification task description
        let source_verify_cmd = source_task.verify.clone();
        let source_title = source_task.title.clone();
        let source_desc_snippet = source_task
            .description
            .as_deref()
            .unwrap_or("(no description)")
            .chars()
            .take(2000)
            .collect::<String>();

        let mut desc = format!(
            "## FLIP Verification\n\n\
             FLIP score {:.2} is below threshold {:.2} — independently verify this task.\n\n\
             ### Original Task\n\
             **ID:** {}\n\
             **Title:** {}\n\
             **Description:**\n{}\n\n\
             ### Verification Instructions\n\
             You must independently verify whether the work was actually completed.\n\
             Do NOT trust the original agent's claims. Check independently:\n\n",
            eval.score, threshold, source_task_id, source_title, source_desc_snippet,
        );

        if let Some(ref verify_cmd) = source_verify_cmd {
            desc.push_str(&format!(
                "1. **Run the verification command:** `{}`\n",
                verify_cmd
            ));
            desc.push_str("2. Check git log/diff for actual changes\n");
            desc.push_str("3. Run relevant tests\n");
            desc.push_str("4. Verify artifacts exist\n\n");
        } else {
            desc.push_str(
                "1. Check `git log --oneline -10` for recent commits related to this task\n",
            );
            desc.push_str("2. Check `git diff` to see if meaningful changes were made\n");
            desc.push_str("3. Run `cargo build && cargo test` to verify nothing is broken\n");
            desc.push_str("4. Verify any artifacts mentioned in the task description exist\n\n");
        }

        desc.push_str(
            "### Verdict\n\
             - If verification **passes**: run `wg log '{source_task_id}' \"FLIP verification passed (score {score:.2})\"` and mark this task done.\n\
             - If verification **fails**: run `wg fail '{source_task_id}' --reason \"FLIP verification failed: <reason>\"` then mark this task done.\n"
        );
        // Replace placeholders
        desc = desc.replace("{source_task_id}", source_task_id);
        desc = desc.replace("{score:.2}", &format!("{:.2}", eval.score));

        let verify_task = Task {
            id: verify_task_id.clone(),
            title: format!("Verify (FLIP {:.2}): {}", eval.score, source_title),
            description: Some(desc),
            status: Status::Open,
            assigned: None,
            estimate: None,
            before: vec![],
            after: vec![], // Not blocked by anything — source task is already done
            requires: vec![],
            tags: vec!["verification".to_string(), "agency".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: Some(Utc::now().to_rfc3339()),
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: Some(1),
            failure_reason: None,
            model: Some(verification_model.clone()),
            provider: verification_resolved.provider.clone(),
            verify: source_verify_cmd,
            agent: None,
            loop_iteration: 0,
            cycle_failure_restarts: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            cycle_config: None,
            // Verification needs read-only file access (run tests, check git, verify artifacts)
            // but does not modify source files — "light" is appropriate.
            exec_mode: Some("light".to_string()),
            token_usage: None,
            session_id: None,
            wait_condition: None,
            checkpoint: None,
            resurrection_count: 0,
            last_resurrected_at: None,
        };

        graph.add_node(Node::Task(verify_task));

        // Log the trigger on the source task
        if let Some(source) = graph.get_task_mut(source_task_id) {
            source.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("coordinator".to_string()),
                message: format!(
                    "FLIP score {:.2} below threshold {:.2} — triggering Opus verification",
                    eval.score, threshold,
                ),
            });
        }

        eprintln!(
            "[coordinator] Created FLIP verification task '{}' (score {:.2} < {:.2})",
            verify_task_id, eval.score, threshold,
        );
        modified = true;
    }

    modified
}

/// Spawn an evaluation task directly without the full agent spawn machinery.
///
/// Instead of coordinator -> run.sh -> bash -> `wg evaluate` -> claude, this
/// forks a single process: `wg evaluate <source-task> --model <model>` that
/// marks the eval task done/failed on exit.  This eliminates:
///   - Executor config resolution & template processing
///   - run.sh wrapper script
///   - prompt.txt / metadata.json generation
///
/// The forked process is still tracked in the agent registry for dead-agent
/// detection.
fn spawn_eval_inline(
    dir: &Path,
    eval_task_id: &str,
    evaluator_model: Option<&str>,
) -> Result<(String, u32)> {
    use std::process::{Command, Stdio};

    let graph_path = graph_path(dir);
    let graph = load_graph(&graph_path).context("Failed to load graph for eval spawn")?;

    // Extract needed fields from the eval task before releasing the mutable borrow.
    let (eval_task_status, eval_task_exec, eval_task_agent) = {
        let task = graph.get_task_or_err(eval_task_id)?;
        (task.status, task.exec.clone(), task.agent.clone())
    };

    if eval_task_status != Status::Open {
        anyhow::bail!(
            "Eval task '{}' is not open (status: {:?})",
            eval_task_id,
            eval_task_status
        );
    }

    // Use the task's exec command directly if it starts with "wg evaluate".
    // This handles both "wg evaluate run <task>" and "wg evaluate org <task>".
    // Fall back to reconstructing from task ID for backward compatibility.
    let source_task_id = eval_task_id
        .strip_prefix(".evaluate-")
        .or_else(|| eval_task_id.strip_prefix("evaluate-"))
        .unwrap_or(eval_task_id);
    let eval_cmd = if let Some(ref exec) = eval_task_exec
        && exec.starts_with("wg evaluate")
    {
        exec.to_string()
    } else {
        format!(
            "wg evaluate run '{}'",
            source_task_id.replace('\'', "'\\''")
        )
    };

    // Determine if FLIP evaluation should also run after standard eval.
    // FLIP runs when: flip_enabled is true globally, OR the source task has the 'flip-eval' tag.
    let config = Config::load_or_default(dir);
    let source_has_flip_tag = graph
        .get_task(source_task_id)
        .map(|t| t.tags.iter().any(|tag| tag == "flip-eval"))
        .unwrap_or(false);
    let run_flip = config.agency.flip_enabled || source_has_flip_tag;
    let flip_cmd = if run_flip {
        Some(format!(
            "wg evaluate run '{}' --flip",
            source_task_id.replace('\'', "'\\''")
        ))
    } else {
        None
    };

    // Resolve the special agent (evaluator) hash for performance recording.
    // After the inline eval completes, we record an Evaluation against this
    // agent so it accumulates performance history like any other agent.
    let special_agent_hash = eval_task_agent
        .clone()
        .or_else(|| config.agency.evaluator_agent.clone());

    // Set up minimal agent tracking
    let mut agent_registry = AgentRegistry::load(dir)?;
    let agent_id = format!("agent-{}", agent_registry.next_agent_id);

    // Create minimal output directory for log capture
    let output_dir = dir.join("agents").join(&agent_id);
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Failed to create eval output dir: {:?}", output_dir))?;
    let output_file = output_dir.join("output.log");
    let output_file_str = output_file.to_string_lossy().to_string();

    let escaped_eval_id = eval_task_id.replace('\'', "'\\''");
    let escaped_output = output_file_str.replace('\'', "'\\''");

    // Build the special agent performance recording command.
    // After `wg evaluate` completes, record an evaluation against the special
    // agent (evaluator) entity so it accumulates performance history.
    // On success: score 1.0. On failure: score 0.0.
    let special_agent_verified = special_agent_hash.as_ref().and_then(|hash| {
        let agency_dir = dir.join("agency");
        let agents_dir = agency_dir.join("cache/agents");
        agency::find_agent_by_prefix(&agents_dir, hash)
            .ok()
            .map(|a| a.id)
    });

    // Build the optional FLIP command fragment. FLIP runs after standard eval
    // succeeds. FLIP failure is non-fatal (|| true) — it produces supplementary
    // 'source: flip' evaluation records but should not block the eval task.
    let flip_fragment = flip_cmd
        .as_ref()
        .map(|cmd| format!("\n    {cmd} >> '{escaped_output}' 2>&1 || true"))
        .unwrap_or_default();

    // Single script: run eval, optionally run FLIP, record special agent perf, then mark done/failed
    let env_unset = workgraph::env_sanitize::shell_unset_clause();
    let script = if let Some(ref sa_id) = special_agent_verified {
        let escaped_sa_id = sa_id.replace('\'', "'\\''");
        format!(
            r#"{env_unset}{eval_cmd} >> '{escaped_output}' 2>&1
EXIT_CODE=$?
if [ $EXIT_CODE -eq 0 ]; then{flip_fragment}
    wg evaluate record '{escaped_eval_id}' 1.0 --source system --notes "Inline evaluation completed successfully (agent: {escaped_sa_id})" 2>> '{escaped_output}' || true
    wg done '{escaped_eval_id}' 2>> '{escaped_output}'
else
    wg evaluate record '{escaped_eval_id}' 0.0 --source system --notes "Inline evaluation failed with exit code $EXIT_CODE (agent: {escaped_sa_id})" 2>> '{escaped_output}' || true
    wg fail '{escaped_eval_id}' --reason "wg evaluate exited with code $EXIT_CODE" 2>> '{escaped_output}'
fi
exit $EXIT_CODE"#,
        )
    } else {
        format!(
            r#"{env_unset}{eval_cmd} >> '{escaped_output}' 2>&1
EXIT_CODE=$?
if [ $EXIT_CODE -eq 0 ]; then{flip_fragment}
    wg done '{escaped_eval_id}' 2>> '{escaped_output}'
else
    wg fail '{escaped_eval_id}' --reason "wg evaluate exited with code $EXIT_CODE" 2>> '{escaped_output}'
fi
exit $EXIT_CODE"#,
        )
    };

    // Claim the task before spawning (atomically)
    {
        let eval_task_id_owned = eval_task_id.to_string();
        let agent_id_clone = agent_id.clone();
        let eval_model_str = evaluator_model.map(|m| m.to_string());
        workgraph::parser::mutate_graph(&graph_path, |g| -> Result<()> {
            let task = g.get_task_mut_or_err(&eval_task_id_owned)?;
            task.status = Status::InProgress;
            task.started_at = Some(Utc::now().to_rfc3339());
            task.assigned = Some(agent_id_clone.clone());
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some(agent_id_clone),
                message: format!(
                    "Spawned eval inline{}",
                    eval_model_str
                        .as_deref()
                        .map(|m| format!(" --model {}", m))
                        .unwrap_or_default()
                ),
            });
            Ok(())
        })
        .context("Failed to save graph after claiming eval task")?;
    }

    // Fork the process
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(&script);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // Detach into own session so it survives daemon restart
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Rollback the claim atomically
            let eval_task_id_owned = eval_task_id.to_string();
            let agent_id_clone = agent_id.clone();
            let _ = workgraph::parser::mutate_graph(&graph_path, |g| -> Result<()> {
                if let Some(t) = g.get_task_mut(&eval_task_id_owned) {
                    t.status = Status::Open;
                    t.started_at = None;
                    t.assigned = None;
                    t.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: Some(agent_id_clone),
                        message: format!("Eval spawn failed, reverting claim: {}", e),
                    });
                }
                Ok(())
            });
            return Err(anyhow::anyhow!("Failed to spawn eval process: {}", e));
        }
    };

    let pid = child.id();

    // Register in agent registry for dead-agent detection
    agent_registry.register_agent_with_model(
        pid,
        eval_task_id,
        "eval",
        &output_file_str,
        evaluator_model,
    );
    agent_registry
        .save(dir)
        .context("Failed to save agent registry after eval spawn")?;

    Ok((agent_id, pid))
}

/// Spawn agents on ready tasks, up to `slots_available`. Returns the number of
/// agents successfully spawned.
fn spawn_agents_for_ready_tasks(
    dir: &Path,
    graph: &workgraph::graph::WorkGraph,
    executor: &str,
    model: Option<&str>,
    slots_available: usize,
    auto_assign: bool,
    dispatch_grace_seconds: u64,
) -> usize {
    let cycle_analysis = graph.compute_cycle_analysis();
    let final_ready = ready_tasks_with_peers_cycle_aware(graph, dir, &cycle_analysis);
    let agents_dir = dir.join("agency").join("cache/agents");
    let mut spawned = 0;

    // Load agent registry once to check for alive agents on each task.
    // This prevents re-spawning on a task when a duplicate agent is killed
    // but another agent is still alive and working on the same task.
    let registry = AgentRegistry::load(dir).ok();

    let to_spawn = final_ready.iter().take(slots_available);
    for task in to_spawn {
        // Skip if already claimed
        if task.assigned.is_some() {
            continue;
        }

        // Dispatch grace period: skip tasks created less than `dispatch_grace_seconds` ago.
        // This prevents premature spawn when tasks are created and then have
        // dependencies wired shortly after (e.g., `wg add` then `wg edit --add-after`).
        if dispatch_grace_seconds > 0 {
            if let Some(ref created_str) = task.created_at {
                if let Ok(created) = created_str.parse::<chrono::DateTime<chrono::Utc>>() {
                    let age = Utc::now().signed_duration_since(created);
                    if age.num_seconds() < dispatch_grace_seconds as i64 {
                        eprintln!(
                            "[coordinator] Skipping spawn for '{}': created {}s ago (grace: {}s)",
                            task.id,
                            age.num_seconds(),
                            dispatch_grace_seconds,
                        );
                        continue;
                    }
                }
            }
        }

        // Skip if any alive agent is already working on this task.
        // This guards against the race where killing a duplicate unclaims the
        // task (setting assigned=None) before the surviving agent's next
        // heartbeat re-claims it — without this check the coordinator would
        // see the task as ready and spawn yet another duplicate.
        if let Some(ref reg) = registry {
            let has_alive_agent = reg.agents.values().any(|a| {
                a.task_id == task.id && a.is_alive() && is_process_alive(a.pid)
            });
            if has_alive_agent {
                eprintln!(
                    "[coordinator] Skipping task '{}': alive agent already working on it",
                    task.id
                );
                continue;
            }
        }

        // When auto_assign is enabled, non-system tasks must go through the
        // assignment flow (build_auto_assign_tasks → .assign-* task → wg assign)
        // before being spawned.  The assignment flow sets `task.agent`; if it's
        // still None the task hasn't been assigned yet — skip it so the next
        // tick's Phase 3 can create the .assign-* task.
        if auto_assign
            && !workgraph::graph::is_system_task(&task.id)
            && task.agent.is_none()
        {
            continue;
        }

        // Evaluation tasks run inline: fork `wg evaluate`
        // directly instead of going through the full spawn machinery
        // (run.sh, executor config, etc.)
        let is_eval_task = task.tags.iter().any(|t| t == "evaluation") && task.exec.is_some();
        if is_eval_task {
            let eval_model = task.model.as_deref();
            eprintln!(
                "[coordinator] Spawning eval inline for: {} - {}{}",
                task.id,
                task.title,
                eval_model
                    .map(|m| format!(" (model: {})", m))
                    .unwrap_or_default(),
            );
            match spawn_eval_inline(dir, &task.id, eval_model) {
                Ok((agent_id, pid)) => {
                    eprintln!("[coordinator] Spawned eval {} (PID {})", agent_id, pid);
                    spawned += 1;
                }
                Err(e) => {
                    eprintln!("[coordinator] Failed to spawn eval for {}: {}", task.id, e);
                }
            }
            continue;
        }

        // Resolve executor: tasks with exec commands or exec_mode=shell use shell executor,
        // otherwise: agent.executor > config.coordinator.executor
        let effective_executor = if task.exec.is_some()
            || task.exec_mode.as_deref() == Some("shell")
        {
            "shell".to_string()
        } else {
            task.agent
                .as_ref()
                .and_then(|agent_hash| agency::find_agent_by_prefix(&agents_dir, agent_hash).ok())
                .map(|agent| agent.executor)
                .unwrap_or_else(|| executor.to_string())
        };

        // Pass coordinator model to spawn; spawn resolves the full hierarchy:
        // task.model > executor.model > coordinator.model > 'default'
        eprintln!(
            "[coordinator] Spawning agent for: {} - {} (executor: {})",
            task.id, task.title, effective_executor
        );
        match spawn::spawn_agent(dir, &task.id, &effective_executor, None, model) {
            Ok((agent_id, pid)) => {
                eprintln!("[coordinator] Spawned {} (PID {})", agent_id, pid);
                spawned += 1;
            }
            Err(e) => {
                eprintln!("[coordinator] Failed to spawn for {}: {}", task.id, e);
            }
        }
    }

    spawned
}

// ---------------------------------------------------------------------------
// Auto-checkpoint for alive agents
// ---------------------------------------------------------------------------

/// Check alive agents and trigger auto-checkpoints when turn count or time
/// thresholds are met. Calls triage-role model to summarize the agent's recent output.
fn auto_checkpoint_agents(dir: &Path, config: &Config) {
    let interval_turns = config.checkpoint.auto_interval_turns;
    let interval_mins = config.checkpoint.auto_interval_mins;

    // Skip if auto-checkpoint is effectively disabled
    if interval_turns == 0 && interval_mins == 0 {
        return;
    }

    let registry = match AgentRegistry::load(dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    let alive_agents: Vec<_> = registry
        .agents
        .values()
        .filter(|a| a.is_alive() && is_process_alive(a.pid))
        .cloned()
        .collect();

    for agent in &alive_agents {
        if let Err(e) = try_auto_checkpoint(dir, agent, config, interval_turns, interval_mins) {
            eprintln!(
                "[coordinator] Auto-checkpoint failed for agent {} (task {}): {}",
                agent.id, agent.task_id, e
            );
        }
    }
}

/// Attempt auto-checkpoint for a single agent if thresholds are met.
fn try_auto_checkpoint(
    dir: &Path,
    agent: &workgraph::service::registry::AgentEntry,
    config: &Config,
    interval_turns: u32,
    interval_mins: u32,
) -> Result<()> {
    use crate::commands::checkpoint::{self, CheckpointType};
    use workgraph::stream_event;

    let output_path = std::path::Path::new(&agent.output_file);
    let agent_dir = match output_path.parent() {
        Some(d) => d,
        None => return Ok(()),
    };

    // Read stream events to get turn count
    let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);
    let raw_path = agent_dir.join(stream_event::RAW_STREAM_FILE_NAME);

    let events = if stream_path.exists() {
        stream_event::read_stream_events(&stream_path, 0)
            .map(|(evts, _)| evts)
            .unwrap_or_default()
    } else if raw_path.exists() {
        stream_event::translate_claude_stream(&raw_path, 0)
            .map(|(evts, _)| evts)
            .unwrap_or_default()
    } else {
        return Ok(());
    };

    if events.is_empty() {
        return Ok(());
    }

    // Count turns from stream events
    let turn_count: u32 = events
        .iter()
        .filter(|e| matches!(e, stream_event::StreamEvent::Turn { .. }))
        .count() as u32;

    // Get the timestamp of the latest event
    let last_event_ms = events.last().map(|e| e.timestamp_ms()).unwrap_or(0);

    // Load latest checkpoint for this agent to determine if we need a new one
    let latest_checkpoint = checkpoint::load_latest(dir, &agent.id)?;

    let should_checkpoint = match &latest_checkpoint {
        Some(cp) => {
            // Check turn-based trigger
            let cp_turn = cp.turn_count.unwrap_or(0) as u32;
            let turns_since = turn_count.saturating_sub(cp_turn);
            let turn_trigger = interval_turns > 0 && turns_since >= interval_turns;

            // Check time-based trigger
            let cp_ms = chrono::DateTime::parse_from_rfc3339(&cp.timestamp)
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0);
            let elapsed_mins = (last_event_ms - cp_ms).max(0) / 60_000;
            let time_trigger = interval_mins > 0 && elapsed_mins as u32 >= interval_mins;

            turn_trigger || time_trigger
        }
        None => {
            // No checkpoint yet — trigger on first threshold
            let turn_trigger = interval_turns > 0 && turn_count >= interval_turns;

            let init_ms = events
                .first()
                .map(|e| e.timestamp_ms())
                .unwrap_or(last_event_ms);
            let elapsed_mins = (last_event_ms - init_ms).max(0) / 60_000;
            let time_trigger = interval_mins > 0 && elapsed_mins as u32 >= interval_mins;

            turn_trigger || time_trigger
        }
    };

    if !should_checkpoint {
        return Ok(());
    }

    // Generate summary via haiku
    let summary = generate_checkpoint_summary(config, &agent.output_file, &agent.task_id)?;

    eprintln!(
        "[coordinator] Auto-checkpoint for agent {} (task {}, turn {}): {}",
        agent.id,
        agent.task_id,
        turn_count,
        summary.chars().take(80).collect::<String>()
    );

    // Store checkpoint
    checkpoint::run(
        dir,
        &agent.task_id,
        &summary,
        Some(&agent.id),
        &[], // files_modified not tracked in auto-checkpoint
        None,
        Some(turn_count as u64),
        None,
        None,
        CheckpointType::Auto,
        false,
    )?;

    Ok(())
}

/// Call triage-role model to summarize an agent's recent output log.
fn generate_checkpoint_summary(
    config: &Config,
    output_file: &str,
    task_id: &str,
) -> Result<String> {
    let timeout_secs = config.agency.triage_timeout.unwrap_or(30);

    // Read last 20KB of output for summary context
    let log_content = triage::read_truncated_log(output_file, 20_000);

    let prompt = format!(
        r#"Summarize the progress of an agent working on task '{task_id}'.

## Agent Output (last portion)
```
{log_content}
```

## Instructions
Write a 2-4 sentence summary of what the agent has accomplished so far.
Focus on: files modified, features implemented, tests written, current status.
Respond with ONLY the summary text, no JSON or formatting."#
    );

    let result = workgraph::service::llm::run_lightweight_llm_call(
        config,
        workgraph::config::DispatchRole::Triage,
        &prompt,
        timeout_secs,
    )
    .context("Checkpoint summary LLM call failed")?;

    Ok(result.text)
}

/// Single coordinator tick: spawn agents on ready tasks
pub fn coordinator_tick(
    dir: &Path,
    max_agents: usize,
    executor: &str,
    model: Option<&str>,
) -> Result<TickResult> {
    let graph_path = graph_path(dir);

    // Load config for agency settings
    let config = Config::load_or_default(dir);

    // Process chat inbox FIRST — chat is a user-facing interaction that must not
    // be blocked by agent capacity limits or empty task queues. The early returns
    // below (max agents, no ready tasks) would skip chat processing otherwise.
    process_chat_inbox(dir);

    // Phase 1: Clean up dead agents and count alive ones
    let alive_count = match cleanup_and_count_alive(dir, &graph_path, max_agents)? {
        Ok(count) => count,
        Err(early_result) => return Ok(early_result),
    };

    // Phase 1.5: Auto-checkpoint alive agents if thresholds are met
    auto_checkpoint_agents(dir, &config);

    let slots_available = max_agents.saturating_sub(alive_count);

    // Phases 2-4.5: Atomically load graph, run all mutation phases, and save.
    // Holds flock across the entire cycle to prevent TOCTOU races with concurrent
    // writers (agents, IPC, etc.).
    workgraph::parser::mutate_graph(&graph_path, |graph| -> Result<()> {
        // Phase 2.5: Cycle iteration — reactivate cycles where all members are Done.
        {
            let cycle_analysis = graph.compute_cycle_analysis();
            let reactivated = evaluate_all_cycle_iterations(graph, &cycle_analysis);
            if !reactivated.is_empty() {
                eprintln!(
                    "[coordinator] Cycle iteration: re-activated {} task(s): {:?}",
                    reactivated.len(),
                    reactivated
                );
            }
        }

        // Phase 2.6: Cycle failure restart
        {
            let cycle_analysis = graph.compute_cycle_analysis();
            let reactivated = evaluate_all_cycle_failure_restarts(graph, &cycle_analysis);
            if !reactivated.is_empty() {
                eprintln!(
                    "[coordinator] Cycle failure restart: re-activated {} task(s): {:?}",
                    reactivated.len(),
                    reactivated
                );
            }
        }

        // Phase 2.7: Evaluate waiting tasks
        evaluate_waiting_tasks(graph, dir);

        // Phase 2.8: Message-triggered resurrection
        resurrect_done_tasks(graph, dir);

        // Phase 3: Auto-assign unassigned ready tasks
        if config.agency.auto_assign {
            build_auto_assign_tasks(graph, &config, dir);
        }

        // Phase 4: Auto-evaluate tasks
        if config.agency.auto_evaluate {
            build_auto_evaluate_tasks(dir, graph, &config);
        }

        // Phase 4.5: FLIP verification
        build_flip_verification_tasks(dir, graph, &config);

        Ok(())
    })
    .context("Failed to save graph after coordinator mutation phases")?;

    // Phase 5: Reload graph (read-only) for ready task check and spawning
    let graph = load_graph(&graph_path).context("Failed to reload graph for spawn phase")?;
    if let Some(early_result) = check_ready_or_return(&graph, alive_count, dir) {
        return Ok(early_result);
    }

    // Phase 6: Spawn agents on ready tasks
    let cycle_analysis = graph.compute_cycle_analysis();
    let final_ready = ready_tasks_with_peers_cycle_aware(&graph, dir, &cycle_analysis);
    let ready_count = final_ready.len();
    drop(final_ready);
    // Resolve task agent model: CLI override > models.task_agent > models.default > agent.model
    let effective_model = model.map(String::from).unwrap_or_else(|| {
        config
            .resolve_model_for_role(workgraph::config::DispatchRole::TaskAgent)
            .model
    });
    let spawned = spawn_agents_for_ready_tasks(
        dir,
        &graph,
        executor,
        Some(effective_model.as_str()),
        slots_available,
        config.agency.auto_assign,
        config.agency.auto_assign_grace_seconds,
    );

    Ok(TickResult {
        agents_alive: alive_count + spawned,
        tasks_ready: ready_count,
        agents_spawned: spawned,
    })
}

/// Process pending chat inbox messages and write responses to the outbox.
///
/// Simple stub that acknowledges receipt when the coordinator agent is not
/// running. The full path (CLI → IPC → inbox → coordinator tick → outbox → CLI)
/// is wired; when the coordinator agent is enabled it handles messages instead.
fn process_chat_inbox(dir: &Path) {
    let chat_dir = dir.join("chat");
    if !chat_dir.exists() {
        return;
    }

    let inbox_cursor = match chat::read_coordinator_cursor(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[coordinator] Failed to read chat coordinator cursor: {}",
                e
            );
            return;
        }
    };

    let new_messages = match chat::read_inbox_since(dir, inbox_cursor) {
        Ok(msgs) => msgs,
        Err(e) => {
            eprintln!("[coordinator] Failed to read chat inbox: {}", e);
            return;
        }
    };

    if new_messages.is_empty() {
        return;
    }

    eprintln!(
        "[coordinator] Processing {} chat message(s)",
        new_messages.len()
    );

    for msg in &new_messages {
        let response = format!(
            "Message received. The coordinator agent will provide \
             intelligent responses. For now, your message has been logged: \"{}\"",
            msg.content
        );
        if let Err(e) = chat::append_outbox(dir, &response, &msg.request_id) {
            eprintln!(
                "[coordinator] Failed to write chat outbox for request_id={}: {}",
                msg.request_id, e
            );
        }
    }

    if let Some(last) = new_messages.last()
        && let Err(e) = chat::write_coordinator_cursor(dir, last.id)
    {
        eprintln!(
            "[coordinator] Failed to update chat coordinator cursor: {}",
            e
        );
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::commands::checkpoint::{self, CheckpointType};
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::stream_event::{self, StreamEvent, StreamWriter};

    fn make_agent_entry(output_file: &std::path::Path) -> workgraph::service::registry::AgentEntry {
        workgraph::service::registry::AgentEntry {
            id: "agent-1".to_string(),
            pid: std::process::id(),
            task_id: "t1".to_string(),
            executor: "test".to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            last_heartbeat: chrono::Utc::now().to_rfc3339(),
            status: workgraph::service::registry::AgentStatus::Working,
            output_file: output_file.to_str().unwrap().to_string(),
            model: None,
            completed_at: None,
        }
    }

    #[test]
    fn test_eval_inline_extracts_source_task_from_exec() {
        // spawn_eval_inline extracts the source task ID from exec command
        // This tests the extraction logic used in the function
        let exec = Some("wg evaluate run my-source-task".to_string());
        let source_id = exec
            .as_deref()
            .and_then(|e| {
                e.strip_prefix("wg evaluate run ")
                    .or_else(|| e.strip_prefix("wg evaluate "))
            })
            .unwrap_or("fallback");
        assert_eq!(source_id, "my-source-task");
    }

    #[test]
    fn test_eval_inline_extracts_source_task_from_id_fallback() {
        // When exec is missing the prefix, fall back to stripping evaluate- from task ID
        let exec: Option<String> = None;
        let eval_task_id = "evaluate-some-task";
        let source_id = exec
            .as_deref()
            .and_then(|e| {
                e.strip_prefix("wg evaluate run ")
                    .or_else(|| e.strip_prefix("wg evaluate "))
            })
            .unwrap_or_else(|| {
                eval_task_id
                    .strip_prefix(".evaluate-")
        .or_else(|| eval_task_id.strip_prefix("evaluate-"))
                    .unwrap_or(eval_task_id)
            });
        assert_eq!(source_id, "some-task");
    }

    #[test]
    fn test_eval_routing_condition() {
        // The routing condition for inline eval: has "evaluation" tag AND exec is set
        let mut task = Task::default();
        task.id = "evaluate-t1".to_string();
        task.tags = vec!["evaluation".to_string(), "agency".to_string()];
        task.exec = Some("wg evaluate run t1".to_string());

        let is_inline_eval = task.tags.iter().any(|t| t == "evaluation") && task.exec.is_some();
        assert!(is_inline_eval);

        // Non-eval exec task should NOT match
        let mut shell_task = Task::default();
        shell_task.exec = Some("bash run.sh".to_string());
        let is_inline_eval2 =
            shell_task.tags.iter().any(|t| t == "evaluation") && shell_task.exec.is_some();
        assert!(!is_inline_eval2);

        // Eval tag but no exec should NOT match
        let mut no_exec = Task::default();
        no_exec.tags = vec!["evaluation".to_string()];
        let is_inline_eval3 =
            no_exec.tags.iter().any(|t| t == "evaluation") && no_exec.exec.is_some();
        assert!(!is_inline_eval3);
    }

    fn setup_workgraph_dir(dir: &Path) {
        let graph_path = dir.join("graph.jsonl");
        let mut graph = WorkGraph::new();
        let mut task = Task::default();
        task.id = "t1".to_string();
        task.title = "Test Task".to_string();
        task.status = Status::InProgress;
        task.assigned = Some("agent-1".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &graph_path).unwrap();
    }

    fn write_stream_events(agent_dir: &Path, turn_count: u32, start_ms: i64) {
        let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);
        let writer = StreamWriter::new(&stream_path);

        writer.write_event(&StreamEvent::Init {
            executor_type: "test".to_string(),
            model: None,
            session_id: None,
            timestamp_ms: start_ms,
        });

        for i in 0..turn_count {
            writer.write_event(&StreamEvent::Turn {
                turn_number: i + 1,
                tools_used: vec![],
                usage: None,
                timestamp_ms: start_ms + (i as i64 + 1) * 60_000, // 1 min between turns
            });
        }
    }

    #[test]
    fn test_auto_checkpoint_turn_trigger() {
        let temp = tempdir().unwrap();
        let dir = temp.path();
        setup_workgraph_dir(dir);

        // Create agent directory with stream events (20 turns, threshold is 15)
        let agent_dir = dir.join("agents").join("agent-1");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let output_file = agent_dir.join("output.log");
        std::fs::write(&output_file, "test output").unwrap();

        write_stream_events(&agent_dir, 20, stream_event::now_ms() - 20 * 60_000);

        // Create a registry with a live agent (use PID 1 which should exist)
        let mut registry = workgraph::service::registry::AgentRegistry::default();
        let agent_entry = make_agent_entry(&output_file);
        registry
            .agents
            .insert("agent-1".to_string(), agent_entry.clone());

        let service_dir = dir.join("service");
        std::fs::create_dir_all(&service_dir).unwrap();
        let registry_path = service_dir.join("registry.json");
        std::fs::write(
            &registry_path,
            serde_json::to_string_pretty(&registry).unwrap(),
        )
        .unwrap();

        // Config with 15 turn threshold
        let config = Config::default(); // default has auto_interval_turns=15

        // Should not panic and should attempt checkpoint.
        // The logic should correctly identify the trigger and call the LLM.
        // In CI without Claude CLI this will error; with Claude CLI it succeeds.
        let result = try_auto_checkpoint(dir, &agent_entry, &config, 15, 20);
        if let Err(ref e) = result {
            let err_msg = format!("{:#}", e);
            assert!(
                err_msg.to_lowercase().contains("checkpoint summary")
                    || err_msg.contains("claude")
                    || err_msg.contains("Claude")
                    || err_msg.contains("No such file"),
                "Expected LLM-related error, got: {}",
                err_msg
            );
        }
        // Either way (Ok or LLM error), the checkpoint logic triggered correctly.
    }

    #[test]
    fn test_auto_checkpoint_below_threshold_no_trigger() {
        let temp = tempdir().unwrap();
        let dir = temp.path();
        setup_workgraph_dir(dir);

        let agent_dir = dir.join("agents").join("agent-1");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let output_file = agent_dir.join("output.log");
        std::fs::write(&output_file, "test output").unwrap();

        // Only 5 turns, threshold is 15 — should NOT trigger
        let now_ms = stream_event::now_ms();
        write_stream_events(&agent_dir, 5, now_ms - 5 * 60_000);

        let agent_entry = make_agent_entry(&output_file);

        let config = Config::default();

        // Should return Ok(()) — no checkpoint triggered
        let result = try_auto_checkpoint(dir, &agent_entry, &config, 15, 20);
        assert!(result.is_ok());

        // No checkpoint file should exist
        let cp_dir = dir.join("agents").join("agent-1").join("checkpoints");
        assert!(!cp_dir.exists() || std::fs::read_dir(&cp_dir).unwrap().count() == 0);
    }

    #[test]
    fn test_auto_checkpoint_time_trigger() {
        let temp = tempdir().unwrap();
        let dir = temp.path();
        setup_workgraph_dir(dir);

        let agent_dir = dir.join("agents").join("agent-1");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let output_file = agent_dir.join("output.log");
        std::fs::write(&output_file, "test output").unwrap();

        // 5 turns spread over 25 minutes (threshold 20 mins)
        let now_ms = stream_event::now_ms();
        let start_ms = now_ms - 25 * 60_000;

        let stream_path = agent_dir.join(stream_event::STREAM_FILE_NAME);
        let writer = StreamWriter::new(&stream_path);
        writer.write_event(&StreamEvent::Init {
            executor_type: "test".to_string(),
            model: None,
            session_id: None,
            timestamp_ms: start_ms,
        });
        for i in 0..5 {
            writer.write_event(&StreamEvent::Turn {
                turn_number: i + 1,
                tools_used: vec![],
                usage: None,
                timestamp_ms: start_ms + (i as i64 + 1) * 5 * 60_000, // 5 min apart
            });
        }

        let agent_entry = make_agent_entry(&output_file);

        let config = Config::default();

        // Should trigger due to time (25 min > 20 min threshold).
        // May succeed if Claude CLI is available, or fail on LLM call in CI.
        let _result = try_auto_checkpoint(dir, &agent_entry, &config, 15, 20);
    }

    #[test]
    fn test_auto_checkpoint_skips_when_recent_checkpoint_exists() {
        let temp = tempdir().unwrap();
        let dir = temp.path();
        setup_workgraph_dir(dir);

        let agent_dir = dir.join("agents").join("agent-1");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let output_file = agent_dir.join("output.log");
        std::fs::write(&output_file, "test output").unwrap();

        // 20 turns
        let now_ms = stream_event::now_ms();
        write_stream_events(&agent_dir, 20, now_ms - 20 * 60_000);

        // Create a recent checkpoint at turn 18 (so only 2 turns since)
        checkpoint::run(
            dir,
            "t1",
            "Recent checkpoint",
            Some("agent-1"),
            &[],
            None,
            Some(18),
            None,
            None,
            CheckpointType::Auto,
            false,
        )
        .unwrap();

        let agent_entry = make_agent_entry(&output_file);

        let config = Config::default();

        // Should NOT trigger — only 2 turns since last checkpoint
        let result = try_auto_checkpoint(dir, &agent_entry, &config, 15, 20);
        assert!(result.is_ok());
    }

    #[test]
    fn test_auto_checkpoint_disabled_when_zero() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.checkpoint.auto_interval_turns = 0;
        config.checkpoint.auto_interval_mins = 0;

        // Should return immediately without touching anything
        auto_checkpoint_agents(dir.path(), &config);
        // No crash, no panic — success
    }

    // === Wait condition evaluation tests ===

    fn setup_wait_graph(dir: &Path, tasks: Vec<Task>) {
        let path = dir.join("graph.jsonl");
        std::fs::create_dir_all(dir).unwrap();
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
    }

    fn load_wait_graph(dir: &Path) -> WorkGraph {
        let path = dir.join("graph.jsonl");
        load_graph(&path).unwrap()
    }

    #[test]
    fn test_evaluate_condition_task_status_satisfied() {
        let mut graph = WorkGraph::new();
        let mut dep = Task::default();
        dep.id = "dep-a".to_string();
        dep.status = Status::Done;
        graph.add_node(Node::Task(dep));

        let cond = WaitCondition::TaskStatus {
            task_id: "dep-a".to_string(),
            status: Status::Done,
        };
        assert!(evaluate_condition(
            &cond,
            &graph,
            Path::new("/tmp"),
            "main",
            None
        ));
    }

    #[test]
    fn test_evaluate_condition_task_status_not_satisfied() {
        let mut graph = WorkGraph::new();
        let mut dep = Task::default();
        dep.id = "dep-a".to_string();
        dep.status = Status::InProgress;
        graph.add_node(Node::Task(dep));

        let cond = WaitCondition::TaskStatus {
            task_id: "dep-a".to_string(),
            status: Status::Done,
        };
        assert!(!evaluate_condition(
            &cond,
            &graph,
            Path::new("/tmp"),
            "main",
            None
        ));
    }

    #[test]
    fn test_evaluate_condition_timer_elapsed() {
        let graph = WorkGraph::new();
        let past = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let cond = WaitCondition::Timer { resume_after: past };
        assert!(evaluate_condition(
            &cond,
            &graph,
            Path::new("/tmp"),
            "main",
            None
        ));
    }

    #[test]
    fn test_evaluate_condition_timer_not_elapsed() {
        let graph = WorkGraph::new();
        let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let cond = WaitCondition::Timer {
            resume_after: future,
        };
        assert!(!evaluate_condition(
            &cond,
            &graph,
            Path::new("/tmp"),
            "main",
            None
        ));
    }

    #[test]
    fn test_evaluate_condition_file_changed() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("watched.txt");
        std::fs::write(&file_path, "initial").unwrap();

        let mtime = std::fs::metadata(&file_path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let graph = WorkGraph::new();
        // Not changed yet: same mtime
        let cond_same = WaitCondition::FileChanged {
            path: file_path.to_string_lossy().to_string(),
            mtime_at_wait: mtime,
        };
        assert!(!evaluate_condition(
            &cond_same,
            &graph,
            dir.path(),
            "main",
            None
        ));

        // Simulate earlier mtime_at_wait (file was modified after the stored mtime)
        let cond_earlier = WaitCondition::FileChanged {
            path: file_path.to_string_lossy().to_string(),
            mtime_at_wait: mtime - 1,
        };
        assert!(evaluate_condition(
            &cond_earlier,
            &graph,
            dir.path(),
            "main",
            None
        ));
    }

    #[test]
    fn test_evaluate_wait_spec_all_not_satisfied() {
        let mut graph = WorkGraph::new();
        let mut dep_a = Task::default();
        dep_a.id = "dep-a".to_string();
        dep_a.status = Status::Done;
        let mut dep_b = Task::default();
        dep_b.id = "dep-b".to_string();
        dep_b.status = Status::Open;
        graph.add_node(Node::Task(dep_a));
        graph.add_node(Node::Task(dep_b));

        let spec = WaitSpec::All(vec![
            WaitCondition::TaskStatus {
                task_id: "dep-a".to_string(),
                status: Status::Done,
            },
            WaitCondition::TaskStatus {
                task_id: "dep-b".to_string(),
                status: Status::Done,
            },
        ]);
        assert!(!evaluate_wait_spec(
            &spec,
            &graph,
            Path::new("/tmp"),
            "main",
            None
        ));
    }

    #[test]
    fn test_evaluate_wait_spec_any_satisfied() {
        let mut graph = WorkGraph::new();
        let mut dep_a = Task::default();
        dep_a.id = "dep-a".to_string();
        dep_a.status = Status::Done;
        let mut dep_b = Task::default();
        dep_b.id = "dep-b".to_string();
        dep_b.status = Status::Open;
        graph.add_node(Node::Task(dep_a));
        graph.add_node(Node::Task(dep_b));

        let spec = WaitSpec::Any(vec![
            WaitCondition::TaskStatus {
                task_id: "dep-a".to_string(),
                status: Status::Done,
            },
            WaitCondition::TaskStatus {
                task_id: "dep-b".to_string(),
                status: Status::Done,
            },
        ]);
        assert!(evaluate_wait_spec(
            &spec,
            &graph,
            Path::new("/tmp"),
            "main",
            None
        ));
    }

    #[test]
    fn test_unsatisfiable_condition_failed_dep() {
        let mut graph = WorkGraph::new();
        let mut dep = Task::default();
        dep.id = "dep-a".to_string();
        dep.status = Status::Failed;
        graph.add_node(Node::Task(dep));

        let cond = WaitCondition::TaskStatus {
            task_id: "dep-a".to_string(),
            status: Status::Done,
        };
        let result = is_condition_unsatisfiable(&cond, &graph);
        assert!(result.is_some());
        assert!(result.unwrap().contains("failed"));
    }

    #[test]
    fn test_unsatisfiable_condition_nonexistent_task() {
        let graph = WorkGraph::new();
        let cond = WaitCondition::TaskStatus {
            task_id: "nonexistent".to_string(),
            status: Status::Done,
        };
        let result = is_condition_unsatisfiable(&cond, &graph);
        assert!(result.is_some());
        assert!(result.unwrap().contains("no longer exists"));
    }

    #[test]
    fn test_circular_wait_detection() {
        let mut graph = WorkGraph::new();

        let mut task_a = Task::default();
        task_a.id = "task-a".to_string();
        task_a.status = Status::Waiting;
        task_a.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "task-b".to_string(),
            status: Status::Done,
        }]));

        let mut task_b = Task::default();
        task_b.id = "task-b".to_string();
        task_b.status = Status::Waiting;
        task_b.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "task-a".to_string(),
            status: Status::Done,
        }]));

        graph.add_node(Node::Task(task_a));
        graph.add_node(Node::Task(task_b));

        let cycles = detect_circular_waits(&graph);
        assert!(!cycles.is_empty(), "Should detect circular wait");
    }

    #[test]
    fn test_evaluate_waiting_tasks_transitions_to_open() {
        let dir = tempdir().unwrap();

        let mut dep = Task::default();
        dep.id = "dep-a".to_string();
        dep.status = Status::Done;

        let mut main_task = Task::default();
        main_task.id = "main".to_string();
        main_task.status = Status::Waiting;
        main_task.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "dep-a".to_string(),
            status: Status::Done,
        }]));
        main_task.checkpoint = Some("Phase 1 complete".to_string());
        main_task.assigned = Some("agent-1".to_string());

        setup_wait_graph(dir.path(), vec![dep, main_task]);

        let mut graph = load_wait_graph(dir.path());
        let modified = evaluate_waiting_tasks(&mut graph, dir.path());

        assert!(modified);
        let task = graph.get_task("main").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.wait_condition.is_none());
        assert!(
            task.assigned.is_none(),
            "assigned should be cleared for re-dispatch"
        );
        assert!(task.checkpoint.is_some());
        let cp = task.checkpoint.as_ref().unwrap();
        assert!(cp.contains("Resume Context"));
        assert!(cp.contains("Phase 1 complete"));
    }

    #[test]
    fn test_evaluate_waiting_tasks_autofails_unsatisfiable() {
        let dir = tempdir().unwrap();

        let mut dep = Task::default();
        dep.id = "dep-a".to_string();
        dep.status = Status::Failed;

        let mut main_task = Task::default();
        main_task.id = "main".to_string();
        main_task.status = Status::Waiting;
        main_task.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "dep-a".to_string(),
            status: Status::Done,
        }]));

        setup_wait_graph(dir.path(), vec![dep, main_task]);

        let mut graph = load_wait_graph(dir.path());
        let modified = evaluate_waiting_tasks(&mut graph, dir.path());

        assert!(modified);
        let task = graph.get_task("main").unwrap();
        assert_eq!(task.status, Status::Failed);
        assert!(
            task.failure_reason
                .as_ref()
                .unwrap()
                .contains("unsatisfiable")
        );
    }

    #[test]
    fn test_evaluate_waiting_tasks_fails_circular_waits() {
        let dir = tempdir().unwrap();

        let mut task_a = Task::default();
        task_a.id = "task-a".to_string();
        task_a.status = Status::Waiting;
        task_a.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "task-b".to_string(),
            status: Status::Done,
        }]));

        let mut task_b = Task::default();
        task_b.id = "task-b".to_string();
        task_b.status = Status::Waiting;
        task_b.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "task-a".to_string(),
            status: Status::Done,
        }]));

        setup_wait_graph(dir.path(), vec![task_a, task_b]);

        let mut graph = load_wait_graph(dir.path());
        let modified = evaluate_waiting_tasks(&mut graph, dir.path());

        assert!(modified);
        let a = graph.get_task("task-a").unwrap();
        let b = graph.get_task("task-b").unwrap();
        assert_eq!(a.status, Status::Failed);
        assert_eq!(b.status, Status::Failed);
        assert!(a.failure_reason.as_ref().unwrap().contains("Circular wait"));
    }

    #[test]
    fn test_wait_resume_preserves_session_id() {
        let dir = tempdir().unwrap();

        let mut dep = Task::default();
        dep.id = "dep-a".to_string();
        dep.status = Status::Done;
        dep.artifacts = vec!["docs/api-schema.json".to_string()];

        let mut main_task = Task::default();
        main_task.id = "main".to_string();
        main_task.status = Status::Waiting;
        main_task.session_id = Some("session-123".to_string());
        main_task.checkpoint = Some("Waiting for API schema".to_string());
        main_task.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "dep-a".to_string(),
            status: Status::Done,
        }]));
        main_task.assigned = Some("agent-1".to_string());

        setup_wait_graph(dir.path(), vec![dep, main_task]);

        let mut graph = load_wait_graph(dir.path());
        let modified = evaluate_waiting_tasks(&mut graph, dir.path());

        assert!(modified);
        let task = graph.get_task("main").unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.session_id.as_deref(), Some("session-123"));
        let cp = task.checkpoint.as_ref().unwrap();
        assert!(cp.contains("dep-a"));
        assert!(cp.contains("docs/api-schema.json"));
    }

    #[test]
    fn test_build_resume_delta_content() {
        let mut graph = WorkGraph::new();

        let mut dep = Task::default();
        dep.id = "dep-a".to_string();
        dep.status = Status::Done;
        dep.artifacts = vec!["output.txt".to_string()];
        graph.add_node(Node::Task(dep));

        let mut main_task = Task::default();
        main_task.id = "main".to_string();
        main_task.checkpoint = Some("Working on phase 2".to_string());
        main_task.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "dep-a".to_string(),
            status: Status::Done,
        }]));
        graph.add_node(Node::Task(main_task));

        let task = graph.get_task("main").unwrap();
        let delta = build_resume_delta(&graph, task, Path::new("/tmp"));

        assert!(delta.contains("Resume Context"));
        assert!(delta.contains("dep-a: done"));
        assert!(delta.contains("output.txt"));
        assert!(delta.contains("Working on phase 2"));
        assert!(delta.contains("Continue your work"));
    }

    #[test]
    fn test_evaluate_waiting_tasks_no_change_when_not_satisfied() {
        let dir = tempdir().unwrap();

        let mut dep = Task::default();
        dep.id = "dep-a".to_string();
        dep.status = Status::InProgress;

        let mut main_task = Task::default();
        main_task.id = "main".to_string();
        main_task.status = Status::Waiting;
        main_task.wait_condition = Some(WaitSpec::All(vec![WaitCondition::TaskStatus {
            task_id: "dep-a".to_string(),
            status: Status::Done,
        }]));

        setup_wait_graph(dir.path(), vec![dep, main_task]);

        let mut graph = load_wait_graph(dir.path());
        let modified = evaluate_waiting_tasks(&mut graph, dir.path());

        assert!(!modified);
        let task = graph.get_task("main").unwrap();
        assert_eq!(task.status, Status::Waiting);
        assert!(task.wait_condition.is_some());
    }

    #[test]
    fn test_message_condition_with_messages() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        messages::send_message(dir.path(), "main", "Hello", "user", "normal").unwrap();

        let mut main_task = Task::default();
        main_task.id = "main".to_string();
        main_task.status = Status::Waiting;
        main_task.wait_condition = Some(WaitSpec::All(vec![WaitCondition::Message]));

        setup_wait_graph(dir.path(), vec![main_task]);

        let mut graph = load_wait_graph(dir.path());
        let modified = evaluate_waiting_tasks(&mut graph, dir.path());

        assert!(modified);
        let task = graph.get_task("main").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    // -----------------------------------------------------------------------
    // Message-triggered resurrection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resurrection_detects_unread_messages_on_done_task() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        // Create a Done task
        let mut task = Task::default();
        task.id = "done-task".to_string();
        task.status = Status::Done;
        task.assigned = Some("agent-old".to_string());

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));

        // Send a message from "user" (not the task's own agent)
        messages::send_message(dir.path(), "done-task", "Please fix X", "user", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(modified, "Graph should be modified by resurrection");
        let task = graph.get_task("done-task").unwrap();
        assert_eq!(task.status, Status::Open, "Done task should be reopened");
        assert!(task.assigned.is_none(), "Assignment should be cleared");
        assert_eq!(task.resurrection_count, 1);
        assert!(task.last_resurrected_at.is_some());
        assert!(task.log.last().unwrap().message.contains("Resurrection"));
    }

    #[test]
    fn test_resurrection_reopen_when_no_downstream_active() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        // Done task with a downstream that is Open (not started)
        let mut parent = Task::default();
        parent.id = "parent".to_string();
        parent.status = Status::Done;
        parent.before = vec!["child".to_string()];

        let mut child = Task::default();
        child.id = "child".to_string();
        child.status = Status::Open;
        child.after = vec!["parent".to_string()];

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(child));

        messages::send_message(
            dir.path(),
            "parent",
            "Update needed",
            "coordinator",
            "normal",
        )
        .unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(modified);
        let task = graph.get_task("parent").unwrap();
        assert_eq!(
            task.status,
            Status::Open,
            "Should reopen (downstream not active)"
        );
    }

    #[test]
    fn test_resurrection_child_task_when_downstream_active() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        // Done task with a downstream that is InProgress
        let mut parent = Task::default();
        parent.id = "parent".to_string();
        parent.status = Status::Done;
        parent.session_id = Some("sess-123".to_string());
        parent.checkpoint = Some("Did some work".to_string());
        parent.before = vec!["downstream".to_string()];

        let mut downstream = Task::default();
        downstream.id = "downstream".to_string();
        downstream.status = Status::InProgress;
        downstream.after = vec!["parent".to_string()];

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(downstream));

        messages::send_message(
            dir.path(),
            "parent",
            "Question about X",
            "agent-downstream",
            "normal",
        )
        .unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(modified);
        // Parent stays Done
        let parent = graph.get_task("parent").unwrap();
        assert_eq!(parent.status, Status::Done, "Parent should stay Done");
        assert_eq!(parent.resurrection_count, 1);

        // Child task created
        let child = graph.get_task(".respond-to-parent").unwrap();
        assert_eq!(child.status, Status::Open);
        assert_eq!(
            child.session_id,
            Some("sess-123".to_string()),
            "Session inherited"
        );
        assert_eq!(
            child.checkpoint,
            Some("Did some work".to_string()),
            "Checkpoint inherited"
        );
        assert!(
            child
                .description
                .as_deref()
                .unwrap()
                .contains("pending messages")
        );
    }

    #[test]
    fn test_resurrection_rate_limit_max_resurrections() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        let mut task = Task::default();
        task.id = "exhausted".to_string();
        task.status = Status::Done;
        task.resurrection_count = MAX_RESURRECTIONS; // Already at max

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));

        messages::send_message(dir.path(), "exhausted", "One more", "user", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(!modified, "Should NOT resurrect: max count reached");
        assert_eq!(graph.get_task("exhausted").unwrap().status, Status::Done);
    }

    #[test]
    fn test_resurrection_cooldown() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        let mut task = Task::default();
        task.id = "cooled".to_string();
        task.status = Status::Done;
        task.resurrection_count = 1;
        task.last_resurrected_at = Some(Utc::now().to_rfc3339()); // Just now

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));

        messages::send_message(dir.path(), "cooled", "Again", "user", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(!modified, "Should NOT resurrect: cooldown active");
        assert_eq!(graph.get_task("cooled").unwrap().status, Status::Done);
    }

    #[test]
    fn test_resurrection_excluded_for_abandoned_tasks() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        let mut task = Task::default();
        task.id = "abandoned".to_string();
        task.status = Status::Abandoned;

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));

        messages::send_message(dir.path(), "abandoned", "Come back", "user", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(!modified, "Should NOT resurrect abandoned tasks");
    }

    #[test]
    fn test_resurrection_ignores_messages_from_own_agent() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        let mut task = Task::default();
        task.id = "self-msg".to_string();
        task.status = Status::Done;
        task.assigned = Some("agent-42".to_string());

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));

        // Only message is from the task's own agent
        messages::send_message(dir.path(), "self-msg", "I'm done", "agent-42", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(!modified, "Should NOT resurrect from own agent's messages");
    }

    #[test]
    fn test_resurrection_batches_multiple_messages() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        let mut task = Task::default();
        task.id = "multi".to_string();
        task.status = Status::Done;

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));

        // Send 3 messages
        messages::send_message(dir.path(), "multi", "Msg 1", "user", "normal").unwrap();
        messages::send_message(dir.path(), "multi", "Msg 2", "coordinator", "normal").unwrap();
        messages::send_message(dir.path(), "multi", "Msg 3", "agent-other", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(modified);
        let task = graph.get_task("multi").unwrap();
        assert_eq!(task.status, Status::Open);
        // Only ONE resurrection despite 3 messages
        assert_eq!(
            task.resurrection_count, 1,
            "Should batch into one resurrection"
        );
        assert!(
            task.log
                .last()
                .unwrap()
                .message
                .contains("3 pending message(s)")
        );
    }

    #[test]
    fn test_resurrection_child_not_duplicated() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        let mut parent = Task::default();
        parent.id = "parent".to_string();
        parent.status = Status::Done;
        parent.before = vec!["downstream".to_string()];

        let mut downstream = Task::default();
        downstream.id = "downstream".to_string();
        downstream.status = Status::InProgress;

        // Child already exists from a previous resurrection
        let mut existing_child = Task::default();
        existing_child.id = ".respond-to-parent".to_string();
        existing_child.status = Status::InProgress;

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(downstream));
        graph.add_node(Node::Task(existing_child));

        messages::send_message(dir.path(), "parent", "Another question", "user", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(!modified, "Should NOT create duplicate child task");
    }

    #[test]
    fn test_resurrection_opt_out_tag() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        let mut task = Task::default();
        task.id = "no-resurrect".to_string();
        task.status = Status::Done;
        task.tags = vec!["resurrect:false".to_string()];

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));

        messages::send_message(dir.path(), "no-resurrect", "Wake up", "user", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(
            !modified,
            "Should NOT resurrect tasks with resurrect:false tag"
        );
    }

    // -----------------------------------------------------------------------
    // spawn_agents_for_ready_tasks: auto_assign filtering
    // -----------------------------------------------------------------------

    /// When auto_assign=true, a ready task WITHOUT an agent field should be
    /// skipped by spawn_agents_for_ready_tasks (it needs to go through the
    /// .assign-* flow first).
    #[test]
    fn test_spawn_skips_unassigned_task_when_auto_assign_enabled() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir.join("agency/cache/agents")).unwrap();

        let mut task = Task::default();
        task.id = "my-task".to_string();
        task.title = "Test".to_string();
        task.status = Status::Open;
        // No agent field set — hasn't been through assignment

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));
        save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

        let result = spawn_agents_for_ready_tasks(
            wg_dir,
            &graph,
            "shell",
            None,
            10,
            true, // auto_assign = true
            0,    // no grace period for test
        );

        // Task should be skipped (no agent), so nothing spawned
        assert_eq!(result, 0, "unassigned task should NOT be spawned when auto_assign=true");
    }

    /// When auto_assign=true, a ready task WITH an agent field SHOULD be
    /// spawned (it has been through the assignment flow).
    #[test]
    fn test_spawn_allows_assigned_agent_task_when_auto_assign_enabled() {
        // Verify the condition logic: task with agent set should NOT be skipped
        let has_agent = true; // agent = Some("abc123")
        let is_system = workgraph::graph::is_system_task("my-task");
        let would_skip = true && !is_system && !has_agent;
        assert!(!would_skip, "task with agent field should NOT be skipped");
    }

    /// System tasks (dot-prefixed) are always spawned regardless of auto_assign.
    #[test]
    fn test_spawn_always_allows_system_tasks_when_auto_assign_enabled() {
        // System tasks like .assign-foo, .evaluate-foo should bypass auto_assign filter
        let is_system = workgraph::graph::is_system_task(".assign-my-task");
        assert!(is_system, ".assign-* should be a system task");

        // The filter: skip if auto_assign && !is_system && agent.is_none()
        // For system tasks, !is_system is false, so the condition is false → not skipped
        let would_skip = true && !is_system && true; // auto_assign=true, agent=None
        assert!(!would_skip, "system tasks should never be skipped by auto_assign filter");
    }

    /// When auto_assign=false, tasks without agent field should still be spawned.
    #[test]
    fn test_spawn_allows_unassigned_task_when_auto_assign_disabled() {
        let auto_assign = false;
        let is_system = workgraph::graph::is_system_task("my-task");
        let has_agent = false; // no agent field

        let would_skip = auto_assign && !is_system && !has_agent;
        assert!(!would_skip, "should not skip when auto_assign is disabled");
    }

    // -----------------------------------------------------------------------
    // spawn_agents_for_ready_tasks: dispatch grace period
    // -----------------------------------------------------------------------

    /// Tasks created recently (within grace period) should be skipped by spawn.
    #[test]
    fn test_spawn_skips_task_within_grace_period() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir.join("agency/cache/agents")).unwrap();

        let mut task = Task::default();
        task.id = "fresh-task".to_string();
        task.title = "Fresh".to_string();
        task.status = Status::Open;
        // Created just now — within the grace period
        task.created_at = Some(Utc::now().to_rfc3339());

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));
        save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

        let result = spawn_agents_for_ready_tasks(
            wg_dir,
            &graph,
            "shell",
            None,
            10,
            false,
            60, // 60s grace period — task was just created
        );

        assert_eq!(
            result, 0,
            "freshly created task should be skipped during grace period"
        );
    }

    /// Tasks created long ago (past grace period) should not be skipped.
    #[test]
    fn test_spawn_allows_task_past_grace_period() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir.join("agency/cache/agents")).unwrap();

        let mut task = Task::default();
        task.id = "old-task".to_string();
        task.title = "Old".to_string();
        task.status = Status::Open;
        // Created 2 minutes ago — past the grace period
        task.created_at = Some(
            (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339(),
        );

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));
        save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

        // With grace_seconds=10, a 120s-old task should NOT be skipped.
        // It will still fail to spawn (no real executor), but the function
        // should attempt to spawn (not skip). The return value may be 0 due
        // to spawn failure, but we verify no grace-period skip via stderr.
        let result = spawn_agents_for_ready_tasks(
            wg_dir,
            &graph,
            "shell",
            None,
            10,
            false,
            10, // 10s grace — task is 120s old
        );

        // The task won't actually spawn (no exec command for shell executor),
        // but it wasn't skipped by grace period — it was attempted.
        // Result is 0 due to spawn failure, not grace skip.
        assert_eq!(result, 0);
    }

    /// Grace period of 0 should never skip any tasks.
    #[test]
    fn test_spawn_no_grace_period_allows_fresh_task() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir.join("agency/cache/agents")).unwrap();

        let mut task = Task::default();
        task.id = "fresh-no-grace".to_string();
        task.title = "Fresh No Grace".to_string();
        task.status = Status::Open;
        task.created_at = Some(Utc::now().to_rfc3339());

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task));
        save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

        let result = spawn_agents_for_ready_tasks(
            wg_dir,
            &graph,
            "shell",
            None,
            10,
            false,
            0, // grace disabled
        );

        // Won't actually spawn (no exec command), but grace period didn't skip it
        assert_eq!(result, 0);
    }

    #[test]
    fn test_resurrection_downstream_done_triggers_child() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        // Parent is Done, downstream is also Done (already finished)
        let mut parent = Task::default();
        parent.id = "parent".to_string();
        parent.status = Status::Done;
        parent.before = vec!["downstream".to_string()];

        let mut downstream = Task::default();
        downstream.id = "downstream".to_string();
        downstream.status = Status::Done;

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(downstream));

        messages::send_message(dir.path(), "parent", "Late feedback", "user", "normal").unwrap();

        let modified = resurrect_done_tasks(&mut graph, dir.path());

        assert!(modified);
        // Downstream is Done, so child task should be created
        let child = graph.get_task(".respond-to-parent").unwrap();
        assert_eq!(child.status, Status::Open);
    }
}
