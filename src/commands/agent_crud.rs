use anyhow::{Context, Result};
use std::path::Path;
use workgraph::agency::{self, Agent, Lineage, PerformanceRecord};
use workgraph::graph::TrustLevel;

/// Get the agency agents subdirectory (creates agency structure if needed).
fn agents_dir(workgraph_dir: &Path) -> Result<std::path::PathBuf> {
    let agency_dir = workgraph_dir.join("agency");
    agency::init(&agency_dir).context("Failed to initialise agency directory")?;
    Ok(agency_dir.join("agents"))
}

/// Parse a trust level string into a TrustLevel enum.
fn parse_trust_level(s: &str) -> Result<TrustLevel> {
    match s.to_lowercase().as_str() {
        "verified" => Ok(TrustLevel::Verified),
        "provisional" => Ok(TrustLevel::Provisional),
        "unknown" => Ok(TrustLevel::Unknown),
        _ => anyhow::bail!(
            "Invalid trust level '{}'. Expected: verified, provisional, unknown",
            s
        ),
    }
}

/// `wg agent create <name> [--role <hash>] [--motivation <hash>] [--capabilities ...] [--rate N] [--capacity N] [--trust-level L] [--contact C] [--executor E]`
#[allow(clippy::too_many_arguments)]
pub fn run_create(
    workgraph_dir: &Path,
    name: &str,
    role_id: Option<&str>,
    motivation_id: Option<&str>,
    capabilities: &[String],
    rate: Option<f64>,
    capacity: Option<f64>,
    trust_level: Option<&str>,
    contact: Option<&str>,
    executor: &str,
) -> Result<()> {
    let agency_dir = workgraph_dir.join("agency");
    agency::init(&agency_dir).context("Failed to initialise agency directory")?;

    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");

    let is_human = agency::is_human_executor(executor);

    // Resolve role and motivation if provided
    let resolved_role = match role_id {
        Some(rid) => Some(
            agency::find_role_by_prefix(&roles_dir, rid)
                .with_context(|| format!("Failed to find role '{}'", rid))?,
        ),
        None => {
            if !is_human {
                anyhow::bail!("--role is required for AI agents (executor={})", executor);
            }
            None
        }
    };

    let resolved_motivation = match motivation_id {
        Some(mid) => Some(
            agency::find_motivation_by_prefix(&motivations_dir, mid)
                .with_context(|| format!("Failed to find motivation '{}'", mid))?,
        ),
        None => {
            if !is_human {
                anyhow::bail!(
                    "--motivation is required for AI agents (executor={})",
                    executor
                );
            }
            None
        }
    };

    // Compute agent ID based on available identity fields
    let (agent_role_id, agent_motivation_id, id) = match (&resolved_role, &resolved_motivation) {
        (Some(role), Some(mot)) => {
            let id = agency::content_hash_agent(&role.id, &mot.id);
            (role.id.clone(), mot.id.clone(), id)
        }
        _ => {
            // For human agents without role/motivation, hash the name + executor
            use sha2::{Digest, Sha256};
            let input = format!("human-agent:{}:{}", name, executor);
            let digest = Sha256::digest(input.as_bytes());
            let id = format!("{:x}", digest);
            let role_id = resolved_role
                .as_ref()
                .map(|r| r.id.clone())
                .unwrap_or_default();
            let mot_id = resolved_motivation
                .as_ref()
                .map(|m| m.id.clone())
                .unwrap_or_default();
            (role_id, mot_id, id)
        }
    };

    let agents_dir = agency_dir.join("agents");
    let agent_path = agents_dir.join(format!("{}.yaml", id));
    if agent_path.exists() {
        anyhow::bail!(
            "Agent with identical identity already exists ({})",
            agency::short_hash(&id)
        );
    }

    let trust = match trust_level {
        Some(s) => parse_trust_level(s)?,
        None => TrustLevel::default(),
    };

    let agent = Agent {
        id,
        role_id: agent_role_id,
        motivation_id: agent_motivation_id,
        name: name.to_string(),
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::default(),
        capabilities: capabilities.to_vec(),
        rate,
        capacity,
        trust_level: trust,
        contact: contact.map(|s| s.to_string()),
        executor: executor.to_string(),
    };

    let path = agency::save_agent(&agent, &agents_dir).context("Failed to save agent")?;

    println!(
        "Created agent '{}' ({}) at {}",
        name,
        agency::short_hash(&agent.id),
        path.display()
    );

    if let Some(role) = &resolved_role {
        println!(
            "  role:       {} ({})",
            role.name,
            agency::short_hash(&role.id)
        );
    }
    if let Some(mot) = &resolved_motivation {
        println!(
            "  motivation: {} ({})",
            mot.name,
            agency::short_hash(&mot.id)
        );
    }
    println!("  executor:   {}", executor);
    if !capabilities.is_empty() {
        println!("  capabilities: {}", capabilities.join(", "));
    }
    if let Some(r) = rate {
        println!("  rate:       {}", r);
    }
    if let Some(c) = capacity {
        println!("  capacity:   {}", c);
    }
    if let Some(ct) = contact {
        println!("  contact:    {}", ct);
    }

    Ok(())
}

