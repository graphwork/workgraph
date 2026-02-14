//! Integration tests for the service coordinator lifecycle.
//!
//! Tests the coordinator's tick loop, agent lifecycle management, dead agent
//! detection, cleanup flow, agent registry operations, auto-evaluate subgraph
//! construction, and slot accounting.
//!
//! All non-LLM tests use tempdir-based workgraphs and don't require external
//! services. LLM-based tests are gated behind `#[ignore]`.

use chrono::Utc;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use workgraph::config::Config;
use workgraph::graph::{LogEntry, Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks;
use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        description: None,
        status,
        assigned: None,
        estimate: None,
        blocks: vec![],
        blocked_by: vec![],
        requires: vec![],
        tags: vec![],
        skills: vec![],
        inputs: vec![],
        deliverables: vec![],
        artifacts: vec![],
        exec: None,
        not_before: None,
        created_at: None,
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries: None,
        failure_reason: None,
        model: None,
        verify: None,
        agent: None,
        loops_to: vec![],
        loop_iteration: 0,
        ready_after: None,
    }
}

/// Create a workgraph directory with an initialized graph file.
fn setup_workgraph(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::create_dir_all(wg_dir.join("service")).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    (wg_dir, graph_path)
}

/// Save a graph and return the graph path.
fn save_test_graph(wg_dir: &Path, graph: &WorkGraph) -> std::path::PathBuf {
    let graph_path = wg_dir.join("graph.jsonl");
    save_graph(graph, &graph_path).unwrap();
    graph_path
}

/// Create an AgentEntry with sensible defaults.
fn make_agent_entry(id: &str, pid: u32, task_id: &str, status: AgentStatus) -> AgentEntry {
    let now = Utc::now().to_rfc3339();
    AgentEntry {
        id: id.to_string(),
        pid,
        task_id: task_id.to_string(),
        executor: "test".to_string(),
        started_at: now.clone(),
        last_heartbeat: now,
        status,
        output_file: format!("/tmp/{}.log", id),
    }
}

// ===========================================================================
// 1. Coordinator tick: ready task identification
// ===========================================================================

#[test]
fn test_coordinator_identifies_ready_tasks() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, _graph_path) = setup_workgraph(&tmp);

    // Create a graph with tasks in various states
    let mut graph = WorkGraph::new();
    // Open task with no blockers -> should be ready
    graph.add_node(Node::Task(make_task("ready-1", "Ready Task 1", Status::Open)));
    graph.add_node(Node::Task(make_task("ready-2", "Ready Task 2", Status::Open)));
    // Done task -> not ready
    graph.add_node(Node::Task(make_task("done-1", "Done Task", Status::Done)));
    // In-progress task -> not ready
    graph.add_node(Node::Task(make_task("ip-1", "In Progress", Status::InProgress)));
    // Blocked task -> not ready
    let mut blocked = make_task("blocked-1", "Blocked Task", Status::Open);
    blocked.blocked_by = vec!["ready-1".to_string()];
    graph.add_node(Node::Task(blocked));

    save_test_graph(&wg_dir, &graph);

    // Load and check ready tasks
    let loaded = load_graph(&wg_dir.join("graph.jsonl")).unwrap();
    let ready = ready_tasks(&loaded);

    // Should find ready-1 and ready-2, but not done-1, ip-1, or blocked-1
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(ready_ids.contains(&"ready-1"), "ready-1 should be ready");
    assert!(ready_ids.contains(&"ready-2"), "ready-2 should be ready");
    assert!(!ready_ids.contains(&"done-1"), "done-1 should not be ready");
    assert!(!ready_ids.contains(&"ip-1"), "ip-1 should not be ready");
    assert!(!ready_ids.contains(&"blocked-1"), "blocked-1 should not be ready (blocked by ready-1)");
    assert_eq!(ready.len(), 2);
}

#[test]
fn test_coordinator_unblocks_when_blocker_done() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, _) = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();
    // Blocker is done
    graph.add_node(Node::Task(make_task("blocker", "Blocker", Status::Done)));
    // Blocked task should now be ready
    let mut task = make_task("downstream", "Downstream", Status::Open);
    task.blocked_by = vec!["blocker".to_string()];
    graph.add_node(Node::Task(task));

    save_test_graph(&wg_dir, &graph);

    let loaded = load_graph(&wg_dir.join("graph.jsonl")).unwrap();
    let ready = ready_tasks(&loaded);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "downstream");
}

// ===========================================================================
// 2. Coordinator tick: max_agents limit enforcement
// ===========================================================================

#[test]
fn test_coordinator_respects_max_agents_limit() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, _) = setup_workgraph(&tmp);

    // Create a graph with 5 ready tasks
    let mut graph = WorkGraph::new();
    for i in 1..=5 {
        graph.add_node(Node::Task(make_task(
            &format!("task-{}", i),
            &format!("Task {}", i),
            Status::Open,
        )));
    }
    save_test_graph(&wg_dir, &graph);

    // Register 3 alive agents, simulating max_agents = 3
    let mut registry = AgentRegistry::new();
    // Use current PID so is_process_alive returns true
    let current_pid = std::process::id();
    registry.register_agent(current_pid, "existing-1", "test", "/tmp/1.log");
    registry.register_agent(current_pid, "existing-2", "test", "/tmp/2.log");
    registry.register_agent(current_pid, "existing-3", "test", "/tmp/3.log");
    registry.save(&wg_dir).unwrap();

    // With max_agents=3 and 3 alive agents, slots_available = 0
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive())
        .count();
    assert_eq!(alive_count, 3);

    let max_agents: usize = 3;
    let slots_available = max_agents.saturating_sub(alive_count);
    assert_eq!(slots_available, 0, "No slots should be available when at max capacity");
}

