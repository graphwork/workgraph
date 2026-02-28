use anyhow::{Context, Result};
use std::path::Path;
use workgraph::agency;
use workgraph::config::Config;
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Record an evaluation against the assigner special agent's performance.
///
/// When auto_evaluate is enabled and an assigner_agent is configured, this
/// creates an evaluation entry for the assignment itself (source = "system"),
/// recording against the assigner agent entity so it accumulates performance
/// history. The actual quality signal comes later from the agent's task
/// evaluation, but recording the event here lets the system attribute
/// downstream scores back to the assignment decision via the 6-step cascade.
fn record_assigner_evaluation(
    agency_dir: &Path,
    task_id: &str,
    _assigned_agent: &agency::Agent,
    config: &Config,
) {
    if !config.agency.auto_evaluate {
        return;
    }

    // Resolve the assigner special agent from config
    let assigner_agent = match config.agency.assigner_agent {
        Some(ref hash) => {
            let agents_dir = agency_dir.join("cache/agents");
            match agency::find_agent_by_prefix(&agents_dir, hash) {
                Ok(agent) => agent,
                Err(_) => return, // No assigner agent found — skip recording
            }
        }
        None => return, // No assigner agent configured
    };

    let assign_task_id = format!("assign-{}", task_id);
    let eval = agency::Evaluation {
        id: format!("eval-assign-{}", task_id),
        task_id: assign_task_id,
        agent_id: assigner_agent.id.clone(),
        role_id: assigner_agent.role_id.clone(),
        tradeoff_id: assigner_agent.tradeoff_id.clone(),
        // Placeholder score — actual quality will be determined by downstream
        // evaluation. The assigner's "score" is updated
        // retrospectively when the assigned agent's task completes.
        score: 0.5,
        dimensions: std::collections::HashMap::new(),
        notes: format!("Assignment recorded for task '{}'. Awaiting downstream evaluation.", task_id),
        evaluator: "system".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        model: None,
        source: "system".to_string(),
    };

    if let Err(e) = agency::record_evaluation(&eval, agency_dir) {
        eprintln!("Warning: failed to record assigner evaluation for '{}': {}", task_id, e);
    }
}

/// `wg assign <task-id> <agent-hash>`  — explicitly assign agent to task
/// `wg assign <task-id> --clear`       — remove agent assignment
pub fn run(dir: &Path, task_id: &str, agent_hash: Option<&str>, clear: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    if clear {
        return run_clear(dir, &path, task_id);
    }

    match agent_hash {
        Some(hash) => run_explicit_assign(dir, &path, task_id, hash),
        None => {
            anyhow::bail!(
                "Usage: wg assign <task-id> <agent-hash>\n\
                 Or use --clear to remove assignment."
            );
        }
    }
}

