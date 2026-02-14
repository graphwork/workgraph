//! Edge-case tests for the agency system.
//!
//! Covers: nonexistent entity references, deletion of referenced entities,
//! extreme performance scores, content hash collision resistance, prefix lookup
//! edge cases, and corrupted YAML handling.

use std::collections::HashMap;
use tempfile::TempDir;

use workgraph::agency::{
    self, Agent, Evaluation, EvaluationRef, Lineage, PerformanceRecord, SkillRef,
};

// ---------------------------------------------------------------------------
// 1. Creating an agent with nonexistent role_id or motivation_id
// ---------------------------------------------------------------------------

/// An agent can be created (saved) referencing role/motivation IDs that don't
/// exist on disk. The agent is just data — it doesn't validate references at
/// save time.
#[test]
fn test_create_agent_with_nonexistent_role_and_motivation() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let agents_dir = agency_dir.join("agents");
    let fake_role_id = "aaaa".repeat(16); // 64 hex chars
    let fake_mot_id = "bbbb".repeat(16);

    let agent_id = agency::content_hash_agent(&fake_role_id, &fake_mot_id);
    let agent = Agent {
        id: agent_id.clone(),
        role_id: fake_role_id.clone(),
        motivation_id: fake_mot_id.clone(),
        name: "orphan-agent".to_string(),
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

    // Save succeeds even though the role/motivation don't exist
    agency::save_agent(&agent, &agents_dir).unwrap();

    // Agent can be loaded back
    let loaded = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(loaded.role_id, fake_role_id);
    assert_eq!(loaded.motivation_id, fake_mot_id);
}

// ---------------------------------------------------------------------------
// 2. Recording an evaluation for a nonexistent agent
// ---------------------------------------------------------------------------

/// record_evaluation gracefully handles a nonexistent agent_id — it still saves
/// the evaluation JSON and skips the agent performance update without error.
#[test]
fn test_record_evaluation_nonexistent_agent() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    // Create a role and motivation so those parts succeed
    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");

    let role = agency::build_role("R", "desc", vec![], "outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    let mot = agency::build_motivation("M", "desc", vec![], vec![]);
    agency::save_motivation(&mot, &motivations_dir).unwrap();

    let eval = Evaluation {
        id: "eval-ghost-1".to_string(),
        task_id: "ghost-task".to_string(),
        agent_id: "nonexistent_agent_id_1234567890abcdef".to_string(),
        role_id: role.id.clone(),
        motivation_id: mot.id.clone(),
        score: 0.75,
        dimensions: HashMap::new(),
        notes: "Agent doesn't exist".to_string(),
        evaluator: "test".to_string(),
        timestamp: "2025-01-01T00:00:00Z".to_string(),
    };

    // Should succeed — evaluation saved, role/motivation updated, agent skipped
    let eval_path = agency::record_evaluation(&eval, &agency_dir).unwrap();
    assert!(eval_path.exists());

    // Role and motivation should still have their performance updated
    let updated_role = agency::load_role(&roles_dir.join(format!("{}.yaml", role.id))).unwrap();
    assert_eq!(updated_role.performance.task_count, 1);

    let updated_mot =
        agency::load_motivation(&motivations_dir.join(format!("{}.yaml", mot.id))).unwrap();
    assert_eq!(updated_mot.performance.task_count, 1);
}

/// record_evaluation with empty agent_id skips agent update entirely.
#[test]
fn test_record_evaluation_empty_agent_id() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");

    let role = agency::build_role("R", "desc", vec![], "outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    let mot = agency::build_motivation("M", "desc", vec![], vec![]);
    agency::save_motivation(&mot, &motivations_dir).unwrap();

    let eval = Evaluation {
        id: "eval-no-agent-1".to_string(),
        task_id: "task-1".to_string(),
        agent_id: String::new(),
        role_id: role.id.clone(),
        motivation_id: mot.id.clone(),
        score: 0.80,
        dimensions: HashMap::new(),
        notes: "No agent".to_string(),
        evaluator: "test".to_string(),
        timestamp: "2025-01-01T00:00:00Z".to_string(),
    };

    let eval_path = agency::record_evaluation(&eval, &agency_dir).unwrap();
    assert!(eval_path.exists());

    // No agents should exist
    let agents = agency::load_all_agents(&agency_dir.join("agents")).unwrap();
    assert!(agents.is_empty());
}

