//! Integration test: full agency lifecycle.
//!
//! Exercises the complete agency data model, storage, prompt rendering,
//! and evaluation recording end-to-end, using a tempdir for isolation.
//!
//! All roles and motivations use content-hash IDs (SHA-256 of immutable fields).

use std::collections::HashMap;
use tempfile::TempDir;

use workgraph::agency::{
    self, Agent, Evaluation, Lineage, PerformanceRecord, SkillRef,
};
use workgraph::graph::{LogEntry, Status, Task};

/// Helper: create a minimal Task for testing.
fn make_task(id: &str, title: &str, description: Option<&str>, tags: Vec<&str>, skills: Vec<&str>, verify: Option<&str>) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        description: description.map(|s| s.to_string()),
        status: Status::Open,
        assigned: None,
        estimate: None,
        blocks: vec![],
        blocked_by: vec![],
        requires: vec![],
        tags: tags.into_iter().map(|s| s.to_string()).collect(),
        skills: skills.into_iter().map(|s| s.to_string()).collect(),
        inputs: vec![],
        deliverables: vec![],
        artifacts: vec![],
        exec: None,
        not_before: None,
        created_at: None,
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries: None,
        failure_reason: None,
        model: None,
        verify: verify.map(|s| s.to_string()),
        agent: None,
        loops_to: vec![],
        loop_iteration: 0,
        ready_after: None,
    }
}

