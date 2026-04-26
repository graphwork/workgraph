//! Eager lifecycle-task scaffolding.
//!
//! Creates `.assign-<task>`, `.flip-<task>`, and `.evaluate-<task>` tasks at
//! publish time so every published task has a full lifecycle chain as real graph
//! edges, wired atomically in one graph write:
//!
//! ```text
//! .assign-foo → foo → .flip-foo → .evaluate-foo
//! ```
//!
//! Note: placement (dependency edge decisions) is now merged into the assignment
//! step — no separate `.place-*` tasks are created.

use chrono::Utc;
use std::path::Path;

use workgraph::config::Config;
use workgraph::graph::{Node, PRIORITY_DEFAULT, Priority, Status, Task, WorkGraph, lower_priority};

/// Tags that mark tasks as part of the evaluation/assignment infrastructure.
/// Tasks with these tags do not get their own eval tasks (no meta-evaluation).
const DOMINATED_TAGS: &[&str] = &["evaluation", "assignment", "flip", "placement"];

/// System task prefixes that are eligible for the full agency pipeline.
/// These tasks go through placement, assignment, FLIP, and evaluation like
/// regular tasks — unlike other system tasks (`.evaluate-*`, `.assign-*`,
/// `.flip-*`) which are infrastructure and skip the pipeline.
const PIPELINE_ELIGIBLE_PREFIXES: &[&str] = &[".verify-"];

/// Returns true if a task uses the shell executor (command execution, no LLM).
/// Shell tasks are exempt from the agency pipeline — no .assign-*, .flip-*,
/// or .evaluate-* scaffolding.
pub fn is_shell_task(task: &Task) -> bool {
    task.exec.is_some() || task.exec_mode.as_deref() == Some("shell")
}

/// Returns true if a system task (dot-prefixed) should still go through the
/// agency pipeline. `.verify-*` tasks are the primary example: they need
/// intelligent agent matching via the same placement/assignment/evaluation
/// chain as regular tasks.
pub fn is_pipeline_eligible_system_task(task_id: &str) -> bool {
    PIPELINE_ELIGIBLE_PREFIXES
        .iter()
        .any(|prefix| task_id.starts_with(prefix))
}

/// Calculate the automatic priority for a scaffolded task based on its parent.
///
/// Rules:
/// - .assign-* tasks: inherit parent priority (they gate the parent)
/// - .evaluate-* and .flip-* tasks: parent priority minus one level
/// - Defaults to Normal if parent priority cannot be determined
fn calculate_auto_priority(
    graph: &WorkGraph,
    parent_task_id: &str,
    scaffolding_type: &str,
) -> Priority {
    let parent_task = match graph.get_task(parent_task_id) {
        Some(task) => task,
        None => return PRIORITY_DEFAULT,
    };

    let parent_priority = parent_task.priority;

    match scaffolding_type {
        "assign" => parent_priority,
        "evaluate" | "flip" => lower_priority(parent_priority),
        _ => PRIORITY_DEFAULT,
    }
}

/// Returns true if FLIP should run for a given task, based on global config
/// and the task's `flip-eval` tag.
fn should_run_flip(graph: &WorkGraph, task_id: &str, config: &Config) -> bool {
    let source_has_flip_tag = graph
        .get_task(task_id)
        .map(|t| t.tags.iter().any(|tag| tag == "flip-eval"))
        .unwrap_or(false);
    config.agency.flip_enabled || source_has_flip_tag
}

/// Create a `.flip-<task_id>` task in `graph`, blocked by `task_id`.
///
/// Returns `true` if the graph was modified (i.e. the flip task was created).
/// Idempotent: returns `false` if the flip task already exists.
pub fn scaffold_flip_task(graph: &mut WorkGraph, task_id: &str, config: &Config) -> bool {
    let flip_task_id = format!(".flip-{}", task_id);

    // Skip system tasks (unless pipeline-eligible like .verify-*)
    if workgraph::graph::is_system_task(task_id) && !is_pipeline_eligible_system_task(task_id) {
        return false;
    }

    // Idempotency: skip if flip task already exists
    if graph.get_task(&flip_task_id).is_some() {
        return false;
    }

    let flip_resolved = config.resolve_model_for_role(workgraph::config::DispatchRole::Evaluator);

    // Calculate auto-priority for flip task
    let priority = calculate_auto_priority(graph, task_id, "flip");

    let flip_task = Task {
        id: flip_task_id.clone(),
        title: format!("FLIP: {}", task_id),
        description: Some(format!(
            "Run FLIP (Fidelity via Latent Intent Probing) evaluation for task '{}'.",
            task_id,
        )),
        status: Status::Open,
        priority,
        after: vec![task_id.to_string()],
        tags: vec!["flip".to_string(), "agency".to_string()],
        exec: Some(format!("wg evaluate run {} --flip", task_id)),
        model: Some(flip_resolved.model),
        provider: flip_resolved.provider,
        exec_mode: Some("bare".to_string()),
        visibility: "internal".to_string(),
        created_at: Some(Utc::now().to_rfc3339()),
        ..Task::default()
    };

    graph.add_node(Node::Task(flip_task));

    eprintln!(
        "[eval-scaffold] Created FLIP task '{}' blocked by '{}'",
        flip_task_id, task_id,
    );

    true
}