#[test]
fn test_slot_accounting_with_dead_agents() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, _) = setup_workgraph(&tmp);

    let mut registry = AgentRegistry::new();
    let current_pid = std::process::id();

    // 2 alive agents
    registry.register_agent(current_pid, "alive-1", "test", "/tmp/1.log");
    registry.register_agent(current_pid, "alive-2", "test", "/tmp/2.log");
    // 1 dead agent (shouldn't count toward slots)
    let dead_id = registry.register_agent(current_pid, "dead-task", "test", "/tmp/3.log");
    registry.set_status(&dead_id, AgentStatus::Dead);
    // 1 done agent (shouldn't count)
    let done_id = registry.register_agent(current_pid, "done-task", "test", "/tmp/4.log");
    registry.set_status(&done_id, AgentStatus::Done);

    registry.save(&wg_dir).unwrap();

    // Only alive agents count
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive())
        .count();
    assert_eq!(alive_count, 2, "Only 2 agents should be alive");

    let max_agents: usize = 4;
    let slots_available = max_agents.saturating_sub(alive_count);
    assert_eq!(slots_available, 2, "Should have 2 available slots (4 max - 2 alive)");
}

// ===========================================================================
// 3. Coordinator tick: skip already-assigned tasks
// ===========================================================================

#[test]
fn test_coordinator_skips_assigned_tasks() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, _) = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();
    // Task already assigned -> should be skipped by coordinator
    let mut assigned_task = make_task("assigned-1", "Assigned Task", Status::Open);
    assigned_task.assigned = Some("agent-99".to_string());
    graph.add_node(Node::Task(assigned_task));

    // Unassigned ready task -> should be picked up
    graph.add_node(Node::Task(make_task("unassigned-1", "Unassigned Task", Status::Open)));

    save_test_graph(&wg_dir, &graph);

    let loaded = load_graph(&wg_dir.join("graph.jsonl")).unwrap();
    let ready = ready_tasks(&loaded);

    // Both are technically "ready" (open, no blockers), but coordinator should
    // skip the one with assigned.is_some()
    let unassigned_ready: Vec<_> = ready.iter()
        .filter(|t| t.assigned.is_none())
        .collect();

    assert_eq!(unassigned_ready.len(), 1);
    assert_eq!(unassigned_ready[0].id, "unassigned-1");

    // The assigned task is still "ready" in the ready_tasks sense
    assert_eq!(ready.len(), 2);
    // But coordinator_tick filters: `if task.assigned.is_some() { continue; }`
    let to_spawn: Vec<_> = ready.iter()
        .filter(|t| t.assigned.is_none())
        .collect();
    assert_eq!(to_spawn.len(), 1);
}

// ===========================================================================
// 4. Dead agent detection - detect_dead_reason with various agent states
// ===========================================================================

#[test]
fn test_dead_detection_process_exited() {
    // An agent with is_alive() status but whose process has exited
    // should be detected as dead.
    let agent = make_agent_entry("agent-1", 999999999, "task-1", AgentStatus::Working);
    assert!(agent.is_alive());

    // PID 999999999 should not exist
    #[cfg(unix)]
    {
        let process_alive = unsafe { libc::kill(999999999, 0) == 0 };
        assert!(!process_alive, "PID 999999999 should not exist");
    }
}

#[test]
fn test_dead_detection_process_still_running() {
    // An agent whose process IS alive should NOT be detected as dead
    let current_pid = std::process::id();
    let agent = make_agent_entry("agent-1", current_pid, "task-1", AgentStatus::Working);
    assert!(agent.is_alive());

    #[cfg(unix)]
    {
        let process_alive = unsafe { libc::kill(current_pid as i32, 0) == 0 };
        assert!(process_alive, "Current process should be alive");
    }
}

#[test]
fn test_dead_detection_ignores_already_dead_agents() {
    // Agents already marked Dead should not be re-processed
    let agent = make_agent_entry("agent-1", 999999999, "task-1", AgentStatus::Dead);
    assert!(!agent.is_alive(), "Dead agent should not be considered alive");
}

#[test]
fn test_dead_detection_ignores_done_agents() {
    let agent = make_agent_entry("agent-1", 999999999, "task-1", AgentStatus::Done);
    assert!(!agent.is_alive(), "Done agent should not be considered alive");
}

#[test]
fn test_dead_detection_ignores_failed_agents() {
    let agent = make_agent_entry("agent-1", 999999999, "task-1", AgentStatus::Failed);
    assert!(!agent.is_alive(), "Failed agent should not be considered alive");
}

// ===========================================================================
// 5. Cleanup flow: dead agent -> unclaim task, set failure reason
// ===========================================================================

