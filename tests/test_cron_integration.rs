use chrono::{TimeZone, Timelike, Utc};
use serde_json;
use tempfile::TempDir;
use workgraph::cron::{calculate_next_fire, is_cron_due, parse_cron_expression};
use workgraph::graph::{Node, Priority, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

#[test]
fn test_cron_expression_parsing() {
    // Test 5-field format (gets converted to 6-field)
    let result = parse_cron_expression("0 2 * * *");
    assert!(
        result.is_ok(),
        "5-field cron expression should parse: {:?}",
        result
    );

    // Test 6-field format
    let result = parse_cron_expression("0 0 2 * * *");
    assert!(
        result.is_ok(),
        "6-field cron expression should parse: {:?}",
        result
    );

    // Test invalid format
    let result = parse_cron_expression("invalid cron");
    assert!(result.is_err(), "Invalid cron expression should fail");

    // Test edge cases
    let result = parse_cron_expression("0 */5 * * * *"); // Every 5 minutes
    assert!(result.is_ok(), "Every 5 minutes should parse: {:?}", result);

    let result = parse_cron_expression("0 0 12 * * 1-5"); // Weekdays at noon
    assert!(
        result.is_ok(),
        "Weekdays at noon should parse: {:?}",
        result
    );
}

#[test]
fn test_cron_due_checking() {
    // Create a task with daily cron at 2 AM
    let task = Task {
        id: "test-cron".to_string(),
        title: "Test Cron Task".to_string(),
        cron_enabled: true,
        cron_schedule: Some("0 0 2 * * *".to_string()), // Daily at 2 AM
        last_cron_fire: None,
        ..create_test_task()
    };

    // Test when schedule matches current time (should be due)
    let schedule_time = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();
    assert!(
        is_cron_due(&task, schedule_time),
        "Task should be due at scheduled time"
    );

    // Test when schedule doesn't match (should not be due)
    let non_schedule_time = Utc.with_ymd_and_hms(2024, 1, 1, 3, 0, 0).unwrap();
    assert!(
        !is_cron_due(&task, non_schedule_time),
        "Task should not be due at non-scheduled time"
    );

    // Test disabled cron
    let disabled_task = Task {
        cron_enabled: false,
        ..task.clone()
    };
    assert!(
        !is_cron_due(&disabled_task, schedule_time),
        "Disabled cron task should never be due"
    );

    // Test task with no schedule
    let no_schedule_task = Task {
        cron_schedule: None,
        ..task.clone()
    };
    assert!(
        !is_cron_due(&no_schedule_task, schedule_time),
        "Task with no schedule should never be due"
    );
}

#[test]
fn test_cron_due_with_last_fire() {
    let task = Task {
        id: "test-cron".to_string(),
        title: "Test Cron Task".to_string(),
        cron_enabled: true,
        cron_schedule: Some("0 0 2 * * *".to_string()), // Daily at 2 AM
        last_cron_fire: Some("2024-01-01T02:00:00Z".to_string()), // Fired at 2 AM on Jan 1
        ..create_test_task()
    };

    // Test at 1 AM next day - should not be due yet
    let early_time = Utc.with_ymd_and_hms(2024, 1, 2, 1, 0, 0).unwrap();
    assert!(
        !is_cron_due(&task, early_time),
        "Task should not be due before next scheduled time"
    );

    // Test at 2 AM next day - should be due
    let due_time = Utc.with_ymd_and_hms(2024, 1, 2, 2, 0, 0).unwrap();
    assert!(
        is_cron_due(&task, due_time),
        "Task should be due at next scheduled time"
    );

    // Test at 3 AM next day - should be due (missed window)
    let late_time = Utc.with_ymd_and_hms(2024, 1, 2, 3, 0, 0).unwrap();
    assert!(
        is_cron_due(&task, late_time),
        "Task should be due even after missed window"
    );
}

#[test]
fn test_next_fire_calculation() {
    let schedule = parse_cron_expression("0 0 2 * * *").unwrap(); // Daily at 2 AM

    // Test from 1 AM - next should be 2 AM same day
    let from_1am = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();
    let next = calculate_next_fire(&schedule, from_1am);
    assert!(next.is_some(), "Should calculate next fire time");
    let next_time = next.unwrap();
    assert_eq!(next_time.hour(), 2, "Next fire should be at 2 AM");
    assert_eq!(
        next_time.date_naive(),
        from_1am.date_naive(),
        "Next fire should be same day"
    );

    // Test from 3 AM - next should be 2 AM next day
    let from_3am = Utc.with_ymd_and_hms(2024, 1, 1, 3, 0, 0).unwrap();
    let next = calculate_next_fire(&schedule, from_3am);
    assert!(next.is_some(), "Should calculate next fire time");
    let next_time = next.unwrap();
    assert_eq!(next_time.hour(), 2, "Next fire should be at 2 AM");
    assert_eq!(
        next_time.date_naive(),
        from_3am.date_naive().succ_opt().unwrap(),
        "Next fire should be next day"
    );
}

#[test]
fn test_cron_task_serialization() {
    // Test serialization with cron fields
    let task = Task {
        id: "test-cron".to_string(),
        title: "Test Cron Task".to_string(),
        cron_schedule: Some("0 2 * * *".to_string()),
        cron_enabled: true,
        last_cron_fire: Some("2026-04-12T02:00:00Z".to_string()),
        next_cron_fire: Some("2026-04-13T02:00:00Z".to_string()),
        ..create_test_task()
    };

    // Test serialization
    let json = serde_json::to_string(&Node::Task(task.clone())).unwrap();
    assert!(
        json.contains("\"cron_schedule\":\"0 2 * * *\""),
        "JSON should contain cron_schedule"
    );
    assert!(
        json.contains("\"cron_enabled\":true"),
        "JSON should contain cron_enabled"
    );
    assert!(
        json.contains("\"last_cron_fire\":\"2026-04-12T02:00:00Z\""),
        "JSON should contain last_cron_fire"
    );
    assert!(
        json.contains("\"next_cron_fire\":\"2026-04-13T02:00:00Z\""),
        "JSON should contain next_cron_fire"
    );

    // Test deserialization
    let deserialized: Node = serde_json::from_str(&json).unwrap();
    if let Node::Task(deserialized_task) = deserialized {
        assert_eq!(
            deserialized_task.cron_schedule, task.cron_schedule,
            "cron_schedule should round-trip"
        );
        assert_eq!(
            deserialized_task.cron_enabled, task.cron_enabled,
            "cron_enabled should round-trip"
        );
        assert_eq!(
            deserialized_task.last_cron_fire, task.last_cron_fire,
            "last_cron_fire should round-trip"
        );
        assert_eq!(
            deserialized_task.next_cron_fire, task.next_cron_fire,
            "next_cron_fire should round-trip"
        );
    } else {
        panic!("Deserialized node should be a Task");
    }
}

#[test]
fn test_cron_workflow_end_to_end() {
    let temp_dir = TempDir::new().unwrap();
    let graph_path = temp_dir.path().join("graph.jsonl");

    // Create a test graph with a cron task
    let mut graph = WorkGraph::new();

    let cron_task = Task {
        id: "nightly-cleanup".to_string(),
        title: "Nightly Cleanup".to_string(),
        description: Some("Clean up old logs and temp files".to_string()),
        cron_enabled: true,
        cron_schedule: Some("0 0 2 * * *".to_string()), // Daily at 2 AM
        last_cron_fire: None,
        next_cron_fire: None,
        ..create_test_task()
    };

    graph.add_node(Node::Task(cron_task));

    // Save graph
    save_graph(&graph, &graph_path).unwrap();

    // Reload and verify cron task is present
    let loaded_graph = load_graph(&graph_path).unwrap();
    let task = loaded_graph.get_task("nightly-cleanup").unwrap();

    assert!(task.cron_enabled, "Cron should be enabled");
    assert_eq!(
        task.cron_schedule,
        Some("0 0 2 * * *".to_string()),
        "Cron schedule should be preserved"
    );
    assert!(
        task.last_cron_fire.is_none(),
        "Last fire should initially be None"
    );
}

/// Helper to create a default test task
fn create_test_task() -> Task {
    Task {
        id: "test".to_string(),
        title: "Test Task".to_string(),
        description: None,
        status: Status::Open,
        priority: Priority::Normal,
        assigned: None,
        estimate: None,
        before: vec![],
        after: vec![],
        requires: vec![],
        tags: vec![],
        skills: vec![],
        inputs: vec![],
        deliverables: vec![],
        artifacts: vec![],
        exec: None,
        timeout: None,
        not_before: None,
        created_at: None,
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries: None,
        failure_reason: None,
        model: None,
        provider: None,
        endpoint: None,
        verify: None,
        verify_timeout: None,
        agent: None,
        loop_iteration: 0,
        last_iteration_completed_at: None,
        cycle_failure_restarts: 0,
        cycle_config: None,
        ready_after: None,
        paused: false,
        visibility: "internal".to_string(),
        context_scope: None,
        exec_mode: None,
        token_usage: None,
        session_id: None,
        wait_condition: None,
        checkpoint: None,
        triage_count: 0,
        resurrection_count: 0,
        last_resurrected_at: None,
        validation: None,
        validation_commands: vec![],

        validator_agent: None,
        validator_model: None,
        gate_attempts: 0,
        test_required: false,
        rejection_count: 0,
        max_rejections: None,
        verify_failures: 0,
        spawn_failures: 0,
        tried_models: vec![],
        superseded_by: vec![],
        supersedes: None,
        unplaced: false,
        place_near: vec![],
        place_before: vec![],
        independent: false,
        iteration_round: 0,
        iteration_anchor: None,
        iteration_parent: None,
        iteration_config: None,
        cron_schedule: None,
        cron_enabled: false,
        last_cron_fire: None,
        next_cron_fire: None,
    }
}
