//! Tests for evaluation recording and performance aggregation.
//!
//! Covers:
//! 1. record_evaluation writes correct JSON format
//! 2. Multiple evaluations for same agent aggregate correctly
//! 3. update_performance on role, motivation, and agent independently track context_ids
//! 4. Performance record with 0, 1, and 10+ evaluations
//! 5. Evaluation dimensions scoring (all dimension fields preserved)
//! 6. recalculate_avg_score with various edge cases

use std::collections::HashMap;
use tempfile::TempDir;

use workgraph::agency::{
    self, Agent, Evaluation, EvaluationRef, Lineage, PerformanceRecord, SkillRef,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a test agency dir with one role, one motivation, and optionally an agent.
struct TestFixture {
    agency_dir: std::path::PathBuf,
    role_id: String,
    motivation_id: String,
    agent_id: String,
    _tmp: TempDir,
}

impl TestFixture {
    fn new() -> Self {
        Self::with_agent(true)
    }

    fn with_agent(create_agent: bool) -> Self {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        agency::init(&agency_dir).unwrap();

        let role = agency::build_role(
            "Test Role",
            "A role for testing evaluations.",
            vec![
                SkillRef::Name("rust".to_string()),
                SkillRef::Inline("Write tests".to_string()),
            ],
            "Tested code",
        );
        let role_id = role.id.clone();
        agency::save_role(&role, &agency_dir.join("roles")).unwrap();

        let motivation = agency::build_motivation(
            "Test Motivation",
            "A motivation for testing evaluations.",
            vec!["Slower pace".to_string()],
            vec!["Untested code".to_string()],
        );
        let motivation_id = motivation.id.clone();
        agency::save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

        let agent_id = agency::content_hash_agent(&role_id, &motivation_id);

        if create_agent {
            let agent = Agent {
                id: agent_id.clone(),
                role_id: role_id.clone(),
                motivation_id: motivation_id.clone(),
                name: "test-agent".to_string(),
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
            agency::save_agent(&agent, &agency_dir.join("agents")).unwrap();
        }

        TestFixture {
            agency_dir,
            role_id,
            motivation_id,
            agent_id,
            _tmp: tmp,
        }
    }

    fn make_evaluation(&self, id: &str, task_id: &str, score: f64, with_agent: bool) -> Evaluation {
        self.make_evaluation_with_dims(id, task_id, score, with_agent, HashMap::new())
    }

    fn make_evaluation_with_dims(
        &self,
        id: &str,
        task_id: &str,
        score: f64,
        with_agent: bool,
        dimensions: HashMap<String, f64>,
    ) -> Evaluation {
        Evaluation {
            id: id.to_string(),
            task_id: task_id.to_string(),
            agent_id: if with_agent {
                self.agent_id.clone()
            } else {
                String::new()
            },
            role_id: self.role_id.clone(),
            motivation_id: self.motivation_id.clone(),
            score,
            dimensions,
            notes: format!("Test eval {}", id),
            evaluator: "test-harness".to_string(),
            timestamp: "2025-06-01T12:00:00Z".to_string(),
        }
    }
}

// ===========================================================================
// 1. record_evaluation writes correct JSON format
// ===========================================================================

/// Verify the evaluation JSON file contains all expected fields with correct values.
#[test]
fn test_record_evaluation_json_format() {
    let fix = TestFixture::new();

    let mut dimensions = HashMap::new();
    dimensions.insert("correctness".to_string(), 0.92);
    dimensions.insert("completeness".to_string(), 0.85);
    dimensions.insert("efficiency".to_string(), 0.78);
    dimensions.insert("style_adherence".to_string(), 0.91);

    let eval = Evaluation {
        id: "eval-json-format-1".to_string(),
        task_id: "json-task".to_string(),
        agent_id: fix.agent_id.clone(),
        role_id: fix.role_id.clone(),
        motivation_id: fix.motivation_id.clone(),
        score: 0.87,
        dimensions: dimensions.clone(),
        notes: "Testing JSON format preservation.".to_string(),
        evaluator: "human-reviewer".to_string(),
        timestamp: "2025-06-15T14:30:00Z".to_string(),
    };

    let eval_path = agency::record_evaluation(&eval, &fix.agency_dir).unwrap();

    // Verify file exists and is valid JSON
    assert!(eval_path.exists());
    let raw_json = std::fs::read_to_string(&eval_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw_json).unwrap();

    // Verify all top-level fields
    assert_eq!(parsed["id"], "eval-json-format-1");
    assert_eq!(parsed["task_id"], "json-task");
    assert_eq!(parsed["agent_id"], fix.agent_id.as_str());
    assert_eq!(parsed["role_id"], fix.role_id.as_str());
    assert_eq!(parsed["motivation_id"], fix.motivation_id.as_str());
    assert_eq!(parsed["score"], 0.87);
    assert_eq!(parsed["notes"], "Testing JSON format preservation.");
    assert_eq!(parsed["evaluator"], "human-reviewer");
    assert_eq!(parsed["timestamp"], "2025-06-15T14:30:00Z");

    // Verify dimensions map preserved
    let dims = parsed["dimensions"].as_object().unwrap();
    assert_eq!(dims.len(), 4);
    assert_eq!(dims["correctness"], 0.92);
    assert_eq!(dims["completeness"], 0.85);
    assert_eq!(dims["efficiency"], 0.78);
    assert_eq!(dims["style_adherence"], 0.91);
}

/// Verify the evaluation filename uses the expected format: eval-{task_id}-{safe_timestamp}.json
#[test]
fn test_record_evaluation_filename_format() {
    let fix = TestFixture::new();
    let eval = fix.make_evaluation("eval-1", "my-task", 0.8, false);

    let eval_path = agency::record_evaluation(&eval, &fix.agency_dir).unwrap();

    let filename = eval_path.file_name().unwrap().to_str().unwrap();
    // Timestamp "2025-06-01T12:00:00Z" has colons replaced with hyphens
    assert_eq!(filename, "eval-my-task-2025-06-01T12-00-00Z.json");
}

/// Verify that the JSON file can be deserialized back into an identical Evaluation.
#[test]
fn test_record_evaluation_round_trip() {
    let fix = TestFixture::new();

    let mut dimensions = HashMap::new();
    dimensions.insert("correctness".to_string(), 1.0);
    dimensions.insert("completeness".to_string(), 0.0);

    let eval = fix.make_evaluation_with_dims("round-trip", "rt-task", 0.5, true, dimensions);
    let eval_path = agency::record_evaluation(&eval, &fix.agency_dir).unwrap();

    let loaded = agency::load_evaluation(&eval_path).unwrap();
    assert_eq!(loaded.id, eval.id);
    assert_eq!(loaded.task_id, eval.task_id);
    assert_eq!(loaded.agent_id, eval.agent_id);
    assert_eq!(loaded.role_id, eval.role_id);
    assert_eq!(loaded.motivation_id, eval.motivation_id);
    assert_eq!(loaded.score, eval.score);
    assert_eq!(loaded.dimensions, eval.dimensions);
    assert_eq!(loaded.notes, eval.notes);
    assert_eq!(loaded.evaluator, eval.evaluator);
    assert_eq!(loaded.timestamp, eval.timestamp);
}

/// Verify that evaluations with empty dimensions map serialize as empty object.
#[test]
fn test_record_evaluation_empty_dimensions() {
    let fix = TestFixture::new();
    let eval = fix.make_evaluation("no-dims", "task-no-dims", 0.75, false);

    let eval_path = agency::record_evaluation(&eval, &fix.agency_dir).unwrap();
    let loaded = agency::load_evaluation(&eval_path).unwrap();

    assert!(loaded.dimensions.is_empty());
}

// ===========================================================================
// 2. Multiple evaluations for same agent aggregate correctly
// ===========================================================================

/// Two evaluations for the same agent produce correct cumulative avg_score.
#[test]
fn test_multiple_evaluations_same_agent_avg() {
    let fix = TestFixture::new();

    let eval1 = Evaluation {
        id: "e1".to_string(),
        task_id: "task-1".to_string(),
        agent_id: fix.agent_id.clone(),
        role_id: fix.role_id.clone(),
        motivation_id: fix.motivation_id.clone(),
        score: 0.80,
        dimensions: HashMap::new(),
        notes: String::new(),
        evaluator: "test".to_string(),
        timestamp: "2025-06-01T10:00:00Z".to_string(),
    };
    let eval2 = Evaluation {
        id: "e2".to_string(),
        task_id: "task-2".to_string(),
        agent_id: fix.agent_id.clone(),
        role_id: fix.role_id.clone(),
        motivation_id: fix.motivation_id.clone(),
        score: 0.90,
        dimensions: HashMap::new(),
        notes: String::new(),
        evaluator: "test".to_string(),
        timestamp: "2025-06-01T11:00:00Z".to_string(),
    };

    agency::record_evaluation(&eval1, &fix.agency_dir).unwrap();
    agency::record_evaluation(&eval2, &fix.agency_dir).unwrap();

    let agent =
        agency::find_agent_by_prefix(&fix.agency_dir.join("agents"), &fix.agent_id).unwrap();

    assert_eq!(agent.performance.task_count, 2);
    let expected = (0.80 + 0.90) / 2.0;
    assert!(
        (agent.performance.avg_score.unwrap() - expected).abs() < 1e-10,
        "Expected avg {}, got {:?}",
        expected,
        agent.performance.avg_score
    );
    assert_eq!(agent.performance.evaluations.len(), 2);
    assert_eq!(agent.performance.evaluations[0].task_id, "task-1");
    assert_eq!(agent.performance.evaluations[1].task_id, "task-2");
}

/// Three evaluations for the same agent: incremental avg is always recalculated, not accumulated.
#[test]
fn test_three_evaluations_incremental_avg() {
    let fix = TestFixture::new();

    let scores = [0.60, 0.80, 1.0];
    for (i, &score) in scores.iter().enumerate() {
        let eval = Evaluation {
            id: format!("e{}", i),
            task_id: format!("task-{}", i),
            agent_id: fix.agent_id.clone(),
            role_id: fix.role_id.clone(),
            motivation_id: fix.motivation_id.clone(),
            score,
            dimensions: HashMap::new(),
            notes: String::new(),
            evaluator: "test".to_string(),
            timestamp: format!("2025-06-01T{}:00:00Z", 10 + i),
        };
        agency::record_evaluation(&eval, &fix.agency_dir).unwrap();
    }

    let agent =
        agency::find_agent_by_prefix(&fix.agency_dir.join("agents"), &fix.agent_id).unwrap();

    assert_eq!(agent.performance.task_count, 3);
    let expected = (0.60 + 0.80 + 1.0) / 3.0;
    assert!(
        (agent.performance.avg_score.unwrap() - expected).abs() < 1e-10,
        "Expected avg {}, got {:?}",
        expected,
        agent.performance.avg_score
    );
}

// ===========================================================================
// 3. update_performance on role, motivation, and agent independently track context_ids
// ===========================================================================

/// Role stores motivation_id as context_id, motivation stores role_id as context_id,
/// agent stores task_id as context_id — all independently.
#[test]
fn test_context_ids_tracked_independently() {
    let fix = TestFixture::new();

    let eval = Evaluation {
        id: "ctx-test-1".to_string(),
        task_id: "context-task".to_string(),
        agent_id: fix.agent_id.clone(),
        role_id: fix.role_id.clone(),
        motivation_id: fix.motivation_id.clone(),
        score: 0.88,
        dimensions: HashMap::new(),
        notes: String::new(),
        evaluator: "test".to_string(),
        timestamp: "2025-06-01T12:00:00Z".to_string(),
    };
    agency::record_evaluation(&eval, &fix.agency_dir).unwrap();

    // Agent's context_id = task_id
    let agent =
        agency::find_agent_by_prefix(&fix.agency_dir.join("agents"), &fix.agent_id).unwrap();
    assert_eq!(
        agent.performance.evaluations[0].context_id, "context-task",
        "Agent context_id should be the task_id"
    );

    // Role's context_id = motivation_id
    let role = agency::load_role(
        &fix.agency_dir
            .join("roles")
            .join(format!("{}.yaml", fix.role_id)),
    )
    .unwrap();
    assert_eq!(
        role.performance.evaluations[0].context_id, fix.motivation_id,
        "Role context_id should be the motivation_id"
    );

    // Motivation's context_id = role_id
    let motivation = agency::load_motivation(
        &fix.agency_dir
            .join("motivations")
            .join(format!("{}.yaml", fix.motivation_id)),
    )
    .unwrap();
    assert_eq!(
        motivation.performance.evaluations[0].context_id, fix.role_id,
        "Motivation context_id should be the role_id"
    );
}

/// When the same role is used with different motivations, each evaluation ref stores
/// the correct motivation_id as context_id on the role.
#[test]
fn test_role_tracks_different_motivation_context_ids() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let role = agency::build_role("Multi-mot Role", "desc", vec![], "outcome");
    agency::save_role(&role, &agency_dir.join("roles")).unwrap();

    let mot_a = agency::build_motivation("Mot A", "first", vec![], vec![]);
    let mot_b = agency::build_motivation("Mot B", "second", vec!["compromise".to_string()], vec![]);
    agency::save_motivation(&mot_a, &agency_dir.join("motivations")).unwrap();
    agency::save_motivation(&mot_b, &agency_dir.join("motivations")).unwrap();

    // Eval with mot_a
    let eval_a = Evaluation {
        id: "e-a".to_string(),
        task_id: "task-a".to_string(),
        agent_id: String::new(),
        role_id: role.id.clone(),
        motivation_id: mot_a.id.clone(),
        score: 0.70,
        dimensions: HashMap::new(),
        notes: String::new(),
        evaluator: "test".to_string(),
        timestamp: "2025-06-01T10:00:00Z".to_string(),
    };
    agency::record_evaluation(&eval_a, &agency_dir).unwrap();

    // Eval with mot_b
    let eval_b = Evaluation {
        id: "e-b".to_string(),
        task_id: "task-b".to_string(),
        agent_id: String::new(),
        role_id: role.id.clone(),
        motivation_id: mot_b.id.clone(),
        score: 0.90,
        dimensions: HashMap::new(),
        notes: String::new(),
        evaluator: "test".to_string(),
        timestamp: "2025-06-01T11:00:00Z".to_string(),
    };
    agency::record_evaluation(&eval_b, &agency_dir).unwrap();

    let updated_role =
        agency::load_role(&agency_dir.join("roles").join(format!("{}.yaml", role.id))).unwrap();
    assert_eq!(updated_role.performance.evaluations.len(), 2);
    assert_eq!(
        updated_role.performance.evaluations[0].context_id, mot_a.id,
        "First eval should track mot_a as context"
    );
    assert_eq!(
        updated_role.performance.evaluations[1].context_id, mot_b.id,
        "Second eval should track mot_b as context"
    );
}

/// When the same motivation is used with different roles, each evaluation ref stores
/// the correct role_id as context_id on the motivation.
#[test]
fn test_motivation_tracks_different_role_context_ids() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let role_a = agency::build_role("Role A", "first", vec![], "outcome a");
    let role_b = agency::build_role("Role B", "second", vec![], "outcome b");
    agency::save_role(&role_a, &agency_dir.join("roles")).unwrap();
    agency::save_role(&role_b, &agency_dir.join("roles")).unwrap();

    let mot = agency::build_motivation("Multi-role Mot", "desc", vec![], vec![]);
    agency::save_motivation(&mot, &agency_dir.join("motivations")).unwrap();

    let eval_a = Evaluation {
        id: "e-ra".to_string(),
        task_id: "task-ra".to_string(),
        agent_id: String::new(),
        role_id: role_a.id.clone(),
        motivation_id: mot.id.clone(),
        score: 0.65,
        dimensions: HashMap::new(),
        notes: String::new(),
        evaluator: "test".to_string(),
        timestamp: "2025-06-01T10:00:00Z".to_string(),
    };
    let eval_b = Evaluation {
        id: "e-rb".to_string(),
        task_id: "task-rb".to_string(),
        agent_id: String::new(),
        role_id: role_b.id.clone(),
        motivation_id: mot.id.clone(),
        score: 0.95,
        dimensions: HashMap::new(),
        notes: String::new(),
        evaluator: "test".to_string(),
        timestamp: "2025-06-01T11:00:00Z".to_string(),
    };

    agency::record_evaluation(&eval_a, &agency_dir).unwrap();
    agency::record_evaluation(&eval_b, &agency_dir).unwrap();

    let updated_mot = agency::load_motivation(
        &agency_dir
            .join("motivations")
            .join(format!("{}.yaml", mot.id)),
    )
    .unwrap();
    assert_eq!(updated_mot.performance.evaluations.len(), 2);
    assert_eq!(
        updated_mot.performance.evaluations[0].context_id, role_a.id,
        "First eval should track role_a as context"
    );
    assert_eq!(
        updated_mot.performance.evaluations[1].context_id, role_b.id,
        "Second eval should track role_b as context"
    );
}

