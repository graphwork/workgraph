use std::fs;
use std::path::{Path, PathBuf};

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

/// Record an evaluation: persist the eval JSON, and update agent, role, and motivation performance.
///
/// Steps:
/// 1. Save the `Evaluation` as JSON in `agency_dir/evaluations/eval-{task_id}-{timestamp}.json`.
/// 2. Load the agent (if agent_id is set), add an `EvaluationRef`, recalculate scores, save.
/// 3. Load the role, add an `EvaluationRef` (with motivation_id as context), recalculate scores, save.
/// 4. Load the motivation, add an `EvaluationRef` (with role_id as context), recalculate scores, save.
///
/// Returns the path to the saved evaluation JSON.
pub fn record_evaluation(
    evaluation: &Evaluation,
    agency_dir: &Path,
) -> Result<PathBuf, AgencyError> {
    init(agency_dir)?;

    let evals_dir = agency_dir.join("evaluations");
    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");
    let agents_dir = agency_dir.join("agents");

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
            context_id: evaluation.motivation_id.clone(),
        };
        update_performance(&mut role.performance, role_eval_ref);
        save_role(&role, &roles_dir)?;
    }

    // 4. Update motivation performance
    if let Ok(mut motivation) =
        find_motivation_by_prefix(&motivations_dir, &evaluation.motivation_id)
    {
        let motivation_eval_ref = EvaluationRef {
            score: evaluation.score,
            task_id: evaluation.task_id.clone(),
            timestamp: evaluation.timestamp.clone(),
            context_id: evaluation.role_id.clone(),
        };
        update_performance(&mut motivation.performance, motivation_eval_ref);
        save_motivation(&motivation, &motivations_dir)?;
    }

    Ok(eval_path)
}

#[cfg(test)]
mod tests {
    use super::super::starters::{build_motivation, build_role};
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn sample_role() -> Role {
        build_role(
            "Implementer",
            "Writes code to fulfil task requirements.",
            vec![
                SkillRef::Name("rust".into()),
                SkillRef::Inline("fn main() {}".into()),
            ],
            "Working, tested code merged to main.",
        )
    }

    fn sample_motivation() -> Motivation {
        build_motivation(
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
        let mut record = PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        };

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
        save_role(&role, &agency_dir.join("roles")).unwrap();
        let motivation = sample_motivation();
        let motivation_id = motivation.id.clone();
        save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

        let eval = Evaluation {
            id: "eval-test-1".into(),
            task_id: "task-42".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            motivation_id: motivation_id.clone(),
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
        let role_path = agency_dir.join("roles").join(format!("{}.yaml", role_id));
        let updated_role = load_role(&role_path).unwrap();
        assert_eq!(updated_role.performance.task_count, 1);
        assert!((updated_role.performance.avg_score.unwrap() - 0.85).abs() < f64::EPSILON);
        assert_eq!(updated_role.performance.evaluations.len(), 1);
        assert_eq!(updated_role.performance.evaluations[0].task_id, "task-42");
        assert_eq!(
            updated_role.performance.evaluations[0].context_id,
            motivation_id
        );

        // 3. Motivation performance was updated
        let motivation_path = agency_dir
            .join("motivations")
            .join(format!("{}.yaml", motivation_id));
        let updated_motivation = load_motivation(&motivation_path).unwrap();
        assert_eq!(updated_motivation.performance.task_count, 1);
        assert!((updated_motivation.performance.avg_score.unwrap() - 0.85).abs() < f64::EPSILON);
        assert_eq!(updated_motivation.performance.evaluations.len(), 1);
        assert_eq!(
            updated_motivation.performance.evaluations[0].context_id,
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
        save_role(&role, &agency_dir.join("roles")).unwrap();
        let motivation = sample_motivation();
        let motivation_id = motivation.id.clone();
        save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

        let eval1 = Evaluation {
            id: "eval-1".into(),
            task_id: "task-1".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            motivation_id: motivation_id.clone(),
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
            motivation_id: motivation_id.clone(),
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

        let role_path = agency_dir.join("roles").join(format!("{}.yaml", role_id));
        let updated_role = load_role(&role_path).unwrap();
        assert_eq!(updated_role.performance.task_count, 2);
        assert!((updated_role.performance.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
        assert_eq!(updated_role.performance.evaluations.len(), 2);

        let motivation_path = agency_dir
            .join("motivations")
            .join(format!("{}.yaml", motivation_id));
        let updated_motivation = load_motivation(&motivation_path).unwrap();
        assert_eq!(updated_motivation.performance.task_count, 2);
        assert!((updated_motivation.performance.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_record_evaluation_missing_role_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let motivation = sample_motivation();
        let motivation_id = motivation.id.clone();
        save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

        let eval = Evaluation {
            id: "eval-orphan".into(),
            task_id: "task-99".into(),
            agent_id: String::new(),
            role_id: "nonexistent-role".into(),
            motivation_id: motivation_id.clone(),
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

        let motivation_path = agency_dir
            .join("motivations")
            .join(format!("{}.yaml", motivation_id));
        let updated = load_motivation(&motivation_path).unwrap();
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
