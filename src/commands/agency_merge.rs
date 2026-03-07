use std::path::Path;

use anyhow::Result;

use workgraph::agency::{AgencyStore, LocalStore};
use workgraph::federation::{self, TransferOptions, TransferSummary};

/// Options for the merge command.
pub struct MergeOptions {
    pub sources: Vec<String>,
    pub into: Option<String>,
    pub dry_run: bool,
    pub json: bool,
}

/// Resolve a source string, checking named remotes in federation.yaml first.
fn resolve_source(source: &str, workgraph_dir: &Path) -> Result<LocalStore> {
    federation::resolve_store_with_remotes(source, workgraph_dir)
}

/// Accumulate one transfer summary into a running total.
fn accumulate(total: &mut TransferSummary, part: &TransferSummary) {
    total.components_added += part.components_added;
    total.components_updated += part.components_updated;
    total.components_skipped += part.components_skipped;
    total.components_access_denied += part.components_access_denied;
    total.outcomes_added += part.outcomes_added;
    total.outcomes_updated += part.outcomes_updated;
    total.outcomes_skipped += part.outcomes_skipped;
    total.outcomes_access_denied += part.outcomes_access_denied;
    total.roles_added += part.roles_added;
    total.roles_updated += part.roles_updated;
    total.roles_skipped += part.roles_skipped;
    total.tradeoffs_added += part.tradeoffs_added;
    total.tradeoffs_updated += part.tradeoffs_updated;
    total.tradeoffs_skipped += part.tradeoffs_skipped;
    total.tradeoffs_access_denied += part.tradeoffs_access_denied;
    total.agents_added += part.agents_added;
    total.agents_updated += part.agents_updated;
    total.agents_skipped += part.agents_skipped;
    total.evaluations_added += part.evaluations_added;
    total.evaluations_skipped += part.evaluations_skipped;
}