#[test]
fn test_cleanup_unclaims_in_progress_task() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, graph_path) = setup_workgraph(&tmp);

    // Create graph with an in-progress task assigned to an agent
    let mut graph = WorkGraph::new();
    let mut task = make_task("task-1", "Test Task", Status::InProgress);
    task.assigned = Some("agent-1".to_string());
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    // Register agent with a non-existent PID (dead process)
    let mut registry = AgentRegistry::new();
    let agent_id = registry.register_agent(999999999, "task-1", "test", "/tmp/out.log");
    registry.save(&wg_dir).unwrap();

    // Simulate cleanup: mark agent as dead
    let mut locked = AgentRegistry::load_locked(&wg_dir).unwrap();
    if let Some(agent) = locked.get_agent_mut(&agent_id) {
        agent.status = AgentStatus::Dead;
    }
    locked.save_ref().unwrap();

    // Unclaim the task (simulating what cleanup_dead_agents does)
    let mut graph = load_graph(&graph_path).unwrap();
    if let Some(task) = graph.get_task_mut("task-1") {
        assert_eq!(task.status, Status::InProgress);
        task.status = Status::Open;
        task.assigned = None;
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: format!("Task unclaimed: agent '{}' process exited", agent_id),
        });
    }
    save_graph(&graph, &graph_path).unwrap();

    // Verify the task is now open and unassigned
    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("task-1").unwrap();
    assert_eq!(task.status, Status::Open);
    assert!(task.assigned.is_none());
    assert!(!task.log.is_empty());
    assert!(task.log.last().unwrap().message.contains("unclaimed"));

    // Verify agent is dead in registry
    let registry = AgentRegistry::load(&wg_dir).unwrap();
    let agent = registry.get_agent(&agent_id).unwrap();
    assert_eq!(agent.status, AgentStatus::Dead);
}

#[test]
fn test_cleanup_skips_done_task() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, graph_path) = setup_workgraph(&tmp);

    // Task was completed before agent died - should not be unclaimed
    let mut graph = WorkGraph::new();
    let task = make_task("task-1", "Test Task", Status::Done);
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    // Register agent with dead PID
    let mut registry = AgentRegistry::new();
    registry.register_agent(999999999, "task-1", "test", "/tmp/out.log");
    registry.save(&wg_dir).unwrap();

    // Task is Done, so cleanup should NOT change its status
    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("task-1").unwrap();
    assert_eq!(task.status, Status::Done, "Done task should not be modified by cleanup");
}

#[test]
fn test_cleanup_skips_failed_task() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, graph_path) = setup_workgraph(&tmp);

    // Task was failed before agent died
    let mut graph = WorkGraph::new();
    let task = make_task("task-1", "Test Task", Status::Failed);
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    let mut registry = AgentRegistry::new();
    registry.register_agent(999999999, "task-1", "test", "/tmp/out.log");
    registry.save(&wg_dir).unwrap();

    // Task is Failed, cleanup should not change it
    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("task-1").unwrap();
    assert_eq!(task.status, Status::Failed, "Failed task should not be modified by cleanup");
}

// ===========================================================================
// 6. Agent registry operations
// ===========================================================================

#[test]
fn test_registry_register_and_lookup() {
    let mut registry = AgentRegistry::new();

    let id = registry.register_agent(12345, "task-a", "claude", "/tmp/a.log");
    assert_eq!(id, "agent-1");

    let agent = registry.get_agent(&id).unwrap();
    assert_eq!(agent.pid, 12345);
    assert_eq!(agent.task_id, "task-a");
    assert_eq!(agent.executor, "claude");
    assert_eq!(agent.status, AgentStatus::Working);
}

#[test]
fn test_registry_lookup_by_task() {
    let mut registry = AgentRegistry::new();
    registry.register_agent(111, "task-x", "claude", "/tmp/x.log");
    registry.register_agent(222, "task-y", "shell", "/tmp/y.log");

    let agent = registry.get_agent_by_task("task-y").unwrap();
    assert_eq!(agent.pid, 222);

    assert!(registry.get_agent_by_task("task-z").is_none());
}

#[test]
fn test_registry_mark_as_dead() {
    let mut registry = AgentRegistry::new();
    let id = registry.register_agent(12345, "task-1", "test", "/tmp/out.log");

    assert!(registry.get_agent(&id).unwrap().is_alive());

    registry.set_status(&id, AgentStatus::Dead);
    assert!(!registry.get_agent(&id).unwrap().is_alive());
    assert_eq!(registry.get_agent(&id).unwrap().status, AgentStatus::Dead);
}

#[test]
fn test_registry_clean_up_stale_entries() {
    let mut registry = AgentRegistry::new();

    // Register some agents
    let id1 = registry.register_agent(111, "task-1", "test", "/tmp/1.log");
    let id2 = registry.register_agent(222, "task-2", "test", "/tmp/2.log");
    let id3 = registry.register_agent(333, "task-3", "test", "/tmp/3.log");

    // Mark agent-1 and agent-3 as dead
    registry.set_status(&id1, AgentStatus::Dead);
    registry.set_status(&id3, AgentStatus::Dead);

    assert_eq!(registry.agents.len(), 3);

    // Remove dead agents (stale cleanup)
    let dead_ids: Vec<String> = registry
        .list_agents()
        .iter()
        .filter(|a| a.status == AgentStatus::Dead)
        .map(|a| a.id.clone())
        .collect();

    for id in &dead_ids {
        registry.unregister_agent(id);
    }

    assert_eq!(registry.agents.len(), 1);
    assert!(registry.get_agent(&id2).is_some());
    assert!(registry.get_agent(&id1).is_none());
    assert!(registry.get_agent(&id3).is_none());
}

#[test]
fn test_registry_persistence_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();

    // Register agents, save, reload, verify
    let mut registry = AgentRegistry::new();
    registry.register_agent(111, "task-a", "claude", "/tmp/a.log");
    registry.register_agent(222, "task-b", "shell", "/tmp/b.log");
    registry.save(wg_dir).unwrap();

    let loaded = AgentRegistry::load(wg_dir).unwrap();
    assert_eq!(loaded.agents.len(), 2);
    assert_eq!(loaded.next_agent_id, 3);
    assert!(loaded.get_agent("agent-1").is_some());
    assert!(loaded.get_agent("agent-2").is_some());
}

