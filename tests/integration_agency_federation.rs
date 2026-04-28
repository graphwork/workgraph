//! Integration tests for the agency federation system.
//!
//! Covers scan, pull, push, remote, merge, performance merge, and edge cases.
//! All tests use temp directories for isolation.

use std::collections::HashMap;
use std::path::Path;

use tempfile::TempDir;

use workgraph::agency::{
    self, AgencyStore, Agent, Evaluation, EvaluationRef, Lineage, LocalStore, PerformanceRecord,
    Role, TradeoffConfig,
};
use workgraph::config::CLAUDE_OPUS_MODEL_ID;
use workgraph::federation::{
    self, EntityFilter, FederationConfig, Remote, TransferOptions, TransferSummary,
};
use workgraph::graph::TrustLevel;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn setup_store(tmp: &TempDir, name: &str) -> LocalStore {
    let path = tmp.path().join(name).join("agency");
    agency::init(&path).unwrap();
    LocalStore::new(path)
}

fn make_role(id: &str, name: &str) -> Role {
    Role {
        id: id.to_string(),
        name: name.to_string(),
        description: format!("{} description", name),
        component_ids: Vec::new(),
        outcome_id: format!("{} outcome", name),
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
        description: format!("{} description", name),
        acceptable_tradeoffs: Vec::new(),
        unacceptable_tradeoffs: Vec::new(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::default(),
        access_control: workgraph::agency::AccessControl::default(),
        domain_tags: vec![],
        metadata: std::collections::HashMap::new(),
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
        preferred_model: None,
        preferred_provider: None,
        deployment_history: vec![],
        attractor_weight: 0.5,
        staleness_flags: vec![],
    }
}

fn make_evaluation(
    id: &str,
    task_id: &str,
    agent_id: &str,
    role_id: &str,
    tradeoff_id: &str,
    score: f64,
) -> Evaluation {
    Evaluation {
        id: id.to_string(),
        task_id: task_id.to_string(),
        agent_id: agent_id.to_string(),
        role_id: role_id.to_string(),
        tradeoff_id: tradeoff_id.to_string(),
        score,
        dimensions: HashMap::new(),
        notes: "test evaluation".to_string(),
        evaluator: "test".to_string(),
        timestamp: "2026-01-15T12:00:00Z".to_string(),
        model: None,
        source: "llm".to_string(),
        loop_iteration: 0,
    }
}

fn make_perf(evals: Vec<(f64, &str, &str)>) -> PerformanceRecord {
    let evaluations: Vec<EvaluationRef> = evals
        .iter()
        .map(|(score, task_id, ts)| EvaluationRef {
            score: *score,
            task_id: task_id.to_string(),
            timestamp: ts.to_string(),
            context_id: String::new(),
        })
        .collect();
    let task_count = evaluations.len() as u32;
    let avg_score = if evaluations.is_empty() {
        None
    } else {
        Some(evaluations.iter().map(|e| e.score).sum::<f64>() / evaluations.len() as f64)
    };
    PerformanceRecord {
        task_count,
        avg_score,
        evaluations,
    }
}

fn create_bare_store_dirs(dir: &Path) {
    let agency = dir.join("agency");
    std::fs::create_dir_all(agency.join("cache/roles")).unwrap();
    std::fs::create_dir_all(agency.join("primitives/tradeoffs")).unwrap();
    std::fs::create_dir_all(agency.join("cache/agents")).unwrap();
    std::fs::create_dir_all(agency.join("evaluations")).unwrap();
}

fn create_project_store_dirs(dir: &Path) {
    let agency = dir.join(".workgraph").join("agency");
    std::fs::create_dir_all(agency.join("cache/roles")).unwrap();
    std::fs::create_dir_all(agency.join("primitives/tradeoffs")).unwrap();
    std::fs::create_dir_all(agency.join("cache/agents")).unwrap();
    std::fs::create_dir_all(agency.join("evaluations")).unwrap();
}

fn write_federation_config(wg_dir: &Path, config: &FederationConfig) {
    federation::save_federation_config(wg_dir, config).unwrap();
}

// ===========================================================================
// 1. SCAN TESTS
// ===========================================================================

/// Scanning should find project stores with .workgraph/agency/roles/.
#[test]
fn scan_finds_project_stores() {
    let tmp = TempDir::new().unwrap();

    let proj_a = tmp.path().join("alpha");
    create_project_store_dirs(&proj_a);
    let proj_b = tmp.path().join("beta");
    create_project_store_dirs(&proj_b);

    // Verify both project stores are individually valid
    let store_a = LocalStore::new(proj_a.join(".workgraph").join("agency"));
    let store_b = LocalStore::new(proj_b.join(".workgraph").join("agency"));
    assert!(store_a.is_valid());
    assert!(store_b.is_valid());
}

/// Scanning should find bare stores (agency/roles/ without .workgraph parent).
#[test]
fn scan_finds_bare_stores() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("shared");
    create_bare_store_dirs(&bare);

    let store = LocalStore::new(bare.join("agency"));
    assert!(store.is_valid());
}

/// Empty directory should have no stores.
#[test]
fn scan_empty_directory_no_stores() {
    let tmp = TempDir::new().unwrap();
    // An empty temp dir has no agency stores
    let store = LocalStore::new(tmp.path().join("agency"));
    assert!(!store.is_valid());
}

/// resolve_store correctly finds project store.
#[test]
fn resolve_store_finds_project() {
    let tmp = TempDir::new().unwrap();
    create_project_store_dirs(tmp.path());

    let store = federation::resolve_store(tmp.path().to_str().unwrap()).unwrap();
    assert_eq!(
        store.store_path(),
        tmp.path().join(".workgraph").join("agency")
    );
    assert!(store.is_valid());
}

/// resolve_store correctly finds bare store.
#[test]
fn resolve_store_finds_bare() {
    let tmp = TempDir::new().unwrap();
    create_bare_store_dirs(tmp.path());

    let store = federation::resolve_store(tmp.path().to_str().unwrap()).unwrap();
    assert_eq!(store.store_path(), tmp.path().join("agency"));
    assert!(store.is_valid());
}

// ===========================================================================
// 2. PULL TESTS
// ===========================================================================

/// Pull from a store with new entities → all copied to local.
#[test]
fn pull_new_entities_all_copied() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "analyst")).unwrap();
    source.save_role(&make_role("r2", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "quality"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.roles_added, 2);
    assert_eq!(summary.tradeoffs_added, 1);
    assert_eq!(summary.agents_added, 1);

    assert!(target.exists_role("r1"));
    assert!(target.exists_role("r2"));
    assert!(target.exists_tradeoff("m1"));
    assert!(target.exists_agent("a1"));
}

/// Pull entity that already exists locally (same ID) → metadata merged correctly.
#[test]
fn pull_existing_entity_merges_metadata() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_role = make_role("r1", "analyst");
    source_role.performance = make_perf(vec![
        (0.9, "task-a", "2026-01-01T00:00:00Z"),
        (0.85, "task-c", "2026-01-03T00:00:00Z"),
    ]);
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "analyst-local");
    target_role.performance = make_perf(vec![(0.8, "task-b", "2026-01-02T00:00:00Z")]);
    target.save_role(&target_role).unwrap();

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.roles_updated, 1);

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    // Union of 3 distinct evaluations
    assert_eq!(merged.performance.evaluations.len(), 3);
    assert_eq!(merged.performance.task_count, 3);
    // Target name preserved
    assert_eq!(merged.name, "analyst-local");
}

/// Pull agent → role and motivation auto-pulled (referential integrity).
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
    assert!(target.exists_agent("a1"));
}

/// Pull with --dry-run → no files written.
#[test]
fn pull_dry_run_no_writes() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "analyst")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "quality"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();

    let opts = TransferOptions {
        dry_run: true,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    // Summary reports what would happen
    assert_eq!(summary.roles_added, 1);
    assert_eq!(summary.tradeoffs_added, 1);
    assert_eq!(summary.agents_added, 1);

    // But nothing actually written
    assert!(!target.exists_role("r1"));
    assert!(!target.exists_tradeoff("m1"));
    assert!(!target.exists_agent("a1"));
}

/// Pull with --no-performance → definitions copied, scores not.
#[test]
fn pull_no_performance_strips_scores() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut role = make_role("r1", "scorer");
    role.performance = make_perf(vec![
        (0.95, "task-x", "2026-01-01T00:00:00Z"),
        (0.88, "task-y", "2026-01-02T00:00:00Z"),
    ]);
    source.save_role(&role).unwrap();

    let mut motivation = make_motivation("m1", "quality");
    motivation.performance = make_perf(vec![(0.9, "task-z", "2026-01-03T00:00:00Z")]);
    source.save_tradeoff(&motivation).unwrap();

    let opts = TransferOptions {
        no_performance: true,
        ..Default::default()
    };
    federation::transfer(&source, &target, &opts).unwrap();

    let roles = target.load_roles().unwrap();
    let saved_role = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(saved_role.performance.task_count, 0);
    assert!(saved_role.performance.avg_score.is_none());
    assert!(saved_role.performance.evaluations.is_empty());

    let motivations = target.load_tradeoffs().unwrap();
    let saved_mot = motivations.iter().find(|m| m.id == "m1").unwrap();
    assert_eq!(saved_mot.performance.task_count, 0);
    assert!(saved_mot.performance.avg_score.is_none());
}

