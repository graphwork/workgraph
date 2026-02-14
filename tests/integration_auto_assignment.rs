//! Integration tests for the auto-assignment pipeline.
//!
//! Tests cover:
//! 1. Unit tests for assign subgraph construction logic in service.rs
//!    (coordinator_tick creates assign-* tasks with correct relationships)
//! 2. Assignment CLI: wg assign <task> <agent-hash>, wg assign --clear, prefix matching
//! 3. Integration: create roles+motivations+agents, run assignment subgraph construction,
//!    verify assign-* task is created with correct description/context
//! 4. Assigned agents appear in task.agent field and in rendered prompts
//!
//! LLM-based tests (ones that actually call Claude to do assignment reasoning)
//! are gated behind #[ignore] with the env var WG_TEST_LLM=1 as an alternative gate.

use std::fs;
use std::path::Path;
use tempfile::TempDir;

use workgraph::agency::{self, Agent, Lineage, PerformanceRecord, SkillRef};
use workgraph::config::Config;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks;
use workgraph::service::executor::TemplateVars;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_task(id: &str, title: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        description: None,
        status: Status::Open,
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

fn make_task_with_desc(id: &str, title: &str, desc: &str) -> Task {
    let mut t = make_task(id, title);
    t.description = Some(desc.to_string());
    t
}

fn make_task_with_skills(id: &str, title: &str, skills: Vec<&str>) -> Task {
    let mut t = make_task(id, title);
    t.skills = skills.into_iter().map(|s| s.to_string()).collect();
    t
}

/// Set up a workgraph directory with graph.jsonl containing the given tasks.
fn setup_workgraph(dir: &Path, tasks: Vec<Task>) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &path).unwrap();
}

/// Write a config.toml with auto_assign enabled.
fn write_config_auto_assign(dir: &Path, auto_assign: bool) {
    let content = format!(
        r#"[agency]
auto_assign = {}
"#,
        auto_assign
    );
    fs::write(dir.join("config.toml"), content).unwrap();
}

/// Set up agency with a single role+motivation+agent, returning (agent_id, role_id, motivation_id).
fn setup_agency(dir: &Path) -> (String, String, String) {
    let agency_dir = dir.join("agency");
    agency::init(&agency_dir).unwrap();

    let role = agency::build_role(
        "Implementer",
        "Writes production-quality Rust code",
        vec![SkillRef::Name("rust".to_string())],
        "Working, tested code",
    );
    let role_id = role.id.clone();
    agency::save_role(&role, &agency_dir.join("roles")).unwrap();

    let motivation = agency::build_motivation(
        "Quality First",
        "Prioritise correctness over speed",
        vec!["Slower delivery".to_string()],
        vec!["Skipping tests".to_string()],
    );
    let mot_id = motivation.id.clone();
    agency::save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

    let agent_id = agency::content_hash_agent(&role_id, &mot_id);
    let agent = Agent {
        id: agent_id.clone(),
        role_id: role_id.clone(),
        motivation_id: mot_id.clone(),
        name: "impl-agent".to_string(),
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::default(),
        capabilities: Vec::new(),
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
    };
    agency::save_agent(&agent, &agency_dir.join("agents")).unwrap();

    (agent_id, role_id, mot_id)
}

/// Set up a second agent with a different role, returning its agent_id.
fn setup_second_agent(dir: &Path) -> String {
    let agency_dir = dir.join("agency");

    let role = agency::build_role(
        "Reviewer",
        "Reviews code for correctness",
        vec![SkillRef::Name("code-review".to_string())],
        "Reviewed, approved code",
    );
    let role_id = role.id.clone();
    agency::save_role(&role, &agency_dir.join("roles")).unwrap();

    let motivation = agency::build_motivation(
        "Thoroughness",
        "Leave no stone unturned",
        vec!["Takes longer".to_string()],
        vec!["Rubber-stamping".to_string()],
    );
    let mot_id = motivation.id.clone();
    agency::save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

    let agent_id = agency::content_hash_agent(&role_id, &mot_id);
    let agent = Agent {
        id: agent_id.clone(),
        role_id: role_id.clone(),
        motivation_id: mot_id.clone(),
        name: "review-agent".to_string(),
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::default(),
        capabilities: Vec::new(),
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
    };
    agency::save_agent(&agent, &agency_dir.join("agents")).unwrap();

    agent_id
}

