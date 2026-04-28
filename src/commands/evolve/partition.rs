use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use workgraph::agency::{Evaluation, Role, TradeoffConfig};

use super::strategy::Strategy;

/// Maximum evaluations per analyzer slice (context budget guard).
const MAX_EVALS_PER_SLICE: usize = 400;

/// Model tier recommendation for an analyzer task.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Haiku,
    #[default]
    Sonnet,
    Opus,
}

impl ModelTier {
    pub fn label(self) -> &'static str {
        match self {
            Self::Haiku => "haiku",
            Self::Sonnet => "sonnet",
            Self::Opus => "opus",
        }
    }
}

/// A data slice prepared for a single analyzer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzerSlice {
    pub strategy: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub evaluations: Vec<Evaluation>,
    pub roles: Vec<Role>,
    pub tradeoffs: Vec<TradeoffConfig>,
    pub summary: String,
    #[serde(skip)]
    pub model_tier: ModelTier,
    pub stats: SliceStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceStats {
    pub total_evaluations_in_system: usize,
    pub evaluations_in_slice: usize,
    pub roles_in_slice: usize,
    pub tradeoffs_in_slice: usize,
    pub truncated: bool,
}

/// Partition evaluation data into per-strategy slices.
pub fn partition_evaluations(
    evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    _agency_dir: &Path,
    run_id: &str,
) -> Vec<(Strategy, AnalyzerSlice)> {
    let total_evals = evaluations.len();
    let strategies = Strategy::all_individual();
    let mut slices = Vec::new();

    for strategy in strategies {
        let slice = match strategy {
            Strategy::Mutation => {
                partition_mutation(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::Crossover => {
                partition_crossover(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::GapAnalysis => {
                partition_gap_analysis(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::Retirement => {
                partition_retirement(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::MotivationTuning => {
                partition_motivation_tuning(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::ComponentMutation => {
                partition_component_mutation(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::Randomisation => {
                partition_randomisation(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::BizarreIdeation => {
                partition_bizarre_ideation(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::CoordinatorEvolution => {
                partition_coordinator_evolution(evaluations, roles, tradeoffs, total_evals, run_id)
            }
            Strategy::All => unreachable!("all_individual excludes All"),
        };
        slices.push((strategy, slice));
    }

    slices
}

fn partition_mutation(
    evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // Roles with moderate scores (improvable, not hopeless)
    let target_role_ids: HashSet<&str> = roles
        .iter()
        .filter(|r| {
            r.performance.task_count >= 3
                && r.performance
                    .avg_score
                    .is_some_and(|s| (0.25..=0.70).contains(&s))
        })
        .map(|r| r.id.as_str())
        .collect();

    let mut filtered_evals: Vec<Evaluation> = evaluations
        .iter()
        .filter(|e| target_role_ids.contains(e.role_id.as_str()))
        .cloned()
        .collect();

    let truncated = filtered_evals.len() > MAX_EVALS_PER_SLICE;
    filtered_evals.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    filtered_evals.truncate(MAX_EVALS_PER_SLICE);

    let filtered_roles: Vec<Role> = roles
        .iter()
        .filter(|r| target_role_ids.contains(r.id.as_str()))
        .cloned()
        .collect();

    let summary = format!(
        "{} roles with moderate scores (0.25-0.70, ≥3 tasks), {} evaluations",
        filtered_roles.len(),
        filtered_evals.len()
    );

    AnalyzerSlice {
        strategy: "mutation".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: filtered_evals.clone(),
        roles: filtered_roles.clone(),
        tradeoffs: tradeoffs.to_vec(),
        summary,
        model_tier: ModelTier::Sonnet,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: filtered_evals.len(),
            roles_in_slice: filtered_roles.len(),
            tradeoffs_in_slice: tradeoffs.len(),
            truncated,
        },
    }
}

fn partition_crossover(
    evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // High-performing roles for crossover pairing
    let mut qualifying_roles: Vec<&Role> = roles
        .iter()
        .filter(|r| {
            r.performance.task_count >= 3 && r.performance.avg_score.is_some_and(|s| s >= 0.55)
        })
        .collect();
    qualifying_roles.sort_by(|a, b| {
        b.performance
            .avg_score
            .partial_cmp(&a.performance.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    qualifying_roles.truncate(20);

    let target_role_ids: HashSet<&str> = qualifying_roles.iter().map(|r| r.id.as_str()).collect();

    let mut filtered_evals: Vec<Evaluation> = evaluations
        .iter()
        .filter(|e| target_role_ids.contains(e.role_id.as_str()))
        .cloned()
        .collect();

    // Keep top 20 evals per role
    let truncated = filtered_evals.len() > MAX_EVALS_PER_SLICE;
    filtered_evals.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    filtered_evals.truncate(MAX_EVALS_PER_SLICE);

    let filtered_roles: Vec<Role> = qualifying_roles.into_iter().cloned().collect();

    let summary = format!(
        "{} high-performing roles (≥0.55 avg, ≥3 tasks), {} evaluations",
        filtered_roles.len(),
        filtered_evals.len()
    );

    AnalyzerSlice {
        strategy: "crossover".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: filtered_evals.clone(),
        roles: filtered_roles.clone(),
        tradeoffs: tradeoffs.to_vec(),
        summary,
        model_tier: ModelTier::Sonnet,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: filtered_evals.len(),
            roles_in_slice: filtered_roles.len(),
            tradeoffs_in_slice: tradeoffs.len(),
            truncated,
        },
    }
}

fn partition_gap_analysis(
    _evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // Gap analysis gets role summaries only, no raw evals
    let summary = format!(
        "{} roles total, {} tradeoffs. Analyze coverage gaps based on role descriptions and compositions.",
        roles.len(),
        tradeoffs.len()
    );

    AnalyzerSlice {
        strategy: "gap-analysis".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: vec![],
        roles: roles.to_vec(),
        tradeoffs: tradeoffs.to_vec(),
        summary,
        model_tier: ModelTier::Opus,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: 0,
            roles_in_slice: roles.len(),
            tradeoffs_in_slice: tradeoffs.len(),
            truncated: false,
        },
    }
}

fn partition_retirement(
    evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // Poor performers with sufficient signal
    let target_role_ids: HashSet<&str> = roles
        .iter()
        .filter(|r| {
            r.performance.task_count >= 5 && r.performance.avg_score.is_some_and(|s| s < 0.35)
        })
        .map(|r| r.id.as_str())
        .collect();

    let target_tradeoff_ids: HashSet<&str> = tradeoffs
        .iter()
        .filter(|t| {
            t.performance.task_count >= 5 && t.performance.avg_score.is_some_and(|s| s < 0.35)
        })
        .map(|t| t.id.as_str())
        .collect();

    let mut filtered_evals: Vec<Evaluation> = evaluations
        .iter()
        .filter(|e| {
            target_role_ids.contains(e.role_id.as_str())
                || target_tradeoff_ids.contains(e.tradeoff_id.as_str())
        })
        .cloned()
        .collect();

    let truncated = filtered_evals.len() > MAX_EVALS_PER_SLICE;
    // For retirement, keep lowest-scoring evals to show the pattern
    filtered_evals.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    filtered_evals.truncate(MAX_EVALS_PER_SLICE);

    let filtered_roles: Vec<Role> = roles
        .iter()
        .filter(|r| target_role_ids.contains(r.id.as_str()))
        .cloned()
        .collect();
    let filtered_tradeoffs: Vec<TradeoffConfig> = tradeoffs
        .iter()
        .filter(|t| target_tradeoff_ids.contains(t.id.as_str()))
        .cloned()
        .collect();

    let summary = format!(
        "{} low-performing roles (<0.35 avg, ≥5 tasks), {} low-performing tradeoffs, {} evaluations",
        filtered_roles.len(),
        filtered_tradeoffs.len(),
        filtered_evals.len()
    );

    AnalyzerSlice {
        strategy: "retirement".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: filtered_evals.clone(),
        roles: filtered_roles.clone(),
        tradeoffs: filtered_tradeoffs.clone(),
        summary,
        model_tier: ModelTier::Haiku,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: filtered_evals.len(),
            roles_in_slice: filtered_roles.len(),
            tradeoffs_in_slice: filtered_tradeoffs.len(),
            truncated,
        },
    }
}

fn partition_motivation_tuning(
    evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // All tradeoffs with sufficient data
    let mut qualifying_tradeoffs: Vec<&TradeoffConfig> = tradeoffs
        .iter()
        .filter(|t| t.performance.task_count >= 2)
        .collect();
    qualifying_tradeoffs.sort_by(|a, b| b.performance.task_count.cmp(&a.performance.task_count));
    qualifying_tradeoffs.truncate(30);

    let target_tradeoff_ids: HashSet<&str> =
        qualifying_tradeoffs.iter().map(|t| t.id.as_str()).collect();

    let mut filtered_evals: Vec<Evaluation> = evaluations
        .iter()
        .filter(|e| target_tradeoff_ids.contains(e.tradeoff_id.as_str()))
        .cloned()
        .collect();

    let truncated = filtered_evals.len() > MAX_EVALS_PER_SLICE;
    filtered_evals.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    filtered_evals.truncate(MAX_EVALS_PER_SLICE);

    let filtered_tradeoffs: Vec<TradeoffConfig> =
        qualifying_tradeoffs.into_iter().cloned().collect();

    let summary = format!(
        "{} tradeoffs with ≥2 tasks, {} evaluations",
        filtered_tradeoffs.len(),
        filtered_evals.len()
    );

    AnalyzerSlice {
        strategy: "motivation-tuning".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: filtered_evals.clone(),
        roles: roles.to_vec(),
        tradeoffs: filtered_tradeoffs.clone(),
        summary,
        model_tier: ModelTier::Sonnet,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: filtered_evals.len(),
            roles_in_slice: roles.len(),
            tradeoffs_in_slice: filtered_tradeoffs.len(),
            truncated,
        },
    }
}

fn partition_component_mutation(
    evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // Roles with components and sufficient eval data
    let target_role_ids: HashSet<&str> = roles
        .iter()
        .filter(|r| !r.component_ids.is_empty() && r.performance.task_count >= 2)
        .map(|r| r.id.as_str())
        .collect();

    let mut filtered_evals: Vec<Evaluation> = evaluations
        .iter()
        .filter(|e| target_role_ids.contains(e.role_id.as_str()))
        .cloned()
        .collect();

    let truncated = filtered_evals.len() > MAX_EVALS_PER_SLICE;
    filtered_evals.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    filtered_evals.truncate(MAX_EVALS_PER_SLICE);

    let filtered_roles: Vec<Role> = roles
        .iter()
        .filter(|r| target_role_ids.contains(r.id.as_str()))
        .cloned()
        .collect();

    let summary = format!(
        "{} roles with components and ≥2 tasks, {} evaluations",
        filtered_roles.len(),
        filtered_evals.len()
    );

    AnalyzerSlice {
        strategy: "component-mutation".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: filtered_evals.clone(),
        roles: filtered_roles.clone(),
        tradeoffs: tradeoffs.to_vec(),
        summary,
        model_tier: ModelTier::Sonnet,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: filtered_evals.len(),
            roles_in_slice: filtered_roles.len(),
            tradeoffs_in_slice: tradeoffs.len(),
            truncated,
        },
    }
}

fn partition_randomisation(
    _evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // Randomisation just needs the inventory, not eval data
    let summary = format!(
        "Inventory: {} roles, {} tradeoffs. Propose random compositions from existing primitives.",
        roles.len(),
        tradeoffs.len()
    );

    AnalyzerSlice {
        strategy: "randomisation".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: vec![],
        roles: roles.to_vec(),
        tradeoffs: tradeoffs.to_vec(),
        summary,
        model_tier: ModelTier::Haiku,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: 0,
            roles_in_slice: roles.len(),
            tradeoffs_in_slice: tradeoffs.len(),
            truncated: false,
        },
    }
}

fn partition_bizarre_ideation(
    evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // Minimal context — just enough to inspire divergent thinking
    // Include 5 highest and 5 lowest scoring roles
    let mut scored_roles: Vec<&Role> = roles
        .iter()
        .filter(|r| r.performance.avg_score.is_some())
        .collect();
    scored_roles.sort_by(|a, b| {
        b.performance
            .avg_score
            .partial_cmp(&a.performance.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut context_roles: Vec<Role> = Vec::new();
    // Top 5
    for r in scored_roles.iter().take(5) {
        context_roles.push((*r).clone());
    }
    // Bottom 5
    for r in scored_roles.iter().rev().take(5) {
        if !context_roles.iter().any(|cr| cr.id == r.id) {
            context_roles.push((*r).clone());
        }
    }

    // No raw eval data — scores constrain creativity
    let _ = evaluations;

    let summary = format!(
        "Context: {} roles (top 5 + bottom 5 by score), {} tradeoffs. Generate novel, unconventional primitives.",
        context_roles.len(),
        tradeoffs.len()
    );

    AnalyzerSlice {
        strategy: "bizarre-ideation".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: vec![],
        roles: context_roles.clone(),
        tradeoffs: tradeoffs.to_vec(),
        summary,
        model_tier: ModelTier::Opus,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: 0,
            roles_in_slice: context_roles.len(),
            tradeoffs_in_slice: tradeoffs.len(),
            truncated: false,
        },
    }
}

fn partition_coordinator_evolution(
    evaluations: &[Evaluation],
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    total_evals: usize,
    run_id: &str,
) -> AnalyzerSlice {
    // Coordinator evolution gets all roles/tradeoffs (to understand the landscape)
    // but no raw evaluations — it works from aggregate patterns and coordinator prompt files.
    // Evaluations with coordinator-specific dimensions are included if present.
    let mut filtered_evals: Vec<Evaluation> = evaluations
        .iter()
        .filter(|e| {
            e.dimensions.keys().any(|k| {
                k.starts_with("coord")
                    || k.starts_with("decomposition")
                    || k.starts_with("dispatch")
            })
        })
        .cloned()
        .collect();

    let truncated = filtered_evals.len() > MAX_EVALS_PER_SLICE;
    filtered_evals.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    filtered_evals.truncate(MAX_EVALS_PER_SLICE);

    let summary = format!(
        "{} coordinator-relevant evaluations, {} roles, {} tradeoffs. Analyze coordinator behavior patterns.",
        filtered_evals.len(),
        roles.len(),
        tradeoffs.len()
    );

    AnalyzerSlice {
        strategy: "coordinator".to_string(),
        run_id: run_id.to_string(),
        timestamp: Some(chrono::Utc::now().to_rfc3339()),
        evaluations: filtered_evals.clone(),
        roles: roles.to_vec(),
        tradeoffs: tradeoffs.to_vec(),
        summary,
        model_tier: ModelTier::Sonnet,
        stats: SliceStats {
            total_evaluations_in_system: total_evals,
            evaluations_in_slice: filtered_evals.len(),
            roles_in_slice: roles.len(),
            tradeoffs_in_slice: tradeoffs.len(),
            truncated,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use workgraph::agency::{AccessControl, Lineage, PerformanceRecord};

    fn make_role(id: &str, avg_score: Option<f64>, task_count: u32) -> Role {
        Role {
            id: id.to_string(),
            name: format!("Role {}", id),
            description: "test role".to_string(),
            component_ids: vec!["comp1".to_string()],
            outcome_id: "outcome1".to_string(),
            performance: PerformanceRecord {
                task_count,
                avg_score,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        }
    }

    fn make_tradeoff(id: &str, avg_score: Option<f64>, task_count: u32) -> TradeoffConfig {
        TradeoffConfig {
            id: id.to_string(),
            name: format!("Tradeoff {}", id),
            description: "test tradeoff".to_string(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord {
                task_count,
                avg_score,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            domain_tags: vec![],
            metadata: HashMap::new(),
            former_agents: vec![],
            former_deployments: vec![],
        }
    }

    fn make_eval(id: &str, role_id: &str, tradeoff_id: &str, score: f64) -> Evaluation {
        Evaluation {
            id: id.to_string(),
            task_id: format!("task-{}", id),
            agent_id: String::new(),
            role_id: role_id.to_string(),
            tradeoff_id: tradeoff_id.to_string(),
            score,
            dimensions: HashMap::new(),
            notes: String::new(),
            evaluator: "test".to_string(),
            timestamp: "2026-03-13T12:00:00Z".to_string(),
            model: None,
            source: "llm".to_string(),
            loop_iteration: 0,
        }
    }

    #[test]
    fn test_partition_mutation_filters_moderate_scores() {
        let roles = vec![
            make_role("r1", Some(0.50), 5), // qualifies
            make_role("r2", Some(0.10), 5), // too low
            make_role("r3", Some(0.80), 5), // too high
            make_role("r4", Some(0.50), 1), // too few tasks
        ];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.5), 5)];
        let evals = vec![
            make_eval("e1", "r1", "t1", 0.5),
            make_eval("e2", "r2", "t1", 0.1),
            make_eval("e3", "r3", "t1", 0.8),
        ];

        let slices =
            partition_evaluations(&evals, &roles, &tradeoffs, Path::new("/tmp"), "test-run");
        let mutation = slices
            .iter()
            .find(|(s, _)| *s == Strategy::Mutation)
            .unwrap();
        assert_eq!(mutation.1.stats.evaluations_in_slice, 1);
        assert_eq!(mutation.1.stats.roles_in_slice, 1);
    }

    #[test]
    fn test_partition_retirement_filters_poor_performers() {
        let roles = vec![
            make_role("r1", Some(0.30), 6), // qualifies
            make_role("r2", Some(0.50), 6), // too high
        ];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.25), 6)]; // qualifies
        let evals = vec![
            make_eval("e1", "r1", "t1", 0.3),
            make_eval("e2", "r2", "t1", 0.5),
        ];

        let slices =
            partition_evaluations(&evals, &roles, &tradeoffs, Path::new("/tmp"), "test-run");
        let retirement = slices
            .iter()
            .find(|(s, _)| *s == Strategy::Retirement)
            .unwrap();
        assert_eq!(retirement.1.stats.roles_in_slice, 1);
        assert_eq!(retirement.1.stats.tradeoffs_in_slice, 1);
    }

    #[test]
    fn test_partition_gap_analysis_sends_no_evals() {
        let roles = vec![make_role("r1", Some(0.5), 5)];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.5), 5)];
        let evals = vec![make_eval("e1", "r1", "t1", 0.5)];

        let slices =
            partition_evaluations(&evals, &roles, &tradeoffs, Path::new("/tmp"), "test-run");
        let gap = slices
            .iter()
            .find(|(s, _)| *s == Strategy::GapAnalysis)
            .unwrap();
        assert_eq!(gap.1.stats.evaluations_in_slice, 0);
        // But gets all roles for coverage analysis
        assert_eq!(gap.1.stats.roles_in_slice, 1);
    }

    #[test]
    fn test_all_strategies_get_slices() {
        let roles = vec![make_role("r1", Some(0.5), 5)];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.5), 5)];
        let evals = vec![make_eval("e1", "r1", "t1", 0.5)];

        let slices =
            partition_evaluations(&evals, &roles, &tradeoffs, Path::new("/tmp"), "test-run");
        assert_eq!(slices.len(), 9); // all individual strategies
    }
}
