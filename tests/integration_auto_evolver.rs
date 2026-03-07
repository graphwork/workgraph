//! Smoke test for the auto-evolver cycle.
//!
//! Exercises:
//! 1. Bootstrap agency (roles, motivations, agents)
//! 2. Generate sufficient evaluations to trigger evolution (>=10)
//! 3. Verify should_trigger_evolution() returns true
//! 4. Verify .evolve-* meta-task created
//! 5. Verify evolver state file updated
//! 6. Verify reactive trigger fires on low average score
//! 7. Verify minimum interval enforced (2hr gap)
//! 8. Verify safe strategy subset (no crossover/bizarre-ideation)

use std::collections::HashMap;
use std::fs;
use tempfile::TempDir;

use workgraph::agency::{
    Evaluation, EvaluationRef, EvolverState, EvolutionTrigger, build_role, build_tradeoff,
    count_evaluation_files, init, recalculate_avg_score, record_evaluation, save_role,
    save_tradeoff, should_trigger_evolution,
};
use workgraph::agency::evolver;
use workgraph::config::AgencyConfig;
use workgraph::graph::{Node, Status, Task, WorkGraph, is_system_task};
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

fn setup_workgraph(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::create_dir_all(wg_dir.join("service")).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    (wg_dir, graph_path)
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
        timestamp: format!("2025-06-01T12:00:{}Z", id.len() % 60),
        model: Some("test-model".to_string()),
        source: "llm".to_string(),
    }
}

/// Bootstrap a minimal agency with one role and one tradeoff, returning their IDs.
fn bootstrap_agency(agency_dir: &std::path::Path) -> (String, String) {
    init(agency_dir).unwrap();

    let role = build_role(
        "Implementer",
        "Writes code to fulfil task requirements",
        vec!["rust".to_string()],
        "Working, tested code",
    );
    let role_id = role.id.clone();
    save_role(&role, &agency_dir.join("cache/roles")).unwrap();

    let tradeoff = build_tradeoff(
        "Quality First",
        "Prioritise correctness and maintainability",
        vec!["Slower delivery for higher quality".into()],
        vec!["Skipping tests".into()],
    );
    let tradeoff_id = tradeoff.id.clone();
    save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

    (role_id, tradeoff_id)
}

/// Seed N evaluations into the agency directory, returning the count written.
fn seed_evaluations(
    agency_dir: &std::path::Path,
    role_id: &str,
    tradeoff_id: &str,
    count: u32,
    score: f64,
) -> u32 {
    for i in 0..count {
        let eval = make_evaluation(
            &format!("eval-seed-{}", i),
            &format!("task-seed-{}", i),
            score,
            role_id,
            tradeoff_id,
        );
        record_evaluation(&eval, agency_dir).unwrap();
    }
    count
}

// ===========================================================================
// 1. Evolution triggers after threshold evaluations
// ===========================================================================

#[test]
fn test_smoke_evolver_threshold_trigger() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    let (role_id, tradeoff_id) = bootstrap_agency(&agency_dir);

    // Seed 12 evaluations (threshold = 10)
    seed_evaluations(&agency_dir, &role_id, &tradeoff_id, 12, 0.7);

    let eval_count = count_evaluation_files(&agency_dir.join("evaluations"));
    assert_eq!(eval_count, 12, "Should have 12 evaluation files on disk");

    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 10,
        evolution_interval: 0, // no interval restriction for this test
        ..AgencyConfig::default()
    };
    let state = EvolverState::default();

    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(
        trigger.is_some(),
        "should_trigger_evolution must return Some after 12 evals (threshold=10)"
    );
    match trigger.unwrap() {
        EvolutionTrigger::Threshold { new_evals } => {
            assert_eq!(new_evals, 12);
        }
        other => panic!("Expected Threshold trigger, got {:?}", other),
    }
}

// ===========================================================================
// 2. .evolve-* task created with correct metadata
// ===========================================================================

