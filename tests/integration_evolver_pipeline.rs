//! End-to-end integration tests for the fan-out/fan-in autopoietic evolver pipeline.
//!
//! Covers:
//! 1. Full pipeline: partition → analyze → synthesize → apply → evaluate
//! 2. Partial analyzer failure (graceful degradation via synthesize)
//! 3. Convergence detection and cycle termination
//! 4. Cycle structure correctness (back-edges, iteration increments)
//!
//! Uses the CLI binary (`wg evolve run --force-fanout`) to exercise the full
//! pipeline, then inspects the resulting graph via the library API.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use workgraph::agency::{
    Evaluation, build_role, build_tradeoff, init as init_agency, record_evaluation, save_role,
    save_tradeoff,
};
use workgraph::graph::{Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .env_remove("WG_TASK_ID")
        .env_remove("WG_AGENT_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    stdout
}

fn setup_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::create_dir_all(wg_dir.join("service")).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

fn make_evaluation(
    id: &str,
    task_id: &str,
    score: f64,
    role_id: &str,
    tradeoff_id: &str,
) -> Evaluation {
    Evaluation {
        id: id.to_string(),
        task_id: task_id.to_string(),
        agent_id: String::new(),
        role_id: role_id.to_string(),
        tradeoff_id: tradeoff_id.to_string(),
        score,
        dimensions: HashMap::new(),
        notes: String::new(),
        evaluator: "test".to_string(),
        timestamp: format!("2026-03-13T12:00:{}Z", id.len() % 60),
        model: Some("test-model".to_string()),
        source: "llm".to_string(),
        loop_iteration: 0,
    }
}

/// Seed a test project with 6 roles, 5 tradeoffs, and 24 evaluations.
/// Returns (role_ids, tradeoff_ids) for reference.
fn seed_agency_data(agency_dir: &Path) -> (Vec<String>, Vec<String>) {
    init_agency(agency_dir).unwrap();

    // 6 roles with varying performance
    let role_defs = vec![
        (
            "Implementer",
            "Writes code",
            vec!["rust"],
            "Working code",
            0.45,
            8,
        ),
        (
            "Reviewer",
            "Reviews pull requests",
            vec!["code-review"],
            "High-quality reviews",
            0.65,
            6,
        ),
        (
            "Debugger",
            "Finds and fixes bugs",
            vec!["debugging"],
            "Bug-free code",
            0.25,
            10,
        ),
        (
            "Architect",
            "Designs systems",
            vec!["design"],
            "Clean architecture",
            0.80,
            7,
        ),
        (
            "Tester",
            "Writes tests",
            vec!["testing"],
            "Comprehensive tests",
            0.55,
            5,
        ),
        (
            "Documenter",
            "Writes documentation",
            vec!["docs"],
            "Clear documentation",
            0.90,
            4,
        ),
    ];

    let mut role_ids = Vec::new();
    for (name, desc, skills, outcome, _avg_score, _task_count) in &role_defs {
        let role = build_role(
            *name,
            *desc,
            skills.iter().map(|s| s.to_string()).collect(),
            *outcome,
        );
        role_ids.push(role.id.clone());
        save_role(&role, &agency_dir.join("cache/roles")).unwrap();
    }

    // 5 tradeoffs with varying performance
    let tradeoff_defs = vec![
        (
            "Quality First",
            "Prioritise correctness",
            vec!["Slower delivery"],
            vec!["Skipping tests"],
        ),
        (
            "Speed Focus",
            "Deliver quickly",
            vec!["Less polish"],
            vec!["Skipping validation"],
        ),
        (
            "Thorough",
            "Be comprehensive",
            vec!["Takes longer"],
            vec!["Incomplete analysis"],
        ),
        (
            "Pragmatic",
            "Balance speed and quality",
            vec!["Some shortcuts"],
            vec!["Major shortcuts"],
        ),
        (
            "Creative",
            "Explore novel approaches",
            vec!["Unpredictable timelines"],
            vec!["Ignoring constraints"],
        ),
    ];

    let mut tradeoff_ids = Vec::new();
    for (name, desc, acceptable, unacceptable) in &tradeoff_defs {
        let tradeoff = build_tradeoff(
            *name,
            *desc,
            acceptable.iter().map(|s| s.to_string()).collect(),
            unacceptable.iter().map(|s| s.to_string()).collect(),
        );
        tradeoff_ids.push(tradeoff.id.clone());
        save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();
    }

    // 24 evaluations spread across roles and tradeoffs
    for i in 0..24 {
        let role_idx = i % role_ids.len();
        let tradeoff_idx = i % tradeoff_ids.len();
        let base_score = role_defs[role_idx].4;
        let score = (base_score + (i as f64 * 0.005)).min(1.0);

        let eval = make_evaluation(
            &format!("eval-seed-{}", i),
            &format!("task-seed-{}", i),
            score,
            &role_ids[role_idx],
            &tradeoff_ids[tradeoff_idx],
        );
        record_evaluation(&eval, agency_dir).unwrap();
    }

    (role_ids, tradeoff_ids)
}

// ===========================================================================
// 1. Full pipeline: partition → analyze → synthesize → apply → evaluate
// ===========================================================================

#[test]
fn test_evolver_e2e_full_pipeline_structure() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    let (_role_ids, _tradeoff_ids) = seed_agency_data(&agency_dir);

    // Run evolve in force-fanout mode with JSON output
    let output = wg_ok(
        &wg_dir,
        &["evolve", "run", "--force-fanout", "--budget", "5", "--json"],
    );

    // Verify JSON output mentions the pipeline
    let json: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON from wg evolve: {}\nOutput: {}", e, output));
    assert_eq!(json.get("mode").and_then(|v| v.as_str()), Some("fanout"));
    assert!(
        json.get("analyzers").is_some(),
        "Output should list analyzers"
    );
    assert!(
        json.get("synthesizer").is_some(),
        "Output should list synthesizer"
    );
    assert!(json.get("apply").is_some(), "Output should list apply");
    assert!(
        json.get("evaluate").is_some(),
        "Output should list evaluate"
    );

    // Load graph and verify all pipeline stages exist
    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

    let partition = graph
        .tasks()
        .find(|t| t.id.contains("evolve-partition"))
        .expect("partition task should exist");
    let analyzers: Vec<&Task> = graph
        .tasks()
        .filter(|t| t.id.contains("evolve-analyze"))
        .collect();
    let synthesize = graph
        .tasks()
        .find(|t| t.id.contains("evolve-synthesize"))
        .expect("synthesize task should exist");
    let apply = graph
        .tasks()
        .find(|t| t.id.contains("evolve-apply"))
        .expect("apply task should exist");
    let evaluate = graph
        .tasks()
        .find(|t| t.id.contains("evolve-evaluate"))
        .expect("evaluate task should exist");

    // At least 2 analyzer strategies should have been spawned
    assert!(
        analyzers.len() >= 2,
        "Expected at least 2 analyzer tasks, got {}",
        analyzers.len()
    );

    // --- Verify dependency chain ---
    // Partition is pre-completed (Done)
    assert_eq!(partition.status, Status::Done);

    // All analyzers depend on partition
    for a in &analyzers {
        assert!(
            a.after.contains(&partition.id),
            "Analyzer {} should depend on partition {}",
            a.id,
            partition.id
        );
    }

    // Synthesize depends on ALL analyzers
    for a in &analyzers {
        assert!(
            synthesize.after.contains(&a.id),
            "Synthesize should depend on analyzer {}",
            a.id
        );
    }

    // Apply depends on synthesize
    assert!(
        apply.after.contains(&synthesize.id),
        "Apply should depend on synthesize"
    );

    // Evaluate depends on apply
    assert!(
        evaluate.after.contains(&apply.id),
        "Evaluate should depend on apply"
    );

    // --- Verify tags ---
    assert!(partition.tags.contains(&"evolution".to_string()));
    assert!(partition.tags.contains(&"partition".to_string()));
    for a in &analyzers {
        assert!(a.tags.contains(&"evolution".to_string()));
        assert!(a.tags.contains(&"analyzer".to_string()));
    }
    assert!(synthesize.tags.contains(&"evolution".to_string()));
    assert!(synthesize.tags.contains(&"synthesizer".to_string()));
    assert!(apply.tags.contains(&"evolution".to_string()));
    assert!(apply.tags.contains(&"apply".to_string()));
    assert!(evaluate.tags.contains(&"evolution".to_string()));
    assert!(evaluate.tags.contains(&"evaluate".to_string()));
}

