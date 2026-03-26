use std::fs;
use std::path::{Path, PathBuf};

use super::store::*;
use super::types::*;
use crate::config::AgencyConfig;

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

/// Record an evaluation: persist the eval JSON, and update agent, role, tradeoff, component,
/// and outcome performance.
///
/// Steps:
/// 1. Save the `Evaluation` as JSON in `agency_dir/evaluations/eval-{task_id}-{timestamp}.json`.
/// 2. Load the agent (if agent_id is set), add an `EvaluationRef`, recalculate scores, save.
/// 3. Load the role, add an `EvaluationRef` (with tradeoff_id as context), recalculate scores, save.
/// 4. Load the tradeoff, add an `EvaluationRef` (with role_id as context), recalculate scores, save.
/// 5. Propagate to each role component (with role_id as context), recalculate scores, save.
/// 6. Propagate to the role's desired outcome (with agent_id as context), recalculate scores, save.
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
    let mut role_component_ids: Vec<String> = Vec::new();
    let mut role_outcome_id = String::new();
    if let Ok(mut role) = find_role_by_prefix(&roles_dir, &evaluation.role_id) {
        role_component_ids = role.component_ids.clone();
        role_outcome_id = role.outcome_id.clone();
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
    if let Ok(mut tradeoff) = find_tradeoff_by_prefix(&tradeoffs_dir, &evaluation.tradeoff_id) {
        let tradeoff_eval_ref = EvaluationRef {
            score: evaluation.score,
            task_id: evaluation.task_id.clone(),
            timestamp: evaluation.timestamp.clone(),
            context_id: evaluation.role_id.clone(),
        };
        update_performance(&mut tradeoff.performance, tradeoff_eval_ref);
        save_tradeoff(&tradeoff, &tradeoffs_dir)?;
    }

    // 5. Propagate to each role component (context_id = role_id)
    let components_dir = agency_dir.join("primitives/components");
    for comp_id in &role_component_ids {
        if comp_id.is_empty() {
            continue;
        }
        match find_component_by_prefix(&components_dir, comp_id) {
            Ok(mut component) => {
                let comp_eval_ref = EvaluationRef {
                    score: evaluation.score,
                    task_id: evaluation.task_id.clone(),
                    timestamp: evaluation.timestamp.clone(),
                    context_id: evaluation.role_id.clone(),
                };
                update_performance(&mut component.performance, comp_eval_ref);
                save_component(&component, &components_dir)?;
            }
            Err(e) => {
                eprintln!(
                    "Warning: could not propagate eval to component '{}': {}",
                    comp_id, e
                );
            }
        }
    }

    // 6. Propagate to the role's desired outcome (context_id = agent_id)
    let outcomes_dir = agency_dir.join("primitives/outcomes");
    if !role_outcome_id.is_empty() {
        match find_outcome_by_prefix(&outcomes_dir, &role_outcome_id) {
            Ok(mut outcome) => {
                let outcome_eval_ref = EvaluationRef {
                    score: evaluation.score,
                    task_id: evaluation.task_id.clone(),
                    timestamp: evaluation.timestamp.clone(),
                    context_id: evaluation.agent_id.clone(),
                };
                update_performance(&mut outcome.performance, outcome_eval_ref);
                save_outcome(&outcome, &outcomes_dir)?;
            }
            Err(e) => {
                eprintln!(
                    "Warning: could not propagate eval to outcome '{}': {}",
                    role_outcome_id, e
                );
            }
        }
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

    // POST evaluation to Agency bridge when configured
    if config.agency_server_url.is_some() {
        let assignments_dir = agency_dir.join("assignments");
        if let Ok(record) = load_assignment_record_by_task(&assignments_dir, &evaluation.task_id)
            && let Some(ref agency_task_id) = record.agency_task_id
            && let Err(e) =
                super::agency_bridge::post_evaluation_to_agency(evaluation, agency_task_id, config)
        {
            eprintln!(
                "Warning: agency bridge POST failed for task '{}': {}",
                evaluation.task_id, e
            );
        }
    }

    Ok(eval_path)
}

// ---------------------------------------------------------------------------
// Proper scoring rules (Brier accuracy, calibration, resolution)
// ---------------------------------------------------------------------------

/// Brier score for a single prediction: (prediction - outcome)^2.
///
/// `prediction` is the evaluator's score (0–1) and `outcome` is the
/// ground-truth outcome (0 or 1, e.g. did the task actually succeed?).
/// Lower is better. Perfect prediction on a binary outcome gives 0.0.
pub fn brier_score(prediction: f64, outcome: f64) -> f64 {
    (prediction - outcome).powi(2)
}

/// Average Brier score across multiple (prediction, outcome) pairs.
///
/// Used to assess evaluator accuracy: are the scores calibrated
/// against actual task outcomes?
pub fn brier_accuracy(pairs: &[(f64, f64)]) -> Option<f64> {
    if pairs.is_empty() {
        return None;
    }
    let sum: f64 = pairs.iter().map(|(p, o)| brier_score(*p, *o)).sum();
    Some(sum / pairs.len() as f64)
}

/// Calibration error: average absolute difference between predicted
/// probability and observed frequency within bins.
///
/// Bins scores into `n_bins` equal-width buckets and computes:
///   (1/K) * Σ |avg_prediction_in_bin - fraction_positive_in_bin|
///
/// Perfect calibration returns 0.0. Scores are expected in [0, 1].
pub fn calibration_error(pairs: &[(f64, f64)], n_bins: usize) -> Option<f64> {
    if pairs.is_empty() || n_bins == 0 {
        return None;
    }
    let bin_width = 1.0 / n_bins as f64;
    let mut total_error = 0.0;
    let mut bins_used = 0;

    for i in 0..n_bins {
        let lower = i as f64 * bin_width;
        let upper = if i == n_bins - 1 {
            1.0 + f64::EPSILON
        } else {
            (i + 1) as f64 * bin_width
        };

        let in_bin: Vec<&(f64, f64)> = pairs
            .iter()
            .filter(|(p, _)| *p >= lower && *p < upper)
            .collect();

        if in_bin.is_empty() {
            continue;
        }

        let avg_pred = in_bin.iter().map(|(p, _)| p).sum::<f64>() / in_bin.len() as f64;
        let frac_positive = in_bin.iter().map(|(_, o)| o).sum::<f64>() / in_bin.len() as f64;

        total_error += (avg_pred - frac_positive).abs();
        bins_used += 1;
    }

    if bins_used == 0 {
        return None;
    }
    Some(total_error / bins_used as f64)
}

/// Resolution: the variance of the observed frequencies across bins.
///
/// Higher resolution means the evaluator can discriminate between
/// events that happen and those that don't. Computed as:
///   (1/K) * Σ (fraction_positive_in_bin - base_rate)^2 * n_in_bin / N
///
/// Returns a value in [0, 0.25] (maximum when base_rate is 0.5).
pub fn resolution(pairs: &[(f64, f64)], n_bins: usize) -> Option<f64> {
    if pairs.is_empty() || n_bins == 0 {
        return None;
    }
    let n = pairs.len() as f64;
    let base_rate = pairs.iter().map(|(_, o)| o).sum::<f64>() / n;
    let bin_width = 1.0 / n_bins as f64;
    let mut total = 0.0;

    for i in 0..n_bins {
        let lower = i as f64 * bin_width;
        let upper = if i == n_bins - 1 {
            1.0 + f64::EPSILON
        } else {
            (i + 1) as f64 * bin_width
        };

        let in_bin: Vec<&(f64, f64)> = pairs
            .iter()
            .filter(|(p, _)| *p >= lower && *p < upper)
            .collect();

        if in_bin.is_empty() {
            continue;
        }

        let frac_positive = in_bin.iter().map(|(_, o)| o).sum::<f64>() / in_bin.len() as f64;
        total += (frac_positive - base_rate).powi(2) * in_bin.len() as f64 / n;
    }

    Some(total)
}

/// Combined scoring metrics for an evaluator.
#[derive(Debug, Clone)]
pub struct ScoringMetrics {
    pub brier: f64,
    pub calibration: f64,
    pub resolution: f64,
}

/// Compute all three proper scoring metrics for a set of (prediction, outcome) pairs.
pub fn compute_scoring_metrics(pairs: &[(f64, f64)], n_bins: usize) -> Option<ScoringMetrics> {
    let brier = brier_accuracy(pairs)?;
    let cal = calibration_error(pairs, n_bins)?;
    let res = resolution(pairs, n_bins)?;
    Some(ScoringMetrics {
        brier,
        calibration: cal,
        resolution: res,
    })
}

#[cfg(test)]
mod tests {
    use super::super::starters::{build_component, build_outcome, build_role, build_tradeoff};
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn test_brier_score_perfect() {
        assert!((brier_score(1.0, 1.0)).abs() < f64::EPSILON);
        assert!((brier_score(0.0, 0.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_brier_score_worst() {
        assert!((brier_score(0.0, 1.0) - 1.0).abs() < f64::EPSILON);
        assert!((brier_score(1.0, 0.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_brier_accuracy_basic() {
        let pairs = vec![(0.9, 1.0), (0.1, 0.0)];
        let b = brier_accuracy(&pairs).unwrap();
        // (0.01 + 0.01) / 2 = 0.01
        assert!((b - 0.01).abs() < 1e-10);
    }

    #[test]
    fn test_brier_accuracy_empty() {
        assert!(brier_accuracy(&[]).is_none());
    }

    #[test]
    fn test_calibration_error_perfect() {
        // All predictions in one bin that perfectly match outcomes
        let pairs = vec![(0.5, 0.5), (0.5, 0.5)];
        let c = calibration_error(&pairs, 5).unwrap();
        assert!(c.abs() < f64::EPSILON);
    }

    #[test]
    fn test_calibration_error_empty() {
        assert!(calibration_error(&[], 5).is_none());
    }

    #[test]
    fn test_resolution_empty() {
        assert!(resolution(&[], 5).is_none());
    }

    #[test]
    fn test_resolution_no_discrimination() {
        // All outcomes the same, so no discrimination possible
        let pairs = vec![(0.1, 1.0), (0.5, 1.0), (0.9, 1.0)];
        let r = resolution(&pairs, 5).unwrap();
        // base_rate = 1.0, all bin fractions = 1.0, so resolution = 0
        assert!(r.abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_scoring_metrics() {
        let pairs = vec![(0.8, 1.0), (0.2, 0.0), (0.9, 1.0), (0.1, 0.0)];
        let metrics = compute_scoring_metrics(&pairs, 5).unwrap();
        assert!(metrics.brier < 0.05); // Good predictions
        assert!(metrics.calibration < 0.5); // Reasonable calibration
    }

    fn sample_role() -> Role {
        build_role(
            "Implementer",
            "Writes code to fulfil task requirements.",
            vec!["rust".to_string(), "inline:fn main() {}".to_string()],
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
    fn test_classify_rubric_level() {
        use super::super::types::{RubricLevel, classify_rubric_level};
        assert_eq!(classify_rubric_level(0.0), RubricLevel::Failing);
        assert_eq!(classify_rubric_level(0.1), RubricLevel::Failing);
        assert_eq!(classify_rubric_level(0.19), RubricLevel::Failing);
        assert_eq!(classify_rubric_level(0.2), RubricLevel::BelowExpectations);
        assert_eq!(classify_rubric_level(0.39), RubricLevel::BelowExpectations);
        assert_eq!(classify_rubric_level(0.4), RubricLevel::MeetsExpectations);
        assert_eq!(classify_rubric_level(0.59), RubricLevel::MeetsExpectations);
        assert_eq!(classify_rubric_level(0.6), RubricLevel::ExceedsExpectations);
        assert_eq!(
            classify_rubric_level(0.79),
            RubricLevel::ExceedsExpectations
        );
        assert_eq!(classify_rubric_level(0.8), RubricLevel::Exceptional);
        assert_eq!(classify_rubric_level(1.0), RubricLevel::Exceptional);
    }

    #[test]
    fn test_rubric_level_display() {
        use super::super::types::{RubricLevel, classify_rubric_level};
        assert_eq!(RubricLevel::Failing.to_string(), "Failing");
        assert_eq!(
            RubricLevel::BelowExpectations.to_string(),
            "Below Expectations"
        );
        assert_eq!(
            RubricLevel::MeetsExpectations.to_string(),
            "Meets Expectations"
        );
        assert_eq!(
            RubricLevel::ExceedsExpectations.to_string(),
            "Exceeds Expectations"
        );
        assert_eq!(RubricLevel::Exceptional.to_string(), "Exceptional");
        // label() and Display should match
        assert_eq!(classify_rubric_level(0.5).label(), "Meets Expectations");
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
        let role_path = agency_dir
            .join("cache/roles")
            .join(format!("{}.yaml", role_id));
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

        let role_path = agency_dir
            .join("cache/roles")
            .join(format!("{}.yaml", role_id));
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

    #[test]
    fn test_record_evaluation_propagates_to_components() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        // Create two components
        let comp1 = build_component(
            "Rust coding",
            "Write idiomatic Rust",
            ComponentCategory::Translated,
            ContentRef::Inline("fn main() {}".into()),
        );
        let comp2 = build_component(
            "Testing",
            "Write thorough tests",
            ComponentCategory::Enhanced,
            ContentRef::Inline("assert!(true)".into()),
        );
        save_component(&comp1, &agency_dir.join("primitives/components")).unwrap();
        save_component(&comp2, &agency_dir.join("primitives/components")).unwrap();

        // Create an outcome
        let outcome = build_outcome(
            "Merged code",
            "Working, tested code merged to main.",
            vec!["Tests pass".into()],
        );
        save_outcome(&outcome, &agency_dir.join("primitives/outcomes")).unwrap();

        // Create a role referencing those components and outcome
        let role = build_role(
            "Implementer",
            "Writes code",
            vec![comp1.id.clone(), comp2.id.clone()],
            &outcome.id,
        );
        let role_id = role.id.clone();
        save_role(&role, &agency_dir.join("cache/roles")).unwrap();

        let tradeoff = sample_tradeoff();
        let tradeoff_id = tradeoff.id.clone();
        save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

        let eval = Evaluation {
            id: "eval-comp-1".into(),
            task_id: "task-50".into(),
            agent_id: "agent-abc".into(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            score: 0.9,
            dimensions: HashMap::new(),
            notes: "Great".into(),
            evaluator: "test".into(),
            timestamp: "2025-06-01T12:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
        };

        record_evaluation(&eval, &agency_dir).unwrap();

        // Verify component 1 was updated
        let updated_comp1 =
            find_component_by_prefix(&agency_dir.join("primitives/components"), &comp1.id).unwrap();
        assert_eq!(updated_comp1.performance.task_count, 1);
        assert!((updated_comp1.performance.avg_score.unwrap() - 0.9).abs() < f64::EPSILON);
        assert_eq!(updated_comp1.performance.evaluations[0].context_id, role_id);

        // Verify component 2 was updated
        let updated_comp2 =
            find_component_by_prefix(&agency_dir.join("primitives/components"), &comp2.id).unwrap();
        assert_eq!(updated_comp2.performance.task_count, 1);
        assert!((updated_comp2.performance.avg_score.unwrap() - 0.9).abs() < f64::EPSILON);
        assert_eq!(updated_comp2.performance.evaluations[0].context_id, role_id);
    }

    #[test]
    fn test_record_evaluation_propagates_to_outcome() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let outcome = build_outcome(
            "Merged code",
            "Working, tested code merged to main.",
            vec!["Tests pass".into()],
        );
        save_outcome(&outcome, &agency_dir.join("primitives/outcomes")).unwrap();

        let role = build_role("Implementer", "Writes code", vec![], &outcome.id);
        let role_id = role.id.clone();
        save_role(&role, &agency_dir.join("cache/roles")).unwrap();

        let tradeoff = sample_tradeoff();
        let tradeoff_id = tradeoff.id.clone();
        save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

        let eval = Evaluation {
            id: "eval-out-1".into(),
            task_id: "task-51".into(),
            agent_id: "agent-xyz".into(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            score: 0.75,
            dimensions: HashMap::new(),
            notes: "Decent".into(),
            evaluator: "test".into(),
            timestamp: "2025-06-01T13:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
        };

        record_evaluation(&eval, &agency_dir).unwrap();

        // Verify outcome was updated with agent_id as context_id
        let updated_outcome =
            find_outcome_by_prefix(&agency_dir.join("primitives/outcomes"), &outcome.id).unwrap();
        assert_eq!(updated_outcome.performance.task_count, 1);
        assert!((updated_outcome.performance.avg_score.unwrap() - 0.75).abs() < f64::EPSILON);
        assert_eq!(
            updated_outcome.performance.evaluations[0].context_id,
            "agent-xyz"
        );
    }

    #[test]
    fn test_record_evaluation_missing_component_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        // Create a role with component_ids that don't exist on disk
        let role = build_role(
            "Implementer",
            "Writes code",
            vec!["nonexistent-comp-id".into()],
            "",
        );
        let role_id = role.id.clone();
        save_role(&role, &agency_dir.join("cache/roles")).unwrap();

        let tradeoff = sample_tradeoff();
        let tradeoff_id = tradeoff.id.clone();
        save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

        let eval = Evaluation {
            id: "eval-miss-1".into(),
            task_id: "task-52".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            score: 0.5,
            dimensions: HashMap::new(),
            notes: "".into(),
            evaluator: "test".into(),
            timestamp: "2025-06-01T14:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
        };

        // Should not error — missing components/outcomes are warned, not failed
        let result = record_evaluation(&eval, &agency_dir);
        assert!(result.is_ok());
    }

    #[test]
    fn test_record_evaluation_full_4_level_cascade() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        // Build all 4 levels
        let comp = build_component(
            "Rust coding",
            "Write idiomatic Rust",
            ComponentCategory::Translated,
            ContentRef::Inline("fn main() {}".into()),
        );
        save_component(&comp, &agency_dir.join("primitives/components")).unwrap();

        let outcome = build_outcome(
            "Merged code",
            "Working, tested code merged to main.",
            vec!["Tests pass".into()],
        );
        save_outcome(&outcome, &agency_dir.join("primitives/outcomes")).unwrap();

        let tradeoff = sample_tradeoff();
        let tradeoff_id = tradeoff.id.clone();
        save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

        let role = build_role(
            "Implementer",
            "Writes code",
            vec![comp.id.clone()],
            &outcome.id,
        );
        let role_id = role.id.clone();
        save_role(&role, &agency_dir.join("cache/roles")).unwrap();

        // Record two evaluations to test accumulation
        for (i, score) in [(1, 0.7), (2, 0.9)] {
            let eval = Evaluation {
                id: format!("eval-full-{}", i),
                task_id: format!("task-{}", i),
                agent_id: "agent-full".into(),
                role_id: role_id.clone(),
                tradeoff_id: tradeoff_id.clone(),
                score,
                dimensions: HashMap::new(),
                notes: "".into(),
                evaluator: "test".into(),
                timestamp: format!("2025-06-01T1{}:00:00Z", i),
                model: None,
                source: "llm".to_string(),
            };
            record_evaluation(&eval, &agency_dir).unwrap();
        }

        let expected_avg = (0.7 + 0.9) / 2.0;

        // Check role
        let updated_role = find_role_by_prefix(&agency_dir.join("cache/roles"), &role_id).unwrap();
        assert_eq!(updated_role.performance.task_count, 2);
        assert!((updated_role.performance.avg_score.unwrap() - expected_avg).abs() < f64::EPSILON);

        // Check tradeoff
        let updated_tradeoff =
            find_tradeoff_by_prefix(&agency_dir.join("primitives/tradeoffs"), &tradeoff_id)
                .unwrap();
        assert_eq!(updated_tradeoff.performance.task_count, 2);
        assert!(
            (updated_tradeoff.performance.avg_score.unwrap() - expected_avg).abs() < f64::EPSILON
        );

        // Check component
        let updated_comp =
            find_component_by_prefix(&agency_dir.join("primitives/components"), &comp.id).unwrap();
        assert_eq!(updated_comp.performance.task_count, 2);
        assert!((updated_comp.performance.avg_score.unwrap() - expected_avg).abs() < f64::EPSILON);
        // Component context_id should be role_id
        assert_eq!(updated_comp.performance.evaluations[0].context_id, role_id);

        // Check outcome
        let updated_outcome =
            find_outcome_by_prefix(&agency_dir.join("primitives/outcomes"), &outcome.id).unwrap();
        assert_eq!(updated_outcome.performance.task_count, 2);
        assert!(
            (updated_outcome.performance.avg_score.unwrap() - expected_avg).abs() < f64::EPSILON
        );
        // Outcome context_id should be agent_id
        assert_eq!(
            updated_outcome.performance.evaluations[0].context_id,
            "agent-full"
        );
    }
}