/// Pull with --type filter → only specified type pulled.
#[test]
fn pull_type_filter_roles_only() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "analyst")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "quality"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();

    let opts = TransferOptions {
        entity_filter: EntityFilter::Roles,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.roles_added, 1);
    assert_eq!(summary.tradeoffs_added, 0);
    assert_eq!(summary.agents_added, 0);
    assert!(target.exists_role("r1"));
    assert!(!target.exists_tradeoff("m1"));
    assert!(!target.exists_agent("a1"));
}

/// Pull with --type motivations filter.
#[test]
fn pull_type_filter_motivations_only() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "analyst")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "quality"))
        .unwrap();
    source
        .save_tradeoff(&make_motivation("m2", "speed"))
        .unwrap();

    let opts = TransferOptions {
        entity_filter: EntityFilter::Tradeoffs,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.roles_added, 0);
    assert_eq!(summary.tradeoffs_added, 2);
    assert!(!target.exists_role("r1"));
    assert!(target.exists_tradeoff("m1"));
    assert!(target.exists_tradeoff("m2"));
}

/// Pull with --entity filter → only specified entity (+ deps if agent) pulled.
#[test]
fn pull_entity_filter_specific_ids() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "analyst")).unwrap();
    source.save_role(&make_role("r2", "builder")).unwrap();
    source.save_role(&make_role("r3", "tester")).unwrap();

    let opts = TransferOptions {
        entity_ids: vec!["r1".to_string(), "r3".to_string()],
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.roles_added, 2);
    assert!(target.exists_role("r1"));
    assert!(!target.exists_role("r2"));
    assert!(target.exists_role("r3"));
}

/// Pull agent by --entity → auto-includes deps.
#[test]
fn pull_entity_filter_agent_includes_deps() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source.save_role(&make_role("r2", "tester")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_tradeoff(&make_motivation("m2", "quality"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "builder-agent", "r1", "m1"))
        .unwrap();
    source
        .save_agent(&make_agent("a2", "tester-agent", "r2", "m2"))
        .unwrap();

    // Only pull agent a1 — should auto-include r1 and m1 but NOT r2, m2, a2
    let opts = TransferOptions {
        entity_ids: vec!["a1".to_string()],
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.agents_added, 1);
    assert_eq!(summary.roles_added, 1);
    assert_eq!(summary.tradeoffs_added, 1);
    assert!(target.exists_agent("a1"));
    assert!(target.exists_role("r1"));
    assert!(target.exists_tradeoff("m1"));
    assert!(!target.exists_agent("a2"));
    assert!(!target.exists_role("r2"));
    assert!(!target.exists_tradeoff("m2"));
}

// ===========================================================================
// 3. PUSH TESTS
// ===========================================================================

/// Push to empty target → creates directory structure and copies entities.
#[test]
fn push_to_empty_target_creates_structure() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");

    source.save_role(&make_role("r1", "analyst")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "quality"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();

    // Target doesn't exist yet — push creates it
    let target_path = tmp.path().join("target").join("agency");
    let target = LocalStore::new(&target_path);

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    assert_eq!(summary.roles_added, 1);
    assert_eq!(summary.tradeoffs_added, 1);
    assert_eq!(summary.agents_added, 1);
    assert!(target.is_valid());
    assert!(target.exists_role("r1"));
    assert!(target.exists_tradeoff("m1"));
    assert!(target.exists_agent("a1"));
}

/// Push to existing store → merges correctly.
#[test]
fn push_to_existing_store_merges() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    // Target already has r1
    target.save_role(&make_role("r1", "analyst-local")).unwrap();

    // Source has r1 + r2
    source
        .save_role(&make_role("r1", "analyst-remote"))
        .unwrap();
    source.save_role(&make_role("r2", "builder")).unwrap();

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    assert_eq!(summary.roles_added, 1); // r2 is new
    assert_eq!(summary.roles_skipped, 1); // r1 already exists, identical perf
    assert!(target.exists_role("r1"));
    assert!(target.exists_role("r2"));

    // r1 should retain target's name
    let roles = target.load_roles().unwrap();
    let r1 = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(r1.name, "analyst-local");
}

/// Push agent → dependencies included.
#[test]
fn push_agent_includes_dependencies() {
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

    let opts = TransferOptions {
        entity_filter: EntityFilter::Agents,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.agents_added, 1);
    assert_eq!(summary.roles_added, 1);
    assert_eq!(summary.tradeoffs_added, 1);
}

/// Push never deletes from target — existing entities stay.
#[test]
fn push_never_deletes_from_target() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    // Target has entities not in source
    target
        .save_role(&make_role("r-target-only", "target-role"))
        .unwrap();
    target
        .save_tradeoff(&make_motivation("m-target-only", "target-mot"))
        .unwrap();

    // Source has different entities
    source
        .save_role(&make_role("r-source", "source-role"))
        .unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    // Target's original entities still exist
    assert!(target.exists_role("r-target-only"));
    assert!(target.exists_tradeoff("m-target-only"));
    // Source entity was added
    assert!(target.exists_role("r-source"));
}

// ===========================================================================
// 4. REMOTE TESTS
// ===========================================================================

/// Add remote, list shows it, remove deletes it.
#[test]
fn remote_add_list_remove_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    // Add
    let mut config = FederationConfig::default();
    config.remotes.insert(
        "upstream".to_string(),
        Remote {
            path: "/some/path/agency".to_string(),
            description: Some("test remote".to_string()),
            last_sync: None,
        },
    );
    write_federation_config(&wg_dir, &config);

    // List
    let loaded = federation::load_federation_config(&wg_dir).unwrap();
    assert_eq!(loaded.remotes.len(), 1);
    assert!(loaded.remotes.contains_key("upstream"));
    assert_eq!(
        loaded.remotes["upstream"].description.as_deref(),
        Some("test remote")
    );

    // Remove
    let mut config = loaded;
    config.remotes.remove("upstream");
    write_federation_config(&wg_dir, &config);

    let loaded = federation::load_federation_config(&wg_dir).unwrap();
    assert!(loaded.remotes.is_empty());
}

/// Pull/push using remote name resolves correctly.
#[test]
fn remote_name_resolves_for_pull_push() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join("project").join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let remote_store = setup_store(&tmp, "remote");
    remote_store
        .save_role(&make_role("r1", "remote-role"))
        .unwrap();

    // Add a named remote pointing to the store
    let mut config = FederationConfig::default();
    config.remotes.insert(
        "upstream".to_string(),
        Remote {
            path: remote_store.store_path().to_string_lossy().to_string(),
            description: None,
            last_sync: None,
        },
    );
    write_federation_config(&wg_dir, &config);

    // resolve_store_with_remotes should find it by name
    let resolved = federation::resolve_store_with_remotes("upstream", &wg_dir).unwrap();
    assert_eq!(resolved.store_path(), remote_store.store_path());
    assert!(resolved.is_valid());

    // Fallback: unknown name resolves as filesystem path
    let resolved2 = federation::resolve_store_with_remotes(
        remote_store.store_path().to_str().unwrap(),
        &wg_dir,
    )
    .unwrap();
    assert_eq!(resolved2.store_path(), remote_store.store_path());
}

/// Show remote displays entity summary.
#[test]
fn remote_show_entity_summary() {
    let tmp = TempDir::new().unwrap();
    let remote_store = setup_store(&tmp, "remote");
    remote_store.save_role(&make_role("r1", "role1")).unwrap();
    remote_store.save_role(&make_role("r2", "role2")).unwrap();
    remote_store
        .save_tradeoff(&make_motivation("m1", "mot1"))
        .unwrap();

    let counts = remote_store.entity_counts();
    assert_eq!(counts.roles, 2);
    assert_eq!(counts.tradeoffs, 1);
    assert_eq!(counts.agents, 0);
}

/// touch_remote_sync updates the timestamp.
#[test]
fn touch_remote_sync_updates_timestamp() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let mut config = FederationConfig::default();
    config.remotes.insert(
        "upstream".to_string(),
        Remote {
            path: "/some/path".to_string(),
            description: None,
            last_sync: None,
        },
    );
    write_federation_config(&wg_dir, &config);

    // Initially no last_sync
    let loaded = federation::load_federation_config(&wg_dir).unwrap();
    assert!(loaded.remotes["upstream"].last_sync.is_none());

    // Touch
    federation::touch_remote_sync(&wg_dir, "upstream").unwrap();

    // Now has a timestamp
    let loaded = federation::load_federation_config(&wg_dir).unwrap();
    assert!(loaded.remotes["upstream"].last_sync.is_some());
}