// ===========================================================================
// 4. Performance record with 0, 1, and 10+ evaluations
// ===========================================================================

/// A fresh performance record (0 evaluations) has None avg_score and empty evaluations.
#[test]
fn test_performance_zero_evaluations() {
    let record = PerformanceRecord {
        task_count: 0,
        avg_score: None,
        evaluations: vec![],
    };
    assert_eq!(record.task_count, 0);
    assert!(record.avg_score.is_none());
    assert!(record.evaluations.is_empty());
}

/// A fresh performance record round-trips through YAML (0 evaluations).
#[test]
fn test_performance_zero_evaluations_yaml_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();

    let role = agency::build_role("Fresh", "no evals yet", vec![], "outcome");
    agency::save_role(&role, &agency_dir.join("roles")).unwrap();

    let loaded =
        agency::load_role(&agency_dir.join("roles").join(format!("{}.yaml", role.id))).unwrap();
    assert_eq!(loaded.performance.task_count, 0);
    assert!(loaded.performance.avg_score.is_none());
    assert!(loaded.performance.evaluations.is_empty());
}

/// Performance record with exactly 1 evaluation.
#[test]
fn test_performance_one_evaluation() {
    let mut record = PerformanceRecord {
        task_count: 0,
        avg_score: None,
        evaluations: vec![],
    };

    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: 0.77,
            task_id: "single-task".to_string(),
            timestamp: "2025-06-01T10:00:00Z".to_string(),
            context_id: "ctx-1".to_string(),
        },
    );

    assert_eq!(record.task_count, 1);
    assert!((record.avg_score.unwrap() - 0.77).abs() < 1e-10);
    assert_eq!(record.evaluations.len(), 1);
    assert_eq!(record.evaluations[0].task_id, "single-task");
}

