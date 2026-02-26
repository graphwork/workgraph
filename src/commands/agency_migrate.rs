use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use workgraph::agency::{
    self, content_hash_agent, content_hash_component, content_hash_outcome, content_hash_role,
    content_hash_tradeoff, save_agent, save_component, save_outcome, save_role, save_tradeoff,
    short_hash, AccessControl, Agent, ComponentCategory, ContentRef, DesiredOutcome, Lineage,
    PerformanceRecord, Role, RoleComponent, TradeoffConfig,
};
use workgraph::graph::TrustLevel;

// ---------------------------------------------------------------------------
// Old-format structs for deserialization
// ---------------------------------------------------------------------------

/// Old-format SkillRef: used `!name`, `!inline`, `!file`, `!url` YAML tags.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OldSkillRef {
    Name(String),
    File(PathBuf),
    Url(String),
    Inline(String),
}

impl From<OldSkillRef> for ContentRef {
    fn from(s: OldSkillRef) -> Self {
        match s {
            OldSkillRef::Name(n) => ContentRef::Name(n),
            OldSkillRef::File(p) => ContentRef::File(p),
            OldSkillRef::Url(u) => ContentRef::Url(u),
            OldSkillRef::Inline(i) => ContentRef::Inline(i),
        }
    }
}

/// Old-format Role: may have `skills` + `desired_outcome`, or just `description` + `outcome_id`.
#[derive(Debug, Clone, Deserialize)]
struct OldRole {
    id: String,
    name: String,
    description: String,
    #[serde(default)]
    skills: Vec<OldSkillRef>,
    #[serde(default)]
    desired_outcome: Option<String>,
    #[serde(default)]
    outcome_id: Option<String>,
    #[serde(default)]
    component_ids: Vec<String>,
    #[serde(default)]
    performance: PerformanceRecord,
    #[serde(default)]
    lineage: Lineage,
    #[serde(default)]
    default_context_scope: Option<String>,
}

/// Old-format Motivation: same fields as TradeoffConfig but stored in motivations/ dir.
#[derive(Debug, Clone, Deserialize)]
struct OldMotivation {
    id: String,
    name: String,
    description: String,
    #[serde(default)]
    acceptable_tradeoffs: Vec<String>,
    #[serde(default)]
    unacceptable_tradeoffs: Vec<String>,
    #[serde(default)]
    performance: PerformanceRecord,
    #[serde(default)]
    lineage: Lineage,
}

/// Old-format Agent: may have `motivation_id` or `tradeoff_id`.
#[derive(Debug, Clone, Deserialize)]
struct OldAgent {
    id: String,
    role_id: String,
    #[serde(default)]
    motivation_id: Option<String>,
    #[serde(default)]
    tradeoff_id: Option<String>,
    name: String,
    #[serde(default)]
    performance: PerformanceRecord,
    #[serde(default)]
    lineage: Lineage,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    rate: Option<f64>,
    #[serde(default)]
    capacity: Option<f64>,
    #[serde(default)]
    trust_level: TrustLevel,
    #[serde(default)]
    contact: Option<String>,
    #[serde(default = "default_executor")]
    executor: String,
}

fn default_executor() -> String {
    "claude".to_string()
}

// ---------------------------------------------------------------------------
// Migration report
// ---------------------------------------------------------------------------

struct MigrationReport {
    components_created: usize,
    outcomes_created: usize,
    tradeoffs_created: usize,
    roles_created: usize,
    agents_created: usize,
    old_roles_read: usize,
    old_motivations_read: usize,
    old_agents_read: usize,
    warnings: Vec<String>,
    /// old_role_id -> new_role_id
    role_id_map: HashMap<String, String>,
    /// old_motivation_id -> new_tradeoff_id
    tradeoff_id_map: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Core migration logic
// ---------------------------------------------------------------------------

fn load_all_yaml_from_dir<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let mut items = Vec::new();
    if !dir.exists() {
        return Ok(items);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            let item: T = serde_yaml::from_str(&contents)
                .with_context(|| format!("Failed to parse {}", path.display()))?;
            items.push(item);
        }
    }
    Ok(items)
}

fn category_for_skill(skill: &OldSkillRef) -> ComponentCategory {
    match skill {
        OldSkillRef::Inline(_) => ComponentCategory::Enhanced,
        OldSkillRef::Name(_) | OldSkillRef::File(_) | OldSkillRef::Url(_) => {
            ComponentCategory::Translated
        }
    }
}

