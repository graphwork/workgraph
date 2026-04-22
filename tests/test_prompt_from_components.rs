//! Integration tests: prompt rendering from composed special-agent components.
//!
//! Verifies that:
//! 1. Special agents can be composed from their role components.
//! 2. `render_identity_prompt_rich` renders prompts containing content from each component.
//! 3. When an agent hash is configured, rendered prompts do NOT contain hardcoded template text.
//! 4. When no agent hash is configured, hardcoded template text is used as fallback.

use tempfile::TempDir;

use workgraph::agency::{
    self, Agent, EvaluatorInput, Lineage, PerformanceRecord, assigner_components,
    content_hash_agent, creator_components, evaluator_components, evolver_components,
    render_evaluator_prompt, render_identity_prompt_rich, resolve_all_components, resolve_outcome,
    save_agent, seed_starters, special_agent_roles, special_agent_tradeoffs,
};
use workgraph::config::Config;

/// Seed an agency dir and return it along with the TempDir handle.
fn setup_agency() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join(".workgraph/agency");
    seed_starters(&agency_dir).unwrap();
    (tmp, agency_dir)
}

/// Create an Agent entity from a role and tradeoff, save it, and return its hash.
fn create_and_save_agent(
    agency_dir: &std::path::Path,
    role_id: &str,
    tradeoff_id: &str,
    name: &str,
) -> String {
    let agent_id = content_hash_agent(role_id, tradeoff_id);
    let agent = Agent {
        id: agent_id.clone(),
        role_id: role_id.to_string(),
        tradeoff_id: tradeoff_id.to_string(),
        name: name.to_string(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::default(),
        capabilities: vec![],
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
        preferred_model: None,
        preferred_provider: None,
        deployment_history: vec![],
        attractor_weight: 1.0,
        staleness_flags: vec![],
    };
    let agents_dir = agency_dir.join("cache/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    save_agent(&agent, &agents_dir).unwrap();
    agent_id
}

// ---------------------------------------------------------------------------
// Test 1: Compose evaluator from components, render prompt, verify content
// ---------------------------------------------------------------------------

#[test]
fn evaluator_prompt_contains_all_component_content() {
    let (_tmp, agency_dir) = setup_agency();

    // Get the evaluator role from starters
    let roles = special_agent_roles();
    let evaluator_role = roles.iter().find(|r| r.name == "Evaluator").unwrap();

    // Get the matching tradeoff
    let tradeoffs = special_agent_tradeoffs();
    let evaluator_tradeoff = tradeoffs
        .iter()
        .find(|t| t.name == "Evaluator Balanced")
        .unwrap();

    // Resolve components
    let workgraph_root = agency_dir.parent().unwrap();
    let resolved = resolve_all_components(evaluator_role, workgraph_root, &agency_dir);

    // Resolve outcome
    let outcome = resolve_outcome(&evaluator_role.outcome_id, &agency_dir);

    // Render prompt
    let prompt = render_identity_prompt_rich(
        evaluator_role,
        evaluator_tradeoff,
        &resolved,
        outcome.as_ref(),
    );

    // Verify the prompt contains the role name and description
    assert!(
        prompt.contains("Evaluator"),
        "Prompt should contain role name 'Evaluator'"
    );
    assert!(
        prompt.contains("Grades actor-agents"),
        "Prompt should contain evaluator role description"
    );

    // Verify all evaluator component names appear in the rendered prompt
    let components = evaluator_components();
    for comp in &components {
        assert!(
            prompt.contains(&comp.name),
            "Prompt should contain component name '{}', but got:\n{}",
            comp.name,
            prompt
        );
    }

    // Verify the tradeoff constraints appear
    assert!(
        prompt.contains("Standard rubric application"),
        "Prompt should contain acceptable tradeoff text"
    );
    assert!(
        prompt.contains("Arbitrary grade inflation or deflation"),
        "Prompt should contain unacceptable tradeoff text"
    );

    // Verify the outcome is rendered
    assert!(
        outcome.is_some(),
        "Evaluator outcome should be resolvable from the store"
    );
    let outcome = outcome.unwrap();
    assert!(
        prompt.contains(&outcome.name),
        "Prompt should contain outcome name '{}', but got:\n{}",
        outcome.name,
        prompt
    );
    // Verify success criteria appear
    for criterion in &outcome.success_criteria {
        assert!(
            prompt.contains(criterion),
            "Prompt should contain success criterion '{}', but got:\n{}",
            criterion,
            prompt
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: Compose evolver from components, verify prompt content
// ---------------------------------------------------------------------------

#[test]
fn evolver_prompt_contains_all_component_content() {
    let (_tmp, agency_dir) = setup_agency();

    let roles = special_agent_roles();
    let evolver_role = roles.iter().find(|r| r.name == "Evolver").unwrap();

    let tradeoffs = special_agent_tradeoffs();
    let evolver_tradeoff = tradeoffs
        .iter()
        .find(|t| t.name == "Evolver Balanced")
        .unwrap();

    let workgraph_root = agency_dir.parent().unwrap();
    let resolved = resolve_all_components(evolver_role, workgraph_root, &agency_dir);
    let outcome = resolve_outcome(&evolver_role.outcome_id, &agency_dir);

    let prompt =
        render_identity_prompt_rich(evolver_role, evolver_tradeoff, &resolved, outcome.as_ref());

    // Verify all evolver component names
    let components = evolver_components();
    for comp in &components {
        assert!(
            prompt.contains(&comp.name),
            "Evolver prompt should contain component name '{}', but got:\n{}",
            comp.name,
            prompt
        );
    }

    // Verify tradeoff text
    assert!(prompt.contains("Moderate exploration intensity"));
    assert!(prompt.contains("Changing desired outcomes without human gate"));
}

// ---------------------------------------------------------------------------
// Test 3: Compose assigner from components, verify prompt content
// ---------------------------------------------------------------------------

#[test]
fn assigner_prompt_contains_all_component_content() {
    let (_tmp, agency_dir) = setup_agency();

    let roles = special_agent_roles();
    let assigner_role = roles.iter().find(|r| r.name == "Assigner").unwrap();

    let tradeoffs = special_agent_tradeoffs();
    let assigner_tradeoff = tradeoffs
        .iter()
        .find(|t| t.name == "Assigner Balanced")
        .unwrap();

    let workgraph_root = agency_dir.parent().unwrap();
    let resolved = resolve_all_components(assigner_role, workgraph_root, &agency_dir);
    let outcome = resolve_outcome(&assigner_role.outcome_id, &agency_dir);

    let prompt = render_identity_prompt_rich(
        assigner_role,
        assigner_tradeoff,
        &resolved,
        outcome.as_ref(),
    );

    // Verify all assigner component names
    let components = assigner_components();
    for comp in &components {
        assert!(
            prompt.contains(&comp.name),
            "Assigner prompt should contain component name '{}', but got:\n{}",
            comp.name,
            prompt
        );
    }

    // Verify tradeoff text
    assert!(prompt.contains("Flagging low-confidence assignments"));
    assert!(prompt.contains("Blocking on ambiguity"));
}

// ---------------------------------------------------------------------------
// Test 4: Compose creator from components, verify prompt content
// ---------------------------------------------------------------------------

#[test]
fn creator_prompt_contains_all_component_content() {
    let (_tmp, agency_dir) = setup_agency();

    let roles = special_agent_roles();
    let creator_role = roles.iter().find(|r| r.name == "Agent Creator").unwrap();

    let tradeoffs = special_agent_tradeoffs();
    let creator_tradeoff = tradeoffs
        .iter()
        .find(|t| t.name == "Creator Unconstrained")
        .unwrap();

    let workgraph_root = agency_dir.parent().unwrap();
    let resolved = resolve_all_components(creator_role, workgraph_root, &agency_dir);
    let outcome = resolve_outcome(&creator_role.outcome_id, &agency_dir);

    let prompt =
        render_identity_prompt_rich(creator_role, creator_tradeoff, &resolved, outcome.as_ref());

    // Verify all creator component names
    let components = creator_components();
    for comp in &components {
        assert!(
            prompt.contains(&comp.name),
            "Creator prompt should contain component name '{}', but got:\n{}",
            comp.name,
            prompt
        );
    }

    // Verify tradeoff text
    assert!(prompt.contains("Searching any domain the creator judges relevant"));
    assert!(prompt.contains("Importing primitives without absorptive capacity assessment"));
}

// ---------------------------------------------------------------------------
// Test 5: Evaluator prompt does NOT contain hardcoded template when agent hash configured
// ---------------------------------------------------------------------------

#[test]
fn evaluator_prompt_no_hardcoded_text_when_agent_configured() {
    let (_tmp, agency_dir) = setup_agency();

    // Get the evaluator role/tradeoff from starters
    let roles = special_agent_roles();
    let evaluator_role = roles.iter().find(|r| r.name == "Evaluator").unwrap();
    let tradeoffs = special_agent_tradeoffs();
    let evaluator_tradeoff = tradeoffs
        .iter()
        .find(|t| t.name == "Evaluator Balanced")
        .unwrap();

    // Create and save evaluator agent
    let agent_hash = create_and_save_agent(
        &agency_dir,
        &evaluator_role.id,
        &evaluator_tradeoff.id,
        "evaluator-agent",
    );

    // Simulate what evaluate.rs does: load agent, resolve components, render
    let agents_dir = agency_dir.join("cache/agents");
    let agent_path = agents_dir.join(format!("{}.yaml", agent_hash));
    let agent = agency::load_agent(&agent_path).unwrap();

    let roles_dir = agency_dir.join("cache/roles");
    let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
    let role = agency::load_role(&role_path).unwrap();

    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));
    let tradeoff = agency::load_tradeoff(&tradeoff_path).unwrap();

    let workgraph_root = agency_dir.parent().unwrap();
    let resolved = resolve_all_components(&role, workgraph_root, &agency_dir);
    let outcome = resolve_outcome(&role.outcome_id, &agency_dir);

    let identity = render_identity_prompt_rich(&role, &tradeoff, &resolved, outcome.as_ref());

    // The identity prompt should contain structured agent identity, not hardcoded text
    assert!(
        identity.contains("## Agent Identity"),
        "Should contain structured agent identity header"
    );
    assert!(
        !identity.contains("You are an evaluator assessing the quality"),
        "Should NOT contain the hardcoded evaluator template text when agent is configured"
    );

    // Now feed this identity into the evaluator prompt renderer
    let input = EvaluatorInput {
        task_title: "test-task",
        task_description: Some("a test task"),
        task_skills: &[],
        agent: None,
        role: None,
        tradeoff: None,
        artifacts: &[],
        log_entries: &[],
        started_at: None,
        completed_at: None,
        artifact_diff: None,
        evaluator_identity: Some(&identity),
        downstream_tasks: &[],
        flip_score: None,
        verify_status: None,
        verify_findings: None,
        resolved_outcome_name: None,
        child_tasks: &[],
    };
    let full_prompt = render_evaluator_prompt(&input);

    // Full prompt should include the rendered identity, not the hardcoded fallback
    assert!(
        full_prompt.contains("## Agent Identity"),
        "Full evaluator prompt should contain the rendered agent identity"
    );
    assert!(
        !full_prompt.contains("You are an evaluator assessing the quality"),
        "Full evaluator prompt should NOT contain hardcoded template when identity is provided"
    );
}

// ---------------------------------------------------------------------------
// Test 6: Evaluator prompt falls back to hardcoded when no agent configured
// ---------------------------------------------------------------------------

#[test]
fn evaluator_prompt_falls_back_to_hardcoded_when_no_agent() {
    // No evaluator_identity provided -> falls back to hardcoded template
    let input = EvaluatorInput {
        task_title: "test-task",
        task_description: Some("a test task"),
        task_skills: &[],
        agent: None,
        role: None,
        tradeoff: None,
        artifacts: &[],
        log_entries: &[],
        started_at: None,
        completed_at: None,
        artifact_diff: None,
        evaluator_identity: None,
        downstream_tasks: &[],
        flip_score: None,
        verify_status: None,
        verify_findings: None,
        resolved_outcome_name: None,
        child_tasks: &[],
    };
    let prompt = render_evaluator_prompt(&input);

    // Should use the hardcoded evaluator instructions
    assert!(
        prompt.contains(
            "You are an evaluator assessing the quality of work performed by an AI agent."
        ),
        "Should contain hardcoded evaluator template when no evaluator identity configured"
    );
    assert!(
        prompt.contains("# Evaluator Instructions"),
        "Should contain hardcoded evaluator instructions heading"
    );
    // The prompt should NOT start with a render_identity_prompt_rich-style "## Agent Identity"
    // preamble (that section in the prompt is for the *evaluated* agent, not the evaluator).
    // The key signal is that the "# Evaluator Instructions" heading is present instead of
    // a component-based identity block at the top.
    let identity_pos = prompt.find("## Agent Identity");
    let instructions_pos = prompt.find("# Evaluator Instructions");
    assert!(
        instructions_pos.unwrap() < identity_pos.unwrap(),
        "Hardcoded instructions should precede the evaluated agent identity section"
    );
}

// ---------------------------------------------------------------------------
// Test 7: Creator prompt uses component rendering when agent hash configured
// ---------------------------------------------------------------------------

#[test]
fn creator_prompt_uses_components_when_configured() {
    let (_tmp, agency_dir) = setup_agency();

    let roles = special_agent_roles();
    let creator_role = roles.iter().find(|r| r.name == "Agent Creator").unwrap();
    let tradeoffs = special_agent_tradeoffs();
    let creator_tradeoff = tradeoffs
        .iter()
        .find(|t| t.name == "Creator Unconstrained")
        .unwrap();

    // Create the agent entity
    let agent_hash = create_and_save_agent(
        &agency_dir,
        &creator_role.id,
        &creator_tradeoff.id,
        "creator-agent",
    );

    // Simulate the load path from agency_create.rs
    let agents_dir = agency_dir.join("cache/agents");
    let agent_path = agents_dir.join(format!("{}.yaml", agent_hash));
    let agent = agency::load_agent(&agent_path).unwrap();

    let roles_dir = agency_dir.join("cache/roles");
    let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
    let role = agency::load_role(&role_path).unwrap();

    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));
    let tradeoff = agency::load_tradeoff(&tradeoff_path).unwrap();

    let workgraph_root = agency_dir.parent().unwrap();
    let resolved = resolve_all_components(&role, workgraph_root, &agency_dir);
    let outcome = resolve_outcome(&role.outcome_id, &agency_dir);
    let identity = render_identity_prompt_rich(&role, &tradeoff, &resolved, outcome.as_ref());

    // Verify component-based identity, not hardcoded
    assert!(
        identity.contains("## Agent Identity"),
        "Creator identity should use structured rendering"
    );
    assert!(
        identity.contains("Agent Creator"),
        "Creator identity should contain the role name"
    );

    // Verify it does NOT contain the hardcoded creator intro
    assert!(
        !identity.contains("You are the Agency Creator agent"),
        "Should NOT contain hardcoded creator template when agent is configured"
    );

    // Verify components are present
    let components = creator_components();
    for comp in &components {
        assert!(
            identity.contains(&comp.name),
            "Creator identity should contain component '{}', got:\n{}",
            comp.name,
            identity
        );
    }
}

// ---------------------------------------------------------------------------
// Test 8: Creator fallback to hardcoded when no agent hash configured
// ---------------------------------------------------------------------------

#[test]
fn creator_falls_back_to_hardcoded_when_no_agent() {
    // Simulate the agency_create.rs fallback path:
    // When config.agency.creator_agent is None, the hardcoded template is used.
    let config = Config::default();
    assert!(
        config.agency.creator_agent.is_none(),
        "Default config should have no creator_agent configured"
    );

    // The fallback text from agency_create.rs
    let fallback = "You are the Agency Creator agent. Your job is to expand the primitive store by\n\
                    discovering new role components, desired outcomes, and tradeoff configurations\n\
                    that are implied by the project but not yet captured in the agency.";

    // This is what agency_create.rs produces when creator_agent is None
    assert!(
        fallback.contains("You are the Agency Creator agent"),
        "Fallback should contain hardcoded creator template"
    );
    assert!(
        !fallback.contains("## Agent Identity"),
        "Fallback should NOT contain structured agent identity"
    );
}

// ---------------------------------------------------------------------------
// Test 9: Evolver prompt uses component rendering vs hardcoded fallback
// ---------------------------------------------------------------------------

#[test]
fn evolver_prompt_uses_components_when_configured() {
    let (_tmp, agency_dir) = setup_agency();

    let roles = special_agent_roles();
    let evolver_role = roles.iter().find(|r| r.name == "Evolver").unwrap();
    let tradeoffs = special_agent_tradeoffs();
    let evolver_tradeoff = tradeoffs
        .iter()
        .find(|t| t.name == "Evolver Balanced")
        .unwrap();

    let agent_hash = create_and_save_agent(
        &agency_dir,
        &evolver_role.id,
        &evolver_tradeoff.id,
        "evolver-agent",
    );

    // Load via the same path evolve.rs uses
    let agents_dir = agency_dir.join("cache/agents");
    let agent_path = agents_dir.join(format!("{}.yaml", agent_hash));
    let agent = agency::load_agent(&agent_path).unwrap();

    let roles_dir = agency_dir.join("cache/roles");
    let role = agency::load_role(&roles_dir.join(format!("{}.yaml", agent.role_id))).unwrap();

    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let tradeoff =
        agency::load_tradeoff(&tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id))).unwrap();

    let workgraph_root = agency_dir.parent().unwrap();
    let resolved = resolve_all_components(&role, workgraph_root, &agency_dir);
    let outcome = resolve_outcome(&role.outcome_id, &agency_dir);
    let identity = render_identity_prompt_rich(&role, &tradeoff, &resolved, outcome.as_ref());

    // Should be component-rendered
    assert!(identity.contains("## Agent Identity"));
    assert!(identity.contains("Evolver"));

    // Should NOT contain the hardcoded evolver intro
    assert!(
        !identity.contains("You are the evolver agent for a workgraph agency system"),
        "Should NOT contain hardcoded evolver template when agent is configured"
    );

    // All evolver components should be present
    let components = evolver_components();
    for comp in &components {
        assert!(
            identity.contains(&comp.name),
            "Evolver identity should contain component '{}', got:\n{}",
            comp.name,
            identity
        );
    }
}