/// Simulate the auto-assign subgraph construction from coordinator_tick.
///
/// This is a faithful extraction of the auto-assign logic from service.rs
/// (lines 340-438) so we can test it without starting a real daemon.
fn build_assign_subgraph(dir: &Path) {
    let config = Config::load(dir).unwrap_or_default();
    if !config.agency.auto_assign {
        return;
    }

    let graph_path = dir.join("graph.jsonl");
    let graph = load_graph(&graph_path).unwrap();
    let ready = ready_tasks(&graph);

    let mut mutable_graph = load_graph(&graph_path).unwrap();
    let mut graph_modified = false;

    for ready_task in &ready {
        if ready_task.agent.is_some() || ready_task.assigned.is_some() {
            continue;
        }

        let assign_task_id = format!("assign-{}", ready_task.id);

        if mutable_graph.get_task(&assign_task_id).is_some() {
            continue;
        }

        let mut desc = format!(
            "Assign an agent to task '{}'.\n\n## Original Task\n**Title:** {}\n",
            ready_task.id, ready_task.title,
        );
        if let Some(ref d) = ready_task.description {
            desc.push_str(&format!("**Description:** {}\n", d));
        }
        if !ready_task.skills.is_empty() {
            desc.push_str(&format!("**Skills:** {}\n", ready_task.skills.join(", ")));
        }
        desc.push_str(&format!(
            "\n## Instructions\n\
             Inspect the agency with `wg agent list`, `wg role list`, etc.\n\
             Choose the best agent for this task, then run:\n\
             ```\n\
             wg assign {} <agent-hash>\n\
             wg done {}\n\
             ```",
            ready_task.id, assign_task_id,
        ));

        let assign_task = Task {
            id: assign_task_id.clone(),
            title: format!("Assign agent for: {}", ready_task.title),
            description: Some(desc),
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec![ready_task.id.clone()],
            blocked_by: vec![],
            requires: vec![],
            tags: vec!["assignment".to_string(), "agency".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: Some(chrono::Utc::now().to_rfc3339()),
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
        };

        mutable_graph.add_node(Node::Task(assign_task));

        if let Some(t) = mutable_graph.get_task_mut(&ready_task.id) {
            if !t.blocked_by.contains(&assign_task_id) {
                t.blocked_by.push(assign_task_id.clone());
            }
        }

        graph_modified = true;
    }

    if graph_modified {
        save_graph(&mutable_graph, &graph_path).unwrap();
    }
}

// ===========================================================================
// 1. Unit tests for assign subgraph construction
// ===========================================================================

#[test]
fn test_assign_subgraph_created_for_ready_task() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(dir, vec![make_task("task-1", "Implement feature X")]);
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();

    // assign-task-1 should exist
    let assign_task = graph.get_task("assign-task-1");
    assert!(assign_task.is_some(), "assign-task-1 should be created");

    let assign_task = assign_task.unwrap();
    assert_eq!(assign_task.status, Status::Open);
    assert!(assign_task.tags.contains(&"assignment".to_string()));
    assert!(assign_task.tags.contains(&"agency".to_string()));
    assert!(
        assign_task.title.contains("Implement feature X"),
        "assign task title should reference original task title"
    );
}

