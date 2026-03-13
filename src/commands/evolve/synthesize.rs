use std::collections::HashMap;

use super::strategy::{EvolverOperation, EvolverOutput};
use super::strategy::Strategy;

/// Conflict resolution priority (higher wins).
/// Retirement > GapAnalysis > Mutation > Crossover > BizarreIdeation
fn strategy_priority(strategy: Strategy) -> u8 {
    match strategy {
        Strategy::Retirement => 10,
        Strategy::GapAnalysis => 8,
        Strategy::Mutation | Strategy::ComponentMutation | Strategy::MotivationTuning => 6,
        Strategy::Crossover => 4,
        Strategy::CoordinatorEvolution => 3,
        Strategy::Randomisation => 2,
        Strategy::BizarreIdeation => 1,
        Strategy::All => 0,
    }
}

/// A proposal tagged with its originating strategy.
#[derive(Debug, Clone)]
struct TaggedOp {
    strategy: Strategy,
    op: EvolverOperation,
}

/// Key that identifies the "target" of an operation for conflict/dedup detection.
/// Operations with the same fingerprint are either duplicates or conflicts.
fn op_fingerprint(op: &EvolverOperation) -> String {
    // Primary key is (op_type, target_id).
    // For create operations, use the new_id instead.
    let target = match op.op.as_str() {
        "create_role" | "create_motivation" | "bizarre_ideation" => op
            .new_id
            .as_deref()
            .or(op.new_name.as_deref())
            .unwrap_or("unknown")
            .to_string(),
        _ => op.target_id.as_deref().unwrap_or("none").to_string(),
    };
    format!("{}::{}", op.op, target)
}

/// Key that identifies the target entity for conflict detection.
/// Different op types on the same entity key may conflict.
fn target_entity_key(op: &EvolverOperation) -> String {
    match op.op.as_str() {
        "create_role" | "create_motivation" | "bizarre_ideation" => {
            // Creates target new entities — use new_id/new_name as key
            // These won't conflict with existing-entity operations
            let id = op
                .new_id
                .as_deref()
                .or(op.new_name.as_deref())
                .unwrap_or("unknown");
            format!("new::{}", id)
        }
        "random_compose_role" | "random_compose_agent" => {
            // Random compositions create new entities
            format!("compose::{}", op.role_id.as_deref().unwrap_or("unknown"))
        }
        "modify_coordinator_prompt" => {
            format!("coordinator::{}", op.target_id.as_deref().unwrap_or("unknown"))
        }
        _ => {
            // Modifications and retirements target existing entities
            op.target_id.as_deref().unwrap_or("none").to_string()
        }
    }
}

/// Determines if two operations conflict (mutually exclusive) rather than
/// being simple duplicates. Conflicts are resolved by strategy priority.
fn ops_conflict(a: &EvolverOperation, b: &EvolverOperation) -> bool {
    let a_target = a.target_id.as_deref().unwrap_or("");
    let b_target = b.target_id.as_deref().unwrap_or("");

    if a_target.is_empty() || b_target.is_empty() {
        return false;
    }

    // A retire + modify/mutate on the same entity is a conflict
    let a_retires = a.op.starts_with("retire");
    let b_retires = b.op.starts_with("retire");
    let a_modifies = a.op.starts_with("modify") || a.op.contains("mutation") || a.op.contains("substitution") || a.op.contains("swap") || a.op.contains("add_component") || a.op.contains("remove_component");
    let b_modifies = b.op.starts_with("modify") || b.op.contains("mutation") || b.op.contains("substitution") || b.op.contains("swap") || b.op.contains("add_component") || b.op.contains("remove_component");

    // Retire vs modify on same target
    if a_target == b_target && ((a_retires && b_modifies) || (a_modifies && b_retires)) {
        return true;
    }

    false
}

