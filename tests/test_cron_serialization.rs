use serde_json;
use workgraph::graph::{Node, Priority, Status, Task};

#[test]
fn test_cron_task_serialization() {
    // Test serialization with cron fields
    let task = Task {
        id: "test-cron".to_string(),
        title: "Test Cron Task".to_string(),
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
        dispatch_count: 0,
        tier: None,
        no_tier_escalation: false,
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
        cron_schedule: Some("0 2 * * *".to_string()),
        cron_enabled: true,
        last_cron_fire: Some("2026-04-12T02:00:00Z".to_string()),
        next_cron_fire: Some("2026-04-13T02:00:00Z".to_string()),
    };

    // Test serialization
    let json = serde_json::to_string(&Node::Task(task)).unwrap();
    println!("Serialization successful");
    println!(
        "JSON contains cron_schedule: {}",
        json.contains("\"cron_schedule\":\"0 2 * * *\"")
    );
    println!(
        "JSON contains cron_enabled: {}",
        json.contains("\"cron_enabled\":true")
    );
    println!(
        "JSON contains last_cron_fire: {}",
        json.contains("\"last_cron_fire\":\"2026-04-12T02:00:00Z\"")
    );
    println!(
        "JSON contains next_cron_fire: {}",
        json.contains("\"next_cron_fire\":\"2026-04-13T02:00:00Z\"")
    );

    // Test deserialization
    let deserialized: Node = serde_json::from_str(&json).unwrap();
    println!("Deserialization successful");

    if let Node::Task(task) = deserialized {
        println!("Cron fields successfully round-tripped:");
        println!("  cron_schedule: {:?}", task.cron_schedule);
        println!("  cron_enabled: {}", task.cron_enabled);
        println!("  last_cron_fire: {:?}", task.last_cron_fire);
        println!("  next_cron_fire: {:?}", task.next_cron_fire);
    }
}
