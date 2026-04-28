//! Auto-evolver state management.
//!
//! Tracks evolution history, pre-evolution baselines, and determines when
//! automatic evolution should trigger based on evaluation data.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use super::store::{AgencyError, load_all_evaluations};
use crate::config::AgencyConfig;

/// Safe strategies allowed in automatic evolution cycles.
/// Excludes crossover and bizarre-ideation which require human judgment.
pub const SAFE_STRATEGIES: &[&str] = &[
    "mutation",
    "gap-analysis",
    "retirement",
    "motivation-tuning",
];

/// Maximum operations per automatic evolution cycle.
const DEFAULT_MAX_OPS: u32 = 5;

/// Record of a single automatic evolution cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionRecord {
    pub run_id: String,
    pub timestamp: String,
    pub evaluations_consumed: u32,
    pub operations_applied: u32,
    pub strategies_used: Vec<String>,
    pub pre_evolution_avg_score: Option<f64>,
    #[serde(default)]
    pub task_id: Option<String>,
}

/// Persistent evolver state, stored in `.workgraph/agency/evolver_state.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvolverState {
    /// Total evaluations processed up to the last evolution cycle.
    pub last_eval_count: u32,
    /// Timestamp of the last evolution cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_evolution_at: Option<String>,
    /// History of evolution cycles.
    #[serde(default)]
    pub history: Vec<EvolutionRecord>,
    /// Pre-evolution baseline average scores by role ID.
    #[serde(default)]
    pub baselines: std::collections::HashMap<String, f64>,
}

impl EvolverState {
    /// Load evolver state from disk. Returns default if file doesn't exist.
    pub fn load(agency_dir: &Path) -> Self {
        let path = Self::path(agency_dir);
        if !path.exists() {
            return Self::default();
        }
        match fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save evolver state to disk.
    pub fn save(&self, agency_dir: &Path) -> Result<(), AgencyError> {
        let path = Self::path(agency_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        Ok(())
    }

    /// Path to the evolver state file.
    pub fn path(agency_dir: &Path) -> PathBuf {
        agency_dir.join("evolver_state.json")
    }

    /// Record a completed evolution cycle.
    pub fn record_evolution(
        &mut self,
        run_id: &str,
        evaluations_consumed: u32,
        operations_applied: u32,
        strategies_used: Vec<String>,
        pre_avg_score: Option<f64>,
        task_id: Option<&str>,
    ) {
        self.last_eval_count += evaluations_consumed;
        self.last_evolution_at = Some(Utc::now().to_rfc3339());
        self.history.push(EvolutionRecord {
            run_id: run_id.to_string(),
            timestamp: Utc::now().to_rfc3339(),
            evaluations_consumed,
            operations_applied,
            strategies_used,
            pre_evolution_avg_score: pre_avg_score,
            task_id: task_id.map(String::from),
        });
    }
}

/// Trigger reason for an automatic evolution.
#[derive(Debug, Clone, PartialEq)]
pub enum EvolutionTrigger {
    /// Normal threshold: enough new evaluations accumulated.
    Threshold { new_evals: u32 },
    /// Reactive: average score dropped below threshold.
    Reactive { avg_score: f64 },
}

/// Check whether evolution should be triggered.
///
/// Returns `Some(trigger)` if evolution is warranted, `None` otherwise.
pub fn should_trigger_evolution(
    agency_dir: &Path,
    config: &AgencyConfig,
    state: &EvolverState,
) -> Option<EvolutionTrigger> {
    if !config.auto_evolve {
        return None;
    }

    // Count current evaluations
    let evals_dir = agency_dir.join("evaluations");
    let current_eval_count = count_evaluation_files(&evals_dir);

    let new_evals = current_eval_count.saturating_sub(state.last_eval_count);

    // Check minimum interval
    if let Some(ref last_ts) = state.last_evolution_at
        && let Ok(last_time) = last_ts.parse::<DateTime<Utc>>()
    {
        let elapsed = Utc::now().signed_duration_since(last_time);
        if elapsed.num_seconds() < config.evolution_interval as i64 {
            // Interval not met — only allow reactive trigger
            return check_reactive_trigger(agency_dir, config, new_evals);
        }
    }

    // Check threshold trigger
    if new_evals >= config.evolution_threshold {
        return Some(EvolutionTrigger::Threshold { new_evals });
    }

    // Check reactive trigger even if threshold not met
    check_reactive_trigger(agency_dir, config, new_evals)
}

/// Check for reactive trigger (avg score below threshold).
fn check_reactive_trigger(
    agency_dir: &Path,
    config: &AgencyConfig,
    new_evals: u32,
) -> Option<EvolutionTrigger> {
    // Need at least some evaluations to compute an average
    if new_evals < 3 {
        return None;
    }

    let evals_dir = agency_dir.join("evaluations");
    let evaluations = load_all_evaluations(&evals_dir).ok()?;
    if evaluations.is_empty() {
        return None;
    }

    // Compute average of the most recent evaluations (up to new_evals count)
    let recent: Vec<_> = evaluations.iter().rev().take(new_evals as usize).collect();
    let valid_scores: Vec<f64> = recent
        .iter()
        .map(|e| e.score)
        .filter(|s| s.is_finite())
        .collect();

    if valid_scores.is_empty() {
        return None;
    }

    let avg = valid_scores.iter().sum::<f64>() / valid_scores.len() as f64;

    if avg < config.evolution_reactive_threshold {
        Some(EvolutionTrigger::Reactive { avg_score: avg })
    } else {
        None
    }
}

/// Count evaluation JSON files in the evaluations directory.
pub fn count_evaluation_files(dir: &Path) -> u32 {
    if !dir.is_dir() {
        return 0;
    }
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .count() as u32
        })
        .unwrap_or(0)
}