#[test]
fn test_smoke_evolver_creates_evolve_task() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, graph_path) = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    let (role_id, tradeoff_id) = bootstrap_agency(&agency_dir);

    seed_evaluations(&agency_dir, &role_id, &tradeoff_id, 12, 0.7);

    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 10,
        evolution_interval: 0,
        ..AgencyConfig::default()
    };
    let state = EvolverState::default();

    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(trigger.is_some());

    let budget = evolver::evolution_budget(&config);
    let trigger_reason = match trigger.unwrap() {
        EvolutionTrigger::Threshold { new_evals } => {
            format!("Threshold trigger: {} new evaluations", new_evals)
        }
        EvolutionTrigger::Reactive { avg_score } => {
            format!("Reactive trigger: avg score {:.2}", avg_score)
        }
    };

    // Simulate coordinator creating the .evolve-* task
    let mut graph = load_graph(&graph_path).unwrap();
    let evolve_task_id = ".evolve-auto-smoke";
    let mut evolve_task = make_task(
        evolve_task_id,
        &format!("Auto-evolve: {}", trigger_reason),
        Status::Open,
    );
    evolve_task.tags = vec!["evolution".to_string(), "agency".to_string()];
    evolve_task.exec = Some(format!("wg evolve --budget {}", budget));
    evolve_task.exec_mode = Some("bare".to_string());
    graph.add_node(Node::Task(evolve_task));
    save_graph(&graph, &graph_path).unwrap();

    // Verify
    let final_graph = load_graph(&graph_path).unwrap();
    let evolve = final_graph.get_task(evolve_task_id).unwrap();
    assert!(
        is_system_task(&evolve.id),
        "Evolve task must be a dot-prefixed system task"
    );
    assert!(evolve.tags.contains(&"evolution".to_string()));
    assert!(evolve.tags.contains(&"agency".to_string()));
    assert!(
        evolve.exec.as_ref().unwrap().contains("wg evolve"),
        "exec should contain wg evolve command"
    );
    assert_eq!(evolve.exec_mode.as_deref(), Some("bare"));
}

// ===========================================================================
// 3. Evolver state file tracks history
// ===========================================================================

#[test]
fn test_smoke_evolver_state_tracks_history() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    fs::create_dir_all(&agency_dir).unwrap();

    // Fresh state
    let mut state = EvolverState::default();
    assert_eq!(state.last_eval_count, 0);
    assert!(state.history.is_empty());
    assert!(state.last_evolution_at.is_none());

    // First cycle
    state.record_evolution(
        "run-001",
        10,
        3,
        vec!["mutation".to_string(), "gap-analysis".to_string()],
        Some(0.72),
        Some(".evolve-auto-001"),
    );
    state.save(&agency_dir).unwrap();

    // Verify persisted
    let loaded = EvolverState::load(&agency_dir);
    assert_eq!(loaded.last_eval_count, 10);
    assert!(loaded.last_evolution_at.is_some());
    assert_eq!(loaded.history.len(), 1);
    assert_eq!(loaded.history[0].run_id, "run-001");
    assert_eq!(loaded.history[0].evaluations_consumed, 10);
    assert_eq!(loaded.history[0].operations_applied, 3);
    assert_eq!(
        loaded.history[0].strategies_used,
        vec!["mutation".to_string(), "gap-analysis".to_string()]
    );
    assert!(
        (loaded.history[0].pre_evolution_avg_score.unwrap() - 0.72).abs() < f64::EPSILON
    );
    assert_eq!(
        loaded.history[0].task_id.as_deref(),
        Some(".evolve-auto-001")
    );

    // Second cycle accumulates
    let mut state2 = loaded;
    state2.record_evolution(
        "run-002",
        8,
        2,
        vec!["retirement".to_string()],
        Some(0.78),
        Some(".evolve-auto-002"),
    );
    state2.save(&agency_dir).unwrap();

    let final_state = EvolverState::load(&agency_dir);
    assert_eq!(final_state.last_eval_count, 18); // 10 + 8
    assert_eq!(final_state.history.len(), 2);
    assert_eq!(final_state.history[1].run_id, "run-002");

    // Verify state file exists on disk
    let state_path = EvolverState::path(&agency_dir);
    assert!(state_path.exists(), "Evolver state file must exist on disk");
}

// ===========================================================================
// 4. Reactive trigger works on score drop
// ===========================================================================

#[test]
fn test_smoke_evolver_reactive_trigger_low_scores() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    let (role_id, tradeoff_id) = bootstrap_agency(&agency_dir);

    // Seed 5 low-scoring evaluations (below reactive threshold 0.4)
    seed_evaluations(&agency_dir, &role_id, &tradeoff_id, 5, 0.15);

    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 100, // Very high — won't trigger via threshold
        evolution_interval: 0,
        evolution_reactive_threshold: 0.4,
        ..AgencyConfig::default()
    };
    let state = EvolverState::default();

    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(
        trigger.is_some(),
        "Reactive trigger must fire when average score is below threshold"
    );
    match trigger.unwrap() {
        EvolutionTrigger::Reactive { avg_score } => {
            assert!(
                avg_score < 0.4,
                "Average score {:.2} should be below reactive threshold 0.4",
                avg_score
            );
        }
        other => panic!("Expected Reactive trigger, got {:?}", other),
    }
}