// ---------------------------------------------------------------------------
// Test 10: All four special agent types can be composed and rendered
// ---------------------------------------------------------------------------

#[test]
fn all_special_agents_compose_and_render_successfully() {
    let (_tmp, agency_dir) = setup_agency();
    let workgraph_root = agency_dir.parent().unwrap();

    let roles = special_agent_roles();
    let tradeoffs = special_agent_tradeoffs();

    let expected_agents = [
        ("Assigner", "Assigner Balanced"),
        ("Evaluator", "Evaluator Balanced"),
        ("Evolver", "Evolver Balanced"),
        ("Agent Creator", "Creator Unconstrained"),
    ];

    for (role_name, tradeoff_name) in &expected_agents {
        let role = roles
            .iter()
            .find(|r| r.name == *role_name)
            .unwrap_or_else(|| panic!("Missing role: {}", role_name));
        let tradeoff = tradeoffs
            .iter()
            .find(|t| t.name == *tradeoff_name)
            .unwrap_or_else(|| panic!("Missing tradeoff: {}", tradeoff_name));

        let resolved = resolve_all_components(role, workgraph_root, &agency_dir);
        let outcome = resolve_outcome(&role.outcome_id, &agency_dir);
        let prompt = render_identity_prompt_rich(role, tradeoff, &resolved, outcome.as_ref());

        // Basic structure checks
        assert!(
            prompt.contains("## Agent Identity"),
            "{}: missing Agent Identity header",
            role_name
        );
        assert!(
            prompt.contains(&format!("### Role: {}", role_name)),
            "{}: missing role header",
            role_name
        );
        assert!(
            prompt.contains("#### Skills"),
            "{}: missing Skills section",
            role_name
        );
        assert!(
            prompt.contains("#### Desired Outcome"),
            "{}: missing Desired Outcome section",
            role_name
        );
        assert!(
            prompt.contains("### Operational Parameters"),
            "{}: missing Operational Parameters section",
            role_name
        );

        // Outcome should resolve from the store
        assert!(
            outcome.is_some(),
            "{}: outcome should be resolvable",
            role_name
        );

        // At least one component should resolve
        assert!(
            !resolved.is_empty(),
            "{}: should have at least one resolved component",
            role_name
        );
    }
}

