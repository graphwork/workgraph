use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::Path;

use workgraph::agency::{
    self, AccessControl, ComponentCategory, ContentRef, Lineage, PerformanceRecord, Role,
    TradeoffConfig,
};
use workgraph::config::Config;

use super::deferred::{defer_operation, defer_self_mutation, should_defer};
use super::meta::{
    apply_bizarre_ideation, apply_meta_compose_agent, apply_meta_swap_role,
    apply_meta_swap_tradeoff, apply_random_compose_agent, apply_random_compose_role,
};
use super::strategy::EvolverOperation;

/// Resolve the evolver agent's own role and tradeoff IDs from config.
/// Returns an empty set if no evolver_agent is configured.
fn evolver_entity_ids(agency_dir: &Path, dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let config = Config::load_or_default(dir);
    if let Some(ref agent_hash) = config.agency.evolver_agent {
        let agent_path = agency_dir
            .join("agents")
            .join(format!("{}.yaml", agent_hash));
        if let Ok(agent) = agency::load_agent(&agent_path) {
            ids.insert(agent.role_id.clone());
            ids.insert(agent.tradeoff_id);
        }
    }
    ids
}

/// Check if an operation is a self-mutation (targets the evolver's own identity)
/// and defer it if so. Returns `Some(result)` if deferred, `None` if safe to proceed.
pub(crate) fn check_self_mutation(
    op: &EvolverOperation,
    agency_dir: &Path,
    dir: &Path,
    run_id: &str,
) -> Option<Result<serde_json::Value>> {
    let entity_ids = evolver_entity_ids(agency_dir, dir);

    // Check 1: operation target_id matches evolver's role or tradeoff
    if !entity_ids.is_empty() {
        if let Some(ref target) = op.target_id {
            let target_ids: Vec<&str> = target
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if target_ids.iter().any(|tid| entity_ids.contains(*tid)) {
                return Some(defer_self_mutation(op, dir, run_id).map(|task_id| {
                    serde_json::json!({
                        "op": op.op,
                        "target_id": op.target_id,
                        "status": "deferred_for_review",
                        "review_task": task_id,
                        "reason": "Operation targets evolver's own identity — requires human approval",
                    })
                }));
            }
        }
    }

    // Check 2: meta operations targeting the "evolver" slot
    if matches!(
        op.op.as_str(),
        "meta_swap_role" | "meta_swap_tradeoff" | "meta_compose_agent"
    ) && op.meta_role.as_deref() == Some("evolver")
    {
        return Some(defer_self_mutation(op, dir, run_id).map(|task_id| {
            serde_json::json!({
                "op": op.op,
                "meta_role": "evolver",
                "status": "deferred_for_review",
                "review_task": task_id,
                "reason": "Operation targets evolver's own configuration — requires human approval",
            })
        }));
    }

    None
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_operation(
    op: &EvolverOperation,
    existing_roles: &[Role],
    existing_tradeoffs: &[TradeoffConfig],
    run_id: &str,
    roles_dir: &Path,
    tradeoffs_dir: &Path,
    agency_dir: &Path,
    dir: &Path,
) -> Result<serde_json::Value> {
    // Self-mutation safety: operations targeting the evolver's own
    // role or tradeoff are deferred to a verified workgraph task
    // that requires human approval. This protects both single-shot
    // and fan-out evolution paths.
    if let Some(result) = check_self_mutation(op, agency_dir, dir, run_id) {
        return result;
    }

    match op.op.as_str() {
        // Legacy operations
        "create_role" => apply_create_role(op, run_id, roles_dir),
        "modify_role" => apply_modify_role(op, existing_roles, run_id, roles_dir),
        "create_motivation" => apply_create_motivation(op, run_id, tradeoffs_dir),
        "modify_motivation" => {
            apply_modify_motivation(op, existing_tradeoffs, run_id, tradeoffs_dir)
        }
        "retire_role" => apply_retire_role(op, existing_roles, roles_dir),
        "retire_motivation" => apply_retire_motivation(op, existing_tradeoffs, tradeoffs_dir),
        // New mutation operations
        "wording_mutation" => apply_wording_mutation(op, run_id, agency_dir),
        "component_substitution" => {
            apply_component_substitution(op, existing_roles, run_id, roles_dir)
        }
        "config_add_component" => apply_config_add_component(op, existing_roles, run_id, roles_dir),
        "config_remove_component" => {
            apply_config_remove_component(op, existing_roles, run_id, roles_dir)
        }
        "config_swap_outcome" => {
            apply_config_swap_outcome(op, existing_roles, run_id, roles_dir, agency_dir)
        }
        "config_swap_tradeoff" => apply_config_swap_tradeoff(op, run_id, agency_dir),
        // Randomisation operations
        "random_compose_role" => apply_random_compose_role(op, run_id, agency_dir),
        "random_compose_agent" => apply_random_compose_agent(op, run_id, agency_dir),
        // Bizarre ideation
        "bizarre_ideation" => apply_bizarre_ideation(op, run_id, agency_dir),
        // Meta-agent (AgentConfigurations level) operations
        "meta_swap_role" => apply_meta_swap_role(op, run_id, agency_dir, dir),
        "meta_swap_tradeoff" => apply_meta_swap_tradeoff(op, run_id, agency_dir, dir),
        "meta_compose_agent" => apply_meta_compose_agent(op, run_id, agency_dir, dir),
        // Coordinator prompt evolution
        "modify_coordinator_prompt" => apply_modify_coordinator_prompt(op, agency_dir),
        other => bail!("Unknown operation type: '{}'", other),
    }
}

fn apply_create_role(
    op: &EvolverOperation,
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let name = op
        .name
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("create_role requires name"))?;

    let component_ids: Vec<String> = op
        .component_ids
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|s| {
            agency::content_hash_component(
                s,
                &ComponentCategory::Translated,
                &ContentRef::Name(s.to_string()),
            )
        })
        .collect();

    let description = op.description.clone().unwrap_or_default();
    let outcome_id = op.outcome_id.clone().unwrap_or_default();
    let id = agency::content_hash_role(&component_ids, &outcome_id);

    let role = Role {
        id: id.clone(),
        name: name.to_string(),
        description,
        component_ids,
        outcome_id,
        performance: PerformanceRecord::default(),
        lineage: Lineage {
            parent_ids: vec![],
            generation: 0,
            created_by: format!("evolver-{}", run_id),
            created_at: chrono::Utc::now(),
        },
        default_context_scope: None,
        default_exec_mode: None,
    };

    let path = agency::save_role(&role, roles_dir).context("Failed to save new role")?;

    Ok(serde_json::json!({
        "op": "create_role",
        "id": id,
        "name": name,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_modify_role(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("modify_role requires target_id"))?;

    // Support crossover: target_id may be "parent-a,parent-b"
    let parent_ids: Vec<&str> = target_id
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if parent_ids.is_empty() {
        anyhow::bail!("modify_role target_id produced zero valid parent IDs after parsing");
    }

    // Find parent(s) and compute lineage
    let lineage = if parent_ids.len() == 1 {
        let parent = existing_roles
            .iter()
            .find(|r| r.id == parent_ids[0])
            .ok_or_else(|| anyhow::anyhow!("Parent role '{}' not found", parent_ids[0]))?;
        Lineage::mutation(parent_ids[0], parent.lineage.generation, run_id)
    } else {
        for pid in &parent_ids {
            if !existing_roles.iter().any(|r| r.id == *pid) {
                anyhow::bail!("Parent role '{}' not found for crossover", pid);
            }
        }
        let max_gen = parent_ids
            .iter()
            .filter_map(|pid| existing_roles.iter().find(|r| r.id == *pid))
            .map(|r| r.lineage.generation)
            .max()
            .unwrap_or(0);
        Lineage::crossover(&parent_ids, max_gen, run_id)
    };

    let component_ids: Vec<String> = op
        .component_ids
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|s| {
            agency::content_hash_component(
                s,
                &ComponentCategory::Translated,
                &ContentRef::Name(s.to_string()),
            )
        })
        .collect();

    let description = op.description.clone().unwrap_or_default();
    let outcome_id = op.outcome_id.clone().unwrap_or_default();
    let id = agency::content_hash_role(&component_ids, &outcome_id);

    let role = Role {
        id: id.clone(),
        name: op.name.clone().unwrap_or_else(|| id.clone()),
        description,
        component_ids,
        outcome_id,
        performance: PerformanceRecord::default(),
        lineage,
        default_context_scope: None,
        default_exec_mode: None,
    };

    let path = agency::save_role(&role, roles_dir).context("Failed to save modified role")?;

    Ok(serde_json::json!({
        "op": "modify_role",
        "target_id": target_id,
        "new_id": id,
        "name": role.name,
        "generation": role.lineage.generation,
        "parent_ids": role.lineage.parent_ids,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_create_motivation(
    op: &EvolverOperation,
    run_id: &str,
    tradeoffs_dir: &Path,
) -> Result<serde_json::Value> {
    let name = op
        .name
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("create_motivation requires name"))?;

    let description = op.description.clone().unwrap_or_default();
    let acceptable = op.acceptable_tradeoffs.clone().unwrap_or_default();
    let unacceptable = op.unacceptable_tradeoffs.clone().unwrap_or_default();
    let id = agency::content_hash_tradeoff(&acceptable, &unacceptable, &description);

    let tradeoff = TradeoffConfig {
        id: id.clone(),
        name: name.to_string(),
        description,
        acceptable_tradeoffs: acceptable,
        unacceptable_tradeoffs: unacceptable,
        performance: PerformanceRecord::default(),
        lineage: Lineage {
            parent_ids: vec![],
            generation: 0,
            created_by: format!("evolver-{}", run_id),
            created_at: chrono::Utc::now(),
        },
        access_control: AccessControl::default(),
        former_agents: vec![],
        former_deployments: vec![],
    };

    let path =
        agency::save_tradeoff(&tradeoff, tradeoffs_dir).context("Failed to save new tradeoff")?;

    Ok(serde_json::json!({
        "op": "create_motivation",
        "id": id,
        "name": name,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_modify_motivation(
    op: &EvolverOperation,
    existing_tradeoffs: &[TradeoffConfig],
    run_id: &str,
    tradeoffs_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("modify_motivation requires target_id"))?;

    // Support crossover: target_id may be "parent-a,parent-b"
    let parent_ids: Vec<&str> = target_id
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if parent_ids.is_empty() {
        anyhow::bail!("modify_motivation target_id produced zero valid parent IDs after parsing");
    }

    let lineage = if parent_ids.len() == 1 {
        let parent = existing_tradeoffs
            .iter()
            .find(|m| m.id == parent_ids[0])
            .ok_or_else(|| anyhow::anyhow!("Parent tradeoff '{}' not found", parent_ids[0]))?;
        Lineage::mutation(parent_ids[0], parent.lineage.generation, run_id)
    } else {
        for pid in &parent_ids {
            if !existing_tradeoffs.iter().any(|m| m.id == *pid) {
                anyhow::bail!("Parent tradeoff '{}' not found for crossover", pid);
            }
        }
        let max_gen = parent_ids
            .iter()
            .filter_map(|pid| existing_tradeoffs.iter().find(|m| m.id == *pid))
            .map(|m| m.lineage.generation)
            .max()
            .unwrap_or(0);
        Lineage::crossover(&parent_ids, max_gen, run_id)
    };

    let description = op.description.clone().unwrap_or_default();
    let acceptable = op.acceptable_tradeoffs.clone().unwrap_or_default();
    let unacceptable = op.unacceptable_tradeoffs.clone().unwrap_or_default();
    let id = agency::content_hash_tradeoff(&acceptable, &unacceptable, &description);

    let tradeoff = TradeoffConfig {
        id: id.clone(),
        name: op.name.clone().unwrap_or_else(|| id.clone()),
        description,
        acceptable_tradeoffs: acceptable,
        unacceptable_tradeoffs: unacceptable,
        performance: PerformanceRecord::default(),
        lineage,
        access_control: AccessControl::default(),
        former_agents: vec![],
        former_deployments: vec![],
    };

    let path = agency::save_tradeoff(&tradeoff, tradeoffs_dir)
        .context("Failed to save modified tradeoff")?;

    Ok(serde_json::json!({
        "op": "modify_motivation",
        "target_id": target_id,
        "new_id": id,
        "name": tradeoff.name,
        "generation": tradeoff.lineage.generation,
        "parent_ids": tradeoff.lineage.parent_ids,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_retire_role(
    op: &EvolverOperation,
    existing_roles: &[Role],
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("retire_role requires target_id"))?;

    // Verify the role exists
    if !existing_roles.iter().any(|r| r.id == target_id) {
        bail!("Role '{}' not found", target_id);
    }

    // Safety: never retire the last role
    if existing_roles.len() <= 1 {
        bail!(
            "Cannot retire '{}': it is the only remaining role. Create a replacement first.",
            target_id
        );
    }

    // Rename .yaml to .yaml.retired
    let yaml_path = roles_dir.join(format!("{}.yaml", target_id));
    let retired_path = roles_dir.join(format!("{}.yaml.retired", target_id));

    if yaml_path.exists() {
        std::fs::rename(&yaml_path, &retired_path)
            .with_context(|| format!("Failed to retire role '{}'", target_id))?;
    } else {
        bail!("Role file not found: {}", yaml_path.display());
    }

    Ok(serde_json::json!({
        "op": "retire_role",
        "target_id": target_id,
        "retired_path": retired_path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_retire_motivation(
    op: &EvolverOperation,
    existing_tradeoffs: &[TradeoffConfig],
    tradeoffs_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("retire_motivation requires target_id"))?;

    // Verify the tradeoff exists
    if !existing_tradeoffs.iter().any(|m| m.id == target_id) {
        bail!("Tradeoff '{}' not found", target_id);
    }

    // Safety: never retire the last tradeoff
    if existing_tradeoffs.len() <= 1 {
        bail!(
            "Cannot retire '{}': it is the only remaining tradeoff. Create a replacement first.",
            target_id
        );
    }

    // Rename .yaml to .yaml.retired
    let yaml_path = tradeoffs_dir.join(format!("{}.yaml", target_id));
    let retired_path = tradeoffs_dir.join(format!("{}.yaml.retired", target_id));

    if yaml_path.exists() {
        std::fs::rename(&yaml_path, &retired_path)
            .with_context(|| format!("Failed to retire tradeoff '{}'", target_id))?;
    } else {
        bail!("Tradeoff file not found: {}", yaml_path.display());
    }

    Ok(serde_json::json!({
        "op": "retire_motivation",
        "target_id": target_id,
        "retired_path": retired_path.display().to_string(),
        "status": "applied",
    }))
}

// ---------------------------------------------------------------------------
// New apply functions: mutation operations
// ---------------------------------------------------------------------------

/// Parse entity_type string to ComponentCategory, defaulting to Novel.
pub(super) fn parse_category(s: Option<&str>) -> ComponentCategory {
    match s {
        Some("translated") => ComponentCategory::Translated,
        Some("enhanced") => ComponentCategory::Enhanced,
        _ => ComponentCategory::Novel,
    }
}

fn apply_wording_mutation(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let entity_type = op
        .entity_type
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("wording_mutation requires entity_type"))?;
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("wording_mutation requires target_id"))?;

    // Check deferred gate
    if let Some(reason) = should_defer(op, agency_dir) {
        return defer_operation(op, reason, run_id, agency_dir);
    }

    match entity_type {
        "component" => {
            let components_dir = agency_dir.join("primitives/components");
            let source_path = components_dir.join(format!("{}.yaml", target_id));
            let source: agency::RoleComponent =
                agency::load_component(&source_path).context("Source component not found")?;

            let new_desc = op.new_description.as_deref().unwrap_or(&source.description);
            let new_content = if let Some(ref c) = op.new_content {
                agency::ContentRef::Inline(c.clone())
            } else {
                source.content.clone()
            };
            let category = parse_category(op.new_category.as_deref());

            let new_id = agency::content_hash_component(new_desc, &category, &new_content);
            let new_component = agency::RoleComponent {
                id: new_id.clone(),
                name: op.new_name.clone().unwrap_or_else(|| source.name.clone()),
                description: new_desc.to_string(),
                category,
                content: new_content,
                performance: PerformanceRecord::default(),
                lineage: Lineage::mutation(target_id, source.lineage.generation, run_id),
                access_control: source.access_control.clone(),
                former_agents: vec![],
                former_deployments: vec![],
            };

            let path = agency::save_component(&new_component, &components_dir)?;
            Ok(serde_json::json!({
                "op": "wording_mutation",
                "entity_type": "component",
                "source_id": target_id,
                "new_id": new_id,
                "path": path.display().to_string(),
                "status": "applied",
            }))
        }
        "tradeoff" => {
            let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
            let source_path = tradeoffs_dir.join(format!("{}.yaml", target_id));
            let source: agency::TradeoffConfig =
                agency::load_tradeoff(&source_path).context("Source tradeoff not found")?;

            let new_desc = op.new_description.as_deref().unwrap_or(&source.description);
            let acceptable = op
                .new_acceptable_tradeoffs
                .clone()
                .unwrap_or_else(|| source.acceptable_tradeoffs.clone());
            let unacceptable = op
                .new_unacceptable_tradeoffs
                .clone()
                .unwrap_or_else(|| source.unacceptable_tradeoffs.clone());

            let new_id = agency::content_hash_tradeoff(&acceptable, &unacceptable, new_desc);
            let new_tradeoff = agency::TradeoffConfig {
                id: new_id.clone(),
                name: op.new_name.clone().unwrap_or_else(|| source.name.clone()),
                description: new_desc.to_string(),
                acceptable_tradeoffs: acceptable,
                unacceptable_tradeoffs: unacceptable,
                performance: PerformanceRecord::default(),
                lineage: Lineage::mutation(target_id, source.lineage.generation, run_id),
                access_control: source.access_control.clone(),
                former_agents: vec![],
                former_deployments: vec![],
            };

            let path = agency::save_tradeoff(&new_tradeoff, &tradeoffs_dir)?;
            Ok(serde_json::json!({
                "op": "wording_mutation",
                "entity_type": "tradeoff",
                "source_id": target_id,
                "new_id": new_id,
                "path": path.display().to_string(),
                "status": "applied",
            }))
        }
        "outcome" => {
            let outcomes_dir = agency_dir.join("primitives/outcomes");
            let source_path = outcomes_dir.join(format!("{}.yaml", target_id));
            let source: agency::DesiredOutcome =
                agency::load_outcome(&source_path).context("Source outcome not found")?;

            let new_desc = op.new_description.as_deref().unwrap_or(&source.description);
            let criteria = op
                .new_success_criteria
                .clone()
                .unwrap_or_else(|| source.success_criteria.clone());

            let new_id = agency::content_hash_outcome(new_desc, &criteria);
            let new_outcome = agency::DesiredOutcome {
                id: new_id.clone(),
                name: op.new_name.clone().unwrap_or_else(|| source.name.clone()),
                description: new_desc.to_string(),
                success_criteria: criteria,
                performance: PerformanceRecord::default(),
                lineage: Lineage::mutation(target_id, source.lineage.generation, run_id),
                access_control: source.access_control.clone(),
                requires_human_oversight: source.requires_human_oversight,
                former_agents: vec![],
                former_deployments: vec![],
            };

            let path = agency::save_outcome(&new_outcome, &outcomes_dir)?;
            Ok(serde_json::json!({
                "op": "wording_mutation",
                "entity_type": "outcome",
                "source_id": target_id,
                "new_id": new_id,
                "path": path.display().to_string(),
                "status": "applied",
            }))
        }
        other => bail!("wording_mutation: unsupported entity_type '{}'", other),
    }
}

fn apply_component_substitution(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("component_substitution requires target_id"))?;
    let remove_id = op
        .remove_component_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("component_substitution requires remove_component_id"))?;
    let add_id = op
        .add_component_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("component_substitution requires add_component_id"))?;

    let old_role = existing_roles
        .iter()
        .find(|r| r.id == target_id)
        .ok_or_else(|| anyhow::anyhow!("Role '{}' not found", target_id))?;

    let mut new_comp_ids: Vec<String> = old_role
        .component_ids
        .iter()
        .filter(|c| c.as_str() != remove_id)
        .cloned()
        .collect();
    if !new_comp_ids.contains(&add_id.to_string()) {
        new_comp_ids.push(add_id.to_string());
    }
    new_comp_ids.sort();

    let new_role_id = agency::content_hash_role(&new_comp_ids, &old_role.outcome_id);
    if new_role_id == old_role.id {
        return Ok(serde_json::json!({
            "op": "component_substitution",
            "status": "no_op",
            "reason": "Substitution produces identical role hash",
        }));
    }

    let new_role = Role {
        id: new_role_id.clone(),
        name: op.new_name.clone().unwrap_or_else(|| old_role.name.clone()),
        description: op
            .new_description
            .clone()
            .unwrap_or_else(|| old_role.description.clone()),
        component_ids: new_comp_ids,
        outcome_id: old_role.outcome_id.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_role.lineage.generation, run_id),
        default_context_scope: old_role.default_context_scope.clone(),
        default_exec_mode: old_role.default_exec_mode.clone(),
    };

    let path = agency::save_role(&new_role, roles_dir)?;
    Ok(serde_json::json!({
        "op": "component_substitution",
        "target_id": target_id,
        "removed": remove_id,
        "added": add_id,
        "new_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_config_add_component(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_add_component requires target_id"))?;
    let add_id = op
        .add_component_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_add_component requires add_component_id"))?;

    let old_role = existing_roles
        .iter()
        .find(|r| r.id == target_id)
        .ok_or_else(|| anyhow::anyhow!("Role '{}' not found", target_id))?;

    let mut new_comp_ids = old_role.component_ids.clone();
    if !new_comp_ids.contains(&add_id.to_string()) {
        new_comp_ids.push(add_id.to_string());
    }
    new_comp_ids.sort();

    let new_role_id = agency::content_hash_role(&new_comp_ids, &old_role.outcome_id);
    if new_role_id == old_role.id {
        return Ok(serde_json::json!({
            "op": "config_add_component",
            "status": "no_op",
            "reason": "Component already present in role",
        }));
    }

    let new_role = Role {
        id: new_role_id.clone(),
        name: old_role.name.clone(),
        description: old_role.description.clone(),
        component_ids: new_comp_ids,
        outcome_id: old_role.outcome_id.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_role.lineage.generation, run_id),
        default_context_scope: old_role.default_context_scope.clone(),
        default_exec_mode: old_role.default_exec_mode.clone(),
    };

    let path = agency::save_role(&new_role, roles_dir)?;
    Ok(serde_json::json!({
        "op": "config_add_component",
        "target_id": target_id,
        "added": add_id,
        "new_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_config_remove_component(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_remove_component requires target_id"))?;
    let remove_id = op
        .remove_component_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_remove_component requires remove_component_id"))?;

    let old_role = existing_roles
        .iter()
        .find(|r| r.id == target_id)
        .ok_or_else(|| anyhow::anyhow!("Role '{}' not found", target_id))?;

    let new_comp_ids: Vec<String> = old_role
        .component_ids
        .iter()
        .filter(|c| c.as_str() != remove_id)
        .cloned()
        .collect();

    if new_comp_ids.len() == old_role.component_ids.len() {
        return Ok(serde_json::json!({
            "op": "config_remove_component",
            "status": "no_op",
            "reason": "Component not present in role",
        }));
    }

    let new_role_id = agency::content_hash_role(&new_comp_ids, &old_role.outcome_id);

    let new_role = Role {
        id: new_role_id.clone(),
        name: old_role.name.clone(),
        description: old_role.description.clone(),
        component_ids: new_comp_ids,
        outcome_id: old_role.outcome_id.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_role.lineage.generation, run_id),
        default_context_scope: old_role.default_context_scope.clone(),
        default_exec_mode: old_role.default_exec_mode.clone(),
    };

    let path = agency::save_role(&new_role, roles_dir)?;
    Ok(serde_json::json!({
        "op": "config_remove_component",
        "target_id": target_id,
        "removed": remove_id,
        "new_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_config_swap_outcome(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    // config_swap_outcome is always deferred (outcome change)
    if let Some(reason) = should_defer(op, agency_dir) {
        return defer_operation(op, reason, run_id, agency_dir);
    }

    // This branch executes only if the deferred operation was approved and
    // is being re-applied (should_defer won't fire in that context).
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_swap_outcome requires target_id"))?;
    let new_oid = op
        .new_outcome_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_swap_outcome requires new_outcome_id"))?;

    let old_role = existing_roles
        .iter()
        .find(|r| r.id == target_id)
        .ok_or_else(|| anyhow::anyhow!("Role '{}' not found", target_id))?;

    let new_role_id = agency::content_hash_role(&old_role.component_ids, new_oid);

    let new_role = Role {
        id: new_role_id.clone(),
        name: old_role.name.clone(),
        description: old_role.description.clone(),
        component_ids: old_role.component_ids.clone(),
        outcome_id: new_oid.to_string(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_role.lineage.generation, run_id),
        default_context_scope: old_role.default_context_scope.clone(),
        default_exec_mode: old_role.default_exec_mode.clone(),
    };

    let path = agency::save_role(&new_role, roles_dir)?;
    Ok(serde_json::json!({
        "op": "config_swap_outcome",
        "target_id": target_id,
        "new_outcome_id": new_oid,
        "new_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_config_swap_tradeoff(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_swap_tradeoff requires target_id"))?;
    let new_tid = op
        .new_tradeoff_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_swap_tradeoff requires new_tradeoff_id"))?;

    let agents_dir = agency_dir.join("cache/agents");
    let agent_path = agents_dir.join(format!("{}.yaml", target_id));
    let old_agent: agency::Agent =
        agency::load_agent(&agent_path).context("Target agent not found")?;

    let new_agent_id = agency::content_hash_agent(&old_agent.role_id, new_tid);
    if new_agent_id == old_agent.id {
        return Ok(serde_json::json!({
            "op": "config_swap_tradeoff",
            "status": "no_op",
            "reason": "Agent already has this tradeoff",
        }));
    }

    let new_agent = agency::Agent {
        id: new_agent_id.clone(),
        role_id: old_agent.role_id.clone(),
        tradeoff_id: new_tid.to_string(),
        name: old_agent.name.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_agent.lineage.generation, run_id),
        capabilities: old_agent.capabilities.clone(),
        rate: old_agent.rate,
        capacity: old_agent.capacity,
        trust_level: old_agent.trust_level.clone(),
        contact: old_agent.contact.clone(),
        executor: old_agent.executor.clone(),
        preferred_model: old_agent.preferred_model.clone(),
        preferred_provider: old_agent.preferred_provider.clone(),
        deployment_history: vec![],
        attractor_weight: 0.3, // untested new config
        staleness_flags: vec![],
    };

    let path = agency::save_agent(&new_agent, &agents_dir)?;
    Ok(serde_json::json!({
        "op": "config_swap_tradeoff",
        "target_id": target_id,
        "new_tradeoff_id": new_tid,
        "new_id": new_agent_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

/// Apply a coordinator prompt modification.
///
/// `target_id` must be "evolved-amendments" or "common-patterns" (the mutable files).
/// `new_content` is written to the file (replacing existing content).
fn apply_modify_coordinator_prompt(
    op: &EvolverOperation,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let target = op
        .target_id
        .as_deref()
        .context("modify_coordinator_prompt requires target_id")?;

    // Only allow modification of mutable prompt files
    let allowed = ["evolved-amendments", "common-patterns"];
    if !allowed.contains(&target) {
        bail!(
            "Cannot modify coordinator prompt file '{}'. Only {:?} are mutable.",
            target,
            allowed
        );
    }

    let content = op
        .new_content
        .as_deref()
        .context("modify_coordinator_prompt requires new_content")?;

    let filename = format!("{}.md", target);
    let prompt_dir = agency_dir.join("coordinator-prompt");
    std::fs::create_dir_all(&prompt_dir)
        .context("Failed to create coordinator-prompt directory")?;

    let path = prompt_dir.join(&filename);
    std::fs::write(&path, content).with_context(|| {
        format!(
            "Failed to write coordinator prompt file: {}",
            path.display()
        )
    })?;

    Ok(serde_json::json!({
        "op": "modify_coordinator_prompt",
        "target_id": target,
        "path": path.display().to_string(),
        "content_length": content.len(),
        "status": "applied",
    }))
}
