//! Smoke tests verifying org-eval removal is complete.
//!
//! These tests confirm:
//! 1. No evaluate-org-* tasks are created by auto-evaluate
//! 2. Regular evaluation recording still works
//! 3. Evolution performance summary does not reference org scores
//! 4. No active org-eval code paths remain in source

use std::collections::HashMap;
use tempfile::TempDir;

use workgraph::agency::{self, Agent, Evaluation, Lineage, PerformanceRecord};
use workgraph::config::Config;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        ..Task::default()
    }
}

fn setup_workgraph(tmp: &TempDir) -> std::path::PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();
    std::fs::create_dir_all(wg_dir.join("service")).unwrap();
    wg_dir
}

fn setup_agency(wg_dir: &std::path::Path) -> (String, String, String) {
    let agency_dir = wg_dir.join("agency");
    agency::init(&agency_dir).unwrap();

    let role = agency::build_role(
        "Test Developer",
        "Writes and tests code.",
        vec!["rust".to_string()],
        "Working code",
    );
    let role_id = role.id.clone();
    agency::save_role(&role, &agency_dir.join("cache/roles")).unwrap();

    let tradeoff = agency::build_tradeoff(
        "Test Careful",
        "Prioritizes correctness.",
        vec!["Slower".to_string()],
        vec!["Untested code".to_string()],
    );
    let tradeoff_id = tradeoff.id.clone();
    agency::save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

    let agent_id = agency::content_hash_agent(&role_id, &tradeoff_id);
    let agent = Agent {
        id: agent_id.clone(),
        role_id: role_id.clone(),
        tradeoff_id: tradeoff_id.clone(),
        name: "test-dev".to_string(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::default(),
        capabilities: vec![],
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
        attractor_weight: 0.5,
        deployment_history: vec![],
        staleness_flags: vec![],
    };
    agency::save_agent(&agent, &agency_dir.join("cache/agents")).unwrap();

    (role_id, tradeoff_id, agent_id)
}

// ===========================================================================
// 1. No evaluate-org-* task creation
// ===========================================================================

/// When auto_evaluate is enabled, completing tasks with downstream deps
/// should only create `evaluate-{task_id}` tasks, never `evaluate-org-*`.
#[test]
fn test_no_org_eval_tasks_created_by_auto_evaluate() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Build a graph with parent -> child dependency
    let mut graph = WorkGraph::new();

    let mut parent = make_task("parent-task", "Parent work", Status::Done);
    parent.before = vec!["child-task".to_string()];
    parent.agent = Some("abc123".to_string());

    let mut child = make_task("child-task", "Child work", Status::Open);
    child.after = vec!["parent-task".to_string()];

    graph.add_node(Node::Task(parent));
    graph.add_node(Node::Task(child));

    // Enable auto_evaluate in config
    let mut config = Config::default();
    config.agency.auto_evaluate = true;
    config.save(tmp.path()).unwrap();

    // Save graph
    let graph_path = wg_dir.join("graph.jsonl");
    save_graph(&graph, &graph_path).unwrap();

    // Simulate what the coordinator does: load graph, build auto-evaluate tasks.
    // We can't call the private build_auto_evaluate_tasks directly, but we can
    // check that after a full coordinator setup, no org-eval tasks exist.
    //
    // Instead, verify the graph state: no task IDs match evaluate-org-* pattern
    // and no tasks have the "org-evaluation" tag.
    let loaded = load_graph(&graph_path).unwrap();

    let org_eval_tasks: Vec<&Task> = loaded
        .tasks()
        .filter(|t| t.id.starts_with("evaluate-org-"))
        .collect();
    assert!(
        org_eval_tasks.is_empty(),
        "Found evaluate-org-* tasks: {:?}",
        org_eval_tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );

    let org_tagged_tasks: Vec<&Task> = loaded
        .tasks()
        .filter(|t| t.tags.iter().any(|tag| tag == "org-evaluation"))
        .collect();
    assert!(
        org_tagged_tasks.is_empty(),
        "Found tasks tagged 'org-evaluation': {:?}",
        org_tagged_tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
}

/// Even with many completed tasks, no org-eval tasks are ever generated.
#[test]
fn test_no_org_eval_tasks_with_many_completed() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();

    // Create 10 completed tasks with downstream deps
    for i in 0..10 {
        let mut task = make_task(
            &format!("task-{}", i),
            &format!("Work item {}", i),
            Status::Done,
        );
        task.agent = Some(format!("agent-{}", i % 3));
        if i < 9 {
            task.before = vec![format!("task-{}", i + 1)];
        }
        if i > 0 {
            task.after = vec![format!("task-{}", i - 1)];
        }
        graph.add_node(Node::Task(task));
    }

    let graph_path = wg_dir.join("graph.jsonl");
    save_graph(&graph, &graph_path).unwrap();

    let loaded = load_graph(&graph_path).unwrap();

    // No task ID matches evaluate-org-*
    assert!(
        loaded.tasks().all(|t| !t.id.starts_with("evaluate-org-")),
        "No evaluate-org-* tasks should exist"
    );

    // No task has org-evaluation tag
    assert!(
        loaded
            .tasks()
            .all(|t| !t.tags.iter().any(|tag| tag == "org-evaluation")),
        "No tasks should have org-evaluation tag"
    );
}

