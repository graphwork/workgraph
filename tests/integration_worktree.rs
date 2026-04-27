//! Integration tests for worktree isolation and CARGO_TARGET_DIR per-worktree.
//!
//! This verifies that agents running in isolated worktrees don't contend
//! over cargo file locks, which was the #1 source of task failures.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;
use tempfile::TempDir;

// Test helper to initialize a git repo with basic Rust project structure
fn init_test_repo(path: &Path) {
    // Initialize git
    Command::new("git")
        .args(["init"])
        .arg(path)
        .output()
        .expect("Failed to init git repo");

    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(path)
        .output()
        .expect("Failed to set git email");

    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .expect("Failed to set git name");

    // Create a basic Rust project
    std::fs::write(
        path.join("Cargo.toml"),
        r#"
[package]
name = "testproject"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "testbin"
path = "src/main.rs"
"#,
    )
    .expect("Failed to write Cargo.toml");

    std::fs::create_dir_all(path.join("src")).expect("Failed to create src dir");

    std::fs::write(
        path.join("src/main.rs"),
        r#"
fn main() {
    println!("Hello from test project");
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_basic() {
        assert_eq!(2 + 2, 4);
    }
}
"#,
    )
    .expect("Failed to write src/main.rs");

    // Initial commit
    Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .expect("Failed to git add");

    Command::new("git")
        .args(["commit", "-m", "initial commit"])
        .current_dir(path)
        .output()
        .expect("Failed to git commit");
}

// Test helper to create a worktree
fn create_test_worktree(project_root: &Path, agent_id: &str) -> std::path::PathBuf {
    let worktree_dir = project_root.join(".wg-worktrees").join(agent_id);
    let branch = format!("wg/{}/test-task", agent_id);

    std::fs::create_dir_all(&worktree_dir.parent().unwrap())
        .expect("Failed to create worktrees dir");

    let output = Command::new("git")
        .args(["worktree", "add"])
        .arg(&worktree_dir)
        .args(["-b", &branch, "HEAD"])
        .current_dir(project_root)
        .output()
        .expect("Failed to create worktree");

    if !output.status.success() {
        panic!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    worktree_dir
}

#[test]
fn test_worktree_cargo_isolation() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("Failed to create project dir");

    // Initialize the test repo
    init_test_repo(&project_root);

    // Create two worktrees
    let wt1 = create_test_worktree(&project_root, "agent-1");
    let wt2 = create_test_worktree(&project_root, "agent-2");

    // Verify worktrees exist
    assert!(wt1.exists(), "Worktree 1 should exist");
    assert!(wt2.exists(), "Worktree 2 should exist");

    // Test concurrent cargo operations with different target dirs
    let start = Instant::now();

    let handle1 = std::thread::spawn({
        let wt1 = wt1.clone();
        move || {
            let mut cmd = Command::new("cargo");
            cmd.arg("test")
                .current_dir(&wt1)
                .env("CARGO_TARGET_DIR", wt1.join("target"))
                .stdout(Stdio::null())
                .stderr(Stdio::null());

            let output = cmd.output().expect("Failed to run cargo test in wt1");
            output.status.success()
        }
    });

    let handle2 = std::thread::spawn({
        let wt2 = wt2.clone();
        move || {
            let mut cmd = Command::new("cargo");
            cmd.arg("test")
                .current_dir(&wt2)
                .env("CARGO_TARGET_DIR", wt2.join("target"))
                .stdout(Stdio::null())
                .stderr(Stdio::null());

            let output = cmd.output().expect("Failed to run cargo test in wt2");
            output.status.success()
        }
    });

    // Wait for both to complete
    let result1 = handle1.join().expect("Thread 1 panicked");
    let result2 = handle2.join().expect("Thread 2 panicked");
    let elapsed = start.elapsed();

    // Both should succeed
    assert!(result1, "Cargo test in worktree 1 should succeed");
    assert!(result2, "Cargo test in worktree 2 should succeed");

    // If they were properly isolated, they should complete relatively quickly
    // (not serialized waiting for locks). This is a rough heuristic.
    assert!(
        elapsed.as_secs() < 30,
        "Concurrent tests should complete in reasonable time if properly isolated"
    );

    println!(
        "✓ Worktree isolation test passed - concurrent cargo operations completed in {:?}",
        elapsed
    );
}