/// Scaffold the FULL agency pipeline for a task in one pass:
///
/// ```text
/// .assign-{id} → {id} → .flip-{id} → .evaluate-{id}
/// ```
///
/// All tasks are created and all dependency edges are wired atomically
/// (caller is responsible for the single `save_graph` / `modify_graph` call).
///
/// - `.assign-*` is created if `config.agency.auto_assign` is enabled.
///   Placement (dependency edge decisions) is merged into the assignment step
///   when `config.agency.auto_place` is enabled.
/// - `.flip-*` is created if FLIP is enabled (globally or per-task tag).
/// - `.evaluate-*` is created if `config.agency.auto_evaluate` is enabled.
///
/// Returns `true` if the graph was modified.
/// Idempotent: skips tasks that already exist.
pub fn scaffold_full_pipeline(
    dir: &Path,
    graph: &mut WorkGraph,
    task_id: &str,
    task_title: &str,
    config: &Config,
) -> bool {
    // Skip system tasks (unless pipeline-eligible like .verify-*) and dominated-tag tasks
    if workgraph::graph::is_system_task(task_id) && !is_pipeline_eligible_system_task(task_id) {
        return false;
    }
    // Skip shell executor tasks — they're commands, not agent work
    if let Some(task) = graph.get_task(task_id)
        && is_shell_task(task)
    {
        return false;
    }
    if let Some(task) = graph.get_task(task_id)
        && task
            .tags
            .iter()
            .any(|tag| DOMINATED_TAGS.contains(&tag.as_str()))
    {
        return false;
    }
    // NOTE: We intentionally do NOT early-return on "eval-scheduled" here.
    // The tag may have been set by `scaffold_eval_task` (which only creates
    // .flip/.evaluate), so .place/.assign might still be missing.  Each
    // individual task creation below has its own idempotency guard.

    let assign_task_id = format!(".assign-{}", task_id);
    let flip_task_id = format!(".flip-{}", task_id);
    let eval_task_id = format!(".evaluate-{}", task_id);

    let mut any_created = false;

    // 1. Create .assign-* task (no deps — runs first via lightweight LLM call)
    // Placement (dependency edge decisions) is handled within the assignment step.
    if config.agency.auto_assign && graph.get_task(&assign_task_id).is_none() {
        let assign_task = Task {
            id: assign_task_id.clone(),
            title: format!("Assign agent for: {}", task_title),
            status: Status::Open,
            after: vec![],
            before: vec![task_id.to_string()],
            tags: vec!["assignment".to_string(), "agency".to_string()],
            exec: Some(format!("wg assign {} --auto", task_id)),
            exec_mode: Some("bare".to_string()),
            visibility: "internal".to_string(),
            created_at: Some(Utc::now().to_rfc3339()),
            ..Task::default()
        };
        graph.add_node(Node::Task(assign_task));
        any_created = true;
        eprintln!(
            "[eval-scaffold] Created assignment task '{}' blocking '{}'",
            assign_task_id, task_id,
        );
    }

    // 2. Wire main task to depend on .assign-* (so it waits for assignment)
    if graph.get_task(&assign_task_id).is_some()
        && let Some(source) = graph.get_task_mut(task_id)
        && !source.after.iter().any(|a| a == &assign_task_id)
    {
        source.after.push(assign_task_id.clone());
    }

    // 3. Create .flip-* task (depends on main task)
    let run_flip = should_run_flip(graph, task_id, config);
    if run_flip && graph.get_task(&flip_task_id).is_none() {
        let flip_resolved =
            config.resolve_model_for_role(workgraph::config::DispatchRole::Evaluator);
        let flip_task = Task {
            id: flip_task_id.clone(),
            title: format!("FLIP: {}", task_id),
            description: Some(format!(
                "Run FLIP (Fidelity via Latent Intent Probing) evaluation for task '{}'.",
                task_id,
            )),
            status: Status::Open,
            after: vec![task_id.to_string()],
            tags: vec!["flip".to_string(), "agency".to_string()],
            exec: Some(format!("wg evaluate run {} --flip", task_id)),
            model: Some(flip_resolved.model),
            provider: flip_resolved.provider,
            exec_mode: Some("bare".to_string()),
            visibility: "internal".to_string(),
            created_at: Some(Utc::now().to_rfc3339()),
            ..Task::default()
        };
        graph.add_node(Node::Task(flip_task));
        any_created = true;
        eprintln!(
            "[eval-scaffold] Created FLIP task '{}' blocked by '{}'",
            flip_task_id, task_id,
        );
    }

    // 4. Create .evaluate-* task (depends on .flip-* if FLIP enabled, else main task)
    if config.agency.auto_evaluate && graph.get_task(&eval_task_id).is_none() {
        let eval_after = if run_flip {
            vec![flip_task_id.clone()]
        } else {
            vec![task_id.to_string()]
        };

        let evaluator_identity = resolve_evaluator_identity(dir, config);
        let mut desc = String::new();
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

        let eval_resolved =
            config.resolve_model_for_role(workgraph::config::DispatchRole::Evaluator);
        let eval_task = Task {
            id: eval_task_id.clone(),
            title: format!("Evaluate: {}", task_title),
            description: Some(desc),
            status: Status::Open,
            after: eval_after,
            tags: vec!["evaluation".to_string(), "agency".to_string()],
            exec: Some(format!("wg evaluate run {}", task_id)),
            model: Some(eval_resolved.model),
            provider: eval_resolved.provider,
            agent: config.agency.evaluator_agent.clone(),
            exec_mode: Some("bare".to_string()),
            visibility: "internal".to_string(),
            created_at: Some(Utc::now().to_rfc3339()),
            ..Task::default()
        };
        graph.add_node(Node::Task(eval_task));
        any_created = true;
        eprintln!(
            "[eval-scaffold] Created evaluation task '{}' blocked by '{}'",
            eval_task_id, task_id,
        );
    }

    // Tag source task as eval-scheduled (prevents duplicate scaffolding after gc)
    if any_created
        && let Some(source) = graph.get_task_mut(task_id)
        && !source.tags.iter().any(|t| t == "eval-scheduled")
    {
        source.tags.push("eval-scheduled".to_string());
    }

    any_created
}

/// Scaffold the full pipeline for multiple tasks at once (batch mode for publish).
/// Returns the number of tasks for which the pipeline was created.
pub fn scaffold_full_pipeline_batch(
    dir: &Path,
    graph: &mut WorkGraph,
    task_ids: &[(String, String)], // (id, title) pairs
    config: &Config,
) -> usize {
    let mut count = 0;
    for (task_id, task_title) in task_ids {
        if scaffold_full_pipeline(dir, graph, task_id, task_title, config) {
            count += 1;
        }
    }
    count
}

