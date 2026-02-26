use anyhow::{Context, Result};
use std::path::Path;
use workgraph::agency::{self, Agent, Lineage, PerformanceRecord};
use workgraph::config::Config;
use workgraph::graph::TrustLevel;

/// `wg agency init` — bootstrap agency with starter roles, motivations, a default
/// agent, and enable auto_assign + auto_evaluate in config.
pub fn run(workgraph_dir: &Path) -> Result<()> {
    let agency_dir = workgraph_dir.join("agency");

    // 1. Seed starter roles and motivations
    let (roles_created, motivations_created) =
        agency::seed_starters(&agency_dir).context("Failed to seed agency starters")?;

    if roles_created > 0 || motivations_created > 0 {
        println!(
            "Seeded {} roles and {} motivations.",
            roles_created, motivations_created
        );
    }

    // 2. Create a default agent: Programmer + Careful
    let agents_dir = agency_dir.join("cache/agents");
    std::fs::create_dir_all(&agents_dir).context("Failed to create agents directory")?;

    let roles = agency::starter_roles();
    let motivations = agency::starter_tradeoffs();

    let programmer = roles
        .iter()
        .find(|r| r.name == "Programmer")
        .ok_or_else(|| {
            anyhow::anyhow!("Programmer starter role missing from agency::starter_roles()")
        })?;
    let careful = motivations
        .iter()
        .find(|m| m.name == "Careful")
        .ok_or_else(|| {
            anyhow::anyhow!("Careful starter motivation missing from agency::starter_motivations()")
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

    // 3. Enable auto_assign and auto_evaluate in config
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
    if config.agency.assigner_model.is_none() {
        config.agency.assigner_model = Some("haiku".to_string());
        config_changed = true;
    }
    if config.agency.evaluator_model.is_none() {
        config.agency.evaluator_model = Some("haiku".to_string());
        config_changed = true;
    }

    if config_changed {
        config
            .save(workgraph_dir)
            .context("Failed to save config")?;
        println!("Enabled auto_assign and auto_evaluate in config.");
    }

    // 4. Register the creator-pipeline function if it doesn't exist
    let func_dir = workgraph::function::functions_dir(workgraph_dir);
    let pipeline_path = func_dir.join("creator-pipeline.yaml");
    if !pipeline_path.exists() {
        let func = agency::creator_pipeline_function();
        if let Err(e) = workgraph::function::save_function(&func, &func_dir) {
            eprintln!("Warning: failed to register creator-pipeline function: {}", e);
        } else {
            println!("Registered creator-pipeline function (creator → evolver → assigner).");
        }
    }

    // Summary
    if roles_created == 0 && motivations_created == 0 && !agent_created && !config_changed {
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

        // Verify roles were created
        let roles_dir = wg_dir.join("agency").join("cache/roles");
        let role_count = std::fs::read_dir(&roles_dir).unwrap().count();
        assert!(
            role_count >= 4,
            "Expected at least 4 roles, got {}",
            role_count
        );

        // Verify tradeoffs were created
        let tradeoffs_dir = wg_dir.join("agency").join("primitives/tradeoffs");
        let tradeoff_count = std::fs::read_dir(&tradeoffs_dir).unwrap().count();
        assert!(
            tradeoff_count >= 4,
            "Expected at least 4 tradeoffs, got {}",
            tradeoff_count
        );

        // Verify agent was created
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        let agent_count = std::fs::read_dir(&agents_dir).unwrap().count();
        assert_eq!(agent_count, 1, "Expected 1 default agent");

        // Verify config was updated
        let config = Config::load(&wg_dir).unwrap();
        assert!(config.agency.auto_assign);
        assert!(config.agency.auto_evaluate);
    }

    #[test]
    fn test_agency_init_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        // Run init twice
        run(&wg_dir).unwrap();
        run(&wg_dir).unwrap();

        // Should still have exactly 1 agent
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        let agent_count = std::fs::read_dir(&agents_dir).unwrap().count();
        assert_eq!(agent_count, 1);
    }
}
