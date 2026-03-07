use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{CycleConfig, Estimate, Node, Status, Task, parse_delay};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Parse a guard expression string into a LoopGuard.
/// Formats: 'task:<id>=<status>' or 'always'
pub fn parse_guard_expr(expr: &str) -> Result<workgraph::graph::LoopGuard> {
    let expr = expr.trim();
    if expr.eq_ignore_ascii_case("always") {
        return Ok(workgraph::graph::LoopGuard::Always);
    }
    if let Some(rest) = expr.strip_prefix("task:") {
        if let Some((task_id, status_str)) = rest.split_once('=') {
            let status = match status_str.to_lowercase().as_str() {
                "open" => Status::Open,
                "in-progress" => Status::InProgress,
                "done" => Status::Done,
                "blocked" => Status::Blocked,
                "failed" => Status::Failed,
                "abandoned" => Status::Abandoned,
                "pending-review" => Status::Done, // pending-review is deprecated, maps to done
                _ => anyhow::bail!("Unknown status '{}' in guard expression", status_str),
            };
            return Ok(workgraph::graph::LoopGuard::TaskStatus {
                task: task_id.to_string(),
                status,
            });
        }
        anyhow::bail!(
            "Invalid guard format. Expected 'task:<id>=<status>', got '{}'",
            expr
        );
    }
    anyhow::bail!(
        "Invalid guard expression '{}'. Expected 'task:<id>=<status>' or 'always'",
        expr
    );
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    title: &str,
    id: Option<&str>,
    description: Option<&str>,
    after: &[String],
    assign: Option<&str>,
    hours: Option<f64>,
    cost: Option<f64>,
    tags: &[String],
    skills: &[String],
    inputs: &[String],
    deliverables: &[String],
    max_retries: Option<u32>,
    model: Option<&str>,
    provider: Option<&str>,
    verify: Option<&str>,
    max_iterations: Option<u32>,
    cycle_guard: Option<&str>,
    cycle_delay: Option<&str>,
    no_converge: bool,
    no_restart_on_failure: bool,
    max_failure_restarts: Option<u32>,
    visibility: &str,
    context_scope: Option<&str>,
    exec_mode: Option<&str>,
    paused: bool,
    delay: Option<&str>,
    not_before: Option<&str>,
) -> Result<()> {
    if title.trim().is_empty() {
        anyhow::bail!("Task title cannot be empty");
    }

    // Validate visibility
    match visibility {
        "internal" | "public" | "peer" => {}
        _ => anyhow::bail!(
            "Invalid visibility '{}'. Valid values: internal, public, peer",
            visibility
        ),
    }

    // Validate context_scope if provided
    if let Some(scope) = context_scope {
        scope
            .parse::<workgraph::context_scope::ContextScope>()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    // Validate exec_mode if provided
    if let Some(mode) = exec_mode {
        match mode {
            "full" | "light" | "bare" | "shell" => {}
            _ => anyhow::bail!(
                "Invalid exec_mode '{}'. Valid values: full, light, bare, shell",
                mode
            ),
        }
    }

    let path = graph_path(dir);

    // Load existing graph to check for ID conflicts
    let mut graph = if path.exists() {
        load_graph(&path).context("Failed to load graph")?
    } else {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    };

    // --- Autopoietic guardrails ---
    let config = workgraph::config::Config::load_or_default(dir);
    let guardrails = &config.guardrails;

    // 1. Per-agent task creation limit (only enforced in agent context)
    let agent_id = std::env::var("WG_AGENT_ID").ok();
    if let Some(ref agent_id) = agent_id {
        let max_child = guardrails.max_child_tasks_per_agent;
        // Count add_task operations by this agent in the provenance log
        let count = count_agent_created_tasks(dir, agent_id);
        if count >= max_child {
            anyhow::bail!(
                "Agent {} has already created {}/{} tasks. \
                 Use wg fail or wg log to explain why more decomposition is needed.",
                agent_id,
                count,
                max_child
            );
        }
    }

    // 2. Task depth limit (enforced when --after is specified)
    if !after.is_empty() {
        let max_depth = guardrails.max_task_depth;
        // The new task's depth = max(depth of each parent) + 1
        let max_parent_depth = after
            .iter()
            .map(|parent_id| graph.task_depth(parent_id))
            .max()
            .unwrap_or(0);
        let new_depth = max_parent_depth + 1;
        if new_depth > max_depth {
            anyhow::bail!(
                "Task would be at depth {} (max: {}). \
                 Consider creating tasks at the current level instead.",
                new_depth,
                max_depth
            );
        }
    }

    // Generate ID if not provided
    let task_id = match id {
        Some(id) => {
            if graph.get_node(id).is_some() {
                anyhow::bail!("Task with ID '{}' already exists", id);
            }
            id.to_string()
        }
        None => generate_id(title, &graph),
    };

    // Validate after references (supports cross-repo peer:task-id syntax)
    for blocker_id in after {
        if blocker_id == &task_id {
            anyhow::bail!("Task '{}' cannot block itself", task_id);
        }
        if workgraph::federation::parse_remote_ref(blocker_id).is_some() {
            // Cross-repo dependency — validated at resolution time, not here
        } else if graph.get_node(blocker_id).is_none() {
            eprintln!(
                "Warning: blocker '{}' does not exist yet (will be treated as unresolved until created)",
                blocker_id
            );
            // Suggest fuzzy match if a close task ID exists
            let all_ids: Vec<&str> = graph.tasks().map(|t| t.id.as_str()).collect();
            if let Some((suggestion, _)) =
                workgraph::check::fuzzy_match_task_id(blocker_id, all_ids.iter().copied(), 3)
            {
                eprintln!("  → Did you mean '{}'?", suggestion);
            }
        }
    }

    let estimate = if hours.is_some() || cost.is_some() {
        Some(Estimate { hours, cost })
    } else {
        None
    };

    // Build cycle config if --max-iterations specified
    let cycle_config = if let Some(max_iter) = max_iterations {
        let guard = match cycle_guard {
            Some(expr) => Some(parse_guard_expr(expr)?),
            None => None,
        };
        let delay = match cycle_delay {
            Some(d) => {
                parse_delay(d).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Invalid cycle delay '{}'. Use format: 30s, 5m, 1h, 24h, 7d",
                        d
                    )
                })?;
                Some(d.to_string())
            }
            None => None,
        };
        Some(CycleConfig {
            max_iterations: max_iter,
            guard,
            delay,
            no_converge,
            restart_on_failure: !no_restart_on_failure,
            max_failure_restarts,
        })
    } else {
        if cycle_guard.is_some() || cycle_delay.is_some() {
            anyhow::bail!("--cycle-guard and --cycle-delay require --max-iterations");
        }
        if no_converge {
            anyhow::bail!("--no-converge requires --max-iterations");
        }
        if no_restart_on_failure || max_failure_restarts.is_some() {
            anyhow::bail!(
                "--no-restart-on-failure and --max-failure-restarts require --max-iterations"
            );
        }
        None
    };

    // Compute not_before from --delay or --not-before
    if delay.is_some() && not_before.is_some() {
        anyhow::bail!("Cannot specify both --delay and --not-before");
    }
    let computed_not_before = if let Some(d) = delay {
        let secs = parse_delay(d).ok_or_else(|| {
            anyhow::anyhow!("Invalid delay '{}'. Use format: 30s, 5m, 1h, 24h, 7d", d)
        })?;
        Some((Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339())
    } else if let Some(ts) = not_before {
        ts.parse::<chrono::DateTime<Utc>>()
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S")
                    .map(|ndt| ndt.and_utc())
            })
            .map_err(|_| anyhow::anyhow!("Invalid timestamp '{}'. Use ISO 8601 format", ts))?;
        Some(ts.to_string())
    } else {
        None
    };

    let log = if paused {
        vec![workgraph::graph::LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: "Task paused".to_string(),
        }]
    } else {
        vec![]
    };

    let task = Task {
        id: task_id.clone(),
        title: title.to_string(),
        description: description.map(String::from),
        status: Status::Open,
        assigned: assign.map(String::from),
        estimate,
        before: vec![],
        after: after.to_vec(),
        requires: vec![],
        tags: tags.to_vec(),
        skills: skills.to_vec(),
        inputs: inputs.to_vec(),
        deliverables: deliverables.to_vec(),
        artifacts: vec![],
        exec: None,
        not_before: computed_not_before,
        created_at: Some(Utc::now().to_rfc3339()),
        started_at: None,
        completed_at: None,
        log,
        retry_count: 0,
        max_retries,
        failure_reason: None,
        model: model.map(String::from),
        provider: provider.map(String::from),
        verify: verify.map(String::from),
        agent: None,
        loop_iteration: 0,
        cycle_failure_restarts: 0,
        cycle_config,
        ready_after: None,
        paused,
        visibility: visibility.to_string(),
        context_scope: context_scope.map(String::from),
        exec_mode: exec_mode.map(String::from),
        token_usage: None,
        session_id: None,
        wait_condition: None,
        checkpoint: None,
        resurrection_count: 0,
        last_resurrected_at: None,
            validation: None,
            validation_commands: vec![],
            test_required: false,
            rejection_count: 0,
            max_rejections: None,
        superseded_by: vec![],
        supersedes: None,
    };

    // Add task to graph
    graph.add_node(Node::Task(task));

    // Maintain bidirectional consistency: update `blocks` on referenced blocker tasks
    // (skip cross-repo refs — those live in a different graph)
    for dep in after {
        if workgraph::federation::parse_remote_ref(dep).is_some() {
            continue; // Cross-repo dep; can't update remote graph's blocks field
        }
        if let Some(blocker) = graph.get_task_mut(dep)
            && !blocker.before.contains(&task_id)
        {
            blocker.before.push(task_id.clone());
        }
    }

    // Auto-create back-edges when --max-iterations is set and --after deps exist.
    // For each --after dep, add the new task's ID to the dep's after list,
    // forming a structural cycle that the SCC detector will find.
    if max_iterations.is_some() && !after.is_empty() {
        for dep_id in after {
            if workgraph::federation::parse_remote_ref(dep_id).is_some() {
                continue; // Skip cross-repo deps
            }
            if let Some(dep_task) = graph.get_task_mut(dep_id)
                && !dep_task.after.contains(&task_id)
            {
                dep_task.after.push(task_id.clone());
            }
            // Maintain bidirectional consistency for the back-edge
            if let Some(new_task) = graph.get_task_mut(&task_id)
                && !new_task.before.contains(dep_id)
            {
                new_task.before.push(dep_id.clone());
            }
        }
    }

    // Save atomically (temp file + rename)
    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);
    super::notify_new_task_focus(dir, &task_id);

    // Record operation (include agent_id if running in agent context for guardrail tracking)
    let mut detail = serde_json::json!({ "title": title });
    if let Some(ref aid) = agent_id {
        detail["agent_id"] = serde_json::Value::String(aid.clone());
    }
    let _ = workgraph::provenance::record(
        dir,
        "add_task",
        Some(&task_id),
        assign,
        detail,
        config.log.rotation_threshold,
    );

    if paused {
        println!("Added task (draft): {} ({})", title, task_id);
        println!(
            "  Task is paused (draft mode). When ready, run: wg publish {}",
            task_id
        );
    } else {
        println!("Added task: {} ({})", title, task_id);
    }
    if id.is_none() {
        println!("  Use --after {} to depend on this task", task_id);
    }
    super::print_service_hint(dir);
    Ok(())
}