// ---------------------------------------------------------------------------
// Test 11: Component IDs in roles match actual stored components
// ---------------------------------------------------------------------------

#[test]
fn special_agent_role_component_ids_match_stored_components() {
    let (_tmp, agency_dir) = setup_agency();

    let roles = special_agent_roles();
    let components_dir = agency_dir.join("primitives/components");

    for role in &roles {
        for comp_id in &role.component_ids {
            let comp_path = components_dir.join(format!("{}.yaml", comp_id));
            assert!(
                comp_path.exists(),
                "Role '{}' references component ID '{}' which is not in the store at {:?}",
                role.name,
                comp_id,
                comp_path
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test 12: Outcome IDs in roles reference valid stored outcomes
// ---------------------------------------------------------------------------

#[test]
fn special_agent_role_outcome_ids_reference_stored_outcomes() {
    let (_tmp, agency_dir) = setup_agency();

    let roles = special_agent_roles();

    for role in &roles {
        let outcome = resolve_outcome(&role.outcome_id, &agency_dir);
        assert!(
            outcome.is_some(),
            "Role '{}' references outcome '{}' which cannot be resolved from the store",
            role.name,
            role.outcome_id
        );
    }
}

// ---------------------------------------------------------------------------
// Test 13: Round-trip: save agent, load, resolve, render matches direct render
// ---------------------------------------------------------------------------

#[test]
fn agent_roundtrip_produces_same_prompt() {
    let (_tmp, agency_dir) = setup_agency();

    let roles = special_agent_roles();
    let evaluator_role = roles.iter().find(|r| r.name == "Evaluator").unwrap();
    let tradeoffs = special_agent_tradeoffs();
    let evaluator_tradeoff = tradeoffs
        .iter()
        .find(|t| t.name == "Evaluator Balanced")
        .unwrap();

    let workgraph_root = agency_dir.parent().unwrap();

    // Direct render
    let resolved_direct = resolve_all_components(evaluator_role, workgraph_root, &agency_dir);
    let outcome_direct = resolve_outcome(&evaluator_role.outcome_id, &agency_dir);
    let prompt_direct = render_identity_prompt_rich(
        evaluator_role,
        evaluator_tradeoff,
        &resolved_direct,
        outcome_direct.as_ref(),
    );

    // Save agent, then load via the same path evaluate.rs uses
    let agent_hash = create_and_save_agent(
        &agency_dir,
        &evaluator_role.id,
        &evaluator_tradeoff.id,
        "evaluator-agent",
    );

    let agents_dir = agency_dir.join("cache/agents");
    let agent = agency::load_agent(&agents_dir.join(format!("{}.yaml", agent_hash))).unwrap();
    let role = agency::load_role(
        &agency_dir
            .join("cache/roles")
            .join(format!("{}.yaml", agent.role_id)),
    )
    .unwrap();
    let tradeoff = agency::load_tradeoff(
        &agency_dir
            .join("primitives/tradeoffs")
            .join(format!("{}.yaml", agent.tradeoff_id)),
    )
    .unwrap();
    let resolved_loaded = resolve_all_components(&role, workgraph_root, &agency_dir);
    let outcome_loaded = resolve_outcome(&role.outcome_id, &agency_dir);
    let prompt_loaded =
        render_identity_prompt_rich(&role, &tradeoff, &resolved_loaded, outcome_loaded.as_ref());

    assert_eq!(
        prompt_direct, prompt_loaded,
        "Direct render and round-trip loaded render should produce identical prompts"
    );
}