// ===========================================================================
// 5. Interval enforcement prevents rapid re-evolution
// ===========================================================================

#[test]
fn test_smoke_evolver_interval_enforcement() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    let (role_id, tradeoff_id) = bootstrap_agency(&agency_dir);

    // Seed 15 good-scoring evaluations (above reactive threshold)
    seed_evaluations(&agency_dir, &role_id, &tradeoff_id, 15, 0.7);

    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 10,
        evolution_interval: 7200, // 2 hours
        evolution_reactive_threshold: 0.4,
        ..AgencyConfig::default()
    };

    // State shows evolution happened just now — interval not met
    let state = EvolverState {
        last_eval_count: 0,
        last_evolution_at: Some(chrono::Utc::now().to_rfc3339()),
        history: vec![],
        baselines: Default::default(),
    };

    // Despite 15 new evals exceeding threshold, interval blocks threshold trigger.
    // And scores are good (0.7 > 0.4), so reactive trigger doesn't fire either.
    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(
        trigger.is_none(),
        "Evolution must NOT trigger when interval has not elapsed and scores are healthy"
    );

    // Now test that reactive trigger CAN bypass interval when scores are low
    let agency_dir2 = tmp.path().join("agency2");
    let (role_id2, tradeoff_id2) = bootstrap_agency(&agency_dir2);
    seed_evaluations(&agency_dir2, &role_id2, &tradeoff_id2, 5, 0.1);

    let state2 = EvolverState {
        last_eval_count: 0,
        last_evolution_at: Some(chrono::Utc::now().to_rfc3339()),
        history: vec![],
        baselines: Default::default(),
    };

    let trigger2 = should_trigger_evolution(&agency_dir2, &config, &state2);
    assert!(
        trigger2.is_some(),
        "Reactive trigger should bypass interval when scores are critically low"
    );
    assert!(
        matches!(trigger2.unwrap(), EvolutionTrigger::Reactive { .. }),
        "Should be a Reactive trigger"
    );
}

// ===========================================================================
// 6. Safe strategy subset (no crossover/bizarre-ideation)
// ===========================================================================

#[test]
fn test_smoke_evolver_safe_strategies() {
    // Only safe strategies are allowed in automatic evolution
    let safe = evolver::SAFE_STRATEGIES;
    assert!(safe.contains(&"mutation"), "mutation must be safe");
    assert!(safe.contains(&"gap-analysis"), "gap-analysis must be safe");
    assert!(safe.contains(&"retirement"), "retirement must be safe");
    assert!(
        safe.contains(&"motivation-tuning"),
        "motivation-tuning must be safe"
    );

    // Dangerous strategies MUST be excluded
    assert!(
        !safe.contains(&"crossover"),
        "crossover must NOT be in safe strategies"
    );
    assert!(
        !safe.contains(&"bizarre-ideation"),
        "bizarre-ideation must NOT be in safe strategies"
    );
}

// ===========================================================================
// 7. Full end-to-end: bootstrap → evaluate → trigger → evolve → state update
// ===========================================================================

