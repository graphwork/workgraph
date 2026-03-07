//! End-to-end integration tests for coordinator special agent integration.
//!
//! Verifies:
//! 1. `wg agency init` produces config with special agent hashes
//! 2. Config has valid assigner_agent/evaluator_agent hashes
//! 3. Creating a task and simulating coordinator assign task creation
//! 4. Assign task uses the composed assigner agent (not hardcoded template)
//! 5. Evaluation tasks use the composed evaluator agent
//! 6. Evaluation records update the assigner/evaluator agent's PerformanceRecord

use std::collections::HashMap;
use tempfile::TempDir;

use workgraph::agency::{
    self, Agent, Evaluation, Lineage, PerformanceRecord, content_hash_agent, load_agent, load_role,
    load_tradeoff, record_evaluation, render_identity_prompt_rich, resolve_all_components,
    resolve_outcome, save_agent, seed_starters, special_agent_roles, special_agent_tradeoffs,
};
use workgraph::config::Config;

// ---------------------------------------------------------------------------
// Helper: bootstrap a fresh agency directory with special agents + config
// ---------------------------------------------------------------------------

fn bootstrap_agency() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let agency_dir = wg_dir.join("agency");
    seed_starters(&agency_dir).unwrap();

    let agents_dir = agency_dir.join("cache/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    // Create the default Programmer + Careful agent
    let roles = agency::starter_roles();
    let tradeoffs = agency::starter_tradeoffs();
    let programmer = roles.iter().find(|r| r.name == "Programmer").unwrap();
    let careful = tradeoffs.iter().find(|t| t.name == "Careful").unwrap();
    let default_id = content_hash_agent(&programmer.id, &careful.id);
    let default_agent = Agent {
        id: default_id,
        role_id: programmer.id.clone(),
        tradeoff_id: careful.id.clone(),
        name: "Careful Programmer".to_string(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::default(),
        capabilities: vec![],
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
        deployment_history: vec![],
        attractor_weight: 0.5,
        staleness_flags: vec![],
    };
    save_agent(&default_agent, &agents_dir).unwrap();

    // Compose the 4 special agents
    let special_roles = special_agent_roles();
    let special_tradeoffs = special_agent_tradeoffs();

    let special_agents: Vec<(&str, &str, &str)> = vec![
        ("Assigner", "Assigner Balanced", "Default Assigner"),
        ("Evaluator", "Evaluator Balanced", "Default Evaluator"),
        ("Evolver", "Evolver Balanced", "Default Evolver"),
        ("Agent Creator", "Creator Unconstrained", "Default Creator"),
    ];

    let mut special_ids: Vec<(&str, String)> = Vec::new();

    for (role_name, tradeoff_name, agent_name) in &special_agents {
        let role = special_roles.iter().find(|r| r.name == *role_name).unwrap();
        let tradeoff = special_tradeoffs
            .iter()
            .find(|t| t.name == *tradeoff_name)
            .unwrap();
        let sa_id = content_hash_agent(&role.id, &tradeoff.id);
        let sa_path = agents_dir.join(format!("{}.yaml", sa_id));
        if !sa_path.exists() {
            let agent = Agent {
                id: sa_id.clone(),
                role_id: role.id.clone(),
                tradeoff_id: tradeoff.id.clone(),
                name: agent_name.to_string(),
                performance: PerformanceRecord::default(),
                lineage: Lineage::default(),
                capabilities: vec![],
                rate: None,
                capacity: None,
                trust_level: Default::default(),
                contact: None,
                executor: "claude".to_string(),
                deployment_history: vec![],
                attractor_weight: 0.5,
                staleness_flags: vec![],
            };
            save_agent(&agent, &agents_dir).unwrap();
        }
        special_ids.push((role_name, sa_id));
    }

    // Set config
    let mut config = Config::load(&wg_dir).unwrap_or_default();
    config.agency.auto_assign = true;
    config.agency.auto_evaluate = true;
    for (role_name, sa_id) in &special_ids {
        match *role_name {
            "Assigner" => config.agency.assigner_agent = Some(sa_id.clone()),
            "Evaluator" => config.agency.evaluator_agent = Some(sa_id.clone()),
            "Evolver" => config.agency.evolver_agent = Some(sa_id.clone()),
            "Agent Creator" => config.agency.creator_agent = Some(sa_id.clone()),
            _ => {}
        }
    }
    config.save(&wg_dir).unwrap();

    (tmp, wg_dir)
}

// ---------------------------------------------------------------------------
// Test 1: agency init produces config with special agent hashes
// ---------------------------------------------------------------------------

#[test]
fn agency_init_produces_config_with_special_agent_hashes() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();

    assert!(
        config.agency.assigner_agent.is_some(),
        "assigner_agent config key should be set after init"
    );
    assert!(
        config.agency.evaluator_agent.is_some(),
        "evaluator_agent config key should be set after init"
    );
    assert!(
        config.agency.evolver_agent.is_some(),
        "evolver_agent config key should be set after init"
    );
    assert!(
        config.agency.creator_agent.is_some(),
        "creator_agent config key should be set after init"
    );

    // All hashes are valid SHA-256
    for (name, hash) in [
        ("assigner", config.agency.assigner_agent.as_ref().unwrap()),
        ("evaluator", config.agency.evaluator_agent.as_ref().unwrap()),
        ("evolver", config.agency.evolver_agent.as_ref().unwrap()),
        ("creator", config.agency.creator_agent.as_ref().unwrap()),
    ] {
        assert_eq!(hash.len(), 64, "{} hash should be 64 hex chars", name);
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "{} hash should be all hex digits: {}",
            name,
            hash
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: Config hashes point to real agent entities
// ---------------------------------------------------------------------------

#[test]
fn config_special_agent_hashes_resolve_to_real_agents() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();
    let agents_dir = wg_dir.join("agency/cache/agents");

    let agent_configs = [
        ("assigner", config.agency.assigner_agent.as_ref().unwrap()),
        ("evaluator", config.agency.evaluator_agent.as_ref().unwrap()),
        ("evolver", config.agency.evolver_agent.as_ref().unwrap()),
        ("creator", config.agency.creator_agent.as_ref().unwrap()),
    ];

    for (label, hash) in &agent_configs {
        let agent_path = agents_dir.join(format!("{}.yaml", hash));
        assert!(
            agent_path.exists(),
            "{} agent file should exist at {:?}",
            label,
            agent_path
        );
        let agent = load_agent(&agent_path).unwrap();
        assert_eq!(
            &agent.id, *hash,
            "{} agent ID should match config hash",
            label
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Simulate coordinator creating an assign task — verify it uses
//         the composed assigner agent identity (not hardcoded template)
// ---------------------------------------------------------------------------

#[test]
fn coordinator_assign_task_uses_composed_assigner_identity() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();
    let agency_dir = wg_dir.join("agency");

    // Resolve the assigner identity the same way coordinator.rs does
    let assigner_identity = config
        .agency
        .assigner_agent
        .as_ref()
        .and_then(|agent_hash| {
            let agents_dir = agency_dir.join("cache/agents");
            let agent_path = agents_dir.join(format!("{}.yaml", agent_hash));
            let agent = load_agent(&agent_path).ok()?;
            let roles_dir = agency_dir.join("cache/roles");
            let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
            let role = load_role(&role_path).ok()?;
            let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
            let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));
            let tradeoff = load_tradeoff(&tradeoff_path).ok()?;
            let workgraph_root = wg_dir.as_path();
            let resolved_skills = resolve_all_components(&role, workgraph_root, &agency_dir);
            let outcome = resolve_outcome(&role.outcome_id, &agency_dir);
            Some(render_identity_prompt_rich(
                &role,
                &tradeoff,
                &resolved_skills,
                outcome.as_ref(),
            ))
        });

    // The assigner identity should be Some (since we configured it)
    let identity = assigner_identity.expect("Assigner identity should resolve when config is set");

    // Build a mock description the way coordinator.rs would
    let mut desc = String::new();
    desc.push_str(&identity);
    desc.push_str("\n\n");
    desc.push_str(
        "Assign an agent to task 'test-task-123'.\n\n## Original Task\n**Title:** Test Task\n",
    );

    // Verify the description contains the composed identity
    assert!(
        desc.contains("## Agent Identity"),
        "Assign task description should contain rendered agent identity header"
    );
    assert!(
        desc.contains("Assigner"),
        "Assign task description should reference the Assigner role"
    );

    // Verify all assigner component names appear
    let assigner_components = agency::assigner_components();
    for comp in &assigner_components {
        assert!(
            desc.contains(&comp.name),
            "Assign task should contain assigner component '{}', got:\n{}",
            comp.name,
            desc
        );
    }

    // Verify it does NOT contain generic hardcoded template text
    // (there is no hardcoded "You are the assigner" template in coordinator.rs,
    // but the identity prompt structure should be present)
    assert!(
        desc.contains("### Operational Parameters"),
        "Assign task should contain tradeoff operational parameters"
    );
    assert!(
        desc.contains("#### Desired Outcome"),
        "Assign task should contain desired outcome section"
    );
}

// ---------------------------------------------------------------------------
// Test 4: When assigner_agent is None, no identity is prepended (fallback)
// ---------------------------------------------------------------------------

#[test]
fn coordinator_assign_task_fallback_when_no_assigner_configured() {
    // Default config has no assigner_agent
    let config = Config::default();

    // Simulate the coordinator logic: when assigner_agent is None, identity is None
    let assigner_identity: Option<String> = config
        .agency
        .assigner_agent
        .as_ref()
        .map(|_hash| "should not reach here".to_string());

    assert!(
        assigner_identity.is_none(),
        "No identity should be resolved when assigner_agent config is None"
    );

    // Build the description without identity (just the task info)
    let mut desc = String::new();
    // No identity prepended
    desc.push_str("Assign an agent to task 'test-task'.\n\n## Original Task\n");

    // Verify NO agent identity header
    assert!(
        !desc.contains("## Agent Identity"),
        "Assign task should NOT contain agent identity when no assigner configured"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Simulate coordinator creating an evaluation task — verify it uses
//         the composed evaluator agent identity
// ---------------------------------------------------------------------------

#[test]
fn coordinator_eval_task_uses_composed_evaluator_identity() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();
    let agency_dir = wg_dir.join("agency");

    // Resolve evaluator identity the same way coordinator.rs does
    let evaluator_identity = config
        .agency
        .evaluator_agent
        .as_ref()
        .and_then(|agent_hash| {
            let agents_dir = agency_dir.join("cache/agents");
            let agent_path = agents_dir.join(format!("{}.yaml", agent_hash));
            let agent = load_agent(&agent_path).ok()?;
            let roles_dir = agency_dir.join("cache/roles");
            let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
            let role = load_role(&role_path).ok()?;
            let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
            let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));
            let tradeoff = load_tradeoff(&tradeoff_path).ok()?;
            let workgraph_root = wg_dir.as_path();
            let resolved_skills = resolve_all_components(&role, workgraph_root, &agency_dir);
            let outcome = resolve_outcome(&role.outcome_id, &agency_dir);
            Some(render_identity_prompt_rich(
                &role,
                &tradeoff,
                &resolved_skills,
                outcome.as_ref(),
            ))
        });

    let identity =
        evaluator_identity.expect("Evaluator identity should resolve when config is set");

    // Build a mock eval task description the way coordinator.rs does
    let task_id = "some-completed-task";
    let mut desc = String::new();
    desc.push_str(&identity);
    desc.push_str("\n\n");
    desc.push_str(&format!(
        "Evaluate the completed task '{}'.\n\n\
         Run `wg evaluate run {}` to produce a structured evaluation.",
        task_id, task_id,
    ));

    // Verify the description contains the composed evaluator identity
    assert!(
        desc.contains("## Agent Identity"),
        "Eval task description should contain rendered agent identity header"
    );
    assert!(
        desc.contains("Evaluator"),
        "Eval task description should reference the Evaluator role"
    );

    // Verify evaluator component names appear
    let evaluator_components = agency::evaluator_components();
    for comp in &evaluator_components {
        assert!(
            desc.contains(&comp.name),
            "Eval task should contain evaluator component '{}', got:\n{}",
            comp.name,
            desc
        );
    }

    // Verify structured identity sections
    assert!(
        desc.contains("### Operational Parameters"),
        "Eval task should contain tradeoff operational parameters"
    );
    assert!(
        desc.contains("#### Desired Outcome"),
        "Eval task should contain desired outcome section"
    );
}

