//! Run mode continuum: assignment routing, UCB1 primitive selection,
//! novelty bonus, and retrospective inference.
//!
//! Implements the performance/learning continuum described in the run-modes
//! design document. Controls how task assignments are routed between
//! cache-first performance mode and structured learning experiments.

use std::collections::HashMap;
use std::path::Path;

use crate::config::AgencyConfig;

use super::store::*;
use super::types::*;

// ---------------------------------------------------------------------------
// Assignment routing
// ---------------------------------------------------------------------------

/// Which path a single assignment should take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AssignmentPath {
    /// Use cached agent (performance mode).
    Performance,
    /// Run a structured learning experiment.
    Learning,
    /// Forced exploration episode (exploration_interval trigger).
    ForcedExploration,
}

/// Determine the assignment path for a given task.
///
/// `task_count` is the total number of assignments made so far (used for
/// exploration_interval checks). `rng_value` is a uniform random number
/// in [0, 1) for probabilistic routing.
pub fn determine_assignment_path(
    config: &AgencyConfig,
    task_count: u32,
    rng_value: f64,
) -> AssignmentPath {
    // Forced exploration takes precedence
    if config.exploration_interval > 0
        && task_count > 0
        && task_count % config.exploration_interval == 0
    {
        return AssignmentPath::ForcedExploration;
    }

    let effective_rate = config.run_mode.max(config.min_exploration_rate);

    if rng_value < effective_rate {
        AssignmentPath::Learning
    } else {
        AssignmentPath::Performance
    }
}

// ---------------------------------------------------------------------------
// UCB1 primitive selection
// ---------------------------------------------------------------------------

/// Compute the UCB1 score for a primitive.
///
/// `avg_score`: average evaluation score for this primitive (None if never evaluated).
/// `eval_count`: number of evaluations for this primitive.
/// `total_assignments`: total number of assignments across all primitives.
/// `exploration_constant`: the C parameter (default √2).
/// `attractor_weight`: the primitive's attractor weight (0..1, higher = more conventional).
/// `novelty_bonus_multiplier`: multiplier for low-attractor primitives.
pub fn ucb1_score(
    avg_score: Option<f64>,
    eval_count: u32,
    total_assignments: u32,
    exploration_constant: f64,
    attractor_weight: f64,
    novelty_bonus_multiplier: f64,
) -> f64 {
    let base_score = avg_score.unwrap_or(0.5); // Optimistic prior for unscored primitives
    let n = total_assignments.max(1) as f64;
    let ni = eval_count.max(1) as f64;

    let exploration_bonus = exploration_constant * (n.ln() / ni).sqrt();

    // Novelty bonus: inversely proportional to attractor weight.
    // Low-attractor primitives get boosted; high-attractor ones stay at 1.0.
    let novelty_factor = if attractor_weight < 0.5 {
        novelty_bonus_multiplier
    } else {
        1.0
    };

    (base_score + exploration_bonus) * novelty_factor
}

/// Select a primitive from candidates using UCB1 scoring with novelty bonus.
///
/// Returns (selected_id, ucb_scores) where ucb_scores maps each candidate
/// to its UCB1 score for post-hoc analysis.
pub fn select_primitive_ucb1(
    candidates: &[(String, Option<f64>, u32, f64)], // (id, avg_score, eval_count, attractor_weight)
    total_assignments: u32,
    exploration_constant: f64,
    novelty_bonus_multiplier: f64,
) -> Option<(String, HashMap<String, f64>)> {
    if candidates.is_empty() {
        return None;
    }

    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut best_id = &candidates[0].0;
    let mut best_score = f64::NEG_INFINITY;

    for (id, avg, eval_count, attractor_weight) in candidates {
        let score = ucb1_score(
            *avg,
            *eval_count,
            total_assignments,
            exploration_constant,
            *attractor_weight,
            novelty_bonus_multiplier,
        );
        scores.insert(id.clone(), score);
        if score > best_score {
            best_score = score;
            best_id = id;
        }
    }

    Some((best_id.clone(), scores))
}

// ---------------------------------------------------------------------------
// Experiment design
// ---------------------------------------------------------------------------