/// Compute the current average evaluation score for baseline tracking.
pub fn compute_current_avg_score(agency_dir: &Path) -> Option<f64> {
    let evals_dir = agency_dir.join("evaluations");
    let evaluations = load_all_evaluations(&evals_dir).ok()?;
    if evaluations.is_empty() {
        return None;
    }
    let valid: Vec<f64> = evaluations
        .iter()
        .map(|e| e.score)
        .filter(|s| s.is_finite())
        .collect();
    if valid.is_empty() {
        return None;
    }
    Some(valid.iter().sum::<f64>() / valid.len() as f64)
}

/// Strategy string for auto-evolution (safe subset only).
pub fn safe_strategy_label() -> &'static str {
    "mutation"
}

/// Get the budget cap for auto-evolution, respecting the config.
pub fn evolution_budget(config: &AgencyConfig) -> u32 {
    config.evolution_budget.min(DEFAULT_MAX_OPS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_evolver_state_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        fs::create_dir_all(&agency_dir).unwrap();

        let mut state = EvolverState::default();
        state.last_eval_count = 15;
        state.record_evolution(
            "run-test-001",
            10,
            3,
            vec!["mutation".to_string(), "gap-analysis".to_string()],
            Some(0.72),
            Some(".evolve-test-001"),
        );

        state.save(&agency_dir).unwrap();

        let loaded = EvolverState::load(&agency_dir);
        assert_eq!(loaded.last_eval_count, 25); // 15 + 10
        assert!(loaded.last_evolution_at.is_some());
        assert_eq!(loaded.history.len(), 1);
        assert_eq!(loaded.history[0].run_id, "run-test-001");
        assert_eq!(loaded.history[0].evaluations_consumed, 10);
        assert_eq!(loaded.history[0].operations_applied, 3);
        assert!((loaded.history[0].pre_evolution_avg_score.unwrap() - 0.72).abs() < f64::EPSILON);
    }

    #[test]
    fn test_evolver_state_load_missing_returns_default() {
        let tmp = TempDir::new().unwrap();
        let state = EvolverState::load(tmp.path());
        assert_eq!(state.last_eval_count, 0);
        assert!(state.last_evolution_at.is_none());
        assert!(state.history.is_empty());
    }

    #[test]
    fn test_should_trigger_disabled() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        fs::create_dir_all(agency_dir.join("evaluations")).unwrap();

        let config = AgencyConfig {
            auto_evolve: false,
            ..AgencyConfig::default()
        };
        let state = EvolverState::default();

        assert!(should_trigger_evolution(&agency_dir, &config, &state).is_none());
    }

    #[test]
    fn test_should_trigger_threshold() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        let evals_dir = agency_dir.join("evaluations");
        fs::create_dir_all(&evals_dir).unwrap();

        // Create 12 evaluation files
        for i in 0..12 {
            let eval = crate::agency::Evaluation {
                id: format!("eval-{}", i),
                task_id: format!("task-{}", i),
                agent_id: String::new(),
                role_id: "role-1".into(),
                tradeoff_id: "tradeoff-1".into(),
                score: 0.7,
                dimensions: std::collections::HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: format!("2025-06-01T1{}:00:00Z", i),
                model: None,
                source: "llm".into(),
                loop_iteration: 0,
            };
            let path = evals_dir.join(format!("eval-{}.json", i));
            fs::write(&path, serde_json::to_string(&eval).unwrap()).unwrap();
        }

        let config = AgencyConfig {
            auto_evolve: true,
            evolution_threshold: 10,
            evolution_interval: 7200,
            ..AgencyConfig::default()
        };
        let state = EvolverState::default();

        let trigger = should_trigger_evolution(&agency_dir, &config, &state);
        assert!(trigger.is_some());
        assert!(matches!(
            trigger.unwrap(),
            EvolutionTrigger::Threshold { new_evals: 12 }
        ));
    }

    #[test]
    fn test_should_trigger_reactive() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        let evals_dir = agency_dir.join("evaluations");
        fs::create_dir_all(&evals_dir).unwrap();

        // Create 5 low-scoring evaluations
        for i in 0..5 {
            let eval = crate::agency::Evaluation {
                id: format!("eval-{}", i),
                task_id: format!("task-{}", i),
                agent_id: String::new(),
                role_id: "role-1".into(),
                tradeoff_id: "tradeoff-1".into(),
                score: 0.2, // Below 0.4 threshold
                dimensions: std::collections::HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: format!("2025-06-01T1{}:00:00Z", i),
                model: None,
                source: "llm".into(),
                loop_iteration: 0,
            };
            let path = evals_dir.join(format!("eval-{}.json", i));
            fs::write(&path, serde_json::to_string(&eval).unwrap()).unwrap();
        }

        let config = AgencyConfig {
            auto_evolve: true,
            evolution_threshold: 10, // Threshold not met (only 5)
            evolution_interval: 7200,
            evolution_reactive_threshold: 0.4,
            ..AgencyConfig::default()
        };
        let state = EvolverState::default();

        let trigger = should_trigger_evolution(&agency_dir, &config, &state);
        assert!(trigger.is_some());
        match trigger.unwrap() {
            EvolutionTrigger::Reactive { avg_score } => {
                assert!(avg_score < 0.4);
            }
            other => panic!("Expected Reactive trigger, got {:?}", other),
        }
    }

    #[test]
    fn test_should_trigger_interval_not_met() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        let evals_dir = agency_dir.join("evaluations");
        fs::create_dir_all(&evals_dir).unwrap();

        // Create enough evaluations for threshold
        for i in 0..12 {
            let eval = crate::agency::Evaluation {
                id: format!("eval-{}", i),
                task_id: format!("task-{}", i),
                agent_id: String::new(),
                role_id: "role-1".into(),
                tradeoff_id: "tradeoff-1".into(),
                score: 0.7,
                dimensions: std::collections::HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: format!("2025-06-01T1{}:00:00Z", i),
                model: None,
                source: "llm".into(),
                loop_iteration: 0,
            };
            let path = evals_dir.join(format!("eval-{}.json", i));
            fs::write(&path, serde_json::to_string(&eval).unwrap()).unwrap();
        }

        let config = AgencyConfig {
            auto_evolve: true,
            evolution_threshold: 10,
            evolution_interval: 7200,
            ..AgencyConfig::default()
        };

        // State shows evolution happened just now
        let state = EvolverState {
            last_eval_count: 0,
            last_evolution_at: Some(Utc::now().to_rfc3339()),
            history: vec![],
            baselines: Default::default(),
        };

        // Should NOT trigger because interval not met (evolved just now, scores are fine)
        let trigger = should_trigger_evolution(&agency_dir, &config, &state);
        assert!(trigger.is_none());
    }

    #[test]
    fn test_safe_strategies() {
        assert!(SAFE_STRATEGIES.contains(&"mutation"));
        assert!(SAFE_STRATEGIES.contains(&"gap-analysis"));
        assert!(SAFE_STRATEGIES.contains(&"retirement"));
        assert!(SAFE_STRATEGIES.contains(&"motivation-tuning"));
        assert!(!SAFE_STRATEGIES.contains(&"crossover"));
        assert!(!SAFE_STRATEGIES.contains(&"bizarre-ideation"));
    }

    #[test]
    fn test_evolution_budget_cap() {
        let config = AgencyConfig {
            evolution_budget: 3,
            ..AgencyConfig::default()
        };
        assert_eq!(evolution_budget(&config), 3);

        let config_high = AgencyConfig {
            evolution_budget: 100,
            ..AgencyConfig::default()
        };
        assert_eq!(evolution_budget(&config_high), 5); // Capped at DEFAULT_MAX_OPS
    }

    #[test]
    fn test_count_evaluation_files() {
        let tmp = TempDir::new().unwrap();
        let evals_dir = tmp.path().join("evaluations");
        fs::create_dir_all(&evals_dir).unwrap();

        assert_eq!(count_evaluation_files(&evals_dir), 0);

        fs::write(evals_dir.join("eval-1.json"), "{}").unwrap();
        fs::write(evals_dir.join("eval-2.json"), "{}").unwrap();
        fs::write(evals_dir.join("not-eval.yaml"), "{}").unwrap();

        assert_eq!(count_evaluation_files(&evals_dir), 2);
    }

    #[test]
    fn test_compute_current_avg_score() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        let evals_dir = agency_dir.join("evaluations");
        fs::create_dir_all(&evals_dir).unwrap();

        // No evaluations
        assert!(compute_current_avg_score(&agency_dir).is_none());

        // Add some evaluations
        for (i, score) in [(0, 0.6), (1, 0.8), (2, 1.0)] {
            let eval = crate::agency::Evaluation {
                id: format!("eval-{}", i),
                task_id: format!("task-{}", i),
                agent_id: String::new(),
                role_id: "role-1".into(),
                tradeoff_id: "tradeoff-1".into(),
                score,
                dimensions: std::collections::HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: format!("2025-06-01T1{}:00:00Z", i),
                model: None,
                source: "llm".into(),
                loop_iteration: 0,
            };
            let path = evals_dir.join(format!("eval-{}.json", i));
            fs::write(&path, serde_json::to_string(&eval).unwrap()).unwrap();
        }

        let avg = compute_current_avg_score(&agency_dir).unwrap();
        assert!((avg - 0.8).abs() < f64::EPSILON);
    }
}