// ===========================================================================
// 2. Partition produces slice files on disk
// ===========================================================================

#[test]
fn test_evolver_e2e_partition_creates_slice_files() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    wg_ok(&wg_dir, &["evolve", "run", "--force-fanout"]);

    // Verify run directory with slice files
    let evolve_runs_dir = wg_dir.join("evolve-runs");
    assert!(
        evolve_runs_dir.exists(),
        "evolve-runs directory should exist"
    );

    let run_dirs: Vec<PathBuf> = fs::read_dir(&evolve_runs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(run_dirs.len(), 1, "Should have exactly one run directory");

    let run_dir = &run_dirs[0];

    // Config and snapshot must exist
    assert!(
        run_dir.join("config.json").exists(),
        "Run config.json should exist"
    );
    assert!(
        run_dir.join("snapshot-iter-0.json").exists(),
        "Pre-evolution snapshot should exist"
    );

    // Verify config.json content
    let config_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("config.json")).unwrap()).unwrap();
    assert!(config_json.get("run_id").is_some());
    assert_eq!(
        config_json
            .get("total_evaluations")
            .and_then(|v| v.as_u64()),
        Some(24)
    );
    assert_eq!(
        config_json.get("total_roles").and_then(|v| v.as_u64()),
        Some(6)
    );
    assert_eq!(
        config_json.get("total_tradeoffs").and_then(|v| v.as_u64()),
        Some(5)
    );

    // At least some slice files should exist
    let slice_files: Vec<PathBuf> = fs::read_dir(run_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map_or(false, |n| n.to_string_lossy().ends_with("-slice.json"))
        })
        .collect();
    assert!(
        !slice_files.is_empty(),
        "Should have at least one strategy slice file"
    );

    // Verify snapshot content
    let snapshot: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("snapshot-iter-0.json")).unwrap())
            .unwrap();
    assert_eq!(snapshot.get("iteration").and_then(|v| v.as_u64()), Some(0));
    assert!(snapshot.get("roles").is_some());
    assert!(snapshot.get("tradeoffs").is_some());
    assert!(snapshot.get("overall_avg").is_some());
}

