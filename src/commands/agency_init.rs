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
    let agents_dir = agency_dir.join("agents");
    std::fs::create_dir_all(&agents_dir).context("Failed to create agents directory")?;

    let roles = agency::starter_roles();
    let motivations = agency::starter_motivations();

    let programmer = roles
        .iter()
        .find(|r| r.name == "Programmer")
        .expect("Programmer starter role must exist");
    let careful = motivations
        .iter()
        .find(|m| m.name == "Careful")
        .expect("Careful starter motivation must exist");

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
            motivation_id: careful.id.clone(),
            name: "Careful Programmer".to_string(),
            performance: PerformanceRecord {
                task_count: 0,
                avg_score: None,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::default(),
            contact: None,
            executor: "claude".to_string(),
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

    if config_changed {
        config
            .save(workgraph_dir)
            .context("Failed to save config")?;
        println!("Enabled auto_assign and auto_evaluate in config.");
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
        let roles_dir = wg_dir.join("agency").join("roles");
        let role_count = std::fs::read_dir(&roles_dir).unwrap().count();
        assert!(
            role_count >= 4,
            "Expected at least 4 roles, got {}",
            role_count
        );

        // Verify motivations were created
        let motivations_dir = wg_dir.join("agency").join("motivations");
        let motivation_count = std::fs::read_dir(&motivations_dir).unwrap().count();
        assert!(
            motivation_count >= 4,
            "Expected at least 4 motivations, got {}",
            motivation_count
        );

        // Verify agent was created
        let agents_dir = wg_dir.join("agency").join("agents");
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
        let agents_dir = wg_dir.join("agency").join("agents");
        let agent_count = std::fs::read_dir(&agents_dir).unwrap().count();
        assert_eq!(agent_count, 1);
    }
}