/// Full lifecycle test covering all major agency subsystems.
///
/// Steps:
/// 1. Initialize agency storage.
/// 2. Create a role and motivation via build_role / build_motivation (content-hash IDs).
/// 3. Create a task with an agent assigned.
/// 4. Render the identity prompt and verify it contains role skills and motivation constraints.
/// 5. Simulate task completion.
/// 6. Record an evaluation and verify performance records update.
/// 7. Run role-task matching and verify the role ranks appropriately.
/// 8. Verify motivation selection respects constraint compatibility.
#[test]
fn test_full_agency_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");

    // ---------------------------------------------------------------
    // Step 1: Initialize agency storage
    // ---------------------------------------------------------------
    agency::init(&agency_dir).unwrap();
    assert!(agency_dir.join("roles").is_dir());
    assert!(agency_dir.join("motivations").is_dir());
    assert!(agency_dir.join("evaluations").is_dir());

    // ---------------------------------------------------------------
    // Step 2: Create a role and motivation using content-hash IDs
    // ---------------------------------------------------------------
    let role = agency::build_role(
        "Rust Developer",
        "Writes, tests, and debugs Rust code.",
        vec![
            SkillRef::Name("rust".to_string()),
            SkillRef::Name("testing".to_string()),
            SkillRef::Inline("Write idiomatic Rust with proper error handling".to_string()),
        ],
        "Working, tested Rust code with proper error handling",
    );
    let role_id = role.id.clone();

    let motivation = agency::build_motivation(
        "Careful Quality",
        "Prioritizes reliability and correctness above speed.",
        vec![
            "Slower delivery".to_string(),
            "More verbose code".to_string(),
        ],
        vec![
            "Untested code".to_string(),
            "Skipping error handling".to_string(),
        ],
    );
    let motivation_id = motivation.id.clone();

    // Verify IDs are content hashes (64 hex chars = SHA-256)
    assert_eq!(role_id.len(), 64, "Role ID should be a full SHA-256 hex hash, got: {}", role_id);
    assert_eq!(motivation_id.len(), 64, "Motivation ID should be a full SHA-256 hex hash, got: {}", motivation_id);

    // Save and reload to verify storage round-trip
    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");

    agency::save_role(&role, &roles_dir).unwrap();
    agency::save_motivation(&motivation, &motivations_dir).unwrap();

    let loaded_roles = agency::load_all_roles(&roles_dir).unwrap();
    assert_eq!(loaded_roles.len(), 1);
    assert_eq!(loaded_roles[0].id, role_id);
    assert_eq!(loaded_roles[0].name, "Rust Developer");
    assert_eq!(loaded_roles[0].skills.len(), 3);

    let loaded_motivations = agency::load_all_motivations(&motivations_dir).unwrap();
    assert_eq!(loaded_motivations.len(), 1);
    assert_eq!(loaded_motivations[0].id, motivation_id);
    assert_eq!(loaded_motivations[0].unacceptable_tradeoffs.len(), 2);

    // ---------------------------------------------------------------
    // Step 3: Create a task with an agent assigned
    // ---------------------------------------------------------------
    let mut task = make_task(
        "impl-parser",
        "Implement the Rust parser module",
        Some("Write a parser for the configuration file format with proper error handling and tests"),
        vec!["rust", "parser"],
        vec!["rust", "testing"],
        Some("cargo test passes with no failures"),
    );
    let agent_id = agency::content_hash_agent(&role_id, &motivation_id);
    task.agent = Some(agent_id.clone());

    // ---------------------------------------------------------------
    // Step 4: Render the identity prompt and verify contents
    // ---------------------------------------------------------------
    let resolved_skills = agency::resolve_all_skills(&role, tmp.path());
    // We have Name("rust"), Name("testing"), and Inline(...)
    assert_eq!(resolved_skills.len(), 3);

    let prompt = agency::render_identity_prompt(&role, &motivation, &resolved_skills);

    // Verify the prompt contains the role name
    assert!(
        prompt.contains("Rust Developer"),
        "Prompt should contain the role name. Got:\n{}",
        prompt
    );
    // Verify skills are included
    assert!(
        prompt.contains("rust"),
        "Prompt should contain skill 'rust'. Got:\n{}",
        prompt
    );
    assert!(
        prompt.contains("testing"),
        "Prompt should contain skill 'testing'. Got:\n{}",
        prompt
    );
    assert!(
        prompt.contains("idiomatic Rust"),
        "Prompt should contain inline skill content. Got:\n{}",
        prompt
    );
    // Verify desired outcome
    assert!(
        prompt.contains("Working, tested Rust code"),
        "Prompt should contain desired outcome. Got:\n{}",
        prompt
    );
    // Verify motivation constraints (acceptable tradeoffs)
    assert!(
        prompt.contains("Slower delivery"),
        "Prompt should contain acceptable tradeoff. Got:\n{}",
        prompt
    );
    // Verify non-negotiable constraints (unacceptable tradeoffs)
    assert!(
        prompt.contains("Untested code"),
        "Prompt should contain non-negotiable constraint. Got:\n{}",
        prompt
    );
    assert!(
        prompt.contains("Skipping error handling"),
        "Prompt should contain non-negotiable constraint. Got:\n{}",
        prompt
    );

    // ---------------------------------------------------------------
    // Step 5: Simulate task completion
    // ---------------------------------------------------------------
    task.status = Status::Done;
    task.completed_at = Some("2025-01-15T10:30:00Z".to_string());
    task.artifacts = vec!["src/parser.rs".to_string(), "tests/parser_test.rs".to_string()];

    assert_eq!(task.status, Status::Done);
    assert_eq!(task.artifacts.len(), 2);

    // ---------------------------------------------------------------
    // Step 6: Record an evaluation and verify performance records update
    // ---------------------------------------------------------------
    let mut dimensions = HashMap::new();
    dimensions.insert("correctness".to_string(), 0.9);
    dimensions.insert("completeness".to_string(), 0.85);
    dimensions.insert("efficiency".to_string(), 0.8);
    dimensions.insert("style_adherence".to_string(), 0.95);

    let evaluation = Evaluation {
        id: "eval-impl-parser-1".to_string(),
        task_id: "impl-parser".to_string(),
        agent_id: String::new(),
        role_id: role_id.clone(),
        motivation_id: motivation_id.clone(),
        score: 0.88,
        dimensions,
        notes: "Good implementation with thorough tests.".to_string(),
        evaluator: "human".to_string(),
        timestamp: "2025-01-15T11:00:00Z".to_string(),
    };

    let eval_path = agency::record_evaluation(&evaluation, &agency_dir).unwrap();
    assert!(eval_path.exists(), "Evaluation file should exist at {:?}", eval_path);

    // Verify the evaluation was saved and can be loaded back
    let loaded_eval = agency::load_evaluation(&eval_path).unwrap();
    assert_eq!(loaded_eval.id, "eval-impl-parser-1");
    assert_eq!(loaded_eval.score, 0.88);
    assert_eq!(loaded_eval.dimensions.len(), 4);

    // Verify role performance was updated (file uses content-hash ID as filename)
    let updated_role = agency::load_role(&roles_dir.join(format!("{}.yaml", role_id))).unwrap();
    assert_eq!(updated_role.performance.task_count, 1);
    assert!(
        (updated_role.performance.avg_score.unwrap() - 0.88).abs() < 1e-6,
        "Role avg_score should be 0.88, got {:?}",
        updated_role.performance.avg_score
    );
    assert_eq!(updated_role.performance.evaluations.len(), 1);
    assert_eq!(updated_role.performance.evaluations[0].task_id, "impl-parser");
    assert_eq!(updated_role.performance.evaluations[0].context_id, motivation_id);

    // Verify motivation performance was updated
    let updated_motivation = agency::load_motivation(&motivations_dir.join(format!("{}.yaml", motivation_id))).unwrap();
    assert_eq!(updated_motivation.performance.task_count, 1);
    assert!(
        (updated_motivation.performance.avg_score.unwrap() - 0.88).abs() < 1e-6,
        "Motivation avg_score should be 0.88, got {:?}",
        updated_motivation.performance.avg_score
    );
    assert_eq!(updated_motivation.performance.evaluations.len(), 1);
    assert_eq!(updated_motivation.performance.evaluations[0].context_id, role_id);

    // ---------------------------------------------------------------
    // Step 7: Record a second evaluation and verify cumulative updates
    // ---------------------------------------------------------------
    let mut dimensions2 = HashMap::new();
    dimensions2.insert("correctness".to_string(), 0.95);
    dimensions2.insert("completeness".to_string(), 0.90);
    dimensions2.insert("efficiency".to_string(), 0.85);
    dimensions2.insert("style_adherence".to_string(), 0.92);

    let evaluation2 = Evaluation {
        id: "eval-fix-parser-bug-1".to_string(),
        task_id: "fix-parser-bug".to_string(),
        agent_id: String::new(),
        role_id: role_id.clone(),
        motivation_id: motivation_id.clone(),
        score: 0.92,
        dimensions: dimensions2,
        notes: "Excellent bugfix with regression tests.".to_string(),
        evaluator: "human".to_string(),
        timestamp: "2025-01-16T09:00:00Z".to_string(),
    };

    agency::record_evaluation(&evaluation2, &agency_dir).unwrap();

    let updated_role2 = agency::load_role(&roles_dir.join(format!("{}.yaml", role_id))).unwrap();
    assert_eq!(updated_role2.performance.task_count, 2);
    // avg should be (0.88 + 0.92) / 2 = 0.90
    let expected_avg = (0.88 + 0.92) / 2.0;
    assert!(
        (updated_role2.performance.avg_score.unwrap() - expected_avg).abs() < 1e-6,
        "Role avg_score should be {}, got {:?}",
        expected_avg,
        updated_role2.performance.avg_score
    );
    assert_eq!(updated_role2.performance.evaluations.len(), 2);

    // All evaluations should be loadable
    let all_evals = agency::load_all_evaluations(&agency_dir.join("evaluations")).unwrap();
    assert_eq!(all_evals.len(), 2, "Should have 2 evaluations on disk");
}

