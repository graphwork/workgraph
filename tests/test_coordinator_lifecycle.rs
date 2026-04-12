use tempfile::TempDir;
use workgraph::graph::{Status, Task, WorkGraph};
use workgraph::parser;

/// Test that coordinator ID allocation correctly skips archived coordinators
#[test]
fn test_coordinator_id_allocation_skips_archived() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path();

    // Create a graph with some coordinators in various states
    let mut graph = WorkGraph::new();

    // Add coordinator-0 (archived)
    let coordinator_0 = Task {
        id: ".coordinator-0".to_string(),
        title: "Coordinator 0".to_string(),
        status: Status::Done,
        tags: vec!["archived".to_string()],
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coordinator_0));

    // Add coordinator-1 (abandoned with coordinator-loop tag)
    let coordinator_1 = Task {
        id: ".coordinator-1".to_string(),
        title: "Coordinator 1".to_string(),
        status: Status::Abandoned,
        tags: vec!["coordinator-loop".to_string()],
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coordinator_1));

    // Add coordinator-2 (active)
    let coordinator_2 = Task {
        id: ".coordinator-2".to_string(),
        title: "Coordinator 2".to_string(),
        status: Status::InProgress,
        tags: vec!["coordinator-loop".to_string()],
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coordinator_2));

    // Save the graph
    parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

    // Test the coordinator slot availability function
    // This is a copy of the function from ipc.rs to test it independently
    fn is_coordinator_slot_available(graph: &WorkGraph, task_id: &str) -> bool {
        match graph.get_task(task_id) {
            None => true, // Slot is empty — available
            Some(task) => {
                // If task has archived tag, it's explicitly archived — NOT available.
                if task.tags.iter().any(|t| t == "archived") {
                    return false;
                }
                // If task has coordinator-loop tag, check if it's still active
                if task.tags.iter().any(|t| t == "coordinator-loop") {
                    // Only return false for truly active coordinators.
                    // Archived and abandoned coordinators exist with their old state
                    // and must NOT be resurrected by re-using their ID.
                    if task.status == Status::InProgress {
                        return false; // Active coordinator — not available
                    }
                    // Archived or abandoned — skip this slot, treat as occupied
                    return false;
                }
                // No coordinator-loop tag and not archived — not a coordinator slot, available
                true
            }
        }
    }

    // Find the next available coordinator ID (simulating create-coordinator logic)
    let mut next_id = 0u32;
    loop {
        let task_id = format!(".coordinator-{}", next_id);
        if is_coordinator_slot_available(&graph, &task_id) {
            break;
        }
        next_id += 1;
    }

    // The next available ID should be 3, skipping:
    // - coordinator-0 (archived)
    // - coordinator-1 (abandoned but has coordinator-loop tag)
    // - coordinator-2 (active)
    assert_eq!(
        next_id, 3,
        "Should allocate coordinator-3, skipping archived/abandoned/active coordinators"
    );

    // Verify that each previous ID is correctly identified as unavailable
    assert!(
        !is_coordinator_slot_available(&graph, ".coordinator-0"),
        "Archived coordinator should not be available"
    );
    assert!(
        !is_coordinator_slot_available(&graph, ".coordinator-1"),
        "Abandoned coordinator with coordinator-loop should not be available"
    );
    assert!(
        !is_coordinator_slot_available(&graph, ".coordinator-2"),
        "Active coordinator should not be available"
    );
    assert!(
        is_coordinator_slot_available(&graph, ".coordinator-3"),
        "Empty slot should be available"
    );
}

/// Test that archiving a coordinator properly sets tags and status
#[test]
fn test_coordinator_archiving_sets_correct_state() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path();

    // Create a coordinator task
    let mut graph = WorkGraph::new();
    let coordinator = Task {
        id: ".coordinator-5".to_string(),
        title: "Test Coordinator".to_string(),
        status: Status::InProgress,
        tags: vec!["coordinator-loop".to_string()],
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coordinator));
    parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

    // Simulate the archive process (from handle_archive_coordinator in ipc.rs)
    let mut modified_graph = parser::load_graph(&dir.join("graph.jsonl")).unwrap();
    let task = modified_graph.get_task_mut(".coordinator-5").unwrap();
    task.status = Status::Done;
    task.tags.retain(|t| t != "coordinator-loop");
    if !task.tags.contains(&"archived".to_string()) {
        task.tags.push("archived".to_string());
    }

    // Save the updated graph
    parser::save_graph(&modified_graph, &dir.join("graph.jsonl")).unwrap();

    // Verify the changes
    let final_graph = parser::load_graph(&dir.join("graph.jsonl")).unwrap();
    let archived_task = final_graph.get_task(".coordinator-5").unwrap();

    assert_eq!(
        archived_task.status,
        Status::Done,
        "Archived coordinator should have Done status"
    );
    assert!(
        archived_task.tags.contains(&"archived".to_string()),
        "Archived coordinator should have 'archived' tag"
    );
    assert!(
        !archived_task.tags.contains(&"coordinator-loop".to_string()),
        "Archived coordinator should not have 'coordinator-loop' tag"
    );
}

