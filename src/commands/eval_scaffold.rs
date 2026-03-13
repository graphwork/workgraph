//! Eager lifecycle-task scaffolding.
//!
//! Creates `.place-<task>`, `.assign-<task>`, `.flip-<task>`, and
//! `.evaluate-<task>` tasks at publish time so every published task has a full
//! lifecycle chain as real graph edges, wired atomically in one graph write:
//!
//! ```text
//! .place-foo → .assign-foo → foo → .flip-foo → .evaluate-foo
//! ```

use chrono::Utc;
use std::path::Path;

use workgraph::config::Config;
use workgraph::graph::{Node, Status, Task, WorkGraph};

/// Tags that mark tasks as part of the evaluation/assignment infrastructure.
/// Tasks with these tags do not get their own eval tasks (no meta-evaluation).
const DOMINATED_TAGS: &[&str] = &["evaluation", "assignment", "evolution", "flip", "placement"];

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

    // Idempotency: skip if flip task already exists
    if graph.get_task(&flip_task_id).is_some() {
        return false;
    }

    let flip_resolved = config.resolve_model_for_role(workgraph::config::DispatchRole::Evaluator);

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

    eprintln!(
        "[eval-scaffold] Created FLIP task '{}' blocked by '{}'",
        flip_task_id, task_id,
    );

    true
}

/// Extract file paths from a task description using heuristics.
fn extract_file_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-'
        });
        if word.contains('/')
            && (word.ends_with(".rs")
                || word.ends_with(".ts")
                || word.ends_with(".js")
                || word.ends_with(".py")
                || word.ends_with(".go")
                || word.ends_with(".toml")
                || word.ends_with(".md")
                || word.ends_with(".yaml")
                || word.ends_with(".yml")
                || word.ends_with(".json"))
        {
            paths.push(word.to_string());
        }
    }
    paths
}

/// Build placement context string for a `.place-*` task description.
///
/// Includes:
/// - Task summary and placement hints
/// - Active tasks with their artifacts and titles
/// - Explicit restriction: only add edges to the MAIN task, not dot-tasks
fn build_placement_context(graph: &WorkGraph, task_id: &str) -> String {
    use workgraph::graph::is_system_task;

    let mut ctx = String::new();

    if let Some(task) = graph.get_task(task_id) {
        let mentioned_files = extract_file_paths(task.description.as_deref().unwrap_or(""));
        ctx.push_str("## Task to place\n");
        ctx.push_str(&format!("ID: {}\n", task.id));
        ctx.push_str(&format!("Title: {}\n", task.title));
        if let Some(ref desc) = task.description {
            let summary = desc
                .split("\n\n")
                .next()
                .unwrap_or(desc)
                .lines()
                .next()
                .unwrap_or(desc);
            let summary = if summary.len() > 150 {
                format!("{}…", &summary[..summary.floor_char_boundary(150)])
            } else {
                summary.to_string()
            };
            ctx.push_str(&format!("Summary: {}\n", summary));
        }
        if !mentioned_files.is_empty() {
            ctx.push_str(&format!(
                "Files mentioned: {}\n",
                mentioned_files.join(", ")
            ));
        }
        if !task.after.is_empty() {
            ctx.push_str(&format!("Existing deps: {}\n", task.after.join(", ")));
        }
        ctx.push('\n');

        if !task.place_near.is_empty() || !task.place_before.is_empty() {
            ctx.push_str("## Placement hints\n");
            if !task.place_near.is_empty() {
                ctx.push_str(&format!("near: {}\n", task.place_near.join(", ")));
            }
            if !task.place_before.is_empty() {
                ctx.push_str(&format!("before: {}\n", task.place_before.join(", ")));
            }
            ctx.push('\n');
        }
    }

    ctx.push_str("## Active tasks (non-terminal)\n");
    let active_tasks: Vec<_> = graph
        .tasks()
        .filter(|t| {
            !t.status.is_terminal() && !t.paused && !is_system_task(&t.id) && t.id != task_id
        })
        .collect();
    if active_tasks.is_empty() {
        ctx.push_str("(none)\n");
    } else {
        for t in &active_tasks {
            ctx.push_str(&format!("- {} ({})", t.id, t.status));
            if !t.artifacts.is_empty() {
                ctx.push_str(&format!(" [files: {}]", t.artifacts.join(", ")));
            }
            ctx.push('\n');
        }
    }

    ctx.push_str("\n## Your job\n");
    ctx.push_str(&format!(
        "Add `--after` or `--before` edges to the MAIN task '{}' only.\n",
        task_id
    ));
    ctx.push_str("Do NOT modify .assign-*, .flip-*, .evaluate-*, or any other dot-task.\n");
    ctx.push_str("Use: wg edit ");
    ctx.push_str(task_id);
    ctx.push_str(" --after <dep-id>  (or --before <dep-id>)\n");
    ctx.push_str("If no placement changes are needed, do nothing (no-op is valid).\n");

    ctx
}