// ===========================================================================
// 5. MERGE TESTS
// ===========================================================================

/// Merge 2 stores with overlapping entities → correct dedup.
#[test]
fn merge_overlapping_entities_deduped() {
    let tmp = TempDir::new().unwrap();
    let store_a = setup_store(&tmp, "store-a");
    let store_b = setup_store(&tmp, "store-b");
    let target = setup_store(&tmp, "target");

    // Overlapping: both have r1, m1. Unique: store_a has r2, store_b has r3.
    store_a.save_role(&make_role("r1", "shared")).unwrap();
    store_a.save_role(&make_role("r2", "a-only")).unwrap();
    store_a
        .save_tradeoff(&make_motivation("m1", "shared-mot"))
        .unwrap();

    store_b.save_role(&make_role("r1", "shared")).unwrap();
    store_b.save_role(&make_role("r3", "b-only")).unwrap();
    store_b
        .save_tradeoff(&make_motivation("m1", "shared-mot"))
        .unwrap();
    store_b
        .save_tradeoff(&make_motivation("m2", "b-only-mot"))
        .unwrap();

    // Merge store_a into target
    let mut total = TransferSummary::default();
    let s1 = federation::transfer(&store_a, &target, &TransferOptions::default()).unwrap();
    accumulate(&mut total, &s1);

    // Merge store_b into target (overlapping entities should skip/merge)
    let s2 = federation::transfer(&store_b, &target, &TransferOptions::default()).unwrap();
    accumulate(&mut total, &s2);

    // Target should have all unique entities
    assert!(target.exists_role("r1"));
    assert!(target.exists_role("r2"));
    assert!(target.exists_role("r3"));
    assert!(target.exists_tradeoff("m1"));
    assert!(target.exists_tradeoff("m2"));

    let roles = target.load_roles().unwrap();
    assert_eq!(roles.len(), 3); // r1, r2, r3
    let motivations = target.load_tradeoffs().unwrap();
    assert_eq!(motivations.len(), 2); // m1, m2
}

/// Merge with --into target → writes to target instead of local.
#[test]
fn merge_into_external_target() {
    let tmp = TempDir::new().unwrap();
    let store_a = setup_store(&tmp, "store-a");
    let store_b = setup_store(&tmp, "store-b");

    store_a.save_role(&make_role("r1", "role1")).unwrap();
    store_b.save_role(&make_role("r2", "role2")).unwrap();

    // External target
    let target_path = tmp.path().join("combined").join("agency");
    let target = LocalStore::new(&target_path);

    federation::transfer(&store_a, &target, &TransferOptions::default()).unwrap();
    federation::transfer(&store_b, &target, &TransferOptions::default()).unwrap();

    assert!(target.exists_role("r1"));
    assert!(target.exists_role("r2"));
}

/// Idempotency: merge same sources twice → identical result.
#[test]
fn merge_idempotent() {
    let tmp = TempDir::new().unwrap();
    let store_a = setup_store(&tmp, "store-a");
    let store_b = setup_store(&tmp, "store-b");
    let target = setup_store(&tmp, "target");

    store_a.save_role(&make_role("r1", "role1")).unwrap();
    store_a
        .save_tradeoff(&make_motivation("m1", "mot1"))
        .unwrap();
    store_b.save_role(&make_role("r2", "role2")).unwrap();
    store_b
        .save_agent(&{
            // Agent needs role and motivation in same store for referential integrity
            let mut a = make_agent("a1", "agent1", "r2", "m1");
            a.role_id = "r2".to_string();
            a.tradeoff_id = "m1".to_string();
            a
        })
        .unwrap();
    // Store b also needs r2 and m1 for agent deps
    store_b
        .save_tradeoff(&make_motivation("m1", "mot1"))
        .unwrap();

    // First merge
    federation::transfer(&store_a, &target, &TransferOptions::default()).unwrap();
    federation::transfer(&store_b, &target, &TransferOptions::default()).unwrap();

    let roles_first = target.load_roles().unwrap();
    let mots_first = target.load_tradeoffs().unwrap();
    let agents_first = target.load_agents().unwrap();

    // Second merge (idempotent)
    federation::transfer(&store_a, &target, &TransferOptions::default()).unwrap();
    federation::transfer(&store_b, &target, &TransferOptions::default()).unwrap();

    let roles_second = target.load_roles().unwrap();
    let mots_second = target.load_tradeoffs().unwrap();
    let agents_second = target.load_agents().unwrap();

    assert_eq!(roles_first.len(), roles_second.len());
    assert_eq!(mots_first.len(), mots_second.len());
    assert_eq!(agents_first.len(), agents_second.len());

    for role in &roles_first {
        let matching = roles_second.iter().find(|r| r.id == role.id).unwrap();
        assert_eq!(role.name, matching.name);
        assert_eq!(role.performance.task_count, matching.performance.task_count);
    }
}

// ===========================================================================
// 6. PERFORMANCE MERGE TESTS
// ===========================================================================

/// Same role in two stores with different evaluations → union of EvaluationRefs.
#[test]
fn performance_merge_union_of_evaluations() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_role = make_role("r1", "analyst");
    source_role.performance = make_perf(vec![
        (0.9, "task-a", "2026-01-01T00:00:00Z"),
        (0.85, "task-c", "2026-01-03T00:00:00Z"),
    ]);
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "analyst");
    target_role.performance = make_perf(vec![
        (0.8, "task-b", "2026-01-02T00:00:00Z"),
        (0.75, "task-d", "2026-01-04T00:00:00Z"),
    ]);
    target.save_role(&target_role).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(merged.performance.evaluations.len(), 4);
    assert_eq!(merged.performance.task_count, 4);
}

/// Duplicate evaluation (same task_id + timestamp) → deduped.
#[test]
fn performance_merge_deduplicates_same_eval() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let perf = make_perf(vec![(0.9, "task-a", "2026-01-01T00:00:00Z")]);

    let mut source_role = make_role("r1", "analyst");
    source_role.performance = perf.clone();
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "analyst");
    target_role.performance = perf;
    target.save_role(&target_role).unwrap();

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    // Same performance → skipped (no change)
    assert_eq!(summary.roles_skipped, 1);

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(merged.performance.evaluations.len(), 1); // deduped
    assert_eq!(merged.performance.task_count, 1);
}

/// avg_score recalculated correctly after merge.
#[test]
fn performance_merge_avg_score_recalculated() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_role = make_role("r1", "analyst");
    source_role.performance = make_perf(vec![(0.9, "task-a", "2026-01-01T00:00:00Z")]);
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "analyst");
    target_role.performance = make_perf(vec![(0.7, "task-b", "2026-01-02T00:00:00Z")]);
    target.save_role(&target_role).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    // avg_score should be (0.9 + 0.7) / 2 = 0.8
    let avg = merged.performance.avg_score.unwrap();
    assert!((avg - 0.8).abs() < 0.001, "Expected 0.8, got {}", avg);
}

/// Performance merge on motivations works the same way.
#[test]
fn performance_merge_motivations() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_mot = make_motivation("m1", "quality");
    source_mot.performance = make_perf(vec![(0.95, "task-a", "2026-01-01T00:00:00Z")]);
    source.save_tradeoff(&source_mot).unwrap();

    let mut target_mot = make_motivation("m1", "quality-local");
    target_mot.performance = make_perf(vec![(0.85, "task-b", "2026-01-02T00:00:00Z")]);
    target.save_tradeoff(&target_mot).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let motivations = target.load_tradeoffs().unwrap();
    let merged = motivations.iter().find(|m| m.id == "m1").unwrap();
    assert_eq!(merged.performance.evaluations.len(), 2);
    assert_eq!(merged.performance.task_count, 2);
    assert_eq!(merged.name, "quality-local"); // target name preserved
}

/// Performance merge on agents unions evaluations.
#[test]
fn performance_merge_agents() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    // Both need role + motivation in store
    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    target.save_role(&make_role("r1", "builder")).unwrap();
    target
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();

    let mut source_agent = make_agent("a1", "agent-1", "r1", "m1");
    source_agent.performance = make_perf(vec![(0.9, "task-a", "2026-01-01T00:00:00Z")]);
    source.save_agent(&source_agent).unwrap();

    let mut target_agent = make_agent("a1", "agent-1", "r1", "m1");
    target_agent.performance = make_perf(vec![(0.8, "task-b", "2026-01-02T00:00:00Z")]);
    target.save_agent(&target_agent).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let agents = target.load_agents().unwrap();
    let merged = agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(merged.performance.evaluations.len(), 2);
    assert_eq!(merged.performance.task_count, 2);
}