/// record_evaluation with nonexistent role_id and motivation_id still saves the
/// evaluation JSON — it just skips the role/motivation performance updates.
#[test]
fn test_record_evaluation_nonexistent_role_and_motivation() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let eval = Evaluation {
        id: "eval-orphan-1".to_string(),
        task_id: "orphan-task".to_string(),
        agent_id: String::new(),
        role_id: "nonexistent_role_id".to_string(),
        motivation_id: "nonexistent_motivation_id".to_string(),
        score: 0.50,
        dimensions: HashMap::new(),
        notes: "Orphan eval".to_string(),
        evaluator: "test".to_string(),
        timestamp: "2025-01-01T00:00:00Z".to_string(),
    };

    // Should succeed — the eval JSON is saved even if role/motivation not found
    let eval_path = agency::record_evaluation(&eval, &agency_dir).unwrap();
    assert!(eval_path.exists());

    let loaded = agency::load_evaluation(&eval_path).unwrap();
    assert_eq!(loaded.score, 0.50);
}

// ---------------------------------------------------------------------------
// 3. Deleting a role referenced by an existing agent
// ---------------------------------------------------------------------------

/// Deleting a role YAML file that is referenced by an agent succeeds (it's just
/// a file delete). The agent still loads fine — it just has a dangling role_id.
#[test]
fn test_delete_role_referenced_by_agent() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");
    let agents_dir = agency_dir.join("agents");

    let role = agency::build_role("Doomed Role", "Will be deleted", vec![], "Outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    let mot = agency::build_motivation("M", "desc", vec![], vec![]);
    agency::save_motivation(&mot, &motivations_dir).unwrap();

    let agent_id = agency::content_hash_agent(&role.id, &mot.id);
    let agent = Agent {
        id: agent_id.clone(),
        role_id: role.id.clone(),
        motivation_id: mot.id.clone(),
        name: "agent-with-doomed-role".to_string(),
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
    agency::save_agent(&agent, &agents_dir).unwrap();

    // Delete the role file
    let role_path = roles_dir.join(format!("{}.yaml", role.id));
    std::fs::remove_file(&role_path).unwrap();

    // Agent is still loadable with dangling role_id
    let loaded_agent = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(loaded_agent.role_id, role.id);

    // Looking up the role fails with NotFound
    let result = agency::find_role_by_prefix(&roles_dir, &role.id);
    assert!(result.is_err());

    // Recording an evaluation still succeeds (role update is skipped gracefully)
    let eval = Evaluation {
        id: "eval-after-delete-1".to_string(),
        task_id: "task-1".to_string(),
        agent_id: agent_id.clone(),
        role_id: role.id.clone(),
        motivation_id: mot.id.clone(),
        score: 0.85,
        dimensions: HashMap::new(),
        notes: "Role was deleted".to_string(),
        evaluator: "test".to_string(),
        timestamp: "2025-01-01T00:00:00Z".to_string(),
    };
    let eval_path = agency::record_evaluation(&eval, &agency_dir).unwrap();
    assert!(eval_path.exists());

    // Agent performance was still updated (agent exists)
    let updated_agent = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(updated_agent.performance.task_count, 1);

    // Motivation performance was updated (motivation exists)
    let updated_mot =
        agency::load_motivation(&motivations_dir.join(format!("{}.yaml", mot.id))).unwrap();
    assert_eq!(updated_mot.performance.task_count, 1);
}

// ---------------------------------------------------------------------------
// 4. Deleting a motivation referenced by an existing agent
// ---------------------------------------------------------------------------

/// Deleting a motivation YAML that is referenced by an agent. The agent still
/// loads, and evaluations recording gracefully skips the missing motivation.
#[test]
fn test_delete_motivation_referenced_by_agent() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");
    let agents_dir = agency_dir.join("agents");

    let role = agency::build_role("R", "desc", vec![], "Outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    let mot = agency::build_motivation("Doomed Mot", "Will be deleted", vec![], vec![]);
    agency::save_motivation(&mot, &motivations_dir).unwrap();

    let agent_id = agency::content_hash_agent(&role.id, &mot.id);
    let agent = Agent {
        id: agent_id.clone(),
        role_id: role.id.clone(),
        motivation_id: mot.id.clone(),
        name: "agent-with-doomed-mot".to_string(),
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
    agency::save_agent(&agent, &agents_dir).unwrap();

    // Delete the motivation file
    let mot_path = motivations_dir.join(format!("{}.yaml", mot.id));
    std::fs::remove_file(&mot_path).unwrap();

    // Agent is still loadable
    let loaded_agent = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(loaded_agent.motivation_id, mot.id);

    // Motivation lookup fails
    let result = agency::find_motivation_by_prefix(&motivations_dir, &mot.id);
    assert!(result.is_err());

    // Evaluation recording still succeeds (motivation update skipped)
    let eval = Evaluation {
        id: "eval-mot-deleted-1".to_string(),
        task_id: "task-1".to_string(),
        agent_id: agent_id.clone(),
        role_id: role.id.clone(),
        motivation_id: mot.id.clone(),
        score: 0.90,
        dimensions: HashMap::new(),
        notes: "Motivation was deleted".to_string(),
        evaluator: "test".to_string(),
        timestamp: "2025-01-01T00:00:00Z".to_string(),
    };
    let eval_path = agency::record_evaluation(&eval, &agency_dir).unwrap();
    assert!(eval_path.exists());

    // Agent got updated
    let updated_agent = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(updated_agent.performance.task_count, 1);

    // Role got updated (still exists)
    let updated_role = agency::load_role(&roles_dir.join(format!("{}.yaml", role.id))).unwrap();
    assert_eq!(updated_role.performance.task_count, 1);
}

// ---------------------------------------------------------------------------
// 5. Performance record updates with extreme scores
// ---------------------------------------------------------------------------

/// Score of exactly 0.0 is valid and correctly computed.
#[test]
fn test_performance_score_zero() {
    let mut record = PerformanceRecord {
        task_count: 0,
        avg_score: None,
        evaluations: vec![],
    };

    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: 0.0,
            task_id: "t1".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            context_id: "ctx".to_string(),
        },
    );

    assert_eq!(record.task_count, 1);
    assert!((record.avg_score.unwrap() - 0.0).abs() < 1e-10);
}