pub fn run(workgraph_dir: &Path, opts: &MergeOptions) -> Result<()> {
    if opts.sources.len() < 2 {
        anyhow::bail!("Merge requires at least 2 sources");
    }

    // Determine the target store
    let target = if let Some(ref into_path) = opts.into {
        let store = federation::resolve_store(into_path)?;
        federation::ensure_store_dirs(&store)?;
        store
    } else {
        let agency_dir = workgraph_dir.join("agency");
        let store = LocalStore::new(&agency_dir);
        federation::ensure_store_dirs(&store)?;
        store
    };

    let transfer_opts = TransferOptions {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    let mut total = TransferSummary::default();
    let source_count = opts.sources.len();

    for source_ref in &opts.sources {
        let source = resolve_source(source_ref, workgraph_dir)?;
        if !source.is_valid() {
            anyhow::bail!(
                "Source is not a valid agency store: {}",
                source.store_path().display()
            );
        }
        let summary = federation::transfer(&source, &target, &transfer_opts)?;
        accumulate(&mut total, &summary);
    }

    if opts.json {
        let output = serde_json::json!({
            "action": if opts.dry_run { "dry_run" } else { "merge" },
            "source_count": source_count,
            "roles": {
                "added": total.roles_added,
                "updated": total.roles_updated,
                "skipped": total.roles_skipped,
            },
            "motivations": {
                "added": total.tradeoffs_added,
                "updated": total.tradeoffs_updated,
                "skipped": total.tradeoffs_skipped,
            },
            "agents": {
                "added": total.agents_added,
                "updated": total.agents_updated,
                "skipped": total.agents_skipped,
            },
            "evaluations": {
                "added": total.evaluations_added,
                "skipped": total.evaluations_skipped,
            },
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        let prefix = if opts.dry_run {
            "Would merge"
        } else {
            "Merged"
        };
        println!("{} from {} sources:", prefix, source_count);

        let total_roles = total.roles_added + total.roles_updated + total.roles_skipped;
        let total_motivations =
            total.tradeoffs_added + total.tradeoffs_updated + total.tradeoffs_skipped;
        let total_agents = total.agents_added + total.agents_updated + total.agents_skipped;

        println!(
            "  Role transfers: {} ({} new, {} existing)",
            total_roles,
            total.roles_added,
            total.roles_updated + total.roles_skipped
        );
        println!(
            "  Motivation transfers: {} ({} new, {} existing)",
            total_motivations,
            total.tradeoffs_added,
            total.tradeoffs_updated + total.tradeoffs_skipped
        );
        println!(
            "  Agent transfers: {} ({} new, {} existing)",
            total_agents,
            total.agents_added,
            total.agents_updated + total.agents_skipped
        );

        if total.evaluations_added > 0 || total.evaluations_skipped > 0 {
            println!(
                "  Evaluations: +{} new, {} skipped",
                total.evaluations_added, total.evaluations_skipped
            );
        }
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
    fn merge_two_stores_with_overlapping_and_unique_entities() {
        let tmp = TempDir::new().unwrap();
        let store_a = setup_store(&tmp, "store-a");
        let store_b = setup_store(&tmp, "store-b");

        // Store A: r1, r2, m1
        store_a.save_role(&make_role("r1", "shared-role")).unwrap();
        store_a.save_role(&make_role("r2", "a-only-role")).unwrap();
        store_a
            .save_tradeoff(&make_motivation("m1", "shared-mot"))
            .unwrap();

        // Store B: r1 (overlap), r3, m1 (overlap), m2
        store_b.save_role(&make_role("r1", "shared-role")).unwrap();
        store_b.save_role(&make_role("r3", "b-only-role")).unwrap();
        store_b
            .save_tradeoff(&make_motivation("m1", "shared-mot"))
            .unwrap();
        store_b
            .save_tradeoff(&make_motivation("m2", "b-only-mot"))
            .unwrap();

        // Set up a workgraph dir for the target
        let wg_dir = tmp.path().join("target").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        workgraph::agency::init(&agency_dir).unwrap();

        let opts = MergeOptions {
            sources: vec![
                store_a.store_path().to_string_lossy().to_string(),
                store_b.store_path().to_string_lossy().to_string(),
            ],
            into: None,
            dry_run: false,
            json: false,
        };

        run(&wg_dir, &opts).unwrap();

        let result = LocalStore::new(&agency_dir);
        // Should have all unique roles: r1, r2, r3
        assert!(result.exists_role("r1"));
        assert!(result.exists_role("r2"));
        assert!(result.exists_role("r3"));
        // Should have all unique motivations: m1, m2
        assert!(result.exists_tradeoff("m1"));
        assert!(result.exists_tradeoff("m2"));
    }

    #[test]
    fn merge_into_target_path() {
        let tmp = TempDir::new().unwrap();
        let store_a = setup_store(&tmp, "store-a");
        let store_b = setup_store(&tmp, "store-b");

        store_a.save_role(&make_role("r1", "role1")).unwrap();
        store_b.save_role(&make_role("r2", "role2")).unwrap();

        // --into a specific bare store path
        let into_dir = tmp.path().join("combined");
        std::fs::create_dir_all(&into_dir).unwrap();

        // Set up a minimal workgraph dir (merge needs it for resolve_source)
        let wg_dir = tmp.path().join("project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let opts = MergeOptions {
            sources: vec![
                store_a.store_path().to_string_lossy().to_string(),
                store_b.store_path().to_string_lossy().to_string(),
            ],
            into: Some(into_dir.to_string_lossy().to_string()),
            dry_run: false,
            json: false,
        };

        run(&wg_dir, &opts).unwrap();

        // The target should be at into_dir/agency/ (bare store convention)
        let result = federation::resolve_store(into_dir.to_str().unwrap()).unwrap();
        assert!(result.exists_role("r1"));
        assert!(result.exists_role("r2"));
    }

    #[test]
    fn merge_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store_a = setup_store(&tmp, "store-a");
        let store_b = setup_store(&tmp, "store-b");

        store_a.save_role(&make_role("r1", "role1")).unwrap();
        store_a
            .save_tradeoff(&make_motivation("m1", "mot1"))
            .unwrap();
        store_b.save_role(&make_role("r2", "role2")).unwrap();

        let wg_dir = tmp.path().join("target").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        workgraph::agency::init(&agency_dir).unwrap();

        let opts = MergeOptions {
            sources: vec![
                store_a.store_path().to_string_lossy().to_string(),
                store_b.store_path().to_string_lossy().to_string(),
            ],
            into: None,
            dry_run: false,
            json: false,
        };

        // First merge
        run(&wg_dir, &opts).unwrap();

        let result = LocalStore::new(&agency_dir);
        let roles_after_first = result.load_roles().unwrap();
        let mots_after_first = result.load_tradeoffs().unwrap();

        // Second merge (idempotent)
        run(&wg_dir, &opts).unwrap();

        let roles_after_second = result.load_roles().unwrap();
        let mots_after_second = result.load_tradeoffs().unwrap();

        assert_eq!(roles_after_first.len(), roles_after_second.len());
        assert_eq!(mots_after_first.len(), mots_after_second.len());

        // Verify content is identical
        for role in &roles_after_first {
            let matching = roles_after_second.iter().find(|r| r.id == role.id).unwrap();
            assert_eq!(role.name, matching.name);
            assert_eq!(role.performance.task_count, matching.performance.task_count);
        }
    }

    #[test]
    fn merge_performance_records_unioned() {
        let tmp = TempDir::new().unwrap();
        let store_a = setup_store(&tmp, "store-a");
        let store_b = setup_store(&tmp, "store-b");

        // Same role in both stores but with different evaluations
        let mut role_a = make_role("r1", "shared-role");
        role_a.performance = PerformanceRecord {
            task_count: 1,
            avg_score: Some(0.8),
            evaluations: vec![EvaluationRef {
                score: 0.8,
                task_id: "task-1".to_string(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                context_id: "ctx-1".to_string(),
                cost_usd: None,
            }],
        };
        store_a.save_role(&role_a).unwrap();

        let mut role_b = make_role("r1", "shared-role");
        role_b.performance = PerformanceRecord {
            task_count: 1,
            avg_score: Some(0.9),
            evaluations: vec![EvaluationRef {
                score: 0.9,
                task_id: "task-2".to_string(),
                timestamp: "2026-01-02T00:00:00Z".to_string(),
                context_id: "ctx-2".to_string(),
                cost_usd: None,
            }],
        };
        store_b.save_role(&role_b).unwrap();

        let wg_dir = tmp.path().join("target").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        workgraph::agency::init(&agency_dir).unwrap();

        let opts = MergeOptions {
            sources: vec![
                store_a.store_path().to_string_lossy().to_string(),
                store_b.store_path().to_string_lossy().to_string(),
            ],
            into: None,
            dry_run: false,
            json: false,
        };

        run(&wg_dir, &opts).unwrap();

        let result = LocalStore::new(&agency_dir);
        let roles = result.load_roles().unwrap();
        let merged = roles.iter().find(|r| r.id == "r1").unwrap();

        // Both evaluations should be present (union)
        assert_eq!(merged.performance.evaluations.len(), 2);
        assert_eq!(merged.performance.task_count, 2);
    }

    #[test]
    fn merge_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let store_a = setup_store(&tmp, "store-a");
        let store_b = setup_store(&tmp, "store-b");

        store_a.save_role(&make_role("r1", "role1")).unwrap();
        store_b.save_role(&make_role("r2", "role2")).unwrap();

        let wg_dir = tmp.path().join("target").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        workgraph::agency::init(&agency_dir).unwrap();

        let opts = MergeOptions {
            sources: vec![
                store_a.store_path().to_string_lossy().to_string(),
                store_b.store_path().to_string_lossy().to_string(),
            ],
            into: None,
            dry_run: true,
            json: false,
        };

        run(&wg_dir, &opts).unwrap();

        let result = LocalStore::new(&agency_dir);
        assert!(!result.exists_role("r1"));
        assert!(!result.exists_role("r2"));
    }

    #[test]
    fn merge_requires_at_least_two_sources() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let opts = MergeOptions {
            sources: vec!["one-source".to_string()],
            into: None,
            dry_run: false,
            json: false,
        };

        let result = run(&wg_dir, &opts);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("at least 2 sources")
        );
    }

    #[test]
    fn merge_with_agents_pulls_dependencies() {
        let tmp = TempDir::new().unwrap();
        let store_a = setup_store(&tmp, "store-a");
        let store_b = setup_store(&tmp, "store-b");

        // Store A has role + motivation + agent
        store_a.save_role(&make_role("r1", "builder")).unwrap();
        store_a
            .save_tradeoff(&make_motivation("m1", "speed"))
            .unwrap();
        store_a
            .save_agent(&make_agent("a1", "fast-builder", "r1", "m1"))
            .unwrap();

        // Store B has a different role + motivation + agent
        store_b.save_role(&make_role("r2", "tester")).unwrap();
        store_b
            .save_tradeoff(&make_motivation("m2", "quality"))
            .unwrap();
        store_b
            .save_agent(&make_agent("a2", "quality-tester", "r2", "m2"))
            .unwrap();

        let wg_dir = tmp.path().join("target").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let agency_dir = wg_dir.join("agency");
        workgraph::agency::init(&agency_dir).unwrap();

        let opts = MergeOptions {
            sources: vec![
                store_a.store_path().to_string_lossy().to_string(),
                store_b.store_path().to_string_lossy().to_string(),
            ],
            into: None,
            dry_run: false,
            json: false,
        };

        run(&wg_dir, &opts).unwrap();

        let result = LocalStore::new(&agency_dir);
        assert!(result.exists_agent("a1"));
        assert!(result.exists_agent("a2"));
        assert!(result.exists_role("r1"));
        assert!(result.exists_role("r2"));
        assert!(result.exists_tradeoff("m1"));
        assert!(result.exists_tradeoff("m2"));
    }
}
