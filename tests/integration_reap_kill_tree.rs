//! Integration tests for the reap + kill-tree features.
//!
//! Test scenarios:
//! 1. Spawn-storm simulation: create a task tree (A → B,C → D), assign fake agents,
//!    kill --tree A — verify all agents killed, all tasks abandoned
//! 2. Reap after kill: kill --tree leaves dead agents, wg reap cleans them up
//! 3. Partial tree: kill --tree on a mid-tree task only affects downstream, not upstream
//! 4. Already-done tasks in the tree: kill --tree skips done tasks
//! 5. --dry-run for both commands shows correct output without side effects
//! 6. Edge case: kill --tree on a task with no agent — should still abandon downstream

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::{AgentRegistry, AgentStatus};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

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

fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        ..Task::default()
    }
}

/// Create the .workgraph directory with a graph and return its path.
fn setup_workgraph(tmp: &TempDir, tasks: Vec<Task>) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

/// Load the current graph from the workgraph directory.
fn graph(wg_dir: &Path) -> WorkGraph {
    load_graph(wg_dir.join("graph.jsonl")).unwrap()
}

/// Save a registry into the workgraph service directory.
fn save_registry(wg_dir: &Path, registry: &AgentRegistry) {
    let service_dir = wg_dir.join("service");
    std::fs::create_dir_all(&service_dir).unwrap();
    registry.save(wg_dir).unwrap();
}

/// Load the registry from the workgraph service directory.
fn load_registry(wg_dir: &Path) -> AgentRegistry {
    AgentRegistry::load(wg_dir).unwrap()
}

/// Build a 4-task tree:
///   task-a
///     ├── task-b
///     │     └── task-d
///     └── task-c
///
/// task-b and task-c depend on task-a. task-d depends on task-b.
fn build_tree_tasks() -> Vec<Task> {
    let a = make_task("task-a", "Task A (root)", Status::InProgress);

    let mut b = make_task("task-b", "Task B", Status::InProgress);
    b.after = vec!["task-a".to_string()];

    let mut c = make_task("task-c", "Task C", Status::Open);
    c.after = vec!["task-a".to_string()];

    let mut d = make_task("task-d", "Task D (leaf)", Status::Open);
    d.after = vec!["task-b".to_string()];

    vec![a, b, c, d]
}

/// Register fake agents for given task IDs. Uses PID 999999999 (not running).
fn register_fake_agents(registry: &mut AgentRegistry, task_ids: &[&str]) {
    for &tid in task_ids {
        let aid = registry.register_agent(999999999, tid, "claude", "/dev/null");
        // Keep them as Working so they show up as alive in the registry
        registry.set_status(&aid, AgentStatus::Working);
    }
}