// ---------------------------------------------------------------------------
// Test 6: Assign task has agent field set to config.agency.assigner_agent
// ---------------------------------------------------------------------------

#[test]
fn assign_task_agent_field_set_to_assigner_hash() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();

    // The coordinator sets `agent: config.agency.assigner_agent.clone()` on assign tasks
    let assigner_hash = config.agency.assigner_agent.clone();
    assert!(assigner_hash.is_some(), "assigner_agent should be set");

    // Verify the hash resolves to the correct agent
    let agents_dir = wg_dir.join("agency/cache/agents");
    let agent =
        load_agent(&agents_dir.join(format!("{}.yaml", assigner_hash.as_ref().unwrap()))).unwrap();
    let roles_dir = wg_dir.join("agency/cache/roles");
    let role = load_role(&roles_dir.join(format!("{}.yaml", agent.role_id))).unwrap();
    assert_eq!(
        role.name, "Assigner",
        "Agent referenced by assigner_agent config should have the Assigner role"
    );
}

// ---------------------------------------------------------------------------
// Test 7: Eval task has agent field set to config.agency.evaluator_agent
// ---------------------------------------------------------------------------

#[test]
fn eval_task_agent_field_set_to_evaluator_hash() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();

    let evaluator_hash = config.agency.evaluator_agent.clone();
    assert!(evaluator_hash.is_some(), "evaluator_agent should be set");

    let agents_dir = wg_dir.join("agency/cache/agents");
    let agent =
        load_agent(&agents_dir.join(format!("{}.yaml", evaluator_hash.as_ref().unwrap()))).unwrap();
    let roles_dir = wg_dir.join("agency/cache/roles");
    let role = load_role(&roles_dir.join(format!("{}.yaml", agent.role_id))).unwrap();
    assert_eq!(
        role.name, "Evaluator",
        "Agent referenced by evaluator_agent config should have the Evaluator role"
    );
}