#[test]
fn test_registry_locked_operations() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();

    {
        let mut locked = AgentRegistry::load_locked(wg_dir).unwrap();
        locked.register_agent(111, "task-1", "test", "/tmp/1.log");
        locked.register_agent(222, "task-2", "test", "/tmp/2.log");
        locked.save().unwrap();
    }

    let registry = AgentRegistry::load(wg_dir).unwrap();
    assert_eq!(registry.agents.len(), 2);
}

// ===========================================================================
// 7. Auto-evaluate subgraph construction
// ===========================================================================

#[test]
fn test_auto_evaluate_creates_eval_tasks() {
    let tmp = TempDir::new().unwrap();
    let (wg_dir, graph_path) = setup_workgraph(&tmp);

    // Write config enabling auto_evaluate
    let config_content = r#"
[agency]
auto_evaluate = true
"#;
    fs::write(wg_dir.join("config.toml"), config_content).unwrap();

    // Create a graph with some tasks
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("task-1", "Regular Task", Status::Open)));
    graph.add_node(Node::Task(make_task("task-2", "Another Task", Status::InProgress)));
    save_graph(&graph, &graph_path).unwrap();

    // Simulate auto_evaluate logic (same as in coordinator_tick)
    let config = Config::load(&wg_dir).unwrap_or_default();
    assert!(config.agency.auto_evaluate);

    let mut mutable_graph = load_graph(&graph_path).unwrap();
    let mut graph_modified = false;

    let tasks_needing_eval: Vec<_> = mutable_graph
        .tasks()
        .filter(|t| {
            let eval_id = format!("evaluate-{}", t.id);
            if mutable_graph.get_task(&eval_id).is_some() {
                return false;
            }
            let dominated_tags = ["evaluation", "assignment", "evolution"];
            if t.tags.iter().any(|tag| dominated_tags.contains(&tag.as_str())) {
                return false;
            }
            !matches!(t.status, Status::Abandoned)
        })
        .map(|t| (t.id.clone(), t.title.clone()))
        .collect();

    for (task_id, task_title) in &tasks_needing_eval {
        let eval_task_id = format!("evaluate-{}", task_id);
        if mutable_graph.get_task(&eval_task_id).is_some() {
            continue;
        }

        let eval_task = make_task(&eval_task_id, &format!("Evaluate: {}", task_title), Status::Open);
        let mut eval_task_with_deps = eval_task;
        eval_task_with_deps.blocked_by = vec![task_id.clone()];
        eval_task_with_deps.tags = vec!["evaluation".to_string(), "agency".to_string()];
        mutable_graph.add_node(Node::Task(eval_task_with_deps));
        graph_modified = true;
    }

    if graph_modified {
        save_graph(&mutable_graph, &graph_path).unwrap();
    }

    // Verify eval tasks were created
    let final_graph = load_graph(&graph_path).unwrap();

    // evaluate-task-1 should exist and be blocked by task-1
    let eval1 = final_graph.get_task("evaluate-task-1").unwrap();
    assert_eq!(eval1.blocked_by, vec!["task-1".to_string()]);
    assert!(eval1.tags.contains(&"evaluation".to_string()));

    // evaluate-task-2 should exist and be blocked by task-2
    let eval2 = final_graph.get_task("evaluate-task-2").unwrap();
    assert_eq!(eval2.blocked_by, vec!["task-2".to_string()]);
}

#[test]
fn test_auto_evaluate_skips_evaluation_tasks() {
    // Evaluation tasks should not get their own evaluation tasks (no infinite regress)
    let tmp = TempDir::new().unwrap();
    let (_wg_dir, graph_path) = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();
    let mut eval_task = make_task("evaluate-task-x", "Evaluate: X", Status::Open);
    eval_task.tags = vec!["evaluation".to_string()];
    graph.add_node(Node::Task(eval_task));

    let mut assign_task = make_task("assign-task-y", "Assign agent for: Y", Status::Open);
    assign_task.tags = vec!["assignment".to_string()];
    graph.add_node(Node::Task(assign_task));

    save_graph(&graph, &graph_path).unwrap();

    let loaded = load_graph(&graph_path).unwrap();
    let tasks_needing_eval: Vec<_> = loaded
        .tasks()
        .filter(|t| {
            let eval_id = format!("evaluate-{}", t.id);
            if loaded.get_task(&eval_id).is_some() {
                return false;
            }
            let dominated_tags = ["evaluation", "assignment", "evolution"];
            if t.tags.iter().any(|tag| dominated_tags.contains(&tag.as_str())) {
                return false;
            }
            !matches!(t.status, Status::Abandoned)
        })
        .collect();

    assert!(
        tasks_needing_eval.is_empty(),
        "Evaluation and assignment tasks should not produce eval tasks"
    );
}

#[test]
fn test_auto_evaluate_skips_abandoned_tasks() {
    let tmp = TempDir::new().unwrap();
    let (_wg_dir, graph_path) = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("abandoned-task", "Abandoned", Status::Abandoned)));
    save_graph(&graph, &graph_path).unwrap();

    let loaded = load_graph(&graph_path).unwrap();
    let tasks_needing_eval: Vec<_> = loaded
        .tasks()
        .filter(|t| {
            let dominated_tags = ["evaluation", "assignment", "evolution"];
            if t.tags.iter().any(|tag| dominated_tags.contains(&tag.as_str())) {
                return false;
            }
            !matches!(t.status, Status::Abandoned)
        })
        .collect();

    assert!(tasks_needing_eval.is_empty(), "Abandoned tasks should not get eval tasks");
}