/// Performance record with 10+ evaluations accumulates correctly.
#[test]
fn test_performance_ten_plus_evaluations() {
    let mut record = PerformanceRecord {
        task_count: 0,
        avg_score: None,
        evaluations: vec![],
    };

    let scores: Vec<f64> = (0..15).map(|i| 0.5 + (i as f64) * 0.03).collect();

    for (i, &score) in scores.iter().enumerate() {
        agency::update_performance(
            &mut record,
            EvaluationRef {
                score,
                task_id: format!("task-{}", i),
                timestamp: format!("2025-06-{:02}T10:00:00Z", i + 1),
                context_id: format!("ctx-{}", i),
            },
        );
    }

    assert_eq!(record.task_count, 15);
    assert_eq!(record.evaluations.len(), 15);

    let expected_avg: f64 = scores.iter().sum::<f64>() / scores.len() as f64;
    assert!(
        (record.avg_score.unwrap() - expected_avg).abs() < 1e-10,
        "Expected avg {}, got {:?}",
        expected_avg,
        record.avg_score
    );

    // Verify each evaluation ref stored correctly
    for (i, eval_ref) in record.evaluations.iter().enumerate() {
        assert_eq!(eval_ref.task_id, format!("task-{}", i));
        assert_eq!(eval_ref.context_id, format!("ctx-{}", i));
        assert!((eval_ref.score - scores[i]).abs() < 1e-10);
    }
}