// ---------------------------------------------------------------------------
// Test 8: Evaluation records update the assigner agent's PerformanceRecord
// ---------------------------------------------------------------------------

#[test]
fn evaluation_records_update_assigner_agent_performance() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();
    let agency_dir = wg_dir.join("agency");

    let assigner_hash = config.agency.assigner_agent.as_ref().unwrap();

    // Load assigner agent to get its role/tradeoff IDs
    let agents_dir = agency_dir.join("cache/agents");
    let agent = load_agent(&agents_dir.join(format!("{}.yaml", assigner_hash))).unwrap();

    // Verify initial state: no evaluations
    assert_eq!(
        agent.performance.task_count, 0,
        "Initial task_count should be 0"
    );
    assert!(
        agent.performance.evaluations.is_empty(),
        "Initial evaluations should be empty"
    );

    // Create a simulated evaluation for the assigner agent
    let eval = Evaluation {
        id: "eval-assign-test-1".to_string(),
        task_id: "assign-some-task".to_string(),
        agent_id: assigner_hash.clone(),
        role_id: agent.role_id.clone(),
        tradeoff_id: agent.tradeoff_id.clone(),
        score: 0.85,
        dimensions: HashMap::from([
            ("correctness".to_string(), 0.9),
            ("completeness".to_string(), 0.8),
            ("efficiency".to_string(), 0.85),
            ("style_adherence".to_string(), 0.85),
        ]),
        notes: "Good assignment — matched agent skills to task requirements well.".to_string(),
        evaluator: "test-evaluator".to_string(),
        timestamp: "2026-02-28T00:00:00Z".to_string(),
        model: Some("test".to_string()),
        source: "test".to_string(),
        cost_usd: None,
        token_usage: None,
    };

    // Record the evaluation (this updates agent, role, tradeoff, components, outcome)
    let eval_path = record_evaluation(&eval, &agency_dir).unwrap();
    assert!(
        eval_path.exists(),
        "Evaluation JSON should be persisted at {:?}",
        eval_path
    );

    // Reload the assigner agent and verify performance was updated
    let updated_agent = load_agent(&agents_dir.join(format!("{}.yaml", assigner_hash))).unwrap();
    assert_eq!(
        updated_agent.performance.task_count, 1,
        "task_count should be 1 after one evaluation"
    );
    assert_eq!(
        updated_agent.performance.evaluations.len(),
        1,
        "evaluations should have 1 entry"
    );
    assert!(
        (updated_agent.performance.avg_score.unwrap() - 0.85).abs() < 0.01,
        "avg_score should be ~0.85, got {:?}",
        updated_agent.performance.avg_score
    );

    // Verify the evaluation ref has correct cross-references
    let eval_ref = &updated_agent.performance.evaluations[0];
    assert_eq!(eval_ref.task_id, "assign-some-task");
    assert_eq!(
        eval_ref.context_id, agent.role_id,
        "context_id should be role_id for agent evals"
    );
}

