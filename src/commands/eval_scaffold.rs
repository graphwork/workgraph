//! Eager lifecycle-task scaffolding.
//!
//! Creates `.assign-<task>`, `.evaluate-<task>`, and `.flip-<task>` tasks at
//! publish time so every published task has a full lifecycle chain as real
//! graph edges:
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
const DOMINATED_TAGS: &[&str] = &["evaluation", "assignment", "evolution", "flip"];

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

    // If a .place-* task exists for this source task, make .assign-* depend on it.
    // This enforces the pipeline ordering: .place-* → .assign-* → task
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

    // --- Full lifecycle chain test ---

    #[test]
    fn test_publish_creates_full_lifecycle_chain() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("foo", "Foo Task")));

        // Scaffold the full lifecycle chain (no .place-* — direct publish)
        scaffold_assign_task(&mut graph, "foo", "Foo Task");
        scaffold_eval_task(dir.path(), &mut graph, "foo", "Foo Task", &config);

        // Verify .assign-foo exists and blocks foo
        let assign = graph.get_task(".assign-foo").unwrap();
        assert_eq!(assign.status, Status::Open);
        assert_eq!(assign.before, vec!["foo".to_string()]);
        assert!(assign.after.is_empty()); // No .place-* → no deps

        // Verify foo has .assign-foo in its after list
        let foo = graph.get_task("foo").unwrap();
        assert!(foo.after.contains(&".assign-foo".to_string()));

        // Verify .flip-foo exists and depends on foo
        let flip = graph.get_task(".flip-foo").unwrap();
        assert_eq!(flip.after, vec!["foo".to_string()]);

        // Verify .evaluate-foo exists and depends on .flip-foo
        let eval = graph.get_task(".evaluate-foo").unwrap();
        assert_eq!(eval.after, vec![".flip-foo".to_string()]);

        // Full chain: .assign-foo → foo → .flip-foo → .evaluate-foo
    }

    #[test]
    fn test_full_lifecycle_chain_with_placement() {
        let dir = tempdir().unwrap();
        let mut config = Config::default();
        config.agency.flip_enabled = true;
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("bar", "Bar Task")));

        // Simulate coordinator Phase 2.9: create .place-* first
        let place_task = Task {
            id: ".place-bar".to_string(),
            title: "Place: bar".to_string(),
            status: Status::Open,
            tags: vec!["placement".to_string()],
            ..Task::default()
        };
        graph.add_node(Node::Task(place_task));

        // Simulate publish (after placement agent calls wg publish):
        // scaffold assign + eval
        scaffold_assign_task(&mut graph, "bar", "Bar Task");
        scaffold_eval_task(dir.path(), &mut graph, "bar", "Bar Task", &config);

        // Full chain: .place-bar → .assign-bar → bar → .flip-bar → .evaluate-bar
        let place = graph.get_task(".place-bar").unwrap();
        assert_eq!(place.status, Status::Open);

        let assign = graph.get_task(".assign-bar").unwrap();
        assert_eq!(assign.after, vec![".place-bar".to_string()]);
        assert_eq!(assign.before, vec!["bar".to_string()]);

        let bar = graph.get_task("bar").unwrap();
        assert!(bar.after.contains(&".assign-bar".to_string()));

        let flip = graph.get_task(".flip-bar").unwrap();
        assert_eq!(flip.after, vec!["bar".to_string()]);

        let eval = graph.get_task(".evaluate-bar").unwrap();
        assert_eq!(eval.after, vec![".flip-bar".to_string()]);
    }
}