// ---------------------------------------------------------------------------
// Test 1: Spawn-storm — kill --tree on root abandons entire tree
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_abandons_all_tasks_in_tree() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    // Register fake agents for task-a and task-b (in-progress tasks)
    let mut registry = AgentRegistry::new();
    register_fake_agents(&mut registry, &["task-a", "task-b"]);
    save_registry(&wg_dir, &registry);

    // Kill the tree rooted at task-a
    let output = wg_ok(&wg_dir, &["kill", "--tree", "task-a", "--force"]);

    // Verify output mentions killing and abandoning
    assert!(
        output.contains("abandoned") || output.contains("Abandoned") || output.contains("agent"),
        "Expected kill --tree output to mention abandoned/agents, got: {}",
        output
    );

    // Verify all tasks are abandoned
    let g = graph(&wg_dir);
    assert_eq!(g.get_task("task-a").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("task-b").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("task-c").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("task-d").unwrap().status, Status::Abandoned);

    // Verify failure_reason mentions root
    for tid in &["task-b", "task-c", "task-d"] {
        let reason = g
            .get_task(tid)
            .unwrap()
            .failure_reason
            .as_ref()
            .expect("expected failure_reason");
        assert!(
            reason.contains("task-a"),
            "Expected failure_reason to mention root task-a for {}, got: {}",
            tid,
            reason
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: Reap after kill — dead agents get cleaned up
// ---------------------------------------------------------------------------

#[test]
fn test_reap_after_kill_tree_cleans_dead_agents() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    // Register agents in Dead/Done/Failed states (simulating post-kill state)
    let mut registry = AgentRegistry::new();
    let a1 = registry.register_agent(999999999, "task-a", "claude", "/dev/null");
    registry.set_status(&a1, AgentStatus::Dead);
    let a2 = registry.register_agent(999999998, "task-b", "claude", "/dev/null");
    registry.set_status(&a2, AgentStatus::Dead);
    // One alive agent for a different task (should not be reaped)
    let a3 = registry.register_agent(999999997, "task-c", "claude", "/dev/null");
    registry.set_status(&a3, AgentStatus::Working);
    save_registry(&wg_dir, &registry);

    // Verify pre-reap state
    let pre = load_registry(&wg_dir);
    assert_eq!(pre.agents.len(), 3);

    // Reap dead agents
    let output = wg_ok(&wg_dir, &["reap"]);
    assert!(
        output.contains("Reaped") || output.contains("reaped") || output.contains("2"),
        "Expected reap output to mention reaped agents, got: {}",
        output
    );

    // Verify post-reap: only the alive agent remains
    let post = load_registry(&wg_dir);
    assert_eq!(
        post.agents.len(),
        1,
        "Expected only 1 alive agent after reap"
    );
    assert!(
        post.get_agent(&a3).is_some(),
        "Alive agent should still be in registry"
    );
    assert!(
        post.get_agent(&a1).is_none(),
        "Dead agent a1 should have been reaped"
    );
    assert!(
        post.get_agent(&a2).is_none(),
        "Dead agent a2 should have been reaped"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Partial tree — mid-tree kill only affects downstream, not upstream
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_partial_only_affects_downstream() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    // Kill tree rooted at task-b (mid-tree) — should affect task-b and task-d only
    let output = wg_ok(&wg_dir, &["kill", "--tree", "task-b", "--force"]);

    let g = graph(&wg_dir);

    // Upstream tasks should be unaffected
    assert_eq!(
        g.get_task("task-a").unwrap().status,
        Status::InProgress,
        "task-a (upstream) should remain InProgress"
    );
    assert_eq!(
        g.get_task("task-c").unwrap().status,
        Status::Open,
        "task-c (sibling) should remain Open"
    );

    // Downstream tasks should be abandoned
    assert_eq!(
        g.get_task("task-b").unwrap().status,
        Status::Abandoned,
        "task-b (root of subtree) should be Abandoned"
    );
    assert_eq!(
        g.get_task("task-d").unwrap().status,
        Status::Abandoned,
        "task-d (downstream of task-b) should be Abandoned"
    );

    // Verify output mentions correct counts
    assert!(
        output.contains("abandoned") || output.contains("Abandoned"),
        "Expected kill output to mention abandonment, got: {}",
        output
    );
}

// ---------------------------------------------------------------------------
// Test 4: Already-done tasks are skipped — don't abandon completed work
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_skips_done_tasks() {
    let tmp = TempDir::new().unwrap();

    // Build tree with task-b already done
    let a = make_task("task-a", "Task A", Status::InProgress);

    let mut b = make_task("task-b", "Task B (done)", Status::Done);
    b.after = vec!["task-a".to_string()];

    let mut c = make_task("task-c", "Task C", Status::Open);
    c.after = vec!["task-a".to_string()];

    let mut d = make_task("task-d", "Task D", Status::Open);
    d.after = vec!["task-b".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a, b, c, d]);

    wg_ok(&wg_dir, &["kill", "--tree", "task-a", "--force"]);

    let g = graph(&wg_dir);

    // task-b was Done — it should stay Done
    assert_eq!(
        g.get_task("task-b").unwrap().status,
        Status::Done,
        "Done tasks should not be re-abandoned"
    );

    // Non-terminal tasks should be abandoned
    assert_eq!(g.get_task("task-a").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("task-c").unwrap().status, Status::Abandoned);
    // task-d is Open (non-terminal), should be abandoned
    assert_eq!(g.get_task("task-d").unwrap().status, Status::Abandoned);
}

// ---------------------------------------------------------------------------
// Test 5a: --dry-run for kill --tree shows output without side effects
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_dry_run_no_side_effects() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    // Register a fake agent
    let mut registry = AgentRegistry::new();
    register_fake_agents(&mut registry, &["task-a"]);
    save_registry(&wg_dir, &registry);

    let output = wg_ok(&wg_dir, &["kill", "--tree", "task-a", "--dry-run"]);

    // Dry run should mention what it would do
    assert!(
        output.contains("Dry run") || output.contains("dry_run") || output.contains("Would"),
        "Expected dry-run output, got: {}",
        output
    );

    // Verify no side effects — all tasks remain as they were
    let g = graph(&wg_dir);
    assert_eq!(g.get_task("task-a").unwrap().status, Status::InProgress);
    assert_eq!(g.get_task("task-b").unwrap().status, Status::InProgress);
    assert_eq!(g.get_task("task-c").unwrap().status, Status::Open);
    assert_eq!(g.get_task("task-d").unwrap().status, Status::Open);

    // Verify registry unchanged
    let post_reg = load_registry(&wg_dir);
    assert_eq!(
        post_reg.agents.len(),
        1,
        "Registry should be unchanged after dry run"
    );
}

