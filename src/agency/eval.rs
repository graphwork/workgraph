use std::fs;
use std::path::{Path, PathBuf};

use crate::config::AgencyConfig;
use super::store::*;
use super::types::*;

/// Recalculate the average score from a list of EvaluationRefs.
///
/// Returns `None` if the list is empty.
pub fn recalculate_avg_score(evaluations: &[EvaluationRef]) -> Option<f64> {
    if evaluations.is_empty() {
        return None;
    }
    let valid_scores: Vec<f64> = evaluations
        .iter()
        .map(|e| e.score)
        .filter(|s| s.is_finite())
        .collect();
    if valid_scores.is_empty() {
        return None;
    }
    let sum: f64 = valid_scores.iter().sum();
    let avg = sum / valid_scores.len() as f64;
    if avg.is_finite() { Some(avg) } else { None }
}

/// Update a PerformanceRecord with a new evaluation reference.
///
/// Increments task_count, appends the EvaluationRef, and recalculates avg_score.
pub fn update_performance(record: &mut PerformanceRecord, eval_ref: EvaluationRef) {
    record.task_count = record.task_count.saturating_add(1);
    record.evaluations.push(eval_ref);
    record.avg_score = recalculate_avg_score(&record.evaluations);
}

/// Record an evaluation: persist the eval JSON, and update agent, role, and tradeoff performance.
///
/// Steps:
/// 1. Save the `Evaluation` as JSON in `agency_dir/evaluations/eval-{task_id}-{timestamp}.json`.
/// 2. Load the agent (if agent_id is set), add an `EvaluationRef`, recalculate scores, save.
/// 3. Load the role, add an `EvaluationRef` (with tradeoff_id as context), recalculate scores, save.
/// 4. Load the tradeoff, add an `EvaluationRef` (with role_id as context), recalculate scores, save.
///
/// Returns the path to the saved evaluation JSON.
pub fn record_evaluation(
    evaluation: &Evaluation,
    agency_dir: &Path,
) -> Result<PathBuf, AgencyError> {
    init(agency_dir)?;

    let evals_dir = agency_dir.join("evaluations");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let agents_dir = agency_dir.join("cache/agents");

    // 1. Save the full Evaluation JSON with task_id-timestamp naming
    let safe_ts = evaluation.timestamp.replace(':', "-");
    let eval_filename = format!("eval-{}-{}.json", evaluation.task_id, safe_ts);
    let eval_path = evals_dir.join(&eval_filename);
    let json = serde_json::to_string_pretty(evaluation)?;
    fs::write(&eval_path, json)?;

    // 2. Update agent performance (if agent_id is present)
    if !evaluation.agent_id.is_empty()
        && let Ok(mut agent) = find_agent_by_prefix(&agents_dir, &evaluation.agent_id)
    {
        let agent_eval_ref = EvaluationRef {
            score: evaluation.score,
            task_id: evaluation.task_id.clone(),
            timestamp: evaluation.timestamp.clone(),
            context_id: evaluation.role_id.clone(),
        };
        update_performance(&mut agent.performance, agent_eval_ref);
        save_agent(&agent, &agents_dir)?;
    }

    // 3. Update role performance (look up by prefix to support both full and short IDs)
    if let Ok(mut role) = find_role_by_prefix(&roles_dir, &evaluation.role_id) {
        let role_eval_ref = EvaluationRef {
            score: evaluation.score,
            task_id: evaluation.task_id.clone(),
            timestamp: evaluation.timestamp.clone(),
            context_id: evaluation.tradeoff_id.clone(),
        };
        update_performance(&mut role.performance, role_eval_ref);
        save_role(&role, &roles_dir)?;
    }

    // 4. Update tradeoff performance
    if let Ok(mut tradeoff) =
        find_tradeoff_by_prefix(&tradeoffs_dir, &evaluation.tradeoff_id)
    {
        let tradeoff_eval_ref = EvaluationRef {
            score: evaluation.score,
            task_id: evaluation.task_id.clone(),
            timestamp: evaluation.timestamp.clone(),
            context_id: evaluation.role_id.clone(),
        };
        update_performance(&mut tradeoff.performance, tradeoff_eval_ref);
        save_tradeoff(&tradeoff, &tradeoffs_dir)?;
    }

    Ok(eval_path)
}