#[test]
fn test_worktree_isolation_default_config() {
    use workgraph::config::CoordinatorConfig;

    // Verify that worktree isolation is enabled by default
    let config = CoordinatorConfig::default();
    assert!(
        config.worktree_isolation,
        "Worktree isolation should be enabled by default to prevent cargo lock contention"
    );
}

#[test]
fn test_worktree_creates_separate_target_dirs() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("Failed to create project dir");

    // Initialize the test repo
    init_test_repo(&project_root);

    // Create two worktrees
    let wt1 = create_test_worktree(&project_root, "agent-1");
    let wt2 = create_test_worktree(&project_root, "agent-2");

    // Run a simple cargo check to create target directories
    let output1 = Command::new("cargo")
        .arg("check")
        .current_dir(&wt1)
        .env("CARGO_TARGET_DIR", wt1.join("target"))
        .output()
        .expect("Failed to run cargo check in wt1");
    assert!(
        output1.status.success(),
        "Cargo check should succeed in wt1"
    );

    let output2 = Command::new("cargo")
        .arg("check")
        .current_dir(&wt2)
        .env("CARGO_TARGET_DIR", wt2.join("target"))
        .output()
        .expect("Failed to run cargo check in wt2");
    assert!(
        output2.status.success(),
        "Cargo check should succeed in wt2"
    );

    // Check that target dirs are separate
    assert!(
        wt1.join("target").exists(),
        "Target dir should exist in wt1"
    );
    assert!(
        wt2.join("target").exists(),
        "Target dir should exist in wt2"
    );

    // The target directories should be different
    assert_ne!(
        wt1.join("target").canonicalize().unwrap(),
        wt2.join("target").canonicalize().unwrap(),
        "Target directories should be separate between worktrees"
    );

    println!("✓ Worktree target directory isolation test passed");
}

#[test]
fn test_cleanup_orphaned_worktrees_skips_live_agents() {
    use workgraph::commands::service::worktree::cleanup_orphaned_worktrees;
    use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};

    let temp = TempDir::new().expect("Failed to create temp dir");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("Failed to create project dir");
    init_test_repo(&project);

    let wg_dir = project.join(".workgraph");
    std::fs::create_dir_all(wg_dir.join("service")).expect("Failed to create service dir");

    let worktree_dir = create_test_worktree(&project, "agent-1");

    // Use our own PID so is_live passes.
    let our_pid = std::process::id();
    let now = chrono::Utc::now().to_rfc3339();
    let mut registry = AgentRegistry::default();
    registry.agents.insert(
        "agent-1".to_string(),
        AgentEntry {
            id: "agent-1".to_string(),
            pid: our_pid,
            task_id: "task-1".to_string(),
            executor: "test".to_string(),
            started_at: now.clone(),
            last_heartbeat: now.clone(),
            status: AgentStatus::Working,
            output_file: String::new(),
            model: None,
            completed_at: None,
            worktree_path: None,
        },
    );
    registry.save(&wg_dir).expect("Failed to save registry");

    let cleaned_count = cleanup_orphaned_worktrees(&wg_dir).expect("Cleanup should not fail");
    assert_eq!(cleaned_count, 0);
    assert!(worktree_dir.exists());
}