#[test]
fn test_assign_subgraph_blocks_original_task() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(dir, vec![make_task("task-1", "Build thing")]);
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();

    // The assign task should have blocks = [task-1]
    let assign = graph.get_task("assign-task-1").unwrap();
    assert!(
        assign.blocks.contains(&"task-1".to_string()),
        "assign task should block the original task"
    );

    // The original task should have blocked_by = [assign-task-1]
    let original = graph.get_task("task-1").unwrap();
    assert!(
        original.blocked_by.contains(&"assign-task-1".to_string()),
        "original task should be blocked by assign task"
    );

    // After subgraph construction, the original task is no longer ready
    let ready = ready_tasks(&graph);
    let original_ready = ready.iter().any(|t| t.id == "task-1");
    assert!(
        !original_ready,
        "original task should NOT be ready after assign subgraph blocks it"
    );

    // But assign-task-1 should be ready
    let assign_ready = ready.iter().any(|t| t.id == "assign-task-1");
    assert!(assign_ready, "assign-task-1 should be ready (no blockers)");
}

#[test]
fn test_assign_subgraph_includes_description() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(
        dir,
        vec![make_task_with_desc(
            "task-2",
            "Fix the bug",
            "There is a null pointer in parser.rs",
        )],
    );
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();
    let assign = graph.get_task("assign-task-2").unwrap();
    let desc = assign.description.as_ref().unwrap();

    assert!(
        desc.contains("Fix the bug"),
        "assign description should contain original title"
    );
    assert!(
        desc.contains("null pointer in parser.rs"),
        "assign description should contain original description"
    );
    assert!(
        desc.contains("wg assign task-2"),
        "assign description should contain instructions with task ID"
    );
    assert!(
        desc.contains("wg done assign-task-2"),
        "assign description should contain instructions to mark assign task done"
    );
}

#[test]
fn test_assign_subgraph_includes_skills() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(
        dir,
        vec![make_task_with_skills(
            "task-3",
            "Write tests",
            vec!["rust", "testing"],
        )],
    );
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();
    let assign = graph.get_task("assign-task-3").unwrap();
    let desc = assign.description.as_ref().unwrap();

    assert!(
        desc.contains("rust, testing"),
        "assign description should list required skills: got {}",
        desc
    );
}

#[test]
fn test_assign_subgraph_skips_already_assigned_agent() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut task = make_task("task-4", "Already assigned");
    task.agent = Some("some-agent-hash".to_string());
    setup_workgraph(dir, vec![task]);
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();
    assert!(
        graph.get_task("assign-task-4").is_none(),
        "should NOT create assign task for task that already has an agent"
    );
}

#[test]
fn test_assign_subgraph_skips_already_claimed() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut task = make_task("task-5", "Already claimed");
    task.assigned = Some("agent-session-id".to_string());
    setup_workgraph(dir, vec![task]);
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();
    assert!(
        graph.get_task("assign-task-5").is_none(),
        "should NOT create assign task for task that is already claimed (assigned field set)"
    );
}

#[test]
fn test_assign_subgraph_idempotent() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(dir, vec![make_task("task-6", "Idempotent test")]);
    write_config_auto_assign(dir, true);

    // Run twice
    build_assign_subgraph(dir);
    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();

    // The assign-task-6 task should exist exactly once (not duplicated)
    assert!(
        graph.get_task("assign-task-6").is_some(),
        "assign-task-6 should exist"
    );

    // There should be no duplicate: assign-task-6 should not be created twice.
    // On the second run, assign-task-6 is ready and unassigned so it gets
    // assign-assign-task-6, which is expected coordinator behavior (recursion
    // bottoms out at the assign task having no agent). But assign-task-6
    // itself is NOT duplicated.
    let assign_task_6_count = graph.tasks().filter(|t| t.id == "assign-task-6").count();
    assert_eq!(
        assign_task_6_count, 1,
        "assign-task-6 should not be duplicated"
    );
}

#[test]
fn test_assign_subgraph_not_created_when_disabled() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(dir, vec![make_task("task-7", "No auto assign")]);
    write_config_auto_assign(dir, false);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();
    assert!(
        graph.get_task("assign-task-7").is_none(),
        "should NOT create assign task when auto_assign is disabled"
    );
}