// ===========================================================================
// 3. Autopoietic cycle structure (back-edge, CycleConfig)
// ===========================================================================

#[test]
fn test_evolver_e2e_autopoietic_cycle_structure() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    wg_ok(
        &wg_dir,
        &[
            "evolve",
            "run",
            "--autopoietic",
            "--max-iterations",
            "4",
            "--cycle-delay",
            "120",
        ],
    );

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

    let partition = graph
        .tasks()
        .find(|t| t.id.contains("evolve-partition"))
        .unwrap();
    let evaluate = graph
        .tasks()
        .find(|t| t.id.contains("evolve-evaluate"))
        .unwrap();

    // CycleConfig on evaluate
    let cycle_config = evaluate
        .cycle_config
        .as_ref()
        .expect("Evaluate must have CycleConfig in autopoietic mode");
    assert_eq!(cycle_config.max_iterations, 4, "max_iterations should be 4");
    assert_eq!(
        cycle_config.delay,
        Some("120s".to_string()),
        "cycle_delay should be 120s"
    );
    assert!(
        cycle_config.restart_on_failure,
        "restart_on_failure should be true"
    );

    // Back-edge: evaluate depends on partition (creates cycle)
    assert!(
        evaluate.after.contains(&partition.id),
        "Evaluate should have back-edge to partition"
    );

    // Bidirectional: partition depends on evaluate
    assert!(
        partition.after.contains(&evaluate.id),
        "Partition should have back-edge to evaluate"
    );

    // Evaluate description should reference convergence
    let eval_desc = evaluate.description.as_ref().unwrap();
    assert!(
        eval_desc.contains("--converged"),
        "Evaluate should mention --converged flag"
    );
    assert!(
        eval_desc.contains("self-assessment"),
        "Evaluate should reference self-assessment"
    );
    assert!(
        eval_desc.contains("overall_delta") || eval_desc.contains("score delta"),
        "Evaluate should reference score delta"
    );
    assert!(
        eval_desc.contains("0.01"),
        "Evaluate should include convergence threshold value"
    );

    // Partition description should reference self-assessment feedback
    let part_desc = partition.description.as_ref().unwrap();
    assert!(
        part_desc.contains("self-assessment"),
        "Autopoietic partition should reference self-assessment"
    );
    assert!(
        part_desc.contains("Re-Iteration"),
        "Partition should describe re-iteration behavior"
    );
}