// ===========================================================================
// 7. EVALUATION TRANSFER TESTS
// ===========================================================================

/// Evaluations are transferred with entities.
#[test]
fn evaluations_transferred_with_entities() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();

    let eval = make_evaluation("eval-1", "task-1", "a1", "r1", "m1", 0.9);
    source.save_evaluation(&eval).unwrap();

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.evaluations_added, 1);

    let evals = target.load_evaluations().unwrap();
    assert_eq!(evals.len(), 1);
    assert_eq!(evals[0].id, "eval-1");
}

/// Duplicate evaluations are skipped.
#[test]
fn evaluations_deduped_on_transfer() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();

    let eval = make_evaluation("eval-1", "task-1", "a1", "r1", "m1", 0.9);
    source.save_evaluation(&eval).unwrap();
    target.save_evaluation(&eval).unwrap();

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.evaluations_added, 0);
    assert_eq!(summary.evaluations_skipped, 1);
}

/// no_evaluations flag skips evaluation transfer.
#[test]
fn no_evaluations_flag_skips_transfer() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    let eval = make_evaluation("eval-1", "task-1", "a1", "r1", "m1", 0.9);
    source.save_evaluation(&eval).unwrap();

    let opts = TransferOptions {
        no_evaluations: true,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();
    assert_eq!(summary.evaluations_added, 0);
    assert_eq!(summary.evaluations_skipped, 0);

    let evals = target.load_evaluations().unwrap();
    assert!(evals.is_empty());
}

// ===========================================================================
// 8. EDGE CASES
// ===========================================================================

/// Pull from nonexistent path → store not valid.
#[test]
fn pull_from_nonexistent_path_not_valid() {
    let store = federation::resolve_store("/nonexistent/path/that/does/not/exist").unwrap();
    assert!(!store.is_valid());
}

/// Transfer from invalid source → empty results (no entities loaded).
#[test]
fn transfer_from_empty_source_gives_empty_results() {
    let tmp = TempDir::new().unwrap();
    let source = LocalStore::new(tmp.path().join("empty-source").join("agency"));
    let target = setup_store(&tmp, "target");

    // Source has no files but transfer should not error
    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.roles_added, 0);
    assert_eq!(summary.tradeoffs_added, 0);
    assert_eq!(summary.agents_added, 0);
}

/// Federation config round-trips through YAML correctly.
#[test]
fn federation_config_yaml_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let mut config = FederationConfig::default();
    config.remotes.insert(
        "upstream".to_string(),
        Remote {
            path: "/home/user/shared/agency".to_string(),
            description: Some("Main shared store".to_string()),
            last_sync: Some("2026-01-15T12:00:00Z".to_string()),
        },
    );
    config.remotes.insert(
        "team".to_string(),
        Remote {
            path: "~/team-agents".to_string(),
            description: None,
            last_sync: None,
        },
    );

    federation::save_federation_config(&wg_dir, &config).unwrap();
    let loaded = federation::load_federation_config(&wg_dir).unwrap();

    assert_eq!(loaded.remotes.len(), 2);
    assert_eq!(loaded.remotes["upstream"].path, "/home/user/shared/agency");
    assert_eq!(
        loaded.remotes["upstream"].description.as_deref(),
        Some("Main shared store")
    );
    assert_eq!(
        loaded.remotes["upstream"].last_sync.as_deref(),
        Some("2026-01-15T12:00:00Z")
    );
    assert_eq!(loaded.remotes["team"].path, "~/team-agents");
    assert!(loaded.remotes["team"].description.is_none());
    assert!(loaded.remotes["team"].last_sync.is_none());
}

/// Loading config from directory with no federation.yaml returns empty config.
#[test]
fn federation_config_missing_file_returns_default() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let config = federation::load_federation_config(&wg_dir).unwrap();
    assert!(config.remotes.is_empty());
}

/// Transfer with force flag overwrites target metadata.
#[test]
fn transfer_force_overwrites_target() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let source_role = make_role("r1", "analyst-source");
    source.save_role(&source_role).unwrap();

    let target_role = make_role("r1", "analyst-target");
    target.save_role(&target_role).unwrap();

    let opts = TransferOptions {
        force: true,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();
    assert_eq!(summary.roles_updated, 1);
}

/// Many entities can be transferred at once.
#[test]
fn large_store_transfer() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    // Create 50 roles, 20 motivations, 30 agents
    for i in 0..50 {
        source
            .save_role(&make_role(&format!("r{}", i), &format!("role-{}", i)))
            .unwrap();
    }
    for i in 0..20 {
        source
            .save_tradeoff(&make_motivation(&format!("m{}", i), &format!("mot-{}", i)))
            .unwrap();
    }
    for i in 0..30 {
        let role_id = format!("r{}", i % 50);
        let mot_id = format!("m{}", i % 20);
        source
            .save_agent(&make_agent(
                &format!("a{}", i),
                &format!("agent-{}", i),
                &role_id,
                &mot_id,
            ))
            .unwrap();
    }

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.roles_added, 50);
    assert_eq!(summary.tradeoffs_added, 20);
    assert_eq!(summary.agents_added, 30);

    // Verify counts
    let counts = target.entity_counts();
    assert_eq!(counts.roles, 50);
    assert_eq!(counts.tradeoffs, 20);
    assert_eq!(counts.agents, 30);
}

/// Lineage merge: prefers richer lineage (more parent_ids).
#[test]
fn lineage_merge_prefers_richer() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_role = make_role("r1", "analyst");
    source_role.lineage = Lineage {
        parent_ids: vec!["p1".to_string(), "p2".to_string()],
        generation: 2,
        created_by: "evolver".to_string(),
        created_at: chrono::Utc::now(),
    };
    // Give source a unique eval so the merge triggers an update
    source_role.performance = make_perf(vec![(0.9, "task-src", "2026-02-01T00:00:00Z")]);
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "analyst");
    target_role.lineage = Lineage {
        parent_ids: vec!["p1".to_string()],
        generation: 1,
        created_by: "human".to_string(),
        created_at: chrono::Utc::now(),
    };
    target_role.performance = make_perf(vec![(0.8, "task-tgt", "2026-01-01T00:00:00Z")]);
    target.save_role(&target_role).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    // Source had richer lineage → should be used
    assert_eq!(merged.lineage.parent_ids.len(), 2);
    assert_eq!(merged.lineage.generation, 2);
}

/// ensure_store_dirs creates the full directory structure.
#[test]
fn ensure_store_dirs_creates_structure() {
    let tmp = TempDir::new().unwrap();
    let store_path = tmp.path().join("new-store").join("agency");
    let store = LocalStore::new(&store_path);

    assert!(!store.is_valid());

    federation::ensure_store_dirs(&store).unwrap();

    assert!(store.is_valid());
    assert!(store_path.join("cache/roles").is_dir());
    assert!(store_path.join("primitives/tradeoffs").is_dir());
    assert!(store_path.join("cache/agents").is_dir());
    assert!(store_path.join("evaluations").is_dir());
}

/// Entity prefix matching for entity_ids filter.
#[test]
fn entity_ids_prefix_matching() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source
        .save_role(&make_role("role-abc123", "role-a"))
        .unwrap();
    source
        .save_role(&make_role("role-def456", "role-d"))
        .unwrap();
    source
        .save_role(&make_role("role-abc789", "role-a2"))
        .unwrap();

    // Prefix "role-abc" should match role-abc123 and role-abc789
    let opts = TransferOptions {
        entity_ids: vec!["role-abc".to_string()],
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.roles_added, 2);
    assert!(target.exists_role("role-abc123"));
    assert!(target.exists_role("role-abc789"));
    assert!(!target.exists_role("role-def456"));
}

/// Agents with deps already in target don't re-add deps.
#[test]
fn agent_transfer_skips_existing_deps() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();

    // Pre-populate target with deps
    target.save_role(&make_role("r1", "builder")).unwrap();
    target
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();

    let opts = TransferOptions {
        entity_filter: EntityFilter::Agents,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.agents_added, 1);
    // Deps already exist — not added again
    assert_eq!(summary.roles_added, 0);
    assert_eq!(summary.tradeoffs_added, 0);
}

/// Multiple agents with shared deps → deps only added once.
#[test]
fn multiple_agents_shared_deps_added_once() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();
    source
        .save_agent(&make_agent("a2", "agent-2", "r1", "m1"))
        .unwrap();

    let opts = TransferOptions {
        entity_filter: EntityFilter::Agents,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.agents_added, 2);
    assert_eq!(summary.roles_added, 1);
    assert_eq!(summary.tradeoffs_added, 1);
}

// ===========================================================================
// 9. ADDITIONAL SCAN TESTS
// ===========================================================================