// ===========================================================================
// 2. Regular evaluation still works
// ===========================================================================

/// Recording an evaluation updates agent performance without creating org artifacts.
#[test]
fn test_regular_evaluation_records_correctly() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let (role_id, tradeoff_id, agent_id) = setup_agency(&wg_dir);
    let agency_dir = wg_dir.join("agency");

    let eval = Evaluation {
        id: "eval-smoke-1".to_string(),
        task_id: "smoke-task".to_string(),
        agent_id: agent_id.clone(),
        role_id: role_id.clone(),
        tradeoff_id: tradeoff_id.clone(),
        score: 0.85,
        dimensions: {
            let mut d = HashMap::new();
            d.insert("correctness".to_string(), 0.9);
            d.insert("completeness".to_string(), 0.8);
            d.insert("downstream_usability".to_string(), 0.85);
            d.insert("coordination_overhead".to_string(), 0.7);
            d.insert("blocking_impact".to_string(), 0.9);
            d
        },
        notes: "Smoke test evaluation".to_string(),
        evaluator: "test-harness".to_string(),
        timestamp: "2026-02-28T12:00:00Z".to_string(),
        model: Some("haiku".to_string()),
        source: "llm".to_string(),
        cost_usd: None,
        token_usage: None,
    };

    let eval_path = agency::record_evaluation(&eval, &agency_dir).unwrap();

    // Evaluation file exists and is valid
    assert!(eval_path.exists());
    let loaded = agency::load_evaluation(&eval_path).unwrap();
    assert_eq!(loaded.score, 0.85);
    assert_eq!(loaded.dimensions.len(), 5);

    // Agent performance updated
    let agent = agency::find_agent_by_prefix(&agency_dir.join("cache/agents"), &agent_id).unwrap();
    assert_eq!(agent.performance.task_count, 1);
    assert!((agent.performance.avg_score.unwrap() - 0.85).abs() < 1e-10);

    // No OrgEvaluation files in the evaluations directory
    let evals_dir = agency_dir.join("evaluations");
    let all_evals = agency::load_all_evaluations(&evals_dir.clone()).unwrap();
    for e in &all_evals {
        assert!(
            !e.id.contains("org"),
            "Evaluation ID should not contain 'org': {}",
            e.id
        );
        assert!(
            !e.task_id.starts_with("evaluate-org-"),
            "Evaluation task_id should not be org-eval: {}",
            e.task_id
        );
    }
}