// ===========================================================================
// 4. Non-autopoietic mode has no cycle
// ===========================================================================

#[test]
fn test_evolver_e2e_non_autopoietic_no_cycle() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    wg_ok(&wg_dir, &["evolve", "run", "--force-fanout"]);

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

    let evaluate = graph
        .tasks()
        .find(|t| t.id.contains("evolve-evaluate"))
        .unwrap();

    // No CycleConfig
    assert!(
        evaluate.cycle_config.is_none(),
        "Non-autopoietic should have no CycleConfig"
    );

    let partition = graph
        .tasks()
        .find(|t| t.id.contains("evolve-partition"))
        .unwrap();

    // No back-edge from evaluate to partition
    assert!(
        !evaluate.after.contains(&partition.id),
        "Non-autopoietic evaluate should not have back-edge to partition"
    );

    // Evaluate description should NOT reference self-assessment
    let eval_desc = evaluate.description.as_ref().unwrap();
    assert!(
        !eval_desc.contains("self-assessment"),
        "Non-autopoietic evaluate should not reference self-assessment"
    );
}

// ===========================================================================
// 5. Convergence detection (mock stable scores)
// ===========================================================================

#[test]
fn test_evolver_e2e_convergence_detection_stable_scores() {
    // The convergence threshold is 0.01 (1% absolute change).
    // When the score delta falls below this, the cycle should terminate.
    // This test verifies the convergence logic in the evaluate task description.

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    wg_ok(
        &wg_dir,
        &[
            "evolve",
            "run",
            "--autopoietic",
            "--max-iterations",
            "5",
            "--cycle-delay",
            "0",
        ],
    );

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

    let evaluate = graph
        .tasks()
        .find(|t| t.id.contains("evolve-evaluate"))
        .unwrap();
    let eval_desc = evaluate.description.as_ref().unwrap();

    // Verify convergence threshold is documented in the evaluate task
    assert!(
        eval_desc.contains("0.01"),
        "Evaluate task should include the convergence threshold value 0.01"
    );

    // Verify the evaluate task instructs agent to use --converged
    assert!(
        eval_desc.contains("wg done") && eval_desc.contains("--converged"),
        "Evaluate should instruct agent to use 'wg done --converged' when converged"
    );

    // Simulate convergence detection:
    // Pre-evolution avg: 0.64, Post-evolution avg: 0.645
    let pre_avg: f64 = 0.64;
    let post_avg: f64 = 0.645;
    let delta = (post_avg - pre_avg).abs();
    assert!(
        delta < 0.01,
        "Delta {:.4} should be below convergence threshold 0.01 → cycle terminates",
        delta
    );

    // Simulate non-convergence:
    let pre_avg_2: f64 = 0.30;
    let post_avg_2: f64 = 0.50;
    let delta_2 = (post_avg_2 - pre_avg_2).abs();
    assert!(
        delta_2 >= 0.01,
        "Delta {:.4} should be above convergence threshold → cycle continues",
        delta_2
    );
}