/// Scaffold the FULL agency pipeline for a task in one pass:
///
/// ```text
/// .place-{id} → .assign-{id} → {id} → .flip-{id} → .evaluate-{id}
/// ```
///
/// All five tasks are created and all dependency edges are wired atomically
/// (caller is responsible for the single `save_graph` / `modify_graph` call).
///
/// - `.place-*` is created if `config.agency.auto_place` is enabled.
/// - `.assign-*` is created if `config.agency.auto_assign` is enabled.
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
    // Skip system tasks and dominated-tag tasks
    if workgraph::graph::is_system_task(task_id) {
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

    let place_task_id = format!(".place-{}", task_id);
    let assign_task_id = format!(".assign-{}", task_id);
    let flip_task_id = format!(".flip-{}", task_id);
    let eval_task_id = format!(".evaluate-{}", task_id);

    let mut any_created = false;

    // 1. Create .place-* task (no deps — runs first; agent adds edges to main task)
    if config.agency.auto_place && graph.get_task(&place_task_id).is_none() {
        let placement_context = build_placement_context(graph, task_id);
        let placer_model = config.resolve_model_for_role(workgraph::config::DispatchRole::Placer);
        let place_task = Task {
            id: place_task_id.clone(),
            title: format!("Place: {}", task_id),
            description: Some(placement_context),
            status: Status::Open,
            after: vec![],
            tags: vec!["placement".to_string(), "agency".to_string()],
            exec_mode: Some("bare".to_string()),
            visibility: "internal".to_string(),
            created_at: Some(Utc::now().to_rfc3339()),
            model: Some(placer_model.model),
            provider: placer_model.provider,
            agent: config.agency.placer_agent.clone(),
            ..Task::default()
        };
        graph.add_node(Node::Task(place_task));
        any_created = true;
        eprintln!(
            "[eval-scaffold] Created placement task '{}' for '{}'",
            place_task_id, task_id,
        );
    }

    // 2. Create .assign-* task (depends on .place-* if it was created, else no deps)
    if config.agency.auto_assign && graph.get_task(&assign_task_id).is_none() {
        let assign_after = if graph.get_task(&place_task_id).is_some() {
            vec![place_task_id.clone()]
        } else {
            vec![]
        };
        let assign_task = Task {
            id: assign_task_id.clone(),
            title: format!("Assign agent for: {}", task_title),
            status: Status::Open,
            after: assign_after,
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

    // 3. Wire main task to depend on .assign-* (so it waits for assignment)
    if graph.get_task(&assign_task_id).is_some()
        && let Some(source) = graph.get_task_mut(task_id)
        && !source.after.iter().any(|a| a == &assign_task_id)
    {
        source.after.push(assign_task_id.clone());
    }

    // 4. Create .flip-* task (depends on main task)
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

    // 5. Create .evaluate-* task (depends on .flip-* if FLIP enabled, else main task)
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

    // Skip system tasks (no assign for .evaluate, .flip, etc.)
    if workgraph::graph::is_system_task(task_id) {
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

    // Dual-wiring strategy for .place-* → .assign-* ordering:
    //
    // This is one half of a two-sided wiring approach. Either side can run first:
    //   1. Here (publish-side): if .place-* already exists, add it as a dep.
    //   2. Coordinator Phase 2.9 (build_placement_tasks): if .assign-* already
    //      exists when .place-* is created, retroactively add .place-* to
    //      .assign-*'s after list.
    //
    // Whoever arrives second completes the chain. This makes the wiring
    // idempotent regardless of creation order.
    let place_task_id = format!(".place-{}", task_id);
    let after = if graph.get_task(&place_task_id).is_some() {
        vec![place_task_id.clone()]
    } else {
        vec![]
    };

    let assign_task = Task {
        id: assign_task_id.clone(),
        title: format!("Assign agent for: {}", task_title),
        status: Status::Open,
        after,
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
        let config = Config::default();
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
        let config = Config::default(); // flip_enabled = false
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
        let config = Config::default(); // flip_enabled = false globally
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
            "DOMINATED_TAGS must include 'placement' to prevent .place-* tasks from spawning eval overhead"
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
        assert!(assign.after.is_empty()); // No .place-* exists → no deps
        assert!(assign.tags.contains(&"assignment".to_string()));
        assert!(assign.tags.contains(&"agency".to_string()));
        assert_eq!(assign.visibility, "internal");

        // Source task should have .assign-* as a blocker
        let source = graph.get_task("my-task").unwrap();
        assert!(source.after.contains(&".assign-my-task".to_string()));
    }

    #[test]
    fn test_scaffold_assign_depends_on_place_when_exists() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("my-task", "My Task")));

        // Create a .place-* task first (as coordinator Phase 2.9 would)
        let place_task = Task {
            id: ".place-my-task".to_string(),
            title: "Place: my-task".to_string(),
            status: Status::Open,
            tags: vec!["placement".to_string()],
            ..Task::default()
        };
        graph.add_node(Node::Task(place_task));

        // Now scaffold .assign-* — it should depend on .place-*
        let modified = scaffold_assign_task(&mut graph, "my-task", "My Task");
        assert!(modified);

        let assign = graph.get_task(".assign-my-task").unwrap();
        assert_eq!(assign.after, vec![".place-my-task".to_string()]);
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
    fn test_scaffold_full_pipeline_creates_all_five_tasks() {
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

        // All five tasks exist
        assert!(graph.get_task(".place-foo").is_some());
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

        // .place-foo has no deps (runs first)
        let place = graph.get_task(".place-foo").unwrap();
        assert!(place.after.is_empty());

        // .assign-foo depends on .place-foo
        let assign = graph.get_task(".assign-foo").unwrap();
        assert_eq!(assign.after, vec![".place-foo".to_string()]);
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
    fn test_scaffold_full_pipeline_no_place_when_auto_place_disabled() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_place = false;
        config.agency.auto_assign = true;
        config.agency.auto_evaluate = true;
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);

        // No .place-* when auto_place=false
        assert!(graph.get_task(".place-foo").is_none());
        // .assign-* still created (no .place-* dep)
        let assign = graph.get_task(".assign-foo").unwrap();
        assert!(assign.after.is_empty()); // no .place-* dep
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
    fn test_scaffold_full_pipeline_place_description_restricts_to_main_task() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.auto_place = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);

        let place = graph.get_task(".place-foo").unwrap();
        let desc = place.description.as_deref().unwrap_or("");
        // Description must explicitly restrict edge additions to the main task
        assert!(
            desc.contains("MAIN task") || desc.contains("main task"),
            "Placement task description should restrict edges to main task, got: {}",
            desc
        );
        assert!(
            desc.contains("Do NOT") || desc.contains("do not"),
            "Placement task description should prohibit modifying dot-tasks, got: {}",
            desc
        );
    }

    #[test]
    fn test_scaffold_full_pipeline_creates_place_even_if_eval_scheduled() {
        // Regression: if scaffold_eval_task ran first (coordinator path) and set
        // the eval-scheduled tag, scaffold_full_pipeline must still create
        // .place-* and .assign-* tasks.
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
        // .place-* and .assign-* despite the eval-scheduled tag
        let modified = scaffold_full_pipeline(dir.path(), &mut graph, "foo", "Foo Task", &config);
        assert!(
            modified,
            "scaffold_full_pipeline should have created .place and .assign"
        );

        assert!(
            graph.get_task(".place-foo").is_some(),
            ".place-foo must exist even when eval-scheduled tag is set"
        );
        assert!(
            graph.get_task(".assign-foo").is_some(),
            ".assign-foo must exist even when eval-scheduled tag is set"
        );
    }
}