/// Test the seed_starters function populates default roles and motivations.
#[test]
fn test_seed_starters_and_round_trip() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");

    let (roles_created, motivations_created) = agency::seed_starters(&agency_dir).unwrap();
    assert!(roles_created > 0, "Should create at least one starter role");
    assert!(motivations_created > 0, "Should create at least one starter motivation");

    // Verify round-trip: load all and check they're valid
    let roles = agency::load_all_roles(&agency_dir.join("roles")).unwrap();
    assert_eq!(roles.len(), roles_created);

    // All starter roles should have content-hash IDs (64 hex chars)
    for role in &roles {
        assert_eq!(role.id.len(), 64, "Starter role '{}' should have a content-hash ID, got: {}", role.name, role.id);
    }

    let motivations = agency::load_all_motivations(&agency_dir.join("motivations")).unwrap();
    assert_eq!(motivations.len(), motivations_created);

    // All starter motivations should have content-hash IDs (64 hex chars)
    for motivation in &motivations {
        assert_eq!(motivation.id.len(), 64, "Starter motivation '{}' should have a content-hash ID, got: {}", motivation.name, motivation.id);
    }

    // Seeding again should create 0 new items (idempotent)
    let (r2, m2) = agency::seed_starters(&agency_dir).unwrap();
    assert_eq!(r2, 0, "Second seed should not create duplicate roles");
    assert_eq!(m2, 0, "Second seed should not create duplicate motivations");
}