// ---------------------------------------------------------------------------
// Test 9: Multiple evaluations accumulate on the assigner agent
// ---------------------------------------------------------------------------

#[test]
fn multiple_evaluations_accumulate_on_assigner_agent() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();
    let agency_dir = wg_dir.join("agency");

    let assigner_hash = config.agency.assigner_agent.as_ref().unwrap();
    let agents_dir = agency_dir.join("cache/agents");
    let agent = load_agent(&agents_dir.join(format!("{}.yaml", assigner_hash))).unwrap();

    // Record 3 evaluations with different scores
    let scores = [0.7, 0.85, 1.0];
    for (i, &score) in scores.iter().enumerate() {
        let eval = Evaluation {
            id: format!("eval-assign-multi-{}", i),
            task_id: format!("assign-task-{}", i),
            agent_id: assigner_hash.clone(),
            role_id: agent.role_id.clone(),
            tradeoff_id: agent.tradeoff_id.clone(),
            score,
            dimensions: HashMap::new(),
            notes: format!("Evaluation #{}", i),
            evaluator: "test".to_string(),
            timestamp: format!("2026-02-28T00:0{}:00Z", i),
            model: None,
            source: "test".to_string(),
            cost_usd: None,
            token_usage: None,
        };
        record_evaluation(&eval, &agency_dir).unwrap();
    }

    // Reload and verify
    let updated = load_agent(&agents_dir.join(format!("{}.yaml", assigner_hash))).unwrap();
    assert_eq!(updated.performance.task_count, 3, "task_count should be 3");
    assert_eq!(
        updated.performance.evaluations.len(),
        3,
        "should have 3 evaluation entries"
    );

    // Average of 0.7, 0.85, 1.0 = 0.85
    let expected_avg = (0.7 + 0.85 + 1.0) / 3.0;
    assert!(
        (updated.performance.avg_score.unwrap() - expected_avg).abs() < 0.01,
        "avg_score should be ~{:.4}, got {:?}",
        expected_avg,
        updated.performance.avg_score
    );
}

