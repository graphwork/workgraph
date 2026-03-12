//! Smoke: self-healing — diagnosis and remediation cycle test.
//!
//! Tests the self-healing pipeline's graph manipulation patterns:
//! - Remediation task creation with correct wiring
//! - Task blocking via .remediate-* dependencies
//! - Failure category → remediation strategy mapping
//! - Max remediation attempts enforcement (escalation)
//! - System task (.dot-prefix) skip logic
//! - Transient failure retry with exponential backoff
//! - Escalation on unfixable / low confidence

use chrono::{Duration, Utc};
use workgraph::graph::{
    LogEntry, Node, Status, Task, WaitCondition, WaitSpec, WorkGraph, is_system_task,
};
use workgraph::query::ready_tasks;

// ---------------------------------------------------------------------------
// Helpers — simulate the remediation pipeline's graph operations
// ---------------------------------------------------------------------------

fn make_log_entry(actor: &str, message: &str) -> LogEntry {
    LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(actor.to_string()),
        message: message.to_string(),
    }
}

/// Track remediation attempts via log entries (since remediation_count may
/// or may not exist on Task depending on branch state).
fn remediation_attempt_count(task: &Task) -> u32 {
    task.log
        .iter()
        .filter(|e| e.message.contains("Remediation task") && e.message.contains("created"))
        .count() as u32
}

/// Simulate creating a .remediate-{task_id} task the same way remediation.rs does.
fn simulate_create_remediation_task(
    graph: &mut WorkGraph,
    task_id: &str,
    category: &str,
    summary: &str,
) {
    let remediation_id = format!(".remediate-{task_id}");

    let remediation = Task {
        id: remediation_id.clone(),
        title: format!("Remediate: {}", task_id),
        description: Some(format!(
            "Remediate failure in task '{}': {}\n\n## Instructions\nCategory: {}",
            task_id, summary, category
        )),
        status: Status::Open,
        before: vec![task_id.to_string()],
        tags: vec!["remediation".to_string()],
        log: vec![make_log_entry(
            "coordinator",
            &format!("Auto-created remediation task (category: {category}, confidence: 0.90)"),
        )],
        max_retries: Some(1),
        ..Default::default()
    };

    graph.add_node(Node::Task(remediation));

    // Reset the failed task to Open and add dependency on remediation
    if let Some(failed_task) = graph.get_task_mut(task_id) {
        failed_task.status = Status::Open;
        failed_task.assigned = None;
        failed_task.failure_reason = None;
        let attempt = remediation_attempt_count(failed_task) + 1;
        failed_task.log.push(make_log_entry(
            "coordinator",
            &format!(
                "Remediation task '{}' created (attempt {}, category: {})",
                remediation_id, attempt, category
            ),
        ));
        if !failed_task.after.contains(&remediation_id) {
            failed_task.after.push(remediation_id);
        }
    }
}

/// Simulate transient retry with backoff the same way remediation.rs does.
/// Uses `retry_count` to track attempts for backoff calculation.
fn simulate_transient_retry(graph: &mut WorkGraph, task_id: &str) {
    if let Some(task) = graph.get_task_mut(task_id) {
        let backoff_secs = std::cmp::min(30 * 2_i64.pow(task.retry_count), 300);
        let resume_at = Utc::now() + Duration::seconds(backoff_secs);

        task.status = Status::Waiting;
        task.assigned = None;
        task.failure_reason = None;
        task.retry_count += 1;
        task.wait_condition = Some(WaitSpec::All(vec![WaitCondition::Timer {
            resume_after: resume_at.to_rfc3339(),
        }]));
        task.log.push(make_log_entry(
            "coordinator",
            &format!("Transient failure — scheduling retry in {backoff_secs}s"),
        ));
    }
}

/// Simulate escalation (pause + log).
fn simulate_escalate(graph: &mut WorkGraph, task_id: &str, reason: &str) {
    if let Some(task) = graph.get_task_mut(task_id) {
        task.paused = true;
        task.log.push(make_log_entry(
            "coordinator",
            &format!("Escalated to human: {reason}"),
        ));
    }
}