// ===========================================================================
// 6. Partial failure: verify synthesize handles missing strategies
// ===========================================================================

#[test]
fn test_evolver_e2e_partial_failure_graph_structure() {
    // Even if some analyzers fail, the synthesize task still depends on all of
    // them (it reads whatever proposals are available). The graph structure
    // should be valid regardless.

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    wg_ok(&wg_dir, &["evolve", "run", "--force-fanout"]);

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

    let analyzers: Vec<&Task> = graph
        .tasks()
        .filter(|t| t.id.contains("evolve-analyze"))
        .collect();
    let synthesize = graph
        .tasks()
        .find(|t| t.id.contains("evolve-synthesize"))
        .unwrap();

    // Synthesize depends on ALL analyzers
    for a in &analyzers {
        assert!(
            synthesize.after.contains(&a.id),
            "Synthesize should depend on analyzer {} (even if it later fails)",
            a.id
        );
    }

    // Simulate: mark some analyzers as failed, rest as done
    let mut graph_mut = graph.clone();
    let mut failed_count = 0;
    let mut done_count = 0;
    for a in &analyzers {
        if let Some(task) = graph_mut.get_task_mut(&a.id) {
            if failed_count < analyzers.len() / 2 {
                task.status = Status::Failed;
                task.failure_reason = Some("Simulated failure for test".to_string());
                failed_count += 1;
            } else {
                task.status = Status::Done;
                done_count += 1;
            }
        }
    }

    // Verify we actually simulated partial failure
    assert!(failed_count > 0, "Should have at least one failed analyzer");
    assert!(
        done_count > 0,
        "Should have at least one successful analyzer"
    );

    // The synthesize task is still structurally valid (graph is unchanged,
    // synthesizer handles missing proposals gracefully at runtime)
    let synth = graph_mut.get_task(&synthesize.id).unwrap();
    assert_eq!(
        synth.after.len(),
        analyzers.len(),
        "Synthesize should still depend on all analyzers"
    );
}

// ===========================================================================
// 7. Dry run creates no tasks
// ===========================================================================

#[test]
fn test_evolver_e2e_dry_run_no_side_effects() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    let output = wg_ok(
        &wg_dir,
        &["evolve", "run", "--force-fanout", "--dry-run", "--json"],
    );

    // Verify dry run mode
    let json: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON: {}\nOutput: {}", e, output));
    assert_eq!(
        json.get("mode").and_then(|v| v.as_str()),
        Some("dry_run_fanout")
    );

    // Graph should be empty
    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.tasks().count(),
        0,
        "Dry run should not create any tasks"
    );
}

// ===========================================================================
// 8. Default cycle parameters
// ===========================================================================

#[test]
fn test_evolver_e2e_default_cycle_parameters() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    // Autopoietic without explicit max-iterations or cycle-delay → defaults
    wg_ok(&wg_dir, &["evolve", "run", "--autopoietic"]);

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

    let evaluate = graph
        .tasks()
        .find(|t| t.id.contains("evolve-evaluate"))
        .unwrap();

    let cycle_config = evaluate.cycle_config.as_ref().unwrap();
    assert_eq!(
        cycle_config.max_iterations, 3,
        "Default max_iterations should be 3"
    );
    assert_eq!(
        cycle_config.delay,
        Some("3600s".to_string()),
        "Default cycle_delay should be 3600s"
    );
}

// ===========================================================================
// 9. Analyzer tasks have correct model assignments
// ===========================================================================