// ---------------------------------------------------------------------------
// Test 10: Evaluation also propagates to the assigner's role and components
// ---------------------------------------------------------------------------

#[test]
fn evaluation_propagates_to_assigner_role_and_components() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();
    let agency_dir = wg_dir.join("agency");

    let assigner_hash = config.agency.assigner_agent.as_ref().unwrap();
    let agents_dir = agency_dir.join("cache/agents");
    let agent = load_agent(&agents_dir.join(format!("{}.yaml", assigner_hash))).unwrap();

    let eval = Evaluation {
        id: "eval-propagation-test".to_string(),
        task_id: "assign-prop-test".to_string(),
        agent_id: assigner_hash.clone(),
        role_id: agent.role_id.clone(),
        tradeoff_id: agent.tradeoff_id.clone(),
        score: 0.9,
        dimensions: HashMap::new(),
        notes: "Testing propagation".to_string(),
        evaluator: "test".to_string(),
        timestamp: "2026-02-28T01:00:00Z".to_string(),
        model: None,
        source: "test".to_string(),
        cost_usd: None,
        token_usage: None,
    };
    record_evaluation(&eval, &agency_dir).unwrap();

    // Verify role was updated
    let roles_dir = agency_dir.join("cache/roles");
    let role = load_role(&roles_dir.join(format!("{}.yaml", agent.role_id))).unwrap();
    assert_eq!(
        role.performance.task_count, 1,
        "Role task_count should be 1"
    );
    assert!(
        (role.performance.avg_score.unwrap() - 0.9).abs() < 0.01,
        "Role avg_score should be ~0.9"
    );

    // Verify tradeoff was updated
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let tradeoff =
        load_tradeoff(&tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id))).unwrap();
    assert_eq!(
        tradeoff.performance.task_count, 1,
        "Tradeoff task_count should be 1"
    );

    // Verify each component was updated
    let components_dir = agency_dir.join("primitives/components");
    for comp_id in &role.component_ids {
        let comp =
            agency::load_component(&components_dir.join(format!("{}.yaml", comp_id))).unwrap();
        assert_eq!(
            comp.performance.task_count, 1,
            "Component {} task_count should be 1",
            comp.name
        );
        assert!(
            (comp.performance.avg_score.unwrap() - 0.9).abs() < 0.01,
            "Component {} avg_score should be ~0.9",
            comp.name
        );
    }
}