#[test]
fn test_smoke_evolver() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, graph_path) = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    let (role_id, tradeoff_id) = bootstrap_agency(&agency_dir);

    // --- Step 1: Populate graph with completed tasks ---
    let mut graph = WorkGraph::new();
    for i in 0..12 {
        let mut task = make_task(
            &format!("task-{}", i),
            &format!("Implement feature {}", i),
            Status::Done,
        );
        task.agent = Some(format!("agent-{}", i % 3));
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &graph_path).unwrap();

    // --- Step 2: Record evaluations (scores 0.6..0.93) ---
    for i in 0..12 {
        let eval = make_evaluation(
            &format!("eval-{}", i),
            &format!("task-{}", i),
            0.6 + (i as f64 * 0.03),
            &role_id,
            &tradeoff_id,
        );
        record_evaluation(&eval, &agency_dir).unwrap();
    }

    let eval_count = count_evaluation_files(&agency_dir.join("evaluations"));
    assert_eq!(eval_count, 12);

    // --- Step 3: Check trigger fires ---
    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 10,
        evolution_interval: 0,
        ..AgencyConfig::default()
    };
    let state = EvolverState::default();

    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(trigger.is_some(), "Evolution should trigger after 12 evals");
    assert!(matches!(
        trigger.unwrap(),
        EvolutionTrigger::Threshold { new_evals: 12 }
    ));

    // --- Step 4: Create .evolve-* task ---
    let mut graph = load_graph(&graph_path).unwrap();
    let evolve_task_id = ".evolve-auto-smoke-e2e";
    let budget = evolver::evolution_budget(&config);
    let mut evolve_task = make_task(
        evolve_task_id,
        "Auto-evolve: Threshold trigger: 12 new evaluations",
        Status::Open,
    );
    evolve_task.tags = vec!["evolution".to_string(), "agency".to_string()];
    evolve_task.exec = Some(format!("wg evolve --budget {}", budget));
    evolve_task.exec_mode = Some("bare".to_string());
    graph.add_node(Node::Task(evolve_task));
    save_graph(&graph, &graph_path).unwrap();

    let final_graph = load_graph(&graph_path).unwrap();
    let evolve = final_graph.get_task(evolve_task_id).unwrap();
    assert!(is_system_task(&evolve.id));
    assert!(evolve.tags.contains(&"evolution".to_string()));

    // --- Step 5: Simulate evolver completing — update state ---
    let avg_score = {
        let refs: Vec<EvaluationRef> = (0..12)
            .map(|i| EvaluationRef {
                score: 0.6 + (i as f64 * 0.03),
                task_id: format!("task-{}", i),
                timestamp: "2025-06-01T12:00:00Z".to_string(),
                context_id: tradeoff_id.clone(),
            })
            .collect();
        recalculate_avg_score(&refs)
    };

    let mut evolver_state = EvolverState::default();
    evolver_state.record_evolution(
        "run-smoke-e2e",
        12,
        3,
        vec!["mutation".to_string()],
        avg_score,
        Some(evolve_task_id),
    );
    evolver_state.save(&agency_dir).unwrap();

    // --- Step 6: Verify final state ---
    let final_state = EvolverState::load(&agency_dir);
    assert_eq!(final_state.last_eval_count, 12);
    assert_eq!(final_state.history.len(), 1);
    assert_eq!(final_state.history[0].run_id, "run-smoke-e2e");
    assert_eq!(final_state.history[0].evaluations_consumed, 12);
    assert_eq!(final_state.history[0].operations_applied, 3);
    assert!(final_state.history[0].pre_evolution_avg_score.is_some());
    assert_eq!(
        final_state.history[0].task_id.as_deref(),
        Some(".evolve-auto-smoke-e2e")
    );

    // Verify only safe strategies were used
    for strategy in &final_state.history[0].strategies_used {
        assert!(
            evolver::SAFE_STRATEGIES.contains(&strategy.as_str()),
            "Strategy '{}' is not in the safe subset",
            strategy
        );
    }

    // --- Step 7: No re-trigger without new evaluations ---
    let trigger_after = should_trigger_evolution(&agency_dir, &config, &final_state);
    assert!(
        trigger_after.is_none(),
        "Must NOT re-trigger without new evaluations"
    );

    // --- Step 8: Verify graph totals ---
    let final_graph = load_graph(&graph_path).unwrap();
    assert_eq!(final_graph.tasks().count(), 13); // 12 tasks + 1 evolve task
    let evolve_tasks: Vec<_> = final_graph
        .tasks()
        .filter(|t| t.id.starts_with(".evolve-"))
        .collect();
    assert_eq!(evolve_tasks.len(), 1);

    // --- Step 9: Budget cap respected ---
    assert!(budget <= 5, "Budget should be capped at DEFAULT_MAX_OPS=5");
}

// ===========================================================================
// 8. Evolution does NOT trigger when auto_evolve = false
// ===========================================================================

#[test]
fn test_smoke_evolver_disabled() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    let (role_id, tradeoff_id) = bootstrap_agency(&agency_dir);

    seed_evaluations(&agency_dir, &role_id, &tradeoff_id, 20, 0.7);

    let config = AgencyConfig {
        auto_evolve: false,
        evolution_threshold: 5,
        ..AgencyConfig::default()
    };
    let state = EvolverState::default();

    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(trigger.is_none(), "Must not trigger when auto_evolve=false");
}

// ===========================================================================
// 9. Budget cap enforced
// ===========================================================================

#[test]
fn test_smoke_evolver_budget_cap() {
    let low = AgencyConfig {
        evolution_budget: 3,
        ..AgencyConfig::default()
    };
    assert_eq!(evolver::evolution_budget(&low), 3);

    let high = AgencyConfig {
        evolution_budget: 100,
        ..AgencyConfig::default()
    };
    assert_eq!(
        evolver::evolution_budget(&high),
        5,
        "Budget must be capped at DEFAULT_MAX_OPS"
    );
}