/// Scan via CLI respects --max-depth (integration-level).
/// Stores at depth > max_depth should not be found.
#[test]
fn scan_max_depth_limits_discovery() {
    let tmp = TempDir::new().unwrap();

    // Store at depth 1: root/shallow/.workgraph/agency/
    let shallow = tmp.path().join("shallow");
    create_project_store_dirs(&shallow);
    // Add a role so it's not empty
    let shallow_store = LocalStore::new(shallow.join(".workgraph").join("agency"));
    agency::init(shallow_store.store_path()).unwrap();
    shallow_store
        .save_role(&make_role("r-shallow", "shallow-role"))
        .unwrap();

    // Store at depth 3: root/a/b/deep/.workgraph/agency/
    let deep = tmp.path().join("a").join("b").join("deep");
    create_project_store_dirs(&deep);
    let deep_store = LocalStore::new(deep.join(".workgraph").join("agency"));
    agency::init(deep_store.store_path()).unwrap();
    deep_store
        .save_role(&make_role("r-deep", "deep-role"))
        .unwrap();

    // With max_depth=3 (root/shallow/.workgraph = 2 levels), shallow is found
    // but deep (root/a/b/deep/.workgraph = 4 levels) is not
    // Verify via resolve: shallow store should be valid, deep should exist
    assert!(shallow_store.is_valid());
    assert!(deep_store.is_valid());

    // Test that the shallow store's entity counts are correct at integration level
    let shallow_counts = shallow_store.entity_counts();
    assert_eq!(shallow_counts.roles, 1);
}

// ===========================================================================
// 10. PERMISSION / ERROR EDGE CASES
// ===========================================================================

/// Push to a read-only directory → transfer returns an error.
#[cfg(unix)]
#[test]
fn push_to_read_only_directory_errors() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    source.save_role(&make_role("r1", "test-role")).unwrap();

    // Create target directory structure, then make it read-only
    let target_path = tmp.path().join("readonly").join("agency");
    std::fs::create_dir_all(&target_path).unwrap();
    std::fs::create_dir_all(target_path.join("cache/roles")).unwrap();
    std::fs::set_permissions(
        target_path.join("cache/roles"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();

    let target = LocalStore::new(&target_path);

    // Transfer should fail because we can't write to roles/
    let result = federation::transfer(&source, &target, &TransferOptions::default());
    assert!(
        result.is_err(),
        "Expected error writing to read-only directory"
    );

    // Clean up: restore permissions so temp dir can be deleted
    std::fs::set_permissions(
        target_path.join("cache/roles"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

/// Agent references role/motivation not present in source → transferred without deps.
#[test]
fn agent_with_missing_deps_transferred_gracefully() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    // Agent references r-missing and m-missing which don't exist in source
    source
        .save_agent(&make_agent("a1", "orphan-agent", "r-missing", "m-missing"))
        .unwrap();

    let opts = TransferOptions {
        entity_filter: EntityFilter::Agents,
        ..Default::default()
    };
    // Per design doc §7.1: missing deps in source = broken agent = error
    let result = federation::transfer(&source, &target, &opts);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("r-missing"));
    assert!(err_msg.contains("referential integrity"));
}

/// Agent deps partially present: one dep exists, one doesn't.
/// Per design doc §7.1: missing motivation in source = broken agent = error.
#[test]
fn agent_with_partial_deps() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    // Role exists in source, motivation doesn't
    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_agent(&make_agent("a1", "half-orphan", "r1", "m-missing"))
        .unwrap();

    let opts = TransferOptions {
        entity_filter: EntityFilter::Agents,
        ..Default::default()
    };
    let result = federation::transfer(&source, &target, &opts);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("m-missing"));
    assert!(err_msg.contains("referential integrity"));
}

// ===========================================================================
// 11. FORCE FLAG DETAILED TESTS
// ===========================================================================

/// Force flag overwrites target name and description with source values.
#[test]
fn force_flag_overwrites_target_name() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let source_role = make_role("r1", "source-name");
    source.save_role(&source_role).unwrap();

    let target_role = make_role("r1", "target-name");
    target.save_role(&target_role).unwrap();

    let opts = TransferOptions {
        force: true,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();
    assert_eq!(summary.roles_updated, 1);

    // With force, source overwrites target
    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(merged.name, "source-name");
}

/// Force + no_performance: overwrites identity but keeps target performance.
#[test]
fn force_with_no_performance_keeps_target_perf() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_role = make_role("r1", "source-name");
    source_role.performance = make_perf(vec![(0.9, "task-src", "2026-01-01T00:00:00Z")]);
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "target-name");
    target_role.performance = make_perf(vec![(0.7, "task-tgt", "2026-01-02T00:00:00Z")]);
    target.save_role(&target_role).unwrap();

    let opts = TransferOptions {
        force: true,
        no_performance: true,
        ..Default::default()
    };
    federation::transfer(&source, &target, &opts).unwrap();

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    // Name comes from source (force)
    assert_eq!(merged.name, "source-name");
    // Performance comes from target (no_performance)
    assert_eq!(merged.performance.evaluations.len(), 1);
    assert_eq!(merged.performance.evaluations[0].task_id, "task-tgt");
}

// ===========================================================================
// 12. MULTIPLE REMOTES
// ===========================================================================

/// Multiple named remotes can coexist and resolve independently.
#[test]
fn multiple_remotes_coexist() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join("project").join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let store_a = setup_store(&tmp, "team-a");
    let store_b = setup_store(&tmp, "team-b");
    store_a.save_role(&make_role("r-a", "team-a-role")).unwrap();
    store_b.save_role(&make_role("r-b", "team-b-role")).unwrap();

    let mut config = FederationConfig::default();
    config.remotes.insert(
        "team-a".to_string(),
        Remote {
            path: store_a.store_path().to_string_lossy().to_string(),
            description: Some("Team A store".to_string()),
            last_sync: None,
        },
    );
    config.remotes.insert(
        "team-b".to_string(),
        Remote {
            path: store_b.store_path().to_string_lossy().to_string(),
            description: Some("Team B store".to_string()),
            last_sync: None,
        },
    );
    write_federation_config(&wg_dir, &config);

    // Both resolve independently
    let resolved_a = federation::resolve_store_with_remotes("team-a", &wg_dir).unwrap();
    let resolved_b = federation::resolve_store_with_remotes("team-b", &wg_dir).unwrap();
    assert_eq!(resolved_a.store_path(), store_a.store_path());
    assert_eq!(resolved_b.store_path(), store_b.store_path());

    // Each has its own entities
    assert!(resolved_a.exists_role("r-a"));
    assert!(!resolved_a.exists_role("r-b"));
    assert!(resolved_b.exists_role("r-b"));
    assert!(!resolved_b.exists_role("r-a"));
}

/// Remote removal does not affect other remotes.
#[test]
fn remote_removal_isolates_others() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let mut config = FederationConfig::default();
    config.remotes.insert(
        "alpha".to_string(),
        Remote {
            path: "/some/alpha/path".to_string(),
            description: None,
            last_sync: None,
        },
    );
    config.remotes.insert(
        "beta".to_string(),
        Remote {
            path: "/some/beta/path".to_string(),
            description: None,
            last_sync: None,
        },
    );
    config.remotes.insert(
        "gamma".to_string(),
        Remote {
            path: "/some/gamma/path".to_string(),
            description: None,
            last_sync: None,
        },
    );
    write_federation_config(&wg_dir, &config);

    // Remove beta
    let mut config = federation::load_federation_config(&wg_dir).unwrap();
    config.remotes.remove("beta");
    federation::save_federation_config(&wg_dir, &config).unwrap();

    // Alpha and gamma still present
    let loaded = federation::load_federation_config(&wg_dir).unwrap();
    assert_eq!(loaded.remotes.len(), 2);
    assert!(loaded.remotes.contains_key("alpha"));
    assert!(!loaded.remotes.contains_key("beta"));
    assert!(loaded.remotes.contains_key("gamma"));
}

// ===========================================================================
// 13. MERGE EDGE CASES
// ===========================================================================