fn component_name_for_skill(skill: &OldSkillRef) -> String {
    match skill {
        OldSkillRef::Name(n) => n.clone(),
        OldSkillRef::File(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file-skill".to_string()),
        OldSkillRef::Url(u) => u.clone(),
        OldSkillRef::Inline(text) => {
            // Use first few words as name
            let words: Vec<&str> = text.split_whitespace().take(4).collect();
            if words.is_empty() {
                "inline-skill".to_string()
            } else {
                words.join(" ")
            }
        }
    }
}

fn component_description_for_skill(skill: &OldSkillRef) -> String {
    match skill {
        OldSkillRef::Name(n) => format!("Skill: {}", n),
        OldSkillRef::File(p) => format!("Skill from file: {}", p.display()),
        OldSkillRef::Url(u) => format!("Skill from URL: {}", u),
        OldSkillRef::Inline(text) => text.clone(),
    }
}

fn migrate_roles(
    old_roles: &[OldRole],
    agency_dir: &Path,
    report: &mut MigrationReport,
) -> Result<()> {
    let components_dir = agency_dir.join("primitives/components");
    let outcomes_dir = agency_dir.join("primitives/outcomes");
    let roles_dir = agency_dir.join("cache/roles");

    for old_role in old_roles {
        let mut component_ids: Vec<String> = Vec::new();

        // 1. Convert each SkillRef -> RoleComponent primitive
        for skill in &old_role.skills {
            let content: ContentRef = skill.clone().into();
            let description = component_description_for_skill(skill);
            let category = category_for_skill(skill);
            let id = content_hash_component(&description, &category, &content);

            // Only create if not already present
            let comp_path = components_dir.join(format!("{}.yaml", id));
            if !comp_path.exists() {
                let component = RoleComponent {
                    id: id.clone(),
                    name: component_name_for_skill(skill),
                    description,
                    category,
                    content,
                    performance: old_role.performance.clone(),
                    lineage: old_role.lineage.clone(),
                    access_control: AccessControl::default(),
                    former_agents: vec![],
                    former_deployments: vec![],
                };
                save_component(&component, &components_dir)
                    .with_context(|| format!("Failed to save component {}", short_hash(&id)))?;
                report.components_created += 1;
            }
            component_ids.push(id);
        }

        // Also include any existing component_ids (roles partially migrated)
        for cid in &old_role.component_ids {
            if !component_ids.contains(cid) {
                component_ids.push(cid.clone());
            }
        }

        // 2. Create DesiredOutcome primitive from desired_outcome string
        let outcome_id = if let Some(ref desired_outcome) = old_role.desired_outcome {
            if !desired_outcome.is_empty() {
                let id = content_hash_outcome(desired_outcome, &[]);
                let out_path = outcomes_dir.join(format!("{}.yaml", id));
                if !out_path.exists() {
                    let outcome = DesiredOutcome {
                        id: id.clone(),
                        name: format!("{} outcome", old_role.name),
                        description: desired_outcome.clone(),
                        success_criteria: vec![],
                        performance: old_role.performance.clone(),
                        lineage: old_role.lineage.clone(),
                        access_control: AccessControl::default(),
                        requires_human_oversight: true,
                        former_agents: vec![],
                        former_deployments: vec![],
                    };
                    save_outcome(&outcome, &outcomes_dir).with_context(|| {
                        format!("Failed to save outcome {}", short_hash(&id))
                    })?;
                    report.outcomes_created += 1;
                }
                id
            } else {
                old_role
                    .outcome_id
                    .clone()
                    .unwrap_or_default()
            }
        } else {
            old_role
                .outcome_id
                .clone()
                .unwrap_or_default()
        };

        // If role has a description but no outcome, create an outcome from the description
        let outcome_id = if outcome_id.is_empty() && !old_role.description.is_empty() {
            let desc = format!("Successfully perform: {}", old_role.description);
            let id = content_hash_outcome(&desc, &[]);
            let out_path = outcomes_dir.join(format!("{}.yaml", id));
            if !out_path.exists() {
                let outcome = DesiredOutcome {
                    id: id.clone(),
                    name: format!("{} outcome", old_role.name),
                    description: desc,
                    success_criteria: vec![],
                    performance: old_role.performance.clone(),
                    lineage: old_role.lineage.clone(),
                    access_control: AccessControl::default(),
                    requires_human_oversight: true,
                    former_agents: vec![],
                    former_deployments: vec![],
                };
                save_outcome(&outcome, &outcomes_dir)
                    .with_context(|| format!("Failed to save outcome {}", short_hash(&id)))?;
                report.outcomes_created += 1;
            }
            id
        } else {
            outcome_id
        };

        // If role has a description but no components, create a component from the description
        if component_ids.is_empty() && !old_role.description.is_empty() {
            let content = ContentRef::Inline(old_role.description.clone());
            let id = content_hash_component(
                &old_role.description,
                &ComponentCategory::Translated,
                &content,
            );
            let comp_path = components_dir.join(format!("{}.yaml", id));
            if !comp_path.exists() {
                let component = RoleComponent {
                    id: id.clone(),
                    name: old_role.name.clone(),
                    description: old_role.description.clone(),
                    category: ComponentCategory::Translated,
                    content,
                    performance: old_role.performance.clone(),
                    lineage: old_role.lineage.clone(),
                    access_control: AccessControl::default(),
                    former_agents: vec![],
                    former_deployments: vec![],
                };
                save_component(&component, &components_dir)
                    .with_context(|| format!("Failed to save component {}", short_hash(&id)))?;
                report.components_created += 1;
            }
            component_ids.push(id);
        }

        // Sort component IDs for deterministic hashing
        component_ids.sort();

        // 3. Create new-schema Role composition
        let new_role_id = content_hash_role(&component_ids, &outcome_id);
        let role_path = roles_dir.join(format!("{}.yaml", new_role_id));
        if !role_path.exists() {
            let new_role = Role {
                id: new_role_id.clone(),
                name: old_role.name.clone(),
                description: old_role.description.clone(),
                component_ids,
                outcome_id,
                performance: old_role.performance.clone(),
                lineage: old_role.lineage.clone(),
                default_context_scope: old_role.default_context_scope.clone(),
            };
            save_role(&new_role, &roles_dir)
                .with_context(|| format!("Failed to save role {}", short_hash(&new_role_id)))?;
            report.roles_created += 1;
        }

        // Map old role ID -> new role ID
        report
            .role_id_map
            .insert(old_role.id.clone(), new_role_id);
    }

    Ok(())
}

fn migrate_motivations(
    old_motivations: &[OldMotivation],
    agency_dir: &Path,
    report: &mut MigrationReport,
) -> Result<()> {
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");

    for old_mot in old_motivations {
        // TradeoffConfig uses the same hash as Motivation
        let id = content_hash_tradeoff(
            &old_mot.acceptable_tradeoffs,
            &old_mot.unacceptable_tradeoffs,
            &old_mot.description,
        );

        let path = tradeoffs_dir.join(format!("{}.yaml", id));
        if !path.exists() {
            let tradeoff = TradeoffConfig {
                id: id.clone(),
                name: old_mot.name.clone(),
                description: old_mot.description.clone(),
                acceptable_tradeoffs: old_mot.acceptable_tradeoffs.clone(),
                unacceptable_tradeoffs: old_mot.unacceptable_tradeoffs.clone(),
                performance: old_mot.performance.clone(),
                lineage: old_mot.lineage.clone(),
                access_control: AccessControl::default(),
                former_agents: vec![],
                former_deployments: vec![],
            };
            save_tradeoff(&tradeoff, &tradeoffs_dir)
                .with_context(|| format!("Failed to save tradeoff {}", short_hash(&id)))?;
            report.tradeoffs_created += 1;
        }

        // Map old motivation ID -> new tradeoff ID
        // Note: the hash may be the same if fields are unchanged
        report
            .tradeoff_id_map
            .insert(old_mot.id.clone(), id);
    }

    Ok(())
}

fn migrate_agents(
    old_agents: &[OldAgent],
    agency_dir: &Path,
    report: &mut MigrationReport,
) -> Result<()> {
    let agents_dir = agency_dir.join("cache/agents");

    for old_agent in old_agents {
        // Resolve the old motivation_id / tradeoff_id to the new tradeoff_id
        let old_tradeoff_id = old_agent
            .tradeoff_id
            .clone()
            .or_else(|| old_agent.motivation_id.clone())
            .unwrap_or_default();

        let new_tradeoff_id = report
            .tradeoff_id_map
            .get(&old_tradeoff_id)
            .cloned()
            .unwrap_or_else(|| {
                report.warnings.push(format!(
                    "Agent '{}' ({}): tradeoff/motivation ID {} not found in migration map, using as-is",
                    old_agent.name,
                    short_hash(&old_agent.id),
                    short_hash(&old_tradeoff_id)
                ));
                old_tradeoff_id.clone()
            });

        // Resolve old role_id -> new role_id
        let new_role_id = report
            .role_id_map
            .get(&old_agent.role_id)
            .cloned()
            .unwrap_or_else(|| {
                report.warnings.push(format!(
                    "Agent '{}' ({}): role ID {} not found in migration map, using as-is",
                    old_agent.name,
                    short_hash(&old_agent.id),
                    short_hash(&old_agent.role_id)
                ));
                old_agent.role_id.clone()
            });

        // Compute new agent hash
        let new_agent_id = content_hash_agent(&new_role_id, &new_tradeoff_id);
        let agent_path = agents_dir.join(format!("{}.yaml", new_agent_id));

        if !agent_path.exists() {
            let new_agent = Agent {
                id: new_agent_id.clone(),
                role_id: new_role_id,
                tradeoff_id: new_tradeoff_id,
                name: old_agent.name.clone(),
                performance: old_agent.performance.clone(),
                lineage: old_agent.lineage.clone(),
                capabilities: old_agent.capabilities.clone(),
                rate: old_agent.rate,
                capacity: old_agent.capacity,
                trust_level: old_agent.trust_level.clone(),
                contact: old_agent.contact.clone(),
                executor: old_agent.executor.clone(),
                deployment_history: vec![],
                attractor_weight: 0.5,
                staleness_flags: vec![],
            };
            save_agent(&new_agent, &agents_dir)
                .with_context(|| format!("Failed to save agent {}", short_hash(&new_agent_id)))?;
            report.agents_created += 1;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

fn verify_migration(agency_dir: &Path) -> Result<Vec<String>> {
    let mut errors = Vec::new();

    let components_dir = agency_dir.join("primitives/components");
    let outcomes_dir = agency_dir.join("primitives/outcomes");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let roles_dir = agency_dir.join("cache/roles");
    let agents_dir = agency_dir.join("cache/agents");

    // Load all new-format entities
    let roles: Vec<Role> = load_all_yaml_from_dir(&roles_dir)?;
    let agents: Vec<Agent> = load_all_yaml_from_dir(&agents_dir)?;

    // Check: all component_ids in roles resolve
    for role in &roles {
        for cid in &role.component_ids {
            let comp_path = components_dir.join(format!("{}.yaml", cid));
            if !comp_path.exists() {
                errors.push(format!(
                    "Role '{}' ({}): component {} not found",
                    role.name,
                    short_hash(&role.id),
                    short_hash(cid)
                ));
            }
        }
        // Check outcome_id resolves (if non-empty)
        if !role.outcome_id.is_empty() {
            let out_path = outcomes_dir.join(format!("{}.yaml", role.outcome_id));
            if !out_path.exists() {
                errors.push(format!(
                    "Role '{}' ({}): outcome {} not found",
                    role.name,
                    short_hash(&role.id),
                    short_hash(&role.outcome_id)
                ));
            }
        }
    }

    // Check: all role_id and tradeoff_id in agents resolve
    for agent in &agents {
        let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
        if !role_path.exists() {
            errors.push(format!(
                "Agent '{}' ({}): role {} not found",
                agent.name,
                short_hash(&agent.id),
                short_hash(&agent.role_id)
            ));
        }
        let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));
        if !tradeoff_path.exists() {
            errors.push(format!(
                "Agent '{}' ({}): tradeoff {} not found",
                agent.name,
                short_hash(&agent.id),
                short_hash(&agent.tradeoff_id)
            ));
        }
    }

    // Check: hashes are deterministic (recompute and compare)
    for role in &roles {
        let expected_id = content_hash_role(&role.component_ids, &role.outcome_id);
        if expected_id != role.id {
            errors.push(format!(
                "Role '{}': hash mismatch — stored {} vs computed {}",
                role.name,
                short_hash(&role.id),
                short_hash(&expected_id)
            ));
        }
    }

    for agent in &agents {
        let expected_id = content_hash_agent(&agent.role_id, &agent.tradeoff_id);
        if expected_id != agent.id {
            errors.push(format!(
                "Agent '{}': hash mismatch — stored {} vs computed {}",
                agent.name,
                short_hash(&agent.id),
                short_hash(&expected_id)
            ));
        }
    }

    Ok(errors)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(workgraph_dir: &Path, dry_run: bool) -> Result<()> {
    let agency_dir = workgraph_dir.join("agency");
    let old_roles_dir = agency_dir.join("roles");
    let old_motivations_dir = agency_dir.join("motivations");
    let old_agents_dir = agency_dir.join("agents");

    // Check if old-format directories exist
    let has_old_roles = old_roles_dir.is_dir()
        && fs::read_dir(&old_roles_dir)?
            .any(|e| e.ok().map_or(false, |e| e.path().extension().and_then(|x| x.to_str()) == Some("yaml")));
    let has_old_motivations = old_motivations_dir.is_dir()
        && fs::read_dir(&old_motivations_dir)?
            .any(|e| e.ok().map_or(false, |e| e.path().extension().and_then(|x| x.to_str()) == Some("yaml")));
    let has_old_agents = old_agents_dir.is_dir()
        && fs::read_dir(&old_agents_dir)?
            .any(|e| e.ok().map_or(false, |e| e.path().extension().and_then(|x| x.to_str()) == Some("yaml")));

    if !has_old_roles && !has_old_motivations && !has_old_agents {
        println!("Nothing to migrate — no old-format agency data found.");
        println!("  Looked for: {}/roles/, {}/motivations/, {}/agents/",
            agency_dir.display(), agency_dir.display(), agency_dir.display());
        return Ok(());
    }

    // Load old-format data
    let old_roles: Vec<OldRole> = if has_old_roles {
        load_all_yaml_from_dir(&old_roles_dir)?
    } else {
        vec![]
    };
    let old_motivations: Vec<OldMotivation> = if has_old_motivations {
        load_all_yaml_from_dir(&old_motivations_dir)?
    } else {
        vec![]
    };
    let old_agents: Vec<OldAgent> = if has_old_agents {
        load_all_yaml_from_dir(&old_agents_dir)?
    } else {
        vec![]
    };

    println!(
        "Found old-format data: {} roles, {} motivations, {} agents",
        old_roles.len(),
        old_motivations.len(),
        old_agents.len()
    );

    if dry_run {
        println!("\n[dry-run] Would migrate:");
        for r in &old_roles {
            println!(
                "  Role: {} ({}) — {} skills, desired_outcome: {}",
                r.name,
                short_hash(&r.id),
                r.skills.len(),
                r.desired_outcome.as_deref().unwrap_or("(none)")
            );
        }
        for m in &old_motivations {
            println!("  Motivation: {} ({})", m.name, short_hash(&m.id));
        }
        for a in &old_agents {
            let tid = a.tradeoff_id.as_deref()
                .or(a.motivation_id.as_deref())
                .unwrap_or("(none)");
            println!(
                "  Agent: {} ({}) — role: {}, tradeoff/motivation: {}",
                a.name,
                short_hash(&a.id),
                short_hash(&a.role_id),
                short_hash(tid)
            );
        }
        return Ok(());
    }

    // Ensure new directories exist
    agency::init(&agency_dir).context("Failed to initialize agency directories")?;

    let mut report = MigrationReport {
        components_created: 0,
        outcomes_created: 0,
        tradeoffs_created: 0,
        roles_created: 0,
        agents_created: 0,
        old_roles_read: old_roles.len(),
        old_motivations_read: old_motivations.len(),
        old_agents_read: old_agents.len(),
        warnings: vec![],
        role_id_map: HashMap::new(),
        tradeoff_id_map: HashMap::new(),
    };

    // Step 1: Migrate motivations -> tradeoffs (need tradeoff IDs before agents)
    migrate_motivations(&old_motivations, &agency_dir, &mut report)
        .context("Failed to migrate motivations")?;

    // Step 2: Migrate roles -> components + outcomes + cache roles
    migrate_roles(&old_roles, &agency_dir, &mut report)
        .context("Failed to migrate roles")?;

    // Step 3: Migrate agents -> cache agents (uses role_id_map and tradeoff_id_map)
    migrate_agents(&old_agents, &agency_dir, &mut report)
        .context("Failed to migrate agents")?;

    // Step 4: Verification
    let errors = verify_migration(&agency_dir)?;

    // Report
    println!("\nMigration complete:");
    println!(
        "  Components created: {} (from {} old roles)",
        report.components_created, report.old_roles_read
    );
    println!(
        "  Outcomes created:   {} (from {} old roles)",
        report.outcomes_created, report.old_roles_read
    );
    println!(
        "  Tradeoffs created:  {} (from {} old motivations)",
        report.tradeoffs_created, report.old_motivations_read
    );
    println!(
        "  Roles created:      {} (cache compositions)",
        report.roles_created
    );
    println!(
        "  Agents created:     {} (cache compositions, from {} old agents)",
        report.agents_created, report.old_agents_read
    );

    if !report.warnings.is_empty() {
        println!("\nWarnings:");
        for w in &report.warnings {
            println!("  ⚠ {}", w);
        }
    }

    if errors.is_empty() {
        println!("\nVerification: ✓ all primitive references resolve, hashes are deterministic");
    } else {
        println!("\nVerification errors:");
        for e in &errors {
            println!("  ✗ {}", e);
        }
        anyhow::bail!(
            "Migration verification failed with {} error(s). New files were created but old files are preserved.",
            errors.len()
        );
    }

    println!("\nOld directories preserved (not deleted):");
    if has_old_roles {
        println!("  {}", old_roles_dir.display());
    }
    if has_old_motivations {
        println!("  {}", old_motivations_dir.display());
    }
    if has_old_agents {
        println!("  {}", old_agents_dir.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_yaml(dir: &Path, id: &str, content: &str) {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{}.yaml", id));
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_migrate_full_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        let agency_dir = wg_dir.join("agency");

        // Create old-format directories
        let roles_dir = agency_dir.join("roles");
        let motivations_dir = agency_dir.join("motivations");
        let agents_dir = agency_dir.join("agents");

        // Write old role with skills and desired_outcome
        write_yaml(
            &roles_dir,
            "role1",
            r#"
id: role1
name: Programmer
description: Writes code
skills:
  - !name rust
  - !inline "Always test before done"
desired_outcome: Working tested code
performance:
  task_count: 5
  avg_score: 0.8
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        // Write old motivation
        write_yaml(
            &motivations_dir,
            "mot1",
            r#"
id: mot1
name: Careful
description: Prioritizes reliability
acceptable_tradeoffs:
  - Slow
unacceptable_tradeoffs:
  - Untested
performance:
  task_count: 3
  avg_score: 0.7
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        // Write old agent referencing role1 + mot1
        write_yaml(
            &agents_dir,
            "agent1",
            r#"
id: agent1
role_id: role1
motivation_id: mot1
name: Careful Programmer
performance:
  task_count: 3
  avg_score: 0.75
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        // Run migration
        run(&wg_dir, false).unwrap();

        // Verify new primitives exist
        let comps: Vec<_> = fs::read_dir(agency_dir.join("primitives/components"))
            .unwrap()
            .collect();
        assert!(
            comps.len() >= 2,
            "Expected at least 2 components, got {}",
            comps.len()
        );

        let outcomes: Vec<_> = fs::read_dir(agency_dir.join("primitives/outcomes"))
            .unwrap()
            .collect();
        assert!(
            !outcomes.is_empty(),
            "Expected at least 1 outcome"
        );

        let tradeoffs: Vec<_> = fs::read_dir(agency_dir.join("primitives/tradeoffs"))
            .unwrap()
            .collect();
        assert_eq!(tradeoffs.len(), 1, "Expected 1 tradeoff");

        let new_roles: Vec<_> = fs::read_dir(agency_dir.join("cache/roles"))
            .unwrap()
            .collect();
        assert_eq!(new_roles.len(), 1, "Expected 1 new role");

        let new_agents: Vec<_> = fs::read_dir(agency_dir.join("cache/agents"))
            .unwrap()
            .collect();
        assert_eq!(new_agents.len(), 1, "Expected 1 new agent");

        // Run again — should be idempotent (no new files created)
        run(&wg_dir, false).unwrap();

        let comps2: Vec<_> = fs::read_dir(agency_dir.join("primitives/components"))
            .unwrap()
            .collect();
        assert_eq!(comps.len(), comps2.len(), "Idempotent: same component count");
    }

    #[test]
    fn test_migrate_role_without_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        let agency_dir = wg_dir.join("agency");

        // Write old role without skills (evolved role)
        write_yaml(
            &agency_dir.join("roles"),
            "role2",
            r#"
id: role2
name: implementer
description: Implements code changes
outcome_id: ''
performance:
  task_count: 10
  avg_score: 0.85
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        // Need at least one motivation so we don't fail on agents
        write_yaml(
            &agency_dir.join("motivations"),
            "mot2",
            r#"
id: mot2
name: Fast
description: Prioritizes speed
acceptable_tradeoffs:
  - Less documentation
unacceptable_tradeoffs:
  - Broken code
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        run(&wg_dir, false).unwrap();

        // Should have created a component from the description
        let comps: Vec<_> = fs::read_dir(agency_dir.join("primitives/components"))
            .unwrap()
            .collect();
        assert_eq!(
            comps.len(),
            1,
            "Expected 1 component from description"
        );

        // Should have created an outcome from the description
        let outcomes: Vec<_> = fs::read_dir(agency_dir.join("primitives/outcomes"))
            .unwrap()
            .collect();
        assert_eq!(outcomes.len(), 1, "Expected 1 outcome from description");
    }

    #[test]
    fn test_migrate_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        let agency_dir = wg_dir.join("agency");

        write_yaml(
            &agency_dir.join("roles"),
            "role3",
            r#"
id: role3
name: Test Role
description: Test
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        // Dry run should not create any new files
        run(&wg_dir, true).unwrap();

        assert!(
            !agency_dir.join("primitives/components").exists(),
            "Dry run should not create directories"
        );
    }

    #[test]
    fn test_migrate_nothing_to_migrate() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        // No old directories at all
        run(&wg_dir, false).unwrap();
    }

    #[test]
    fn test_deterministic_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        let agency_dir = wg_dir.join("agency");

        write_yaml(
            &agency_dir.join("roles"),
            "roleA",
            r#"
id: roleA
name: TestRole
description: A test role
skills:
  - !name testing
desired_outcome: Working tests
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        write_yaml(
            &agency_dir.join("motivations"),
            "motA",
            r#"
id: motA
name: TestMot
description: Test motivation
acceptable_tradeoffs: []
unacceptable_tradeoffs: []
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        write_yaml(
            &agency_dir.join("agents"),
            "agentA",
            r#"
id: agentA
role_id: roleA
motivation_id: motA
name: Test Agent
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
        );

        run(&wg_dir, false).unwrap();

        // Collect all new file names
        let mut first_run_files: Vec<String> = Vec::new();
        for dir_name in &[
            "primitives/components",
            "primitives/outcomes",
            "primitives/tradeoffs",
            "cache/roles",
            "cache/agents",
        ] {
            let dir = agency_dir.join(dir_name);
            if dir.exists() {
                for entry in fs::read_dir(&dir).unwrap() {
                    first_run_files.push(entry.unwrap().file_name().to_string_lossy().to_string());
                }
            }
        }

        // Delete new files and run again
        for dir_name in &[
            "primitives/components",
            "primitives/outcomes",
            "primitives/tradeoffs",
            "cache/roles",
            "cache/agents",
        ] {
            let dir = agency_dir.join(dir_name);
            if dir.exists() {
                for entry in fs::read_dir(&dir).unwrap() {
                    fs::remove_file(entry.unwrap().path()).unwrap();
                }
            }
        }

        run(&wg_dir, false).unwrap();

        let mut second_run_files: Vec<String> = Vec::new();
        for dir_name in &[
            "primitives/components",
            "primitives/outcomes",
            "primitives/tradeoffs",
            "cache/roles",
            "cache/agents",
        ] {
            let dir = agency_dir.join(dir_name);
            if dir.exists() {
                for entry in fs::read_dir(&dir).unwrap() {
                    second_run_files
                        .push(entry.unwrap().file_name().to_string_lossy().to_string());
                }
            }
        }

        first_run_files.sort();
        second_run_files.sort();
        assert_eq!(
            first_run_files, second_run_files,
            "Hashes must be deterministic across runs"
        );
    }
}
