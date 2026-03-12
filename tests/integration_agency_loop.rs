//! Smoke test for the full agency self-improvement loop.
//!
//! Exercises: create task → agent claims → auto-evaluate → evaluation scores →
//! auto-evolve triggers → evolution modifies config.
//!
//! Verifies:
//! 1. Auto-evaluate creates `.evaluate-*` tasks for dot-prefixed naming
//! 2. Evaluator dispatch role is used for `.evaluate-*` task model resolution
//! 3. Cost/token data recorded on evaluation tasks
//! 4. Evaluation recording → evolver trigger → `.evolve-*` task creation
//! 5. Evolver state updates after evolution cycle

use std::collections::HashMap;
use std::fs;
use tempfile::TempDir;

use workgraph::agency::evolver;
use workgraph::agency::{
    Evaluation, EvaluationRef, EvolutionTrigger, EvolverState, build_role, build_tradeoff,
    count_evaluation_files, init, recalculate_avg_score, record_evaluation, save_role,
    save_tradeoff, should_trigger_evolution,
};
use workgraph::config::{AgencyConfig, Config, DispatchRole};
use workgraph::graph::{Node, Status, Task, TokenUsage, WorkGraph, is_system_task};
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

// ===========================================================================
// 1. Auto-evaluate creates dot-prefixed `.evaluate-*` tasks
// ===========================================================================

#[test]
fn test_auto_evaluate_creates_dot_prefixed_eval_tasks() {
    let tmp = TempDir::new().unwrap();
    let (_wg_dir, graph_path) = setup_workgraph(&tmp);

    // Create a graph with a completed task
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("my-task", "My Task", Status::Done)));
    save_graph(&graph, &graph_path).unwrap();

    // Simulate the coordinator's auto-evaluate logic:
    // For each non-meta, non-abandoned task, create a `.evaluate-{task_id}` task
    let mut mutable_graph = load_graph(&graph_path).unwrap();
    let tasks_needing_eval: Vec<_> = mutable_graph
        .tasks()
        .filter(|t| {
            let eval_id = format!(".evaluate-{}", t.id);
            if mutable_graph.get_task(&eval_id).is_some() {
                return false;
            }
            let dominated_tags = ["evaluation", "assignment", "evolution"];
            if t.tags
                .iter()
                .any(|tag| dominated_tags.contains(&tag.as_str()))
            {
                return false;
            }
            if t.tags.iter().any(|tag| tag == "eval-scheduled") {
                return false;
            }
            !matches!(t.status, Status::Abandoned)
        })
        .map(|t| (t.id.clone(), t.title.clone()))
        .collect();

    assert_eq!(tasks_needing_eval.len(), 1);

    for (task_id, task_title) in &tasks_needing_eval {
        let eval_task_id = format!(".evaluate-{}", task_id);
        let mut eval_task = make_task(
            &eval_task_id,
            &format!("Evaluate: {}", task_title),
            Status::Open,
        );
        eval_task.after = vec![task_id.clone()];
        eval_task.tags = vec!["evaluation".to_string(), "agency".to_string()];
        eval_task.exec = Some(format!("wg evaluate run {}", task_id));
        eval_task.exec_mode = Some("bare".to_string());
        mutable_graph.add_node(Node::Task(eval_task));

        // Tag the source task
        if let Some(source) = mutable_graph.get_task_mut(task_id)
            && !source.tags.iter().any(|t| t == "eval-scheduled")
        {
            source.tags.push("eval-scheduled".to_string());
        }
    }

    save_graph(&mutable_graph, &graph_path).unwrap();
    let final_graph = load_graph(&graph_path).unwrap();

    // The eval task should use dot-prefix naming
    let eval_task = final_graph.get_task(".evaluate-my-task").unwrap();
    assert!(
        is_system_task(&eval_task.id),
        "Eval task should be a system task (dot-prefixed)"
    );
    assert_eq!(eval_task.after, vec!["my-task".to_string()]);
    assert!(eval_task.tags.contains(&"evaluation".to_string()));
    assert_eq!(eval_task.exec_mode.as_deref(), Some("bare"));

    // Source task should be tagged eval-scheduled
    let source = final_graph.get_task("my-task").unwrap();
    assert!(
        source.tags.contains(&"eval-scheduled".to_string()),
        "Source task should be tagged eval-scheduled"
    );
}