/// Explicitly assign an agent (by hash or prefix) to a task.
fn run_explicit_assign(dir: &Path, path: &Path, task_id: &str, agent_hash: &str) -> Result<()> {
    let agency_dir = dir.join("agency");
    let agents_dir = agency_dir.join("cache/agents");

    // Resolve agent by prefix
    let agent = agency::find_agent_by_prefix(&agents_dir, agent_hash).with_context(|| {
        let available = list_available_agent_ids(&agents_dir);
        let hint = if available.is_empty() {
            "No agents defined. Use 'wg agent create' to create one.".to_string()
        } else {
            format!("Available agents: {}", available.join(", "))
        };
        format!("No agent matching '{}'. {}", agent_hash, hint)
    })?;

    let mut graph = load_graph(path).context("Failed to load graph")?;

    let task = graph.get_task_mut_or_err(task_id)?;

    task.agent = Some(agent.id.clone());
    save_graph(&graph, path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "assign",
        Some(task_id),
        None,
        serde_json::json!({ "agent_hash": agent.id, "role_id": agent.role_id }),
        config.log.rotation_threshold,
    );

    // Update preliminary TaskAssignmentRecord (created by coordinator) with actual agent info.
    // If no preliminary record exists, create one with CacheMiss mode.
    let assignments_dir = agency_dir.join("assignments");
    let record = match agency::load_assignment_record_by_task(&assignments_dir, task_id) {
        Ok(mut existing) => {
            existing.agent_id = agent.id.clone();
            existing.composition_id = agent.id.clone();
            existing
        }
        Err(_) => {
            // No preliminary record — create a basic one
            agency::TaskAssignmentRecord {
                task_id: task_id.to_string(),
                agent_id: agent.id.clone(),
                composition_id: agent.id.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                run_mode_value: config.agency.run_mode,
                mode: agency::AssignmentMode::CacheMiss,
            }
        }
    };
    if let Err(e) = agency::save_assignment_record(&record, &assignments_dir) {
        eprintln!("Warning: failed to save assignment record for '{}': {}", task_id, e);
    }

    // Record assigner evaluation for downstream attribution
    record_assigner_evaluation(&agency_dir, task_id, &agent, &config);

    // Resolve role/tradeoff names for display
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");

    let role_name = agency::find_role_by_prefix(&roles_dir, &agent.role_id)
        .map(|r| r.name)
        .unwrap_or_else(|_| "(not found)".to_string());
    let tradeoff_name = agency::find_tradeoff_by_prefix(&tradeoffs_dir, &agent.tradeoff_id)
        .map(|t| t.name)
        .unwrap_or_else(|_| "(not found)".to_string());

    println!("Assigned agent to task '{}':", task_id);
    println!(
        "  Agent:      {} ({})",
        agent.name,
        agency::short_hash(&agent.id)
    );
    println!(
        "  Role:       {} ({})",
        role_name,
        agency::short_hash(&agent.role_id)
    );
    println!(
        "  Tradeoff:   {} ({})",
        tradeoff_name,
        agency::short_hash(&agent.tradeoff_id)
    );

    Ok(())
}

/// Clear the agent assignment from a task.
fn run_clear(dir: &Path, path: &Path, task_id: &str) -> Result<()> {
    let mut graph = load_graph(path).context("Failed to load graph")?;

    let task = graph.get_task_mut_or_err(task_id)?;

    let prev_agent = task.agent.clone();
    task.agent = None;
    save_graph(&graph, path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "assign",
        Some(task_id),
        None,
        serde_json::json!({ "action": "clear", "prev_agent": prev_agent }),
        config.log.rotation_threshold,
    );

    if prev_agent.is_some() {
        println!("Cleared agent from task '{}'", task_id);
    } else {
        println!("Task '{}' had no agent assigned (no change)", task_id);
    }
    Ok(())
}