/// Full end-to-end: record 12 evaluations via record_evaluation and verify
/// agent, role, and motivation all accumulate correctly.
#[test]
fn test_twelve_evaluations_end_to_end() {
    let fix = TestFixture::new();

    let scores: Vec<f64> = (0..12).map(|i| 0.60 + (i as f64) * 0.03).collect();

    for (i, &score) in scores.iter().enumerate() {
        let eval = Evaluation {
            id: format!("e12-{}", i),
            task_id: format!("task-12-{}", i),
            agent_id: fix.agent_id.clone(),
            role_id: fix.role_id.clone(),
            motivation_id: fix.motivation_id.clone(),
            score,
            dimensions: HashMap::new(),
            notes: String::new(),
            evaluator: "test".to_string(),
            timestamp: format!("2025-06-{:02}T10:00:00Z", i + 1),
        };
        agency::record_evaluation(&eval, &fix.agency_dir).unwrap();
    }

    let expected_avg = scores.iter().sum::<f64>() / scores.len() as f64;

    // Agent
    let agent =
        agency::find_agent_by_prefix(&fix.agency_dir.join("agents"), &fix.agent_id).unwrap();
    assert_eq!(agent.performance.task_count, 12);
    assert_eq!(agent.performance.evaluations.len(), 12);
    assert!(
        (agent.performance.avg_score.unwrap() - expected_avg).abs() < 1e-10,
        "Agent avg: expected {}, got {:?}",
        expected_avg,
        agent.performance.avg_score
    );

    // Role
    let role = agency::load_role(
        &fix.agency_dir
            .join("roles")
            .join(format!("{}.yaml", fix.role_id)),
    )
    .unwrap();
    assert_eq!(role.performance.task_count, 12);
    assert_eq!(role.performance.evaluations.len(), 12);
    assert!((role.performance.avg_score.unwrap() - expected_avg).abs() < 1e-10);

    // Motivation
    let mot = agency::load_motivation(
        &fix.agency_dir
            .join("motivations")
            .join(format!("{}.yaml", fix.motivation_id)),
    )
    .unwrap();
    assert_eq!(mot.performance.task_count, 12);
    assert_eq!(mot.performance.evaluations.len(), 12);
    assert!((mot.performance.avg_score.unwrap() - expected_avg).abs() < 1e-10);

    // All 12 evaluation files should exist on disk
    let all_evals = agency::load_all_evaluations(&fix.agency_dir.join("evaluations")).unwrap();
    assert_eq!(all_evals.len(), 12);
}