#[test]
fn test_assign_subgraph_multiple_ready_tasks() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(
        dir,
        vec![
            make_task("a", "Task A"),
            make_task("b", "Task B"),
            make_task("c", "Task C"),
        ],
    );
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();

    assert!(
        graph.get_task("assign-a").is_some(),
        "assign-a should exist"
    );
    assert!(
        graph.get_task("assign-b").is_some(),
        "assign-b should exist"
    );
    assert!(
        graph.get_task("assign-c").is_some(),
        "assign-c should exist"
    );

    // All original tasks should now be blocked
    for id in &["a", "b", "c"] {
        let task = graph.get_task(id).unwrap();
        let assign_id = format!("assign-{}", id);
        assert!(
            task.blocked_by.contains(&assign_id),
            "task {} should be blocked by {}",
            id,
            assign_id
        );
    }
}

#[test]
fn test_assign_subgraph_skips_blocked_tasks() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    // task-b is blocked by task-a, so only task-a is ready
    let mut task_a = make_task("task-a", "Prerequisite");
    task_a.blocks = vec!["task-b".to_string()];
    let mut task_b = make_task("task-b", "Depends on A");
    task_b.blocked_by = vec!["task-a".to_string()];

    setup_workgraph(dir, vec![task_a, task_b]);
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();

    assert!(
        graph.get_task("assign-task-a").is_some(),
        "assign-task-a should be created (task-a is ready)"
    );
    assert!(
        graph.get_task("assign-task-b").is_none(),
        "assign-task-b should NOT be created (task-b is blocked)"
    );
}

// ===========================================================================
// 2. Assignment CLI tests (wg assign)
// ===========================================================================

// Note: most CLI tests for `wg assign` already exist in src/commands/assign.rs.
// Here we add integration-level tests that exercise the CLI through the library
// API (not spawning a subprocess) to verify the full flow.

#[test]
fn test_assign_sets_agent_field() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(dir, vec![make_task("cli-1", "CLI test task")]);
    let (agent_id, _, _) = setup_agency(dir);

    // Simulate what `wg assign cli-1 <agent-hash>` does:
    // find agent by prefix, then set task.agent
    let agents_dir = dir.join("agency").join("agents");
    let found = agency::find_agent_by_prefix(&agents_dir, &agent_id).unwrap();
    assert_eq!(found.id, agent_id);

    let graph_path = dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task_mut("cli-1").unwrap();
    task.agent = Some(found.id.clone());
    save_graph(&graph, &graph_path).unwrap();

    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("cli-1").unwrap();
    assert_eq!(task.agent, Some(agent_id));
}

#[test]
fn test_assign_cli_clear_removes_agent() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut task = make_task("cli-2", "CLI clear test");
    task.agent = Some("some-hash".to_string());
    setup_workgraph(dir, vec![task]);

    let graph_path = dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task_mut("cli-2").unwrap();
    task.agent = None;
    save_graph(&graph, &graph_path).unwrap();

    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("cli-2").unwrap();
    assert!(task.agent.is_none());
}

#[test]
fn test_assign_cli_prefix_matching() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_workgraph(dir, vec![make_task("cli-3", "Prefix match test")]);
    let (agent_id, _, _) = setup_agency(dir);

    // Prefix match via agency API (same logic as assign command)
    let agents_dir = dir.join("agency").join("agents");
    let prefix = &agent_id[..8];
    let found = agency::find_agent_by_prefix(&agents_dir, prefix).unwrap();
    assert_eq!(found.id, agent_id);
}

// ===========================================================================
// 3. Integration: full assignment pipeline with agency entities
// ===========================================================================