#[test]
fn test_cleanup_orphaned_worktrees_removes_dead_agents() {
    use workgraph::commands::service::worktree::cleanup_orphaned_worktrees;
    use workgraph::graph::{Node, Status, Task, WorkGraph};
    use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};

    let temp = TempDir::new().expect("Failed to create temp dir");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("Failed to create project dir");
    init_test_repo(&project);

    let wg_dir = project.join(".workgraph");
    std::fs::create_dir_all(wg_dir.join("service")).expect("Failed to create service dir");

    // Force the default branch to "main" so the merge-into-main retention check
    // has a target. (init_test_repo uses default `git init` which may produce
    // master.) Rename master→main if needed.
    Command::new("git")
        .args(["branch", "-m", "master", "main"])
        .current_dir(&project)
        .output()
        .ok();

    let worktree_dir = create_test_worktree(&project, "agent-2");

    let now = chrono::Utc::now().to_rfc3339();
    let mut registry = AgentRegistry::default();
    registry.agents.insert(
        "agent-2".to_string(),
        AgentEntry {
            id: "agent-2".to_string(),
            pid: 999_999_999,
            task_id: "test-task".to_string(),
            executor: "test".to_string(),
            started_at: now.clone(),
            last_heartbeat: now.clone(),
            status: AgentStatus::Dead,
            output_file: String::new(),
            model: None,
            completed_at: None,
            worktree_path: None,
        },
    );
    registry.save(&wg_dir).expect("Failed to save registry");

    // Retention policy (worktree-retention-don): orphan cleanup requires
    // task=Done + eval-pass + branch merged. Set up all three.
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "test-task".to_string(),
        title: "test".to_string(),
        status: Status::Done,
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: ".evaluate-test-task".to_string(),
        title: "eval test-task".to_string(),
        status: Status::Done,
        ..Task::default()
    }));
    workgraph::parser::save_graph(&graph, &wg_dir.join("graph.jsonl"))
        .expect("Failed to write graph");

    // Merge the branch into main so retention is satisfied
    Command::new("git")
        .args(["merge", "--no-ff", "--no-edit", "wg/agent-2/test-task"])
        .current_dir(&project)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .expect("Failed to merge branch");

    let cleaned_count = cleanup_orphaned_worktrees(&wg_dir).expect("Cleanup should not fail");
    assert_eq!(cleaned_count, 1);
    assert!(!worktree_dir.exists());
}

/// New retention policy (worktree-retention-don): orphaned dead agents whose
/// task hasn't reached Done+eval+merged are PRESERVED — their WIP must
/// survive for `wg retry` to resume in-place.
#[test]
fn test_cleanup_orphaned_worktrees_preserves_unfinished_work() {
    use workgraph::commands::service::worktree::cleanup_orphaned_worktrees;
    use workgraph::graph::{Node, Status, Task, WorkGraph};
    use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};

    let temp = TempDir::new().expect("Failed to create temp dir");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("Failed to create project dir");
    init_test_repo(&project);

    let wg_dir = project.join(".workgraph");
    std::fs::create_dir_all(wg_dir.join("service")).expect("Failed to create service dir");

    let worktree_dir = create_test_worktree(&project, "agent-crashed");

    let now = chrono::Utc::now().to_rfc3339();
    let mut registry = AgentRegistry::default();
    registry.agents.insert(
        "agent-crashed".to_string(),
        AgentEntry {
            id: "agent-crashed".to_string(),
            pid: 999_999_998,
            task_id: "in-flight-task".to_string(),
            executor: "test".to_string(),
            started_at: now.clone(),
            last_heartbeat: now.clone(),
            status: AgentStatus::Dead,
            output_file: String::new(),
            model: None,
            completed_at: None,
            worktree_path: None,
        },
    );
    registry.save(&wg_dir).expect("Failed to save registry");

    // Task is Failed (agent crashed mid-work). Under the retention policy,
    // the orphaned worktree must NOT be removed — the next `wg retry` must
    // be able to resume in-place.
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "in-flight-task".to_string(),
        title: "in-flight".to_string(),
        status: Status::Failed,
        ..Task::default()
    }));
    workgraph::parser::save_graph(&graph, &wg_dir.join("graph.jsonl"))
        .expect("Failed to write graph");

    let cleaned_count = cleanup_orphaned_worktrees(&wg_dir).expect("Cleanup should not fail");
    assert_eq!(
        cleaned_count, 0,
        "MUST NOT reap orphan when task hasn't completed — WIP must survive"
    );
    assert!(
        worktree_dir.exists(),
        "Worktree directory must survive for retry-in-place"
    );
}

#[test]
fn test_cleanup_dead_agent_worktree() {
    use workgraph::commands::service::worktree::cleanup_dead_agent_worktree_with_config;

    let temp = TempDir::new().expect("Failed to create temp dir");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("Failed to create project dir");
    init_test_repo(&project);

    let worktree_dir = create_test_worktree(&project, "agent-test");

    // cleanup_dead_agent_worktree_with_config returns () — handles errors internally.
    cleanup_dead_agent_worktree_with_config(
        &project,
        &worktree_dir,
        "wg/agent-test/test-task",
        "agent-test",
        None,
    );

    assert!(!worktree_dir.exists());
}