// ===========================================================================
// 5. Evaluation dimensions scoring (all dimension fields preserved)
// ===========================================================================

/// All four standard dimension fields are preserved through record + load.
#[test]
fn test_all_dimension_fields_preserved() {
    let fix = TestFixture::new();

    let mut dimensions = HashMap::new();
    dimensions.insert("correctness".to_string(), 0.95);
    dimensions.insert("completeness".to_string(), 0.88);
    dimensions.insert("efficiency".to_string(), 0.72);
    dimensions.insert("style_adherence".to_string(), 0.91);

    let eval =
        fix.make_evaluation_with_dims("dim-test", "dims-task", 0.87, true, dimensions.clone());

    let eval_path = agency::record_evaluation(&eval, &fix.agency_dir).unwrap();
    let loaded = agency::load_evaluation(&eval_path).unwrap();

    assert_eq!(loaded.dimensions.len(), 4);
    for (key, &expected_val) in &dimensions {
        let actual = loaded.dimensions.get(key).unwrap();
        assert!(
            (actual - expected_val).abs() < 1e-10,
            "Dimension '{}': expected {}, got {}",
            key,
            expected_val,
            actual
        );
    }
}

/// Custom/non-standard dimension fields are also preserved.
#[test]
fn test_custom_dimension_fields_preserved() {
    let fix = TestFixture::new();

    let mut dimensions = HashMap::new();
    dimensions.insert("correctness".to_string(), 0.9);
    dimensions.insert("creativity".to_string(), 0.85);
    dimensions.insert("documentation_quality".to_string(), 0.7);
    dimensions.insert("test_coverage".to_string(), 0.95);
    dimensions.insert("performance_impact".to_string(), 0.6);

    let eval = fix.make_evaluation_with_dims(
        "custom-dims",
        "custom-task",
        0.80,
        false,
        dimensions.clone(),
    );

    let eval_path = agency::record_evaluation(&eval, &fix.agency_dir).unwrap();
    let loaded = agency::load_evaluation(&eval_path).unwrap();

    assert_eq!(loaded.dimensions.len(), 5);
    for (key, &expected_val) in &dimensions {
        assert!(
            (loaded.dimensions[key] - expected_val).abs() < 1e-10,
            "Custom dimension '{}' not preserved correctly",
            key
        );
    }
}