/// Synthesize multiple per-strategy EvolverOutputs into a single merged result.
///
/// - Deduplicates identical operations across strategies
/// - Resolves conflicts using strategy priority
/// - Applies budget cap
/// - Handles partial failures (missing strategies are skipped)
/// - Tracks provenance (which strategy proposed what)
pub fn synthesize(
    inputs: Vec<(Strategy, EvolverOutput)>,
    budget: Option<u32>,
) -> EvolverOutput {
    if inputs.is_empty() {
        return EvolverOutput {
            run_id: None,
            target: None,
            operations: vec![],
            deferred_operations: vec![],
            summary: Some("No strategy outputs to synthesize.".to_string()),
        };
    }

    // Grab run_id from first input that has one
    let run_id = inputs
        .iter()
        .find_map(|(_, out)| out.run_id.clone());

    // Collect all operations tagged with their source strategy
    let mut all_tagged: Vec<TaggedOp> = Vec::new();
    let mut all_deferred: Vec<EvolverOperation> = Vec::new();
    let mut strategies_represented: Vec<String> = Vec::new();
    let mut total_proposed: usize = 0;
    let mut conflict_log: Vec<String> = Vec::new();

    for (strategy, output) in &inputs {
        strategies_represented.push(strategy.label().to_string());
        total_proposed += output.operations.len();

        for op in &output.operations {
            let mut tagged_op = op.clone();
            // Stamp provenance: record which strategy proposed this
            if tagged_op.rationale.is_none() {
                tagged_op.rationale = Some(format!("Proposed by {} strategy", strategy.label()));
            } else {
                let existing = tagged_op.rationale.as_ref().unwrap().clone();
                tagged_op.rationale =
                    Some(format!("{} [source: {}]", existing, strategy.label()));
            }
            all_tagged.push(TaggedOp {
                strategy: *strategy,
                op: tagged_op,
            });
        }

        // Collect deferred operations as-is
        for op in &output.deferred_operations {
            all_deferred.push(op.clone());
        }
    }

    // Phase 1: Dedup exact duplicates (same op type + same target from multiple strategies)
    // Group by fingerprint (op_type::target)
    let mut by_fingerprint: HashMap<String, Vec<TaggedOp>> = HashMap::new();
    for tagged in all_tagged {
        let fp = op_fingerprint(&tagged.op);
        by_fingerprint.entry(fp).or_default().push(tagged);
    }

    // For each fingerprint group, keep the highest-priority/confidence one
    let mut deduped: Vec<TaggedOp> = Vec::new();
    for (_fp, mut group) in by_fingerprint {
        group.sort_by(|a, b| {
            let pri_cmp = strategy_priority(b.strategy).cmp(&strategy_priority(a.strategy));
            if pri_cmp != std::cmp::Ordering::Equal {
                return pri_cmp;
            }
            let conf_a = a.op.confidence.unwrap_or(0.0);
            let conf_b = b.op.confidence.unwrap_or(0.0);
            conf_b
                .partial_cmp(&conf_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        deduped.push(group.remove(0));
    }

    // Phase 2: Detect cross-op conflicts on the same target entity
    // Group by target entity (ignoring op type) to find conflicts
    let mut by_target: HashMap<String, Vec<TaggedOp>> = HashMap::new();
    for tagged in deduped {
        let target_key = target_entity_key(&tagged.op);
        by_target.entry(target_key).or_default().push(tagged);
    }

    let mut accepted: Vec<EvolverOperation> = Vec::new();

    for (target_key, mut group) in by_target {
        if group.len() == 1 {
            accepted.push(group.remove(0).op);
            continue;
        }

        // Multiple different ops on the same target — check for conflicts
        // Sort by strategy priority (highest first)
        group.sort_by(|a, b| {
            let pri_cmp = strategy_priority(b.strategy).cmp(&strategy_priority(a.strategy));
            if pri_cmp != std::cmp::Ordering::Equal {
                return pri_cmp;
            }
            let conf_a = a.op.confidence.unwrap_or(0.0);
            let conf_b = b.op.confidence.unwrap_or(0.0);
            conf_b
                .partial_cmp(&conf_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Check pairwise for conflicts
        let mut conflicting_indices = std::collections::HashSet::new();
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                if ops_conflict(&group[i].op, &group[j].op) {
                    // Higher-priority (lower index after sort) wins
                    conflicting_indices.insert(j);
                    let winner = &group[i];
                    let loser = &group[j];
                    conflict_log.push(format!(
                        "Conflict on {}: {} ({}) wins over {} ({})",
                        target_key,
                        winner.strategy.label(),
                        winner.op.op,
                        loser.strategy.label(),
                        loser.op.op,
                    ));
                }
            }
        }

        for (i, tagged) in group.into_iter().enumerate() {
            if !conflicting_indices.contains(&i) {
                accepted.push(tagged.op);
            }
        }
    }

    // Sort accepted ops by a stable order:
    // retirements first, then creates, then modifications
    accepted.sort_by(|a, b| {
        let order = |op: &str| -> u8 {
            if op.starts_with("retire") {
                0
            } else if op.starts_with("create") || op == "bizarre_ideation" || op.starts_with("random_compose") {
                1
            } else {
                2
            }
        };
        order(&a.op).cmp(&order(&b.op))
    });

    // Apply budget cap
    let total_accepted_before_budget = accepted.len();
    if let Some(max) = budget {
        accepted.truncate(max as usize);
    }

    let summary = format!(
        "Synthesized {} operations from {} strategies ({} total proposed). {} conflicts resolved. {} after budget cap.",
        total_accepted_before_budget,
        strategies_represented.len(),
        total_proposed,
        conflict_log.len(),
        accepted.len(),
    );

    EvolverOutput {
        run_id,
        target: None,
        operations: accepted,
        deferred_operations: all_deferred,
        summary: Some(summary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_op(op: &str, target_id: Option<&str>, confidence: Option<f64>) -> EvolverOperation {
        EvolverOperation {
            op: op.to_string(),
            target_id: target_id.map(|s| s.to_string()),
            confidence,
            ..Default::default()
        }
    }

    fn make_output(ops: Vec<EvolverOperation>) -> EvolverOutput {
        EvolverOutput {
            run_id: Some("test-run".to_string()),
            target: None,
            operations: ops,
            deferred_operations: vec![],
            summary: None,
        }
    }

    #[test]
    fn test_evolver_synthesize_merges_strategies() {
        // Two strategies each propose different operations
        let mutation_output = make_output(vec![
            make_op("modify_role", Some("role-a"), Some(0.8)),
        ]);
        let gap_output = make_output(vec![
            make_op("create_role", Some("role-b"), Some(0.7)),
        ]);

        let result = synthesize(
            vec![
                (Strategy::Mutation, mutation_output),
                (Strategy::GapAnalysis, gap_output),
            ],
            None,
        );

        assert_eq!(result.operations.len(), 2);
        assert!(result.run_id.is_some());
        // create_role should come before modify_role in the sorted output
        assert_eq!(result.operations[0].op, "create_role");
        assert_eq!(result.operations[1].op, "modify_role");
    }

    #[test]
    fn test_evolver_synthesize_deduplicates() {
        // Two strategies propose the same modify on the same target
        let mutation_output = make_output(vec![
            make_op("modify_role", Some("role-a"), Some(0.8)),
        ]);
        let component_output = make_output(vec![
            make_op("modify_role", Some("role-a"), Some(0.6)),
        ]);

        let result = synthesize(
            vec![
                (Strategy::Mutation, mutation_output),
                (Strategy::ComponentMutation, component_output),
            ],
            None,
        );

        // Should deduplicate to 1 operation
        assert_eq!(result.operations.len(), 1);
        // Should keep the higher-priority/confidence one
        assert_eq!(result.operations[0].confidence, Some(0.8));
    }

    #[test]
    fn test_evolver_synthesize_resolves_conflicts() {
        // Retirement wants to retire role-a, Mutation wants to modify it
        let retirement_output = make_output(vec![
            make_op("retire_role", Some("role-a"), Some(0.9)),
        ]);
        let mutation_output = make_output(vec![
            make_op("modify_role", Some("role-a"), Some(0.8)),
        ]);

        let result = synthesize(
            vec![
                (Strategy::Mutation, mutation_output),
                (Strategy::Retirement, retirement_output),
            ],
            None,
        );

        // Retirement has higher priority, so retire_role wins
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].op, "retire_role");
        assert!(result.summary.as_ref().unwrap().contains("1 conflicts resolved"));
    }

    #[test]
    fn test_evolver_synthesize_budget_cap() {
        let output = make_output(vec![
            make_op("modify_role", Some("role-a"), Some(0.8)),
            make_op("modify_role", Some("role-b"), Some(0.7)),
            make_op("modify_role", Some("role-c"), Some(0.6)),
            make_op("create_role", None, Some(0.9)),
        ]);

        let result = synthesize(
            vec![(Strategy::Mutation, output)],
            Some(2), // budget of 2
        );

        assert_eq!(result.operations.len(), 2);
        assert!(result.summary.as_ref().unwrap().contains("after budget cap"));
    }

    #[test]
    fn test_evolver_synthesize_partial_failures() {
        // Only one strategy succeeds, the other is missing entirely
        let gap_output = make_output(vec![
            make_op("create_role", None, Some(0.7)),
        ]);

        let result = synthesize(
            vec![(Strategy::GapAnalysis, gap_output)],
            None,
        );

        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].op, "create_role");
    }

    #[test]
    fn test_evolver_synthesize_empty_input() {
        let result = synthesize(vec![], None);
        assert!(result.operations.is_empty());
        assert!(result.summary.as_ref().unwrap().contains("No strategy outputs"));
    }

    #[test]
    fn test_evolver_synthesize_provenance_tracking() {
        let output = make_output(vec![
            make_op("modify_role", Some("role-a"), Some(0.8)),
        ]);

        let result = synthesize(
            vec![(Strategy::Mutation, output)],
            None,
        );

        assert_eq!(result.operations.len(), 1);
        let rationale = result.operations[0].rationale.as_ref().unwrap();
        assert!(rationale.contains("mutation"), "Rationale should mention source strategy: {}", rationale);
    }

    #[test]
    fn test_evolver_synthesize_deferred_operations_passed_through() {
        let mut output = make_output(vec![
            make_op("modify_role", Some("role-a"), Some(0.8)),
        ]);
        output.deferred_operations = vec![
            make_op("config_swap_outcome", Some("role-a"), None),
        ];

        let result = synthesize(
            vec![(Strategy::Mutation, output)],
            None,
        );

        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.deferred_operations.len(), 1);
        assert_eq!(result.deferred_operations[0].op, "config_swap_outcome");
    }

    #[test]
    fn test_evolver_synthesize_conflict_retire_vs_modify_same_entity() {
        // retire_motivation vs modify_motivation on same target
        let retirement_output = make_output(vec![
            make_op("retire_motivation", Some("tradeoff-x"), Some(0.9)),
        ]);
        let tuning_output = make_output(vec![
            make_op("modify_motivation", Some("tradeoff-x"), Some(0.8)),
        ]);

        let result = synthesize(
            vec![
                (Strategy::MotivationTuning, tuning_output),
                (Strategy::Retirement, retirement_output),
            ],
            None,
        );

        // Retirement wins over MotivationTuning
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].op, "retire_motivation");
    }

    #[test]
    fn test_evolver_synthesize_multiple_strategies_many_ops() {
        // 4 strategies, each with 2 ops on different targets
        let mut inputs = Vec::new();

        inputs.push((Strategy::Mutation, make_output(vec![
            make_op("modify_role", Some("r1"), Some(0.7)),
            make_op("modify_role", Some("r2"), Some(0.6)),
        ])));
        inputs.push((Strategy::GapAnalysis, make_output(vec![
            make_op("create_role", None, Some(0.8)),
            make_op("create_motivation", None, Some(0.7)),
        ])));
        inputs.push((Strategy::Retirement, make_output(vec![
            make_op("retire_role", Some("r3"), Some(0.9)),
        ])));
        inputs.push((Strategy::BizarreIdeation, make_output(vec![
            make_op("bizarre_ideation", None, Some(0.5)),
        ])));

        let result = synthesize(inputs, None);

        // All 6 operations should be accepted (no overlapping targets)
        assert_eq!(result.operations.len(), 6);
        // Retirements first, then creates, then modifications
        assert_eq!(result.operations[0].op, "retire_role");
    }

    #[test]
    fn test_evolver_synthesize_budget_zero() {
        let output = make_output(vec![
            make_op("modify_role", Some("role-a"), Some(0.8)),
        ]);

        let result = synthesize(
            vec![(Strategy::Mutation, output)],
            Some(0),
        );

        assert_eq!(result.operations.len(), 0);
    }
}