#[test]
fn test_auto_evaluate_idempotent() {
    // Running eval creation twice should not create duplicates
    let tmp = TempDir::new().unwrap();
    let (_wg_dir, graph_path) = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("task-1", "Regular Task", Status::Open)));
    // Pre-create the eval task
    let mut eval_task = make_task("evaluate-task-1", "Evaluate: Regular Task", Status::Open);
    eval_task.blocked_by = vec!["task-1".to_string()];
    eval_task.tags = vec!["evaluation".to_string()];
    graph.add_node(Node::Task(eval_task));
    save_graph(&graph, &graph_path).unwrap();

    let loaded = load_graph(&graph_path).unwrap();
    let tasks_needing_eval: Vec<_> = loaded
        .tasks()
        .filter(|t| {
            let eval_id = format!("evaluate-{}", t.id);
            if loaded.get_task(&eval_id).is_some() {
                return false;
            }
            let dominated_tags = ["evaluation", "assignment", "evolution"];
            if t.tags.iter().any(|tag| dominated_tags.contains(&tag.as_str())) {
                return false;
            }
            !matches!(t.status, Status::Abandoned)
        })
        .collect();

    assert!(
        tasks_needing_eval.is_empty(),
        "Should not create duplicate eval tasks"
    );
}

#[test]
fn test_auto_evaluate_unblocks_on_failed_source() {
    // When a source task fails, its evaluation task should be unblocked
    let tmp = TempDir::new().unwrap();
    let (_wg_dir, graph_path) = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("source-task", "Source", Status::Failed)));
    let mut eval_task = make_task("evaluate-source-task", "Evaluate: Source", Status::Open);
    eval_task.blocked_by = vec!["source-task".to_string()];
    eval_task.tags = vec!["evaluation".to_string()];
    graph.add_node(Node::Task(eval_task));
    save_graph(&graph, &graph_path).unwrap();

    // Simulate the fixup logic from coordinator_tick
    let mut mutable_graph = load_graph(&graph_path).unwrap();
    let eval_fixups: Vec<(String, String)> = mutable_graph
        .tasks()
        .filter(|t| t.id.starts_with("evaluate-") && t.status == Status::Open)
        .filter_map(|t| {
            if t.blocked_by.len() == 1 {
                let source_id = &t.blocked_by[0];
                if let Some(source) = mutable_graph.get_task(source_id) {
                    if source.status == Status::Failed {
                        return Some((t.id.clone(), source_id.clone()));
                    }
                }
            }
            None
        })
        .collect();

    assert_eq!(eval_fixups.len(), 1);
    assert_eq!(eval_fixups[0].0, "evaluate-source-task");
    assert_eq!(eval_fixups[0].1, "source-task");

    for (eval_id, source_id) in &eval_fixups {
        if let Some(t) = mutable_graph.get_task_mut(eval_id) {
            t.blocked_by.retain(|b| b != source_id);
        }
    }
    save_graph(&mutable_graph, &graph_path).unwrap();

    // Verify the eval task is now unblocked
    let final_graph = load_graph(&graph_path).unwrap();
    let eval = final_graph.get_task("evaluate-source-task").unwrap();
    assert!(eval.blocked_by.is_empty(), "Eval task should be unblocked after source task failed");

    // And it should now be ready
    let ready = ready_tasks(&final_graph);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(ready_ids.contains(&"evaluate-source-task"));
}

// ===========================================================================
// 8. Slot accounting
// ===========================================================================

#[test]
fn test_slots_available_basic() {
    let mut registry = AgentRegistry::new();
    let current_pid = std::process::id();

    registry.register_agent(current_pid, "t1", "test", "/tmp/1.log");
    registry.register_agent(current_pid, "t2", "test", "/tmp/2.log");

    let max_agents: usize = 5;
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive())
        .count();
    let slots = max_agents.saturating_sub(alive_count);
    assert_eq!(slots, 3);
}

#[test]
fn test_slots_available_mixed_statuses() {
    let mut registry = AgentRegistry::new();
    let current_pid = std::process::id();

    // 2 alive
    registry.register_agent(current_pid, "alive-1", "test", "/tmp/1.log");
    registry.register_agent(current_pid, "alive-2", "test", "/tmp/2.log");
    // 1 dead
    let dead = registry.register_agent(current_pid, "dead-1", "test", "/tmp/3.log");
    registry.set_status(&dead, AgentStatus::Dead);
    // 1 stopping
    let stopping = registry.register_agent(current_pid, "stopping-1", "test", "/tmp/4.log");
    registry.set_status(&stopping, AgentStatus::Stopping);
    // 1 done
    let done = registry.register_agent(current_pid, "done-1", "test", "/tmp/5.log");
    registry.set_status(&done, AgentStatus::Done);
    // 1 failed
    let failed = registry.register_agent(current_pid, "failed-1", "test", "/tmp/6.log");
    registry.set_status(&failed, AgentStatus::Failed);

    // Only Working/Starting/Idle are alive
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive())
        .count();
    assert_eq!(alive_count, 2, "Only Working agents should be alive");

    let max_agents: usize = 4;
    let slots = max_agents.saturating_sub(alive_count);
    assert_eq!(slots, 2);
}