/// Score of exactly 1.0 is valid and correctly computed.
#[test]
fn test_performance_score_one() {
    let mut record = PerformanceRecord {
        task_count: 0,
        avg_score: None,
        evaluations: vec![],
    };

    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: 1.0,
            task_id: "t1".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            context_id: "ctx".to_string(),
        },
    );

    assert_eq!(record.task_count, 1);
    assert!((record.avg_score.unwrap() - 1.0).abs() < 1e-10);
}

/// Negative scores are not rejected by the data model (no validation boundary).
/// The system stores them as-is and computes averages correctly.
#[test]
fn test_performance_score_negative() {
    let mut record = PerformanceRecord {
        task_count: 0,
        avg_score: None,
        evaluations: vec![],
    };

    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: -0.5,
            task_id: "t1".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            context_id: "ctx".to_string(),
        },
    );

    assert_eq!(record.task_count, 1);
    assert!((record.avg_score.unwrap() - (-0.5)).abs() < 1e-10);
}

/// Mixed extreme scores average correctly: (0.0 + 1.0) / 2 = 0.5
#[test]
fn test_performance_mixed_extreme_scores() {
    let mut record = PerformanceRecord {
        task_count: 0,
        avg_score: None,
        evaluations: vec![],
    };

    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: 0.0,
            task_id: "t1".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            context_id: "ctx".to_string(),
        },
    );
    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: 1.0,
            task_id: "t2".to_string(),
            timestamp: "2025-01-01T00:00:01Z".to_string(),
            context_id: "ctx".to_string(),
        },
    );

    assert_eq!(record.task_count, 2);
    assert!((record.avg_score.unwrap() - 0.5).abs() < 1e-10);
}

/// recalculate_avg_score returns None for an empty list.
#[test]
fn test_recalculate_avg_score_empty() {
    assert!(agency::recalculate_avg_score(&[]).is_none());
}