/// Create a `.assign-<task_id>` task in `graph` that blocks `task_id`.
///
/// The assign task is created Open with no dependencies (immediately ready).
/// The source task gets `.assign-<task_id>` added to its `after` list,
/// making it blocked until assignment completes.
///
/// Returns `true` if the graph was modified.
/// Idempotent: returns `false` if the assign task already exists.
pub fn scaffold_assign_task(graph: &mut WorkGraph, task_id: &str, task_title: &str) -> bool {
    let assign_task_id = format!(".assign-{}", task_id);

    // Idempotent: skip if assign task already exists
    if graph.get_task(&assign_task_id).is_some() {
        return false;
    }

    // Skip system tasks (unless pipeline-eligible like .verify-*) — no assign for .evaluate, .flip, etc.
    if workgraph::graph::is_system_task(task_id) && !is_pipeline_eligible_system_task(task_id) {
        return false;
    }

    // Skip shell executor tasks — they're commands, not agent work
    if let Some(task) = graph.get_task(task_id)
        && is_shell_task(task)
    {
        return false;
    }

    // Skip tasks that are part of the evaluation/assignment infrastructure
    if let Some(task) = graph.get_task(task_id)
        && task
            .tags
            .iter()
            .any(|tag| DOMINATED_TAGS.contains(&tag.as_str()))
    {
        return false;
    }

    // Calculate auto-priority for assign task
    let priority = calculate_auto_priority(graph, task_id, "assign");

    let assign_task = Task {
        id: assign_task_id.clone(),
        title: format!("Assign agent for: {}", task_title),
        status: Status::Open,
        priority,
        after: vec![],
        before: vec![task_id.to_string()],
        tags: vec!["assignment".to_string(), "agency".to_string()],
        exec: Some(format!("wg assign {} --auto", task_id)),
        exec_mode: Some("bare".to_string()),
        visibility: "internal".to_string(),
        created_at: Some(Utc::now().to_rfc3339()),
        ..Task::default()
    };

    graph.add_node(Node::Task(assign_task));

    // Add blocking edge: source task depends on .assign-*
    if let Some(source) = graph.get_task_mut(task_id)
        && !source.after.iter().any(|a| a == &assign_task_id)
    {
        source.after.push(assign_task_id.clone());
    }

    eprintln!(
        "[eval-scaffold] Created assignment task '{}' blocking '{}'",
        assign_task_id, task_id,
    );

    true
}

/// Scaffold assign tasks for multiple task IDs at once (batch mode for publish).
/// Returns the number of assign tasks created.
#[allow(dead_code)]
pub fn scaffold_assign_tasks_batch(
    graph: &mut WorkGraph,
    task_ids: &[(String, String)], // (id, title) pairs
) -> usize {
    let mut count = 0;
    for (task_id, task_title) in task_ids {
        if scaffold_assign_task(graph, task_id, task_title) {
            count += 1;
        }
    }
    count
}

/// Create a `.evaluate-<task_id>` task in `graph`, blocked by `task_id`.
///
/// When FLIP is enabled (globally or via `flip-eval` tag on the source task),
/// also creates `.flip-<task_id>` and makes `.evaluate-<task_id>` depend on
/// the flip task instead of the source task directly.
///
/// Returns `true` if the graph was modified (i.e. the eval task was created).
/// Idempotent: returns `false` if the eval task already exists or the source
/// task should not be evaluated (system tags, already scheduled, etc.).
pub fn scaffold_eval_task(
    dir: &Path,
    graph: &mut WorkGraph,
    task_id: &str,
    task_title: &str,
    config: &Config,
) -> bool {
    let eval_task_id = format!(".evaluate-{}", task_id);

    // Skip system tasks (unless pipeline-eligible like .verify-*)
    if workgraph::graph::is_system_task(task_id) && !is_pipeline_eligible_system_task(task_id) {
        return false;
    }

    // Idempotency: skip if eval task already exists
    if graph.get_task(&eval_task_id).is_some() {
        return false;
    }

    // Skip tasks that are part of the evaluation infrastructure themselves
    if let Some(task) = graph.get_task(task_id) {
        if task
            .tags
            .iter()
            .any(|tag| DOMINATED_TAGS.contains(&tag.as_str()))
        {
            return false;
        }
        // Skip if already tagged as having had evaluation scheduled
        if task.tags.iter().any(|tag| tag == "eval-scheduled") {
            return false;
        }
    }

    // When FLIP is enabled, scaffold the flip task and make eval depend on it
    let run_flip = should_run_flip(graph, task_id, config);
    let eval_after = if run_flip {
        scaffold_flip_task(graph, task_id, config);
        let flip_task_id = format!(".flip-{}", task_id);
        vec![flip_task_id]
    } else {
        vec![task_id.to_string()]
    };

    // Resolve evaluator agent identity (if configured)
    let evaluator_identity = resolve_evaluator_identity(dir, config);

    let mut desc = String::new();
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

    let eval_resolved = config.resolve_model_for_role(workgraph::config::DispatchRole::Evaluator);

    // Calculate auto-priority for eval task
    let priority = calculate_auto_priority(graph, task_id, "evaluate");

    let eval_task = Task {
        id: eval_task_id.clone(),
        title: format!("Evaluate: {}", task_title),
        description: Some(desc),
        status: Status::Open,
        priority,
        after: eval_after,
        tags: vec!["evaluation".to_string(), "agency".to_string()],
        exec: Some(format!("wg evaluate run {}", task_id)),
        model: Some(eval_resolved.model),
        provider: eval_resolved.provider,
        agent: config.agency.evaluator_agent.clone(),
        exec_mode: Some("bare".to_string()),
        visibility: "internal".to_string(),
        created_at: Some(Utc::now().to_rfc3339()),
        ..Task::default()
    };

    graph.add_node(Node::Task(eval_task));

    // Tag the source task so we never recreate the eval task after gc
    if let Some(source) = graph.get_task_mut(task_id)
        && !source.tags.iter().any(|t| t == "eval-scheduled")
    {
        source.tags.push("eval-scheduled".to_string());
    }

    eprintln!(
        "[eval-scaffold] Created evaluation task '{}' blocked by '{}'",
        eval_task_id, task_id,
    );

    true
}

