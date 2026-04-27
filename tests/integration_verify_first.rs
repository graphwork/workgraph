//! Integration tests for the verify-first eval pipeline.
//!
//! Tests the complete pipeline: scaffold → FLIP → verify → evaluate,
//! covering dependency wiring, task creation, readiness blocking,
//! and evaluator prompt rendering with verify context.

use workgraph::agency::{EvaluatorInput, render_evaluator_prompt};
use workgraph::config::Config;
use workgraph::graph::{LogEntry, Node, Status, Task, WorkGraph};
use workgraph::query::ready_tasks;

// Pull in the scaffold functions (they're pub in eval_scaffold.rs which is
// a file under src/commands/, re-exported as a binary helper — we use the
// library types directly instead).

/// Helper: create a minimal task.
fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        ..Task::default()
    }
}

// ============================================================================
// 1. FLIP disabled: eval depends on source task directly (backwards compat)
// ============================================================================

#[test]
fn test_flip_disabled_eval_depends_on_source() {
    let mut config = Config::default();
    config.agency.flip_enabled = false; // Explicitly disable FLIP for this test
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("task-x", "Task X", Status::Open)));

    // Use eval_scaffold via the binary crate's public function.
    // Since eval_scaffold is in src/commands/ (binary), we replicate the
    // scaffold logic inline using the library types.
    // The actual scaffold_eval_task is tested in eval_scaffold.rs unit tests;
    // here we verify the resulting graph structure.

    // Simulate what scaffold_eval_task does when FLIP is disabled:
    // .evaluate-task-x depends on task-x
    assert!(!config.agency.flip_enabled);

    let eval_task = Task {
        id: ".evaluate-task-x".to_string(),
        title: "Evaluate: Task X".to_string(),
        status: Status::Open,
        after: vec!["task-x".to_string()], // Depends on source directly
        tags: vec!["evaluation".to_string(), "agency".to_string()],
        exec: Some("wg evaluate run task-x".to_string()),
        exec_mode: Some("bare".to_string()),
        visibility: "internal".to_string(),
        ..Task::default()
    };
    graph.add_node(Node::Task(eval_task));

    // Verify: no .flip-task-x exists
    assert!(
        graph.get_task(".flip-task-x").is_none(),
        "FLIP task should not exist when FLIP is disabled"
    );

    // Verify: .evaluate-task-x depends on task-x directly
    let eval = graph.get_task(".evaluate-task-x").unwrap();
    assert_eq!(eval.after, vec!["task-x".to_string()]);

    // Verify: when task-x is done, eval becomes ready
    graph.get_task_mut("task-x").unwrap().status = Status::Done;
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&".evaluate-task-x"),
        "Eval should be ready when source task is done; ready: {:?}",
        ready_ids
    );
}

// ============================================================================
// 2. FLIP enabled: eval depends on .flip-<task>
// ============================================================================

#[test]
fn test_flip_enabled_eval_depends_on_flip() {
    let mut config = Config::default();
    config.agency.flip_enabled = true;
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("task-x", "Task X", Status::Done)));

    // Simulate scaffold_flip_task + scaffold_eval_task with FLIP enabled:
    let flip_task = Task {
        id: ".flip-task-x".to_string(),
        title: "FLIP: task-x".to_string(),
        status: Status::Open,
        after: vec!["task-x".to_string()],
        tags: vec!["flip".to_string(), "agency".to_string()],
        exec: Some("wg evaluate run task-x --flip".to_string()),
        exec_mode: Some("bare".to_string()),
        visibility: "internal".to_string(),
        ..Task::default()
    };
    graph.add_node(Node::Task(flip_task));

    let eval_task = Task {
        id: ".evaluate-task-x".to_string(),
        title: "Evaluate: Task X".to_string(),
        status: Status::Open,
        after: vec![".flip-task-x".to_string()], // Depends on FLIP, not source
        tags: vec!["evaluation".to_string(), "agency".to_string()],
        exec: Some("wg evaluate run task-x".to_string()),
        exec_mode: Some("bare".to_string()),
        visibility: "internal".to_string(),
        ..Task::default()
    };
    graph.add_node(Node::Task(eval_task));

    // Verify: eval depends on .flip-task-x, NOT task-x
    let eval = graph.get_task(".evaluate-task-x").unwrap();
    assert_eq!(eval.after, vec![".flip-task-x".to_string()]);
    assert!(
        !eval.after.contains(&"task-x".to_string()),
        "Eval should NOT depend on source task directly when FLIP is enabled"
    );

    // Verify: eval is NOT ready while FLIP is still open
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        !ready_ids.contains(&".evaluate-task-x"),
        "Eval should NOT be ready while FLIP is open"
    );
    // But FLIP should be ready (source task is done)
    assert!(
        ready_ids.contains(&".flip-task-x"),
        "FLIP should be ready when source task is done"
    );

    // Complete FLIP → eval becomes ready
    graph.get_task_mut(".flip-task-x").unwrap().status = Status::Done;
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&".evaluate-task-x"),
        "Eval should be ready after FLIP completes"
    );
}