/// Design a learning experiment given the agency state.
///
/// Implements the algorithm from the design doc §4.2:
/// 1. Find best known composition for this task type.
/// 2. Select dimension with highest uncertainty.
/// 3. Pick variant via UCB1.
/// 4. Construct the experiment.
pub fn design_experiment(
    agency_dir: &Path,
    config: &AgencyConfig,
    learning_assignment_count: u32,
) -> AssignmentExperiment {
    // Check bizarre ideation schedule
    if config.bizarre_ideation_interval > 0
        && learning_assignment_count > 0
        && learning_assignment_count % config.bizarre_ideation_interval == 0
    {
        return AssignmentExperiment {
            base_composition: None,
            dimension: ExperimentDimension::NovelComposition,
            bizarre_ideation: true,
            ucb_scores: HashMap::new(),
        };
    }

    let agents_dir = agency_dir.join("cache/agents");
    let components_dir = agency_dir.join("primitives/components");

    // Load agents to find best known composition
    let agents = load_all_agents_or_warn(&agents_dir);
    let best_agent = agents
        .iter()
        .filter(|a| a.performance.avg_score.is_some())
        .max_by(|a, b| {
            a.performance
                .avg_score
                .unwrap_or(0.0)
                .partial_cmp(&b.performance.avg_score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

    let best_agent = match best_agent {
        Some(a) => a,
        None => {
            // No evaluated compositions exist — do novel composition
            return AssignmentExperiment {
                base_composition: None,
                dimension: ExperimentDimension::NovelComposition,
                bizarre_ideation: false,
                ucb_scores: HashMap::new(),
            };
        }
    };

    // Load components to build UCB1 candidate list
    let components = match load_all_components(&components_dir) {
        Ok(c) => c,
        Err(_) => {
            return AssignmentExperiment {
                base_composition: Some(best_agent.id.clone()),
                dimension: ExperimentDimension::NovelComposition,
                bizarre_ideation: false,
                ucb_scores: HashMap::new(),
            };
        }
    };

    // Load the role to get the base component list
    let roles_dir = agency_dir.join("cache/roles");
    let base_role = find_role_by_prefix(&roles_dir, &best_agent.role_id).ok();

    let base_component_ids: Vec<String> = base_role
        .as_ref()
        .map(|r| r.component_ids.clone())
        .unwrap_or_default();

    // Build candidate list of components NOT in the base composition
    let total_assignments = count_assignment_records(&agency_dir.join("assignments"));
    let candidates: Vec<(String, Option<f64>, u32, f64)> = components
        .iter()
        .filter(|c| !base_component_ids.contains(&c.id))
        .map(|c| {
            (
                c.id.clone(),
                c.performance.avg_score,
                c.performance.task_count,
                // Use default attractor weight based on former deployments
                if c.former_deployments.is_empty() {
                    0.1 // Low weight for never-deployed components
                } else {
                    0.5 // Default weight
                },
            )
        })
        .collect();

    if candidates.is_empty() {
        return AssignmentExperiment {
            base_composition: Some(best_agent.id.clone()),
            dimension: ExperimentDimension::NovelComposition,
            bizarre_ideation: false,
            ucb_scores: HashMap::new(),
        };
    }

    // Select variant component via UCB1
    let (selected_id, ucb_scores) = select_primitive_ucb1(
        &candidates,
        total_assignments as u32,
        config.ucb_exploration_constant,
        config.novelty_bonus_multiplier,
    )
    .unwrap();

    // Pick a random base component to replace (prefer least-evaluated)
    let replaced = base_component_ids
        .iter()
        .filter_map(|id| {
            let comp = components.iter().find(|c| c.id == *id)?;
            Some((id.clone(), comp.performance.task_count))
        })
        .min_by_key(|(_, count)| *count)
        .map(|(id, _)| id);

    AssignmentExperiment {
        base_composition: Some(best_agent.id.clone()),
        dimension: ExperimentDimension::RoleComponent {
            replaced,
            introduced: selected_id,
        },
        bizarre_ideation: false,
        ucb_scores,
    }
}

// ---------------------------------------------------------------------------
// Performance mode: cache lookup
// ---------------------------------------------------------------------------

/// Find the best cached agent for a task.
///
/// Returns (agent, score) if a suitable agent is found above threshold.
pub fn find_cached_agent(
    agency_dir: &Path,
    threshold: f64,
) -> Option<(Agent, f64)> {
    let agents_dir = agency_dir.join("cache/agents");
    let agents = load_all_agents_or_warn(&agents_dir);

    agents
        .into_iter()
        .filter_map(|a| {
            let score = a.performance.avg_score?;
            if score >= threshold && a.staleness_flags.is_empty() {
                Some((a, score))
            } else {
                None
            }
        })
        .max_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

// ---------------------------------------------------------------------------
// Retrospective inference
// ---------------------------------------------------------------------------

/// Process retrospective inference when an evaluation arrives for a task
/// that was assigned in learning mode.
///
/// Steps from design doc §6:
/// 1. Load TaskAssignmentRecord.
/// 2. If learning/forced: extract experiment, propagate score.
/// 3. Update attractor weights.
/// 4. Populate cache if above threshold.
pub fn process_retrospective_inference(
    agency_dir: &Path,
    task_id: &str,
    eval_score: f64,
    config: &AgencyConfig,
) -> Result<(), AgencyError> {
    let assignments_dir = agency_dir.join("assignments");
    let record = match load_assignment_record_by_task(&assignments_dir, task_id) {
        Ok(r) => r,
        Err(AgencyError::NotFound(_)) => return Ok(()), // No assignment record — not a learning task
        Err(e) => return Err(e),
    };

    let experiment = match &record.mode {
        AssignmentMode::Learning(exp) | AssignmentMode::ForcedExploration(exp) => exp.clone(),
        AssignmentMode::CacheHit { .. } | AssignmentMode::CacheMiss => return Ok(()),
    };

    let components_dir = agency_dir.join("primitives/components");
    let agents_dir = agency_dir.join("cache/agents");

    match &experiment.dimension {
        ExperimentDimension::RoleComponent { introduced, .. }
        | ExperimentDimension::TradeoffConfig { introduced, .. } => {
            // Propagate score to the introduced primitive
            let component_path = components_dir.join(format!("{}.yaml", introduced));
            if component_path.exists() {
                if let Ok(mut component) = load_component(&component_path) {
                    let eval_ref = EvaluationRef {
                        score: eval_score,
                        task_id: task_id.to_string(),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        context_id: format!("experiment:{}", record.composition_id),
                    };
                    super::eval::update_performance(&mut component.performance, eval_ref);
                    let _ = save_component(&component, &components_dir);
                }
            }

            // Update attractor weight on agent
            if let Some(base_id) = &experiment.base_composition {
                let agent_path = agents_dir.join(format!("{}.yaml", base_id));
                if agent_path.exists() {
                    if let Ok(agent) = load_agent(&agent_path) {
                        let base_avg = agent.performance.avg_score.unwrap_or(0.5);
                        // Adjust attractor weights on the agent
                        // If experiment score > base avg, increase weight; otherwise decrease
                        let mut updated_agent = agent;
                        let learning_rate = 0.1;
                        if eval_score > base_avg {
                            updated_agent.attractor_weight =
                                (updated_agent.attractor_weight + learning_rate).min(1.0);
                        } else {
                            updated_agent.attractor_weight =
                                (updated_agent.attractor_weight - learning_rate).max(0.0);
                        }
                        let _ = save_agent(&updated_agent, &agents_dir);
                    }
                }
            }
        }
        ExperimentDimension::NovelComposition => {
            // Propagate score equally to all component primitives of the assigned agent
            let agent_path = agents_dir.join(format!("{}.yaml", record.agent_id));
            if let Ok(agent) = load_agent(&agent_path) {
                let roles_dir = agency_dir.join("cache/roles");
                if let Ok(role) = find_role_by_prefix(&roles_dir, &agent.role_id) {
                    for comp_id in &role.component_ids {
                        let comp_path = components_dir.join(format!("{}.yaml", comp_id));
                        if comp_path.exists() {
                            if let Ok(mut comp) = load_component(&comp_path) {
                                let eval_ref = EvaluationRef {
                                    score: eval_score,
                                    task_id: task_id.to_string(),
                                    timestamp: chrono::Utc::now().to_rfc3339(),
                                    context_id: format!("experiment:novel:{}", record.composition_id),
                                };
                                super::eval::update_performance(&mut comp.performance, eval_ref);
                                let _ = save_component(&comp, &components_dir);
                            }
                        }
                    }
                }
            }
        }
    }

    // Cache population: if score >= threshold, ensure this composition is in the cache
    if eval_score >= config.cache_population_threshold {
        // The agent already exists in the cache by definition (it was deployed),
        // but update its performance to reflect this high score
        let agent_path = agents_dir.join(format!("{}.yaml", record.agent_id));
        if agent_path.exists() {
            if let Ok(mut agent) = load_agent(&agent_path) {
                let eval_ref = EvaluationRef {
                    score: eval_score,
                    task_id: task_id.to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    context_id: "experiment:cache-population".to_string(),
                };
                super::eval::update_performance(&mut agent.performance, eval_ref);
                let _ = save_agent(&agent, &agents_dir);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config() -> AgencyConfig {
        AgencyConfig {
            run_mode: 0.2,
            min_exploration_rate: 0.05,
            exploration_interval: 20,
            cache_population_threshold: 0.8,
            ucb_exploration_constant: std::f64::consts::SQRT_2,
            novelty_bonus_multiplier: 1.5,
            bizarre_ideation_interval: 10,
            performance_threshold: 0.7,
            ..AgencyConfig::default()
        }
    }

    // -- Assignment routing tests --

    #[test]
    fn test_pure_performance_mode() {
        let mut config = test_config();
        config.run_mode = 0.0;
        config.min_exploration_rate = 0.0;
        config.exploration_interval = 0;

        // Every assignment should be Performance
        for i in 0..100 {
            assert_eq!(
                determine_assignment_path(&config, i, 0.5),
                AssignmentPath::Performance,
            );
        }
    }

    #[test]
    fn test_pure_learning_mode() {
        let mut config = test_config();
        config.run_mode = 1.0;
        config.exploration_interval = 0;

        // Every assignment should be Learning (rng always < 1.0)
        for i in 0..100 {
            assert_eq!(
                determine_assignment_path(&config, i, 0.99),
                AssignmentPath::Learning,
            );
        }
    }

    #[test]
    fn test_min_exploration_rate() {
        let mut config = test_config();
        config.run_mode = 0.0;
        config.min_exploration_rate = 0.05;
        config.exploration_interval = 0;

        // rng < 0.05 should trigger Learning
        assert_eq!(
            determine_assignment_path(&config, 1, 0.01),
            AssignmentPath::Learning,
        );
        // rng >= 0.05 should be Performance
        assert_eq!(
            determine_assignment_path(&config, 1, 0.06),
            AssignmentPath::Performance,
        );
    }

    #[test]
    fn test_forced_exploration_interval() {
        let mut config = test_config();
        config.run_mode = 0.0;
        config.min_exploration_rate = 0.0;
        config.exploration_interval = 10;

        // task_count=10: forced
        assert_eq!(
            determine_assignment_path(&config, 10, 0.99),
            AssignmentPath::ForcedExploration,
        );
        // task_count=20: forced
        assert_eq!(
            determine_assignment_path(&config, 20, 0.99),
            AssignmentPath::ForcedExploration,
        );
        // task_count=11: not forced
        assert_eq!(
            determine_assignment_path(&config, 11, 0.99),
            AssignmentPath::Performance,
        );
        // task_count=0: not forced (avoid triggering on first task)
        assert_eq!(
            determine_assignment_path(&config, 0, 0.99),
            AssignmentPath::Performance,
        );
    }

    #[test]
    fn test_forced_exploration_overrides_performance() {
        let mut config = test_config();
        config.run_mode = 0.0;
        config.min_exploration_rate = 0.0;
        config.exploration_interval = 5;

        // Even with rng_value = 0.99, forced exploration fires at task 5
        assert_eq!(
            determine_assignment_path(&config, 5, 0.99),
            AssignmentPath::ForcedExploration,
        );
    }

    // -- UCB1 tests --

    #[test]
    fn test_ucb1_unscored_gets_optimistic_prior() {
        let score = ucb1_score(None, 0, 100, std::f64::consts::SQRT_2, 0.5, 1.5);
        // Should be > 0.5 due to exploration bonus
        assert!(score > 0.5);
    }

    #[test]
    fn test_ucb1_high_score_low_count_wins() {
        let high_count = ucb1_score(Some(0.8), 50, 100, std::f64::consts::SQRT_2, 0.5, 1.0);
        let low_count = ucb1_score(Some(0.8), 2, 100, std::f64::consts::SQRT_2, 0.5, 1.0);
        // Low count should have higher UCB score due to exploration bonus
        assert!(low_count > high_count);
    }

    #[test]
    fn test_ucb1_novelty_bonus_for_low_attractor() {
        let high_attractor = ucb1_score(Some(0.5), 10, 100, std::f64::consts::SQRT_2, 0.8, 1.5);
        let low_attractor = ucb1_score(Some(0.5), 10, 100, std::f64::consts::SQRT_2, 0.2, 1.5);
        // Low attractor weight should get novelty multiplier
        assert!(low_attractor > high_attractor);
    }

    #[test]
    fn test_select_primitive_empty() {
        let result = select_primitive_ucb1(&[], 100, std::f64::consts::SQRT_2, 1.5);
        assert!(result.is_none());
    }

    #[test]
    fn test_select_primitive_single_candidate() {
        let candidates = vec![("comp-1".to_string(), Some(0.8), 5, 0.5)];
        let (selected, scores) = select_primitive_ucb1(&candidates, 100, std::f64::consts::SQRT_2, 1.5).unwrap();
        assert_eq!(selected, "comp-1");
        assert!(scores.contains_key("comp-1"));
    }

    #[test]
    fn test_select_primitive_prefers_under_explored() {
        let candidates = vec![
            ("well-explored".to_string(), Some(0.7), 50, 0.5),
            ("under-explored".to_string(), Some(0.7), 1, 0.5),
        ];
        let (selected, _) = select_primitive_ucb1(&candidates, 100, std::f64::consts::SQRT_2, 1.5).unwrap();
        // Under-explored should win due to high exploration bonus
        assert_eq!(selected, "under-explored");
    }

    // -- Experiment design tests --

    #[test]
    fn test_design_experiment_no_agents() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let config = test_config();
        let exp = design_experiment(&agency_dir, &config, 1);
        assert!(matches!(exp.dimension, ExperimentDimension::NovelComposition));
        assert!(!exp.bizarre_ideation);
    }

    #[test]
    fn test_design_experiment_bizarre_ideation() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let config = test_config();
        // learning_assignment_count = 10, bizarre_ideation_interval = 10
        let exp = design_experiment(&agency_dir, &config, 10);
        assert!(matches!(exp.dimension, ExperimentDimension::NovelComposition));
        assert!(exp.bizarre_ideation);
    }

    // -- Retrospective inference tests --

    #[test]
    fn test_retrospective_no_record_is_noop() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let config = test_config();
        let result = process_retrospective_inference(&agency_dir, "nonexistent-task", 0.9, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_retrospective_cache_hit_is_noop() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let record = TaskAssignmentRecord {
            task_id: "task-1".to_string(),
            agent_id: "agent-abc".to_string(),
            composition_id: "comp-xyz".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            run_mode_value: 0.0,
            mode: AssignmentMode::CacheHit { cache_score: 0.9 },
        };
        save_assignment_record(&record, &agency_dir.join("assignments")).unwrap();

        let config = test_config();
        let result = process_retrospective_inference(&agency_dir, "task-1", 0.9, &config);
        assert!(result.is_ok());
    }

    // -- Find cached agent tests --

    #[test]
    fn test_find_cached_agent_none() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let result = find_cached_agent(&agency_dir, 0.7);
        assert!(result.is_none());
    }
}