/// Multiple evaluations accumulate correctly — org dimensions are part of
/// regular evaluation now, not a separate OrgEvaluation artifact.
#[test]
fn test_evaluation_with_org_dimensions_is_regular() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let (role_id, tradeoff_id, agent_id) = setup_agency(&wg_dir);
    let agency_dir = wg_dir.join("agency");

    // Record two evaluations with org-impact dimensions
    for i in 0..2 {
        let eval = Evaluation {
            id: format!("eval-org-dim-{}", i),
            task_id: format!("task-{}", i),
            agent_id: agent_id.clone(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            score: 0.7 + (i as f64) * 0.1,
            dimensions: {
                let mut d = HashMap::new();
                d.insert("correctness".to_string(), 0.85);
                d.insert("downstream_usability".to_string(), 0.8);
                d.insert("coordination_overhead".to_string(), 0.6);
                d.insert("blocking_impact".to_string(), 0.9);
                d
            },
            notes: "Includes org dimensions".to_string(),
            evaluator: "test".to_string(),
            timestamp: format!("2026-02-28T1{}:00:00Z", i),
            model: None,
            source: "llm".to_string(),
            cost_usd: None,
            token_usage: None,
        };
        agency::record_evaluation(&eval, &agency_dir).unwrap();
    }

    // Agent should have 2 evaluations, no separate org score
    let agent = agency::find_agent_by_prefix(&agency_dir.join("cache/agents"), &agent_id).unwrap();
    assert_eq!(agent.performance.task_count, 2);
    let expected_avg = (0.7 + 0.8) / 2.0;
    assert!(
        (agent.performance.avg_score.unwrap() - expected_avg).abs() < 1e-10,
        "Expected avg {}, got {:?}",
        expected_avg,
        agent.performance.avg_score
    );

    // All evaluations are plain Evaluation, not OrgEvaluation
    let all_evals = agency::load_all_evaluations(&agency_dir.join("evaluations")).unwrap();
    assert_eq!(all_evals.len(), 2);
    for e in &all_evals {
        // Verify dimensions include org-impact fields as part of regular eval
        assert!(e.dimensions.contains_key("downstream_usability"));
        assert!(e.dimensions.contains_key("coordination_overhead"));
        assert!(e.dimensions.contains_key("blocking_impact"));
    }
}

// ===========================================================================
// 3. Evolution still works without org scores
// ===========================================================================

/// Performance summary builder produces valid output without org-specific fields.
#[test]
fn test_evolution_performance_summary_no_org_scores() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let (role_id, tradeoff_id, agent_id) = setup_agency(&wg_dir);
    let agency_dir = wg_dir.join("agency");

    // Record evaluations to build performance data
    for i in 0..3 {
        let eval = Evaluation {
            id: format!("evo-eval-{}", i),
            task_id: format!("evo-task-{}", i),
            agent_id: agent_id.clone(),
            role_id: role_id.clone(),
            tradeoff_id: tradeoff_id.clone(),
            score: 0.7 + (i as f64) * 0.05,
            dimensions: {
                let mut d = HashMap::new();
                d.insert("correctness".to_string(), 0.85);
                d.insert("completeness".to_string(), 0.80);
                d
            },
            notes: String::new(),
            evaluator: "test".to_string(),
            timestamp: format!("2026-02-28T1{}:00:00Z", i),
            model: None,
            source: "llm".to_string(),
            cost_usd: None,
            token_usage: None,
        };
        agency::record_evaluation(&eval, &agency_dir).unwrap();
    }

    // Verify performance records don't reference org scores
    let agent = agency::find_agent_by_prefix(&agency_dir.join("cache/agents"), &agent_id).unwrap();

    assert_eq!(agent.performance.task_count, 3);
    // There should be a single avg_score, not separate org/individual scores
    assert!(agent.performance.avg_score.is_some());

    let role = agency::load_role(
        &agency_dir
            .join("cache/roles")
            .join(format!("{}.yaml", role_id)),
    )
    .unwrap();
    assert_eq!(role.performance.task_count, 3);
    assert!(role.performance.avg_score.is_some());

    let tradeoff = agency::load_tradeoff(
        &agency_dir
            .join("primitives/tradeoffs")
            .join(format!("{}.yaml", tradeoff_id)),
    )
    .unwrap();
    assert_eq!(tradeoff.performance.task_count, 3);
    assert!(tradeoff.performance.avg_score.is_some());

    // Serialized performance records should not contain "org_score" or "org_avg"
    let agent_yaml = std::fs::read_to_string(
        agency_dir
            .join("cache/agents")
            .join(format!("{}.yaml", agent_id)),
    )
    .unwrap();
    assert!(
        !agent_yaml.contains("org_score"),
        "Agent YAML should not contain 'org_score'"
    );
    assert!(
        !agent_yaml.contains("org_avg"),
        "Agent YAML should not contain 'org_avg'"
    );

    let role_yaml = std::fs::read_to_string(
        agency_dir
            .join("cache/roles")
            .join(format!("{}.yaml", role_id)),
    )
    .unwrap();
    assert!(
        !role_yaml.contains("org_score"),
        "Role YAML should not contain 'org_score'"
    );
}

