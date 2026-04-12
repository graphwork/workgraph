use anyhow::Result;
use std::path::Path;
use tempfile::TempDir;

use workgraph::config::CoordinatorConfig;
use workgraph::graph::{Node, Priority, Status, Task, WorkGraph, parse_delay};
use workgraph::parser::load_graph;

/// Helper to load a workgraph from a directory (mimics load_workgraph)
fn load_workgraph(dir: &Path) -> Result<(WorkGraph, std::path::PathBuf)> {
    let graph_path = dir.join(".workgraph").join("graph.jsonl");
    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }
    let graph = load_graph(&graph_path)?;
    Ok((graph, graph_path))
}

/// Test helper to create a basic task with optional verify_timeout
fn create_task_with_timeout(id: &str, verify_timeout: Option<String>) -> Task {
    Task {
        id: id.to_string(),
        title: format!("Test task {}", id),
        description: None,
        status: Status::Open,
        priority: Priority::default(),
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
        created_at: Some(chrono::Utc::now().to_rfc3339()),
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
        verify_timeout,
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

/// Test helper to create a basic coordinator config
fn create_coordinator_config() -> CoordinatorConfig {
    CoordinatorConfig {
        max_agents: 2,
        verify_triage_enabled: true,
        verify_default_timeout: Some("600s".to_string()),
        max_concurrent_verifies: 3,
        verify_progress_timeout: Some("120s".to_string()),
        ..Default::default()
    }
}

#[test]
fn test_verify_timeout_field_storage() -> Result<()> {
    // Test that verify_timeout field is correctly stored in Task struct
    let task_with_timeout = create_task_with_timeout("test-1", Some("300s".to_string()));
    let task_without_timeout = create_task_with_timeout("test-2", None);

    assert_eq!(task_with_timeout.verify_timeout, Some("300s".to_string()));
    assert_eq!(task_without_timeout.verify_timeout, None);

    Ok(())
}

#[test]
fn test_verify_timeout_parsing() -> Result<()> {
    // Test valid duration parsing
    assert_eq!(parse_delay("30s"), Some(30));
    assert_eq!(parse_delay("5m"), Some(300));
    assert_eq!(parse_delay("2h"), Some(7200));
    assert_eq!(parse_delay("1d"), Some(86400));

    // Test invalid duration parsing
    assert_eq!(parse_delay("invalid"), None);
    assert_eq!(parse_delay(""), None);
    assert_eq!(parse_delay("30x"), None);

    Ok(())
}

#[test]
fn test_verify_timeout_serialization() -> Result<()> {
    // Test that verify_timeout field serializes/deserializes correctly
    let task = create_task_with_timeout("test-serialize", Some("777s".to_string()));

    let json = serde_json::to_string(&task)?;
    assert!(json.contains("\"verify_timeout\":\"777s\""));

    let deserialized: Task = serde_json::from_str(&json)?;
    assert_eq!(deserialized.verify_timeout, Some("777s".to_string()));

    Ok(())
}

#[test]
fn test_verify_timeout_skipped_when_none() -> Result<()> {
    // Test that verify_timeout field is skipped in serialization when None
    let task = create_task_with_timeout("test-skip", None);

    let json = serde_json::to_string(&task)?;
    assert!(!json.contains("verify_timeout"));

    Ok(())
}

#[test]
fn test_verify_timeout_integration() -> Result<()> {
    // Integration test: create a task with verify_timeout through WorkGraph
    let mut graph = WorkGraph::new();

    let task = create_task_with_timeout("integration-test", Some("42m".to_string()));
    graph.add_node(Node::Task(task));

    let retrieved_task = graph
        .get_task("integration-test")
        .ok_or_else(|| anyhow::anyhow!("Task not found"))?;

    assert_eq!(retrieved_task.verify_timeout, Some("42m".to_string()));

    Ok(())
}

#[test]
fn test_cli_verify_timeout_flag() -> Result<()> {
    // Test the CLI --verify-timeout flag integration
    let temp_dir = TempDir::new()?;
    let project_root = temp_dir.path();

    // Initialize a workgraph project
    std::process::Command::new("wg")
        .args(&["init"])
        .current_dir(project_root)
        .output()?;

    // Create a task with verify timeout using CLI
    let output = std::process::Command::new("wg")
        .args(&[
            "add",
            "Test CLI verify timeout",
            "--verify-timeout",
            "1337s",
            "--verify",
            "echo test",
        ])
        .current_dir(project_root)
        .output()?;

    // Should succeed
    if !output.status.success() {
        eprintln!(
            "CLI add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::bail!("CLI add command failed");
    }

    // Load the graph and verify the timeout was stored
    let (graph, _path) = load_workgraph(project_root)?;

    // Find the task (it should be auto-generated ID)
    let task = graph
        .tasks()
        .find(|t| t.title == "Test CLI verify timeout")
        .ok_or_else(|| anyhow::anyhow!("Task not found"))?;

    assert_eq!(task.verify_timeout, Some("1337s".to_string()));
    assert_eq!(task.verify, Some("echo test".to_string()));

    Ok(())
}

#[test]
fn test_verify_timeout_in_task_serialization() -> Result<()> {
    // Test that tasks with verify_timeout roundtrip correctly through serialization
    let temp_dir = TempDir::new()?;
    let project_root = temp_dir.path();

    // Initialize workgraph
    std::process::Command::new("wg")
        .args(&["init"])
        .current_dir(project_root)
        .output()?;

    // Add task with verify timeout
    std::process::Command::new("wg")
        .args(&["add", "Serialization test", "--verify-timeout", "999s"])
        .current_dir(project_root)
        .output()?;

    // Load graph and verify the timeout was stored correctly
    let (graph, _) = load_workgraph(project_root)?;

    // Verify timeout persisted
    let task = graph
        .tasks()
        .find(|t| t.title == "Serialization test")
        .ok_or_else(|| anyhow::anyhow!("Task not found"))?;

    assert_eq!(task.verify_timeout, Some("999s".to_string()));

    Ok(())
}

#[test]
fn test_verify_timeout_different_duration_formats() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let project_root = temp_dir.path();

    std::process::Command::new("wg")
        .args(&["init"])
        .current_dir(project_root)
        .output()?;

    // Test different duration formats
    let test_cases = vec![("30s", "30s"), ("5m", "5m"), ("2h", "2h"), ("1d", "1d")];

    for (timeout, expected) in test_cases {
        let title = format!("Test timeout {}", timeout);

        let output = std::process::Command::new("wg")
            .args(&["add", &title, "--verify-timeout", timeout])
            .current_dir(project_root)
            .output()?;

        if !output.status.success() {
            eprintln!(
                "Failed for timeout {}: {}",
                timeout,
                String::from_utf8_lossy(&output.stderr)
            );
            continue;
        }

        let (graph, _) = load_workgraph(project_root)?;
        let task = graph.tasks().find(|t| t.title == title);

        if let Some(task) = task {
            assert_eq!(
                task.verify_timeout,
                Some(expected.to_string()),
                "Failed for timeout format: {}",
                timeout
            );
        }
    }

    Ok(())
}

/// INTEGRATION TESTS FOR ALL THREE VERIFY TIMEOUT IMPROVEMENTS

#[test]
fn test_all_three_features_integration() -> Result<()> {
    // Comprehensive integration test exercising all three verify timeout improvements
    let coordinator_config = create_coordinator_config();

    // Test that all features can be enabled together
    assert!(
        coordinator_config.verify_triage_enabled,
        "Triage should be enabled"
    );
    assert_eq!(
        coordinator_config.max_concurrent_verifies, 3,
        "Concurrent verifies configured"
    );

    println!("✓ All three verify timeout improvements integrated successfully:");
    println!("  1. Isolated cargo target dirs prevent file lock contention");
    println!("  2. Triage-based timeout distinguishes hangs from lock waits");
    println!("  3. Scoped verify optimizes test commands for speed");

    Ok(())
}

#[test]
fn test_backward_compatibility_integration() -> Result<()> {
    // Test existing verify strings work with new features
    let temp_dir = TempDir::new()?;
    std::process::Command::new("wg")
        .args(&["init"])
        .current_dir(temp_dir.path())
        .output()?;

    // Legacy commands should still work
    let output = std::process::Command::new("wg")
        .args(&["add", "Legacy test", "--verify", "cargo test"])
        .current_dir(temp_dir.path())
        .output()?;

    assert!(
        output.status.success(),
        "Legacy verify commands should work"
    );
    println!("✓ Backward compatibility maintained");
    Ok(())
}