/// Full agency lifecycle with new design: role, motivation, agent creation,
/// assignment, three-level evaluation recording, output capture, and lineage.
///
/// Steps:
/// 1. Create role with content-hash ID.
/// 2. Create motivation with content-hash ID.
/// 3. Create agent from role+motivation.
/// 4. Assign agent to task.
/// 5. Evaluate completed task with three-level recording.
/// 6. Verify evaluation recorded against agent, role, and motivation.
/// 7. Verify output capture.
/// 8. Verify agent lineage.
/// 9. Test that old slug-based entities are rejected gracefully.
#[test]
fn test_full_agency_lifecycle_new_design() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    let agency_dir = wg_dir.join("agency");

    // ---------------------------------------------------------------
    // Step 1: Create role with content-hash ID
    // ---------------------------------------------------------------
    agency::init(&agency_dir).unwrap();

    let role = agency::build_role(
        "Integration Implementer",
        "Implements features with full test coverage.",
        vec![
            SkillRef::Name("rust".to_string()),
            SkillRef::Name("testing".to_string()),
            SkillRef::Inline("Write integration tests covering all edge cases".to_string()),
        ],
        "Fully tested feature implementation",
    );
    let role_id = role.id.clone();

    // Verify content-hash ID
    assert_eq!(role_id.len(), 64, "Role ID should be SHA-256 hex hash");
    assert!(role_id.chars().all(|c| c.is_ascii_hexdigit()), "Role ID should be hex");

    // Deterministic: same content produces same hash
    let role_dup = agency::build_role(
        "Different Name",
        "Implements features with full test coverage.",
        vec![
            SkillRef::Name("rust".to_string()),
            SkillRef::Name("testing".to_string()),
            SkillRef::Inline("Write integration tests covering all edge cases".to_string()),
        ],
        "Fully tested feature implementation",
    );
    assert_eq!(role_dup.id, role_id, "Same immutable content should produce same hash regardless of name");

    let roles_dir = agency_dir.join("roles");
    agency::save_role(&role, &roles_dir).unwrap();

    // ---------------------------------------------------------------
    // Step 2: Create motivation with content-hash ID
    // ---------------------------------------------------------------
    let motivation = agency::build_motivation(
        "Reliable Delivery",
        "Delivers working software with comprehensive testing.",
        vec![
            "Longer development time".to_string(),
            "More verbose implementations".to_string(),
        ],
        vec![
            "Shipping untested code".to_string(),
            "Ignoring edge cases".to_string(),
        ],
    );
    let motivation_id = motivation.id.clone();

    assert_eq!(motivation_id.len(), 64, "Motivation ID should be SHA-256 hex hash");
    assert!(motivation_id.chars().all(|c| c.is_ascii_hexdigit()));

    let motivations_dir = agency_dir.join("motivations");
    agency::save_motivation(&motivation, &motivations_dir).unwrap();

    // ---------------------------------------------------------------
    // Step 3: Create agent from role+motivation
    // ---------------------------------------------------------------
    let agent_id = agency::content_hash_agent(&role_id, &motivation_id);
    assert_eq!(agent_id.len(), 64, "Agent ID should be SHA-256 hex hash");

    // Agent ID is deterministic from role+motivation
    let agent_id_dup = agency::content_hash_agent(&role_id, &motivation_id);
    assert_eq!(agent_id, agent_id_dup, "Same role+motivation should produce same agent ID");

    // Different pairing produces different agent ID
    let alt_motivation = agency::build_motivation(
        "Speed",
        "Prioritizes fast delivery.",
        vec!["Less testing".to_string()],
        vec!["Broken code".to_string()],
    );
    let alt_agent_id = agency::content_hash_agent(&role_id, &alt_motivation.id);
    assert_ne!(agent_id, alt_agent_id, "Different motivation should produce different agent ID");

    let agent = Agent {
        id: agent_id.clone(),
        role_id: role_id.clone(),
        motivation_id: motivation_id.clone(),
        name: "integration-test-agent".to_string(),
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::default(),
        capabilities: Vec::new(),
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
    };
    let agents_dir = agency_dir.join("agents");
    agency::save_agent(&agent, &agents_dir).unwrap();

    // Verify agent round-trip
    let loaded_agent = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(loaded_agent.id, agent_id);
    assert_eq!(loaded_agent.role_id, role_id);
    assert_eq!(loaded_agent.motivation_id, motivation_id);
    assert_eq!(loaded_agent.name, "integration-test-agent");

    // Verify prefix lookup works
    let by_prefix = agency::find_agent_by_prefix(&agents_dir, &agent_id[..8]).unwrap();
    assert_eq!(by_prefix.id, agent_id);

    // ---------------------------------------------------------------
    // Step 4: Assign agent to task
    // ---------------------------------------------------------------
    let mut task = make_task(
        "integration-feature",
        "Implement integration testing framework",
        Some("Build a framework for running integration tests with proper isolation"),
        vec!["rust", "testing"],
        vec!["rust", "testing"],
        Some("cargo test --test integration passes"),
    );
    task.agent = Some(agent_id.clone());
    task.started_at = Some("2025-06-01T09:00:00Z".to_string());
    task.log = vec![
        LogEntry {
            timestamp: "2025-06-01T09:00:00Z".to_string(),
            actor: Some("coordinator".to_string()),
            message: "Task claimed by agent".to_string(),
        },
        LogEntry {
            timestamp: "2025-06-01T10:30:00Z".to_string(),
            actor: Some("agent".to_string()),
            message: "Implemented core test runner".to_string(),
        },
        LogEntry {
            timestamp: "2025-06-01T11:00:00Z".to_string(),
            actor: Some("agent".to_string()),
            message: "Added edge case coverage".to_string(),
        },
    ];

    assert_eq!(task.agent, Some(agent_id.clone()));

    // Verify the identity prompt renders correctly for the assigned agent
    let resolved_skills = agency::resolve_all_skills(&role, tmp.path());
    assert_eq!(resolved_skills.len(), 3);
    let prompt = agency::render_identity_prompt(&role, &motivation, &resolved_skills);
    assert!(prompt.contains("Integration Implementer"));
    assert!(prompt.contains("integration tests"));
    assert!(prompt.contains("Longer development time"));
    assert!(prompt.contains("Shipping untested code"));

    // Verify evaluator prompt renders with agent info
    let evaluator_prompt = agency::render_evaluator_prompt(&agency::EvaluatorInput {
        task_title: &task.title,
        task_description: task.description.as_deref(),
        task_skills: &task.skills,
        verify: task.verify.as_deref(),
        agent: Some(&agent),
        role: Some(&role),
        motivation: Some(&motivation),
        artifacts: &task.artifacts,
        log_entries: &task.log,
        started_at: task.started_at.as_deref(),
        completed_at: task.completed_at.as_deref(),
    });
    assert!(evaluator_prompt.contains("integration-test-agent"));
    assert!(evaluator_prompt.contains("Integration Implementer"));
    assert!(evaluator_prompt.contains("Reliable Delivery"));
    assert!(evaluator_prompt.contains("Implemented core test runner"));

    // ---------------------------------------------------------------
    // Step 5: Simulate task completion and evaluate with three-level recording
    // ---------------------------------------------------------------
    task.status = Status::Done;
    task.completed_at = Some("2025-06-01T12:00:00Z".to_string());
    task.artifacts = vec![
        "src/test_runner.rs".to_string(),
        "tests/integration.rs".to_string(),
    ];

    let mut dimensions = HashMap::new();
    dimensions.insert("correctness".to_string(), 0.92);
    dimensions.insert("completeness".to_string(), 0.88);
    dimensions.insert("efficiency".to_string(), 0.85);
    dimensions.insert("style_adherence".to_string(), 0.90);

    let evaluation = Evaluation {
        id: "eval-integration-feature-1".to_string(),
        task_id: "integration-feature".to_string(),
        agent_id: agent_id.clone(), // Include agent_id for three-level recording
        role_id: role_id.clone(),
        motivation_id: motivation_id.clone(),
        score: 0.89,
        dimensions: dimensions.clone(),
        notes: "Solid implementation with good test coverage.".to_string(),
        evaluator: "auto-evaluator".to_string(),
        timestamp: "2025-06-01T12:30:00Z".to_string(),
    };

    let eval_path = agency::record_evaluation(&evaluation, &agency_dir).unwrap();
    assert!(eval_path.exists());

    // ---------------------------------------------------------------
    // Step 6: Verify evaluation recorded against agent, role, and motivation
    // ---------------------------------------------------------------

    // 6a. Verify evaluation JSON on disk
    let loaded_eval = agency::load_evaluation(&eval_path).unwrap();
    assert_eq!(loaded_eval.id, "eval-integration-feature-1");
    assert_eq!(loaded_eval.task_id, "integration-feature");
    assert_eq!(loaded_eval.agent_id, agent_id);
    assert_eq!(loaded_eval.role_id, role_id);
    assert_eq!(loaded_eval.motivation_id, motivation_id);
    assert_eq!(loaded_eval.score, 0.89);
    assert_eq!(loaded_eval.dimensions.len(), 4);
    assert_eq!(loaded_eval.dimensions["correctness"], 0.92);
    assert_eq!(loaded_eval.evaluator, "auto-evaluator");

    // 6b. Verify AGENT performance was updated (three-level: agent level)
    let updated_agent = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(updated_agent.performance.task_count, 1, "Agent should have 1 task recorded");
    assert!(
        (updated_agent.performance.avg_score.unwrap() - 0.89).abs() < 1e-6,
        "Agent avg_score should be 0.89, got {:?}",
        updated_agent.performance.avg_score
    );
    assert_eq!(updated_agent.performance.evaluations.len(), 1);
    assert_eq!(updated_agent.performance.evaluations[0].task_id, "integration-feature");

    // 6c. Verify ROLE performance was updated (three-level: role level)
    let updated_role = agency::load_role(&roles_dir.join(format!("{}.yaml", role_id))).unwrap();
    assert_eq!(updated_role.performance.task_count, 1, "Role should have 1 task recorded");
    assert!(
        (updated_role.performance.avg_score.unwrap() - 0.89).abs() < 1e-6,
        "Role avg_score should be 0.89"
    );
    assert_eq!(updated_role.performance.evaluations.len(), 1);
    assert_eq!(updated_role.performance.evaluations[0].task_id, "integration-feature");
    assert_eq!(
        updated_role.performance.evaluations[0].context_id, motivation_id,
        "Role eval context_id should be the motivation_id"
    );

    // 6d. Verify MOTIVATION performance was updated (three-level: motivation level)
    let updated_motivation = agency::load_motivation(
        &motivations_dir.join(format!("{}.yaml", motivation_id)),
    ).unwrap();
    assert_eq!(updated_motivation.performance.task_count, 1, "Motivation should have 1 task recorded");
    assert!(
        (updated_motivation.performance.avg_score.unwrap() - 0.89).abs() < 1e-6,
        "Motivation avg_score should be 0.89"
    );
    assert_eq!(updated_motivation.performance.evaluations.len(), 1);
    assert_eq!(
        updated_motivation.performance.evaluations[0].context_id, role_id,
        "Motivation eval context_id should be the role_id"
    );

    // 6e. Verify cumulative recording with a second evaluation
    let mut dims2 = HashMap::new();
    dims2.insert("correctness".to_string(), 0.95);
    dims2.insert("completeness".to_string(), 0.93);
    dims2.insert("efficiency".to_string(), 0.90);
    dims2.insert("style_adherence".to_string(), 0.92);

    let eval2 = Evaluation {
        id: "eval-integration-feature-2".to_string(),
        task_id: "second-task".to_string(),
        agent_id: agent_id.clone(),
        role_id: role_id.clone(),
        motivation_id: motivation_id.clone(),
        score: 0.93,
        dimensions: dims2,
        notes: "Excellent follow-up work.".to_string(),
        evaluator: "auto-evaluator".to_string(),
        timestamp: "2025-06-02T10:00:00Z".to_string(),
    };
    agency::record_evaluation(&eval2, &agency_dir).unwrap();

    // Verify cumulative: agent should have 2 evals, avg = (0.89 + 0.93) / 2 = 0.91
    let agent_after_2 = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(agent_after_2.performance.task_count, 2);
    let expected_avg = (0.89 + 0.93) / 2.0;
    assert!(
        (agent_after_2.performance.avg_score.unwrap() - expected_avg).abs() < 1e-6,
        "Agent avg_score after 2 evals should be {}, got {:?}",
        expected_avg,
        agent_after_2.performance.avg_score
    );
    assert_eq!(agent_after_2.performance.evaluations.len(), 2);

    // Role and motivation should also have 2 evals each
    let role_after_2 = agency::load_role(&roles_dir.join(format!("{}.yaml", role_id))).unwrap();
    assert_eq!(role_after_2.performance.task_count, 2);
    assert_eq!(role_after_2.performance.evaluations.len(), 2);

    let mot_after_2 = agency::load_motivation(
        &motivations_dir.join(format!("{}.yaml", motivation_id)),
    ).unwrap();
    assert_eq!(mot_after_2.performance.task_count, 2);
    assert_eq!(mot_after_2.performance.evaluations.len(), 2);

    // All evaluations should be on disk
    let all_evals = agency::load_all_evaluations(&agency_dir.join("evaluations")).unwrap();
    assert_eq!(all_evals.len(), 2);

    // ---------------------------------------------------------------
    // Step 7: Verify output capture
    // ---------------------------------------------------------------
    let output_dir = agency::capture_task_output(&wg_dir, &task).unwrap();
    assert!(output_dir.exists(), "Output dir should exist");
    assert_eq!(
        output_dir,
        wg_dir.join("output").join("integration-feature"),
    );

    // Verify artifacts.json was written
    let artifacts_path = output_dir.join("artifacts.json");
    assert!(artifacts_path.exists(), "artifacts.json should exist");
    let artifacts_content = std::fs::read_to_string(&artifacts_path).unwrap();
    let artifact_entries: Vec<agency::ArtifactEntry> =
        serde_json::from_str(&artifacts_content).unwrap();
    assert_eq!(artifact_entries.len(), 2);
    assert_eq!(artifact_entries[0].path, "src/test_runner.rs");
    assert_eq!(artifact_entries[1].path, "tests/integration.rs");

    // Verify log.json was written
    let log_path = output_dir.join("log.json");
    assert!(log_path.exists(), "log.json should exist");
    let log_content = std::fs::read_to_string(&log_path).unwrap();
    let log_entries: Vec<LogEntry> = serde_json::from_str(&log_content).unwrap();
    assert_eq!(log_entries.len(), 3);
    assert_eq!(log_entries[0].message, "Task claimed by agent");
    assert_eq!(log_entries[0].actor, Some("coordinator".to_string()));
    assert_eq!(log_entries[1].message, "Implemented core test runner");
    assert_eq!(log_entries[2].message, "Added edge case coverage");

    // Verify changes.patch was written (will be a comment since tempdir is not a git repo)
    let patch_path = output_dir.join("changes.patch");
    assert!(patch_path.exists(), "changes.patch should exist");

    // ---------------------------------------------------------------
    // Step 8: Verify agent lineage
    // ---------------------------------------------------------------

    // 8a. Default lineage for manually-created entities
    let fresh_agent = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(fresh_agent.lineage.generation, 0);
    assert_eq!(fresh_agent.lineage.created_by, "human");
    assert!(fresh_agent.lineage.parent_ids.is_empty());

    // 8b. Create a mutated (evolved) role with lineage
    let mut evolved_role = agency::build_role(
        "Integration Implementer v2",
        "Implements features with full test coverage and benchmarks.",
        vec![
            SkillRef::Name("rust".to_string()),
            SkillRef::Name("testing".to_string()),
            SkillRef::Name("benchmarking".to_string()),
            SkillRef::Inline("Write integration tests covering all edge cases".to_string()),
        ],
        "Fully tested and benchmarked feature implementation",
    );
    evolved_role.lineage = Lineage::mutation(&role_id, 0, "evo-run-1");
    let evolved_role_id = evolved_role.id.clone();
    agency::save_role(&evolved_role, &roles_dir).unwrap();

    // 8c. Create a crossover motivation from two parents
    let motivation_b = agency::build_motivation(
        "Fast Delivery",
        "Ship quickly with acceptable quality.",
        vec!["Less documentation".to_string()],
        vec!["Broken builds".to_string()],
    );
    let motivation_b_id = motivation_b.id.clone();
    agency::save_motivation(&motivation_b, &motivations_dir).unwrap();

    let mut crossover_motivation = agency::build_motivation(
        "Balanced Delivery",
        "Balances speed and quality for optimal delivery.",
        vec![
            "Moderate documentation".to_string(),
            "Slightly longer timelines".to_string(),
        ],
        vec![
            "Broken builds".to_string(),
            "Zero test coverage".to_string(),
        ],
    );
    crossover_motivation.lineage =
        Lineage::crossover(&[&motivation_id, &motivation_b_id], 0, "evo-run-2");
    let crossover_mot_id = crossover_motivation.id.clone();
    agency::save_motivation(&crossover_motivation, &motivations_dir).unwrap();

    // 8d. Verify role ancestry
    let role_ancestry = agency::role_ancestry(&evolved_role_id, &roles_dir).unwrap();
    assert_eq!(role_ancestry.len(), 2, "Evolved role should have 2 ancestors (self + parent)");
    assert_eq!(role_ancestry[0].id, evolved_role_id);
    assert_eq!(role_ancestry[0].generation, 1);
    assert_eq!(role_ancestry[0].created_by, "evolver-evo-run-1");
    assert_eq!(role_ancestry[1].id, role_id);
    assert_eq!(role_ancestry[1].generation, 0);

    // 8e. Verify motivation ancestry with crossover
    let mot_ancestry = agency::motivation_ancestry(&crossover_mot_id, &motivations_dir).unwrap();
    assert_eq!(mot_ancestry.len(), 3, "Crossover motivation should have 3 ancestors (self + 2 parents)");
    assert_eq!(mot_ancestry[0].id, crossover_mot_id);
    assert_eq!(mot_ancestry[0].generation, 1);
    assert_eq!(mot_ancestry[0].created_by, "evolver-evo-run-2");
    let parent_ids: Vec<&str> = mot_ancestry[1..].iter().map(|n| n.id.as_str()).collect();
    assert!(parent_ids.contains(&motivation_id.as_str()));
    assert!(parent_ids.contains(&motivation_b_id.as_str()));

    // 8f. Create a second-generation agent from evolved entities
    let evolved_agent_id = agency::content_hash_agent(&evolved_role_id, &crossover_mot_id);
    let evolved_agent = Agent {
        id: evolved_agent_id.clone(),
        role_id: evolved_role_id.clone(),
        motivation_id: crossover_mot_id.clone(),
        name: "evolved-agent".to_string(),
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::mutation(&agent_id, 0, "agent-evo-1"),
        capabilities: Vec::new(),
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
    };
    agency::save_agent(&evolved_agent, &agents_dir).unwrap();

    let loaded_evolved = agency::find_agent_by_prefix(&agents_dir, &evolved_agent_id).unwrap();
    assert_eq!(loaded_evolved.lineage.generation, 1);
    assert_eq!(loaded_evolved.lineage.parent_ids, vec![agent_id.clone()]);
    assert_eq!(loaded_evolved.lineage.created_by, "evolver-agent-evo-1");

    // ---------------------------------------------------------------
    // Step 9: Test that old slug-based entities are rejected gracefully
    // ---------------------------------------------------------------

    // 9a. A slug-based role (non-hex ID) can be loaded if it exists on disk
    //     but find_by_prefix with a slug won't match content-hash entities
    let slug_role_yaml = r#"