/// Dimensions with extreme values (0.0 and 1.0) are preserved.
#[test]
fn test_dimension_extreme_values() {
    let fix = TestFixture::new();

    let mut dimensions = HashMap::new();
    dimensions.insert("correctness".to_string(), 0.0);
    dimensions.insert("completeness".to_string(), 1.0);

    let eval =
        fix.make_evaluation_with_dims("extreme-dims", "ext-task", 0.5, false, dimensions.clone());

    let eval_path = agency::record_evaluation(&eval, &fix.agency_dir).unwrap();
    let loaded = agency::load_evaluation(&eval_path).unwrap();

    assert!((loaded.dimensions["correctness"] - 0.0).abs() < 1e-10);
    assert!((loaded.dimensions["completeness"] - 1.0).abs() < 1e-10);
}

/// Dimensions are independent of the overall score (score is not derived from dimensions).
#[test]
fn test_dimensions_independent_of_score() {
    let fix = TestFixture::new();

    let mut dimensions = HashMap::new();
    dimensions.insert("correctness".to_string(), 0.5);
    dimensions.insert("completeness".to_string(), 0.5);

    // Overall score intentionally different from any dimension average
    let eval =
        fix.make_evaluation_with_dims("indep-dims", "indep-task", 0.99, false, dimensions.clone());

    let eval_path = agency::record_evaluation(&eval, &fix.agency_dir).unwrap();
    let loaded = agency::load_evaluation(&eval_path).unwrap();

    // Score is stored as-is, not recalculated from dimensions
    assert!((loaded.score - 0.99).abs() < 1e-10);
    assert!((loaded.dimensions["correctness"] - 0.5).abs() < 1e-10);
}