/// Extreme scores round-trip through YAML serialization on a role.
#[test]
fn test_extreme_scores_yaml_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();
    let roles_dir = agency_dir.join("roles");

    let mut role = agency::build_role("R", "desc", vec![], "outcome");
    agency::update_performance(
        &mut role.performance,
        EvaluationRef {
            score: 0.0,
            task_id: "zero".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            context_id: "ctx".to_string(),
        },
    );
    agency::update_performance(
        &mut role.performance,
        EvaluationRef {
            score: 1.0,
            task_id: "one".to_string(),
            timestamp: "2025-01-01T00:00:01Z".to_string(),
            context_id: "ctx".to_string(),
        },
    );

    agency::save_role(&role, &roles_dir).unwrap();
    let loaded = agency::load_role(&roles_dir.join(format!("{}.yaml", role.id))).unwrap();
    assert_eq!(loaded.performance.task_count, 2);
    assert!((loaded.performance.avg_score.unwrap() - 0.5).abs() < 1e-10);
    assert!((loaded.performance.evaluations[0].score - 0.0).abs() < 1e-10);
    assert!((loaded.performance.evaluations[1].score - 1.0).abs() < 1e-10);
}

// ---------------------------------------------------------------------------
// 6. Content hash collision resistance
// ---------------------------------------------------------------------------

/// Slightly different descriptions produce different role hashes.
#[test]
fn test_content_hash_role_different_descriptions() {
    let h1 = agency::content_hash_role(&[], "outcome", "Description A");
    let h2 = agency::content_hash_role(&[], "outcome", "Description B");
    assert_ne!(
        h1, h2,
        "Different descriptions must produce different hashes"
    );
}

/// Slightly different desired_outcomes produce different role hashes.
#[test]
fn test_content_hash_role_different_outcomes() {
    let h1 = agency::content_hash_role(&[], "outcome A", "desc");
    let h2 = agency::content_hash_role(&[], "outcome B", "desc");
    assert_ne!(h1, h2, "Different outcomes must produce different hashes");
}

/// Slightly different skills produce different role hashes.
#[test]
fn test_content_hash_role_different_skills() {
    let h1 = agency::content_hash_role(&[SkillRef::Name("rust".to_string())], "outcome", "desc");
    let h2 = agency::content_hash_role(&[SkillRef::Name("python".to_string())], "outcome", "desc");
    assert_ne!(h1, h2, "Different skills must produce different hashes");
}

/// Skill order matters for content hashing (different order = different hash).
#[test]
fn test_content_hash_role_skill_order_matters() {
    let h1 = agency::content_hash_role(
        &[
            SkillRef::Name("a".to_string()),
            SkillRef::Name("b".to_string()),
        ],
        "outcome",
        "desc",
    );
    let h2 = agency::content_hash_role(
        &[
            SkillRef::Name("b".to_string()),
            SkillRef::Name("a".to_string()),
        ],
        "outcome",
        "desc",
    );
    assert_ne!(
        h1, h2,
        "Different skill order must produce different hashes"
    );
}

/// Different motivation descriptions produce different hashes.
#[test]
fn test_content_hash_motivation_different_descriptions() {
    let h1 = agency::content_hash_motivation(&[], &[], "Description A");
    let h2 = agency::content_hash_motivation(&[], &[], "Description B");
    assert_ne!(
        h1, h2,
        "Different descriptions must produce different hashes"
    );
}

/// Different tradeoffs produce different hashes.
#[test]
fn test_content_hash_motivation_different_tradeoffs() {
    let h1 = agency::content_hash_motivation(&["speed".to_string()], &[], "desc");
    let h2 = agency::content_hash_motivation(&["quality".to_string()], &[], "desc");
    assert_ne!(h1, h2);
}

/// Swapping acceptable and unacceptable tradeoffs produces different hashes.
#[test]
fn test_content_hash_motivation_swapped_tradeoff_categories() {
    let h1 = agency::content_hash_motivation(&["X".to_string()], &["Y".to_string()], "desc");
    let h2 = agency::content_hash_motivation(&["Y".to_string()], &["X".to_string()], "desc");
    assert_ne!(
        h1, h2,
        "Swapping tradeoff categories must produce different hashes"
    );
}