id: my-legacy-role
name: Legacy Role
description: A role from the old slug era
skills: []
desired_outcome: Something working
performance:
  task_count: 5
  avg_score: 0.75
"#;
    std::fs::write(
        roles_dir.join("my-legacy-role.yaml"),
        slug_role_yaml,
    ).unwrap();

    // Slug-based ID can still be found by exact prefix match
    let legacy = agency::find_role_by_prefix(&roles_dir, "my-legacy-role");
    assert!(legacy.is_ok(), "Should still load legacy slug-based role");
    let legacy_role = legacy.unwrap();
    assert_eq!(legacy_role.id, "my-legacy-role");
    assert_ne!(legacy_role.id.len(), 64, "Legacy role should NOT have a content-hash ID");

    // 9b. A slug-based motivation can coexist with content-hash ones
    let slug_mot_yaml = r#"
id: old-motivation
name: Old Motivation
description: From before content-hash IDs
acceptable_tradeoffs: []
unacceptable_tradeoffs: []
performance:
  task_count: 0
  avg_score: null
"#;
    std::fs::write(
        motivations_dir.join("old-motivation.yaml"),
        slug_mot_yaml,
    ).unwrap();

    let legacy_mot = agency::find_motivation_by_prefix(&motivations_dir, "old-motivation");
    assert!(legacy_mot.is_ok(), "Should still load legacy slug-based motivation");

    // 9c. All entities (old and new) coexist in load_all
    let all_roles = agency::load_all_roles(&roles_dir).unwrap();
    assert!(
        all_roles.len() >= 3,
        "Should have original + evolved + legacy roles, got {}",
        all_roles.len()
    );

    let all_mots = agency::load_all_motivations(&motivations_dir).unwrap();
    assert!(
        all_mots.len() >= 4,
        "Should have original + alt + crossover + legacy motivations, got {}",
        all_mots.len()
    );

    // 9d. Recording an evaluation referencing a slug-based role should NOT crash
    //     (the role gets found by prefix, performance is updated)
    let slug_eval = Evaluation {
        id: "eval-legacy-task-1".to_string(),
        task_id: "legacy-task".to_string(),
        agent_id: String::new(),
        role_id: "my-legacy-role".to_string(),
        motivation_id: motivation_id.clone(),
        score: 0.70,
        dimensions: HashMap::new(),
        notes: "Legacy evaluation".to_string(),
        evaluator: "human".to_string(),
        timestamp: "2025-06-03T08:00:00Z".to_string(),
    };
    let slug_eval_result = agency::record_evaluation(&slug_eval, &agency_dir);
    assert!(slug_eval_result.is_ok(), "Evaluation with slug-based role should succeed");

    // Verify the legacy role got its performance updated
    let updated_legacy = agency::find_role_by_prefix(&roles_dir, "my-legacy-role").unwrap();
    assert_eq!(updated_legacy.performance.task_count, 6, "Legacy role task_count should increment from 5 to 6");

    // 9e. Nonexistent slug prefix produces a clean NotFound error
    let missing = agency::find_role_by_prefix(&roles_dir, "nonexistent-slug");
    assert!(missing.is_err());
    let err_msg = missing.unwrap_err().to_string();
    assert!(
        err_msg.contains("No role matching"),
        "Error should mention 'No role matching', got: {}",
        err_msg
    );
}