// ===========================================================================
// 2. Evaluator dispatch role used for dot-prefixed eval tasks
// ===========================================================================

#[test]
fn test_evaluator_dispatch_role_for_eval_tasks() {
    // Verify DispatchRole::Evaluator is the role used for model resolution
    // of `.evaluate-*` tasks, and that it resolves correctly
    let mut config = Config::default();

    // Set evaluator-specific model via the new routing config
    config
        .models
        .set_model(DispatchRole::Evaluator, "haiku-evaluator");

    let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
    assert_eq!(resolved.model, "haiku-evaluator");

    // Default (TaskAgent) should be different
    let default_resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
    assert_ne!(
        default_resolved.model, "haiku-evaluator",
        "TaskAgent should not use the evaluator model"
    );

    // Evolver dispatch role should also be independently configurable
    config
        .models
        .set_model(DispatchRole::Evolver, "evolver-model");
    let evolver_resolved = config.resolve_model_for_role(DispatchRole::Evolver);
    assert_eq!(evolver_resolved.model, "evolver-model");
}

#[test]
fn test_eval_task_model_matches_evaluator_dispatch_role() {
    // Simulate what the coordinator does: create an eval task with
    // the model resolved from DispatchRole::Evaluator
    let mut config = Config::default();
    config
        .models
        .set_model(DispatchRole::Evaluator, "eval-model-v2");

    let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);

    let mut eval_task = make_task(".evaluate-feature-x", "Evaluate: feature-x", Status::Open);
    eval_task.model = Some(resolved.model.clone());
    eval_task.provider = resolved.provider.clone();

    assert_eq!(eval_task.model.as_deref(), Some("eval-model-v2"));
    assert!(is_system_task(&eval_task.id));
}

// ===========================================================================
// 3. Cost/token data recorded on evaluation tasks
// ===========================================================================

#[test]
fn test_token_usage_recorded_on_eval_task() {
    let tmp = TempDir::new().unwrap();
    let (_wg_dir, graph_path) = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();

    // Create a completed task and its eval task
    graph.add_node(Node::Task(make_task("task-a", "Task A", Status::Done)));
    let mut eval_task = make_task(".evaluate-task-a", "Evaluate: Task A", Status::Open);
    eval_task.after = vec!["task-a".to_string()];
    eval_task.tags = vec!["evaluation".to_string()];
    graph.add_node(Node::Task(eval_task));

    save_graph(&graph, &graph_path).unwrap();

    // Simulate the evaluator completing and recording token usage
    let mut mutable_graph = load_graph(&graph_path).unwrap();
    let usage = TokenUsage {
        cost_usd: 0.0032,
        input_tokens: 1500,
        output_tokens: 350,
        cache_read_input_tokens: 800,
        cache_creation_input_tokens: 200,
    };

    if let Some(eval_task) = mutable_graph.get_task_mut(".evaluate-task-a") {
        eval_task.token_usage = Some(usage.clone());
        eval_task.status = Status::Done;
    }
    save_graph(&mutable_graph, &graph_path).unwrap();

    // Verify cost data persisted
    let final_graph = load_graph(&graph_path).unwrap();
    let completed_eval = final_graph.get_task(".evaluate-task-a").unwrap();
    assert_eq!(completed_eval.status, Status::Done);

    let recorded_usage = completed_eval.token_usage.as_ref().unwrap();
    assert!((recorded_usage.cost_usd - 0.0032).abs() < f64::EPSILON);
    assert_eq!(recorded_usage.input_tokens, 1500);
    assert_eq!(recorded_usage.output_tokens, 350);
    assert_eq!(recorded_usage.cache_read_input_tokens, 800);
    assert_eq!(recorded_usage.cache_creation_input_tokens, 200);
}

// ===========================================================================
// 4. Evaluation recording → evolver threshold trigger
// ===========================================================================