// ===========================================================================
// 6. recalculate_avg_score with various edge cases
// ===========================================================================

/// Empty evaluation list returns None.
#[test]
fn test_recalculate_avg_score_empty() {
    assert!(agency::recalculate_avg_score(&[]).is_none());
}

/// Single evaluation returns that score exactly.
#[test]
fn test_recalculate_avg_score_single() {
    let refs = vec![EvaluationRef {
        score: 0.73,
        task_id: "t".to_string(),
        timestamp: "ts".to_string(),
        context_id: "c".to_string(),
    }];
    let avg = agency::recalculate_avg_score(&refs).unwrap();
    assert!((avg - 0.73).abs() < 1e-10);
}

/// Two identical scores return that score.
#[test]
fn test_recalculate_avg_score_identical_scores() {
    let refs = vec![
        EvaluationRef {
            score: 0.80,
            task_id: "t1".to_string(),
            timestamp: "ts1".to_string(),
            context_id: "c1".to_string(),
        },
        EvaluationRef {
            score: 0.80,
            task_id: "t2".to_string(),
            timestamp: "ts2".to_string(),
            context_id: "c2".to_string(),
        },
    ];
    let avg = agency::recalculate_avg_score(&refs).unwrap();
    assert!((avg - 0.80).abs() < 1e-10);
}