/// Test that output capture produces all three expected files.
#[test]
fn test_output_capture_standalone() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let mut task = make_task(
        "capture-test",
        "Test output capture",
        Some("A task to test the capture mechanism"),
        vec![],
        vec![],
        None,
    );
    task.status = Status::Done;
    task.started_at = Some("2025-07-01T08:00:00Z".to_string());
    task.completed_at = Some("2025-07-01T10:00:00Z".to_string());
    task.artifacts = vec!["README.md".to_string()];
    task.log = vec![
        LogEntry {
            timestamp: "2025-07-01T08:00:00Z".to_string(),
            actor: Some("agent".to_string()),
            message: "Started work".to_string(),
        },
        LogEntry {
            timestamp: "2025-07-01T10:00:00Z".to_string(),
            actor: None,
            message: "Completed".to_string(),
        },
    ];

    let output_dir = agency::capture_task_output(&wg_dir, &task).unwrap();
    assert!(output_dir.join("changes.patch").exists());
    assert!(output_dir.join("artifacts.json").exists());
    assert!(output_dir.join("log.json").exists());

    // Verify log.json contents
    let log_json: Vec<LogEntry> = serde_json::from_str(
        &std::fs::read_to_string(output_dir.join("log.json")).unwrap(),
    ).unwrap();
    assert_eq!(log_json.len(), 2);
    assert_eq!(log_json[0].actor, Some("agent".to_string()));
    assert_eq!(log_json[1].actor, None);

    // Verify artifacts.json contents
    let artifacts: Vec<agency::ArtifactEntry> = serde_json::from_str(
        &std::fs::read_to_string(output_dir.join("artifacts.json")).unwrap(),
    ).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].path, "README.md");
    // Size may or may not be present depending on path resolution
}