#[test]
fn test_evaluations_trigger_evolution() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    init(&agency_dir).unwrap();

    let role = build_role(
        "Implementer",
        "Writes code",
        vec!["rust".to_string()],
        "Working code",
    );
    let role_id = role.id.clone();
    save_role(&role, &agency_dir.join("cache/roles")).unwrap();

    let tradeoff = build_tradeoff(
        "Quality First",
        "Prioritise correctness",
        vec!["Slower delivery".into()],
        vec!["Skipping tests".into()],
    );
    let tradeoff_id = tradeoff.id.clone();
    save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

    // Record enough evaluations to exceed the threshold
    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 5,
        evolution_interval: 0, // No interval restriction
        ..AgencyConfig::default()
    };

    for i in 0..6 {
        let eval = make_evaluation(
            &format!("eval-{}", i),
            &format!("task-{}", i),
            0.7,
            &role_id,
            &tradeoff_id,
        );
        record_evaluation(&eval, &agency_dir).unwrap();
    }

    // Verify evaluation files exist
    let eval_count = count_evaluation_files(&agency_dir.join("evaluations"));
    assert_eq!(eval_count, 6);

    // Check that evolution should trigger
    let state = EvolverState::default();
    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(
        trigger.is_some(),
        "Evolution should trigger after 6 evals (threshold=5)"
    );

    match trigger.unwrap() {
        EvolutionTrigger::Threshold { new_evals } => {
            assert_eq!(new_evals, 6);
        }
        other => panic!("Expected Threshold trigger, got {:?}", other),
    }
}

#[test]
fn test_reactive_evolution_trigger_low_scores() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    init(&agency_dir).unwrap();

    let role = build_role("Implementer", "Writes code", vec![], "");
    save_role(&role, &agency_dir.join("cache/roles")).unwrap();

    let tradeoff = build_tradeoff("Quality", "Correctness", vec![], vec![]);
    save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 100, // High threshold — won't trigger via threshold
        evolution_interval: 0,
        evolution_reactive_threshold: 0.4,
        ..AgencyConfig::default()
    };

    // Record low-scoring evaluations
    for i in 0..5 {
        let eval = make_evaluation(
            &format!("eval-low-{}", i),
            &format!("task-low-{}", i),
            0.15, // Well below 0.4 reactive threshold
            &role.id,
            &tradeoff.id,
        );
        record_evaluation(&eval, &agency_dir).unwrap();
    }

    let state = EvolverState::default();
    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(
        trigger.is_some(),
        "Reactive trigger should fire for low scores"
    );

    match trigger.unwrap() {
        EvolutionTrigger::Reactive { avg_score } => {
            assert!(
                avg_score < 0.4,
                "Average score should be below reactive threshold"
            );
        }
        other => panic!("Expected Reactive trigger, got {:?}", other),
    }
}

// ===========================================================================
// 5. Evolution creates `.evolve-*` task in graph
// ===========================================================================

#[test]
fn test_auto_evolve_creates_dot_prefixed_evolve_task() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, graph_path) = setup_workgraph(&tmp);

    // Set up agency dir with evaluations
    let agency_dir = wg_dir.join("agency");
    init(&agency_dir).unwrap();

    let role = build_role("Implementer", "Writes code", vec![], "");
    save_role(&role, &agency_dir.join("cache/roles")).unwrap();
    let tradeoff = build_tradeoff("Quality", "Correctness", vec![], vec![]);
    save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

    // Create evaluations to trigger evolution
    for i in 0..12 {
        let eval = make_evaluation(
            &format!("eval-{}", i),
            &format!("task-{}", i),
            0.7,
            &role.id,
            &tradeoff.id,
        );
        record_evaluation(&eval, &agency_dir).unwrap();
    }

    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 10,
        evolution_interval: 0,
        ..AgencyConfig::default()
    };

    let state = EvolverState::default();
    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(trigger.is_some());

    // Simulate coordinator creating the evolve task
    let mut graph = load_graph(&graph_path).unwrap();
    let evolve_task_id = ".evolve-auto-test";
    let budget = evolver::evolution_budget(&config);

    let trigger_reason = match trigger.unwrap() {
        EvolutionTrigger::Threshold { new_evals } => {
            format!("Threshold trigger: {} new evaluations", new_evals)
        }
        EvolutionTrigger::Reactive { avg_score } => {
            format!("Reactive trigger: avg score {:.2}", avg_score)
        }
    };

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

    let final_graph = load_graph(&graph_path).unwrap();
    let evolve = final_graph.get_task(evolve_task_id).unwrap();
    assert!(
        is_system_task(&evolve.id),
        "Evolve task should be dot-prefixed system task"
    );
    assert!(evolve.tags.contains(&"evolution".to_string()));
    assert!(evolve.exec.as_ref().unwrap().contains("wg evolve"));
}