#[test]
fn test_full_assignment_pipeline() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    // Set up agency
    let (agent_id, _, _) = setup_agency(dir);
    let _second_agent_id = setup_second_agent(dir);

    // Set up workgraph with a task that needs assignment
    let task = make_task_with_desc(
        "feature-1",
        "Add authentication",
        "Implement OAuth2 login flow",
    );
    setup_workgraph(dir, vec![task]);
    write_config_auto_assign(dir, true);

    // Run the assignment subgraph construction
    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();

    // Verify assign task was created
    let assign = graph.get_task("assign-feature-1").unwrap();
    assert_eq!(assign.status, Status::Open);
    assert!(assign.blocks.contains(&"feature-1".to_string()));

    let desc = assign.description.as_ref().unwrap();
    assert!(desc.contains("Add authentication"));
    assert!(desc.contains("OAuth2 login flow"));
    assert!(desc.contains("wg assign feature-1"));
    assert!(desc.contains("wg done assign-feature-1"));
    assert!(desc.contains("wg agent list"));

    // Simulate what an assigner agent would do: assign the agent
    let graph_path = dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task_mut("feature-1").unwrap();
    task.agent = Some(agent_id.clone());
    // Mark assign task as done
    let assign = graph.get_task_mut("assign-feature-1").unwrap();
    assign.status = Status::Done;
    save_graph(&graph, &graph_path).unwrap();

    // Verify the original task now has the agent assigned
    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("feature-1").unwrap();
    assert_eq!(task.agent.as_deref(), Some(agent_id.as_str()));

    // With the assign task done, the original task should be ready again
    let ready = ready_tasks(&graph);
    assert!(
        ready.iter().any(|t| t.id == "feature-1"),
        "feature-1 should be ready after assign task completes"
    );
}

#[test]
fn test_assignment_pipeline_with_mixed_tasks() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    setup_agency(dir);

    // Mix of ready, blocked, and already-assigned tasks
    let mut assigned_task = make_task("already-assigned", "Already has agent");
    assigned_task.agent = Some("existing-agent".to_string());

    let mut blocked_task = make_task("blocked-one", "Blocked by other");
    blocked_task.blocked_by = vec!["already-assigned".to_string()];

    let ready_task = make_task("needs-assignment", "Needs an agent");

    setup_workgraph(dir, vec![assigned_task, blocked_task, ready_task]);
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();

    // Only the ready, unassigned task should get an assign subtask
    assert!(
        graph.get_task("assign-needs-assignment").is_some(),
        "unassigned ready task should get an assign subtask"
    );
    assert!(
        graph.get_task("assign-already-assigned").is_none(),
        "already-assigned task should NOT get an assign subtask"
    );
    assert!(
        graph.get_task("assign-blocked-one").is_none(),
        "blocked task should NOT get an assign subtask"
    );
}

// ===========================================================================
// 4. Assigned agents appear in task.agent field and rendered prompts
// ===========================================================================

#[test]
fn test_assigned_agent_appears_in_rendered_prompt() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    // Set up agency in .workgraph
    let agency_dir = wg_dir.join("agency");
    agency::init(&agency_dir).unwrap();

    let role = agency::build_role(
        "Implementer",
        "Writes Rust code",
        vec![SkillRef::Name("rust".to_string())],
        "Working code",
    );
    let role_id = role.id.clone();
    agency::save_role(&role, &agency_dir.join("roles")).unwrap();

    let motivation = agency::build_motivation(
        "Quality First",
        "Prioritise correctness",
        vec!["Slower delivery".to_string()],
        vec!["Skipping tests".to_string()],
    );
    let mot_id = motivation.id.clone();
    agency::save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

    let agent_id = agency::content_hash_agent(&role_id, &mot_id);
    let agent = Agent {
        id: agent_id.clone(),
        role_id,
        motivation_id: mot_id,
        name: "prompt-test-agent".to_string(),
        performance: PerformanceRecord {
            task_count: 5,
            avg_score: Some(0.85),
            evaluations: vec![],
        },
        lineage: Lineage::default(),
        capabilities: Vec::new(),
        rate: None,
        capacity: None,
        trust_level: Default::default(),
        contact: None,
        executor: "claude".to_string(),
    };
    agency::save_agent(&agent, &agency_dir.join("agents")).unwrap();

    // Create a task with the agent assigned
    let mut task = make_task("prompt-task", "Build the widget");
    task.description = Some("Create a new widget component".to_string());
    task.agent = Some(agent_id.clone());

    // Resolve template vars (this is what the executor does before spawning)
    let vars = TemplateVars::from_task(&task, Some("Context from deps"), Some(&wg_dir));

    // The identity should be populated
    assert!(
        !vars.task_identity.is_empty(),
        "task_identity should be populated when agent is assigned"
    );
    assert!(
        vars.task_identity.contains("Implementer"),
        "identity should contain role name"
    );
    // render_identity_prompt uses "Operational Parameters" section, not motivation name.
    // It renders acceptable/unacceptable tradeoffs directly.
    assert!(
        vars.task_identity.contains("Slower delivery"),
        "identity should contain acceptable tradeoffs"
    );
    assert!(
        vars.task_identity.contains("Skipping tests"),
        "identity should contain unacceptable tradeoffs (non-negotiable constraints)"
    );

    // Apply template to verify it renders into prompts
    let prompt_template = "{{task_identity}}\n## Task\nID: {{task_id}}\nTitle: {{task_title}}\nDesc: {{task_description}}\nContext: {{task_context}}";
    let rendered = vars.apply(prompt_template);

    assert!(
        rendered.contains("Implementer"),
        "rendered prompt should contain role name"
    );
    assert!(
        rendered.contains("prompt-task"),
        "rendered prompt should contain task ID"
    );
    assert!(
        rendered.contains("Build the widget"),
        "rendered prompt should contain task title"
    );
    assert!(
        rendered.contains("Create a new widget component"),
        "rendered prompt should contain task description"
    );
    assert!(
        rendered.contains("Context from deps"),
        "rendered prompt should contain dependency context"
    );
}