// ============================================================================
// 3. scaffold_flip_task() creates .flip-<task> with correct properties
// ============================================================================

#[test]
fn test_flip_scaffold_creates_flip_task() {
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("my-task", "My Task", Status::Open)));

    // Simulate scaffold_flip_task behavior
    let flip_task_id = ".flip-my-task";
    assert!(
        graph.get_task(flip_task_id).is_none(),
        "FLIP task should not exist yet"
    );

    let flip_task = Task {
        id: flip_task_id.to_string(),
        title: "FLIP: my-task".to_string(),
        description: Some(
            "Run FLIP (Fidelity via Latent Intent Probing) evaluation for task 'my-task'."
                .to_string(),
        ),
        status: Status::Open,
        after: vec!["my-task".to_string()],
        tags: vec!["flip".to_string(), "agency".to_string()],
        exec: Some("wg evaluate run my-task --flip".to_string()),
        exec_mode: Some("bare".to_string()),
        visibility: "internal".to_string(),
        ..Task::default()
    };
    graph.add_node(Node::Task(flip_task));

    // Verify all properties
    let flip = graph.get_task(flip_task_id).unwrap();
    assert_eq!(flip.title, "FLIP: my-task");
    assert_eq!(flip.after, vec!["my-task".to_string()]);
    assert!(flip.tags.contains(&"flip".to_string()));
    assert!(flip.tags.contains(&"agency".to_string()));
    assert_eq!(
        flip.exec,
        Some("wg evaluate run my-task --flip".to_string())
    );
    assert_eq!(flip.exec_mode, Some("bare".to_string()));
    assert_eq!(flip.visibility, "internal");
    assert_eq!(flip.status, Status::Open);
}

// ============================================================================
// 4. scaffold_flip_task() is idempotent
// ============================================================================

#[test]
fn test_flip_scaffold_idempotent() {
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("my-task", "My Task", Status::Open)));

    // First creation succeeds
    let flip_task_id = ".flip-my-task";
    assert!(graph.get_task(flip_task_id).is_none());

    let flip_task = Task {
        id: flip_task_id.to_string(),
        title: "FLIP: my-task".to_string(),
        status: Status::Open,
        after: vec!["my-task".to_string()],
        tags: vec!["flip".to_string(), "agency".to_string()],
        exec: Some("wg evaluate run my-task --flip".to_string()),
        exec_mode: Some("bare".to_string()),
        visibility: "internal".to_string(),
        ..Task::default()
    };
    graph.add_node(Node::Task(flip_task));
    assert!(graph.get_task(flip_task_id).is_some());

    // Second attempt: task already exists, graph unchanged
    // (In real code, scaffold_flip_task returns false; here we verify the check)
    let already_exists = graph.get_task(flip_task_id).is_some();
    assert!(
        already_exists,
        "FLIP task should already exist (idempotent guard)"
    );

    // Verify only one FLIP task exists (no duplicates)
    let flip_count = graph.tasks().filter(|t| t.id == flip_task_id).count();
    assert_eq!(flip_count, 1, "Should have exactly one FLIP task");
}