/// Different agent pairings produce different hashes.
#[test]
fn test_content_hash_agent_different_pairings() {
    let h1 = agency::content_hash_agent("role_a", "mot_a");
    let h2 = agency::content_hash_agent("role_a", "mot_b");
    let h3 = agency::content_hash_agent("role_b", "mot_a");
    assert_ne!(h1, h2);
    assert_ne!(h1, h3);
    assert_ne!(h2, h3);
}

/// Swapping role_id and motivation_id produces a different agent hash.
#[test]
fn test_content_hash_agent_swapped_ids() {
    let h1 = agency::content_hash_agent("aaa", "bbb");
    let h2 = agency::content_hash_agent("bbb", "aaa");
    assert_ne!(
        h1, h2,
        "Swapping role/motivation IDs must produce different hashes"
    );
}

/// Content hashing is deterministic — same inputs always yield same hash.
#[test]
fn test_content_hash_determinism() {
    let h1 = agency::content_hash_role(&[SkillRef::Name("x".to_string())], "out", "desc");
    let h2 = agency::content_hash_role(&[SkillRef::Name("x".to_string())], "out", "desc");
    assert_eq!(h1, h2);
    assert_eq!(h1.len(), 64, "SHA-256 hash should be 64 hex chars");
}

/// Role name does NOT affect the content hash (name is mutable metadata).
#[test]
fn test_content_hash_role_name_independent() {
    let r1 = agency::build_role("Name A", "desc", vec![], "outcome");
    let r2 = agency::build_role("Name B", "desc", vec![], "outcome");
    assert_eq!(r1.id, r2.id, "Name should not affect content hash");
}

/// Motivation name does NOT affect the content hash.
#[test]
fn test_content_hash_motivation_name_independent() {
    let m1 = agency::build_motivation("Name A", "desc", vec![], vec![]);
    let m2 = agency::build_motivation("Name B", "desc", vec![], vec![]);
    assert_eq!(m1.id, m2.id, "Name should not affect content hash");
}

// ---------------------------------------------------------------------------
// 7. Prefix lookup edge cases
// ---------------------------------------------------------------------------

/// Prefix lookup with zero matches returns NotFound.
#[test]
fn test_prefix_lookup_zero_matches() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");
    let agents_dir = agency_dir.join("agents");

    // Save one entity of each type so the directories aren't empty
    let role = agency::build_role("R", "desc", vec![], "outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    let mot = agency::build_motivation("M", "desc", vec![], vec![]);
    agency::save_motivation(&mot, &motivations_dir).unwrap();

    let agent_id = agency::content_hash_agent(&role.id, &mot.id);
    let agent = Agent {
        id: agent_id,
        role_id: role.id.clone(),
        motivation_id: mot.id.clone(),
        name: "a".to_string(),
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
    agency::save_agent(&agent, &agents_dir).unwrap();

    // Prefixes that don't match anything
    let r = agency::find_role_by_prefix(&roles_dir, "zzzzzzz");
    assert!(r.is_err());
    assert!(r.unwrap_err().to_string().contains("No role matching"));

    let m = agency::find_motivation_by_prefix(&motivations_dir, "zzzzzzz");
    assert!(m.is_err());
    assert!(
        m.unwrap_err()
            .to_string()
            .contains("No motivation matching")
    );

    let a = agency::find_agent_by_prefix(&agents_dir, "zzzzzzz");
    assert!(a.is_err());
    assert!(a.unwrap_err().to_string().contains("No agent matching"));
}

/// Prefix lookup with exactly one match succeeds.
#[test]
fn test_prefix_lookup_exact_one_match() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    let role = agency::build_role("Only Role", "desc", vec![], "outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    // Full ID match
    let found = agency::find_role_by_prefix(&roles_dir, &role.id).unwrap();
    assert_eq!(found.id, role.id);

    // Short prefix match (first 4 chars)
    let found = agency::find_role_by_prefix(&roles_dir, &role.id[..4]).unwrap();
    assert_eq!(found.id, role.id);
}

/// Prefix lookup with ambiguous prefix (multiple matches) returns Ambiguous error.
#[test]
fn test_prefix_lookup_ambiguous() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();
    let roles_dir = agency_dir.join("roles");

    // Create two roles that start with the same prefix by using slug-based IDs
    let yaml_a = "id: abc123\nname: A\ndescription: d\nskills: []\ndesired_outcome: o\nperformance:\n  task_count: 0\n  avg_score: null\n";
    let yaml_b = "id: abc456\nname: B\ndescription: d\nskills: []\ndesired_outcome: o\nperformance:\n  task_count: 0\n  avg_score: null\n";

    std::fs::write(roles_dir.join("abc123.yaml"), yaml_a).unwrap();
    std::fs::write(roles_dir.join("abc456.yaml"), yaml_b).unwrap();

    // "abc" matches both
    let result = agency::find_role_by_prefix(&roles_dir, "abc");
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("matches 2 roles"), "Got: {}", err_msg);
    assert!(err_msg.contains("abc123"));
    assert!(err_msg.contains("abc456"));

    // "abc1" matches only one
    let found = agency::find_role_by_prefix(&roles_dir, "abc1").unwrap();
    assert_eq!(found.id, "abc123");
}