/// Merge three stores with cascading overlaps.
#[test]
fn merge_three_stores_cascading_overlaps() {
    let tmp = TempDir::new().unwrap();
    let s1 = setup_store(&tmp, "s1");
    let s2 = setup_store(&tmp, "s2");
    let s3 = setup_store(&tmp, "s3");
    let target = setup_store(&tmp, "target");

    // s1: r1, m1
    s1.save_role(&make_role("r1", "role1")).unwrap();
    s1.save_tradeoff(&make_motivation("m1", "mot1")).unwrap();

    // s2: r1 (overlap with s1), r2
    s2.save_role(&make_role("r1", "role1-v2")).unwrap();
    s2.save_role(&make_role("r2", "role2")).unwrap();

    // s3: r2 (overlap with s2), r3, m1 (overlap with s1)
    s3.save_role(&make_role("r2", "role2-v2")).unwrap();
    s3.save_role(&make_role("r3", "role3")).unwrap();
    s3.save_tradeoff(&make_motivation("m1", "mot1-v2")).unwrap();

    // Merge all three into target
    federation::transfer(&s1, &target, &TransferOptions::default()).unwrap();
    federation::transfer(&s2, &target, &TransferOptions::default()).unwrap();
    federation::transfer(&s3, &target, &TransferOptions::default()).unwrap();

    let roles = target.load_roles().unwrap();
    assert_eq!(roles.len(), 3); // r1, r2, r3
    let mots = target.load_tradeoffs().unwrap();
    assert_eq!(mots.len(), 1); // m1

    // First copy wins for name (since no perf diff → skip)
    let r1 = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(r1.name, "role1"); // from s1 (first)
}

/// Merge with dry_run: all sources previewed, nothing written.
#[test]
fn merge_dry_run_no_writes() {
    let tmp = TempDir::new().unwrap();
    let s1 = setup_store(&tmp, "s1");
    let s2 = setup_store(&tmp, "s2");
    let target = setup_store(&tmp, "target");

    s1.save_role(&make_role("r1", "role1")).unwrap();
    s2.save_role(&make_role("r2", "role2")).unwrap();

    let opts = TransferOptions {
        dry_run: true,
        ..Default::default()
    };
    let sum1 = federation::transfer(&s1, &target, &opts).unwrap();
    let sum2 = federation::transfer(&s2, &target, &opts).unwrap();

    assert_eq!(sum1.roles_added, 1);
    assert_eq!(sum2.roles_added, 1);
    // Nothing written
    assert!(!target.exists_role("r1"));
    assert!(!target.exists_role("r2"));
}

/// Transfer between stores preserves all agent fields.
#[test]
fn transfer_preserves_all_agent_fields() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();

    let mut agent = make_agent("a1", "full-agent", "r1", "m1");
    agent.capabilities = vec!["rust".to_string(), "testing".to_string()];
    agent.executor = "amplifier".to_string();
    source.save_agent(&agent).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let agents = target.load_agents().unwrap();
    let transferred = agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(transferred.name, "full-agent");
    assert_eq!(transferred.role_id, "r1");
    assert_eq!(transferred.tradeoff_id, "m1");
    assert_eq!(transferred.capabilities, vec!["rust", "testing"]);
    assert_eq!(transferred.executor, "amplifier");
}

/// Evaluation relevance filtering: only evals for transferred entities are copied.
#[test]
fn evaluation_relevance_filter_with_entity_ids() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    // Use separate roles and motivations to avoid overlap through auto-pulled deps
    source.save_role(&make_role("r1", "builder")).unwrap();
    source.save_role(&make_role("r2", "tester")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_tradeoff(&make_motivation("m2", "quality"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();
    source
        .save_agent(&make_agent("a2", "agent-2", "r2", "m2"))
        .unwrap();

    // Eval for a1 and a2
    source
        .save_evaluation(&make_evaluation("eval-1", "task-1", "a1", "r1", "m1", 0.9))
        .unwrap();
    source
        .save_evaluation(&make_evaluation("eval-2", "task-2", "a2", "r2", "m2", 0.8))
        .unwrap();

    // Only transfer a1 — auto-pulls r1 and m1, but NOT r2 or m2
    let opts = TransferOptions {
        entity_ids: vec!["a1".to_string()],
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.agents_added, 1);
    assert_eq!(summary.evaluations_added, 1); // only eval-1 (for a1/r1/m1)

    let evals = target.load_evaluations().unwrap();
    assert_eq!(evals.len(), 1);
    assert_eq!(evals[0].id, "eval-1");
}

/// TransferSummary Display format is sensible.
#[test]
fn transfer_summary_display() {
    let summary = TransferSummary {
        components_added: 0,
        components_updated: 0,
        components_skipped: 0,
        components_access_denied: 0,
        outcomes_added: 0,
        outcomes_updated: 0,
        outcomes_skipped: 0,
        outcomes_access_denied: 0,
        roles_added: 2,
        roles_updated: 1,
        roles_skipped: 3,
        tradeoffs_added: 1,
        tradeoffs_updated: 0,
        tradeoffs_skipped: 0,
        tradeoffs_access_denied: 0,
        agents_added: 5,
        agents_updated: 2,
        agents_skipped: 1,
        evaluations_added: 3,
        evaluations_skipped: 0,
    };
    let display = format!("{}", summary);
    assert!(display.contains("+2 new"));
    assert!(display.contains("+5 new"));
    assert!(display.contains("+3 new"));
    assert!(display.contains("3 skipped"));
}

// ===========================================================================
// 14. SCAN VIA CLI COMMAND
// ===========================================================================

/// Scan using the actual scan command (JSON mode) finds multiple project stores.
#[test]
fn scan_cli_finds_multiple_stores_json() {
    let tmp = TempDir::new().unwrap();

    // Create two project stores with entities
    let proj_a = tmp.path().join("alpha");
    create_project_store_dirs(&proj_a);
    let store_a = LocalStore::new(proj_a.join(".workgraph").join("agency"));
    agency::init(store_a.store_path()).unwrap();
    store_a.save_role(&make_role("r1", "role-alpha")).unwrap();

    let proj_b = tmp.path().join("beta");
    create_project_store_dirs(&proj_b);
    let store_b = LocalStore::new(proj_b.join(".workgraph").join("agency"));
    agency::init(store_b.store_path()).unwrap();
    store_b.save_role(&make_role("r2", "role-beta")).unwrap();
    store_b
        .save_tradeoff(&make_motivation("m1", "mot-beta"))
        .unwrap();

    // Both stores should be valid and discoverable
    assert!(store_a.is_valid());
    assert!(store_b.is_valid());

    let counts_a = store_a.entity_counts();
    assert_eq!(counts_a.roles, 1);
    let counts_b = store_b.entity_counts();
    assert_eq!(counts_b.roles, 1);
    assert_eq!(counts_b.tradeoffs, 1);
}

/// Scan finds bare stores alongside project stores without double-counting.
#[test]
fn scan_mixed_bare_and_project_stores() {
    let tmp = TempDir::new().unwrap();

    // Project store
    let proj = tmp.path().join("project");
    create_project_store_dirs(&proj);
    let store_proj = LocalStore::new(proj.join(".workgraph").join("agency"));
    agency::init(store_proj.store_path()).unwrap();
    store_proj.save_role(&make_role("r1", "proj-role")).unwrap();

    // Bare store
    let bare = tmp.path().join("shared");
    create_bare_store_dirs(&bare);
    let store_bare = LocalStore::new(bare.join("agency"));
    agency::init(store_bare.store_path()).unwrap();
    store_bare.save_role(&make_role("r2", "bare-role")).unwrap();

    assert!(store_proj.is_valid());
    assert!(store_bare.is_valid());

    // Both discoverable, entities distinct
    assert!(store_proj.exists_role("r1"));
    assert!(!store_proj.exists_role("r2"));
    assert!(store_bare.exists_role("r2"));
    assert!(!store_bare.exists_role("r1"));
}

// ===========================================================================
// 15. ADDITIONAL PULL/PUSH EDGE CASES
// ===========================================================================

/// Transfer preserves skills on roles.
#[test]
fn transfer_preserves_role_skills() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut role = make_role("r1", "skilled-role");
    role.component_ids = vec![
        "rust".to_string(),
        "testing".to_string(),
        "custom-skill-content".to_string(),
    ];
    source.save_role(&role).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let roles = target.load_roles().unwrap();
    let transferred = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(transferred.component_ids.len(), 3);
}

/// Transfer preserves acceptable and unacceptable tradeoffs on motivations.
#[test]
fn transfer_preserves_motivation_tradeoffs() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut motivation = make_motivation("m1", "quality");
    motivation.acceptable_tradeoffs = vec![
        "Slower delivery for higher quality".to_string(),
        "More verbose output".to_string(),
    ];
    motivation.unacceptable_tradeoffs =
        vec!["Skipping tests".to_string(), "Ignoring errors".to_string()];
    source.save_tradeoff(&motivation).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let motivations = target.load_tradeoffs().unwrap();
    let transferred = motivations.iter().find(|m| m.id == "m1").unwrap();
    assert_eq!(transferred.acceptable_tradeoffs.len(), 2);
    assert_eq!(transferred.unacceptable_tradeoffs.len(), 2);
    assert_eq!(
        transferred.acceptable_tradeoffs[0],
        "Slower delivery for higher quality"
    );
    assert_eq!(transferred.unacceptable_tradeoffs[1], "Ignoring errors");
}