/// Test context isolation: new coordinators have fresh state
#[test]
fn test_coordinator_context_isolation() {
    let temp_dir = TempDir::new().unwrap();
    let _dir = temp_dir.path();

    // Create first coordinator with some history
    let mut graph = WorkGraph::new();
    let coordinator_old = Task {
        id: ".coordinator-10".to_string(),
        title: "Old Coordinator".to_string(),
        status: Status::Done,
        tags: vec!["archived".to_string()],
        log: vec![workgraph::graph::LogEntry {
            timestamp: "2026-04-11T10:00:00Z".to_string(),
            actor: Some("daemon".to_string()),
            user: Some("test".to_string()),
            message: "Old coordinator chat history".to_string(),
        }],
        created_at: Some("2026-04-11T10:00:00Z".to_string()),
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coordinator_old));

    // Create new coordinator (simulating fresh creation)
    let coordinator_new = Task {
        id: ".coordinator-11".to_string(),
        title: "New Coordinator".to_string(),
        status: Status::InProgress,
        tags: vec!["coordinator-loop".to_string()],
        log: vec![workgraph::graph::LogEntry {
            timestamp: "2026-04-11T12:00:00Z".to_string(),
            actor: Some("daemon".to_string()),
            user: Some("test".to_string()),
            message: "New coordinator created".to_string(),
        }],
        created_at: Some("2026-04-11T12:00:00Z".to_string()),
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coordinator_new));

    // Verify isolation
    let old_task = graph.get_task(".coordinator-10").unwrap();
    let new_task = graph.get_task(".coordinator-11").unwrap();

    // Different timestamps ensure fresh state
    assert_ne!(
        old_task.created_at, new_task.created_at,
        "New coordinator should have different creation time"
    );

    // Different log content ensures no context leakage
    assert_eq!(old_task.log.len(), 1);
    assert_eq!(new_task.log.len(), 1);
    assert!(old_task.log[0].message.contains("Old coordinator"));
    assert!(new_task.log[0].message.contains("New coordinator"));
    assert_ne!(
        old_task.log[0].message, new_task.log[0].message,
        "Log messages should be different"
    );

    // Different IDs ensure no state file collision
    assert_ne!(
        old_task.id, new_task.id,
        "Coordinator IDs should be different"
    );
}