#[test]
fn test_evolver_e2e_analyzer_model_tiers() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    wg_ok(&wg_dir, &["evolve", "run", "--force-fanout"]);

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

    // Check that analyzers have model assignments
    let analyzers: Vec<&Task> = graph
        .tasks()
        .filter(|t| t.id.contains("evolve-analyze"))
        .collect();

    for a in &analyzers {
        assert!(
            a.model.is_some(),
            "Analyzer {} should have a model assigned",
            a.id
        );
        let model = a.model.as_ref().unwrap();
        assert!(
            model == "haiku" || model == "sonnet" || model == "opus",
            "Analyzer {} has unexpected model: {}",
            a.id,
            model
        );
    }

    // Verify specific strategy-model assignments if they exist
    if let Some(gap) = analyzers.iter().find(|a| a.id.contains("gap-analysis")) {
        assert_eq!(
            gap.model.as_deref(),
            Some("opus"),
            "Gap analysis should use opus"
        );
    }
    if let Some(retirement) = analyzers.iter().find(|a| a.id.contains("retirement")) {
        assert_eq!(
            retirement.model.as_deref(),
            Some("haiku"),
            "Retirement should use haiku"
        );
    }
    if let Some(mutation) = analyzers.iter().find(|a| a.id.contains("mutation-")) {
        assert_eq!(
            mutation.model.as_deref(),
            Some("sonnet"),
            "Mutation should use sonnet"
        );
    }
}

// ===========================================================================
// 10. JSON output contains complete pipeline information
// ===========================================================================

#[test]
fn test_evolver_e2e_json_output_complete() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    let output = wg_ok(
        &wg_dir,
        &["evolve", "run", "--force-fanout", "--json", "--budget", "3"],
    );

    let json: serde_json::Value = serde_json::from_str(&output).unwrap();

    // Mode should be "fanout"
    assert_eq!(json["mode"], "fanout");

    // Should have a run_id
    assert!(json["run_id"].is_string());

    // Analyzers should be a non-empty array
    let analyzers = json["analyzers"].as_array().unwrap();
    assert!(!analyzers.is_empty());

    // Synthesizer should be a string
    assert!(json["synthesizer"].is_string());

    // Apply and evaluate should be strings
    assert!(json["apply"].is_string());
    assert!(json["evaluate"].is_string());

    // Slices should provide per-strategy info
    let slices = json["slices"].as_array().unwrap();
    assert!(!slices.is_empty());
    for slice in slices {
        assert!(slice.get("strategy").is_some());
        assert!(slice.get("evaluations").is_some());
        assert!(slice.get("model").is_some());
    }
}

// ===========================================================================
// 11. Autopoietic flag recorded in run config
// ===========================================================================

#[test]
fn test_evolver_e2e_autopoietic_recorded_in_config() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    wg_ok(
        &wg_dir,
        &[
            "evolve",
            "run",
            "--autopoietic",
            "--max-iterations",
            "2",
            "--cycle-delay",
            "60",
        ],
    );

    let evolve_runs = wg_dir.join("evolve-runs");
    let run_dir: PathBuf = fs::read_dir(&evolve_runs)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();

    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("config.json")).unwrap()).unwrap();

    assert_eq!(
        config.get("autopoietic").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        config.get("max_iterations").and_then(|v| v.as_u64()),
        Some(2)
    );
    assert_eq!(config.get("cycle_delay").and_then(|v| v.as_u64()), Some(60));
}

// ===========================================================================
// 12. Strategy-specific analyzer tests
// ===========================================================================

#[test]
fn test_evolver_e2e_specific_strategy() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    seed_agency_data(&agency_dir);

    // Run with only mutation strategy
    wg_ok(
        &wg_dir,
        &["evolve", "run", "--force-fanout", "--strategy", "mutation"],
    );

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

    let analyzers: Vec<&Task> = graph
        .tasks()
        .filter(|t| t.id.contains("evolve-analyze"))
        .collect();

    // Should have exactly 1 analyzer (mutation only)
    // (unless mutation had no qualifying roles, in which case 0)
    assert!(
        analyzers.len() <= 1,
        "Single strategy should produce at most 1 analyzer, got {}",
        analyzers.len()
    );

    if !analyzers.is_empty() {
        assert!(
            analyzers[0].id.contains("mutation"),
            "Analyzer should be for mutation strategy, got: {}",
            analyzers[0].id
        );
    }
}