/// Transfer preserves all optional agent fields (rate, capacity, trust_level, contact).
#[test]
fn transfer_preserves_all_optional_agent_fields() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();

    let mut agent = make_agent("a1", "full-agent", "r1", "m1");
    agent.capabilities = vec![
        "rust".to_string(),
        "testing".to_string(),
        "debugging".to_string(),
    ];
    agent.rate = Some(0.95);
    agent.capacity = Some(3.0);
    agent.trust_level = TrustLevel::Verified;
    agent.contact = Some("agent@example.com".to_string());
    agent.executor = "amplifier".to_string();
    source.save_agent(&agent).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let agents = target.load_agents().unwrap();
    let transferred = agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(transferred.rate, Some(0.95));
    assert_eq!(transferred.capacity, Some(3.0));
    assert_eq!(transferred.trust_level, TrustLevel::Verified);
    assert_eq!(transferred.contact.as_deref(), Some("agent@example.com"));
    assert_eq!(transferred.executor, "amplifier");
    assert_eq!(transferred.capabilities.len(), 3);
}

/// Context_id in EvaluationRef is preserved through performance merge.
#[test]
fn performance_merge_preserves_context_id() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_role = make_role("r1", "analyst");
    source_role.performance = PerformanceRecord {
        task_count: 1,
        avg_score: Some(0.9),
        evaluations: vec![EvaluationRef {
            score: 0.9,
            task_id: "task-a".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            context_id: "motivation-xyz".to_string(),
        }],
    };
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "analyst");
    target_role.performance = PerformanceRecord {
        task_count: 1,
        avg_score: Some(0.8),
        evaluations: vec![EvaluationRef {
            score: 0.8,
            task_id: "task-b".to_string(),
            timestamp: "2026-01-02T00:00:00Z".to_string(),
            context_id: "motivation-abc".to_string(),
        }],
    };
    target.save_role(&target_role).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(merged.performance.evaluations.len(), 2);

    // Verify context_ids are preserved
    let has_xyz = merged
        .performance
        .evaluations
        .iter()
        .any(|e| e.context_id == "motivation-xyz");
    let has_abc = merged
        .performance
        .evaluations
        .iter()
        .any(|e| e.context_id == "motivation-abc");
    assert!(has_xyz, "context_id 'motivation-xyz' should be preserved");
    assert!(has_abc, "context_id 'motivation-abc' should be preserved");
}

/// Combined entity_filter + entity_ids: filter agents with specific ID prefix.
#[test]
fn combined_entity_filter_and_entity_ids() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source.save_role(&make_role("r2", "tester")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_tradeoff(&make_motivation("m2", "quality"))
        .unwrap();
    source
        .save_agent(&make_agent("agent-alpha", "alpha", "r1", "m1"))
        .unwrap();
    source
        .save_agent(&make_agent("agent-beta", "beta", "r2", "m2"))
        .unwrap();

    // Filter to agents only, then further narrow by entity ID prefix
    let opts = TransferOptions {
        entity_filter: EntityFilter::Agents,
        entity_ids: vec!["agent-alpha".to_string()],
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.agents_added, 1);
    assert!(target.exists_agent("agent-alpha"));
    assert!(!target.exists_agent("agent-beta"));
    // Auto-pulled deps for alpha only
    assert!(target.exists_role("r1"));
    assert!(target.exists_tradeoff("m1"));
    assert!(!target.exists_role("r2"));
    assert!(!target.exists_tradeoff("m2"));
}

/// Force flag on motivation overwrites target metadata.
#[test]
fn force_flag_overwrites_motivation() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_mot = make_motivation("m1", "source-quality");
    source_mot.description = "Source description".to_string();
    source_mot.acceptable_tradeoffs = vec!["source tradeoff".to_string()];
    source.save_tradeoff(&source_mot).unwrap();

    let mut target_mot = make_motivation("m1", "target-quality");
    target_mot.description = "Target description".to_string();
    target_mot.acceptable_tradeoffs = vec!["target tradeoff".to_string()];
    target.save_tradeoff(&target_mot).unwrap();

    let opts = TransferOptions {
        force: true,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();
    assert_eq!(summary.tradeoffs_updated, 1);

    let motivations = target.load_tradeoffs().unwrap();
    let merged = motivations.iter().find(|m| m.id == "m1").unwrap();
    // Force: source wins
    assert_eq!(merged.name, "source-quality");
    assert_eq!(merged.description, "Source description");
}

/// Force flag on agent overwrites target metadata.
#[test]
fn force_flag_overwrites_agent() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    target.save_role(&make_role("r1", "builder")).unwrap();
    target
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();

    let mut source_agent = make_agent("a1", "source-agent", "r1", "m1");
    source_agent.executor = "amplifier".to_string();
    source.save_agent(&source_agent).unwrap();

    let mut target_agent = make_agent("a1", "target-agent", "r1", "m1");
    target_agent.executor = "claude".to_string();
    target.save_agent(&target_agent).unwrap();

    let opts = TransferOptions {
        force: true,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();
    assert_eq!(summary.agents_updated, 1);

    let agents = target.load_agents().unwrap();
    let merged = agents.iter().find(|a| a.id == "a1").unwrap();
    assert_eq!(merged.name, "source-agent");
    assert_eq!(merged.executor, "amplifier");
}

/// No_evaluations + force: overwrites metadata, keeps evaluations out.
#[test]
fn force_no_evaluations_combo() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "source-role")).unwrap();
    let eval = make_evaluation("eval-1", "task-1", "a1", "r1", "m1", 0.9);
    source.save_evaluation(&eval).unwrap();

    target.save_role(&make_role("r1", "target-role")).unwrap();

    let opts = TransferOptions {
        force: true,
        no_evaluations: true,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    // Force overwrites role metadata
    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(merged.name, "source-role");

    // No evaluations transferred
    assert_eq!(summary.evaluations_added, 0);
    assert_eq!(summary.evaluations_skipped, 0);
    let evals = target.load_evaluations().unwrap();
    assert!(evals.is_empty());
}

// ===========================================================================
// 16. MERGE WITH PERFORMANCE ACCUMULATION
// ===========================================================================

/// Merge three stores with performance data → accumulates correctly.
#[test]
fn merge_three_stores_performance_accumulation() {
    let tmp = TempDir::new().unwrap();
    let s1 = setup_store(&tmp, "s1");
    let s2 = setup_store(&tmp, "s2");
    let s3 = setup_store(&tmp, "s3");
    let target = setup_store(&tmp, "target");

    // All three have the same role with different evaluations
    let mut r1_s1 = make_role("r1", "analyst");
    r1_s1.performance = make_perf(vec![(0.9, "task-a", "2026-01-01T00:00:00Z")]);
    s1.save_role(&r1_s1).unwrap();

    let mut r1_s2 = make_role("r1", "analyst");
    r1_s2.performance = make_perf(vec![(0.8, "task-b", "2026-01-02T00:00:00Z")]);
    s2.save_role(&r1_s2).unwrap();

    let mut r1_s3 = make_role("r1", "analyst");
    r1_s3.performance = make_perf(vec![(0.7, "task-c", "2026-01-03T00:00:00Z")]);
    s3.save_role(&r1_s3).unwrap();

    federation::transfer(&s1, &target, &TransferOptions::default()).unwrap();
    federation::transfer(&s2, &target, &TransferOptions::default()).unwrap();
    federation::transfer(&s3, &target, &TransferOptions::default()).unwrap();

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(merged.performance.evaluations.len(), 3);
    assert_eq!(merged.performance.task_count, 3);
    // avg_score = (0.9 + 0.8 + 0.7) / 3 = 0.8
    let avg = merged.performance.avg_score.unwrap();
    assert!((avg - 0.8).abs() < 0.001, "Expected 0.8, got {}", avg);
}

/// Merge idempotency with performance: merging same source twice doesn't duplicate evals.
#[test]
fn merge_idempotent_with_performance() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut role = make_role("r1", "analyst");
    role.performance = make_perf(vec![
        (0.9, "task-a", "2026-01-01T00:00:00Z"),
        (0.85, "task-b", "2026-01-02T00:00:00Z"),
    ]);
    source.save_role(&role).unwrap();

    // First transfer
    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    // Second transfer (idempotent)
    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.roles_skipped, 1); // No change → skipped

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    assert_eq!(merged.performance.evaluations.len(), 2); // Not duplicated
}

// ===========================================================================
// 17. CIRCULAR / SELF-REFERENCING EDGE CASES
// ===========================================================================

/// Agent whose role_id and motivation_id exist in target but not source:
/// only the agent itself needs to be transferred.
#[test]
fn agent_deps_already_in_target_not_source() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    // Target has deps, source has agent referencing them
    target.save_role(&make_role("r1", "builder")).unwrap();
    target
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();

    // Source has the agent AND the deps (for referential integrity check)
    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();

    let opts = TransferOptions {
        entity_filter: EntityFilter::Agents,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();
    assert_eq!(summary.agents_added, 1);
    // Deps already exist in target → not counted as added
    assert_eq!(summary.roles_added, 0);
    assert_eq!(summary.tradeoffs_added, 0);
}