/// Score of 0.0 averaged with 1.0 gives 0.5.
#[test]
fn test_recalculate_avg_score_zero_and_one() {
    let refs = vec![
        EvaluationRef {
            score: 0.0,
            task_id: "t1".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
        EvaluationRef {
            score: 1.0,
            task_id: "t2".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
    ];
    let avg = agency::recalculate_avg_score(&refs).unwrap();
    assert!((avg - 0.5).abs() < 1e-10);
}

/// Large number of evaluations (100) computes correctly.
#[test]
fn test_recalculate_avg_score_large_count() {
    let refs: Vec<EvaluationRef> = (0..100)
        .map(|i| EvaluationRef {
            score: (i as f64) / 99.0, // 0.0 to 1.0
            task_id: format!("t{}", i),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        })
        .collect();

    let avg = agency::recalculate_avg_score(&refs).unwrap();
    // Sum of 0..99 / 99 = (99 * 100 / 2) / 99 = 50. Average = 50 / 100 = 0.5
    assert!(
        (avg - 0.5).abs() < 1e-10,
        "Average of 0.0..1.0 over 100 steps should be 0.5, got {}",
        avg
    );
}

/// Negative scores are handled (no validation boundary).
#[test]
fn test_recalculate_avg_score_with_negatives() {
    let refs = vec![
        EvaluationRef {
            score: -1.0,
            task_id: "t1".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
        EvaluationRef {
            score: 1.0,
            task_id: "t2".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
    ];
    let avg = agency::recalculate_avg_score(&refs).unwrap();
    assert!((avg - 0.0).abs() < 1e-10);
}

/// All zeroes averages to zero.
#[test]
fn test_recalculate_avg_score_all_zeros() {
    let refs: Vec<EvaluationRef> = (0..5)
        .map(|i| EvaluationRef {
            score: 0.0,
            task_id: format!("t{}", i),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        })
        .collect();

    let avg = agency::recalculate_avg_score(&refs).unwrap();
    assert!((avg - 0.0).abs() < 1e-10);
}

/// All ones averages to one.
#[test]
fn test_recalculate_avg_score_all_ones() {
    let refs: Vec<EvaluationRef> = (0..5)
        .map(|i| EvaluationRef {
            score: 1.0,
            task_id: format!("t{}", i),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        })
        .collect();

    let avg = agency::recalculate_avg_score(&refs).unwrap();
    assert!((avg - 1.0).abs() < 1e-10);
}

/// Very small score differences are distinguishable.
#[test]
fn test_recalculate_avg_score_precision() {
    let refs = vec![
        EvaluationRef {
            score: 0.333333333,
            task_id: "t1".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
        EvaluationRef {
            score: 0.666666667,
            task_id: "t2".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
    ];
    let avg = agency::recalculate_avg_score(&refs).unwrap();
    assert!((avg - 0.5).abs() < 1e-8, "Expected ~0.5, got {}", avg);
}

/// update_performance correctly increments task_count and recalculates after each call.
#[test]
fn test_update_performance_sequential_correctness() {
    let mut record = PerformanceRecord {
        task_count: 0,
        avg_score: None,
        evaluations: vec![],
    };

    // After 1st update
    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: 0.60,
            task_id: "t1".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
    );
    assert_eq!(record.task_count, 1);
    assert!((record.avg_score.unwrap() - 0.60).abs() < 1e-10);

    // After 2nd update: avg = (0.60 + 0.80) / 2 = 0.70
    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: 0.80,
            task_id: "t2".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
    );
    assert_eq!(record.task_count, 2);
    assert!((record.avg_score.unwrap() - 0.70).abs() < 1e-10);

    // After 3rd update: avg = (0.60 + 0.80 + 1.0) / 3 = 0.80
    agency::update_performance(
        &mut record,
        EvaluationRef {
            score: 1.0,
            task_id: "t3".to_string(),
            timestamp: "ts".to_string(),
            context_id: "c".to_string(),
        },
    );
    assert_eq!(record.task_count, 3);
    assert!((record.avg_score.unwrap() - 0.80).abs() < 1e-10);
}