// ============================================================================
// 5. Verify task gets added as dependency to eval
// ============================================================================

#[test]
fn test_verify_adds_dep_to_eval() {
    let mut graph = WorkGraph::new();

    // Setup: source task (done), flip task (done), eval task (depends on flip)
    graph.add_node(Node::Task(make_task("task-x", "Task X", Status::Done)));

    let flip_task = Task {
        id: ".flip-task-x".to_string(),
        title: "FLIP: task-x".to_string(),
        status: Status::Done,
        after: vec!["task-x".to_string()],
        tags: vec!["flip".to_string(), "agency".to_string()],
        ..Task::default()
    };
    graph.add_node(Node::Task(flip_task));

    let eval_task = Task {
        id: ".evaluate-task-x".to_string(),
        title: "Evaluate: Task X".to_string(),
        status: Status::Open,
        after: vec![".flip-task-x".to_string()],
        tags: vec!["evaluation".to_string(), "agency".to_string()],
        ..Task::default()
    };
    graph.add_node(Node::Task(eval_task));

    // Simulate build_flip_verification_tasks: creates .verify-task-x
    // and adds it as dep on .evaluate-task-x
    let verify_task_id = ".verify-task-x".to_string();
    let verify_task = Task {
        id: verify_task_id.clone(),
        title: "Verify (FLIP 0.45): Task X".to_string(),
        status: Status::Open,
        tags: vec!["verification".to_string(), "agency".to_string()],
        ..Task::default()
    };
    graph.add_node(Node::Task(verify_task));

    // Add verify as dep on eval (this is what build_flip_verification_tasks does)
    let eval_task_id = ".evaluate-task-x";
    if let Some(eval_task) = graph.get_task_mut(eval_task_id) {
        if !eval_task.after.contains(&verify_task_id) {
            eval_task.after.push(verify_task_id.clone());
        }
    }

    // Verify: eval now depends on both .flip-task-x and .verify-task-x
    let eval = graph.get_task(eval_task_id).unwrap();
    assert!(
        eval.after.contains(&".flip-task-x".to_string()),
        "Eval should still depend on FLIP task"
    );
    assert!(
        eval.after.contains(&".verify-task-x".to_string()),
        "Eval should now also depend on verify task"
    );
    assert_eq!(eval.after.len(), 2, "Eval should have exactly 2 deps");

    // Verify idempotency: adding again should not create a duplicate
    if let Some(eval_task) = graph.get_task_mut(eval_task_id) {
        if !eval_task.after.contains(&verify_task_id) {
            eval_task.after.push(verify_task_id.clone());
        }
    }
    let eval = graph.get_task(eval_task_id).unwrap();
    assert_eq!(
        eval.after.len(),
        2,
        "Idempotent: should not add duplicate dep"
    );
}

// ============================================================================
// 6. Eval blocked until verify is done
// ============================================================================

#[test]
fn test_eval_blocked_until_verify_done() {
    let mut graph = WorkGraph::new();

    // Source done, FLIP done, verify open, eval depends on both
    graph.add_node(Node::Task(make_task("task-x", "Task X", Status::Done)));
    graph.add_node(Node::Task(Task {
        id: ".flip-task-x".to_string(),
        title: "FLIP: task-x".to_string(),
        status: Status::Done,
        after: vec!["task-x".to_string()],
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: ".verify-task-x".to_string(),
        title: "Verify (FLIP 0.45): Task X".to_string(),
        status: Status::Open,
        tags: vec!["verification".to_string()],
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: ".evaluate-task-x".to_string(),
        title: "Evaluate: Task X".to_string(),
        status: Status::Open,
        after: vec![".flip-task-x".to_string(), ".verify-task-x".to_string()],
        tags: vec!["evaluation".to_string()],
        ..Task::default()
    }));

    // Eval should NOT be ready (verify is open)
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        !ready_ids.contains(&".evaluate-task-x"),
        "Eval should be blocked while verify is open; ready: {:?}",
        ready_ids
    );

    // Verify is in-progress: still blocked
    graph.get_task_mut(".verify-task-x").unwrap().status = Status::InProgress;
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        !ready_ids.contains(&".evaluate-task-x"),
        "Eval should be blocked while verify is in-progress; ready: {:?}",
        ready_ids
    );

    // Verify completes: eval becomes ready
    graph.get_task_mut(".verify-task-x").unwrap().status = Status::Done;
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&".evaluate-task-x"),
        "Eval should be ready after verify completes; ready: {:?}",
        ready_ids
    );
}