/// Record an evaluation and trigger retrospective inference for learning assignments.
///
/// This is the recommended entry point when the run mode continuum is active.
/// It calls `record_evaluation` for normal score propagation, then
/// `process_retrospective_inference` to update primitive scores and
/// attractor weights for learning experiments.
pub fn record_evaluation_with_inference(
    evaluation: &Evaluation,
    agency_dir: &Path,
    config: &AgencyConfig,
) -> Result<PathBuf, AgencyError> {
    let eval_path = record_evaluation(evaluation, agency_dir)?;

    // Trigger retrospective inference for learning assignments
    if let Err(e) = super::run_mode::process_retrospective_inference(
        agency_dir,
        &evaluation.task_id,
        evaluation.score,
        config,
    ) {
        eprintln!(
            "Warning: retrospective inference failed for task '{}': {}",
            evaluation.task_id, e
        );
    }

    Ok(eval_path)
}

/// Recalculate the average score from a list of OrgEvalRefs.
pub fn recalculate_org_avg_score(evaluations: &[OrgEvalRef]) -> Option<f64> {
    let valid: Vec<f64> = evaluations.iter().map(|e| e.score).filter(|s| s.is_finite()).collect();
    if valid.is_empty() { return None; }
    let avg = valid.iter().sum::<f64>() / valid.len() as f64;
    if avg.is_finite() { Some(avg) } else { None }
}

/// Update an OrgPerformanceRecord with a new org evaluation reference.
pub fn update_org_performance(record: &mut OrgPerformanceRecord, eval_ref: OrgEvalRef) {
    record.task_count = record.task_count.saturating_add(1);
    record.evaluations.push(eval_ref);
    record.avg_score = recalculate_org_avg_score(&record.evaluations);
}

/// Record an org evaluation: persist the JSON, and update agent, role, and tradeoff
/// org_performance records.
///
/// Steps:
/// 1. Save `OrgEvaluation` as JSON in `agency_dir/org-evaluations/org-eval-{task_id}-{timestamp}.json`.
/// 2. Load the agent (if agent_id is set), update org_performance, save.
/// 3. Load the role, update org_performance, save.
/// 4. Load the tradeoff, update org_performance, save.
///
/// Returns the path to the saved org evaluation JSON.
pub fn record_org_evaluation(
    org_eval: &OrgEvaluation,
    agency_dir: &Path,
) -> Result<PathBuf, AgencyError> {
    init(agency_dir)?;

    let org_evals_dir = agency_dir.join("org-evaluations");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let agents_dir = agency_dir.join("cache/agents");

    // 1. Save the OrgEvaluation JSON
    let safe_ts = org_eval.timestamp.replace(':', "-");
    let filename = format!("org-eval-{}-{}.json", org_eval.task_id, safe_ts);
    let eval_path = org_evals_dir.join(&filename);
    fs::create_dir_all(&org_evals_dir)?;
    fs::write(&eval_path, serde_json::to_string_pretty(org_eval)?)?;

    let org_ref = OrgEvalRef {
        score: org_eval.score,
        task_id: org_eval.task_id.clone(),
        timestamp: org_eval.timestamp.clone(),
        downstream_task_count: org_eval.downstream_task_count,
    };

    // 2. Update agent org_performance (if agent_id is present)
    if !org_eval.agent_id.is_empty()
        && let Ok(mut agent) = find_agent_by_prefix(&agents_dir, &org_eval.agent_id)
    {
        let record = agent.performance.org_performance.get_or_insert_with(OrgPerformanceRecord::default);
        update_org_performance(record, org_ref.clone());
        save_agent(&agent, &agents_dir)?;
    }

    // 3. Update role org_performance
    if let Ok(mut role) = find_role_by_prefix(&roles_dir, &org_eval.role_id) {
        let record = role.performance.org_performance.get_or_insert_with(OrgPerformanceRecord::default);
        update_org_performance(record, org_ref.clone());
        save_role(&role, &roles_dir)?;
    }

    // 4. Update tradeoff org_performance
    if let Ok(mut tradeoff) = find_tradeoff_by_prefix(&tradeoffs_dir, &org_eval.tradeoff_id) {
        let record = tradeoff.performance.org_performance.get_or_insert_with(OrgPerformanceRecord::default);
        update_org_performance(record, org_ref);
        save_tradeoff(&tradeoff, &tradeoffs_dir)?;
    }

    Ok(eval_path)
}