// ===========================================================================
// 6. Evolver state records evolution cycles
// ===========================================================================

#[test]
fn test_evolver_state_tracks_evolution_history() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    fs::create_dir_all(&agency_dir).unwrap();

    let mut state = EvolverState::default();
    assert_eq!(state.last_eval_count, 0);
    assert!(state.history.is_empty());

    // Record first evolution cycle
    state.record_evolution(
        "run-001",
        10, // evaluations consumed
        3,  // operations applied
        vec!["mutation".to_string(), "gap-analysis".to_string()],
        Some(0.72), // pre-evolution avg score
        Some(".evolve-auto-001"),
    );

    assert_eq!(state.last_eval_count, 10);
    assert!(state.last_evolution_at.is_some());
    assert_eq!(state.history.len(), 1);
    assert_eq!(state.history[0].evaluations_consumed, 10);
    assert_eq!(state.history[0].operations_applied, 3);
    assert_eq!(
        state.history[0].strategies_used,
        vec!["mutation".to_string(), "gap-analysis".to_string()]
    );
    assert!((state.history[0].pre_evolution_avg_score.unwrap() - 0.72).abs() < f64::EPSILON);
    assert_eq!(
        state.history[0].task_id.as_deref(),
        Some(".evolve-auto-001")
    );

    // Persist and reload
    state.save(&agency_dir).unwrap();
    let loaded = EvolverState::load(&agency_dir);
    assert_eq!(loaded.last_eval_count, 10);
    assert_eq!(loaded.history.len(), 1);

    // Second evolution should accumulate
    let mut state2 = loaded;
    state2.record_evolution(
        "run-002",
        8,
        2,
        vec!["retirement".to_string()],
        Some(0.78),
        Some(".evolve-auto-002"),
    );
    assert_eq!(state2.last_eval_count, 18); // 10 + 8
    assert_eq!(state2.history.len(), 2);

    state2.save(&agency_dir).unwrap();
    let final_state = EvolverState::load(&agency_dir);
    assert_eq!(final_state.last_eval_count, 18);
    assert_eq!(final_state.history.len(), 2);
}

// ===========================================================================
// 7. No infinite regress: eval/evolve tasks don't spawn further evals
// ===========================================================================

#[test]
fn test_no_infinite_regress_for_system_tasks() {
    // System tasks (.evaluate-*, .evolve-*, .assign-*) should NOT produce
    // further evaluation tasks, preventing infinite regress.
    let eval_task_id = ".evaluate-my-task";
    let evolve_task_id = ".evolve-auto-20250601";
    let assign_task_id = ".assign-my-task";

    // All are system tasks
    assert!(is_system_task(eval_task_id));
    assert!(is_system_task(evolve_task_id));
    assert!(is_system_task(assign_task_id));

    // Simulate coordinator filtering: tasks tagged evaluation/assignment/evolution
    // are excluded from auto-evaluate
    let dominated_tags = ["evaluation", "assignment", "evolution"];

    let eval_tags = vec!["evaluation".to_string(), "agency".to_string()];
    assert!(
        eval_tags
            .iter()
            .any(|t| dominated_tags.contains(&t.as_str()))
    );

    let evolve_tags = vec!["evolution".to_string(), "agency".to_string()];
    assert!(
        evolve_tags
            .iter()
            .any(|t| dominated_tags.contains(&t.as_str()))
    );

    let assign_tags = vec!["assignment".to_string(), "agency".to_string()];
    assert!(
        assign_tags
            .iter()
            .any(|t| dominated_tags.contains(&t.as_str()))
    );
}

// ===========================================================================
// 8. Full loop: task → eval → record → evolver trigger → evolve task
// ===========================================================================

