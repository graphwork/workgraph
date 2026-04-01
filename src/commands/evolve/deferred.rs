use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::fs;
use std::path::Path;

use workgraph::agency;
use workgraph::graph::{Node, Status, Task};
use workgraph::modify_graph;

use super::operations::apply_operation;
use super::strategy::EvolverOperation;

// ---------------------------------------------------------------------------
// Deferred operation types (human oversight gate)
// ---------------------------------------------------------------------------

/// Why an evolver operation was deferred for human review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferralReason {
    /// entity_type = outcome with requires_human_oversight
    ObjectiveChange,
    /// bizarre_ideation on outcome
    BizarreObjective,
    /// trade-off config has protect-objectives
    ProtectObjectivesFlag,
}

/// A human decision on a deferred operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HumanDecision {
    pub approved: bool,
    pub decided_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// An evolver operation placed in the deferred queue for human review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeferredOperation {
    pub id: String,
    pub task_id: String,
    pub operation: EvolverOperation,
    pub deferred_reason: DeferralReason,
    pub proposed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_decision: Option<HumanDecision>,
}

// ---------------------------------------------------------------------------
// Evolver self-mutation deferral
// ---------------------------------------------------------------------------

/// Create a verified workgraph task for an evolver self-mutation operation.
/// The task requires human approval before the mutation can be applied.
pub(crate) fn defer_self_mutation(
    op: &EvolverOperation,
    dir: &Path,
    run_id: &str,
) -> Result<String> {
    let graph_path = super::super::graph_path(dir);

    let task_id = format!(
        "evolve-review-{}-{}",
        op.op,
        op.target_id.as_deref().unwrap_or("unknown"),
    );

    // Check for duplicate outside modify_graph to avoid needless locking
    // (still re-checked inside for safety)
    let task_id_clone = task_id.clone();

    let op_json = serde_json::to_string_pretty(op).unwrap_or_else(|_| format!("{:?}", op.op));

    let desc = format!(
        "The evolver (run {run_id}) proposed a mutation targeting its own identity. \
         This requires human review before applying.\n\n\
         ## Proposed Operation\n\n\
         ```json\n{op_json}\n```\n\n\
         ## Instructions\n\n\
         Review the proposed change. If acceptable, apply it manually with \
         `wg evolve` or by editing the role/motivation YAML directly, then \
         `wg approve {task_id}`.",
    );

    let task = Task {
        id: task_id.clone(),
        title: format!(
            "Review evolver self-mutation: {} on {}",
            op.op,
            op.target_id.as_deref().unwrap_or("?")
        ),
        description: Some(desc),
        status: Status::Open,
        assigned: None,
        estimate: None,
        before: vec![],
        after: vec![],
        requires: vec![],
        tags: vec!["evolution".to_string(), "agency".to_string()],
        skills: vec![],
        inputs: vec![],
        deliverables: vec![],
        artifacts: vec![],
        exec: None,
        not_before: None,
        created_at: Some(Utc::now().to_rfc3339()),
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries: None,
        failure_reason: None,
        model: None,
        provider: None,
        endpoint: None,
        verify: Some("Human must approve evolver self-mutation before applying.".to_string()),
        agent: None,
        loop_iteration: 0,
        last_iteration_completed_at: None,
        cycle_failure_restarts: 0,
        ready_after: None,
        paused: false,
        visibility: "internal".to_string(),
        context_scope: None,
        cycle_config: None,
        token_usage: None,
        session_id: None,
        wait_condition: None,
        checkpoint: None,
        triage_count: 0,
        resurrection_count: 0,
        last_resurrected_at: None,
        validation: None,
        validation_commands: vec![],
        test_required: false,
        rejection_count: 0,
        max_rejections: None,
        exec_mode: None,
        verify_failures: 0,
        spawn_failures: 0,
        superseded_by: vec![],
        supersedes: None,
        unplaced: false,
        place_before: vec![],
        place_near: vec![],
    };

    let mut already_exists = false;
    modify_graph(&graph_path, |graph| {
        // Re-check for duplicate under lock
        if graph.get_task(&task_id_clone).is_some() {
            already_exists = true;
            return false;
        }
        graph.add_node(Node::Task(task));
        true
    })
    .context("Failed to save graph with self-mutation review task")?;

    if !already_exists {
        super::super::notify_graph_changed(dir);
    }

    Ok(task_id)
}

// ---------------------------------------------------------------------------
// Should-defer checks
// ---------------------------------------------------------------------------

/// Check if an operation should be deferred due to human oversight gates.
pub(crate) fn should_defer(op: &EvolverOperation, agency_dir: &Path) -> Option<DeferralReason> {
    let entity_type = op.entity_type.as_deref().unwrap_or("");

    // bizarre_ideation on outcomes is always deferred
    if op.op == "bizarre_ideation" && entity_type == "outcome" {
        return Some(DeferralReason::BizarreObjective);
    }

    // config_swap_outcome is always deferred (outcome change)
    if op.op == "config_swap_outcome" {
        return Some(DeferralReason::ObjectiveChange);
    }

    // wording_mutation on outcomes: check requires_human_oversight
    if entity_type == "outcome" {
        if let Some(ref target_id) = op.target_id {
            let outcome_path = agency_dir
                .join("primitives/outcomes")
                .join(format!("{}.yaml", target_id));
            if let Ok(outcome) = agency::load_outcome(&outcome_path)
                && outcome.requires_human_oversight
            {
                return Some(DeferralReason::ObjectiveChange);
            }
        }
        // For bizarre_ideation outcomes (already handled above), or new outcomes
        // with requires_human_oversight default = true
        if op.op == "wording_mutation" && op.target_id.is_none() {
            return Some(DeferralReason::ObjectiveChange);
        }
    }

    // random_compose_role: check if the selected outcome has requires_human_oversight
    if op.op == "random_compose_role"
        && let Some(ref oid) = op.outcome_id
    {
        let outcome_path = agency_dir
            .join("primitives/outcomes")
            .join(format!("{}.yaml", oid));
        if let Ok(outcome) = agency::load_outcome(&outcome_path)
            && outcome.requires_human_oversight
        {
            return Some(DeferralReason::ObjectiveChange);
        }
    }

    None
}

