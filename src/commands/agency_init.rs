use anyhow::{Context, Result};
use std::path::Path;
use workgraph::agency::{self, Agent, Lineage, PerformanceRecord};
use workgraph::config::Config;
use workgraph::graph::TrustLevel;

use super::agency_import;

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

    // 1b. Auto-import bundled CSV if available and not already imported
    try_csv_import(workgraph_dir)?;

    // 1c. Pull from upstream bureau if configured (non-blocking)
    try_upstream_pull(workgraph_dir);

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
            preferred_model: None,
            preferred_provider: None,
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
                preferred_model: None,
                preferred_provider: None,
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
            model: Some("claude:haiku".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        config_changed = true;
    }
    if config.models.evaluator.is_none() {
        config.models.evaluator = Some(workgraph::config::RoleModelConfig {
            model: Some("claude:haiku".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
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

/// Try to pull primitives from the configured upstream bureau URL.
///
/// This is non-blocking: if no upstream URL is configured, or if the fetch fails
/// (e.g., offline), we print a warning and continue. Init must not fail because
/// of an upstream pull error.
fn try_upstream_pull(workgraph_dir: &Path) {
    // Load merged config (global + local) to pick up upstream_url from global config
    let cfg = match Config::load_merged(workgraph_dir) {
        Ok(cfg) => cfg,
        Err(_) => return, // Can't load config — skip silently
    };

    let url = match cfg.agency.upstream_url {
        Some(ref url) if !url.is_empty() => url.clone(),
        _ => return, // No upstream configured — nothing to do
    };

    println!("Pulling agency bureau from upstream...");

    let opts = agency_import::ImportOptions {
        csv_path: None,
        url: Some(url),
        upstream: false,
        dry_run: false,
        tag: Some("upstream-bureau".to_string()),
        force: false,
        check: false,
    };

    match agency_import::run_import(workgraph_dir, opts) {
        Ok(counts) => {
            let total = counts.role_components + counts.desired_outcomes + counts.trade_off_configs;
            if total > 0 {
                println!(
                    "Pulled {} primitives from upstream bureau ({} components, {} outcomes, {} tradeoffs).",
                    total,
                    counts.role_components,
                    counts.desired_outcomes,
                    counts.trade_off_configs
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to pull upstream agency bureau: {}", e);
            eprintln!(
                "  Init continues without upstream data. Run `wg agency import --upstream` later."
            );
        }
    }
}

/// The full upstream primitive pool, compiled into the binary.
/// This ensures `wg init` seeds the complete pool regardless of whether
/// the on-disk `agency/starter.csv` file exists at the project root.
const EMBEDDED_STARTER_CSV: &[u8] = include_bytes!("../../agency/starter.csv");

/// Try to auto-import primitives from the bundled CSV.
///
/// Import sources (checked in order):
/// 1. On-disk `<project_root>/agency/starter.csv` (for development/overrides)
/// 2. Embedded CSV compiled into the binary (always available)
///
/// Skips if the import manifest already exists (idempotency).
fn try_csv_import(workgraph_dir: &Path) -> Result<()> {
    let manifest = agency_import::manifest_path(workgraph_dir);
    if manifest.exists() {
        return Ok(());
    }

    // Prefer on-disk CSV (allows overrides during development)
    let project_root = workgraph_dir.parent();
    let on_disk_csv = project_root.map(|root| root.join("agency/starter.csv"));
    let use_on_disk = on_disk_csv.as_ref().is_some_and(|p| p.exists());

    let (source_label, csv_bytes): (&str, &[u8]) = if use_on_disk {
        let path = on_disk_csv.as_ref().unwrap();
        // Read on-disk file; fall back to embedded if read fails
        match std::fs::read(path) {
            Ok(bytes) => {
                // Leak is fine: this runs once during init
                let leaked: &'static [u8] = Vec::leak(bytes);
                ("agency/starter.csv (on-disk)", leaked)
            }
            Err(_) => ("agency/starter.csv (embedded)", EMBEDDED_STARTER_CSV),
        }
    } else {
        ("agency/starter.csv (embedded)", EMBEDDED_STARTER_CSV)
    };

    println!("Importing primitives from {}...", source_label);
    let counts = agency_import::run_from_bytes(
        workgraph_dir,
        source_label,
        csv_bytes,
        false,
        Some("bundled-starter"),
    )?;

    let total = counts.role_components + counts.desired_outcomes + counts.trade_off_configs;
    println!(
        "Imported {} primitives from {} ({} components, {} outcomes, {} tradeoffs).",
        total,
        source_label,
        counts.role_components,
        counts.desired_outcomes,
        counts.trade_off_configs
    );

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

    /// Helper: write a small 9-column Agency CSV fixture.
    fn write_test_csv(project_root: &Path) -> std::path::PathBuf {
        let agency_dir = project_root.join("agency");
        std::fs::create_dir_all(&agency_dir).unwrap();
        let csv_path = agency_dir.join("starter.csv");
        let csv = "\
type,name,description,quality,domain_specificity,domain,origin_instance_id,parent_content_hash,scope
role_component,Test Skill A,Skill A description,80,0,,inst-1,,task
role_component,Test Skill B,Skill B description,90,0,,inst-2,,task
desired_outcome,Test Outcome,Outcome description,85,0,,inst-3,,task
trade_off_config,Test Tradeoff,Tradeoff description,70,0,,inst-4,,task
";
        std::fs::write(&csv_path, csv).unwrap();
        csv_path
    }

    #[test]
    fn test_agency_init_auto_imports_csv() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let wg_dir = project_root.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        // Place a bundled CSV at project_root/agency/starter.csv
        write_test_csv(project_root);

        run(&wg_dir).unwrap();

        // Manifest should be written
        let manifest_path = wg_dir.join("agency/import-manifest.yaml");
        assert!(
            manifest_path.exists(),
            "import-manifest.yaml should be created"
        );

        // Parse manifest and verify counts
        let manifest_str = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest: agency_import::ImportManifest = serde_yaml::from_str(&manifest_str).unwrap();
        assert_eq!(manifest.counts.role_components, 2);
        assert_eq!(manifest.counts.desired_outcomes, 1);
        assert_eq!(manifest.counts.trade_off_configs, 1);
        assert!(!manifest.content_hash.is_empty());
        assert!(manifest.source.contains("starter.csv"));

        // Components should include both the hardcoded starters AND CSV imports
        let components_dir = wg_dir.join("agency/primitives/components");
        let comp_count = std::fs::read_dir(&components_dir).unwrap().count();
        // 8 hardcoded + 2 from CSV (may overlap in content-hash, so >= 8+2 is approximate)
        assert!(
            comp_count >= 10,
            "Expected at least 10 components (8 hardcoded + 2 CSV), got {}",
            comp_count
        );
    }

    #[test]
    fn test_agency_init_skips_reimport_when_manifest_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let wg_dir = project_root.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        write_test_csv(project_root);

        // First run: imports CSV and writes manifest
        run(&wg_dir).unwrap();

        let manifest_path = wg_dir.join("agency/import-manifest.yaml");
        let manifest1 = std::fs::read_to_string(&manifest_path).unwrap();

        // Second run: should skip (manifest exists)
        run(&wg_dir).unwrap();

        let manifest2 = std::fs::read_to_string(&manifest_path).unwrap();
        // Manifest should be unchanged (same content, not rewritten)
        assert_eq!(
            manifest1, manifest2,
            "Manifest should not be rewritten on second init"
        );
    }

    #[test]
    fn test_agency_init_imports_embedded_csv_without_on_disk_file() {
        // Even without an on-disk CSV, the embedded CSV should be imported
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        run(&wg_dir).unwrap();

        // Manifest should be created from embedded CSV
        let manifest_path = wg_dir.join("agency/import-manifest.yaml");
        assert!(
            manifest_path.exists(),
            "import-manifest.yaml should exist from embedded CSV"
        );

        // Should have hundreds of primitives from the full pool
        let components_dir = wg_dir.join("agency/primitives/components");
        let comp_count = std::fs::read_dir(&components_dir).unwrap().count();
        assert!(
            comp_count >= 100,
            "Expected at least 100 components from embedded CSV, got {}",
            comp_count
        );

        let outcomes_dir = wg_dir.join("agency/primitives/outcomes");
        let outcome_count = std::fs::read_dir(&outcomes_dir).unwrap().count();
        assert!(
            outcome_count >= 50,
            "Expected at least 50 outcomes from embedded CSV, got {}",
            outcome_count
        );

        let tradeoffs_dir = wg_dir.join("agency/primitives/tradeoffs");
        let tradeoff_count = std::fs::read_dir(&tradeoffs_dir).unwrap().count();
        assert!(
            tradeoff_count >= 100,
            "Expected at least 100 tradeoffs from embedded CSV, got {}",
            tradeoff_count
        );
    }

    #[test]
    fn test_import_manifest_has_correct_content_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let wg_dir = project_root.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let csv_path = write_test_csv(project_root);

        run(&wg_dir).unwrap();

        // Compute expected hash independently
        let csv_bytes = std::fs::read(&csv_path).unwrap();
        use sha2::{Digest, Sha256};
        let expected_hash = format!("{:x}", Sha256::new_with_prefix(&csv_bytes).finalize());

        let manifest_path = wg_dir.join("agency/import-manifest.yaml");
        let manifest: agency_import::ImportManifest =
            serde_yaml::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();

        assert_eq!(manifest.content_hash, expected_hash);
    }

    #[test]
    fn test_init_config_does_not_shadow_global_endpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        // Create empty graph so load works
        std::fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

        run(&wg_dir).unwrap();

        let config_content =
            std::fs::read_to_string(wg_dir.join("config.toml")).unwrap();
        assert!(
            !config_content.contains("endpoints = []"),
            "wg init should not write 'endpoints = []' — it shadows global config.\nGot:\n{}",
            config_content
        );
        assert!(
            !config_content.contains("model_registry = []"),
            "wg init should not write 'model_registry = []' — it shadows global config.\nGot:\n{}",
            config_content
        );
        assert!(
            !config_content.contains("default_skills = []"),
            "wg init should not write 'default_skills = []' — it shadows global config.\nGot:\n{}",
            config_content
        );
    }

    #[test]
    fn test_upstream_pull_no_url_is_noop() {
        // When no upstream_url is configured, try_upstream_pull should be a silent no-op
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        // No config at all — should not panic or error
        try_upstream_pull(&wg_dir);
    }

    #[test]
    fn test_upstream_pull_bad_url_does_not_fail() {
        // When upstream_url is set but unreachable, init should still succeed
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        // Write a config with a bogus upstream URL
        let config_path = wg_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[agency]\nupstream_url = \"http://127.0.0.1:1/nonexistent.csv\"\n",
        )
        .unwrap();

        // try_upstream_pull should warn but not panic
        try_upstream_pull(&wg_dir);

        // Full init should also succeed despite the bad URL
        run(&wg_dir).unwrap();

        // Agency data should still be created (from hardcoded starters)
        let agents_dir = wg_dir.join("agency/cache/agents");
        assert!(
            agents_dir.exists(),
            "agents should be created despite upstream failure"
        );
    }
}