/// Prefix lookup on empty directory returns NotFound.
#[test]
fn test_prefix_lookup_empty_directory() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let result = agency::find_role_by_prefix(&agency_dir.join("roles"), "anything");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("No role matching"));
}

/// Prefix lookup on nonexistent directory returns NotFound (load_all returns empty vec).
#[test]
fn test_prefix_lookup_nonexistent_directory() {
    let tmp = TempDir::new().unwrap();
    let nowhere = tmp.path().join("does-not-exist");

    let result = agency::find_role_by_prefix(&nowhere, "anything");
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// 8. Loading corrupted YAML files gracefully
// ---------------------------------------------------------------------------

/// A corrupted role YAML file causes load_role to return an error, not panic.
#[test]
fn test_load_corrupted_role_yaml() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    std::fs::write(roles_dir.join("corrupt.yaml"), "{{{{not valid yaml!!!!").unwrap();

    let result = agency::load_role(&roles_dir.join("corrupt.yaml"));
    assert!(result.is_err());
}

/// A corrupted motivation YAML file returns an error.
#[test]
fn test_load_corrupted_motivation_yaml() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let motivations_dir = agency_dir.join("motivations");
    std::fs::write(motivations_dir.join("corrupt.yaml"), "not: [valid: {yaml").unwrap();

    let result = agency::load_motivation(&motivations_dir.join("corrupt.yaml"));
    assert!(result.is_err());
}

/// A corrupted agent YAML file returns an error.
#[test]
fn test_load_corrupted_agent_yaml() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let agents_dir = agency_dir.join("agents");
    std::fs::write(agents_dir.join("corrupt.yaml"), ":::broken:::").unwrap();

    let result = agency::load_agent(&agents_dir.join("corrupt.yaml"));
    assert!(result.is_err());
}

/// A corrupted evaluation JSON file returns an error.
#[test]
fn test_load_corrupted_evaluation_json() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let evals_dir = agency_dir.join("evaluations");
    std::fs::write(evals_dir.join("corrupt.json"), "{broken json}").unwrap();

    let result = agency::load_evaluation(&evals_dir.join("corrupt.json"));
    assert!(result.is_err());
}

/// load_all_roles fails (returns Err) if one YAML file in the dir is corrupted.
#[test]
fn test_load_all_roles_with_one_corrupted() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");

    // Save a valid role
    let role = agency::build_role("Good Role", "desc", vec![], "outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    // Write a corrupted role
    std::fs::write(roles_dir.join("bad.yaml"), "not valid yaml {{{{").unwrap();

    // load_all_roles should return an error because it can't deserialize the corrupted file
    let result = agency::load_all_roles(&roles_dir);
    assert!(
        result.is_err(),
        "load_all_roles should fail with corrupted file"
    );
}

/// An empty YAML file produces a deserialization error (not a panic).
#[test]
fn test_load_empty_yaml_file() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    std::fs::write(roles_dir.join("empty.yaml"), "").unwrap();

    let result = agency::load_role(&roles_dir.join("empty.yaml"));
    assert!(result.is_err());
}

