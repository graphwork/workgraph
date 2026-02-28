//! Coordinator tick logic: task readiness, auto-assign, auto-evaluate, agent spawning.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::Path;

use workgraph::agency;
use workgraph::agency::run_mode::{self, AssignmentPath};
use workgraph::agency::{
    AssignerModeContext, AssignmentMode, TaskAssignmentRecord,
    render_assigner_mode_context, count_assignment_records,
    find_cached_agent, save_assignment_record,
    load_agent, load_role, load_tradeoff,
    render_identity_prompt_rich, resolve_all_components, resolve_outcome,
};
use workgraph::config::Config;
use workgraph::graph::{LogEntry, Node, Status, Task};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks_with_peers_cycle_aware;
use workgraph::service::registry::AgentRegistry;

use crate::commands::{graph_path, is_process_alive, spawn};
use super::triage;

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

/// Auto-assign: build assignment subgraph for unassigned ready tasks.
///
/// Per the agency design (§4, §10), when auto_assign is enabled and a ready
/// task has no agent field, the coordinator creates a blocking assignment task
/// `assign-{task-id}` BEFORE spawning any agents.  The assigner agent is then
/// spawned on the assignment task, inspects the agency via wg CLI, and calls
/// `wg assign <task-id> <agent-hash>` followed by `wg done assign-{task-id}`.
///
/// Returns `true` if the graph was modified.
fn build_auto_assign_tasks(graph: &mut workgraph::graph::WorkGraph, config: &Config, dir: &Path) -> bool {
    let mut modified = false;

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
                )
            })
            .collect()
    };

    // Compute total assignments for run mode routing
    let agency_dir = dir.join("agency");
    let total_assignments = count_assignment_records(&agency_dir.join("assignments")) as u32;

    for (task_id, task_title, task_desc, task_skills, task_agent, task_assigned, task_tags, task_after, task_context_scope) in
        ready_task_data
    {
        // Skip tasks that already have an agent or are already claimed
        if task_agent.is_some() || task_assigned.is_some() {
            continue;
        }

        // Skip tasks tagged with assignment/evaluation/evolution/org-evaluation
        // to prevent infinite regress (assign-assign-assign-...)
        let dominated_tags = ["assignment", "evaluation", "evolution", "org-evaluation"];
        if task_tags
            .iter()
            .any(|tag| dominated_tags.contains(&tag.as_str()))
        {
            continue;
        }

        let assign_task_id = format!("assign-{}", task_id);

        // Skip if assignment task already exists (idempotent)
        if graph.get_task(&assign_task_id).is_some() {
            continue;
        }

        // Determine assignment path via run mode continuum
        let rng_value: f64 = {
            // Simple deterministic pseudo-random from task_id hash to avoid
            // requiring rand crate. Provides adequate entropy for routing.
            let hash = task_id.bytes().fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            (hash % 10000) as f64 / 10000.0
        };
        let assignment_path = run_mode::determine_assignment_path(
            &config.agency,
            total_assignments,
            rng_value,
        );

        // Build mode-specific context for the assigner
        let experiment = match assignment_path {
            AssignmentPath::Learning | AssignmentPath::ForcedExploration => {
                let learning_count = count_assignment_records(&agency_dir.join("assignments")) as u32;
                Some(run_mode::design_experiment(&agency_dir, &config.agency, learning_count))
            }
            AssignmentPath::Performance => None,
        };

        let cached_agents: Vec<(String, f64)> = if assignment_path == AssignmentPath::Performance {
            // Gather top cached agents for the performance mode context
            let agents_dir = agency_dir.join("cache/agents");
            let mut agents_with_scores: Vec<(String, f64)> = agency::load_all_agents_or_warn(&agents_dir)
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
            agents_with_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            agents_with_scores.truncate(5); // Top 5
            agents_with_scores
        } else {
            vec![]
        };

        let effective_rate = config.agency.run_mode.max(config.agency.min_exploration_rate);
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
        let is_underspecified = task_desc.is_none()
            || task_desc.as_ref().map(|d| d.len() < 20).unwrap_or(true);
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

        // Resolve assigner agent identity (if configured via assigner_agent hash)
        let assigner_identity = config.agency.assigner_agent.as_ref().and_then(|agent_hash| {
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
            Some(render_identity_prompt_rich(&role, &tradeoff, &resolved_skills, outcome.as_ref()))
        });

        // Build description for the assigner with the original task's context
        let mut desc = String::new();
        // Prepend agent identity when composed assigner is available
        if let Some(ref identity) = assigner_identity {
            desc.push_str(identity);
            desc.push_str("\n\n");
        }
        desc.push_str(&format!(
            "Assign an agent to task '{}'.\n\n## Original Task\n**Title:** {}\n",
            task_id, task_title,
        ));
        if let Some(ref d) = task_desc {
            desc.push_str(&format!("**Description:** {}\n", d));
        }
        if !task_skills.is_empty() {
            desc.push_str(&format!("**Skills:** {}\n", task_skills.join(", ")));
        }
        if !task_after.is_empty() {
            desc.push_str(&format!("**Dependencies ({}):** {}\n", task_after.len(), task_after.join(", ")));
        }
        if let Some(ref scope) = task_context_scope {
            desc.push_str(&format!("**Context scope (pre-set):** {}\n", scope));
        }
        if let Some(ref warning) = underspec_warning {
            desc.push_str(warning);
        }

        // Include run mode context so the assigner knows which path to follow
        desc.push_str(&format!("\n{}\n", mode_context));

        desc.push_str(&format!(
            "\n## Instructions\n\n\
             Use the assignment mode context above to guide your decision.\n\n\
             ### Step 1: Gather Information\n\n\
             Run these commands to understand the available agents and their track records:\n\
             ```\n\
             wg agent list --json\n\
             wg role list --json\n\
             wg motivation list --json\n\
             ```\n\n\
             For agents with evaluation history, drill into performance details:\n\
             ```\n\
             wg agent performance <agent-hash> --json\n\
             ```\n\n\
             ### Step 2: Follow Assignment Path\n\n\
             The mode context above specifies which assignment path to follow:\n\n\
             - **Performance (cache-first)**: Pick the highest-scoring cached agent \
             whose skills match the task. Do NOT vary composition dimensions — \
             deterministic selection only. If no cached agents meet the threshold, \
             fall back to best-guess role+motivation matching.\n\n\
             - **Learning (structured experiment)**: The experiment specification above \
             tells you which composition dimension to vary. Compose a new agent by \
             applying the experiment (e.g., swap a role component) using UCB1-selected \
             primitives. Use `wg agent create` if a matching agent doesn't exist yet.\n\n\
             - **Forced Exploration**: Try novel or unconventional agent compositions. \
             Combine roles and motivations that haven't been paired before. Maximise \
             diversity of signal.\n\n\
             ### Step 3: Match Agent to Task\n\n\
             Compare each agent's capabilities to the task requirements:\n\n\
             1. **Role fit**: The agent's role skills should overlap with the task's \
             required skills. A Programmer (code-writing, testing, debugging) fits \
             implementation tasks; a Reviewer (code-review, security-audit) fits review \
             tasks; an Architect (system-design, dependency-analysis) fits design tasks; \
             a Documenter (technical-writing) fits documentation tasks.\n\n\
             2. **Motivation fit**: The agent's operational parameters should match the \
             task's nature. A Careful agent suits tasks where correctness is critical. \
             A Fast agent suits urgent, low-risk tasks. A Thorough agent suits complex \
             tasks requiring deep analysis.\n\n\
             3. **Capabilities**: Check the agent's `capabilities` list for specific \
             technology or domain tags that match the task (e.g., \"rust\", \"python\", \
             \"kubernetes\").\n\n\
             ### Step 4: Use Performance Data\n\n\
             Each agent has a `performance` record with `task_count`, `avg_score` \
             (0.0–1.0), and individual evaluation entries. Each evaluation has \
             dimension scores: `correctness` (40% weight), `completeness` (30%), \
             `efficiency` (15%), `style_adherence` (15%).\n\n\
             - **Prefer agents with higher avg_score** on similar tasks (check \
             evaluation `task_id` and `context_id` to see what kinds of work they've \
             done before).\n\
             - **Weight recent evaluations more** — an agent's latest scores are more \
             predictive than older ones.\n\
             - **Consider dimension strengths**: If the task demands correctness above \
             all else, prefer agents who score highest on `correctness` even if their \
             overall average is slightly lower.\n\n\
             ### Step 5: Handle Cold Start\n\n\
             When agents have 0 evaluations (new agency, or new agents), you cannot \
             rely on performance data. In this case:\n\n\
             - **Match on role and motivation** — this is the primary signal. Pick the \
             agent whose role skills best cover the task requirements.\n\
             - **Spread work across untested agents** to build evaluation data. If \
             multiple agents have 0 evaluations and similar role fit, prefer whichever \
             has completed fewer tasks (lower `task_count`) so the agency gathers \
             diverse signal.\n\
             - **Default to Careful motivation** for high-stakes tasks and Fast \
             motivation for routine work when there's no data to differentiate.\n\n\
             ### Step 6: Assign\n\n\
             Once you've chosen an agent, run:\n\
             ```\n\
             wg assign {} <agent-hash>\n\
             wg done {}\n\
             ```\n\n\
             If no suitable agent exists for this task, report why:\n\
             ```\n\
             wg fail {} --reason \"No agent with matching skills for: <explanation>\"\n\
             ```\n\n\
             ### Step 6b: Set Context Scope\n\n\
             After assigning the agent, determine the appropriate context scope for \
             the task. The context scope controls how much workgraph context the \
             spawned agent receives in its prompt.\n\n\
             - **clean**: Pure computation, translation, summarization, writing tasks \
             where the agent needs no workgraph interaction. The task description is \
             self-contained input, the output is the deliverable.\n\
               Signals: task has no `after` dependencies with artifacts to inspect, \
             task skills include \"writing\", \"translation\", or \"computation\", task \
             description doesn't reference other tasks.\n\n\
             - **task** (default): Standard implementation, bug fixes, code changes, \
             test writing. The agent needs `wg` CLI for logging and completion.\n\
               Signals: most tasks. If unsure, use this.\n\n\
             - **graph**: Integration tasks, review tasks spanning multiple components, \
             tasks that join outputs from multiple parallel workers.\n\
               Signals: task has 3+ dependencies (`after` edges), task title/description \
             mentions \"integrate\", \"merge\", \"review across\", \"combine\", \"synthesize\", \
             \"harmonize\", or \"coordinate\". Task tags include \"integration\" or \"review\".\n\n\
             - **full**: Meta-tasks about workgraph itself, workflow design, debugging \
             coordination failures, writing specs about the orchestration system.\n\
               Signals: task description references workgraph internals, coordinator, \
             agency system, or \"workflow\". Task tags include \"meta\" or \"system\".\n\n\
             Set the scope (skip if `task` is appropriate, or if a scope is already pre-set \
             on the task):\n\
             ```\n\
             wg edit {} --context-scope <scope>\n\
             ```",
            task_id, assign_task_id, assign_task_id, task_id,
        ));

        // Create the assignment task (blocks the original)
        let assign_task = Task {
            id: assign_task_id.clone(),
            title: format!("Assign agent for: {}", task_title),
            description: Some(desc),
            status: Status::Open,
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
            created_at: Some(Utc::now().to_rfc3339()),
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: config.agency.assigner_model.clone(),
            verify: None,
            agent: config.agency.assigner_agent.clone(),

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            cycle_config: None,
            token_usage: None,
        exec_mode: None,
        };

        graph.add_node(Node::Task(assign_task));

        // Add the assignment task as a blocker on the original task
        if let Some(t) = graph.get_task_mut(&task_id)
            && !t.after.contains(&assign_task_id)
        {
            t.after.push(assign_task_id.clone());
        }

        // Persist preliminary TaskAssignmentRecord with the chosen mode.
        // agent_id will be "pending" until `wg assign` fills it in.
        let assignment_mode = match assignment_path {
            AssignmentPath::Performance => {
                // Check if there's a cached agent above threshold
                match find_cached_agent(&agency_dir, config.agency.performance_threshold) {
                    Some((_, score)) => AssignmentMode::CacheHit { cache_score: score },
                    None => AssignmentMode::CacheMiss,
                }
            }
            AssignmentPath::Learning => {
                // experiment is always Some for Learning path
                AssignmentMode::Learning(experiment.clone().expect("experiment required for Learning path"))
            }
            AssignmentPath::ForcedExploration => {
                AssignmentMode::ForcedExploration(experiment.clone().expect("experiment required for ForcedExploration path"))
            }
        };

        let record = TaskAssignmentRecord {
            task_id: task_id.clone(),
            agent_id: "pending".to_string(),
            composition_id: "pending".to_string(),
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
            "[coordinator] Created assignment task '{}' blocking '{}'",
            assign_task_id, task_id,
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
    // a reflection of a role+motivation prompt so we skip auto-evaluation.
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
            let eval_id = format!("evaluate-{}", t.id);
            if graph.get_task(&eval_id).is_some() {
                return false;
            }
            // Skip tasks tagged with evaluation/assignment/evolution
            let dominated_tags = ["evaluation", "assignment", "evolution", "org-evaluation"];
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
    let evaluator_identity = config.agency.evaluator_agent.as_ref().and_then(|agent_hash| {
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
        Some(render_identity_prompt_rich(&role, &tradeoff, &resolved_skills, outcome.as_ref()))
    });

    for (task_id, task_title) in &tasks_needing_eval {
        let eval_task_id = format!("evaluate-{}", task_id);

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
            model: config.agency.evaluator_model.clone(),
            verify: None,
            agent: config.agency.evaluator_agent.clone(),

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            cycle_config: None,
        exec_mode: None,
            token_usage: None,
        };

        graph.add_node(Node::Task(eval_task));

        // Tag the source task so we never recreate the eval task after gc.
        if let Some(source) = graph.get_task_mut(task_id) {
            if !source.tags.iter().any(|t| t == "eval-scheduled") {
                source.tags.push("eval-scheduled".to_string());
            }
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
        .filter(|t| t.id.starts_with("evaluate-") && t.status == Status::Open)
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
    let mut graph = load_graph(&graph_path).context("Failed to load graph for eval spawn")?;

    let task = graph.get_task_mut_or_err(eval_task_id)?;
    if task.status != Status::Open {
        anyhow::bail!("Eval task '{}' is not open (status: {:?})", eval_task_id, task.status);
    }

    // Use the task's exec command directly if it starts with "wg evaluate".
    // This handles both "wg evaluate run <task>" and "wg evaluate org <task>".
    // Fall back to reconstructing from task ID for backward compatibility.
    let eval_cmd = if let Some(exec) = task.exec.as_deref()
        && exec.starts_with("wg evaluate")
    {
        exec.to_string()
    } else {
        let source_task_id = eval_task_id
            .strip_prefix("evaluate-")
            .unwrap_or(eval_task_id);
        format!("wg evaluate run '{}'", source_task_id.replace('\'', "'\\''"))
    };

    // Resolve the special agent (evaluator) hash for performance recording.
    // After the inline eval completes, we record an Evaluation against this
    // agent so it accumulates performance history like any other agent.
    let config = Config::load_or_default(dir);
    let special_agent_hash = task.agent.clone()
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
        agency::find_agent_by_prefix(&agents_dir, hash).ok().map(|a| a.id)
    });

    // Single script: run eval, record special agent perf, then mark done/failed
    let script = if let Some(ref sa_id) = special_agent_verified {
        let escaped_sa_id = sa_id.replace('\'', "'\\''");
        format!(
            r#"unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT
{eval_cmd} >> '{escaped_output}' 2>&1
EXIT_CODE=$?
if [ $EXIT_CODE -eq 0 ]; then
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
            r#"unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT
{eval_cmd} >> '{escaped_output}' 2>&1
EXIT_CODE=$?
if [ $EXIT_CODE -eq 0 ]; then
    wg done '{escaped_eval_id}' 2>> '{escaped_output}'
else
    wg fail '{escaped_eval_id}' --reason "wg evaluate exited with code $EXIT_CODE" 2>> '{escaped_output}'
fi
exit $EXIT_CODE"#,
        )
    };

    // Claim the task before spawning
    task.status = Status::InProgress;
    task.started_at = Some(Utc::now().to_rfc3339());
    task.assigned = Some(agent_id.clone());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(agent_id.clone()),
        message: format!(
            "Spawned eval inline{}",
            evaluator_model
                .map(|m| format!(" --model {}", m))
                .unwrap_or_default()
        ),
    });
    save_graph(&graph, &graph_path).context("Failed to save graph after claiming eval task")?;

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
            // Rollback the claim
            if let Ok(mut rollback_graph) = load_graph(&graph_path)
                && let Some(t) = rollback_graph.get_task_mut(eval_task_id) {
                    t.status = Status::Open;
                    t.started_at = None;
                    t.assigned = None;
                    t.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: Some(agent_id.clone()),
                        message: format!("Eval spawn failed, reverting claim: {}", e),
                    });
                    let _ = save_graph(&rollback_graph, &graph_path);
                }
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
    agent_registry.save(dir).context("Failed to save agent registry after eval spawn")?;

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
) -> usize {
    let cycle_analysis = graph.compute_cycle_analysis();
    let final_ready = ready_tasks_with_peers_cycle_aware(graph, dir, &cycle_analysis);
    let agents_dir = dir.join("agency").join("cache/agents");
    let mut spawned = 0;

    let to_spawn = final_ready.iter().take(slots_available);
    for task in to_spawn {
        // Skip if already claimed
        if task.assigned.is_some() {
            continue;
        }

        // Evaluation tasks run inline: fork `wg evaluate`
        // directly instead of going through the full spawn machinery
        // (run.sh, executor config, etc.)
        let is_eval_task = task.tags.iter().any(|t| t == "evaluation")
            && task.exec.is_some();
        if is_eval_task {
            let eval_model = task.model.as_deref();
            eprintln!(
                "[coordinator] Spawning eval inline for: {} - {}{}",
                task.id,
                task.title,
                eval_model.map(|m| format!(" (model: {})", m)).unwrap_or_default(),
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

        // Resolve executor: tasks with exec commands use shell executor directly,
        // otherwise: agent.executor > config.coordinator.executor
        let effective_executor = if task.exec.is_some() {
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

    // Phase 1: Clean up dead agents and count alive ones
    let alive_count = match cleanup_and_count_alive(dir, &graph_path, max_agents)? {
        Ok(count) => count,
        Err(early_result) => return Ok(early_result),
    };

    // Phase 2: Load graph
    let mut graph = load_graph(&graph_path).context("Failed to load graph")?;

    let slots_available = max_agents.saturating_sub(alive_count);

    // Phase 3: Auto-assign unassigned ready tasks
    // NOTE: These must run BEFORE the early-return check, because they may
    // create new ready tasks (e.g. evaluate-* tasks) that weren't there before.
    let mut graph_modified = false;
    if config.agency.auto_assign {
        graph_modified |= build_auto_assign_tasks(&mut graph, &config, dir);
    }

    // Phase 4: Auto-evaluate tasks
    if config.agency.auto_evaluate {
        graph_modified |= build_auto_evaluate_tasks(dir, &mut graph, &config);
    }

    // Save graph once if it was modified during auto-assign or auto-evaluate.
    // Abort tick if save fails — continuing with unsaved state would spawn agents
    // on tasks that haven't been persisted.
    if graph_modified {
        save_graph(&graph, &graph_path)
            .context("Failed to save graph after auto-assign/auto-evaluate; aborting tick")?;
    }

    // Phase 5: Check for ready tasks (after agency phases may have created new ones)
    if let Some(early_result) = check_ready_or_return(&graph, alive_count, dir) {
        return Ok(early_result);
    }

    // Phase 6: Spawn agents on ready tasks
    let cycle_analysis = graph.compute_cycle_analysis();
    let final_ready = ready_tasks_with_peers_cycle_aware(&graph, dir, &cycle_analysis);
    let ready_count = final_ready.len();
    drop(final_ready);
    let spawned = spawn_agents_for_ready_tasks(dir, &graph, executor, model, slots_available);

    Ok(TickResult {
        agents_alive: alive_count + spawned,
        tasks_ready: ready_count,
        agents_spawned: spawned,
    })
}

#[cfg(test)]
mod tests {
    use workgraph::graph::Task;

    #[test]
    fn test_eval_inline_extracts_source_task_from_exec() {
        // spawn_eval_inline extracts the source task ID from exec command
        // This tests the extraction logic used in the function
        let exec = Some("wg evaluate run my-source-task".to_string());
        let source_id = exec
            .as_deref()
            .and_then(|e| e.strip_prefix("wg evaluate run ").or_else(|| e.strip_prefix("wg evaluate ")))
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
            .and_then(|e| e.strip_prefix("wg evaluate run ").or_else(|| e.strip_prefix("wg evaluate ")))
            .unwrap_or_else(|| {
                eval_task_id
                    .strip_prefix("evaluate-")
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
}