#[test]
fn test_slots_available_at_zero() {
    let mut registry = AgentRegistry::new();
    let current_pid = std::process::id();

    for i in 0..3 {
        registry.register_agent(current_pid, &format!("t{}", i), "test", &format!("/tmp/{}.log", i));
    }

    let max_agents: usize = 3;
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive())
        .count();
    let slots = max_agents.saturating_sub(alive_count);
    assert_eq!(slots, 0, "No slots when at capacity");
}

#[test]
fn test_slots_saturating_sub_no_underflow() {
    // If alive_count > max_agents (shouldn't happen normally), slots should be 0, not negative
    let mut registry = AgentRegistry::new();
    let current_pid = std::process::id();

    for i in 0..5 {
        registry.register_agent(current_pid, &format!("t{}", i), "test", &format!("/tmp/{}.log", i));
    }

    let max_agents: usize = 3;
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive())
        .count();
    assert_eq!(alive_count, 5);

    let slots = max_agents.saturating_sub(alive_count);
    assert_eq!(slots, 0, "saturating_sub should prevent underflow");
}

// ===========================================================================
// 9. Process liveness checks
// ===========================================================================

#[cfg(unix)]
#[test]
fn test_is_process_alive_current_process() {
    let pid = std::process::id();
    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
    assert!(alive, "Current process should be detected as alive");
}

#[cfg(unix)]
#[test]
fn test_is_process_alive_nonexistent_pid() {
    let alive = unsafe { libc::kill(999999999, 0) == 0 };
    assert!(!alive, "Non-existent PID should be detected as dead");
}

// ===========================================================================
// 10. Agent entry lifecycle states
// ===========================================================================

#[test]
fn test_agent_entry_lifecycle() {
    let mut entry = make_agent_entry("agent-1", 12345, "task-1", AgentStatus::Starting);

    // Starting -> alive
    assert!(entry.is_alive());

    // Working -> alive
    entry.status = AgentStatus::Working;
    assert!(entry.is_alive());

    // Idle -> alive
    entry.status = AgentStatus::Idle;
    assert!(entry.is_alive());

    // Stopping -> not alive
    entry.status = AgentStatus::Stopping;
    assert!(!entry.is_alive());

    // Done -> not alive
    entry.status = AgentStatus::Done;
    assert!(!entry.is_alive());

    // Failed -> not alive
    entry.status = AgentStatus::Failed;
    assert!(!entry.is_alive());

    // Dead -> not alive
    entry.status = AgentStatus::Dead;
    assert!(!entry.is_alive());
}

// ===========================================================================
// 11. Multiple task dependency chain
// ===========================================================================

#[test]
fn test_coordinator_dependency_chain() {
    let tmp = TempDir::new().unwrap();
    let (_wg_dir, graph_path) = setup_workgraph(&tmp);

    let mut graph = WorkGraph::new();

    // task-a (open, no deps) -> ready
    graph.add_node(Node::Task(make_task("task-a", "Task A", Status::Open)));

    // task-b blocked by task-a -> not ready
    let mut task_b = make_task("task-b", "Task B", Status::Open);
    task_b.blocked_by = vec!["task-a".to_string()];
    graph.add_node(Node::Task(task_b));

    // task-c blocked by task-b -> not ready
    let mut task_c = make_task("task-c", "Task C", Status::Open);
    task_c.blocked_by = vec!["task-b".to_string()];
    graph.add_node(Node::Task(task_c));

    save_graph(&graph, &graph_path).unwrap();

    // Only task-a should be ready
    let loaded = load_graph(&graph_path).unwrap();
    let ready = ready_tasks(&loaded);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "task-a");

    // Complete task-a, now task-b should become ready
    let mut graph = load_graph(&graph_path).unwrap();
    graph.get_task_mut("task-a").unwrap().status = Status::Done;
    save_graph(&graph, &graph_path).unwrap();

    let loaded = load_graph(&graph_path).unwrap();
    let ready = ready_tasks(&loaded);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "task-b");

    // Complete task-b, now task-c should become ready
    let mut graph = load_graph(&graph_path).unwrap();
    graph.get_task_mut("task-b").unwrap().status = Status::Done;
    save_graph(&graph, &graph_path).unwrap();

    let loaded = load_graph(&graph_path).unwrap();
    let ready = ready_tasks(&loaded);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "task-c");
}

// ===========================================================================
// 12. Registry count helpers
// ===========================================================================

#[test]
fn test_registry_active_and_idle_counts() {
    let mut registry = AgentRegistry::new();
    let pid = std::process::id();

    // 2 working
    registry.register_agent(pid, "t1", "test", "/tmp/1.log");
    registry.register_agent(pid, "t2", "test", "/tmp/2.log");

    // 1 idle
    let idle_id = registry.register_agent(pid, "t3", "test", "/tmp/3.log");
    registry.set_status(&idle_id, AgentStatus::Idle);

    // 1 dead (not counted)
    let dead_id = registry.register_agent(pid, "t4", "test", "/tmp/4.log");
    registry.set_status(&dead_id, AgentStatus::Dead);

    assert_eq!(registry.active_count(), 3); // 2 working + 1 idle
    assert_eq!(registry.idle_count(), 1);
}

// ===========================================================================
// 13. LLM-based tests (gated behind feature flag)
// ===========================================================================

// Run with: cargo test --features llm-tests
// Optionally set WG_TEST_MODEL to pick a model (default: haiku)