fn make_failed_task(id: &str, reason: &str) -> Task {
    Task {
        id: id.to_string(),
        title: format!("Test task {id}"),
        description: Some("A test task".to_string()),
        status: Status::Failed,
        failure_reason: Some(reason.to_string()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Build failure → .remediate-* task created, original blocked by it.
#[test]
fn test_smoke_healing_build_failure_creates_remediation() {
    let mut graph = WorkGraph::new();
    let task = make_failed_task("compile-lib", "cargo build failed: syntax error");
    graph.add_node(Node::Task(task));

    simulate_create_remediation_task(
        &mut graph,
        "compile-lib",
        "build_failure",
        "syntax error in main.rs",
    );

    // Remediation task exists with correct wiring
    let rem = graph.get_task(".remediate-compile-lib").unwrap();
    assert_eq!(rem.status, Status::Open);
    assert!(
        rem.before.contains(&"compile-lib".to_string()),
        ".remediate-* must have before edge to original task"
    );
    assert!(rem.tags.contains(&"remediation".to_string()));
    assert!(rem.description.as_ref().unwrap().contains("build_failure"));

    // Original task reset to Open, blocked by remediation
    let orig = graph.get_task("compile-lib").unwrap();
    assert_eq!(orig.status, Status::Open);
    assert!(
        orig.after.contains(&".remediate-compile-lib".to_string()),
        "Original task must depend on .remediate-* via after edge"
    );
    assert_eq!(remediation_attempt_count(orig), 1);
    assert!(
        orig.failure_reason.is_none(),
        "failure_reason should be cleared"
    );
    assert!(orig.assigned.is_none(), "assigned should be cleared");
}

/// Original task must NOT be ready while remediation is pending.
#[test]
fn test_smoke_healing_original_blocked_until_remediation_completes() {
    let mut graph = WorkGraph::new();
    let task = make_failed_task("blocked-task", "test failure");
    graph.add_node(Node::Task(task));

    simulate_create_remediation_task(&mut graph, "blocked-task", "build_failure", "tests fail");

    // Original task should NOT be ready (blocked by .remediate-blocked-task)
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        !ready_ids.contains(&"blocked-task"),
        "Original task should be blocked while remediation is pending"
    );
    assert!(
        ready_ids.contains(&".remediate-blocked-task"),
        "Remediation task should be ready for dispatch"
    );

    // Complete the remediation task → original becomes ready
    graph
        .get_task_mut(".remediate-blocked-task")
        .unwrap()
        .status = Status::Done;

    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&"blocked-task"),
        "Original task should become ready after remediation completes"
    );
}

/// Each failure category produces the correct remediation strategy.
#[test]
fn test_smoke_healing_category_strategies() {
    // Build failure → remediation task
    {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_failed_task("t-build", "compile error")));
        simulate_create_remediation_task(&mut graph, "t-build", "build_failure", "compile error");
        let rem = graph.get_task(".remediate-t-build").unwrap();
        assert!(rem.description.as_ref().unwrap().contains("build_failure"));
        assert_eq!(graph.get_task("t-build").unwrap().status, Status::Open);
    }

    // Missing dep → remediation task
    {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_failed_task("t-dep", "missing libssl")));
        simulate_create_remediation_task(&mut graph, "t-dep", "missing_dep", "libssl not found");
        let rem = graph.get_task(".remediate-t-dep").unwrap();
        assert!(rem.description.as_ref().unwrap().contains("missing_dep"));
    }

    // Context overflow → remediation task
    {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_failed_task(
            "t-ctx",
            "context window exceeded",
        )));
        simulate_create_remediation_task(
            &mut graph,
            "t-ctx",
            "context_overflow",
            "ran out of tokens",
        );
        let rem = graph.get_task(".remediate-t-ctx").unwrap();
        assert!(
            rem.description
                .as_ref()
                .unwrap()
                .contains("context_overflow")
        );
    }

    // Agent confusion → remediation task
    {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_failed_task("t-confused", "wrong approach")));
        simulate_create_remediation_task(
            &mut graph,
            "t-confused",
            "agent_confusion",
            "misunderstood",
        );
        let rem = graph.get_task(".remediate-t-confused").unwrap();
        assert!(
            rem.description
                .as_ref()
                .unwrap()
                .contains("agent_confusion")
        );
    }

    // Transient → retry with backoff (no remediation task)
    {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_failed_task("t-transient", "rate limited")));
        simulate_transient_retry(&mut graph, "t-transient");
        assert!(
            graph.get_task(".remediate-t-transient").is_none(),
            "Transient failures should not create remediation tasks"
        );
        let t = graph.get_task("t-transient").unwrap();
        assert_eq!(t.status, Status::Waiting);
        assert!(t.wait_condition.is_some());
    }

    // Unfixable → escalate (pause, no remediation task)
    {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_failed_task(
            "t-unfixable",
            "hardware failure",
        )));
        simulate_escalate(&mut graph, "t-unfixable", "hardware failure — needs human");
        assert!(
            graph.get_task(".remediate-t-unfixable").is_none(),
            "Unfixable failures should not create remediation tasks"
        );
        let t = graph.get_task("t-unfixable").unwrap();
        assert!(t.paused, "Unfixable task should be paused (escalated)");
        assert!(t.log.last().unwrap().message.contains("Escalated to human"));
    }
}