// ---------------------------------------------------------------------------
// Test 5b: --dry-run for reap shows output without removing agents
// ---------------------------------------------------------------------------

#[test]
fn test_reap_dry_run_no_side_effects() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    // Register dead agents
    let mut registry = AgentRegistry::new();
    let a1 = registry.register_agent(999999999, "task-a", "claude", "/dev/null");
    registry.set_status(&a1, AgentStatus::Dead);
    let a2 = registry.register_agent(999999998, "task-b", "claude", "/dev/null");
    registry.set_status(&a2, AgentStatus::Failed);
    save_registry(&wg_dir, &registry);

    let output = wg_ok(&wg_dir, &["reap", "--dry-run"]);

    // Should mention dry-run
    assert!(
        output.contains("Would reap") || output.contains("dry_run"),
        "Expected dry-run output, got: {}",
        output
    );

    // Registry should be unchanged
    let post = load_registry(&wg_dir);
    assert_eq!(
        post.agents.len(),
        2,
        "Registry should be unchanged after reap --dry-run"
    );
}

// ---------------------------------------------------------------------------
// Test 6: kill --tree on task with no agent — still abandons downstream
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_no_agent_still_abandons_downstream() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    // No agents registered at all — task-a has no agent
    // kill --tree should still abandon all downstream tasks
    let output = wg_ok(&wg_dir, &["kill", "--tree", "task-a", "--force"]);

    let g = graph(&wg_dir);
    assert_eq!(g.get_task("task-a").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("task-b").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("task-c").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("task-d").unwrap().status, Status::Abandoned);

    // Output should mention 0 agents killed but tasks abandoned
    assert!(
        output.contains("0 agent") || output.contains("Killed 0"),
        "Expected 0 agents killed output, got: {}",
        output
    );
}

// ---------------------------------------------------------------------------
// Test 7: Combined workflow — kill --tree, then reap, full lifecycle
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_then_reap_full_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    // Register agents for multiple tasks (use non-existent PIDs)
    let mut registry = AgentRegistry::new();
    register_fake_agents(&mut registry, &["task-a", "task-b"]);
    // Also add a dead agent from a previous run (should be reapable)
    let old_agent = registry.register_agent(999999990, "some-old-task", "claude", "/dev/null");
    registry.set_status(&old_agent, AgentStatus::Dead);
    save_registry(&wg_dir, &registry);

    // Step 1: kill --tree — agents for task-a and task-b get killed
    // (PIDs don't exist so the kill will fail gracefully, but tasks get abandoned)
    wg_ok(&wg_dir, &["kill", "--tree", "task-a", "--force"]);

    // Verify tasks are abandoned
    let g = graph(&wg_dir);
    assert_eq!(g.get_task("task-a").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("task-b").unwrap().status, Status::Abandoned);

    // Step 2: reap — clean up dead/done/failed agents from registry
    wg_ok(&wg_dir, &["reap"]);

    // The old dead agent should have been reaped.
    // The tree-killed agents may or may not remain depending on whether
    // kill --tree successfully unregistered them. With non-existent PIDs,
    // they get unregistered during kill, so only the old dead agent matters.
    let post = load_registry(&wg_dir);
    assert!(
        post.get_agent(&old_agent).is_none(),
        "Old dead agent should have been reaped"
    );
}

// ---------------------------------------------------------------------------
// Test 8: kill --tree JSON output
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    let output = wg_cmd(&wg_dir, &["kill", "--tree", "task-a", "--force", "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "kill --tree --json failed: {}",
        stdout
    );

    // Parse JSON output
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON output: {}\n{}", e, stdout));

    assert_eq!(json["root_task"], "task-a");
    let abandoned = json["abandoned_tasks"].as_array().unwrap();
    assert_eq!(abandoned.len(), 4, "Expected 4 abandoned tasks");
}

// ---------------------------------------------------------------------------
// Test 9: kill --tree dry-run JSON output
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_dry_run_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    let output = wg_cmd(
        &wg_dir,
        &["kill", "--tree", "task-a", "--dry-run", "--json"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "kill --tree --dry-run --json failed: {}",
        stdout
    );

    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON output: {}\n{}", e, stdout));

    assert_eq!(json["dry_run"], true);
    assert_eq!(json["root_task"], "task-a");
    assert_eq!(json["total_tasks_in_tree"], 4);

    // Verify nothing changed
    let g = graph(&wg_dir);
    assert_eq!(g.get_task("task-a").unwrap().status, Status::InProgress);
}