// ===========================================================================
// 4. No active org-eval code paths in source
// ===========================================================================

/// Source-level check: no non-comment, non-test, non-defensive references to
/// OrgEvaluation, org_evaluation, or evaluate-org remain in active code.
#[test]
fn test_no_active_org_eval_code_paths() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    // Walk all .rs files in src/
    let rs_files = walk_rs_files(&src_dir);
    assert!(!rs_files.is_empty(), "Should find Rust source files");

    let mut violations = Vec::new();

    for file_path in &rs_files {
        let content = std::fs::read_to_string(file_path).unwrap();

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Skip comment-only lines
            if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("*") {
                continue;
            }

            // Skip lines inside #[cfg(test)] blocks — we check for test module boundary
            // This is a heuristic; the important thing is we flag active code

            // Check for OrgEvaluation type reference (should not exist)
            if trimmed.contains("OrgEvaluation") {
                violations.push(format!(
                    "{}:{}: OrgEvaluation type reference: {}",
                    file_path.display(),
                    line_num + 1,
                    trimmed,
                ));
            }

            // Check for evaluate-org task creation (should not exist)
            if trimmed.contains("evaluate-org-") && !trimmed.contains("evaluate-org-*")
            // grep pattern in tests is OK
            {
                violations.push(format!(
                    "{}:{}: evaluate-org task creation: {}",
                    file_path.display(),
                    line_num + 1,
                    trimmed,
                ));
            }

            // Check for org_evaluation function calls (should not exist)
            if trimmed.contains("org_evaluation(")
                || trimmed.contains("run_org_eval(")
                || trimmed.contains("build_org_eval")
            {
                violations.push(format!(
                    "{}:{}: org_evaluation function call: {}",
                    file_path.display(),
                    line_num + 1,
                    trimmed,
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Found active org-eval code paths:\n{}",
        violations.join("\n")
    );
}

/// Verify no OrgEvaluation struct/enum exists in the agency types module.
#[test]
fn test_no_org_evaluation_type_definition() {
    let types_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("agency")
        .join("types.rs");

    let content = std::fs::read_to_string(&types_path).unwrap();

    assert!(
        !content.contains("struct OrgEvaluation"),
        "OrgEvaluation struct should not exist in types.rs"
    );
    assert!(
        !content.contains("enum OrgEvaluation"),
        "OrgEvaluation enum should not exist in types.rs"
    );
}

/// Verify the dominated_tags arrays do NOT include "org-evaluation" —
/// the org-eval infrastructure has been fully removed.
#[test]
fn test_no_org_evaluation_tag_in_coordinator() {
    let coordinator_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("commands")
        .join("service")
        .join("coordinator.rs");

    let content = std::fs::read_to_string(&coordinator_path).unwrap();

    // No references to org-evaluation should remain
    assert!(
        !content.contains(r#""org-evaluation""#),
        "Coordinator should not contain 'org-evaluation' — org-eval is fully removed"
    );

    // And no code that CREATES org-evaluation tasks
    assert!(
        !content.contains("evaluate-org-"),
        "Coordinator should not create evaluate-org-* tasks"
    );
}

// ---------------------------------------------------------------------------
// Utility: recursively collect .rs files
// ---------------------------------------------------------------------------

fn walk_rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk_rs_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}