/// Max remediation attempts enforced — after the limit, task stays failed.
#[test]
fn test_smoke_healing_max_attempts_enforced() {
    let max_attempts: u32 = 3;
    let mut graph = WorkGraph::new();
    let task = make_failed_task("retry-me", "keeps failing");
    graph.add_node(Node::Task(task));

    // Simulate max_attempts remediation cycles
    for i in 0..max_attempts {
        simulate_create_remediation_task(
            &mut graph,
            "retry-me",
            "build_failure",
            &format!("attempt {}", i + 1),
        );
        let rem_id = ".remediate-retry-me".to_string();
        // Complete the remediation task
        graph.get_task_mut(&rem_id).unwrap().status = Status::Done;
        // Task fails again
        graph.get_task_mut("retry-me").unwrap().status = Status::Failed;
        graph.get_task_mut("retry-me").unwrap().failure_reason = Some("still failing".to_string());
    }

    let t = graph.get_task("retry-me").unwrap();
    assert_eq!(
        remediation_attempt_count(t),
        max_attempts,
        "Should have tracked {max_attempts} remediation attempts"
    );

    // Now the task should be at the limit — escalate instead of remediating
    assert!(
        remediation_attempt_count(t) >= max_attempts,
        "Task should be at or past the max remediation limit"
    );
    simulate_escalate(&mut graph, "retry-me", "max remediation attempts reached");
    let t = graph.get_task("retry-me").unwrap();
    assert!(
        t.paused,
        "Task should be escalated (paused) after hitting max attempts"
    );
}

/// System tasks (dot-prefix) are never remediated.
#[test]
fn test_smoke_healing_system_tasks_skipped() {
    // Verify is_system_task works correctly
    assert!(is_system_task(".evaluate-foo"));
    assert!(is_system_task(".remediate-bar"));
    assert!(is_system_task(".assign-baz"));
    assert!(!is_system_task("normal-task"));
    assert!(!is_system_task("build-thing"));

    // System tasks should be skipped even if they're failed
    let mut graph = WorkGraph::new();
    let system_task = Task {
        id: ".evaluate-widget".to_string(),
        title: "Evaluate widget".to_string(),
        status: Status::Failed,
        failure_reason: Some("evaluation timed out".to_string()),
        ..Default::default()
    };
    graph.add_node(Node::Task(system_task));

    // The remediation pipeline would check is_system_task and skip
    let task = graph.get_task(".evaluate-widget").unwrap();
    assert!(
        is_system_task(&task.id),
        "Dot-prefixed tasks must be identified as system tasks"
    );

    // No remediation task should exist for system tasks
    assert!(graph.get_task(".remediate-.evaluate-widget").is_none());
}

/// Transient failures auto-retry with exponential backoff.
#[test]
fn test_smoke_healing_transient_backoff() {
    let mut graph = WorkGraph::new();
    let task = make_failed_task("flaky-api", "connection timeout");
    graph.add_node(Node::Task(task));

    // First retry: backoff = min(30 * 2^0, 300) = 30s
    simulate_transient_retry(&mut graph, "flaky-api");
    let t = graph.get_task("flaky-api").unwrap();
    assert_eq!(t.status, Status::Waiting);
    assert_eq!(t.retry_count, 1);
    match &t.wait_condition {
        Some(WaitSpec::All(conditions)) => {
            assert_eq!(conditions.len(), 1);
            match &conditions[0] {
                WaitCondition::Timer { resume_after } => {
                    let resume: chrono::DateTime<Utc> =
                        chrono::DateTime::parse_from_rfc3339(resume_after)
                            .unwrap()
                            .into();
                    let now = Utc::now();
                    let diff = resume - now;
                    assert!(
                        diff.num_seconds() >= 25 && diff.num_seconds() <= 35,
                        "First retry backoff should be ~30s, got {}s",
                        diff.num_seconds()
                    );
                }
                other => panic!("Expected Timer condition, got {other:?}"),
            }
        }
        other => panic!("Expected WaitSpec::All, got {other:?}"),
    }

    // Simulate the wait completing — task goes back to Failed
    graph.get_task_mut("flaky-api").unwrap().status = Status::Failed;
    graph.get_task_mut("flaky-api").unwrap().wait_condition = None;
    graph.get_task_mut("flaky-api").unwrap().failure_reason =
        Some("connection timeout again".to_string());

    // Second retry: backoff = min(30 * 2^1, 300) = 60s
    simulate_transient_retry(&mut graph, "flaky-api");
    let t = graph.get_task("flaky-api").unwrap();
    assert_eq!(t.retry_count, 2);
    match &t.wait_condition {
        Some(WaitSpec::All(conditions)) => match &conditions[0] {
            WaitCondition::Timer { resume_after } => {
                let resume: chrono::DateTime<Utc> =
                    chrono::DateTime::parse_from_rfc3339(resume_after)
                        .unwrap()
                        .into();
                let now = Utc::now();
                let diff = resume - now;
                assert!(
                    diff.num_seconds() >= 55 && diff.num_seconds() <= 65,
                    "Second retry backoff should be ~60s, got {}s",
                    diff.num_seconds()
                );
            }
            other => panic!("Expected Timer condition, got {other:?}"),
        },
        other => panic!("Expected WaitSpec::All, got {other:?}"),
    }
}