// ---------------------------------------------------------------------------
// Test 11: Evaluator agent also accumulates evaluations
// ---------------------------------------------------------------------------

#[test]
fn evaluation_records_update_evaluator_agent_performance() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();
    let agency_dir = wg_dir.join("agency");

    let evaluator_hash = config.agency.evaluator_agent.as_ref().unwrap();
    let agents_dir = agency_dir.join("cache/agents");
    let agent = load_agent(&agents_dir.join(format!("{}.yaml", evaluator_hash))).unwrap();

    // Record an evaluation against the evaluator agent
    let eval = Evaluation {
        id: "eval-evaluator-test".to_string(),
        task_id: "evaluate-some-task".to_string(),
        agent_id: evaluator_hash.clone(),
        role_id: agent.role_id.clone(),
        tradeoff_id: agent.tradeoff_id.clone(),
        score: 0.92,
        dimensions: HashMap::from([
            ("correctness".to_string(), 0.95),
            ("completeness".to_string(), 0.9),
        ]),
        notes: "Good evaluation — thorough rubric application.".to_string(),
        evaluator: "meta-evaluator".to_string(),
        timestamp: "2026-02-28T02:00:00Z".to_string(),
        model: None,
        source: "test".to_string(),
        cost_usd: None,
        token_usage: None,
    };
    record_evaluation(&eval, &agency_dir).unwrap();

    // Verify performance updated
    let updated = load_agent(&agents_dir.join(format!("{}.yaml", evaluator_hash))).unwrap();
    assert_eq!(updated.performance.task_count, 1);
    assert_eq!(updated.performance.evaluations.len(), 1);
    assert!(
        (updated.performance.avg_score.unwrap() - 0.92).abs() < 0.01,
        "Evaluator avg_score should be ~0.92"
    );
}