/// Test that agent performance tracking is independent from role and motivation.
#[test]
fn test_agent_independent_performance() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");
    let agents_dir = agency_dir.join("agents");

    // Create two agents sharing the same role but with different motivations
    let role = agency::build_role("Shared Role", "Common role", vec![], "Outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    let mot_a = agency::build_motivation("Mot A", "First", vec![], vec![]);
    let mot_b = agency::build_motivation("Mot B", "Second", vec![], vec!["No bugs".to_string()]);
    agency::save_motivation(&mot_a, &motivations_dir).unwrap();
    agency::save_motivation(&mot_b, &motivations_dir).unwrap();

    let agent_a_id = agency::content_hash_agent(&role.id, &mot_a.id);
    let agent_b_id = agency::content_hash_agent(&role.id, &mot_b.id);
    assert_ne!(agent_a_id, agent_b_id);

    let agent_a = Agent {
        id: agent_a_id.clone(),
        role_id: role.id.clone(),
        motivation_id: mot_a.id.clone(),
        name: "agent-a".to_string(),
        performance: PerformanceRecord { task_count: 0, avg_score: None, evaluations: vec![] },
        lineage: Lineage::default(),
        capabilities: Vec::new(),
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
    };
    let agent_b = Agent {
        id: agent_b_id.clone(),
        role_id: role.id.clone(),
        motivation_id: mot_b.id.clone(),
        name: "agent-b".to_string(),
        performance: PerformanceRecord { task_count: 0, avg_score: None, evaluations: vec![] },
        lineage: Lineage::default(),
        capabilities: Vec::new(),
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
    };
    agency::save_agent(&agent_a, &agents_dir).unwrap();
    agency::save_agent(&agent_b, &agents_dir).unwrap();

    // Record evaluation for agent_a only
    let eval_a = Evaluation {
        id: "eval-a-1".to_string(),
        task_id: "task-a".to_string(),
        agent_id: agent_a_id.clone(),
        role_id: role.id.clone(),
        motivation_id: mot_a.id.clone(),
        score: 0.95,
        dimensions: HashMap::new(),
        notes: "Great".to_string(),
        evaluator: "human".to_string(),
        timestamp: "2025-08-01T10:00:00Z".to_string(),
    };
    agency::record_evaluation(&eval_a, &agency_dir).unwrap();

    // Agent A should have performance recorded
    let a_after = agency::find_agent_by_prefix(&agents_dir, &agent_a_id).unwrap();
    assert_eq!(a_after.performance.task_count, 1);
    assert!((a_after.performance.avg_score.unwrap() - 0.95).abs() < 1e-6);

    // Agent B should still be at zero
    let b_after = agency::find_agent_by_prefix(&agents_dir, &agent_b_id).unwrap();
    assert_eq!(b_after.performance.task_count, 0);
    assert!(b_after.performance.avg_score.is_none());

    // But the shared role should have 1 evaluation (shared across both agents' role)
    let role_after = agency::load_role(&roles_dir.join(format!("{}.yaml", role.id))).unwrap();
    assert_eq!(role_after.performance.task_count, 1);
}
