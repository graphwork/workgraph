use anyhow::{Context, Result};
use std::path::Path;
use workgraph::agency::{self, Agent, Lineage, PerformanceRecord};
use workgraph::config::Config;
use workgraph::graph::TrustLevel;

/// `wg agency init` — bootstrap agency with starter roles, tradeoffs, a default
/// agent, and enable auto_assign + auto_evaluate in config.
pub fn run(workgraph_dir: &Path) -> Result<()> {
    let agency_dir = workgraph_dir.join("agency");

    // 1. Seed starter roles and tradeoffs
    let (roles_created, tradeoffs_created) =
        agency::seed_starters(&agency_dir).context("Failed to seed agency starters")?;

    if roles_created > 0 || tradeoffs_created > 0 {
        println!(
            "Seeded {} roles and {} tradeoffs.",
            roles_created, tradeoffs_created
        );
    }

    // 2. Create a default agent: Programmer + Careful
    let agents_dir = agency_dir.join("cache/agents");
    std::fs::create_dir_all(&agents_dir).context("Failed to create agents directory")?;

    let roles = agency::starter_roles();
    let tradeoffs = agency::starter_tradeoffs();

    let programmer = roles
        .iter()
        .find(|r| r.name == "Programmer")
        .ok_or_else(|| {
            anyhow::anyhow!("Programmer starter role missing from agency::starter_roles()")
        })?;
    let careful = tradeoffs
        .iter()
        .find(|t| t.name == "Careful")
        .ok_or_else(|| {
            anyhow::anyhow!("Careful starter tradeoff missing from agency::starter_tradeoffs()")
        })?;

    let agent_id = agency::content_hash_agent(&programmer.id, &careful.id);
    let agent_path = agents_dir.join(format!("{}.yaml", agent_id));

    let agent_created = if agent_path.exists() {
        println!(
            "Default agent already exists ({}).",
            agency::short_hash(&agent_id)
        );
        false
    } else {
        let agent = Agent {
            id: agent_id.clone(),
            role_id: programmer.id.clone(),
            tradeoff_id: careful.id.clone(),
            name: "Careful Programmer".to_string(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::default(),
            contact: None,
            executor: "claude".to_string(),
            deployment_history: vec![],
            attractor_weight: 0.5,
            staleness_flags: vec![],
        };

        agency::save_agent(&agent, &agents_dir).context("Failed to save default agent")?;
        println!(
            "Created default agent: Careful Programmer ({}).",
            agency::short_hash(&agent_id)
        );
        true
    };

    // 3. Compose special agents from their seeded roles and tradeoffs
    let special_roles = agency::special_agent_roles();
    let special_tradeoffs = agency::special_agent_tradeoffs();

    let special_agents: Vec<(&str, &str, &str)> = vec![
        ("Assigner", "Assigner Balanced", "Default Assigner"),
        ("Evaluator", "Evaluator Balanced", "Default Evaluator"),
        ("Evolver", "Evolver Balanced", "Default Evolver"),
        ("Agent Creator", "Creator Unconstrained", "Default Creator"),
    ];

    let mut special_agent_ids: Vec<(&str, String)> = Vec::new();

    for (role_name, tradeoff_name, agent_name) in &special_agents {
        let role = special_roles
            .iter()
            .find(|r| r.name == *role_name)
            .ok_or_else(|| {
                anyhow::anyhow!("{} role missing from special_agent_roles()", role_name)
            })?;
        let tradeoff = special_tradeoffs
            .iter()
            .find(|t| t.name == *tradeoff_name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} tradeoff missing from special_agent_tradeoffs()",
                    tradeoff_name
                )
            })?;

        let sa_id = agency::content_hash_agent(&role.id, &tradeoff.id);
        let sa_path = agents_dir.join(format!("{}.yaml", sa_id));

        if sa_path.exists() {
            println!(
                "Special agent {} already exists ({}).",
                agent_name,
                agency::short_hash(&sa_id)
            );
        } else {
            let agent = Agent {
                id: sa_id.clone(),
                role_id: role.id.clone(),
                tradeoff_id: tradeoff.id.clone(),
                name: agent_name.to_string(),
                performance: PerformanceRecord::default(),
                lineage: Lineage::default(),
                capabilities: vec![],
                rate: None,
                capacity: None,
                trust_level: TrustLevel::default(),
                contact: None,
                executor: "claude".to_string(),
                deployment_history: vec![],
                attractor_weight: 0.5,
                staleness_flags: vec![],
            };

            agency::save_agent(&agent, &agents_dir)
                .with_context(|| format!("Failed to save special agent {}", agent_name))?;
            println!(
                "Created special agent: {} ({}).",
                agent_name,
                agency::short_hash(&sa_id)
            );
        }

        special_agent_ids.push((role_name, sa_id));
    }

    // 4. Enable auto_assign and auto_evaluate in config
    let mut config = Config::load(workgraph_dir)?;
    let mut config_changed = false;

    if !config.agency.auto_assign {
        config.agency.auto_assign = true;
        config_changed = true;
    }
    if !config.agency.auto_evaluate {
        config.agency.auto_evaluate = true;
        config_changed = true;
    }

    // Default assign/eval to haiku — these are lightweight tasks that don't need
    // a full reasoning model. Using haiku reduces cost and rate limit pressure.
    // Use [models.*] table format instead of deprecated agency.*_model fields.
    if config.models.assigner.is_none() {
        config.models.assigner = Some(workgraph::config::RoleModelConfig {
            model: Some("haiku".to_string()),
            provider: None,
            tier: None,
        });
        config_changed = true;
    }
    if config.models.evaluator.is_none() {
        config.models.evaluator = Some(workgraph::config::RoleModelConfig {
            model: Some("haiku".to_string()),
            provider: None,
            tier: None,
        });
        config_changed = true;
    }

    // Wire special agent hashes into config
    for (role_name, sa_id) in &special_agent_ids {
        let config_field = match *role_name {
            "Assigner" => &mut config.agency.assigner_agent,
            "Evaluator" => &mut config.agency.evaluator_agent,
            "Evolver" => &mut config.agency.evolver_agent,
            "Agent Creator" => &mut config.agency.creator_agent,
            _ => continue,
        };
        if config_field.as_deref() != Some(sa_id.as_str()) {
            *config_field = Some(sa_id.clone());
            config_changed = true;
        }
    }

    if config_changed {
        config
            .save(workgraph_dir)
            .context("Failed to save config")?;
        println!("Enabled auto_assign and auto_evaluate in config.");
    }

    // 5. Register the creator-pipeline function if it doesn't exist
    let func_dir = workgraph::function::functions_dir(workgraph_dir);
    let pipeline_path = func_dir.join("creator-pipeline.yaml");
    if !pipeline_path.exists() {
        let func = agency::creator_pipeline_function();
        if let Err(e) = workgraph::function::save_function(&func, &func_dir) {
            eprintln!(
                "Warning: failed to register creator-pipeline function: {}",
                e
            );
        } else {
            println!("Registered creator-pipeline function (creator → evolver → assigner).");
        }
    }

    // Summary
    let special_agents_created = special_agent_ids.len();
    let _ = special_agents_created; // always 4, used for tracking
    if roles_created == 0 && tradeoffs_created == 0 && !agent_created && !config_changed {
        println!("Agency already initialized.");
    } else {
        println!();
        println!("Agency is ready. The service will now auto-assign agents to tasks.");
        println!("  Next: wg service start");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agency_init_creates_agent_and_config() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        // Run init
        run(&wg_dir).unwrap();

        // Verify roles were created (4 starter + 4 special agent = 8)
        let roles_dir = wg_dir.join("agency").join("cache/roles");
        let role_count = std::fs::read_dir(&roles_dir).unwrap().count();
        assert!(
            role_count >= 8,
            "Expected at least 8 roles (4 starter + 4 special), got {}",
            role_count
        );

        // Verify tradeoffs were created (4 starter + 7 special agent = 11)
        let tradeoffs_dir = wg_dir.join("agency").join("primitives/tradeoffs");
        let tradeoff_count = std::fs::read_dir(&tradeoffs_dir).unwrap().count();
        assert!(
            tradeoff_count >= 11,
            "Expected at least 11 tradeoffs (4 starter + 7 special), got {}",
            tradeoff_count
        );

        // Verify components were seeded
        let components_dir = wg_dir.join("agency").join("primitives/components");
        let component_count = std::fs::read_dir(&components_dir).unwrap().count();
        assert!(
            component_count >= 8,
            "Expected at least 8 components, got {}",
            component_count
        );

        // Verify outcomes were seeded
        let outcomes_dir = wg_dir.join("agency").join("primitives/outcomes");
        let outcome_count = std::fs::read_dir(&outcomes_dir).unwrap().count();
        assert!(
            outcome_count >= 4,
            "Expected at least 4 outcomes, got {}",
            outcome_count
        );

        // Verify agents were created (1 default + 4 special)
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        let agent_count = std::fs::read_dir(&agents_dir).unwrap().count();
        assert_eq!(
            agent_count, 5,
            "Expected 5 agents (1 default + 4 special), got {}",
            agent_count
        );

        // Verify config was updated
        let config = Config::load(&wg_dir).unwrap();
        assert!(config.agency.auto_assign);
        assert!(config.agency.auto_evaluate);

        // Verify special agent hashes are set in config
        assert!(
            config.agency.assigner_agent.is_some(),
            "assigner_agent should be set"
        );
        assert!(
            config.agency.evaluator_agent.is_some(),
            "evaluator_agent should be set"
        );
        assert!(
            config.agency.evolver_agent.is_some(),
            "evolver_agent should be set"
        );
        assert!(
            config.agency.creator_agent.is_some(),
            "creator_agent should be set"
        );

        // Verify each special agent hash points to an existing agent file
        for hash in [
            config.agency.assigner_agent.as_ref().unwrap(),
            config.agency.evaluator_agent.as_ref().unwrap(),
            config.agency.evolver_agent.as_ref().unwrap(),
            config.agency.creator_agent.as_ref().unwrap(),
        ] {
            let agent_path = agents_dir.join(format!("{}.yaml", hash));
            assert!(
                agent_path.exists(),
                "Agent file for hash {} should exist",
                hash
            );
        }
    }

    #[test]
    fn test_agency_init_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        // Run init twice
        run(&wg_dir).unwrap();
        run(&wg_dir).unwrap();

        // Should still have exactly 5 agents (1 default + 4 special)
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        let agent_count = std::fs::read_dir(&agents_dir).unwrap().count();
        assert_eq!(
            agent_count, 5,
            "Expected 5 agents after idempotent re-run, got {}",
            agent_count
        );

        // Config hashes should be stable across runs
        let config1 = Config::load(&wg_dir).unwrap();
        run(&wg_dir).unwrap();
        let config2 = Config::load(&wg_dir).unwrap();
        assert_eq!(config1.agency.assigner_agent, config2.agency.assigner_agent);
        assert_eq!(
            config1.agency.evaluator_agent,
            config2.agency.evaluator_agent
        );
        assert_eq!(config1.agency.evolver_agent, config2.agency.evolver_agent);
        assert_eq!(config1.agency.creator_agent, config2.agency.creator_agent);
    }
}