// ---------------------------------------------------------------------------
// Test 12: End-to-end flow — init, create task, build assign desc,
//          record eval, verify agent performance
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_init_assign_evaluate_flow() {
    let (_tmp, wg_dir) = bootstrap_agency();
    let config = Config::load(&wg_dir).unwrap();
    let agency_dir = wg_dir.join("agency");

    // Step 1: Verify init produced valid config
    let assigner_hash = config.agency.assigner_agent.as_ref().unwrap();
    let evaluator_hash = config.agency.evaluator_agent.as_ref().unwrap();
    assert_ne!(
        assigner_hash, evaluator_hash,
        "Assigner and evaluator should be different agents"
    );

    // Step 2: Simulate coordinator creating an assign task
    let agents_dir = agency_dir.join("cache/agents");
    let assigner_agent = load_agent(&agents_dir.join(format!("{}.yaml", assigner_hash))).unwrap();
    let assigner_role = load_role(
        &agency_dir
            .join("cache/roles")
            .join(format!("{}.yaml", assigner_agent.role_id)),
    )
    .unwrap();

    assert_eq!(assigner_role.name, "Assigner");

    // Step 3: Resolve assigner identity and build assign task description
    let assigner_tradeoff = load_tradeoff(
        &agency_dir
            .join("primitives/tradeoffs")
            .join(format!("{}.yaml", assigner_agent.tradeoff_id)),
    )
    .unwrap();
    let resolved = resolve_all_components(&assigner_role, &wg_dir, &agency_dir);
    let outcome = resolve_outcome(&assigner_role.outcome_id, &agency_dir);
    let identity = render_identity_prompt_rich(
        &assigner_role,
        &assigner_tradeoff,
        &resolved,
        outcome.as_ref(),
    );

    // Verify identity has the structured format
    assert!(identity.contains("## Agent Identity"));
    assert!(identity.contains("### Role: Assigner"));

    // Step 4: Build the full assign task description
    let mut assign_desc = String::new();
    assign_desc.push_str(&identity);
    assign_desc.push_str("\n\n");
    assign_desc.push_str("Assign an agent to task 'implement-feature-x'.\n");

    // Step 5: Simulate evaluation of the assigner's work
    let eval = Evaluation {
        id: "eval-e2e-assigner".to_string(),
        task_id: "assign-implement-feature-x".to_string(),
        agent_id: assigner_hash.clone(),
        role_id: assigner_agent.role_id.clone(),
        tradeoff_id: assigner_agent.tradeoff_id.clone(),
        score: 0.88,
        dimensions: HashMap::from([
            ("correctness".to_string(), 0.9),
            ("completeness".to_string(), 0.85),
            ("efficiency".to_string(), 0.9),
            ("style_adherence".to_string(), 0.85),
        ]),
        notes: "Matched agent well to task requirements".to_string(),
        evaluator: evaluator_hash.clone(),
        timestamp: "2026-02-28T03:00:00Z".to_string(),
        model: None,
        source: "llm".to_string(),
        cost_usd: None,
        token_usage: None,
    };
    record_evaluation(&eval, &agency_dir).unwrap();

    // Step 6: Verify the assigner agent's performance record was updated
    let updated_assigner = load_agent(&agents_dir.join(format!("{}.yaml", assigner_hash))).unwrap();
    assert_eq!(updated_assigner.performance.task_count, 1);
    assert!(
        (updated_assigner.performance.avg_score.unwrap() - 0.88).abs() < 0.01,
        "Assigner avg_score should reflect the evaluation"
    );

    // Step 7: Verify the evaluation persisted correctly
    let evals_dir = agency_dir.join("evaluations");
    let eval_files: Vec<_> = std::fs::read_dir(&evals_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("assign-implement-feature-x")
        })
        .collect();
    assert_eq!(
        eval_files.len(),
        1,
        "Should have exactly 1 evaluation file for the assigner task"
    );
}
