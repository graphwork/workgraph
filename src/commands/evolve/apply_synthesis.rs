use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use workgraph::agency::{
    self, AccessControl, ComponentCategory, ContentRef, Lineage, PerformanceRecord, Role,
    RoleComponent,
};

use super::operations::apply_operation;
use super::strategy::{EvolverOperation, EvolverOutput};

/// Read synthesis-result.json, apply all operations, write apply-results.json.
pub fn run_apply_synthesis(dir: &Path, synthesis_file: &Path, output_file: &Path) -> Result<()> {
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");

    if !roles_dir.exists() {
        anyhow::bail!("Agency not initialized. Run `wg agency init` first.");
    }

    let content = fs::read_to_string(synthesis_file).with_context(|| {
        format!(
            "Failed to read synthesis file: {}",
            synthesis_file.display()
        )
    })?;
    let output: EvolverOutput = serde_json::from_str(&content)
        .context("Failed to parse synthesis file as EvolverOutput")?;

    let run_id = output.run_id.as_deref().unwrap_or("unknown").to_string();
    let total = output.operations.len();
    println!("Applying {} operations from run '{}'", total, run_id);

    let mut existing_roles = agency::load_all_roles(&roles_dir)?;
    let mut existing_tradeoffs = agency::load_all_tradeoffs(&tradeoffs_dir)?;

    let mut results: Vec<serde_json::Value> = Vec::new();

    for (i, op) in output.operations.iter().enumerate() {
        println!("[{}/{}] Applying op '{}' ...", i + 1, total, op.op);

        let result = match op.op.as_str() {
            "modify_role" | "create_role" => {
                apply_role_op_from_synthesis(op, &existing_roles, &run_id, &roles_dir, &agency_dir)
            }
            _ => apply_operation(
                op,
                &existing_roles,
                &existing_tradeoffs,
                &run_id,
                &roles_dir,
                &tradeoffs_dir,
                &agency_dir,
                dir,
            ),
        };

        match result {
            Ok(res) => {
                let status = res["status"].as_str().unwrap_or("applied").to_string();
                println!(
                    "  [+] {} — {:?}",
                    status,
                    res.get("new_id").or(res.get("existing_id"))
                );
                results.push(serde_json::json!({
                    "op": op.op,
                    "target_id": op.target_id,
                    "status": status,
                    "result": res,
                }));
                // Reload so subsequent ops see updated state
                if let Ok(roles) = agency::load_all_roles(&roles_dir) {
                    existing_roles = roles;
                }
                if let Ok(tradeoffs) = agency::load_all_tradeoffs(&tradeoffs_dir) {
                    existing_tradeoffs = tradeoffs;
                }
            }
            Err(e) => {
                println!("  [!] FAILED: {:#}", e);
                results.push(serde_json::json!({
                    "op": op.op,
                    "target_id": op.target_id,
                    "status": "failed",
                    "error": format!("{:#}", e),
                }));
            }
        }
    }

    let applied = results.iter().filter(|r| r["status"] == "applied").count();
    let no_op = results.iter().filter(|r| r["status"] == "no_op").count();
    let deferred = results
        .iter()
        .filter(|r| r["status"] == "deferred_for_review")
        .count();
    let failed = results.iter().filter(|r| r["status"] == "failed").count();

    let output_json = serde_json::json!({
        "run_id": run_id,
        "total_operations": total,
        "applied": applied,
        "no_op": no_op,
        "deferred": deferred,
        "failed": failed,
        "results": results,
    });

    if let Some(parent) = output_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_file, serde_json::to_string_pretty(&output_json)?)
        .with_context(|| format!("Failed to write apply results to {}", output_file.display()))?;

    println!(
        "\nApply complete: {} applied, {} no-op, {} deferred, {} failed",
        applied, no_op, deferred, failed
    );
    println!("Results written to: {}", output_file.display());

    Ok(())
}