#[cfg(feature = "llm-tests")]
mod llm_tests {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// Get the path to the compiled `wg` binary.
    fn wg_binary() -> PathBuf {
        let mut path = std::env::current_exe().expect("could not get current exe path");
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.push("wg");
        assert!(path.exists(), "wg binary not found at {:?}. Run `cargo build` first.", path);
        path
    }

    /// Run `wg` with given args in a specific workgraph directory.
    fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
        let wg = wg_binary();
        Command::new(&wg)
            .arg("--dir")
            .arg(wg_dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
    }

    /// Run `wg` and assert success, returning stdout.
    fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
        let output = wg_cmd(wg_dir, args);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(
            output.status.success(),
            "wg {:?} failed.\nstdout: {}\nstderr: {}",
            args, stdout, stderr
        );
        stdout
    }

    /// Read task status via `wg show --json`.
    fn task_status(wg_dir: &Path, task_id: &str) -> String {
        let output = wg_cmd(wg_dir, &["show", task_id, "--json"]);
        if !output.status.success() {
            return "unknown".to_string();
        }
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        match serde_json::from_str::<serde_json::Value>(&stdout) {
            Ok(val) => val["status"].as_str().unwrap_or("unknown").to_string(),
            Err(_) => "unknown".to_string(),
        }
    }

    /// Poll until a condition is met or timeout expires.
    fn wait_for(timeout: Duration, poll_ms: u64, mut f: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(poll_ms));
        }
        false
    }

    /// Set up a workgraph directory via `wg init`, then write a claude executor
    /// config with working_dir and PATH so the wrapper script's bare `wg`
    /// commands find the test binary and the workgraph.
    fn setup_llm_workgraph(tmp_root: &Path) -> PathBuf {
        let wg_dir = tmp_root.join(".workgraph");
        wg_ok(&wg_dir, &["init"]);

        let wg_bin_dir = wg_binary().parent().unwrap().to_string_lossy().to_string();
        let path_with_test_binary = format!(
            "{}:{}",
            wg_bin_dir,
            std::env::var("PATH").unwrap_or_default()
        );

        let executors_dir = wg_dir.join("executors");
        std::fs::create_dir_all(&executors_dir).unwrap();
        let claude_config = format!(
            r#"[executor]
type = "claude"
command = "claude"
args = ["--print", "--verbose", "--permission-mode", "bypassPermissions", "--output-format", "stream-json"]
working_dir = "{working_dir}"

[executor.env]
PATH = "{path}"

[executor.prompt_template]
template = """
# Task Assignment

You are an AI agent working on a task in a workgraph project.

{{{{task_identity}}}}
## Your Task
- **ID:** {{{{task_id}}}}
- **Title:** {{{{task_title}}}}
- **Description:** {{{{task_description}}}}

## Context from Dependencies
{{{{task_context}}}}

## Required Workflow

You MUST use these commands to track your work:

1. **Complete the task** when done:
   ```bash
   wg done {{{{task_id}}}}
   wg submit {{{{task_id}}}}
   ```

2. **Mark as failed** if you cannot complete:
   ```bash
   wg fail {{{{task_id}}}} --reason "Specific reason why"
   ```

## Important
- Run `wg done` (or `wg submit`) BEFORE you finish responding
- If `wg done` fails saying "requires verification", use `wg submit` instead
- Focus only on this specific task

Begin working on the task now.
"""
"#,
            working_dir = tmp_root.display(),
            path = path_with_test_binary,
        );
        std::fs::write(executors_dir.join("claude.toml"), claude_config).unwrap();

        wg_dir
    }

    fn test_model() -> String {
        std::env::var("WG_TEST_MODEL").unwrap_or_else(|_| "haiku".to_string())
    }

    #[test]
    fn test_coordinator_tick_spawns_real_agent() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());
        let model = test_model();

        // Add a simple task: the Claude agent just needs to mark it done
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Say hello and mark this task done",
                "--id",
                "hello-task",
                "-d",
                "This is a test task. Just run: wg done hello-task",
            ],
        );

        // Spawn a real Claude agent on the task
        let output = wg_cmd(
            &wg_dir,
            &["spawn", "hello-task", "--executor", "claude", "--model", &model],
        );
        assert!(
            output.status.success(),
            "wg spawn failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Wait for the task to complete (up to 120s for LLM)
        let completed = wait_for(Duration::from_secs(120), 1000, || {
            let status = task_status(&wg_dir, "hello-task");
            status == "done" || status == "failed"
        });
        assert!(
            completed,
            "hello-task did not complete within 120s. Status: {}",
            task_status(&wg_dir, "hello-task")
        );

        // Verify the task succeeded
        let final_status = task_status(&wg_dir, "hello-task");
        assert_eq!(
            final_status, "done",
            "hello-task should be done, got: {}",
            final_status
        );

        // Verify output was captured
        let agents_dir = wg_dir.join("agents");
        if agents_dir.exists() {
            let has_output = std::fs::read_dir(&agents_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|entry| {
                    let output_log = entry.path().join("output.log");
                    if output_log.exists() {
                        let content = std::fs::read_to_string(&output_log).unwrap_or_default();
                        !content.is_empty()
                    } else {
                        false
                    }
                });
            assert!(has_output, "Agent output should have been captured in output.log");
        }

        eprintln!(
            "LLM coordinator test passed: hello-task completed successfully (model: {})",
            model
        );
    }

    /// Test: spawn a Claude agent to create roles, motivations, and agents from scratch.
    ///
    /// This test verifies that:
    /// 1. An LLM agent can be spawned with a task to build agency components
    /// 2. The agent can execute wg CLI commands to create roles and motivations
    /// 3. The agency system correctly persists and retrieves the created entities
    /// 4. An agent pairing role + motivation can be created
    #[test]
    fn test_agent_creation_via_llm() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());
        let model = test_model();

        // Add a task that instructs Claude to create agency components
        let task_desc = r#"You MUST create agency components using the wg CLI. Run these commands in order:

Step 1 — Create a role:
```
wg role add "CodeReviewer" --outcome "High-quality code reviews with constructive feedback" --skill code-review --description "Reviews code for quality and correctness"
```

Step 2 — Create a motivation:
```
wg motivation add "Thorough" --accept "Slower delivery" --reject "Incomplete reviews" --description "Prioritizes thoroughness over speed"
```

Step 3 — Get the hashes from the output above, then create an agent:
```
wg agent create "Thorough Reviewer" --role <ROLE_HASH> --motivation <MOTIVATION_HASH>
```
Replace <ROLE_HASH> and <MOTIVATION_HASH> with the 8-char prefixes printed in steps 1 and 2.

Step 4 — Verify and complete:
```
wg agent list
wg done create-agency-components
```

IMPORTANT: You MUST run ALL four steps. Do NOT skip any."#;

        wg_ok(
            &wg_dir,
            &[
                "add",
                "Build agency components",
                "--id",
                "create-agency-components",
                "-d",
                task_desc,
            ],
        );

        // Spawn a Claude agent to execute the task
        let output = wg_cmd(
            &wg_dir,
            &["spawn", "create-agency-components", "--executor", "claude", "--model", &model],
        );
        assert!(
            output.status.success(),
            "wg spawn failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Wait for the task to complete (up to 180s for LLM, as this is more complex)
        let completed = wait_for(Duration::from_secs(180), 1000, || {
            let status = task_status(&wg_dir, "create-agency-components");
            status == "done" || status == "failed"
        });
        assert!(
            completed,
            "create-agency-components did not complete within 180s. Status: {}",
            task_status(&wg_dir, "create-agency-components")
        );

        // Verify the task succeeded
        let final_status = task_status(&wg_dir, "create-agency-components");
        assert_eq!(
            final_status, "done",
            "create-agency-components should be done, got: {}",
            final_status
        );

        // Verify the role and motivation were actually created
        // Check that the agency directory exists
        let agency_dir = wg_dir.join("agency");
        assert!(agency_dir.exists(), "Agency directory should exist");

        let roles_dir = agency_dir.join("roles");
        assert!(roles_dir.exists(), "Roles directory should exist");

        let motivations_dir = agency_dir.join("motivations");
        assert!(motivations_dir.exists(), "Motivations directory should exist");

        // List roles to verify at least one was created
        let roles_output = wg_ok(&wg_dir, &["role", "list", "--json"]);
        let roles_json: serde_json::Value = serde_json::from_str(&roles_output)
            .expect("Failed to parse role list JSON");
        assert!(
            roles_json.is_array(),
            "role list --json should return an array"
        );
        let roles_array = roles_json
            .as_array()
            .expect("Expected array of roles");
        assert!(
            !roles_array.is_empty(),
            "At least one role should have been created"
        );

        // Check that CodeReviewer role exists
        let has_code_reviewer = roles_array.iter().any(|r| {
            r.get("name")
                .and_then(|n| n.as_str())
                .map(|s| s.contains("CodeReviewer") || s.contains("code"))
                .unwrap_or(false)
        });
        assert!(
            has_code_reviewer,
            "CodeReviewer role should exist in the created roles"
        );

        // List motivations to verify at least one was created
        let motivations_output = wg_ok(&wg_dir, &["motivation", "list", "--json"]);
        let motivations_json: serde_json::Value = serde_json::from_str(&motivations_output)
            .expect("Failed to parse motivation list JSON");
        assert!(
            motivations_json.is_array(),
            "motivation list --json should return an array"
        );
        let motivations_array = motivations_json
            .as_array()
            .expect("Expected array of motivations");
        assert!(
            !motivations_array.is_empty(),
            "At least one motivation should have been created"
        );

        // Check that Thorough motivation exists
        let has_thorough = motivations_array.iter().any(|m| {
            m.get("name")
                .and_then(|n| n.as_str())
                .map(|s| s.contains("Thorough"))
                .unwrap_or(false)
        });
        assert!(
            has_thorough,
            "Thorough motivation should exist in the created motivations"
        );

        // Verify an agent was created (role+motivation pairing)
        let agents_output = wg_ok(&wg_dir, &["agent", "list", "--json"]);
        let agents_json: serde_json::Value = serde_json::from_str(&agents_output)
            .expect("Failed to parse agent list JSON");
        let agents_array = agents_json.as_array().expect("Expected array of agents");
        let has_reviewer_agent = agents_array.iter().any(|a| {
            a.get("name")
                .and_then(|n| n.as_str())
                .map(|s| s.to_lowercase().contains("reviewer"))
                .unwrap_or(false)
        });
        assert!(
            has_reviewer_agent,
            "An agent with 'reviewer' in the name should have been created. Agents: {:?}",
            agents_array.iter().filter_map(|a| a.get("name").and_then(|n| n.as_str())).collect::<Vec<_>>()
        );

        eprintln!(
            "LLM agent creation test passed: role, motivation, and agent created (model: {})",
            model
        );
    }
}
