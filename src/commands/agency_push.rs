use std::path::Path;

use anyhow::{Context, Result};

use workgraph::agency::{AgencyStore, LocalStore};
use workgraph::federation::{self, EntityFilter, TransferOptions};

/// Options for the push command.
pub struct PushOptions<'a> {
    pub target: &'a str,
    pub dry_run: bool,
    pub no_performance: bool,
    pub no_evaluations: bool,
    pub force: bool,
    pub global: bool,
    pub entity_ids: &'a [String],
    pub entity_type: Option<&'a str>,
    pub json: bool,
}

/// Get the local (source) store for push.
fn local_store(workgraph_dir: &Path, global: bool) -> Result<LocalStore> {
    let path = if global {
        let home = dirs::home_dir().context("Cannot determine home directory")?;
        home.join(".workgraph").join("agency")
    } else {
        workgraph_dir.join("agency")
    };
    if !LocalStore::new(&path).is_valid() {
        if global {
            anyhow::bail!("No global agency store found at ~/.workgraph/agency/. Run 'wg agency init' first.");
        } else {
            anyhow::bail!("No local agency store found. Run 'wg agency init' first.");
        }
    }
    Ok(LocalStore::new(&path))
}

pub fn run(workgraph_dir: &Path, opts: &PushOptions<'_>) -> Result<()> {
    // Local store is the source (we're pushing FROM local)
    let source = local_store(workgraph_dir, opts.global)?;

    // Resolve target store (check named remotes first, then path)
    let target_store = federation::resolve_store_with_remotes(opts.target, workgraph_dir)?;

    let entity_filter = match opts.entity_type {
        Some("component" | "components") => EntityFilter::Components,
        Some("outcome" | "outcomes") => EntityFilter::Outcomes,
        Some("role" | "roles") => EntityFilter::Roles,
        Some("motivation" | "motivations" | "tradeoff" | "tradeoffs") => EntityFilter::Tradeoffs,
        Some("agent" | "agents") => EntityFilter::Agents,
        Some(other) => anyhow::bail!("Unknown entity type '{}'. Use: component, outcome, role, tradeoff, motivation, or agent", other),
        None => EntityFilter::All,
    };

    let transfer_opts = TransferOptions {
        dry_run: opts.dry_run,
        no_performance: opts.no_performance,
        no_evaluations: opts.no_evaluations,
        force: opts.force,
        entity_ids: opts.entity_ids.to_vec(),
        entity_filter,
    };

    let summary = federation::transfer(&source, &target_store, &transfer_opts)?;

    // Update last_sync if the target was a named remote
    if !opts.dry_run {
        let _ = federation::touch_remote_sync(workgraph_dir, opts.target);
    }

    if opts.json {
        let output = serde_json::json!({
            "action": if opts.dry_run { "dry_run" } else { "push" },
            "target": target_store.store_path().display().to_string(),
            "roles": {
                "added": summary.roles_added,
                "updated": summary.roles_updated,
                "skipped": summary.roles_skipped,
            },
            "motivations": {
                "added": summary.tradeoffs_added,
                "updated": summary.tradeoffs_updated,
                "skipped": summary.tradeoffs_skipped,
            },
            "agents": {
                "added": summary.agents_added,
                "updated": summary.agents_updated,
                "skipped": summary.agents_skipped,
            },
            "evaluations": {
                "added": summary.evaluations_added,
                "skipped": summary.evaluations_skipped,
            },
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if opts.dry_run {
        println!("Dry run — would push to {}:", target_store.store_path().display());
        println!("{}", summary);
    } else {
        println!("Pushed to {}:", target_store.store_path().display());
        println!("{}", summary);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::agency::{self, Agent, AgencyStore, TradeoffConfig, PerformanceRecord, Role, Lineage};
    use workgraph::graph::TrustLevel;

    fn setup_store(tmp: &TempDir, name: &str) -> LocalStore {
        let path = tmp.path().join(name).join("agency");
        agency::init(&path).unwrap();
        LocalStore::new(path)
    }

    fn make_role(id: &str, name: &str) -> Role {
        Role {
            id: id.to_string(),
            name: name.to_string(),
            description: "test role".to_string(),
            component_ids: Vec::new(),
            outcome_id: "test outcome".to_string(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            default_context_scope: None,
        }
    }

    fn make_motivation(id: &str, name: &str) -> TradeoffConfig {
        TradeoffConfig {
            id: id.to_string(),
            name: name.to_string(),
            description: "test motivation".to_string(),
            acceptable_tradeoffs: Vec::new(),
            unacceptable_tradeoffs: Vec::new(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            access_control: workgraph::agency::AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        }
    }

    fn make_agent(id: &str, name: &str, role_id: &str, tradeoff_id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            role_id: role_id.to_string(),
            tradeoff_id: tradeoff_id.to_string(),
            name: name.to_string(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".to_string(),
            deployment_history: vec![],
            attractor_weight: 0.5,
            staleness_flags: vec![],
        }
    }

    fn default_opts(target: &str) -> PushOptions<'_> {
        PushOptions {
            target,
            dry_run: false,
            no_performance: false,
            no_evaluations: false,
            force: false,
            global: false,
            entity_ids: &[],
            entity_type: None,
            json: false,
        }
    }

    #[test]
    fn push_via_run_function() {
        let tmp = TempDir::new().unwrap();

        // Set up a workgraph dir as source
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        agency::init(&agency_dir).unwrap();

        let source = LocalStore::new(&agency_dir);
        source.save_role(&make_role("r1", "tester")).unwrap();
        source.save_tradeoff(&make_motivation("m1", "quality")).unwrap();

        // Target doesn't exist yet — push should create it
        let target_path = tmp.path().join("target");
        std::fs::create_dir_all(&target_path).unwrap();

        run(&wg_dir, &default_opts(target_path.to_str().unwrap())).unwrap();

        let target = LocalStore::new(target_path.join("agency"));
        assert!(target.exists_role("r1"));
        assert!(target.exists_tradeoff("m1"));
    }

    #[test]
    fn push_with_named_remote() {
        let tmp = TempDir::new().unwrap();

        // Set up workgraph dir as source
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        agency::init(&agency_dir).unwrap();

        let source = LocalStore::new(&agency_dir);
        source.save_role(&make_role("r1", "pushed-role")).unwrap();

        // Set up target store
        let target = setup_store(&tmp, "target");

        // Write federation.yaml with a named remote pointing to target
        let federation_yaml = format!(
            "remotes:\n  downstream:\n    path: \"{}\"\n    description: \"test remote\"\n",
            target.store_path().display()
        );
        std::fs::write(wg_dir.join("federation.yaml"), federation_yaml).unwrap();

        run(&wg_dir, &PushOptions {
            target: "downstream",
            no_evaluations: true,
            ..default_opts("")
        }).unwrap();

        assert!(target.exists_role("r1"));
    }

    #[test]
    fn push_invalid_type_errors() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        agency::init(&agency_dir).unwrap();

        let target_path = tmp.path().join("target");
        std::fs::create_dir_all(&target_path).unwrap();

        let result = run(&wg_dir, &PushOptions {
            target: target_path.to_str().unwrap(),
            entity_type: Some("invalid_type"),
            ..default_opts("")
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown entity type"));
    }

    #[test]
    fn push_no_local_store_errors() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join("empty").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        // Don't init agency — no roles/ dir

        let target_path = tmp.path().join("target");
        std::fs::create_dir_all(&target_path).unwrap();

        let result = run(&wg_dir, &default_opts(target_path.to_str().unwrap()));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No local agency store"));
    }

    #[test]
    fn push_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        agency::init(&agency_dir).unwrap();

        let source = LocalStore::new(&agency_dir);
        source.save_role(&make_role("r1", "dry-role")).unwrap();

        let target = setup_store(&tmp, "target");

        run(&wg_dir, &PushOptions {
            target: target.store_path().to_str().unwrap(),
            dry_run: true,
            ..default_opts("")
        }).unwrap();

        assert!(!target.exists_role("r1"));
    }

    #[test]
    fn push_type_filter_roles_only() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        agency::init(&agency_dir).unwrap();

        let source = LocalStore::new(&agency_dir);
        source.save_role(&make_role("r1", "role")).unwrap();
        source.save_tradeoff(&make_motivation("m1", "mot")).unwrap();

        let target = setup_store(&tmp, "target");

        run(&wg_dir, &PushOptions {
            target: target.store_path().to_str().unwrap(),
            entity_type: Some("role"),
            ..default_opts("")
        }).unwrap();

        assert!(target.exists_role("r1"));
        assert!(!target.exists_tradeoff("m1"));
    }

    #[test]
    fn push_agent_auto_pushes_dependencies() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        agency::init(&agency_dir).unwrap();

        let source = LocalStore::new(&agency_dir);
        source.save_role(&make_role("r1", "builder")).unwrap();
        source.save_tradeoff(&make_motivation("m1", "speed")).unwrap();
        source.save_agent(&make_agent("a1", "fast-builder", "r1", "m1")).unwrap();

        let target = setup_store(&tmp, "target");

        run(&wg_dir, &PushOptions {
            target: target.store_path().to_str().unwrap(),
            entity_type: Some("agent"),
            ..default_opts("")
        }).unwrap();

        assert!(target.exists_agent("a1"));
        assert!(target.exists_role("r1"));
        assert!(target.exists_tradeoff("m1"));
    }
}