/// Check if a string is a 64-char lowercase hex hash (i.e., a content-addressed ID).
fn is_content_hash(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Apply a modify_role or create_role operation from the synthesizer.
///
/// The synthesizer may provide component_ids as actual 64-char hex hashes (for crossover
/// operations) or as skill names (for create_role with new skills). We detect which case
/// applies and handle accordingly.
fn apply_role_op_from_synthesis(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let raw_comp_ids = op.component_ids.as_deref().unwrap_or_default();

    // If all component IDs are 64-char hex hashes, they're already content-addressed IDs.
    // Use them directly without re-hashing (crossover / modify_role from synthesizer).
    let all_hashes = !raw_comp_ids.is_empty() && raw_comp_ids.iter().all(|s| is_content_hash(s));

    if all_hashes {
        apply_role_with_direct_hashes(op, existing_roles, run_id, roles_dir)
    } else {
        // Skill names — create component stubs + role (create_role with new skills)
        apply_create_role_from_skill_names(op, run_id, roles_dir, agency_dir)
    }
}

/// Apply a role op where component_ids are already content hashes.
/// Sorts them, computes role hash, writes role YAML.
fn apply_role_with_direct_hashes(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let mut component_ids: Vec<String> = op.component_ids.as_deref().unwrap_or_default().to_vec();
    component_ids.sort();

    let outcome_id = op.outcome_id.clone().unwrap_or_default();
    let new_role_id = agency::content_hash_role(&component_ids, &outcome_id);

    if roles_dir.join(format!("{}.yaml", new_role_id)).exists() {
        return Ok(serde_json::json!({
            "op": op.op,
            "status": "no_op",
            "reason": "This role composition already exists",
            "existing_id": new_role_id,
        }));
    }

    // Compute lineage
    let lineage = if op.op == "create_role" {
        Lineage {
            parent_ids: vec![],
            generation: 0,
            created_by: format!("evolver-{}", run_id),
            created_at: chrono::Utc::now(),
        }
    } else {
        // modify_role — may be crossover with comma-separated parent IDs
        let target_id = op.target_id.as_deref().unwrap_or("");
        let parent_ids: Vec<String> = target_id
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        let max_gen = parent_ids
            .iter()
            .filter_map(|pid| existing_roles.iter().find(|r| r.id == *pid))
            .map(|r| r.lineage.generation)
            .max()
            .unwrap_or(0);

        Lineage {
            parent_ids: parent_ids.clone(),
            generation: max_gen.saturating_add(1),
            created_by: if parent_ids.len() > 1 {
                format!("evolver-crossover-{}", run_id)
            } else {
                format!("evolver-{}", run_id)
            },
            created_at: chrono::Utc::now(),
        }
    };

    let name = op
        .name
        .clone()
        .unwrap_or_else(|| new_role_id[..8].to_string());
    let role = Role {
        id: new_role_id.clone(),
        name: name.clone(),
        description: op.description.clone().unwrap_or_default(),
        component_ids: component_ids.clone(),
        outcome_id,
        performance: PerformanceRecord::default(),
        lineage,
        default_context_scope: None,
        default_exec_mode: None,
    };

    let path = agency::save_role(&role, roles_dir).context("Failed to save role")?;

    Ok(serde_json::json!({
        "op": op.op,
        "new_id": new_role_id,
        "name": name,
        "component_count": component_ids.len(),
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

/// Create a role from skill names, creating component stubs for any new skills.
fn apply_create_role_from_skill_names(
    op: &EvolverOperation,
    run_id: &str,
    roles_dir: &Path,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let skill_names = op.component_ids.as_deref().unwrap_or_default();
    let components_dir = agency_dir.join("primitives/components");
    fs::create_dir_all(&components_dir)?;

    let mut component_ids: Vec<String> = Vec::new();
    for skill_name in skill_names {
        let comp_id = agency::content_hash_component(
            skill_name,
            &ComponentCategory::Translated,
            &ContentRef::Name(skill_name.to_string()),
        );

        let comp_path = components_dir.join(format!("{}.yaml", comp_id));
        if !comp_path.exists() {
            let component = RoleComponent {
                id: comp_id.clone(),
                name: skill_name.to_string(),
                description: skill_name.to_string(),
                category: ComponentCategory::Translated,
                content: ContentRef::Name(skill_name.to_string()),
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
            agency::save_component(&component, &components_dir)
                .with_context(|| format!("Failed to save component stub for '{}'", skill_name))?;
        }

        component_ids.push(comp_id);
    }

    component_ids.sort();
    let outcome_id = op.outcome_id.clone().unwrap_or_default();
    let role_id = agency::content_hash_role(&component_ids, &outcome_id);

    if roles_dir.join(format!("{}.yaml", role_id)).exists() {
        return Ok(serde_json::json!({
            "op": "create_role",
            "status": "no_op",
            "reason": "This role composition already exists",
            "existing_id": role_id,
        }));
    }

    let name = op.name.as_deref().unwrap_or("unnamed-role").to_string();
    let role = Role {
        id: role_id.clone(),
        name: name.clone(),
        description: op.description.clone().unwrap_or_default(),
        component_ids: component_ids.clone(),
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
        "new_id": role_id,
        "name": name,
        "skill_count": skill_names.len(),
        "path": path.display().to_string(),
        "status": "applied",
    }))
}