#[test]
fn test_no_agent_renders_empty_identity() {
    let task = make_task("no-agent-task", "No agent assigned");
    let vars = TemplateVars::from_task(&task, None, None);

    assert_eq!(vars.task_identity, "", "no agent => empty identity");

    let template = "Identity:{{task_identity}}:end";
    let rendered = vars.apply(template);
    assert_eq!(
        rendered, "Identity::end",
        "empty identity should leave no gap"
    );
}

#[test]
fn test_agent_field_persists_through_save_load() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut task = make_task("persist-1", "Persistence test");
    task.agent = Some("abc123def456".to_string());
    setup_workgraph(dir, vec![task]);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();
    let loaded = graph.get_task("persist-1").unwrap();
    assert_eq!(
        loaded.agent.as_deref(),
        Some("abc123def456"),
        "agent field should survive save/load roundtrip"
    );
}

#[test]
fn test_assigned_agent_survives_subgraph_construction() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    // Task with agent set should NOT get an assign subtask
    // AND should retain the agent field after subgraph construction
    let mut task = make_task("survive-1", "Agent survives");
    task.agent = Some("my-agent-hash".to_string());

    // Also add an unassigned task
    let task2 = make_task("survive-2", "Needs assignment");

    setup_workgraph(dir, vec![task, task2]);
    write_config_auto_assign(dir, true);

    build_assign_subgraph(dir);

    let graph = load_graph(&dir.join("graph.jsonl")).unwrap();

    // Agent field should be preserved
    let t1 = graph.get_task("survive-1").unwrap();
    assert_eq!(
        t1.agent.as_deref(),
        Some("my-agent-hash"),
        "agent field should be preserved through subgraph construction"
    );

    // The unassigned task should get an assign subtask
    assert!(graph.get_task("assign-survive-2").is_some());
}

// ===========================================================================
// 5. LLM-gated tests (require actual Claude invocation)
// ===========================================================================

// Run with: cargo test --features llm-tests
// Optionally set WG_TEST_MODEL to pick a model (default: haiku)

