use std::path::Path;

use anyhow::{Context, Result};

use workgraph::agency::{AgencyStore, LocalStore};
use workgraph::federation::{self, EntityFilter, TransferOptions};

/// Options for the pull command.
pub struct PullOptions {
    pub source: String,
    pub dry_run: bool,
    pub no_performance: bool,
    pub no_evaluations: bool,
    pub force: bool,
    pub global: bool,
    pub entity_ids: Vec<String>,
    pub entity_type: Option<String>,
    pub json: bool,
}

/// Resolve a source string, checking named remotes in federation.yaml first.
fn resolve_source(source: &str, workgraph_dir: &Path) -> Result<LocalStore> {
    federation::resolve_store_with_remotes(source, workgraph_dir)
}

/// Get the local (target) store, creating dirs if needed.
fn local_store(workgraph_dir: &Path, global: bool) -> Result<LocalStore> {
    let path = if global {
        let home = dirs::home_dir().context("Cannot determine home directory")?;
        home.join(".workgraph").join("agency")
    } else {
        workgraph_dir.join("agency")
    };
    let store = LocalStore::new(&path);
    federation::ensure_store_dirs(&store)?;
    Ok(store)
}

fn parse_entity_filter(entity_type: Option<&str>) -> Result<EntityFilter> {
    match entity_type {
        Some("component" | "components") => Ok(EntityFilter::Components),
        Some("outcome" | "outcomes") => Ok(EntityFilter::Outcomes),
        Some("role" | "roles") => Ok(EntityFilter::Roles),
        Some("motivation" | "motivations" | "tradeoff" | "tradeoffs") => {
            Ok(EntityFilter::Tradeoffs)
        }
        Some("agent" | "agents") => Ok(EntityFilter::Agents),
        Some(other) => anyhow::bail!(
            "Unknown entity type '{}'. Use: component, outcome, role, tradeoff, motivation, or agent",
            other
        ),
        None => Ok(EntityFilter::All),
    }
}