/// List available agent short IDs from the agents directory.
fn list_available_agent_ids(dir: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                ids.push(agency::short_hash(stem).to_string());
            }
        }
    }
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::agency::{Lineage, PerformanceRecord};
    use workgraph::graph::{Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_workgraph(dir: &Path, tasks: Vec<Task>) {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
    }

    /// Set up agency with test entities, returning (agent_id, role_id, tradeoff_id).
    fn setup_agency(dir: &Path) -> (String, String, String) {
        let agency_dir = dir.join("agency");
        agency::init(&agency_dir).unwrap();

        let role = agency::build_role(
            "Implementer",
            "Writes code",
            vec!["rust".to_string()],
            "Working code",
        );
        let role_id = role.id.clone();
        agency::save_role(&role, &agency_dir.join("cache/roles")).unwrap();

        let mut tradeoff = agency::build_tradeoff(
            "Quality First",
            "Prioritise correctness",
            vec!["Slower delivery".to_string()],
            vec!["Skipping tests".to_string()],
        );
        tradeoff.performance.task_count = 2;
        tradeoff.performance.avg_score = Some(0.9);
        let tradeoff_id = tradeoff.id.clone();
        agency::save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

        // Create an agent for this role+tradeoff pair
        let agent_id = agency::content_hash_agent(&role_id, &tradeoff_id);
        let agent = agency::Agent {
            id: agent_id.clone(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            name: "test-agent".to_string(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "claude".to_string(),
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
        };
        agency::save_agent(&agent, &agency_dir.join("cache/agents")).unwrap();

        (agent_id, role_id, tradeoff_id)
    }

    #[test]
    fn test_assign_explicit_agent_hash() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (agent_id, _role_id, _tradeoff_id) = setup_agency(dir_path);

        let result = run(dir_path, "t1", Some(&agent_id), false);
        assert!(result.is_ok(), "assign failed: {:?}", result.err());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.agent, Some(agent_id));
    }

    #[test]
    fn test_assign_by_prefix() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (agent_id, _role_id, _tradeoff_id) = setup_agency(dir_path);

        // Use 8-char prefix instead of full hash
        let prefix = &agent_id[..8];
        let result = run(dir_path, "t1", Some(prefix), false);
        assert!(
            result.is_ok(),
            "assign by prefix failed: {:?}",
            result.err()
        );

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.agent, Some(agent_id));
    }

    #[test]
    fn test_assign_clear() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task");
        task.agent = Some("some-agent-hash".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", None, true);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.agent.is_none());
    }

    #[test]
    fn test_assign_nonexistent_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);
        let (agent_id, _, _) = setup_agency(dir_path);

        let result = run(dir_path, "nonexistent", Some(&agent_id), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_assign_nonexistent_agent() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        setup_agency(dir_path);

        let result = run(dir_path, "t1", Some("nonexistent"), false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No agent matching 'nonexistent'"));
    }

    #[test]
    fn test_assign_no_args_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);

        let result = run(dir_path, "t1", None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Usage:"));
    }

    #[test]
    fn test_clear_no_agent_is_noop() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);

        let result = run(dir_path, "t1", None, true);
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // Special agent evaluation recording tests
    // -----------------------------------------------------------------------

    /// Set up a full agency with the assigner special agent composed from
    /// real starters, matching the `wg agency init` pathway. Returns
    /// (actor_agent_id, assigner_agent_id).
    fn setup_agency_with_assigner(dir: &Path) -> (String, String) {
        let agency_dir = dir.join("agency");
        agency::seed_starters(&agency_dir).unwrap();

        let agents_dir = agency_dir.join("cache/agents");
        fs::create_dir_all(&agents_dir).unwrap();

        // Create the actor agent (assigned to the task)
        let (actor_id, _role_id, _tradeoff_id) = setup_agency(dir);

        // Compose the assigner special agent from starter primitives
        let special_roles = agency::special_agent_roles();
        let special_tradeoffs = agency::special_agent_tradeoffs();
        let assigner_role = special_roles.iter().find(|r| r.name == "Assigner").unwrap();
        let assigner_tradeoff = special_tradeoffs
            .iter()
            .find(|t| t.name == "Assigner Balanced")
            .unwrap();

        let assigner_id = agency::content_hash_agent(&assigner_role.id, &assigner_tradeoff.id);
        let assigner_path = agents_dir.join(format!("{}.yaml", assigner_id));
        if !assigner_path.exists() {
            let assigner_agent = agency::Agent {
                id: assigner_id.clone(),
                role_id: assigner_role.id.clone(),
                tradeoff_id: assigner_tradeoff.id.clone(),
                name: "Default Assigner".to_string(),
                performance: PerformanceRecord::default(),
                lineage: Lineage::default(),
                capabilities: vec![],
                rate: None,
                capacity: None,
                trust_level: Default::default(),
                contact: None,
                executor: "claude".to_string(),
                attractor_weight: 0.5,
                deployment_history: vec![],
                staleness_flags: vec![],
            };
            agency::save_agent(&assigner_agent, &agents_dir).unwrap();
        }

        // Configure the assigner_agent in config with auto_evaluate enabled
        let mut config = Config::load_or_default(dir);
        config.agency.auto_evaluate = true;
        config.agency.assigner_agent = Some(assigner_id.clone());
        config.save(dir).unwrap();

        (actor_id, assigner_id)
    }

    /// (1) Simulate an inline assign execution and verify it succeeds.
    #[test]
    fn test_assign_records_assigner_evaluation() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (actor_id, assigner_id) = setup_agency_with_assigner(dir_path);

        // Run assign — this triggers record_assigner_evaluation internally
        let result = run(dir_path, "t1", Some(&actor_id), false);
        assert!(result.is_ok(), "assign failed: {:?}", result.err());

        // Verify the evaluation JSON file was created
        let evals_dir = dir_path.join("agency/evaluations");
        let eval_files: Vec<_> = fs::read_dir(&evals_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("eval-assign-t1-")
            })
            .collect();
        assert_eq!(
            eval_files.len(),
            1,
            "Expected exactly one evaluation file for assign-t1, got {}",
            eval_files.len()
        );

        // Load and verify the evaluation contents
        let eval = agency::load_evaluation(&eval_files[0].path()).unwrap();
        assert_eq!(eval.task_id, "assign-t1");
        assert_eq!(eval.agent_id, assigner_id, "Evaluation should be recorded against the assigner agent");
        assert_eq!(eval.source, "system");
        assert_eq!(eval.score, 0.5, "Placeholder score should be 0.5");
    }

    /// (2) Verify the Evaluation is recorded against the assigner agent hash,
    /// not the actor agent.
    #[test]
    fn test_evaluation_recorded_against_assigner_not_actor() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (actor_id, assigner_id) = setup_agency_with_assigner(dir_path);

        run(dir_path, "t1", Some(&actor_id), false).unwrap();

        // Load the assigner agent and verify it has the evaluation
        let agents_dir = dir_path.join("agency/cache/agents");
        let assigner = agency::find_agent_by_prefix(&agents_dir, &assigner_id).unwrap();
        assert_eq!(
            assigner.performance.evaluations.len(),
            1,
            "Assigner agent should have exactly 1 evaluation"
        );
        assert_eq!(assigner.performance.evaluations[0].task_id, "assign-t1");

        // The actor agent should NOT have any evaluation from this assignment
        let actor = agency::find_agent_by_prefix(&agents_dir, &actor_id).unwrap();
        assert_eq!(
            actor.performance.evaluations.len(),
            0,
            "Actor agent should NOT have evaluations from assigner recording"
        );
    }

    /// (3) Verify the assigner's PerformanceRecord.task_count increments.
    #[test]
    fn test_assigner_task_count_increments() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![
                make_task("t1", "First task"),
                make_task("t2", "Second task"),
                make_task("t3", "Third task"),
            ],
        );
        let (actor_id, assigner_id) = setup_agency_with_assigner(dir_path);

        let agents_dir = dir_path.join("agency/cache/agents");

        // Before any assignments
        let assigner = agency::find_agent_by_prefix(&agents_dir, &assigner_id).unwrap();
        assert_eq!(assigner.performance.task_count, 0);

        // First assignment
        run(dir_path, "t1", Some(&actor_id), false).unwrap();
        let assigner = agency::find_agent_by_prefix(&agents_dir, &assigner_id).unwrap();
        assert_eq!(assigner.performance.task_count, 1, "task_count should be 1 after first assign");

        // Second assignment
        run(dir_path, "t2", Some(&actor_id), false).unwrap();
        let assigner = agency::find_agent_by_prefix(&agents_dir, &assigner_id).unwrap();
        assert_eq!(assigner.performance.task_count, 2, "task_count should be 2 after second assign");

        // Third assignment
        run(dir_path, "t3", Some(&actor_id), false).unwrap();
        let assigner = agency::find_agent_by_prefix(&agents_dir, &assigner_id).unwrap();
        assert_eq!(assigner.performance.task_count, 3, "task_count should be 3 after third assign");

        // Verify avg_score is 0.5 (all assignments use placeholder score 0.5)
        assert!(
            (assigner.performance.avg_score.unwrap() - 0.5).abs() < 1e-10,
            "All assignments use placeholder 0.5, avg should be 0.5"
        );
    }

    /// (4) Verify score propagates through the 6-step cascade to the
    /// assigner's role components.
    ///
    /// The 6-step cascade in record_evaluation:
    ///   1. Save evaluation JSON
    ///   2. Update agent performance
    ///   3. Update role performance
    ///   4. Update tradeoff performance
    ///   5. Propagate to each role component
    ///   6. Propagate to the role's desired outcome
    #[test]
    fn test_score_propagates_through_cascade_to_components() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (actor_id, assigner_id) = setup_agency_with_assigner(dir_path);

        // Run assign to trigger the cascade
        run(dir_path, "t1", Some(&actor_id), false).unwrap();

        let agency_dir = dir_path.join("agency");
        let agents_dir = agency_dir.join("cache/agents");
        let roles_dir = agency_dir.join("cache/roles");
        let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
        let components_dir = agency_dir.join("primitives/components");
        let outcomes_dir = agency_dir.join("primitives/outcomes");

        // Step 2: Agent performance updated
        let assigner = agency::find_agent_by_prefix(&agents_dir, &assigner_id).unwrap();
        assert_eq!(assigner.performance.task_count, 1);
        assert!((assigner.performance.avg_score.unwrap() - 0.5).abs() < 1e-10);

        // Step 3: Role performance updated
        let role = agency::find_role_by_prefix(&roles_dir, &assigner.role_id).unwrap();
        assert_eq!(
            role.performance.task_count, 1,
            "Role should have task_count=1 after cascade"
        );
        assert!((role.performance.avg_score.unwrap() - 0.5).abs() < 1e-10);
        // Role's context_id should be the tradeoff_id
        assert_eq!(
            role.performance.evaluations[0].context_id,
            assigner.tradeoff_id,
            "Role eval context_id should be tradeoff_id"
        );

        // Step 4: Tradeoff performance updated
        let tradeoff =
            agency::find_tradeoff_by_prefix(&tradeoffs_dir, &assigner.tradeoff_id).unwrap();
        assert_eq!(
            tradeoff.performance.task_count, 1,
            "Tradeoff should have task_count=1 after cascade"
        );
        assert!((tradeoff.performance.avg_score.unwrap() - 0.5).abs() < 1e-10);
        // Tradeoff's context_id should be the role_id
        assert_eq!(
            tradeoff.performance.evaluations[0].context_id,
            assigner.role_id,
            "Tradeoff eval context_id should be role_id"
        );

        // Step 5: Each role component's performance updated
        let assigner_comps = agency::assigner_components();
        assert!(
            !role.component_ids.is_empty(),
            "Assigner role should have components"
        );
        for comp_id in &role.component_ids {
            let component = agency::find_component_by_prefix(&components_dir, comp_id).unwrap();
            assert_eq!(
                component.performance.task_count,
                1,
                "Component '{}' ({}) should have task_count=1 after cascade",
                component.name,
                agency::short_hash(&component.id)
            );
            assert!(
                (component.performance.avg_score.unwrap() - 0.5).abs() < 1e-10,
                "Component '{}' avg_score should be 0.5",
                component.name
            );
            // Component's context_id should be the role_id
            assert_eq!(
                component.performance.evaluations[0].context_id,
                assigner.role_id,
                "Component '{}' context_id should be role_id",
                component.name
            );
        }
        // Verify all expected assigner components were touched
        assert_eq!(
            role.component_ids.len(),
            assigner_comps.len(),
            "Role should reference all {} assigner components",
            assigner_comps.len()
        );

        // Step 6: Desired outcome performance updated
        assert!(
            !role.outcome_id.is_empty(),
            "Assigner role should have an outcome_id"
        );
        let outcome = agency::find_outcome_by_prefix(&outcomes_dir, &role.outcome_id).unwrap();
        assert_eq!(
            outcome.performance.task_count, 1,
            "Outcome should have task_count=1 after cascade"
        );
        assert!(
            (outcome.performance.avg_score.unwrap() - 0.5).abs() < 1e-10,
            "Outcome avg_score should be 0.5"
        );
        // Outcome's context_id should be the agent_id
        assert_eq!(
            outcome.performance.evaluations[0].context_id,
            assigner.id,
            "Outcome eval context_id should be agent_id"
        );
    }

    /// Verify no evaluation is recorded when auto_evaluate is disabled.
    #[test]
    fn test_no_evaluation_when_auto_evaluate_disabled() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (actor_id, assigner_id) = setup_agency_with_assigner(dir_path);

        // Disable auto_evaluate
        let mut config = Config::load_or_default(dir_path);
        config.agency.auto_evaluate = false;
        config.save(dir_path).unwrap();

        run(dir_path, "t1", Some(&actor_id), false).unwrap();

        // Assigner should have no evaluations
        let agents_dir = dir_path.join("agency/cache/agents");
        let assigner = agency::find_agent_by_prefix(&agents_dir, &assigner_id).unwrap();
        assert_eq!(
            assigner.performance.task_count, 0,
            "No evaluation should be recorded when auto_evaluate is disabled"
        );
    }

    /// Verify no evaluation is recorded when no assigner_agent is configured.
    #[test]
    fn test_no_evaluation_when_no_assigner_configured() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (actor_id, _assigner_id) = setup_agency_with_assigner(dir_path);

        // Remove assigner_agent from config
        let mut config = Config::load_or_default(dir_path);
        config.agency.assigner_agent = None;
        config.save(dir_path).unwrap();

        run(dir_path, "t1", Some(&actor_id), false).unwrap();

        // No evaluation files should be created for assign-t1
        let evals_dir = dir_path.join("agency/evaluations");
        let eval_files: Vec<_> = fs::read_dir(&evals_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("eval-assign-t1-")
            })
            .collect();
        assert_eq!(
            eval_files.len(),
            0,
            "No evaluation should be recorded when assigner_agent is not configured"
        );
    }

    /// Verify multiple assignments accumulate correctly with the cascade.
    #[test]
    fn test_multiple_assignments_cascade_accumulates() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Task one"), make_task("t2", "Task two")],
        );
        let (actor_id, assigner_id) = setup_agency_with_assigner(dir_path);

        run(dir_path, "t1", Some(&actor_id), false).unwrap();
        run(dir_path, "t2", Some(&actor_id), false).unwrap();

        let agency_dir = dir_path.join("agency");

        // Agent should have 2 evaluations
        let assigner =
            agency::find_agent_by_prefix(&agency_dir.join("cache/agents"), &assigner_id).unwrap();
        assert_eq!(assigner.performance.task_count, 2);
        assert_eq!(assigner.performance.evaluations.len(), 2);

        // Role should also have 2
        let role =
            agency::find_role_by_prefix(&agency_dir.join("cache/roles"), &assigner.role_id)
                .unwrap();
        assert_eq!(role.performance.task_count, 2);

        // Each component should have 2
        for comp_id in &role.component_ids {
            let comp = agency::find_component_by_prefix(
                &agency_dir.join("primitives/components"),
                comp_id,
            )
            .unwrap();
            assert_eq!(
                comp.performance.task_count, 2,
                "Component '{}' should have task_count=2 after 2 assignments",
                comp.name
            );
        }

        // Outcome should have 2
        let outcome = agency::find_outcome_by_prefix(
            &agency_dir.join("primitives/outcomes"),
            &role.outcome_id,
        )
        .unwrap();
        assert_eq!(outcome.performance.task_count, 2);
    }
}