// ---------------------------------------------------------------------------
// Test 10: reap JSON output
// ---------------------------------------------------------------------------

#[test]
fn test_reap_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    let mut registry = AgentRegistry::new();
    let a1 = registry.register_agent(999999999, "task-a", "claude", "/dev/null");
    registry.set_status(&a1, AgentStatus::Dead);
    let a2 = registry.register_agent(999999998, "task-b", "claude", "/dev/null");
    registry.set_status(&a2, AgentStatus::Done);
    save_registry(&wg_dir, &registry);

    let output = wg_cmd(&wg_dir, &["reap", "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "reap --json failed: {}", stdout);

    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Invalid JSON output: {}\n{}", e, stdout));

    assert_eq!(json["dry_run"], false);
    assert_eq!(json["count"], 2);
    assert_eq!(json["dead"], 1);
    assert_eq!(json["done"], 1);
}

// ---------------------------------------------------------------------------
// Test 11: reap with --older-than filter
// ---------------------------------------------------------------------------

#[test]
fn test_reap_older_than_filter() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    let mut registry = AgentRegistry::new();
    // Agent that "died" 2 hours ago
    let old_id = registry.register_agent(999999999, "task-a", "claude", "/dev/null");
    registry.set_status(&old_id, AgentStatus::Dead);
    if let Some(agent) = registry.get_agent_mut(&old_id) {
        agent.completed_at = Some((chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339());
    }
    // Agent that "died" 5 seconds ago — should NOT be reaped with --older-than 1h
    let recent_id = registry.register_agent(999999998, "task-b", "claude", "/dev/null");
    registry.set_status(&recent_id, AgentStatus::Dead);
    if let Some(agent) = registry.get_agent_mut(&recent_id) {
        agent.completed_at = Some(chrono::Utc::now().to_rfc3339());
    }
    save_registry(&wg_dir, &registry);

    wg_ok(&wg_dir, &["reap", "--older-than", "1h"]);

    let post = load_registry(&wg_dir);
    assert!(
        post.get_agent(&old_id).is_none(),
        "Old dead agent should have been reaped"
    );
    assert!(
        post.get_agent(&recent_id).is_some(),
        "Recent dead agent should NOT have been reaped (too new)"
    );
}

// ---------------------------------------------------------------------------
// Test 12: kill --tree on nonexistent task fails gracefully
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_nonexistent_task_fails() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, build_tree_tasks());

    let output = wg_cmd(&wg_dir, &["kill", "--tree", "nonexistent-task"]);
    assert!(
        !output.status.success(),
        "kill --tree on nonexistent task should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found"),
        "Expected 'not found' error, got: {}",
        stderr
    );
}

// ---------------------------------------------------------------------------
// Test 13: Diamond dependency — kill --tree from one branch
// ---------------------------------------------------------------------------

#[test]
fn test_kill_tree_diamond_from_branch() {
    let tmp = TempDir::new().unwrap();

    // Build diamond: root -> [left, right] -> merge
    let root = make_task("root", "Root", Status::InProgress);

    let mut left = make_task("left", "Left", Status::InProgress);
    left.after = vec!["root".to_string()];

    let mut right = make_task("right", "Right", Status::Open);
    right.after = vec!["root".to_string()];

    let mut merge = make_task("merge", "Merge", Status::Open);
    merge.after = vec!["left".to_string(), "right".to_string()];

    // Separate task not in tree
    let standalone = make_task("standalone", "Standalone", Status::Open);

    let wg_dir = setup_workgraph(&tmp, vec![root, left, right, merge, standalone]);

    // Kill tree from "left" — should get left + merge (merge depends on left)
    wg_ok(&wg_dir, &["kill", "--tree", "left", "--force"]);

    let g = graph(&wg_dir);

    // Root should be untouched (upstream)
    assert_eq!(g.get_task("root").unwrap().status, Status::InProgress);

    // Right should be untouched (sibling, not downstream of left)
    assert_eq!(g.get_task("right").unwrap().status, Status::Open);

    // Left and merge should be abandoned
    assert_eq!(g.get_task("left").unwrap().status, Status::Abandoned);
    assert_eq!(g.get_task("merge").unwrap().status, Status::Abandoned);

    // Standalone should be untouched
    assert_eq!(g.get_task("standalone").unwrap().status, Status::Open);
}