/// A YAML file with valid YAML but wrong schema returns an error.
#[test]
fn test_load_wrong_schema_yaml() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    // Valid YAML but missing required Role fields
    std::fs::write(roles_dir.join("wrong-schema.yaml"), "foo: bar\nbaz: 42\n").unwrap();

    let result = agency::load_role(&roles_dir.join("wrong-schema.yaml"));
    assert!(result.is_err());
}

/// A YAML file with partial role fields (missing required fields) returns an error.
#[test]
fn test_load_partial_role_yaml() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let roles_dir = agency_dir.join("roles");
    // Has id and name but missing performance, desired_outcome, etc.
    let partial = "id: partial\nname: Partial Role\n";
    std::fs::write(roles_dir.join("partial.yaml"), partial).unwrap();

    let result = agency::load_role(&roles_dir.join("partial.yaml"));
    assert!(result.is_err());
}

/// Loading a nonexistent file returns an IO error.
#[test]
fn test_load_nonexistent_file() {
    let tmp = TempDir::new().unwrap();
    let result = agency::load_role(&tmp.path().join("nonexistent.yaml"));
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Additional edge cases
// ---------------------------------------------------------------------------

/// Content hash with empty fields is stable and produces valid 64-char hex.
#[test]
fn test_content_hash_empty_fields() {
    let h = agency::content_hash_role(&[], "", "");
    assert_eq!(h.len(), 64);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));

    let h2 = agency::content_hash_motivation(&[], &[], "");
    assert_eq!(h2.len(), 64);
    assert!(h2.chars().all(|c| c.is_ascii_hexdigit()));

    let h3 = agency::content_hash_agent("", "");
    assert_eq!(h3.len(), 64);
    assert!(h3.chars().all(|c| c.is_ascii_hexdigit()));

    // All different from each other
    assert_ne!(h, h2);
    assert_ne!(h, h3);
    assert_ne!(h2, h3);
}

/// short_hash returns first 8 chars of a hash.
#[test]
fn test_short_hash() {
    let full = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    assert_eq!(agency::short_hash(full), "abcdef01");
}

/// short_hash on a string shorter than SHORT_HASH_LEN returns the whole string.
#[test]
fn test_short_hash_short_input() {
    assert_eq!(agency::short_hash("abc"), "abc");
}

/// Unicode in descriptions does not break content hashing.
#[test]
fn test_content_hash_unicode() {
    let h1 = agency::content_hash_role(&[], "outcome", "Descripción con acentos");
    let h2 = agency::content_hash_role(&[], "outcome", "Description con acentos");
    assert_ne!(h1, h2);
    assert_eq!(h1.len(), 64);
}

/// Special YAML characters in descriptions are handled correctly.
#[test]
fn test_content_hash_yaml_special_chars() {
    let h1 = agency::content_hash_role(&[], "outcome", "description: with colon");
    let h2 = agency::content_hash_role(&[], "outcome", "description with colon");
    assert_ne!(h1, h2);
    assert_eq!(h1.len(), 64);
    assert_eq!(h2.len(), 64);
}

/// init is idempotent — calling it twice doesn't error.
#[test]
fn test_init_idempotent() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();
    agency::init(&agency_dir).unwrap(); // second call should not fail
    assert!(agency_dir.join("roles").is_dir());
    assert!(agency_dir.join("motivations").is_dir());
    assert!(agency_dir.join("evaluations").is_dir());
    assert!(agency_dir.join("agents").is_dir());
}

/// load_all_* on nonexistent directories returns empty vec (not error).
#[test]
fn test_load_all_nonexistent_dirs() {
    let tmp = TempDir::new().unwrap();
    let nowhere = tmp.path().join("nowhere");

    assert!(agency::load_all_roles(&nowhere).unwrap().is_empty());
    assert!(agency::load_all_motivations(&nowhere).unwrap().is_empty());
    assert!(agency::load_all_evaluations(&nowhere).unwrap().is_empty());
    assert!(agency::load_all_agents(&nowhere).unwrap().is_empty());
}