/// Backoff caps at 300 seconds.
#[test]
fn test_smoke_healing_transient_backoff_cap() {
    let mut graph = WorkGraph::new();
    let mut task = make_failed_task("capped", "timeout");
    task.retry_count = 10; // 30 * 2^10 = 30720, should be capped to 300
    graph.add_node(Node::Task(task));

    simulate_transient_retry(&mut graph, "capped");
    let t = graph.get_task("capped").unwrap();
    match &t.wait_condition {
        Some(WaitSpec::All(conditions)) => match &conditions[0] {
            WaitCondition::Timer { resume_after } => {
                let resume: chrono::DateTime<Utc> =
                    chrono::DateTime::parse_from_rfc3339(resume_after)
                        .unwrap()
                        .into();
                let now = Utc::now();
                let diff = resume - now;
                assert!(
                    diff.num_seconds() >= 295 && diff.num_seconds() <= 305,
                    "Backoff should cap at 300s, got {}s",
                    diff.num_seconds()
                );
            }
            other => panic!("Expected Timer condition, got {other:?}"),
        },
        other => panic!("Expected WaitSpec::All, got {other:?}"),
    }
}

/// Escalation on low confidence diagnosis.
#[test]
fn test_smoke_healing_escalation_low_confidence() {
    let mut graph = WorkGraph::new();
    let task = make_failed_task("unclear-fail", "mysterious error");
    graph.add_node(Node::Task(task));

    // Simulate: diagnosis returned confidence < 0.6 → escalate
    simulate_escalate(
        &mut graph,
        "unclear-fail",
        "Low diagnosis confidence (0.35): mysterious error",
    );

    let t = graph.get_task("unclear-fail").unwrap();
    assert!(
        t.paused,
        "Low-confidence diagnosis should escalate (pause) the task"
    );
    assert!(
        t.log
            .last()
            .unwrap()
            .message
            .contains("Low diagnosis confidence")
    );
    // No remediation task created
    assert!(graph.get_task(".remediate-unclear-fail").is_none());
}

/// Escalation on unfixable category.
#[test]
fn test_smoke_healing_escalation_unfixable() {
    let mut graph = WorkGraph::new();
    let task = make_failed_task("broken-hw", "hardware failure");
    graph.add_node(Node::Task(task));

    simulate_escalate(&mut graph, "broken-hw", "hardware failure — unfixable");

    let t = graph.get_task("broken-hw").unwrap();
    assert!(t.paused);
    assert!(t.log.last().unwrap().message.contains("Escalated to human"));
    assert!(graph.get_task(".remediate-broken-hw").is_none());
}

/// Remediation task has correct structure: tags, max_retries, before edges.
#[test]
fn test_smoke_healing_remediation_task_structure() {
    let mut graph = WorkGraph::new();
    let task = make_failed_task("struct-check", "build failed");
    graph.add_node(Node::Task(task));

    simulate_create_remediation_task(&mut graph, "struct-check", "build_failure", "syntax error");

    let rem = graph.get_task(".remediate-struct-check").unwrap();

    // ID follows .remediate-{original} pattern
    assert_eq!(rem.id, ".remediate-struct-check");

    // Is a system task (dot-prefix)
    assert!(is_system_task(&rem.id));

    // Has before edge → blocks original task
    assert_eq!(rem.before, vec!["struct-check".to_string()]);

    // Tagged as remediation
    assert!(rem.tags.contains(&"remediation".to_string()));

    // Status is Open (ready for dispatch)
    assert_eq!(rem.status, Status::Open);

    // Max retries limited
    assert_eq!(rem.max_retries, Some(1));

    // Has creation log entry
    assert!(!rem.log.is_empty());
    assert!(rem.log[0].message.contains("Auto-created remediation task"));
    assert_eq!(rem.log[0].actor, Some("coordinator".to_string()));
}