#[cfg(feature = "llm-tests")]
mod llm_tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Get the path to the compiled `wg` binary.
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
            args,
            stdout,
            stderr
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

    /// Read task JSON via `wg show --json`.
    fn task_json(wg_dir: &Path, task_id: &str) -> Option<serde_json::Value> {
        let output = wg_cmd(wg_dir, &["show", task_id, "--json"]);
        if !output.status.success() {
            return None;
        }
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).ok()
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

        // wg init doesn't create executors/, so we write a full claude.toml
        // that includes the prompt template (required for stdin piping).
        let executors_dir = wg_dir.join("executors");
        fs::create_dir_all(&executors_dir).unwrap();
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
        fs::write(executors_dir.join("claude.toml"), claude_config).unwrap();

        wg_dir
    }

    fn test_model() -> String {
        std::env::var("WG_TEST_MODEL").unwrap_or_else(|_| "haiku".to_string())
    }

    #[test]
    fn test_llm_assignment_reasoning() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());
        let model = test_model();

        // Set up agency with two agents with different skills
        let (agent_id, _, _) = setup_agency(&wg_dir);
        let second_agent_id = setup_second_agent(&wg_dir);

        // Create the target task that needs Rust skills
        let mut task =
            make_task_with_skills("rust-feature", "Implement a Rust parser", vec!["rust"]);
        task.description = Some("Write a parser for a simple DSL in Rust".to_string());

        // Create a hand-crafted assign task with an explicit, unambiguous description
        // that tells the agent exactly what commands to run. This avoids the problem
        // where the LLM marks the task done without actually running wg assign.
        let assign_desc = format!(
            "You MUST assign an agent to task 'rust-feature'.\n\n\
             ## Step 1: List available agents\n\
             Run this command:\n\
             ```\n\
             wg agent list\n\
             ```\n\n\
             ## Step 2: Choose the best agent for a Rust task\n\
             The task requires the 'rust' skill. Pick the agent whose role best matches.\n\n\
             ## Step 3: Assign the agent (REQUIRED — do NOT skip this)\n\
             ```\n\
             wg assign rust-feature <agent-hash>\n\
             ```\n\
             Replace <agent-hash> with the actual hash from step 1.\n\n\
             ## Step 4: Mark this task done\n\
             Only AFTER running wg assign:\n\
             ```\n\
             wg done assign-rust-feature\n\
             ```\n\n\
             IMPORTANT: You must run BOTH `wg assign` AND `wg done`. Do not skip `wg assign`."
        );
        let assign_task = Task {
            id: "assign-rust-feature".to_string(),
            title: "Assign agent for: Implement a Rust parser".to_string(),
            description: Some(assign_desc),
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec!["rust-feature".to_string()],
            blocked_by: vec![],
            requires: vec![],
            tags: vec!["assignment".to_string(), "agency".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: Some(chrono::Utc::now().to_rfc3339()),
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
        };

        // Wire up: assign-rust-feature blocks rust-feature
        task.blocked_by = vec!["assign-rust-feature".to_string()];
        setup_workgraph(&wg_dir, vec![task, assign_task]);

        // Spawn a real Claude agent on the assign task
        let output = wg_cmd(
            &wg_dir,
            &[
                "spawn",
                "assign-rust-feature",
                "--executor",
                "claude",
                "--model",
                &model,
            ],
        );
        assert!(
            output.status.success(),
            "wg spawn failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Wait for the assign task to complete (up to 120s for LLM)
        let completed = wait_for(Duration::from_secs(120), 1000, || {
            let status = task_status(&wg_dir, "assign-rust-feature");
            status == "done" || status == "failed"
        });
        assert!(
            completed,
            "assign task did not complete within 120s. Status: {}",
            task_status(&wg_dir, "assign-rust-feature")
        );

        // Dump agent output on any failure
        let assign_status = task_status(&wg_dir, "assign-rust-feature");
        let dump_agent_logs = |wg_dir: &Path| {
            let agents_dir = wg_dir.join("agents");
            if agents_dir.exists() {
                for entry in fs::read_dir(&agents_dir).unwrap().filter_map(|e| e.ok()) {
                    for fname in &["output.log", "prompt.txt"] {
                        let fpath = entry.path().join(fname);
                        if fpath.exists() {
                            let content = fs::read_to_string(&fpath).unwrap_or_default();
                            let start = content.len().saturating_sub(3000);
                            eprintln!("--- {} ---\n{}", fpath.display(), &content[start..]);
                        }
                    }
                }
            }
        };

        if assign_status != "done" {
            dump_agent_logs(&wg_dir);
        }
        assert_eq!(
            assign_status, "done",
            "assign task should be done, got: {}",
            assign_status
        );

        // Verify the original task now has an agent assigned
        let task_val = task_json(&wg_dir, "rust-feature");
        assert!(task_val.is_some(), "rust-feature task should exist");
        let task_val = task_val.unwrap();
        let assigned_agent = task_val["agent"].as_str();
        if assigned_agent.is_none() {
            dump_agent_logs(&wg_dir);
        }
        assert!(
            assigned_agent.is_some(),
            "rust-feature task should have an agent assigned after LLM reasoning. Task JSON: {}",
            serde_json::to_string_pretty(&task_val).unwrap()
        );

        // The assigned agent should be one of our two agents
        let assigned = assigned_agent.unwrap();
        assert!(
            assigned == agent_id || assigned == second_agent_id,
            "Assigned agent '{}' should be one of the known agents ({} or {})",
            assigned,
            agent_id,
            second_agent_id
        );

        eprintln!(
            "LLM assignment test passed: agent '{}' was assigned to rust-feature (model: {})",
            assigned, model
        );
    }

    /// Test that `wg evaluate` can evaluate a completed task and record
    /// an evaluation JSON file with scores and dimensions.
    ///
    /// Flow:
    /// 1. Set up agency with an agent, assign it to a task
    /// 2. Manually mark the task done (no need to spawn an LLM for that)
    /// 3. Run `wg evaluate` which internally spawns its own Claude process
    /// 4. Verify evaluation file was created with valid scores
    #[test]
    fn test_llm_evaluation_recording() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());
        let model = test_model();

        // Set up agency with an agent
        let (agent_id, _, _) = setup_agency(&wg_dir);

        // Create a task, assign the agent, and mark it done
        let mut task = make_task_with_desc(
            "eval-target",
            "Implement fizzbuzz",
            "Write a fizzbuzz function in Rust that prints 1..100",
        );
        task.agent = Some(agent_id.clone());
        task.status = Status::Done;
        task.started_at = Some(chrono::Utc::now().to_rfc3339());
        task.completed_at = Some(chrono::Utc::now().to_rfc3339());
        task.log = vec![workgraph::graph::LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: Some("agent-1".to_string()),
            message: "Implemented fizzbuzz with pattern matching".to_string(),
        }];
        setup_workgraph(&wg_dir, vec![task]);

        // Write config so wg evaluate uses our test model
        let config_content = format!("[agency]\nevaluator_model = \"{}\"\n", model);
        fs::write(wg_dir.join("config.toml"), &config_content).unwrap();

        // Run wg evaluate directly (it spawns its own claude process internally)
        let output = wg_cmd(
            &wg_dir,
            &["evaluate", "eval-target", "--evaluator-model", &model],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "wg evaluate failed.\nstdout: {}\nstderr: {}",
            stdout,
            stderr
        );

        // Verify evaluation JSON was created
        let evaluations_dir = wg_dir.join("agency").join("evaluations");
        assert!(
            evaluations_dir.exists(),
            "evaluations directory should exist after wg evaluate"
        );

        let eval_files: Vec<_> = fs::read_dir(&evaluations_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "json"))
            .collect();
        assert!(
            !eval_files.is_empty(),
            "At least one evaluation JSON should exist"
        );

        // Parse the evaluation and check it has valid structure
        let eval_content = fs::read_to_string(eval_files[0].path()).unwrap();
        let eval: serde_json::Value =
            serde_json::from_str(&eval_content).expect("Evaluation file should be valid JSON");

        assert!(
            eval["score"].is_f64() || eval["score"].is_i64(),
            "Evaluation should have a numeric score. Got: {}",
            serde_json::to_string_pretty(&eval).unwrap()
        );
        let score = eval["score"].as_f64().unwrap();
        assert!(
            (0.0..=1.0).contains(&score),
            "Score should be between 0 and 1, got: {}",
            score
        );

        eprintln!(
            "LLM evaluation test passed: score={:.2} (model: {})",
            score, model
        );
    }
}