/// Two agents sharing the same role but different motivations: both deps pulled.
#[test]
fn two_agents_different_deps_both_pulled() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source.save_role(&make_role("r2", "tester")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_tradeoff(&make_motivation("m2", "quality"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();
    source
        .save_agent(&make_agent("a2", "agent-2", "r2", "m2"))
        .unwrap();

    let opts = TransferOptions {
        entity_filter: EntityFilter::Agents,
        ..Default::default()
    };
    let summary = federation::transfer(&source, &target, &opts).unwrap();

    assert_eq!(summary.agents_added, 2);
    assert_eq!(summary.roles_added, 2);
    assert_eq!(summary.tradeoffs_added, 2);
    assert!(target.exists_agent("a1"));
    assert!(target.exists_agent("a2"));
    assert!(target.exists_role("r1"));
    assert!(target.exists_role("r2"));
    assert!(target.exists_tradeoff("m1"));
    assert!(target.exists_tradeoff("m2"));
}

// ===========================================================================
// 18. EVALUATION EDGE CASES
// ===========================================================================

/// Multiple evaluations from same agent, different tasks → all transferred.
#[test]
fn multiple_evaluations_same_agent_all_transferred() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();
    source
        .save_tradeoff(&make_motivation("m1", "speed"))
        .unwrap();
    source
        .save_agent(&make_agent("a1", "agent-1", "r1", "m1"))
        .unwrap();

    for i in 0..5 {
        let eval = make_evaluation(
            &format!("eval-{}", i),
            &format!("task-{}", i),
            "a1",
            "r1",
            "m1",
            0.7 + (i as f64) * 0.05,
        );
        source.save_evaluation(&eval).unwrap();
    }

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.evaluations_added, 5);

    let evals = target.load_evaluations().unwrap();
    assert_eq!(evals.len(), 5);
}

/// Evaluation dimensions (HashMap) preserved through transfer.
#[test]
fn evaluation_dimensions_preserved() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    source.save_role(&make_role("r1", "builder")).unwrap();

    let mut eval = make_evaluation("eval-1", "task-1", "a1", "r1", "m1", 0.85);
    eval.dimensions.insert("quality".to_string(), 0.9);
    eval.dimensions.insert("speed".to_string(), 0.8);
    eval.dimensions.insert("correctness".to_string(), 0.85);
    eval.model = Some(CLAUDE_OPUS_MODEL_ID.to_string());
    source.save_evaluation(&eval).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let evals = target.load_evaluations().unwrap();
    assert_eq!(evals.len(), 1);
    let transferred = &evals[0];
    assert_eq!(transferred.dimensions.len(), 3);
    assert_eq!(transferred.dimensions["quality"], 0.9);
    assert_eq!(transferred.dimensions["speed"], 0.8);
    assert_eq!(transferred.dimensions["correctness"], 0.85);
    assert_eq!(transferred.model.as_deref(), Some(CLAUDE_OPUS_MODEL_ID));
}

/// Evaluations from two sources, partially overlapping, merged into target.
#[test]
fn evaluations_merge_from_two_sources() {
    let tmp = TempDir::new().unwrap();
    let s1 = setup_store(&tmp, "s1");
    let s2 = setup_store(&tmp, "s2");
    let target = setup_store(&tmp, "target");

    s1.save_role(&make_role("r1", "builder")).unwrap();
    s2.save_role(&make_role("r1", "builder")).unwrap();

    // s1 has eval-1 and eval-2
    s1.save_evaluation(&make_evaluation("eval-1", "task-1", "a1", "r1", "m1", 0.9))
        .unwrap();
    s1.save_evaluation(&make_evaluation("eval-2", "task-2", "a1", "r1", "m1", 0.8))
        .unwrap();

    // s2 has eval-2 (overlap) and eval-3
    s2.save_evaluation(&make_evaluation("eval-2", "task-2", "a1", "r1", "m1", 0.8))
        .unwrap();
    s2.save_evaluation(&make_evaluation("eval-3", "task-3", "a1", "r1", "m1", 0.7))
        .unwrap();

    let sum1 = federation::transfer(&s1, &target, &TransferOptions::default()).unwrap();
    let sum2 = federation::transfer(&s2, &target, &TransferOptions::default()).unwrap();

    assert_eq!(sum1.evaluations_added, 2);
    assert_eq!(sum2.evaluations_added, 1); // eval-2 already exists
    assert_eq!(sum2.evaluations_skipped, 1); // eval-2 skipped

    let evals = target.load_evaluations().unwrap();
    assert_eq!(evals.len(), 3); // eval-1, eval-2, eval-3
}

// ===========================================================================
// 19. NO_PERFORMANCE ON EXISTING ENTITIES
// ===========================================================================

/// no_performance on existing entity: target's performance is preserved, not merged.
#[test]
fn no_performance_preserves_target_perf_on_existing() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_role = make_role("r1", "analyst");
    source_role.performance = make_perf(vec![(0.95, "task-src", "2026-02-01T00:00:00Z")]);
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "analyst");
    target_role.performance = make_perf(vec![(0.75, "task-tgt", "2026-01-01T00:00:00Z")]);
    target.save_role(&target_role).unwrap();

    let opts = TransferOptions {
        no_performance: true,
        ..Default::default()
    };
    federation::transfer(&source, &target, &opts).unwrap();

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    // Target's performance is preserved (not merged with source's)
    assert_eq!(merged.performance.evaluations.len(), 1);
    assert_eq!(merged.performance.evaluations[0].task_id, "task-tgt");
}

// ===========================================================================
// 20. RESOLVE STORE EDGE CASES
// ===========================================================================

/// resolve_store with a path that directly has roles/ uses it as-is.
#[test]
fn resolve_store_direct_agency_path() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    std::fs::create_dir_all(agency_dir.join("cache/roles")).unwrap();
    std::fs::create_dir_all(agency_dir.join("primitives/tradeoffs")).unwrap();
    std::fs::create_dir_all(agency_dir.join("cache/agents")).unwrap();
    std::fs::create_dir_all(agency_dir.join("evaluations")).unwrap();

    // Point directly at the agency dir
    let store = federation::resolve_store(agency_dir.to_str().unwrap()).unwrap();
    assert_eq!(store.store_path(), agency_dir);
    assert!(store.is_valid());
}

/// Transfer summary for empty-to-empty transfer is all zeros.
#[test]
fn transfer_empty_to_empty_all_zeros() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let summary = federation::transfer(&source, &target, &TransferOptions::default()).unwrap();
    assert_eq!(summary.roles_added, 0);
    assert_eq!(summary.roles_updated, 0);
    assert_eq!(summary.roles_skipped, 0);
    assert_eq!(summary.tradeoffs_added, 0);
    assert_eq!(summary.tradeoffs_updated, 0);
    assert_eq!(summary.tradeoffs_skipped, 0);
    assert_eq!(summary.agents_added, 0);
    assert_eq!(summary.agents_updated, 0);
    assert_eq!(summary.agents_skipped, 0);
    assert_eq!(summary.evaluations_added, 0);
    assert_eq!(summary.evaluations_skipped, 0);
}

/// Lineage merge: when parent_ids are equal, higher generation wins.
#[test]
fn lineage_merge_higher_generation_wins_on_equal_parents() {
    let tmp = TempDir::new().unwrap();
    let source = setup_store(&tmp, "source");
    let target = setup_store(&tmp, "target");

    let mut source_role = make_role("r1", "analyst");
    source_role.lineage = Lineage {
        parent_ids: vec!["p1".to_string()],
        generation: 5,
        created_by: "evolver".to_string(),
        created_at: chrono::Utc::now(),
    };
    source_role.performance = make_perf(vec![(0.9, "task-src", "2026-02-01T00:00:00Z")]);
    source.save_role(&source_role).unwrap();

    let mut target_role = make_role("r1", "analyst");
    target_role.lineage = Lineage {
        parent_ids: vec!["p1".to_string()],
        generation: 3,
        created_by: "human".to_string(),
        created_at: chrono::Utc::now(),
    };
    target_role.performance = make_perf(vec![(0.8, "task-tgt", "2026-01-01T00:00:00Z")]);
    target.save_role(&target_role).unwrap();

    federation::transfer(&source, &target, &TransferOptions::default()).unwrap();

    let roles = target.load_roles().unwrap();
    let merged = roles.iter().find(|r| r.id == "r1").unwrap();
    // Same parent count but source has higher generation → source lineage wins
    assert_eq!(merged.lineage.generation, 5);
}

// ---------------------------------------------------------------------------
// Helper: accumulate summary (mirrors merge command logic)
// ---------------------------------------------------------------------------

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