#[cfg(test)]
mod tests {
    use super::super::starters::{build_role, build_tradeoff};
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn sample_role() -> Role {
        build_role(
            "Implementer",
            "Writes code to fulfil task requirements.",
            vec![
                "rust".to_string(),
                "inline:fn main() {}".to_string(),
            ],
            "Working, tested code merged to main.",
        )
    }

    fn sample_tradeoff() -> TradeoffConfig {
        build_tradeoff(
            "Quality First",
            "Prioritise correctness and maintainability.",
            vec!["Slower delivery for higher quality".into()],
            vec!["Skipping tests".into()],
        )
    }

    fn make_eval_ref(score: f64, task_id: &str, context_id: &str) -> EvaluationRef {
        EvaluationRef {
            score,
            task_id: task_id.into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
            context_id: context_id.into(),
        }
    }

    #[test]
    fn test_recalculate_avg_score_empty() {
        assert_eq!(recalculate_avg_score(&[]), None);
    }

    #[test]
    fn test_recalculate_avg_score_single() {
        let refs = vec![make_eval_ref(0.8, "t1", "m1")];
        let avg = recalculate_avg_score(&refs).unwrap();
        assert!((avg - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_recalculate_avg_score_multiple() {
        let refs = vec![
            make_eval_ref(0.6, "t1", "m1"),
            make_eval_ref(0.8, "t2", "m1"),
            make_eval_ref(1.0, "t3", "m1"),
        ];
        let avg = recalculate_avg_score(&refs).unwrap();
        assert!((avg - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_recalculate_avg_score_uneven() {
        let refs = vec![
            make_eval_ref(0.0, "t1", "m1"),
            make_eval_ref(1.0, "t2", "m1"),
        ];
        let avg = recalculate_avg_score(&refs).unwrap();
        assert!((avg - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_update_performance_increments_and_recalculates() {
        let mut record = PerformanceRecord::default();

        update_performance(&mut record, make_eval_ref(0.8, "t1", "m1"));
        assert_eq!(record.task_count, 1);
        assert!((record.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
        assert_eq!(record.evaluations.len(), 1);

        update_performance(&mut record, make_eval_ref(0.6, "t2", "m1"));
        assert_eq!(record.task_count, 2);
        assert!((record.avg_score.unwrap() - 0.7).abs() < f64::EPSILON);
        assert_eq!(record.evaluations.len(), 2);

        update_performance(&mut record, make_eval_ref(1.0, "t3", "m1"));
        assert_eq!(record.task_count, 3);
        assert!((record.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
        assert_eq!(record.evaluations.len(), 3);
    }

    #[test]
    fn test_update_performance_from_existing() {
        let mut record = PerformanceRecord {
            task_count: 2,
            avg_score: Some(0.7),
            evaluations: vec![
                make_eval_ref(0.6, "t1", "m1"),
                make_eval_ref(0.8, "t2", "m1"),
            ],
            org_performance: None,
        };

        update_performance(&mut record, make_eval_ref(0.9, "t3", "m1"));
        assert_eq!(record.task_count, 3);
        let expected = (0.6 + 0.8 + 0.9) / 3.0;
        assert!((record.avg_score.unwrap() - expected).abs() < 1e-10);
    }

    #[test]
    fn test_record_evaluation_saves_all_artifacts() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let role = sample_role();
        let role_id = role.id.clone();
        save_role(&role, &agency_dir.join("cache/roles")).unwrap();
        let tradeoff = sample_tradeoff();
        let tradeoff_id = tradeoff.id.clone();
        save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

        let eval = Evaluation {
            id: "eval-test-1".into(),
            task_id: "task-42".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            score: 0.85,
            dimensions: HashMap::new(),
            notes: "Good work".into(),
            evaluator: "test".into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
        };

        let eval_path = record_evaluation(&eval, &agency_dir).unwrap();

        // 1. Evaluation JSON was saved
        assert!(eval_path.exists());
        let saved_eval = load_evaluation(&eval_path).unwrap();
        assert_eq!(saved_eval.score, 0.85);
        assert_eq!(saved_eval.task_id, "task-42");

        // 2. Role performance was updated
        let role_path = agency_dir.join("cache/roles").join(format!("{}.yaml", role_id));
        let updated_role = load_role(&role_path).unwrap();
        assert_eq!(updated_role.performance.task_count, 1);
        assert!((updated_role.performance.avg_score.unwrap() - 0.85).abs() < f64::EPSILON);
        assert_eq!(updated_role.performance.evaluations.len(), 1);
        assert_eq!(updated_role.performance.evaluations[0].task_id, "task-42");
        assert_eq!(
            updated_role.performance.evaluations[0].context_id,
            tradeoff_id
        );

        // 3. Motivation performance was updated
        let tradeoff_path = agency_dir
            .join("primitives/tradeoffs")
            .join(format!("{}.yaml", tradeoff_id));
        let updated_tradeoff = load_tradeoff(&tradeoff_path).unwrap();
        assert_eq!(updated_tradeoff.performance.task_count, 1);
        assert!((updated_tradeoff.performance.avg_score.unwrap() - 0.85).abs() < f64::EPSILON);
        assert_eq!(updated_tradeoff.performance.evaluations.len(), 1);
        assert_eq!(
            updated_tradeoff.performance.evaluations[0].context_id,
            role_id
        );
    }

    #[test]
    fn test_record_evaluation_multiple_accumulates() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let role = sample_role();
        let role_id = role.id.clone();
        save_role(&role, &agency_dir.join("cache/roles")).unwrap();
        let tradeoff = sample_tradeoff();
        let tradeoff_id = tradeoff.id.clone();
        save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

        let eval1 = Evaluation {
            id: "eval-1".into(),
            task_id: "task-1".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            score: 0.6,
            dimensions: HashMap::new(),
            notes: "".into(),
            evaluator: "test".into(),
            timestamp: "2025-05-01T10:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
        };

        let eval2 = Evaluation {
            id: "eval-2".into(),
            task_id: "task-2".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            score: 1.0,
            dimensions: HashMap::new(),
            notes: "".into(),
            evaluator: "test".into(),
            timestamp: "2025-05-01T11:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
        };

        record_evaluation(&eval1, &agency_dir).unwrap();
        record_evaluation(&eval2, &agency_dir).unwrap();

        let role_path = agency_dir.join("cache/roles").join(format!("{}.yaml", role_id));
        let updated_role = load_role(&role_path).unwrap();
        assert_eq!(updated_role.performance.task_count, 2);
        assert!((updated_role.performance.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
        assert_eq!(updated_role.performance.evaluations.len(), 2);

        let tradeoff_path = agency_dir
            .join("primitives/tradeoffs")
            .join(format!("{}.yaml", tradeoff_id));
        let updated_tradeoff = load_tradeoff(&tradeoff_path).unwrap();
        assert_eq!(updated_tradeoff.performance.task_count, 2);
        assert!((updated_tradeoff.performance.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_record_evaluation_missing_role_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let tradeoff = sample_tradeoff();
        let tradeoff_id = tradeoff.id.clone();
        save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

        let eval = Evaluation {
            id: "eval-orphan".into(),
            task_id: "task-99".into(),
            agent_id: String::new(),
            role_id: "nonexistent-role".into(),
            tradeoff_id: tradeoff_id.clone(),
            score: 0.5,
            dimensions: HashMap::new(),
            notes: "".into(),
            evaluator: "test".into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
        };

        let result = record_evaluation(&eval, &agency_dir);
        assert!(result.is_ok());

        let tradeoff_path = agency_dir
            .join("primitives/tradeoffs")
            .join(format!("{}.yaml", tradeoff_id));
        let updated = load_tradeoff(&tradeoff_path).unwrap();
        assert_eq!(updated.performance.task_count, 1);
    }

    #[test]
    fn test_evaluation_ref_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut role = sample_role();
        role.performance.evaluations.push(EvaluationRef {
            score: 0.75,
            task_id: "task-abc".into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
            context_id: "motivation-xyz".into(),
        });
        role.performance.task_count = 1;
        role.performance.avg_score = Some(0.75);

        let path = save_role(&role, tmp.path()).unwrap();
        let loaded = load_role(&path).unwrap();

        assert_eq!(loaded.performance.evaluations.len(), 1);
        let ref0 = &loaded.performance.evaluations[0];
        assert!((ref0.score - 0.75).abs() < f64::EPSILON);
        assert_eq!(ref0.task_id, "task-abc");
        assert_eq!(ref0.timestamp, "2025-05-01T12:00:00Z");
        assert_eq!(ref0.context_id, "motivation-xyz");
    }
}