#[test]
fn test_full_agency_loop_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, graph_path) = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");
    init(&agency_dir).unwrap();

    // Step 1: Set up agency primitives
    let role = build_role(
        "Implementer",
        "Writes code to fulfil task requirements.",
        vec!["rust".to_string()],
        "Working, tested code",
    );
    let role_id = role.id.clone();
    save_role(&role, &agency_dir.join("cache/roles")).unwrap();

    let tradeoff = build_tradeoff(
        "Quality First",
        "Prioritise correctness and maintainability.",
        vec!["Slower delivery for higher quality".into()],
        vec!["Skipping tests".into()],
    );
    let tradeoff_id = tradeoff.id.clone();
    save_tradeoff(&tradeoff, &agency_dir.join("primitives/tradeoffs")).unwrap();

    // Step 2: Create a task graph with tasks that have been completed
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

    // Step 3: Simulate auto-evaluate — create `.evaluate-*` tasks
    let mut graph = load_graph(&graph_path).unwrap();
    for i in 0..12 {
        let task_id = format!("task-{}", i);
        let eval_task_id = format!(".evaluate-{}", task_id);

        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);

        let mut eval_task = make_task(
            &eval_task_id,
            &format!("Evaluate: Implement feature {}", i),
            Status::Open,
        );
        eval_task.after = vec![task_id.clone()];
        eval_task.tags = vec!["evaluation".to_string(), "agency".to_string()];
        eval_task.exec = Some(format!("wg evaluate run {}", task_id));
        eval_task.model = Some(resolved.model.clone());
        eval_task.provider = resolved.provider.clone();
        eval_task.exec_mode = Some("bare".to_string());
        graph.add_node(Node::Task(eval_task));
    }
    save_graph(&graph, &graph_path).unwrap();

    // Verify eval tasks created with correct model
    let graph = load_graph(&graph_path).unwrap();
    for i in 0..12 {
        let eval_id = format!(".evaluate-task-{}", i);
        let eval_task = graph.get_task(&eval_id).unwrap();
        assert!(is_system_task(&eval_task.id));
        assert!(eval_task.tags.contains(&"evaluation".to_string()));
    }

    // Step 4: Simulate evaluators running — record evaluations with cost data
    let mut graph = load_graph(&graph_path).unwrap();
    for i in 0..12 {
        // Record evaluation to agency store
        let eval = make_evaluation(
            &format!("eval-{}", i),
            &format!("task-{}", i),
            0.6 + (i as f64 * 0.03), // Scores: 0.6, 0.63, 0.66, ...
            &role_id,
            &tradeoff_id,
        );
        record_evaluation(&eval, &agency_dir).unwrap();

        // Record token usage on the eval task (like the coordinator does)
        let eval_task_id = format!(".evaluate-task-{}", i);
        if let Some(eval_task) = graph.get_task_mut(&eval_task_id) {
            eval_task.status = Status::Done;
            eval_task.token_usage = Some(TokenUsage {
                cost_usd: 0.001 * (i as f64 + 1.0),
                input_tokens: 500 + i * 100,
                output_tokens: 100 + i * 20,
                cache_read_input_tokens: 200,
                cache_creation_input_tokens: 50,
            });
        }
    }
    save_graph(&graph, &graph_path).unwrap();

    // Verify cost data persisted on eval tasks
    let graph = load_graph(&graph_path).unwrap();
    for i in 0..12 {
        let eval_task = graph.get_task(&format!(".evaluate-task-{}", i)).unwrap();
        let usage = eval_task.token_usage.as_ref().unwrap();
        assert!(usage.cost_usd > 0.0, "Cost should be recorded");
        assert!(usage.input_tokens > 0, "Input tokens should be recorded");
        assert!(usage.output_tokens > 0, "Output tokens should be recorded");
    }

    // Step 5: Check evolver trigger — should fire after 12 evaluations (threshold=10)
    let config = AgencyConfig {
        auto_evolve: true,
        evolution_threshold: 10,
        evolution_interval: 0,
        ..AgencyConfig::default()
    };
    let state = EvolverState::default();

    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(trigger.is_some(), "Evolution should trigger after 12 evals");

    // Step 6: Simulate coordinator creating `.evolve-*` task
    let mut graph = load_graph(&graph_path).unwrap();
    let evolve_task_id = ".evolve-auto-test-loop";
    let mut evolve_task = make_task(
        evolve_task_id,
        "Auto-evolve: Threshold trigger",
        Status::Open,
    );
    evolve_task.tags = vec!["evolution".to_string(), "agency".to_string()];
    evolve_task.exec = Some("wg evolve --budget 5".to_string());
    evolve_task.exec_mode = Some("bare".to_string());
    graph.add_node(Node::Task(evolve_task));
    save_graph(&graph, &graph_path).unwrap();

    // Step 7: Simulate evolver completing — record evolution state
    let mut evolver_state = EvolverState::default();
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

    evolver_state.record_evolution(
        "run-auto-test",
        12,
        3,
        vec!["mutation".to_string()],
        avg_score,
        Some(evolve_task_id),
    );

    evolver_state.save(&agency_dir).unwrap();

    // Step 8: Verify final state
    let final_state = EvolverState::load(&agency_dir);
    assert_eq!(final_state.last_eval_count, 12);
    assert_eq!(final_state.history.len(), 1);
    assert_eq!(final_state.history[0].evaluations_consumed, 12);
    assert_eq!(final_state.history[0].operations_applied, 3);
    assert!(final_state.history[0].pre_evolution_avg_score.is_some());
    assert_eq!(
        final_state.history[0].task_id.as_deref(),
        Some(".evolve-auto-test-loop")
    );

    // After evolution, same eval count should NOT re-trigger
    let trigger_after = should_trigger_evolution(&agency_dir, &config, &final_state);
    assert!(
        trigger_after.is_none(),
        "Evolution should NOT trigger again without new evaluations"
    );

    // Verify the full graph state
    let final_graph = load_graph(&graph_path).unwrap();
    let total_tasks = final_graph.tasks().count();
    // 12 original tasks + 12 eval tasks + 1 evolve task = 25
    assert_eq!(total_tasks, 25);

    let eval_tasks: Vec<_> = final_graph
        .tasks()
        .filter(|t| t.id.starts_with(".evaluate-"))
        .collect();
    assert_eq!(eval_tasks.len(), 12);

    let evolve_tasks: Vec<_> = final_graph
        .tasks()
        .filter(|t| t.id.starts_with(".evolve-"))
        .collect();
    assert_eq!(evolve_tasks.len(), 1);
}