/// Write a deferred operation to agency/deferred/.
pub(crate) fn defer_operation(
    op: &EvolverOperation,
    reason: DeferralReason,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let deferred_dir = agency_dir.join("deferred");
    fs::create_dir_all(&deferred_dir)?;

    let id = format!(
        "def-{}-{}",
        &run_id,
        op.target_id
            .as_deref()
            .or(op.entity_type.as_deref())
            .unwrap_or("unknown")
    );

    let deferred = DeferredOperation {
        id: id.clone(),
        task_id: run_id.to_string(),
        operation: op.clone(),
        deferred_reason: reason,
        proposed_at: Utc::now().to_rfc3339(),
        human_decision: None,
    };

    let path = deferred_dir.join(format!("{}.json", id));
    fs::write(&path, serde_json::to_string_pretty(&deferred)?)?;

    Ok(serde_json::json!({
        "op": op.op,
        "status": "deferred",
        "deferred_id": id,
        "path": path.display().to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Deferred queue management
// ---------------------------------------------------------------------------

/// List pending deferred operations.
pub fn run_deferred_list(dir: &Path, json: bool) -> Result<()> {
    let deferred_dir = dir.join("agency/deferred");
    if !deferred_dir.exists() {
        if json {
            println!("[]");
        } else {
            println!("No deferred operations.");
        }
        return Ok(());
    }

    let mut ops: Vec<DeferredOperation> = Vec::new();
    for entry in fs::read_dir(&deferred_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let contents = fs::read_to_string(&path)?;
            if let Ok(deferred) = serde_json::from_str::<DeferredOperation>(&contents)
                && deferred.human_decision.is_none()
            {
                ops.push(deferred);
            }
        }
    }

    ops.sort_by(|a, b| a.proposed_at.cmp(&b.proposed_at));

    if json {
        println!("{}", serde_json::to_string_pretty(&ops)?);
    } else if ops.is_empty() {
        println!("No pending deferred operations.");
    } else {
        println!("Pending deferred operations:\n");
        for op in &ops {
            println!(
                "  {} — {} on {} ({:?})",
                op.id,
                op.operation.op,
                op.operation.entity_type.as_deref().unwrap_or("?"),
                op.deferred_reason,
            );
            if let Some(ref rationale) = op.operation.rationale {
                println!("    Rationale: {}", rationale);
            }
            println!("    Proposed: {}", op.proposed_at);
            println!();
        }
        println!("{} pending operation(s).", ops.len());
    }

    Ok(())
}

/// Approve a deferred operation.
pub fn run_deferred_approve(dir: &Path, deferred_id: &str, note: Option<&str>) -> Result<()> {
    let deferred_dir = dir.join("agency/deferred");
    let path = deferred_dir.join(format!("{}.json", deferred_id));
    if !path.exists() {
        bail!("Deferred operation '{}' not found", deferred_id);
    }

    let contents = fs::read_to_string(&path)?;
    let mut deferred: DeferredOperation = serde_json::from_str(&contents)?;

    if deferred.human_decision.is_some() {
        bail!(
            "Deferred operation '{}' already has a decision",
            deferred_id
        );
    }

    deferred.human_decision = Some(HumanDecision {
        approved: true,
        decided_at: Utc::now().to_rfc3339(),
        note: note.map(|s| s.to_string()),
    });

    // Save the updated deferred record
    fs::write(&path, serde_json::to_string_pretty(&deferred)?)?;

    // Now apply the operation
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");

    let roles = agency::load_all_roles(&roles_dir).unwrap_or_default();
    let tradeoffs = agency::load_all_tradeoffs(&tradeoffs_dir).unwrap_or_default();

    let result = apply_operation(
        &deferred.operation,
        &roles,
        &tradeoffs,
        &deferred.task_id,
        &roles_dir,
        &tradeoffs_dir,
        &agency_dir,
        dir,
    );

    match result {
        Ok(res) => {
            println!(
                "Approved and applied '{}': {}",
                deferred_id,
                serde_json::to_string(&res)?
            );
        }
        Err(e) => {
            eprintln!("Approved '{}' but failed to apply: {}", deferred_id, e);
        }
    }

    Ok(())
}

/// Reject a deferred operation.
pub fn run_deferred_reject(dir: &Path, deferred_id: &str, note: Option<&str>) -> Result<()> {
    let deferred_dir = dir.join("agency/deferred");
    let path = deferred_dir.join(format!("{}.json", deferred_id));
    if !path.exists() {
        bail!("Deferred operation '{}' not found", deferred_id);
    }

    let contents = fs::read_to_string(&path)?;
    let mut deferred: DeferredOperation = serde_json::from_str(&contents)?;

    if deferred.human_decision.is_some() {
        bail!(
            "Deferred operation '{}' already has a decision",
            deferred_id
        );
    }

    deferred.human_decision = Some(HumanDecision {
        approved: false,
        decided_at: Utc::now().to_rfc3339(),
        note: note.map(|s| s.to_string()),
    });

    fs::write(&path, serde_json::to_string_pretty(&deferred)?)?;
    println!("Rejected deferred operation '{}'.", deferred_id);

    Ok(())
}