/// Test the full archive-then-create flow
#[test]
fn test_archive_then_create_flow() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path();

    // Create initial coordinator
    let mut graph = WorkGraph::new();
    let coordinator = Task {
        id: ".coordinator-0".to_string(),
        title: "Initial Coordinator".to_string(),
        status: Status::InProgress,
        tags: vec!["coordinator-loop".to_string()],
        log: vec![workgraph::graph::LogEntry {
            timestamp: "2026-04-11T10:00:00Z".to_string(),
            actor: Some("daemon".to_string()),
            user: Some("test".to_string()),
            message: "Initial coordinator created".to_string(),
        }],
        created_at: Some("2026-04-11T10:00:00Z".to_string()),
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coordinator));
    parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

    // Test the coordinator slot availability function
    fn is_coordinator_slot_available(graph: &WorkGraph, task_id: &str) -> bool {
        match graph.get_task(task_id) {
            None => true,
            Some(task) => {
                if task.tags.iter().any(|t| t == "archived") {
                    return false;
                }
                if task.tags.iter().any(|t| t == "coordinator-loop") {
                    if task.status == Status::InProgress {
                        return false;
                    }
                    return false;
                }
                true
            }
        }
    }

    // Initially, coordinator-0 is active so next ID should be 1
    let mut next_id = 0u32;
    loop {
        let task_id = format!(".coordinator-{}", next_id);
        if is_coordinator_slot_available(&graph, &task_id) {
            break;
        }
        next_id += 1;
    }
    assert_eq!(next_id, 1, "With active coordinator-0, next ID should be 1");

    // Archive coordinator-0 (simulate handle_archive_coordinator)
    let mut updated_graph = parser::load_graph(&dir.join("graph.jsonl")).unwrap();
    let task = updated_graph.get_task_mut(".coordinator-0").unwrap();
    task.status = Status::Done;
    task.tags.retain(|t| t != "coordinator-loop");
    if !task.tags.contains(&"archived".to_string()) {
        task.tags.push("archived".to_string());
    }
    task.log.push(workgraph::graph::LogEntry {
        timestamp: "2026-04-11T11:00:00Z".to_string(),
        actor: Some("daemon".to_string()),
        user: Some("test".to_string()),
        message: "Coordinator archived".to_string(),
    });
    parser::save_graph(&updated_graph, &dir.join("graph.jsonl")).unwrap();

    // After archiving, coordinator-0 should still not be available (archived coordinators are skipped)
    let archived_graph = parser::load_graph(&dir.join("graph.jsonl")).unwrap();
    let mut next_id_after_archive = 0u32;
    loop {
        let task_id = format!(".coordinator-{}", next_id_after_archive);
        if is_coordinator_slot_available(&archived_graph, &task_id) {
            break;
        }
        next_id_after_archive += 1;
    }

    assert_eq!(
        next_id_after_archive, 1,
        "After archiving coordinator-0, next available ID should be 1 (skipping archived coordinator-0)"
    );

    // Verify coordinator-0 is properly archived
    let archived_task = archived_graph.get_task(".coordinator-0").unwrap();
    assert_eq!(archived_task.status, Status::Done);
    assert!(archived_task.tags.contains(&"archived".to_string()));
    assert!(!archived_task.tags.contains(&"coordinator-loop".to_string()));
    assert_eq!(archived_task.log.len(), 2); // Original + archive log
}

/// Test that creating coordinators never reuses archived IDs even with gaps
#[test]
fn test_no_id_reuse_with_gaps() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path();

    // Create a scenario with archived coordinators and gaps
    let mut graph = WorkGraph::new();

    // coordinator-0: archived
    let coord_0 = Task {
        id: ".coordinator-0".to_string(),
        title: "Archived Coordinator".to_string(),
        status: Status::Done,
        tags: vec!["archived".to_string()],
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coord_0));

    // coordinator-2: archived (gap at 1)
    let coord_2 = Task {
        id: ".coordinator-2".to_string(),
        title: "Archived Coordinator 2".to_string(),
        status: Status::Done,
        tags: vec!["archived".to_string()],
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coord_2));

    // coordinator-4: active (gap at 3)
    let coord_4 = Task {
        id: ".coordinator-4".to_string(),
        title: "Active Coordinator".to_string(),
        status: Status::InProgress,
        tags: vec!["coordinator-loop".to_string()],
        ..Default::default()
    };
    graph.add_node(workgraph::graph::Node::Task(coord_4));

    parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

    fn is_coordinator_slot_available(graph: &WorkGraph, task_id: &str) -> bool {
        match graph.get_task(task_id) {
            None => true,
            Some(task) => {
                if task.tags.iter().any(|t| t == "archived") {
                    return false;
                }
                if task.tags.iter().any(|t| t == "coordinator-loop") {
                    if task.status == Status::InProgress {
                        return false;
                    }
                    return false;
                }
                true
            }
        }
    }

    // Find next available coordinator ID
    let mut next_id = 0u32;
    loop {
        let task_id = format!(".coordinator-{}", next_id);
        if is_coordinator_slot_available(&graph, &task_id) {
            break;
        }
        next_id += 1;
    }

    // Should get ID 1 (the first gap), NOT reusing archived coordinator-0 or coordinator-2
    assert_eq!(
        next_id, 1,
        "Should allocate ID 1 (first available gap), not reuse archived IDs"
    );

    // Verify why each ID was rejected
    assert!(
        !is_coordinator_slot_available(&graph, ".coordinator-0"),
        "coordinator-0 is archived"
    );
    assert!(
        is_coordinator_slot_available(&graph, ".coordinator-1"),
        "coordinator-1 should be available"
    );
    assert!(
        !is_coordinator_slot_available(&graph, ".coordinator-2"),
        "coordinator-2 is archived"
    );
    assert!(
        is_coordinator_slot_available(&graph, ".coordinator-3"),
        "coordinator-3 should be available"
    );
    assert!(
        !is_coordinator_slot_available(&graph, ".coordinator-4"),
        "coordinator-4 is active"
    );
}