pub fn run(workgraph_dir: &Path, opts: &PullOptions) -> Result<()> {
    let source = resolve_source(&opts.source, workgraph_dir)?;

    if !source.is_valid() {
        anyhow::bail!(
            "Source is not a valid agency store: {}",
            source.store_path().display()
        );
    }

    let target = local_store(workgraph_dir, opts.global)?;

    let transfer_opts = TransferOptions {
        dry_run: opts.dry_run,
        no_performance: opts.no_performance,
        no_evaluations: opts.no_evaluations,
        force: opts.force,
        entity_ids: opts.entity_ids.clone(),
        entity_filter: parse_entity_filter(opts.entity_type.as_deref())?,
    };

    let summary = federation::transfer(&source, &target, &transfer_opts)?;

    // Update last_sync if the source was a named remote
    if !opts.dry_run {
        let _ = federation::touch_remote_sync(workgraph_dir, &opts.source);
    }

    if opts.json {
        let output = serde_json::json!({
            "action": if opts.dry_run { "dry_run" } else { "pull" },
            "source": source.store_path().display().to_string(),
            "target": if opts.global {
                "~/.workgraph/agency/".to_string()
            } else {
                target.store_path().display().to_string()
            },
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
    } else {
        let prefix = if opts.dry_run { "Would pull" } else { "Pulled" };
        let target_desc = if opts.global {
            "~/.workgraph/agency/".to_string()
        } else {
            target.store_path().display().to_string()
        };

        println!(
            "{} from {} into {}:",
            prefix,
            source.store_path().display(),
            target_desc,
        );
        println!("{}", summary);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::agency::{
        AgencyStore, Agent, EvaluationRef, Lineage, PerformanceRecord, Role, TradeoffConfig,
    };
    use workgraph::graph::TrustLevel;

    fn setup_store(tmp: &TempDir, name: &str) -> LocalStore {
        let path = tmp.path().join(name).join("agency");
        workgraph::agency::init(&path).unwrap();
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
            default_exec_mode: None,
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

    #[test]
    fn pull_new_entities_into_empty_store() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        source.save_role(&make_role("r1", "analyst")).unwrap();
        source
            .save_tradeoff(&make_motivation("m1", "quality"))
            .unwrap();

        let opts = TransferOptions::default();
        let summary = federation::transfer(&source, &target, &opts).unwrap();

        assert_eq!(summary.roles_added, 1);
        assert_eq!(summary.tradeoffs_added, 1);
        assert!(target.exists_role("r1"));
        assert!(target.exists_tradeoff("m1"));
    }

    #[test]
    fn pull_with_metadata_merge() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        // Target has a role with 1 evaluation
        let mut target_role = make_role("r1", "analyst");
        target_role.performance = PerformanceRecord {
            task_count: 1,
            avg_score: Some(0.8),
            evaluations: vec![EvaluationRef {
                score: 0.8,
                task_id: "task-1".to_string(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                context_id: "mot-1".to_string(),
                cost_usd: None,
            }],
            avg_cost_usd: None,
            total_cost_usd: 0.0,
        };
        target.save_role(&target_role).unwrap();

        // Source has same role with a different evaluation
        let mut source_role = make_role("r1", "analyst");
        source_role.performance = PerformanceRecord {
            task_count: 1,
            avg_score: Some(0.9),
            evaluations: vec![EvaluationRef {
                score: 0.9,
                task_id: "task-2".to_string(),
                timestamp: "2026-01-02T00:00:00Z".to_string(),
                context_id: "mot-2".to_string(),
                cost_usd: None,
            }],
            avg_cost_usd: None,
            total_cost_usd: 0.0,
        };
        source.save_role(&source_role).unwrap();

        let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
        assert_eq!(summary.roles_updated, 1);

        let roles = target.load_roles().unwrap();
        let merged = roles.iter().find(|r| r.id == "r1").unwrap();
        assert_eq!(merged.performance.task_count, 2);
        assert_eq!(merged.performance.evaluations.len(), 2);
    }

    #[test]
    fn pull_agent_auto_pulls_dependencies() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        source.save_role(&make_role("r1", "builder")).unwrap();
        source
            .save_tradeoff(&make_motivation("m1", "speed"))
            .unwrap();
        source
            .save_agent(&make_agent("a1", "fast-builder", "r1", "m1"))
            .unwrap();

        // Pull only agents — should auto-include role and motivation
        let opts = TransferOptions {
            entity_filter: EntityFilter::Agents,
            ..Default::default()
        };
        let summary = federation::transfer(&source, &target, &opts).unwrap();

        assert_eq!(summary.agents_added, 1);
        assert_eq!(summary.roles_added, 1);
        assert_eq!(summary.tradeoffs_added, 1);
        assert!(target.exists_role("r1"));
        assert!(target.exists_tradeoff("m1"));
    }

    #[test]
    fn dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        source.save_role(&make_role("r1", "tester")).unwrap();

        let opts = TransferOptions {
            dry_run: true,
            ..Default::default()
        };
        let summary = federation::transfer(&source, &target, &opts).unwrap();
        assert_eq!(summary.roles_added, 1);
        assert!(!target.exists_role("r1"));
    }

    #[test]
    fn no_performance_strips_scores() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        let mut role = make_role("r1", "scorer");
        role.performance = PerformanceRecord {
            task_count: 5,
            avg_score: Some(0.95),
            evaluations: vec![EvaluationRef {
                score: 0.95,
                task_id: "task-x".to_string(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                context_id: "mot-x".to_string(),
                cost_usd: None,
            }],
            avg_cost_usd: None,
            total_cost_usd: 0.0,
        };
        source.save_role(&role).unwrap();

        let opts = TransferOptions {
            no_performance: true,
            ..Default::default()
        };
        federation::transfer(&source, &target, &opts).unwrap();

        let roles = target.load_roles().unwrap();
        let saved = roles.iter().find(|r| r.id == "r1").unwrap();
        assert_eq!(saved.performance.task_count, 0);
        assert!(saved.performance.avg_score.is_none());
    }

    #[test]
    fn pull_via_run_function() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");

        source.save_role(&make_role("r1", "tester")).unwrap();
        source
            .save_tradeoff(&make_motivation("m1", "quality"))
            .unwrap();

        // Set up a workgraph dir for the target
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        workgraph::agency::init(&agency_dir).unwrap();

        let opts = PullOptions {
            source: source.store_path().to_string_lossy().to_string(),
            dry_run: false,
            no_performance: false,
            no_evaluations: false,
            force: false,
            global: false,
            entity_ids: Vec::new(),
            entity_type: None,
            json: false,
        };

        // The run function expects workgraph_dir to be the .workgraph dir
        run(&wg_dir, &opts).unwrap();

        let result = LocalStore::new(&agency_dir);
        assert!(result.exists_role("r1"));
        assert!(result.exists_tradeoff("m1"));
    }

    #[test]
    fn pull_with_named_remote() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        source.save_role(&make_role("r1", "remote-role")).unwrap();

        // Set up workgraph dir with federation.yaml
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        workgraph::agency::init(&agency_dir).unwrap();

        // Write federation.yaml with a named remote
        let federation_yaml = format!(
            "remotes:\n  upstream:\n    path: \"{}\"\n    description: \"test remote\"\n",
            source.store_path().display()
        );
        std::fs::write(wg_dir.join("federation.yaml"), federation_yaml).unwrap();

        let opts = PullOptions {
            source: "upstream".to_string(),
            dry_run: false,
            no_performance: false,
            no_evaluations: true,
            force: false,
            global: false,
            entity_ids: Vec::new(),
            entity_type: None,
            json: false,
        };

        run(&wg_dir, &opts).unwrap();

        let result = LocalStore::new(&agency_dir);
        assert!(result.exists_role("r1"));
    }

    #[test]
    fn pull_entity_filter_by_type() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        source.save_role(&make_role("r1", "role")).unwrap();
        source.save_tradeoff(&make_motivation("m1", "mot")).unwrap();

        // Pull only roles
        let opts = TransferOptions {
            entity_filter: EntityFilter::Roles,
            ..Default::default()
        };
        let summary = federation::transfer(&source, &target, &opts).unwrap();
        assert_eq!(summary.roles_added, 1);
        assert_eq!(summary.tradeoffs_added, 0);
        assert!(target.exists_role("r1"));
        assert!(!target.exists_tradeoff("m1"));
    }

    #[test]
    fn pull_entity_filter_by_id() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        source.save_role(&make_role("r1", "role1")).unwrap();
        source.save_role(&make_role("r2", "role2")).unwrap();

        let opts = TransferOptions {
            entity_ids: vec!["r1".to_string()],
            ..Default::default()
        };
        let summary = federation::transfer(&source, &target, &opts).unwrap();
        assert_eq!(summary.roles_added, 1);
        assert!(target.exists_role("r1"));
        assert!(!target.exists_role("r2"));
    }
}