/// Add a task to a remote peer workgraph.
///
/// Dispatch order (per §3.2 of cross-repo design doc):
/// 1. Resolve peer to a .workgraph directory
/// 2. If peer service is running → send AddTask IPC request
/// 3. If not running → directly modify the peer's graph.jsonl
/// 4. Print the created task ID with peer prefix
#[allow(clippy::too_many_arguments)]
pub fn run_remote(
    local_workgraph_dir: &Path,
    peer_ref: &str,
    title: &str,
    id: Option<&str>,
    description: Option<&str>,
    after: &[String],
    tags: &[String],
    skills: &[String],
    deliverables: &[String],
    model: Option<&str>,
    provider: Option<&str>,
    verify: Option<&str>,
) -> Result<()> {
    use workgraph::federation::{check_peer_service, resolve_peer};

    if title.trim().is_empty() {
        anyhow::bail!("Task title cannot be empty");
    }

    // Resolve peer reference to a concrete .workgraph directory
    let resolved = resolve_peer(peer_ref, local_workgraph_dir)?;

    // Build origin string for provenance
    let origin = local_workgraph_dir
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Check if peer service is running
    let peer_status = check_peer_service(&resolved.workgraph_dir);

    if peer_status.running {
        // Dispatch via IPC
        let request = super::service::IpcRequest::AddTask {
            title: title.to_string(),
            id: id.map(String::from),
            description: description.map(String::from),
            after: after.to_vec(),
            tags: tags.to_vec(),
            skills: skills.to_vec(),
            deliverables: deliverables.to_vec(),
            model: model.map(String::from),
            verify: verify.map(String::from),
            origin: Some(origin),
        };

        let response = super::service::send_request(&resolved.workgraph_dir, &request)?;

        if response.ok {
            let task_id = response
                .data
                .as_ref()
                .and_then(|d| d.get("task_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            println!(
                "Added task to '{}': {} ({}:{})",
                peer_ref, title, peer_ref, task_id
            );
        } else {
            let err = response
                .error
                .unwrap_or_else(|| "unknown error".to_string());
            anyhow::bail!("Remote add failed: {}", err);
        }
    } else {
        // Fallback: directly modify the peer's graph.jsonl
        let task_id = add_task_directly(
            &resolved.workgraph_dir,
            title,
            id,
            description,
            after,
            tags,
            skills,
            deliverables,
            model,
            provider,
            verify,
            &origin,
        )?;
        println!(
            "Added task to '{}' (direct): {} ({}:{})",
            peer_ref, title, peer_ref, task_id
        );
    }

    Ok(())
}

/// Add a task directly to a peer's graph.jsonl (fallback when service is not running).
#[allow(clippy::too_many_arguments)]
fn add_task_directly(
    peer_workgraph_dir: &Path,
    title: &str,
    id: Option<&str>,
    description: Option<&str>,
    after: &[String],
    tags: &[String],
    skills: &[String],
    deliverables: &[String],
    model: Option<&str>,
    provider: Option<&str>,
    verify: Option<&str>,
    origin: &str,
) -> Result<String> {
    use workgraph::graph::{Node, Status, Task};
    use workgraph::parser::{load_graph, save_graph};

    let graph_path = super::graph_path(peer_workgraph_dir);
    let mut graph = if graph_path.exists() {
        load_graph(&graph_path).context("Failed to load peer graph")?
    } else {
        anyhow::bail!(
            "No graph.jsonl at '{}'. Is this a workgraph project?",
            peer_workgraph_dir.display()
        );
    };

    let task_id = match id {
        Some(id) => {
            if graph.get_node(id).is_some() {
                anyhow::bail!("Task with ID '{}' already exists in peer", id);
            }
            id.to_string()
        }
        None => generate_id(title, &graph),
    };

    let task = Task {
        id: task_id.clone(),
        title: title.to_string(),
        description: description.map(String::from),
        status: Status::Open,
        assigned: None,
        estimate: None,
        before: vec![],
        after: after.to_vec(),
        requires: vec![],
        tags: tags.to_vec(),
        skills: skills.to_vec(),
        inputs: vec![],
        deliverables: deliverables.to_vec(),
        artifacts: vec![],
        exec: None,
        not_before: None,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries: None,
        failure_reason: None,
        model: model.map(String::from),
        provider: provider.map(String::from),
        verify: verify.map(String::from),
        agent: None,
        loop_iteration: 0,
        cycle_failure_restarts: 0,
        ready_after: None,
        paused: false,
        visibility: "internal".to_string(),
        context_scope: None,
        cycle_config: None,
        exec_mode: None,
        token_usage: None,
        session_id: None,
        wait_condition: None,
        checkpoint: None,
        resurrection_count: 0,
        last_resurrected_at: None,
            validation: None,
            validation_commands: vec![],
            test_required: false,
            rejection_count: 0,
            max_rejections: None,
        superseded_by: vec![],
        supersedes: None,
    };

    graph.add_node(Node::Task(task));

    // Maintain bidirectional after/blocks consistency
    for dep in after {
        if let Some(blocker) = graph.get_task_mut(dep)
            && !blocker.before.contains(&task_id)
        {
            blocker.before.push(task_id.clone());
        }
    }

    save_graph(&graph, &graph_path).context("Failed to save peer graph")?;

    // Record provenance in the peer's workgraph
    let config = workgraph::config::Config::load_or_default(peer_workgraph_dir);
    let _ = workgraph::provenance::record(
        peer_workgraph_dir,
        "add_task",
        Some(&task_id),
        None,
        serde_json::json!({ "title": title, "origin": origin, "remote": true }),
        config.log.rotation_threshold,
    );

    Ok(task_id)
}

/// Count how many tasks the given agent has created, by scanning the provenance log
/// for `add_task` operations with a matching `agent_id` in the detail.
fn count_agent_created_tasks(dir: &Path, agent_id: &str) -> u32 {
    let entries = match workgraph::provenance::read_all_operations(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    entries
        .iter()
        .filter(|e| {
            e.op == "add_task"
                && (e.detail.get("agent_id").and_then(|v| v.as_str()) == Some(agent_id))
        })
        .count() as u32
}

fn generate_id(title: &str, graph: &workgraph::WorkGraph) -> String {
    // Generate a slug from the title: take up to 3 non-numeric words,
    // plus any trailing numeric tokens (so "task 1" -> "task-1", not "task").
    let normalized: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let all_tokens: Vec<String> = normalized
        .split('-')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    // Take up to 3 non-numeric words, plus any numeric tokens that appear
    // before or immediately after the last included word.
    let mut result: Vec<&str> = Vec::new();
    let mut word_count = 0;
    for token in &all_tokens {
        let is_numeric = token.chars().all(|c| c.is_ascii_digit());
        if !is_numeric && word_count < 3 {
            result.push(token);
            word_count += 1;
        } else if is_numeric && word_count <= 3 {
            result.push(token);
        } else {
            break;
        }
    }
    let slug = result.join("-");

    let base_id = if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    };

    // Ensure uniqueness
    if graph.get_node(&base_id).is_none() {
        return base_id;
    }

    for i in 2..1000 {
        let candidate = format!("{}-{}", base_id, i);
        if graph.get_node(&candidate).is_none() {
            return candidate;
        }
    }

    // Fallback to timestamp
    format!(
        "task-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::WorkGraph;
    use workgraph::graph::{LoopGuard, Node, Status, Task};

    /// Helper: create a minimal task with the given ID for inserting into a WorkGraph.
    fn stub_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            ..Task::default()
        }
    }

    // ---- parse_guard_expr tests ----

    #[test]
    fn guard_always_lowercase() {
        let g = parse_guard_expr("always").unwrap();
        assert_eq!(g, LoopGuard::Always);
    }

    #[test]
    fn guard_always_mixed_case() {
        let g = parse_guard_expr("Always").unwrap();
        assert_eq!(g, LoopGuard::Always);
    }

    #[test]
    fn guard_always_uppercase() {
        let g = parse_guard_expr("ALWAYS").unwrap();
        assert_eq!(g, LoopGuard::Always);
    }

    #[test]
    fn guard_always_with_whitespace() {
        let g = parse_guard_expr("  always  ").unwrap();
        assert_eq!(g, LoopGuard::Always);
    }

    #[test]
    fn guard_task_status_done() {
        let g = parse_guard_expr("task:my-task=done").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "my-task".to_string(),
                status: Status::Done,
            }
        );
    }

    #[test]
    fn guard_task_status_open() {
        let g = parse_guard_expr("task:build-step=open").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "build-step".to_string(),
                status: Status::Open,
            }
        );
    }

    #[test]
    fn guard_task_status_failed() {
        let g = parse_guard_expr("task:deploy=failed").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "deploy".to_string(),
                status: Status::Failed,
            }
        );
    }

    #[test]
    fn guard_task_status_abandoned() {
        let g = parse_guard_expr("task:cleanup=abandoned").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "cleanup".to_string(),
                status: Status::Abandoned,
            }
        );
    }

    #[test]
    fn guard_task_status_in_progress() {
        let g = parse_guard_expr("task:long-running=in-progress").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "long-running".to_string(),
                status: Status::InProgress,
            }
        );
    }

    #[test]
    fn guard_task_status_blocked() {
        let g = parse_guard_expr("task:waiting=blocked").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "waiting".to_string(),
                status: Status::Blocked,
            }
        );
    }

    #[test]
    fn guard_task_status_pending_review_maps_to_done() {
        let g = parse_guard_expr("task:pr-check=pending-review").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "pr-check".to_string(),
                status: Status::Done,
            }
        );
    }

    #[test]
    fn guard_task_status_case_insensitive() {
        let g = parse_guard_expr("task:check=Done").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "check".to_string(),
                status: Status::Done,
            }
        );
    }

    #[test]
    fn guard_task_id_with_underscores() {
        let g = parse_guard_expr("task:my_task_id=done").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "my_task_id".to_string(),
                status: Status::Done,
            }
        );
    }

    #[test]
    fn guard_task_id_with_dashes() {
        let g = parse_guard_expr("task:my-task-id=open").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "my-task-id".to_string(),
                status: Status::Open,
            }
        );
    }

    #[test]
    fn guard_unknown_status_errors() {
        let result = parse_guard_expr("task:foo=bogus");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Unknown status"), "got: {msg}");
    }

    #[test]
    fn guard_missing_equals_errors() {
        let result = parse_guard_expr("task:foo");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Invalid guard format"), "got: {msg}");
    }

    #[test]
    fn guard_missing_colon_errors() {
        let result = parse_guard_expr("taskfoo=done");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Invalid guard expression"), "got: {msg}");
    }

    #[test]
    fn guard_empty_string_errors() {
        let result = parse_guard_expr("");
        assert!(result.is_err());
    }

    #[test]
    fn guard_whitespace_only_errors() {
        let result = parse_guard_expr("   ");
        assert!(result.is_err());
    }

    // ---- generate_id tests ----

    #[test]
    fn id_slug_from_simple_title() {
        let graph = WorkGraph::new();
        let id = generate_id("Build the widget", &graph);
        assert_eq!(id, "build-the-widget");
    }

    #[test]
    fn id_slug_truncates_to_three_words() {
        let graph = WorkGraph::new();
        let id = generate_id("Build the amazing super widget", &graph);
        assert_eq!(id, "build-the-amazing");
    }

    #[test]
    fn id_slug_strips_special_chars() {
        let graph = WorkGraph::new();
        let id = generate_id("Fix (bug) #123!", &graph);
        assert_eq!(id, "fix-bug-123");
    }

    #[test]
    fn id_slug_collapses_multiple_separators() {
        let graph = WorkGraph::new();
        let id = generate_id("a---b   c", &graph);
        assert_eq!(id, "a-b-c");
    }

    #[test]
    fn id_slug_includes_trailing_number() {
        let graph = WorkGraph::new();
        let id = generate_id("Smoke test task 1", &graph);
        assert_eq!(id, "smoke-test-task-1");
    }

    #[test]
    fn id_slug_number_after_skipped_word_excluded() {
        // Numbers after a skipped (4th+) word are not included
        let graph = WorkGraph::new();
        let id = generate_id("Build the amazing widget 42", &graph);
        assert_eq!(id, "build-the-amazing");
    }

    #[test]
    fn id_slug_leading_number_not_counted_as_word() {
        let graph = WorkGraph::new();
        let id = generate_id("123 fix the bug", &graph);
        assert_eq!(id, "123-fix-the-bug");
    }

    #[test]
    fn id_slug_empty_title_gives_task() {
        let graph = WorkGraph::new();
        let id = generate_id("", &graph);
        assert_eq!(id, "task");
    }

    #[test]
    fn id_slug_whitespace_title_gives_task() {
        let graph = WorkGraph::new();
        let id = generate_id("   ", &graph);
        assert_eq!(id, "task");
    }

    #[test]
    fn id_uniqueness_appends_suffix() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(stub_task("build-it")));
        let id = generate_id("Build it", &graph);
        assert_eq!(id, "build-it-2");
    }

    #[test]
    fn id_uniqueness_increments_until_free() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(stub_task("build-it")));
        graph.add_node(Node::Task(stub_task("build-it-2")));
        graph.add_node(Node::Task(stub_task("build-it-3")));
        let id = generate_id("Build it", &graph);
        assert_eq!(id, "build-it-4");
    }

    #[test]
    fn id_explicit_no_collision() {
        // When an explicit id is provided, generate_id is not called;
        // but the run() function checks uniqueness. Verify generate_id
        // returns the base slug when no collision exists.
        let graph = WorkGraph::new();
        let id = generate_id("Deploy service", &graph);
        assert_eq!(id, "deploy-service");
    }

    #[test]
    fn empty_title_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path();
        // Initialize a workgraph
        std::fs::create_dir_all(dir_path).unwrap();
        let path = super::graph_path(dir_path);
        let graph = WorkGraph::new();
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(
            dir_path,
            "",
            None,
            None,
            &[],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            false,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn whitespace_only_title_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path();
        std::fs::create_dir_all(dir_path).unwrap();
        let path = super::graph_path(dir_path);
        let graph = WorkGraph::new();
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(
            dir_path,
            "   ",
            None,
            None,
            &[],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            false,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn self_blocking_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path();
        std::fs::create_dir_all(dir_path).unwrap();
        let path = super::graph_path(dir_path);
        let graph = WorkGraph::new();
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(
            dir_path,
            "My task",
            Some("my-task"),
            None,
            &["my-task".to_string()], // self-reference
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            false,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot block itself"),
            "Expected 'cannot block itself' error"
        );
    }

    #[test]
    fn nonexistent_blocker_warns_but_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path();
        std::fs::create_dir_all(dir_path).unwrap();
        let path = super::graph_path(dir_path);
        let graph = WorkGraph::new();
        workgraph::parser::save_graph(&graph, &path).unwrap();

        // Should succeed (with a warning to stderr) — nonexistent blockers are allowed
        let result = run(
            dir_path,
            "My task",
            None,
            None,
            &["nonexistent".to_string()],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            false,
            None,
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn after_updates_blocker_blocks_field() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path();
        std::fs::create_dir_all(dir_path).unwrap();
        let path = super::graph_path(dir_path);

        // Create a graph with an existing blocker task
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(stub_task("blocker-a")));
        graph.add_node(Node::Task(stub_task("blocker-b")));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        // Add a new task blocked by both blockers
        let result = run(
            dir_path,
            "Dependent task",
            Some("dep-task"),
            None,
            &["blocker-a".to_string(), "blocker-b".to_string()],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            false,
            None,
            None,
        );
        assert!(result.is_ok());

        // Reload graph and verify symmetry
        let graph = load_graph(&path).unwrap();

        // The new task should have after set
        let dep = graph.get_task("dep-task").unwrap();
        assert!(dep.after.contains(&"blocker-a".to_string()));
        assert!(dep.after.contains(&"blocker-b".to_string()));

        // Each blocker should have the new task in its blocks field
        let a = graph.get_task("blocker-a").unwrap();
        assert!(
            a.before.contains(&"dep-task".to_string()),
            "blocker-a.before should contain dep-task, got: {:?}",
            a.before
        );

        let b = graph.get_task("blocker-b").unwrap();
        assert!(
            b.before.contains(&"dep-task".to_string()),
            "blocker-b.before should contain dep-task, got: {:?}",
            b.before
        );
    }
}