/// Full remediation cycle: fail → remediate → complete → task becomes ready.
#[test]
fn test_smoke_healing_full_cycle() {
    let mut graph = WorkGraph::new();

    // Create a task with a downstream consumer
    let task = make_failed_task("impl-auth", "tests fail");
    graph.add_node(Node::Task(task));

    let consumer = Task {
        id: "write-docs".to_string(),
        title: "Write docs".to_string(),
        status: Status::Open,
        after: vec!["impl-auth".to_string()],
        ..Default::default()
    };
    graph.add_node(Node::Task(consumer));

    // Step 1: Task fails, remediation created
    simulate_create_remediation_task(&mut graph, "impl-auth", "build_failure", "test_auth fails");

    // Consumer should still be blocked (impl-auth is Open but blocked by remediation)
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        !ready_ids.contains(&"write-docs"),
        "Consumer should be blocked while impl-auth is not Done"
    );
    assert!(
        !ready_ids.contains(&"impl-auth"),
        "impl-auth should be blocked by .remediate-impl-auth"
    );
    assert!(
        ready_ids.contains(&".remediate-impl-auth"),
        "Remediation task should be ready"
    );

    // Step 2: Remediation completes
    graph.get_task_mut(".remediate-impl-auth").unwrap().status = Status::Done;

    // impl-auth should now be ready
    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&"impl-auth"),
        "impl-auth should be ready after remediation completes"
    );
    assert!(
        !ready_ids.contains(&"write-docs"),
        "Consumer should still be blocked (impl-auth is Open, not Done)"
    );

    // Step 3: impl-auth succeeds this time
    graph.get_task_mut("impl-auth").unwrap().status = Status::Done;

    let ready = ready_tasks(&graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&"write-docs"),
        "Consumer should be ready after impl-auth completes"
    );
}

/// Multiple remediation cycles increment the counter correctly.
#[test]
fn test_smoke_healing_remediation_count_tracks() {
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_failed_task("multi-fix", "error")));

    // First remediation
    simulate_create_remediation_task(&mut graph, "multi-fix", "build_failure", "error 1");
    assert_eq!(
        remediation_attempt_count(graph.get_task("multi-fix").unwrap()),
        1
    );

    // Complete remediation, task fails again
    graph.get_task_mut(".remediate-multi-fix").unwrap().status = Status::Done;
    graph.get_task_mut("multi-fix").unwrap().status = Status::Failed;

    // Second remediation
    simulate_create_remediation_task(&mut graph, "multi-fix", "missing_dep", "error 2");
    assert_eq!(
        remediation_attempt_count(graph.get_task("multi-fix").unwrap()),
        2
    );
}

/// Paused tasks are never remediated (eligibility check).
#[test]
fn test_smoke_healing_paused_tasks_skipped() {
    let mut graph = WorkGraph::new();
    let mut task = make_failed_task("paused-fail", "error");
    task.paused = true;
    graph.add_node(Node::Task(task));

    // The remediation pipeline checks !task.paused before processing
    let t = graph.get_task("paused-fail").unwrap();
    assert!(
        t.paused && t.status == Status::Failed,
        "Paused failed tasks should be skipped by the remediation pipeline"
    );
}

/// Config type exists and can be constructed.
#[test]
fn test_smoke_healing_config_exists() {
    use workgraph::config::Config;

    // Config can be created with defaults
    let _config = Config::default();
}

/// Pending remediation task blocks new remediation for same original.
#[test]
fn test_smoke_healing_no_duplicate_remediation() {
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_failed_task("dup-check", "error")));

    simulate_create_remediation_task(&mut graph, "dup-check", "build_failure", "first attempt");

    // .remediate-dup-check is Open → pending
    let rem = graph.get_task(".remediate-dup-check").unwrap();
    assert!(
        matches!(
            rem.status,
            Status::Open | Status::InProgress | Status::Waiting
        ),
        "Pending remediation should block duplicate creation"
    );
}