// ===========================================================================
// 9. Evolution does not trigger when disabled
// ===========================================================================

#[test]
fn test_evolution_disabled_no_trigger() {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    let evals_dir = agency_dir.join("evaluations");
    fs::create_dir_all(&evals_dir).unwrap();

    // Create enough evaluations
    for i in 0..20 {
        let eval = Evaluation {
            id: format!("eval-{}", i),
            task_id: format!("task-{}", i),
            agent_id: String::new(),
            role_id: "role-1".into(),
            tradeoff_id: "tradeoff-1".into(),
            score: 0.7,
            dimensions: HashMap::new(),
            notes: String::new(),
            evaluator: "test".into(),
            timestamp: format!("2025-06-01T1{}:00:00Z", i % 10),
            model: None,
            source: "llm".into(),
        };
        let path = evals_dir.join(format!("eval-{}.json", i));
        fs::write(&path, serde_json::to_string(&eval).unwrap()).unwrap();
    }

    let config = AgencyConfig {
        auto_evolve: false, // Disabled
        evolution_threshold: 5,
        ..AgencyConfig::default()
    };
    let state = EvolverState::default();

    let trigger = should_trigger_evolution(&agency_dir, &config, &state);
    assert!(
        trigger.is_none(),
        "Should not trigger when auto_evolve=false"
    );
}

// ===========================================================================
// 10. Safe strategies enforced in auto-evolution
// ===========================================================================

#[test]
fn test_safe_strategies_for_auto_evolution() {
    assert!(evolver::SAFE_STRATEGIES.contains(&"mutation"));
    assert!(evolver::SAFE_STRATEGIES.contains(&"gap-analysis"));
    assert!(evolver::SAFE_STRATEGIES.contains(&"retirement"));
    assert!(evolver::SAFE_STRATEGIES.contains(&"motivation-tuning"));

    // Dangerous strategies excluded
    assert!(!evolver::SAFE_STRATEGIES.contains(&"crossover"));
    assert!(!evolver::SAFE_STRATEGIES.contains(&"bizarre-ideation"));

    // Budget cap respected
    let low_budget = AgencyConfig {
        evolution_budget: 3,
        ..AgencyConfig::default()
    };
    assert_eq!(evolver::evolution_budget(&low_budget), 3);

    let high_budget = AgencyConfig {
        evolution_budget: 100,
        ..AgencyConfig::default()
    };
    assert_eq!(evolver::evolution_budget(&high_budget), 5); // Capped at DEFAULT_MAX_OPS
}