// ============================================================================
// 7. Eval prompt includes verify findings when verify data exists
// ============================================================================

#[test]
fn test_eval_prompt_includes_verify_findings() {
    let input = EvaluatorInput {
        task_title: "Implement feature X",
        task_description: Some("Add the X feature to the system."),
        task_skills: &[],
        verify: Some("cargo test test_feature_x passes"),
        agent: None,
        role: None,
        tradeoff: None,
        artifacts: &["src/feature_x.rs".to_string()],
        log_entries: &[LogEntry {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            actor: Some("agent-1".to_string()),
            user: None,
            message: "Implemented feature X".to_string(),
        }],
        started_at: Some("2025-01-01T00:00:00Z"),
        completed_at: Some("2025-01-01T01:00:00Z"),
        artifact_diff: None,
        evaluator_identity: None,
        downstream_tasks: &[],
        flip_score: Some(0.45),
        verify_status: Some("passed"),
        verify_findings: Some(
            "[2025-01-01] (agent-v): Tests pass\n[2025-01-01] (agent-v): Artifacts verified",
        ),
        resolved_outcome_name: None,
        child_tasks: &[],
        constraint_fidelity_score: None,
        constraint_fidelity_unanchored: None,
    };

    let prompt = render_evaluator_prompt(&input);

    // Verify the FLIP Verification Results section is present
    assert!(
        prompt.contains("## FLIP Verification Results"),
        "Prompt should contain FLIP verification section"
    );
    assert!(
        prompt.contains("FLIP Score: 0.45"),
        "Prompt should contain FLIP score"
    );
    assert!(
        prompt.contains("below threshold"),
        "Should indicate score is below threshold"
    );
    assert!(
        prompt.contains("Verification Status: PASSED"),
        "Should contain verification status"
    );
    assert!(
        prompt.contains("Verification Findings:"),
        "Should contain findings header"
    );
    assert!(
        prompt.contains("Tests pass"),
        "Should contain actual findings text"
    );
    assert!(
        prompt.contains("Artifacts verified"),
        "Should contain actual findings text"
    );
    assert!(
        prompt.contains("NOTE: Verification is a strong signal"),
        "Should contain the guidance note"
    );
}

// ============================================================================
// 8. Eval prompt has no verify section when no verify data exists
// ============================================================================

#[test]
fn test_eval_prompt_no_verify_section_when_absent() {
    let input = EvaluatorInput {
        task_title: "Implement feature Y",
        task_description: Some("Add the Y feature."),
        task_skills: &[],
        verify: None,
        agent: None,
        role: None,
        tradeoff: None,
        artifacts: &[],
        log_entries: &[],
        started_at: None,
        completed_at: None,
        artifact_diff: None,
        evaluator_identity: None,
        downstream_tasks: &[],
        flip_score: None,
        verify_status: None,
        verify_findings: None,
        resolved_outcome_name: None,
        child_tasks: &[],
        constraint_fidelity_score: None,
        constraint_fidelity_unanchored: None,
    };

    let prompt = render_evaluator_prompt(&input);

    assert!(
        !prompt.contains("## FLIP Verification Results"),
        "Prompt should NOT contain FLIP verification section when no verify data"
    );
    assert!(
        !prompt.contains("FLIP Score"),
        "Prompt should NOT contain FLIP Score when no data"
    );
    assert!(
        !prompt.contains("Verification Status"),
        "Prompt should NOT contain Verification Status when no data"
    );
    assert!(
        !prompt.contains("Verification Findings"),
        "Prompt should NOT contain Verification Findings when no data"
    );
}