/// `wg agent list [--json]`
pub fn run_list(workgraph_dir: &Path, json: bool) -> Result<()> {
    let dir = agents_dir(workgraph_dir)?;
    let agents = agency::load_all_agents(&dir).context("Failed to load agents")?;

    if json {
        let output: Vec<serde_json::Value> = agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id,
                    "name": a.name,
                    "role_id": a.role_id,
                    "motivation_id": a.motivation_id,
                    "executor": a.executor,
                    "capabilities": a.capabilities,
                    "avg_score": a.performance.avg_score,
                    "task_count": a.performance.task_count,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if agents.is_empty() {
        println!("No agents defined. Use 'wg agent create' to create one.");
    } else {
        println!("Agents:\n");
        for a in &agents {
            let score_str = a
                .performance
                .avg_score
                .map(|s| format!("{:.2}", s))
                .unwrap_or_else(|| "n/a".to_string());
            let role_str = if a.role_id.is_empty() {
                "-".to_string()
            } else {
                agency::short_hash(&a.role_id).to_string()
            };
            let mot_str = if a.motivation_id.is_empty() {
                "-".to_string()
            } else {
                agency::short_hash(&a.motivation_id).to_string()
            };
            println!(
                "  {}  {:20} role:{} mot:{} exec:{} score:{} tasks:{}",
                agency::short_hash(&a.id),
                a.name,
                role_str,
                mot_str,
                a.executor,
                score_str,
                a.performance.task_count,
            );
        }
    }

    Ok(())
}

/// `wg agent show <hash> [--json]`
pub fn run_show(workgraph_dir: &Path, id: &str, json: bool) -> Result<()> {
    let agency_dir = workgraph_dir.join("agency");
    let dir = agency_dir.join("agents");
    let agent = agency::find_agent_by_prefix(&dir, id)
        .with_context(|| format!("Failed to find agent '{}'", id))?;

    if json {
        // Include resolved role/motivation names in JSON output
        let roles_dir = agency_dir.join("roles");
        let motivations_dir = agency_dir.join("motivations");

        let role_name = agency::find_role_by_prefix(&roles_dir, &agent.role_id)
            .map(|r| r.name)
            .unwrap_or_else(|_| "(not found)".to_string());
        let motivation_name =
            agency::find_motivation_by_prefix(&motivations_dir, &agent.motivation_id)
                .map(|m| m.name)
                .unwrap_or_else(|_| "(not found)".to_string());

        let output = serde_json::json!({
            "id": agent.id,
            "name": agent.name,
            "role_id": agent.role_id,
            "role_name": role_name,
            "motivation_id": agent.motivation_id,
            "motivation_name": motivation_name,
            "executor": agent.executor,
            "capabilities": agent.capabilities,
            "rate": agent.rate,
            "capacity": agent.capacity,
            "trust_level": agent.trust_level,
            "contact": agent.contact,
            "performance": {
                "task_count": agent.performance.task_count,
                "avg_score": agent.performance.avg_score,
                "evaluations": agent.performance.evaluations.len(),
            },
            "lineage": {
                "generation": agent.lineage.generation,
                "parent_ids": agent.lineage.parent_ids,
                "created_by": agent.lineage.created_by,
                "created_at": agent.lineage.created_at.to_rfc3339(),
            },
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Agent: {} ({})", agent.name, agency::short_hash(&agent.id));
        println!("ID: {}", agent.id);
        println!();

        // Resolve role name
        let roles_dir = agency_dir.join("roles");
        let motivations_dir = agency_dir.join("motivations");

        match agency::find_role_by_prefix(&roles_dir, &agent.role_id) {
            Ok(role) => println!("Role: {} ({})", role.name, agency::short_hash(&role.id)),
            Err(_) => println!("Role: {} (not found)", agency::short_hash(&agent.role_id)),
        }

        match agency::find_motivation_by_prefix(&motivations_dir, &agent.motivation_id) {
            Ok(motivation) => println!(
                "Motivation: {} ({})",
                motivation.name,
                agency::short_hash(&motivation.id)
            ),
            Err(_) => println!(
                "Motivation: {} (not found)",
                agency::short_hash(&agent.motivation_id)
            ),
        }

        println!();
        println!("Executor: {}", agent.executor);
        if !agent.capabilities.is_empty() {
            println!("Capabilities: {}", agent.capabilities.join(", "));
        }
        if let Some(rate) = agent.rate {
            println!("Rate: {}", rate);
        }
        if let Some(capacity) = agent.capacity {
            println!("Capacity: {}", capacity);
        }
        if agent.trust_level != TrustLevel::Provisional {
            println!("Trust level: {:?}", agent.trust_level);
        }
        if let Some(contact) = &agent.contact {
            println!("Contact: {}", contact);
        }

        println!();
        println!("Performance:");
        println!("  Tasks: {}", agent.performance.task_count);
        let score_str = agent
            .performance
            .avg_score
            .map(|s| format!("{:.2}", s))
            .unwrap_or_else(|| "n/a".to_string());
        println!("  Avg score: {}", score_str);
        if !agent.performance.evaluations.is_empty() {
            println!("  Evaluations: {}", agent.performance.evaluations.len());
        }

        println!();
        println!("Lineage:");
        println!("  Generation: {}", agent.lineage.generation);
        println!("  Created by: {}", agent.lineage.created_by);
        if !agent.lineage.parent_ids.is_empty() {
            let short_parents: Vec<&str> = agent
                .lineage
                .parent_ids
                .iter()
                .map(|p| agency::short_hash(p))
                .collect();
            println!("  Parents: {}", short_parents.join(", "));
        }
    }

    Ok(())
}

/// `wg agent rm <hash>`
pub fn run_rm(workgraph_dir: &Path, id: &str) -> Result<()> {
    let dir = agents_dir(workgraph_dir)?;
    let agent = agency::find_agent_by_prefix(&dir, id)
        .with_context(|| format!("Failed to find agent '{}'", id))?;

    let path = dir.join(format!("{}.yaml", agent.id));
    std::fs::remove_file(&path)
        .with_context(|| format!("Failed to remove agent file: {}", path.display()))?;

    println!(
        "Removed agent '{}' ({})",
        agent.name,
        agency::short_hash(&agent.id)
    );
    Ok(())
}

/// `wg agent lineage <hash> [--json]`
///
/// Shows the agent itself plus the ancestry of its constituent role and motivation.
pub fn run_lineage(workgraph_dir: &Path, id: &str, json: bool) -> Result<()> {
    let agency_dir = workgraph_dir.join("agency");
    let agents_dir = agency_dir.join("agents");
    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");

    let agent = agency::find_agent_by_prefix(&agents_dir, id)
        .with_context(|| format!("Failed to find agent '{}'", id))?;

    let role_ancestry = agency::role_ancestry(&agent.role_id, &roles_dir).unwrap_or_default();
    let motivation_ancestry =
        agency::motivation_ancestry(&agent.motivation_id, &motivations_dir).unwrap_or_default();

    if json {
        let output = serde_json::json!({
            "agent": {
                "id": agent.id,
                "name": agent.name,
                "generation": agent.lineage.generation,
                "created_by": agent.lineage.created_by,
                "created_at": agent.lineage.created_at.to_rfc3339(),
                "parent_ids": agent.lineage.parent_ids,
            },
            "role_ancestry": role_ancestry.iter().map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "name": n.name,
                    "generation": n.generation,
                    "created_by": n.created_by,
                    "created_at": n.created_at.to_rfc3339(),
                    "parent_ids": n.parent_ids,
                })
            }).collect::<Vec<_>>(),
            "motivation_ancestry": motivation_ancestry.iter().map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "name": n.name,
                    "generation": n.generation,
                    "created_by": n.created_by,
                    "created_at": n.created_at.to_rfc3339(),
                    "parent_ids": n.parent_ids,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!(
        "Lineage for agent: {} ({})",
        agent.name,
        agency::short_hash(&agent.id)
    );
    println!("  Generation: {}", agent.lineage.generation);
    println!("  Created by: {}", agent.lineage.created_by);
    if !agent.lineage.parent_ids.is_empty() {
        let short_parents: Vec<&str> = agent
            .lineage
            .parent_ids
            .iter()
            .map(|p| agency::short_hash(p))
            .collect();
        println!("  Parents: [{}]", short_parents.join(", "));
    }

    println!();
    println!("Role ancestry ({})", agency::short_hash(&agent.role_id));
    if role_ancestry.is_empty() {
        println!("  (role not found)");
    } else {
        for node in &role_ancestry {
            let indent = "  ".repeat(node.generation as usize + 1);
            let gen_label = if node.generation == 0 {
                "gen 0 (root)".to_string()
            } else {
                format!("gen {}", node.generation)
            };
            let parents = if node.parent_ids.is_empty() {
                String::new()
            } else {
                let short_parents: Vec<&str> = node
                    .parent_ids
                    .iter()
                    .map(|p| agency::short_hash(p))
                    .collect();
                format!(" <- [{}]", short_parents.join(", "))
            };
            println!(
                "{}{} ({}) [{}] created by: {}{}",
                indent,
                agency::short_hash(&node.id),
                node.name,
                gen_label,
                node.created_by,
                parents
            );
        }
    }

    println!();
    println!(
        "Motivation ancestry ({})",
        agency::short_hash(&agent.motivation_id)
    );
    if motivation_ancestry.is_empty() {
        println!("  (motivation not found)");
    } else {
        for node in &motivation_ancestry {
            let indent = "  ".repeat(node.generation as usize + 1);
            let gen_label = if node.generation == 0 {
                "gen 0 (root)".to_string()
            } else {
                format!("gen {}", node.generation)
            };
            let parents = if node.parent_ids.is_empty() {
                String::new()
            } else {
                let short_parents: Vec<&str> = node
                    .parent_ids
                    .iter()
                    .map(|p| agency::short_hash(p))
                    .collect();
                format!(" <- [{}]", short_parents.join(", "))
            };
            println!(
                "{}{} ({}) [{}] created by: {}{}",
                indent,
                agency::short_hash(&node.id),
                node.name,
                gen_label,
                node.created_by,
                parents
            );
        }
    }

    Ok(())
}

/// `wg agent performance <hash> [--json]`
///
/// Shows the evaluation history for this agent.
pub fn run_performance(workgraph_dir: &Path, id: &str, json: bool) -> Result<()> {
    let agency_dir = workgraph_dir.join("agency");
    let agents_dir = agency_dir.join("agents");

    let agent = agency::find_agent_by_prefix(&agents_dir, id)
        .with_context(|| format!("Failed to find agent '{}'", id))?;

    // Load all evaluations and filter to this agent's role+motivation pair
    let evals_dir = agency_dir.join("evaluations");
    let all_evals = agency::load_all_evaluations(&evals_dir).unwrap_or_default();

    let agent_evals: Vec<_> = all_evals
        .iter()
        .filter(|e| e.role_id == agent.role_id && e.motivation_id == agent.motivation_id)
        .collect();

    if json {
        let output = serde_json::json!({
            "agent_id": agent.id,
            "agent_name": agent.name,
            "task_count": agent.performance.task_count,
            "avg_score": agent.performance.avg_score,
            "inline_evaluations": agent.performance.evaluations.iter().map(|e| {
                serde_json::json!({
                    "score": e.score,
                    "task_id": e.task_id,
                    "timestamp": e.timestamp,
                    "context_id": e.context_id,
                })
            }).collect::<Vec<_>>(),
            "full_evaluations": agent_evals.iter().map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "task_id": e.task_id,
                    "score": e.score,
                    "dimensions": e.dimensions,
                    "notes": e.notes,
                    "evaluator": e.evaluator,
                    "timestamp": e.timestamp,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!(
        "Performance for agent: {} ({})",
        agent.name,
        agency::short_hash(&agent.id)
    );
    println!("  Tasks: {}", agent.performance.task_count);
    let score_str = agent
        .performance
        .avg_score
        .map(|s| format!("{:.2}", s))
        .unwrap_or_else(|| "n/a".to_string());
    println!("  Avg score: {}", score_str);

    // Show inline evaluation refs from the agent's performance record
    if !agent.performance.evaluations.is_empty() {
        println!();
        println!(
            "Evaluation history ({} entries):",
            agent.performance.evaluations.len()
        );
        for eval in &agent.performance.evaluations {
            println!(
                "  task:{} score:{:.2} context:{} at:{}",
                &eval.task_id[..eval.task_id.len().min(12)],
                eval.score,
                agency::short_hash(&eval.context_id),
                eval.timestamp,
            );
        }
    }

    // Show full evaluation records if any exist
    if !agent_evals.is_empty() {
        println!();
        println!("Full evaluation records ({}):", agent_evals.len());
        for eval in &agent_evals {
            println!(
                "  {} task:{} score:{:.2} by:{}",
                agency::short_hash(&eval.id),
                &eval.task_id[..eval.task_id.len().min(12)],
                eval.score,
                eval.evaluator,
            );
            if !eval.dimensions.is_empty() {
                let dims: Vec<String> = eval
                    .dimensions
                    .iter()
                    .map(|(k, v)| format!("{}={:.2}", k, v))
                    .collect();
                println!("    dims: {}", dims.join(", "));
            }
            if !eval.notes.is_empty() {
                let preview: String = eval.notes.chars().take(80).collect();
                if eval.notes.len() > 80 {
                    println!("    notes: {}...", preview);
                } else {
                    println!("    notes: {}", preview);
                }
            }
        }
    }

    if agent.performance.evaluations.is_empty() && agent_evals.is_empty() {
        println!();
        println!("No evaluation history yet.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("agency").join("agents")).unwrap();
        std::fs::create_dir_all(tmp.path().join("agency").join("roles")).unwrap();
        std::fs::create_dir_all(tmp.path().join("agency").join("motivations")).unwrap();
        std::fs::create_dir_all(tmp.path().join("agency").join("evaluations")).unwrap();
        tmp
    }

    fn create_role(dir: &Path) -> String {
        let role = agency::build_role("Test Role", "A test role", vec![], "Good output");
        let roles_dir = dir.join("agency").join("roles");
        agency::save_role(&role, &roles_dir).unwrap();
        role.id
    }

    fn create_motivation(dir: &Path) -> String {
        let motivation = agency::build_motivation(
            "Test Motivation",
            "A test motivation",
            vec!["Slower delivery".to_string()],
            vec!["Skipping tests".to_string()],
        );
        let mots_dir = dir.join("agency").join("motivations");
        agency::save_motivation(&motivation, &mots_dir).unwrap();
        motivation.id
    }

    /// Helper: create an agent with defaults for the new optional fields.
    fn create_agent(dir: &Path, name: &str, role_id: &str, mot_id: &str) -> Result<()> {
        run_create(
            dir,
            name,
            Some(role_id),
            Some(mot_id),
            &[],
            None,
            None,
            None,
            None,
            "claude",
        )
    }

    #[test]
    fn test_create_and_list() {
        let tmp = setup();
        let role_id = create_role(tmp.path());
        let mot_id = create_motivation(tmp.path());

        create_agent(tmp.path(), "Test Agent", &role_id, &mot_id).unwrap();

        let agents_dir = tmp.path().join("agency").join("agents");
        let agents = agency::load_all_agents(&agents_dir).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "Test Agent");
        assert_eq!(agents[0].role_id, role_id);
        assert_eq!(agents[0].motivation_id, mot_id);
    }

    #[test]
    fn test_create_with_operational_fields() {
        let tmp = setup();
        let role_id = create_role(tmp.path());
        let mot_id = create_motivation(tmp.path());

        run_create(
            tmp.path(),
            "Ops Agent",
            Some(&role_id),
            Some(&mot_id),
            &["rust".to_string(), "python".to_string()],
            Some(50.0),
            Some(3.0),
            Some("verified"),
            Some("ops@example.com"),
            "claude",
        )
        .unwrap();

        let agents_dir = tmp.path().join("agency").join("agents");
        let agents = agency::load_all_agents(&agents_dir).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].capabilities, vec!["rust", "python"]);
        assert_eq!(agents[0].rate, Some(50.0));
        assert_eq!(agents[0].capacity, Some(3.0));
        assert_eq!(
            agents[0].trust_level,
            workgraph::graph::TrustLevel::Verified
        );
        assert_eq!(agents[0].contact, Some("ops@example.com".to_string()));
        assert_eq!(agents[0].executor, "claude");
    }

    #[test]
    fn test_create_human_agent_without_role() {
        let tmp = setup();

        run_create(
            tmp.path(),
            "Human Operator",
            None,
            None,
            &["project-management".to_string()],
            None,
            None,
            None,
            Some("@human:matrix.org"),
            "matrix",
        )
        .unwrap();

        let agents_dir = tmp.path().join("agency").join("agents");
        let agents = agency::load_all_agents(&agents_dir).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "Human Operator");
        assert_eq!(agents[0].executor, "matrix");
        assert_eq!(agents[0].contact, Some("@human:matrix.org".to_string()));
        assert!(agents[0].role_id.is_empty());
        assert!(agents[0].motivation_id.is_empty());
    }

    #[test]
    fn test_create_ai_agent_requires_role_and_motivation() {
        let tmp = setup();

        // AI agent (executor=claude) without role should fail
        let result = run_create(
            tmp.path(),
            "Bad AI",
            None,
            None,
            &[],
            None,
            None,
            None,
            None,
            "claude",
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("--role is required")
        );
    }

    #[test]
    fn test_create_duplicate_fails() {
        let tmp = setup();
        let role_id = create_role(tmp.path());
        let mot_id = create_motivation(tmp.path());

        create_agent(tmp.path(), "Agent 1", &role_id, &mot_id).unwrap();
        let result = create_agent(tmp.path(), "Agent 2", &role_id, &mot_id);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_create_with_bad_role() {
        let tmp = setup();
        let mot_id = create_motivation(tmp.path());
        let result = run_create(
            tmp.path(),
            "Bad Agent",
            Some("nonexistent"),
            Some(&mot_id),
            &[],
            None,
            None,
            None,
            None,
            "claude",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_show_and_rm() {
        let tmp = setup();
        let role_id = create_role(tmp.path());
        let mot_id = create_motivation(tmp.path());

        create_agent(tmp.path(), "Show Agent", &role_id, &mot_id).unwrap();

        let agents_dir = tmp.path().join("agency").join("agents");
        let agents = agency::load_all_agents(&agents_dir).unwrap();
        assert_eq!(agents.len(), 1);
        let agent_id = &agents[0].id;
        assert_eq!(agents[0].name, "Show Agent");
        assert_eq!(agents[0].role_id, role_id);
        assert_eq!(agents[0].motivation_id, mot_id);

        // Show should work (human-readable + JSON)
        run_show(tmp.path(), agent_id, false).unwrap();
        run_show(tmp.path(), agent_id, true).unwrap();

        // Show by prefix should resolve to the same agent
        let resolved = agency::find_agent_by_prefix(&agents_dir, &agent_id[..8]).unwrap();
        assert_eq!(resolved.id, *agent_id);

        // Remove
        run_rm(tmp.path(), agent_id).unwrap();
        assert_eq!(agency::load_all_agents(&agents_dir).unwrap().len(), 0);
    }

    #[test]
    fn test_rm_not_found() {
        let tmp = setup();
        let result = run_rm(tmp.path(), "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_list_empty() {
        let tmp = setup();
        // Verify underlying data is empty
        let agents_dir = tmp.path().join("agency").join("agents");
        let agents = agency::load_all_agents(&agents_dir).unwrap();
        assert!(agents.is_empty(), "Expected no agents in fresh setup");
        // Both output modes should succeed
        run_list(tmp.path(), false).unwrap();
        run_list(tmp.path(), true).unwrap();
    }

    #[test]
    fn test_lineage() {
        let tmp = setup();
        let role_id = create_role(tmp.path());
        let mot_id = create_motivation(tmp.path());

        create_agent(tmp.path(), "Lineage Agent", &role_id, &mot_id).unwrap();

        let agents_dir = tmp.path().join("agency").join("agents");
        let agents = agency::load_all_agents(&agents_dir).unwrap();
        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        let agent_id = &agent.id;

        // Verify lineage data is populated
        assert_eq!(agent.lineage.generation, 0);
        assert_eq!(agent.lineage.created_by, "human");
        assert!(agent.lineage.parent_ids.is_empty());

        // Verify role ancestry resolves
        let roles_dir = tmp.path().join("agency").join("roles");
        let role_ancestry = agency::role_ancestry(&agent.role_id, &roles_dir).unwrap_or_default();
        assert!(!role_ancestry.is_empty(), "Role ancestry should resolve");
        assert_eq!(role_ancestry[0].name, "Test Role");

        run_lineage(tmp.path(), agent_id, false).unwrap();
        run_lineage(tmp.path(), agent_id, true).unwrap();
    }

    #[test]
    fn test_performance_empty() {
        let tmp = setup();
        let role_id = create_role(tmp.path());
        let mot_id = create_motivation(tmp.path());

        create_agent(tmp.path(), "Perf Agent", &role_id, &mot_id).unwrap();

        let agents_dir = tmp.path().join("agency").join("agents");
        let agents = agency::load_all_agents(&agents_dir).unwrap();
        let agent = &agents[0];
        let agent_id = &agent.id;

        // Verify performance data is initialized correctly
        assert_eq!(agent.performance.task_count, 0);
        assert!(agent.performance.avg_score.is_none());
        assert!(agent.performance.evaluations.is_empty());

        // Verify evaluations dir is empty
        let evals_dir = tmp.path().join("agency").join("evaluations");
        let all_evals = agency::load_all_evaluations(&evals_dir).unwrap_or_default();
        assert!(all_evals.is_empty());

        run_performance(tmp.path(), agent_id, false).unwrap();
        run_performance(tmp.path(), agent_id, true).unwrap();
    }
}