/// Resolve the evaluator agent identity prompt, if an evaluator agent is configured.
fn resolve_evaluator_identity(dir: &Path, config: &Config) -> Option<String> {
    use workgraph::agency::{
        load_agent, load_role, load_tradeoff, render_identity_prompt_rich, resolve_all_components,
        resolve_outcome,
    };

    config
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
        })
}

/// Scaffold eval tasks for multiple task IDs at once (batch mode for publish).
/// Returns the number of eval tasks created.
pub fn scaffold_eval_tasks_batch(
    dir: &Path,
    graph: &mut WorkGraph,
    task_ids: &[(String, String)], // (id, title) pairs
    config: &Config,
) -> usize {
    let mut count = 0;
    for (task_id, task_title) in task_ids {
        if scaffold_eval_task(dir, graph, task_id, task_title, config) {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Status, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Open,
            ..Task::default()
        }
    }

    #[test]
    fn test_scaffold_creates_eval_task() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.flip_enabled = false;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        let modified = scaffold_eval_task(dir.path(), &mut graph, "my-task", "My Task", &config);
        assert!(modified);
        let eval = graph.get_task(".evaluate-my-task").unwrap();
        assert_eq!(eval.title, "Evaluate: My Task");
        assert_eq!(eval.after, vec!["my-task".to_string()]);
        assert!(eval.tags.contains(&"evaluation".to_string()));
        assert!(eval.tags.contains(&"agency".to_string()));
        assert_eq!(eval.exec, Some("wg evaluate run my-task".to_string()));
        assert_eq!(eval.exec_mode, Some("bare".to_string()));
        assert_eq!(eval.visibility, "internal");
    }

    #[test]
    fn test_scaffold_idempotent() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        assert!(scaffold_eval_task(
            dir.path(),
            &mut graph,
            "my-task",
            "My Task",
            &config
        ));
        // Second call should be a no-op
        assert!(!scaffold_eval_task(
            dir.path(),
            &mut graph,
            "my-task",
            "My Task",
            &config
        ));
    }

    #[test]
    fn test_scaffold_skips_evaluation_tagged_tasks() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        let mut task = make_task("eval-infra", "Eval Infra");
        task.tags = vec!["evaluation".to_string()];
        graph.add_node(Node::Task(task));

        assert!(!scaffold_eval_task(
            dir.path(),
            &mut graph,
            "eval-infra",
            "Eval Infra",
            &config
        ));
        assert!(graph.get_task(".evaluate-eval-infra").is_none());
    }

    #[test]
    fn test_scaffold_skips_already_scheduled() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        let mut task = make_task("old-task", "Old Task");
        task.tags = vec!["eval-scheduled".to_string()];
        graph.add_node(Node::Task(task));

        assert!(!scaffold_eval_task(
            dir.path(),
            &mut graph,
            "old-task",
            "Old Task",
            &config
        ));
    }

    #[test]
    fn test_scaffold_tags_source_task() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        scaffold_eval_task(dir.path(), &mut graph, "my-task", "My Task", &config);

        let source = graph.get_task("my-task").unwrap();
        assert!(source.tags.contains(&"eval-scheduled".to_string()));
    }

    #[test]
    fn test_scaffold_batch() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Task A")));
        graph.add_node(Node::Task(make_task("b", "Task B")));
        let mut eval_task = make_task("c", "Eval Task");
        eval_task.tags = vec!["evaluation".to_string()];
        graph.add_node(Node::Task(eval_task));

        let ids = vec![
            ("a".to_string(), "Task A".to_string()),
            ("b".to_string(), "Task B".to_string()),
            ("c".to_string(), "Eval Task".to_string()), // should be skipped
        ];
        let count = scaffold_eval_tasks_batch(dir.path(), &mut graph, &ids, &config);
        assert_eq!(count, 2);
        assert!(graph.get_task(".evaluate-a").is_some());
        assert!(graph.get_task(".evaluate-b").is_some());
        assert!(graph.get_task(".evaluate-c").is_none());
    }

    // --- FLIP scaffolding tests ---

    #[test]
    fn test_scaffold_flip_creates_flip_task() {
        let mut config = Config::default();
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        let modified = scaffold_flip_task(&mut graph, "my-task", &config);
        assert!(modified);

        let flip = graph.get_task(".flip-my-task").unwrap();
        assert_eq!(flip.title, "FLIP: my-task");
        assert_eq!(flip.after, vec!["my-task".to_string()]);
        assert!(flip.tags.contains(&"flip".to_string()));
        assert!(flip.tags.contains(&"agency".to_string()));
        assert_eq!(
            flip.exec,
            Some("wg evaluate run my-task --flip".to_string())
        );
        assert_eq!(flip.exec_mode, Some("bare".to_string()));
        assert_eq!(flip.visibility, "internal");
    }

    #[test]
    fn test_scaffold_flip_idempotent() {
        let mut config = Config::default();
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        assert!(scaffold_flip_task(&mut graph, "my-task", &config));
        // Second call should be a no-op
        assert!(!scaffold_flip_task(&mut graph, "my-task", &config));
    }

    #[test]
    fn test_scaffold_eval_depends_on_flip_when_enabled() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        scaffold_eval_task(dir.path(), &mut graph, "my-task", "My Task", &config);

        // .flip-my-task should exist and depend on my-task
        let flip = graph.get_task(".flip-my-task").unwrap();
        assert_eq!(flip.after, vec!["my-task".to_string()]);

        // .evaluate-my-task should depend on .flip-my-task, NOT my-task
        let eval = graph.get_task(".evaluate-my-task").unwrap();
        assert_eq!(eval.after, vec![".flip-my-task".to_string()]);
    }

    #[test]
    fn test_scaffold_eval_depends_on_source_when_flip_disabled() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.flip_enabled = false;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        scaffold_eval_task(dir.path(), &mut graph, "my-task", "My Task", &config);

        // No .flip-my-task should exist
        assert!(graph.get_task(".flip-my-task").is_none());

        // .evaluate-my-task should depend on my-task directly
        let eval = graph.get_task(".evaluate-my-task").unwrap();
        assert_eq!(eval.after, vec!["my-task".to_string()]);
    }

    #[test]
    fn test_scaffold_flip_via_task_tag() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.flip_enabled = false; // flip_enabled = false globally
        let mut graph = WorkGraph::new();
        let mut task = make_task("my-task", "My Task");
        task.tags = vec!["flip-eval".to_string()]; // per-task opt-in
        graph.add_node(Node::Task(task));

        scaffold_eval_task(dir.path(), &mut graph, "my-task", "My Task", &config);

        // FLIP should have been created via the flip-eval tag
        let flip = graph.get_task(".flip-my-task").unwrap();
        assert_eq!(flip.after, vec!["my-task".to_string()]);

        let eval = graph.get_task(".evaluate-my-task").unwrap();
        assert_eq!(eval.after, vec![".flip-my-task".to_string()]);
    }

    #[test]
    fn test_scaffold_skips_flip_tagged_tasks() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        let mut task = make_task("flip-infra", "Flip Infra");
        task.tags = vec!["flip".to_string()];
        graph.add_node(Node::Task(task));

        assert!(!scaffold_eval_task(
            dir.path(),
            &mut graph,
            "flip-infra",
            "Flip Infra",
            &config
        ));
        assert!(graph.get_task(".evaluate-flip-infra").is_none());
    }

    #[test]
    fn test_scaffold_skips_placement_tagged_tasks() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        let mut task = make_task("place-infra", "Place Infra");
        task.tags = vec!["placement".to_string()];
        graph.add_node(Node::Task(task));

        // Placement-tagged tasks should NOT get eval scaffolding
        assert!(!scaffold_eval_task(
            dir.path(),
            &mut graph,
            "place-infra",
            "Place Infra",
            &config
        ));
        assert!(graph.get_task(".evaluate-place-infra").is_none());

        // Placement-tagged tasks should NOT get assign scaffolding
        assert!(!scaffold_assign_task(
            &mut graph,
            "place-infra",
            "Place Infra"
        ));
        assert!(graph.get_task(".assign-place-infra").is_none());
    }

    #[test]
    fn test_dominated_tags_includes_placement() {
        assert!(
            DOMINATED_TAGS.contains(&"placement"),
            "DOMINATED_TAGS must include 'placement' to prevent legacy .place-* tasks from spawning eval overhead"
        );
    }

    // --- Assign scaffolding tests ---

    #[test]
    fn test_scaffold_assign_creates_assign_task() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        let modified = scaffold_assign_task(&mut graph, "my-task", "My Task");
        assert!(modified);

        let assign = graph.get_task(".assign-my-task").unwrap();
        assert_eq!(assign.title, "Assign agent for: My Task");
        assert_eq!(assign.status, Status::Open);
        assert_eq!(assign.before, vec!["my-task".to_string()]);
        assert!(assign.after.is_empty()); // No deps (placement merged into assignment)
        assert!(assign.tags.contains(&"assignment".to_string()));
        assert!(assign.tags.contains(&"agency".to_string()));
        assert_eq!(assign.visibility, "internal");

        // Source task should have .assign-* as a blocker
        let source = graph.get_task("my-task").unwrap();
        assert!(source.after.contains(&".assign-my-task".to_string()));
    }

    #[test]
    fn test_scaffold_assign_no_place_dependency() {
        // .assign-* tasks have no dependencies (placement is handled within the assignment step)
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        let modified = scaffold_assign_task(&mut graph, "my-task", "My Task");
        assert!(modified);

        let assign = graph.get_task(".assign-my-task").unwrap();
        assert!(assign.after.is_empty());
        assert_eq!(assign.before, vec!["my-task".to_string()]);
    }

    #[test]
    fn test_scaffold_assign_idempotent() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        assert!(scaffold_assign_task(&mut graph, "my-task", "My Task"));
        assert!(!scaffold_assign_task(&mut graph, "my-task", "My Task"));
    }

    #[test]
    fn test_scaffold_assign_skips_system_tasks() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(".evaluate-foo", "Eval Foo")));

        assert!(!scaffold_assign_task(
            &mut graph,
            ".evaluate-foo",
            "Eval Foo"
        ));
        assert!(graph.get_task(".assign-.evaluate-foo").is_none());
    }

    #[test]
    fn test_scaffold_assign_skips_dominated_tags() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("assign-infra", "Assign Infra");
        task.tags = vec!["assignment".to_string()];
        graph.add_node(Node::Task(task));

        assert!(!scaffold_assign_task(
            &mut graph,
            "assign-infra",
            "Assign Infra"
        ));
    }

    #[test]
    fn test_scaffold_assign_batch() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Task A")));
        graph.add_node(Node::Task(make_task("b", "Task B")));

        let ids = vec![
            ("a".to_string(), "Task A".to_string()),
            ("b".to_string(), "Task B".to_string()),
        ];
        let count = scaffold_assign_tasks_batch(&mut graph, &ids);
        assert_eq!(count, 2);
        assert!(graph.get_task(".assign-a").is_some());
        assert!(graph.get_task(".assign-b").is_some());
    }

    // --- scaffold_full_pipeline tests ---

    #[test]
    fn test_scaffold_full_pipeline_creates_all_tasks() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_place = true;
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        let modified = scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);
        assert!(modified);

        // Pipeline tasks exist (no separate .place-* task)
        assert!(graph.get_task(".assign-foo").is_some());
        assert!(graph.get_task(".flip-foo").is_some());
        assert!(graph.get_task(".evaluate-foo").is_some());
    }

    #[test]
    fn test_scaffold_full_pipeline_wires_all_edges() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_place = true;
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);

        // .assign-foo has no deps (placement is merged into assignment)
        let assign = graph.get_task(".assign-foo").unwrap();
        assert!(assign.after.is_empty());
        assert_eq!(assign.before, vec!["foo".to_string()]);

        // foo depends on .assign-foo
        let foo = graph.get_task("foo").unwrap();
        assert!(foo.after.contains(&".assign-foo".to_string()));

        // .flip-foo depends on foo
        let flip = graph.get_task(".flip-foo").unwrap();
        assert_eq!(flip.after, vec!["foo".to_string()]);

        // .evaluate-foo depends on .flip-foo (when FLIP enabled)
        let eval = graph.get_task(".evaluate-foo").unwrap();
        assert_eq!(eval.after, vec![".flip-foo".to_string()]);
    }

    #[test]
    fn test_scaffold_full_pipeline_assign_has_no_deps() {
        // .assign-* tasks never have deps (placement is merged, not a separate step)
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);

        let assign = graph.get_task(".assign-foo").unwrap();
        assert!(assign.after.is_empty());
    }

    #[test]
    fn test_scaffold_full_pipeline_idempotent() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_place = true;
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        assert!(scaffold_full_pipeline(
            dir.path(),
            &mut graph,
            "foo",
            "Foo Task",
            &config
        ));
        // Second call is a no-op (eval-scheduled tag prevents re-scaffolding)
        assert!(!scaffold_full_pipeline(
            dir.path(),
            &mut graph,
            "foo",
            "Foo Task",
            &config
        ));
    }

    #[test]
    fn test_scaffold_full_pipeline_tags_source_as_eval_scheduled() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);

        let foo = graph.get_task("foo").unwrap();
        assert!(foo.tags.contains(&"eval-scheduled".to_string()));
    }

    #[test]
    fn test_scaffold_full_pipeline_skips_system_tasks() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(".evaluate-foo", "Eval Foo")));

        let modified =
            scaffold_full_pipeline(dir.path(), &mut graph, ".evaluate-foo", "Eval Foo", &config);
        assert!(!modified);
    }

    #[test]
    fn test_scaffold_full_pipeline_skips_dominated_tags() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        let mut graph = WorkGraph::new();
        let mut task = make_task("eval-infra", "Eval Infra");
        task.tags = vec!["evaluation".to_string()];
        graph.add_node(Node::Task(task));

        let modified =
            scaffold_full_pipeline(dir.path(), &mut graph, "eval-infra", "Eval Infra", &config);
        assert!(!modified);
        assert!(graph.get_task(".assign-eval-infra").is_none());
        assert!(graph.get_task(".evaluate-eval-infra").is_none());
    }

    #[test]
    fn test_scaffold_full_pipeline_no_place_task_created() {
        // Placement is handled by the assignment step — no separate .place-* tasks
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_place = true;
        config.agency.auto_assign = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);

        assert!(
            graph.get_task(".place-foo").is_none(),
            ".place-* tasks should not be created"
        );
        assert!(
            graph.get_task(".assign-foo").is_some(),
            ".assign-* task should still be created"
        );
    }

    #[test]
    fn test_scaffold_full_pipeline_creates_assign_even_if_eval_scheduled() {
        // Regression: if scaffold_eval_task ran first (coordinator path) and set
        // the eval-scheduled tag, scaffold_full_pipeline must still create
        // .assign-* tasks.
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_place = true;
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        // Simulate coordinator's scaffold_eval_task running first:
        // creates .flip-* and .evaluate-*, tags source as eval-scheduled
        scaffold_eval_task(dir.path(), &mut graph, "foo", "Foo Task", &config);
        assert!(graph.get_task(".flip-foo").is_some());
        assert!(graph.get_task(".evaluate-foo").is_some());
        let source = graph.get_task("foo").unwrap();
        assert!(source.tags.contains(&"eval-scheduled".to_string()));

        // Now scaffold_full_pipeline runs (publish path) — must still create
        // .assign-* despite the eval-scheduled tag
        let modified = scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);
        assert!(
            modified,
            "scaffold_full_pipeline should have created .assign"
        );

        assert!(
            graph.get_task(".assign-foo").is_some(),
            ".assign-foo must exist even when eval-scheduled tag is set"
        );
    }

    #[test]
    fn test_verify_task_gets_full_agency_pipeline() {
        // .verify-* tasks are pipeline-eligible system tasks — they should get
        // .assign-*, .flip-*, and .evaluate-* scaffolded just like regular tasks.
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;

        let mut graph = WorkGraph::new();
        let mut verify_task = make_task(".verify-my-task", "Verify: my-task");
        verify_task.tags = vec!["verification".to_string(), "agency".to_string()];
        graph.add_node(Node::Task(verify_task));

        let modified = scaffold_full_pipeline(
            dir.path(),
            &mut graph,
            ".verify-my-task",
            "Verify: my-task",
            &config,
        );
        assert!(modified, "should scaffold pipeline for .verify-* task");

        // .assign-.verify-my-task should exist and block .verify-my-task
        let assign = graph.get_task(".assign-.verify-my-task").unwrap();
        assert!(assign.tags.contains(&"assignment".to_string()));
        assert_eq!(
            assign.exec,
            Some("wg assign .verify-my-task --auto".to_string())
        );
        let verify = graph.get_task(".verify-my-task").unwrap();
        assert!(
            verify
                .after
                .contains(&".assign-.verify-my-task".to_string()),
            ".verify-* should depend on its .assign-* task"
        );

        // .flip-.verify-my-task should exist
        let flip = graph.get_task(".flip-.verify-my-task").unwrap();
        assert!(flip.after.contains(&".verify-my-task".to_string()));
        assert!(flip.tags.contains(&"flip".to_string()));

        // .evaluate-.verify-my-task should exist
        let eval = graph.get_task(".evaluate-.verify-my-task").unwrap();
        assert!(eval.tags.contains(&"evaluation".to_string()));
    }

    #[test]
    fn test_non_verify_system_tasks_still_skip_pipeline() {
        // System tasks like .evaluate-*, .flip-*, .assign-* should NOT get the pipeline.
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;

        let mut graph = WorkGraph::new();
        let mut eval_task = make_task(".evaluate-my-task", "Evaluate: my-task");
        eval_task.tags = vec!["evaluation".to_string(), "agency".to_string()];
        graph.add_node(Node::Task(eval_task));

        let modified = scaffold_full_pipeline(
            dir.path(),
            &mut graph,
            ".evaluate-my-task",
            "Evaluate: my-task",
            &config,
        );
        assert!(
            !modified,
            "should NOT scaffold pipeline for .evaluate-* task"
        );
        assert!(graph.get_task(".assign-.evaluate-my-task").is_none());
    }

    #[test]
    fn test_verify_assign_task_idempotent() {
        // If .assign-.verify-* already exists, scaffold_full_pipeline should not duplicate it.
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;

        let mut graph = WorkGraph::new();
        let mut verify_task = make_task(".verify-t1", "Verify: t1");
        verify_task.tags = vec!["verification".to_string(), "agency".to_string()];
        graph.add_node(Node::Task(verify_task));

        // Pre-create the assign task
        let mut existing_assign = make_task(".assign-.verify-t1", "Pre-existing assign");
        existing_assign.tags = vec!["assignment".to_string(), "agency".to_string()];
        graph.add_node(Node::Task(existing_assign));

        let modified =
            scaffold_full_pipeline(dir.path(), &mut graph, ".verify-t1", "Verify: t1", &config);
        // Should still create .evaluate-* even if .assign-* exists
        assert!(modified);

        // Existing assign should be preserved
        let assign = graph.get_task(".assign-.verify-t1").unwrap();
        assert_eq!(assign.title, "Pre-existing assign");
    }

    #[test]
    fn test_is_pipeline_eligible_system_task() {
        assert!(is_pipeline_eligible_system_task(".verify-my-task"));
        assert!(is_pipeline_eligible_system_task(".verify-feature-x"));
        assert!(!is_pipeline_eligible_system_task(".evaluate-my-task"));
        assert!(!is_pipeline_eligible_system_task(".assign-my-task"));
        assert!(!is_pipeline_eligible_system_task(".flip-my-task"));
        assert!(!is_pipeline_eligible_system_task("regular-task"));
    }

    #[test]
    fn test_scaffold_eval_skips_system_tasks() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();

        // .coordinator-* tasks should NOT get eval scaffolding
        graph.add_node(Node::Task(make_task(
            ".coordinator-test",
            "Coordinator Test",
        )));
        assert!(!scaffold_eval_task(
            dir.path(),
            &mut graph,
            ".coordinator-test",
            "Coordinator Test",
            &config
        ));
        assert!(graph.get_task(".evaluate-.coordinator-test").is_none());
        assert!(graph.get_task(".flip-.coordinator-test").is_none());

        // .archive-* tasks should NOT get eval scaffolding
        graph.add_node(Node::Task(make_task(".archive-test", "Archive Test")));
        assert!(!scaffold_eval_task(
            dir.path(),
            &mut graph,
            ".archive-test",
            "Archive Test",
            &config
        ));
        assert!(graph.get_task(".evaluate-.archive-test").is_none());

        // .compact-* tasks should NOT get eval scaffolding
        graph.add_node(Node::Task(make_task(".compact-0", "Compact")));
        assert!(!scaffold_eval_task(
            dir.path(),
            &mut graph,
            ".compact-0",
            "Compact",
            &config
        ));
        assert!(graph.get_task(".evaluate-.compact-0").is_none());

        // Normal tasks should still get eval scaffolding
        graph.add_node(Node::Task(make_task("normal-task", "Normal Task")));
        assert!(scaffold_eval_task(
            dir.path(),
            &mut graph,
            "normal-task",
            "Normal Task",
            &config
        ));
        assert!(graph.get_task(".evaluate-normal-task").is_some());
    }

    #[test]
    fn test_scaffold_flip_skips_system_tasks() {
        let mut config = Config::default();
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();

        graph.add_node(Node::Task(make_task(
            ".coordinator-test",
            "Coordinator Test",
        )));
        assert!(!scaffold_flip_task(
            &mut graph,
            ".coordinator-test",
            &config
        ));
        assert!(graph.get_task(".flip-.coordinator-test").is_none());

        graph.add_node(Node::Task(make_task(".archive-test", "Archive Test")));
        assert!(!scaffold_flip_task(&mut graph, ".archive-test", &config));
        assert!(graph.get_task(".flip-.archive-test").is_none());

        // Normal tasks should still get FLIP
        graph.add_node(Node::Task(make_task("normal-task", "Normal Task")));
        assert!(scaffold_flip_task(&mut graph, "normal-task", &config));
        assert!(graph.get_task(".flip-normal-task").is_some());
    }

    #[test]
    fn test_scaffold_eval_batch_skips_system_tasks() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Task A")));
        graph.add_node(Node::Task(make_task(
            ".coordinator-main",
            "Coordinator Main",
        )));
        graph.add_node(Node::Task(make_task(".archive-old", "Archive Old")));

        let ids = vec![
            ("a".to_string(), "Task A".to_string()),
            (
                ".coordinator-main".to_string(),
                "Coordinator Main".to_string(),
            ),
            (".archive-old".to_string(), "Archive Old".to_string()),
        ];
        let count = scaffold_eval_tasks_batch(dir.path(), &mut graph, &ids, &config);
        assert_eq!(count, 1); // Only "a" should get eval scaffolding
        assert!(graph.get_task(".evaluate-a").is_some());
        assert!(graph.get_task(".evaluate-.coordinator-main").is_none());
        assert!(graph.get_task(".evaluate-.archive-old").is_none());
    }

    #[test]
    fn test_verify_tasks_still_get_eval_scaffolding() {
        // .verify-* tasks are pipeline-eligible and SHOULD get eval scaffolding
        let dir = tempdir().unwrap();
        let config = Config::default();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(".verify-my-task", "Verify: my-task")));

        assert!(scaffold_eval_task(
            dir.path(),
            &mut graph,
            ".verify-my-task",
            "Verify: my-task",
            &config
        ));
        assert!(graph.get_task(".evaluate-.verify-my-task").is_some());
    }

    #[test]
    fn agency_skips_system_tasks() {
        // Integration-style: system tasks (.coordinator-*, .archive-*, .compact-*) get
        // no FLIP, no evaluate, and no assign scaffolding via any entry point.
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;

        let system_ids = [
            (".coordinator-test", "Coordinator Test"),
            (".archive-test", "Archive Test"),
            (".compact-0", "Compact"),
            (".assign-foo", "Assign Foo"),
            (".flip-foo", "FLIP Foo"),
            (".evaluate-foo", "Evaluate Foo"),
            (".quality-pass-1", "Quality Pass"),
        ];

        let mut graph = WorkGraph::new();
        for (id, title) in &system_ids {
            graph.add_node(Node::Task(make_task(id, title)));
        }
        graph.add_node(Node::Task(make_task("normal-task", "Normal Task")));

        for (id, title) in &system_ids {
            assert!(
                !scaffold_full_pipeline(dir.path(), &mut graph, id, title, &config),
                "scaffold_full_pipeline should skip system task '{}'",
                id
            );
            assert!(
                !scaffold_eval_task(dir.path(), &mut graph, id, title, &config),
                "scaffold_eval_task should skip system task '{}'",
                id
            );
            assert!(
                !scaffold_flip_task(&mut graph, id, &config),
                "scaffold_flip_task should skip system task '{}'",
                id
            );
            assert!(
                !scaffold_assign_task(&mut graph, id, title),
                "scaffold_assign_task should skip system task '{}'",
                id
            );
        }

        // Normal tasks SHOULD still get the full pipeline
        assert!(scaffold_full_pipeline(
            dir.path(),
            &mut graph,
            "normal-task",
            "Normal Task",
            &config
        ));
        assert!(graph.get_task(".assign-normal-task").is_some());
        assert!(graph.get_task(".flip-normal-task").is_some());
        assert!(graph.get_task(".evaluate-normal-task").is_some());
    }

    #[test]
    fn test_is_shell_task() {
        // Task with exec set → shell task
        let mut task = make_task("shell-1", "Shell Task");
        task.exec = Some("echo hello".to_string());
        assert!(is_shell_task(&task));

        // Task with exec_mode=shell → shell task
        let mut task2 = make_task("shell-2", "Shell Task 2");
        task2.exec_mode = Some("shell".to_string());
        assert!(is_shell_task(&task2));

        // Regular task → not a shell task
        let task3 = make_task("regular", "Regular Task");
        assert!(!is_shell_task(&task3));

        // Task with exec_mode=full → not a shell task
        let mut task4 = make_task("full", "Full Task");
        task4.exec_mode = Some("full".to_string());
        assert!(!is_shell_task(&task4));
    }

    #[test]
    fn test_shell_task_skips_full_pipeline() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();

        let mut task = make_task("run-tests", "Run Tests");
        task.exec = Some("cargo test".to_string());
        graph.add_node(Node::Task(task));

        let modified =
            scaffold_full_pipeline(dir.path(), &mut graph, "run-tests", "Run Tests", &config);
        assert!(!modified);
        assert!(graph.get_task(".assign-run-tests").is_none());
        assert!(graph.get_task(".flip-run-tests").is_none());
        assert!(graph.get_task(".evaluate-run-tests").is_none());
    }

    #[test]
    fn test_shell_task_skips_assign() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("run-script", "Run Script");
        task.exec = Some("python3 run.py".to_string());
        graph.add_node(Node::Task(task));

        let modified = scaffold_assign_task(&mut graph, "run-script", "Run Script");
        assert!(!modified);
        assert!(graph.get_task(".assign-run-script").is_none());
    }

    #[test]
    fn test_checker_downstream_of_shell_gets_pipeline() {
        // A non-shell task depending on a shell task should still get full pipeline
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();

        // Shell task
        let mut shell_task = make_task("run-batch", "Run Batch");
        shell_task.exec = Some("python3 batch.py".to_string());
        graph.add_node(Node::Task(shell_task));

        // Checker task (non-shell, depends on shell task)
        let mut checker = make_task("check-batch", "Check Batch");
        checker.after = vec!["run-batch".to_string()];
        graph.add_node(Node::Task(checker));

        // Shell task should not get pipeline
        let modified_shell =
            scaffold_full_pipeline(dir.path(), &mut graph, "run-batch", "Run Batch", &config);
        assert!(!modified_shell);

        // Checker task should get full pipeline
        let modified_checker = scaffold_full_pipeline(
            dir.path(),
            &mut graph,
            "check-batch",
            "Check Batch",
            &config,
        );
        assert!(modified_checker);
        assert!(graph.get_task(".assign-check-batch").is_some());
        assert!(graph.get_task(".flip-check-batch").is_some());
        assert!(graph.get_task(".evaluate-check-batch").is_some());
    }
}
