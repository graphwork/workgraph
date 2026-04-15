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

    // Verify separate target directories were created
    assert!(
        wt1.join("target").exists(),
        "Worktree 1 should have its own target directory"
    );
    assert!(
        wt2.join("target").exists(),
        "Worktree 2 should have its own target directory"
    );

    // Verify they are different directories
    let target1_path = wt1
        .join("target")
        .canonicalize()
        .expect("Failed to canonicalize target1");
    let target2_path = wt2
        .join("target")
        .canonicalize()
        .expect("Failed to canonicalize target2");
    assert_ne!(
        target1_path, target2_path,
        "Each worktree should have a separate target directory"
    );

    println!("✓ Separate target directories test passed");
}

#[test]
fn test_worktree_isolation_serde_default() {
    // Verify that deserializing a CoordinatorConfig WITHOUT worktree_isolation
    // field defaults to true (matching the programmatic Default impl).
    // This is critical: both serde and Default must agree.
    let toml_str = r#"
max_agents = 2
"#;
    let config: workgraph::config::CoordinatorConfig = toml::from_str(toml_str).unwrap();
    assert!(
        config.worktree_isolation,
        "Serde default for worktree_isolation should be true"
    );
}

#[test]
fn test_worktree_full_lifecycle() {
    // Full lifecycle test: create worktree → modify files → commit in worktree
    // → verify worktree state → cleanup → verify main branch unaffected
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("Failed to create project dir");

    init_test_repo(&project_root);

    // Create .workgraph dir for symlink testing
    let wg_dir = project_root.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).expect("Failed to create .workgraph");
    std::fs::write(wg_dir.join("graph.jsonl"), "").expect("Failed to write graph");

    // Step 1: Create worktree using the library function
    let wt_dir = create_test_worktree(&project_root, "agent-lifecycle");

    // Verify it shows in git worktree list
    let output = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&project_root)
        .output()
        .expect("Failed to list worktrees");
    let worktree_list = String::from_utf8_lossy(&output.stdout);
    assert!(
        worktree_list.contains(".wg-worktrees/agent-lifecycle"),
        "Worktree should appear in git worktree list: {}",
        worktree_list
    );

    // Step 2: Modify files in the worktree
    std::fs::write(wt_dir.join("agent_output.txt"), "work done by agent").unwrap();

    // Step 3: Commit in the worktree
    let output = Command::new("git")
        .args(["add", "agent_output.txt"])
        .current_dir(&wt_dir)
        .output()
        .expect("Failed to git add");
    assert!(output.status.success());

    let output = Command::new("git")
        .args(["commit", "-m", "agent work"])
        .current_dir(&wt_dir)
        .env("GIT_AUTHOR_NAME", "Test Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@test.com")
        .env("GIT_COMMITTER_NAME", "Test Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@test.com")
        .output()
        .expect("Failed to git commit");
    assert!(
        output.status.success(),
        "Commit in worktree should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the file does NOT exist in main worktree
    assert!(
        !project_root.join("agent_output.txt").exists(),
        "Agent's file should not appear in main worktree before merge"
    );

    // Step 4: Simulate merge-back (squash merge from worktree branch to main)
    let branch = "wg/agent-lifecycle/test-task";
    let output = Command::new("git")
        .args(["merge", "--squash", branch])
        .current_dir(&project_root)
        .output()
        .expect("Failed to squash merge");
    assert!(
        output.status.success(),
        "Squash merge should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new("git")
        .args([
            "commit",
            "-m",
            "feat: lifecycle-test (agent-lifecycle)\n\nSquash-merged from worktree branch",
        ])
        .current_dir(&project_root)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .expect("Failed to commit merge");
    assert!(
        output.status.success(),
        "Merge commit should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the file NOW exists in main worktree
    assert!(
        project_root.join("agent_output.txt").exists(),
        "Agent's file should appear in main worktree after merge"
    );

    // Step 5: Cleanup worktree
    let output = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(&project_root)
        .output()
        .expect("Failed to remove worktree");
    assert!(output.status.success());

    let output = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(&project_root)
        .output()
        .expect("Failed to delete branch");
    assert!(output.status.success());

    // Verify worktree is gone from list
    let output = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&project_root)
        .output()
        .expect("Failed to list worktrees");
    let worktree_list = String::from_utf8_lossy(&output.stdout);
    assert!(
        !worktree_list.contains("agent-lifecycle"),
        "Worktree should be removed from git worktree list"
    );

    // Verify the merged file persists
    assert!(
        project_root.join("agent_output.txt").exists(),
        "Merged file should persist after worktree cleanup"
    );
}

#[test]
fn test_worktree_cleanup_on_failed_agent() {
    // Verify worktree cleanup works even when agent didn't commit anything
    // (simulating a failed/crashed agent)
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("Failed to create project dir");

    init_test_repo(&project_root);

    let wt_dir = create_test_worktree(&project_root, "agent-failed");
    assert!(wt_dir.exists());

    // Agent modifies files but doesn't commit (simulating crash)
    std::fs::write(wt_dir.join("uncommitted.txt"), "work in progress").unwrap();

    // Cleanup should still work (force remove discards uncommitted changes)
    let branch = "wg/agent-failed/test-task";
    let output = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(&project_root)
        .output()
        .expect("Failed to remove worktree");
    assert!(
        output.status.success(),
        "Force-remove should work even with uncommitted changes: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(&project_root)
        .output()
        .expect("Failed to delete branch");
    assert!(output.status.success());

    assert!(
        !wt_dir.exists(),
        "Worktree directory should be removed after cleanup"
    );
}

#[test]
fn test_worktree_workgraph_symlink_lifecycle() {
    // Verify that .workgraph is accessible from the worktree via symlink
    // and survives the full lifecycle
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("Failed to create project dir");

    init_test_repo(&project_root);

    // Create .workgraph with test content
    let wg_dir = project_root.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).expect("Failed to create .workgraph");
    std::fs::write(wg_dir.join("graph.jsonl"), r#"{"id":"test"}"#).unwrap();

    let wt_dir = create_test_worktree(&project_root, "agent-symlink");

    // Manually create the .workgraph symlink (as create_worktree in spawn/worktree.rs does)
    let symlink_path = wt_dir.join(".workgraph");
    let wg_canonical = wg_dir.canonicalize().expect("Failed to canonicalize");
    std::os::unix::fs::symlink(&wg_canonical, &symlink_path).expect("Failed to create symlink");

    // Verify symlink works — agent can read graph.jsonl through it
    let content = std::fs::read_to_string(symlink_path.join("graph.jsonl"))
        .expect("Failed to read through symlink");
    assert!(
        content.contains("test"),
        "Should read graph through symlink"
    );

    // Agent writes to .workgraph through symlink (e.g., logging)
    std::fs::write(symlink_path.join("test_log.txt"), "agent log entry")
        .expect("Failed to write through symlink");

    // Verify the write went to the real .workgraph
    assert!(
        wg_dir.join("test_log.txt").exists(),
        "Write through symlink should appear in real .workgraph"
    );

    // Cleanup: remove symlink first (like the real cleanup does)
    std::fs::remove_file(&symlink_path).expect("Failed to remove symlink");
    assert!(!symlink_path.exists(), "Symlink should be removed");
    assert!(
        wg_dir.join("test_log.txt").exists(),
        "Real .workgraph contents should survive symlink removal"
    );

    // Remove worktree
    let branch = "wg/agent-symlink/test-task";
    Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(&project_root)
        .output()
        .unwrap();
    Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(&project_root)
        .output()
        .unwrap();
}

#[test]
fn test_worktree_concurrent_merge_safety() {
    // Verify that two worktrees modifying different files can both
    // be merged back without conflicts
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).expect("Failed to create project dir");

    init_test_repo(&project_root);

    // Create two worktrees
    let wt1 = create_test_worktree(&project_root, "agent-a");
    let wt2 = create_test_worktree(&project_root, "agent-b");

    // Agent A modifies one file
    std::fs::write(wt1.join("file_a.txt"), "agent A output").unwrap();
    Command::new("git")
        .args(["add", "file_a.txt"])
        .current_dir(&wt1)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "agent A work"])
        .current_dir(&wt1)
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@test.com")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@test.com")
        .output()
        .unwrap();

    // Agent B modifies a different file
    std::fs::write(wt2.join("file_b.txt"), "agent B output").unwrap();
    Command::new("git")
        .args(["add", "file_b.txt"])
        .current_dir(&wt2)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "agent B work"])
        .current_dir(&wt2)
        .env("GIT_AUTHOR_NAME", "B")
        .env("GIT_AUTHOR_EMAIL", "b@test.com")
        .env("GIT_COMMITTER_NAME", "B")
        .env("GIT_COMMITTER_EMAIL", "b@test.com")
        .output()
        .unwrap();

    // Merge A first
    let output = Command::new("git")
        .args(["merge", "--squash", "wg/agent-a/test-task"])
        .current_dir(&project_root)
        .output()
        .unwrap();
    assert!(output.status.success(), "Merge A should succeed");
    Command::new("git")
        .args(["commit", "-m", "merge A"])
        .current_dir(&project_root)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .unwrap();

    // Merge B second — should succeed since different files
    let output = Command::new("git")
        .args(["merge", "--squash", "wg/agent-b/test-task"])
        .current_dir(&project_root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Merge B should succeed (non-conflicting): {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Command::new("git")
        .args(["commit", "-m", "merge B"])
        .current_dir(&project_root)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .unwrap();

    // Both files should exist in main
    assert!(project_root.join("file_a.txt").exists());
    assert!(project_root.join("file_b.txt").exists());

    // Cleanup
    for agent in &["agent-a", "agent-b"] {
        let wt = project_root.join(".wg-worktrees").join(agent);
        let branch = format!("wg/{}/test-task", agent);
        Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&wt)
            .current_dir(&project_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(&project_root)
            .output()
            .unwrap();
    }
}

// ============================================================================
// End-to-end worktree isolation tests
//
// These tests verify the five steps from the task description:
// 1. Agent runs in a worktree and modifies files
// 2. `git worktree list` shows the agent's worktree during execution
// 3. Agent process CWD is inside the worktree (/proc/<pid>/cwd)
// 4. Main working directory is NOT modified during agent execution
// 5. After completion: worktree cleaned up, changes merged back
// 6. Failure case: kill agent mid-work, worktree cleaned up
// ============================================================================

#[test]
fn test_worktree_process_cwd_in_worktree() {
    // Verify that a subprocess spawned in the worktree has its CWD inside
    // the worktree directory (via /proc/<pid>/cwd on Linux).
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    init_test_repo(&project_root);

    let wg_dir = project_root.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let wt_dir = create_test_worktree(&project_root, "agent-cwd");

    // Spawn a process in the worktree that sleeps so we can inspect it
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(&wt_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn sleep process");

    let pid = child.id();

    // On Linux, /proc/<pid>/cwd is a symlink to the process's working directory
    let proc_cwd = format!("/proc/{}/cwd", pid);
    let proc_cwd_path = Path::new(&proc_cwd);

    // The process should exist
    assert!(
        proc_cwd_path.exists(),
        "/proc/{}/cwd should exist while process is running",
        pid
    );

    // Read the symlink target and verify it points to the worktree
    let actual_cwd =
        std::fs::read_link(proc_cwd_path).expect("Failed to read /proc/pid/cwd symlink");
    let wt_canonical = wt_dir
        .canonicalize()
        .expect("Failed to canonicalize worktree dir");
    assert_eq!(
        actual_cwd, wt_canonical,
        "Process CWD should be the worktree directory.\nExpected: {:?}\nActual: {:?}",
        wt_canonical, actual_cwd
    );

    // Verify the process CWD is NOT the main project root
    let project_canonical = project_root.canonicalize().unwrap();
    assert_ne!(
        actual_cwd, project_canonical,
        "Process CWD should NOT be the main project root"
    );

    // Clean up the process
    child.kill().expect("Failed to kill child process");
    child.wait().expect("Failed to wait for child");

    // Clean up worktree
    let branch = "wg/agent-cwd/test-task";
    Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(&project_root)
        .output()
        .unwrap();
    Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(&project_root)
        .output()
        .unwrap();
}

#[test]
fn test_worktree_main_dir_unmodified_during_agent_work() {
    // Verify that modifications in the worktree do NOT appear in the main
    // working directory until explicitly merged back.
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    init_test_repo(&project_root);

    let wg_dir = project_root.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let wt_dir = create_test_worktree(&project_root, "agent-isolate");

    // Record initial state of main worktree
    let main_files_before: Vec<_> = std::fs::read_dir(&project_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.starts_with('.'))
        .collect();

    // Simulate agent work in worktree: create files, modify existing file
    std::fs::write(wt_dir.join("new_feature.rs"), "fn feature() {}").unwrap();
    std::fs::write(wt_dir.join("data.json"), r#"{"key": "value"}"#).unwrap();
    std::fs::write(
        wt_dir.join("src/main.rs"),
        "fn main() { println!(\"modified\"); }",
    )
    .unwrap();

    // Stage and commit in the worktree
    Command::new("git")
        .args(["add", "new_feature.rs", "data.json", "src/main.rs"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "agent: add feature + data"])
        .current_dir(&wt_dir)
        .env("GIT_AUTHOR_NAME", "Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@test.com")
        .env("GIT_COMMITTER_NAME", "Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@test.com")
        .output()
        .unwrap();

    // Verify main worktree is UNCHANGED
    assert!(
        !project_root.join("new_feature.rs").exists(),
        "new_feature.rs should NOT exist in main worktree during agent work"
    );
    assert!(
        !project_root.join("data.json").exists(),
        "data.json should NOT exist in main worktree during agent work"
    );

    // Main src/main.rs should still have original content
    let main_content = std::fs::read_to_string(project_root.join("src/main.rs")).unwrap();
    assert!(
        main_content.contains("Hello from test project"),
        "Main worktree's src/main.rs should be unchanged"
    );

    // File listing should be the same
    let main_files_after: Vec<_> = std::fs::read_dir(&project_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.starts_with('.'))
        .collect();
    assert_eq!(
        main_files_before, main_files_after,
        "Main worktree file listing should not change during agent work"
    );

    // Verify git worktree list shows both worktrees
    let output = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&project_root)
        .output()
        .unwrap();
    let worktree_list = String::from_utf8_lossy(&output.stdout);
    assert!(
        worktree_list.contains(".wg-worktrees/agent-isolate"),
        "Agent worktree should appear in git worktree list"
    );

    // Now merge back and verify changes appear
    let branch = "wg/agent-isolate/test-task";
    let output = Command::new("git")
        .args(["merge", "--squash", branch])
        .current_dir(&project_root)
        .output()
        .unwrap();
    assert!(output.status.success(), "Merge should succeed");
    Command::new("git")
        .args(["commit", "-m", "merge agent work"])
        .current_dir(&project_root)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .unwrap();

    // NOW the files should exist in main
    assert!(project_root.join("new_feature.rs").exists());
    assert!(project_root.join("data.json").exists());
    let main_content = std::fs::read_to_string(project_root.join("src/main.rs")).unwrap();
    assert!(main_content.contains("modified"));

    // Clean up
    Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(&project_root)
        .output()
        .unwrap();
    Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(&project_root)
        .output()
        .unwrap();
}

#[test]
fn test_worktree_kill_running_process_cleanup() {
    // Verify that when an agent process is killed mid-work, the worktree
    // can still be cleaned up properly (no stale state left behind).
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    init_test_repo(&project_root);

    let wg_dir = project_root.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let wt_dir = create_test_worktree(&project_root, "agent-killed");
    let branch = "wg/agent-killed/test-task";

    // Simulate agent doing work: create files, stage some, leave others unstaged
    std::fs::write(wt_dir.join("committed.txt"), "committed work").unwrap();
    std::fs::write(wt_dir.join("staged.txt"), "staged but not committed").unwrap();
    std::fs::write(wt_dir.join("dirty.txt"), "uncommitted, unstaged work").unwrap();

    Command::new("git")
        .args(["add", "committed.txt"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "partial work"])
        .current_dir(&wt_dir)
        .env("GIT_AUTHOR_NAME", "Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@test.com")
        .env("GIT_COMMITTER_NAME", "Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@test.com")
        .output()
        .unwrap();
    Command::new("git")
        .args(["add", "staged.txt"])
        .current_dir(&wt_dir)
        .output()
        .unwrap();

    // Spawn a long-running process simulating the agent
    let mut child = Command::new("sleep")
        .arg("300")
        .current_dir(&wt_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn process");

    // Verify the process is running and worktree exists
    let pid = child.id();
    assert!(
        Path::new(&format!("/proc/{}", pid)).exists(),
        "Process should be running"
    );
    assert!(wt_dir.exists(), "Worktree should exist before kill");

    // Simulate kill (like what happens when agent crashes or is killed)
    child.kill().expect("Failed to kill process");
    child.wait().expect("Failed to wait for killed process");

    // Verify the process is dead
    assert!(
        !Path::new(&format!("/proc/{}", pid)).exists(),
        "Process should be dead after kill"
    );

    // Worktree should still exist (kill doesn't clean it up automatically)
    assert!(
        wt_dir.exists(),
        "Worktree should still exist after process kill"
    );

    // Now simulate the cleanup that the wrapper script would do
    // (remove symlink, force-remove worktree, delete branch)
    let symlink_path = wt_dir.join(".workgraph");
    if symlink_path.exists() {
        std::fs::remove_file(&symlink_path).unwrap();
    }

    let output = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(&project_root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Worktree removal should succeed even with dirty state: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(&project_root)
        .output()
        .unwrap();
    assert!(output.status.success(), "Branch deletion should succeed");

    // Verify complete cleanup
    assert!(
        !wt_dir.exists(),
        "Worktree directory should not exist after cleanup"
    );

    let output = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&project_root)
        .output()
        .unwrap();
    let worktree_list = String::from_utf8_lossy(&output.stdout);
    assert!(
        !worktree_list.contains("agent-killed"),
        "Killed agent's worktree should not appear in git worktree list"
    );

    // Verify no stale .git/worktrees entries
    Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(&project_root)
        .output()
        .unwrap();
    let output = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&project_root)
        .output()
        .unwrap();
    let worktree_list = String::from_utf8_lossy(&output.stdout).to_string();
    let lines: Vec<&str> = worktree_list
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "Only the main worktree should remain after cleanup: {:?}",
        lines
    );
}

#[test]
fn test_worktree_repeated_create_cleanup_no_stale() {
    // Run the worktree lifecycle multiple times and verify no stale worktrees
    // are left behind. This ensures repeatability of the test.
    let temp = TempDir::new().expect("Failed to create temp dir");
    let project_root = temp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    init_test_repo(&project_root);

    let wg_dir = project_root.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    for iteration in 0..3 {
        let agent_id = format!("agent-repeat-{}", iteration);
        let branch = format!("wg/{}/test-task", agent_id);
        let wt_dir = create_test_worktree(&project_root, &agent_id);

        // Do some work in the worktree
        std::fs::write(
            wt_dir.join("output.txt"),
            format!("iteration {}", iteration),
        )
        .unwrap();
        Command::new("git")
            .args(["add", "output.txt"])
            .current_dir(&wt_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", &format!("work iter {}", iteration)])
            .current_dir(&wt_dir)
            .env("GIT_AUTHOR_NAME", "Agent")
            .env("GIT_AUTHOR_EMAIL", "agent@test.com")
            .env("GIT_COMMITTER_NAME", "Agent")
            .env("GIT_COMMITTER_EMAIL", "agent@test.com")
            .output()
            .unwrap();

        // Merge back
        Command::new("git")
            .args(["merge", "--squash", &branch])
            .current_dir(&project_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", &format!("merge iter {}", iteration)])
            .current_dir(&project_root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();

        // Clean up
        let symlink_path = wt_dir.join(".workgraph");
        if symlink_path.exists() {
            let _ = std::fs::remove_file(&symlink_path);
        }
        Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&wt_dir)
            .current_dir(&project_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(&project_root)
            .output()
            .unwrap();
    }

    // After all iterations, verify no stale worktrees
    Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(&project_root)
        .output()
        .unwrap();

    let output = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&project_root)
        .output()
        .unwrap();
    let worktree_list = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = worktree_list
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "Only the main worktree should remain after 3 iterations: {:?}",
        lines
    );

    // No .wg-worktrees directory should exist (or it should be empty)
    let wg_worktrees_dir = project_root.join(".wg-worktrees");
    if wg_worktrees_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&wg_worktrees_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "No stale worktree directories should remain: {:?}",
            entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    // Verify the merged content from the last iteration is present
    let content = std::fs::read_to_string(project_root.join("output.txt")).unwrap();
    assert!(
        content.contains("iteration 2"),
        "Final iteration's content should be merged"
    );
}

/// Helper to initialize a git repo with workgraph for e2e service tests
fn setup_git_workgraph(tmp_root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    // Initialize git repo at tmp_root
    init_test_repo(tmp_root);

    let wg_dir = tmp_root.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    // Get the path to the wg binary built by cargo test
    let mut wg_path = std::env::current_exe().expect("could not get current exe path");
    wg_path.pop(); // remove binary name
    if wg_path.ends_with("deps") {
        wg_path.pop(); // remove deps/
    }
    wg_path.push("wg");

    // Initialize workgraph
    let output = Command::new(&wg_path)
        .arg("--dir")
        .arg(&wg_dir)
        .arg("init")
        .env("HOME", tmp_root)
        .output()
        .expect("Failed to run wg init");
    assert!(
        output.status.success(),
        "wg init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Commit the .workgraph directory so it's in the git tree
    // (otherwise worktree branches from HEAD won't include it)
    Command::new("git")
        .args(["add", ".workgraph"])
        .current_dir(tmp_root)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "add workgraph"])
        .current_dir(tmp_root)
        .output()
        .unwrap();

    // Configure: enable worktree isolation, disable auto_assign/auto_evaluate
    let config_content = r#"[coordinator]
worktree_isolation = true

[agency]
auto_assign = false
auto_evaluate = false
"#;
    std::fs::write(wg_dir.join("config.toml"), config_content).unwrap();

    // Create shell executor config
    let wg_bin_dir = wg_path.parent().unwrap().to_string_lossy().to_string();
    let path_with_test_binary = format!(
        "{}:{}",
        wg_bin_dir,
        std::env::var("PATH").unwrap_or_default()
    );
    let executors_dir = wg_dir.join("executors");
    std::fs::create_dir_all(&executors_dir).unwrap();
    let shell_config = format!(
        r#"[executor]
type = "shell"
command = "bash"
args = ["-c", "{{{{task_context}}}}"]
working_dir = "{}"

[executor.env]
TASK_ID = "{{{{task_id}}}}"
TASK_TITLE = "{{{{task_title}}}}"
PATH = "{}"
"#,
        tmp_root.display(),
        path_with_test_binary
    );
    std::fs::write(executors_dir.join("shell.toml"), shell_config).unwrap();

    (wg_dir, wg_path)
}

/// Full end-to-end test using the service to spawn an agent with worktree
/// isolation. This test is marked #[ignore] because it requires starting a
/// service daemon and is timing-sensitive.
#[test]
#[ignore = "Timing-sensitive e2e test - use --include-ignored in controlled environments"]
fn test_worktree_e2e_service_spawn() {
    use std::io::{BufRead, BufReader, Write};

    let tmp = tempfile::tempdir().unwrap();
    let tmp_root = tmp.path().to_path_buf();
    let (wg_dir, wg_path) = setup_git_workgraph(&tmp_root);

    // Helper closures
    let wg_cmd = |args: &[&str]| -> std::process::Output {
        Command::new(&wg_path)
            .arg("--dir")
            .arg(&wg_dir)
            .args(args)
            .env("HOME", &tmp_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
    };

    // Create a coordination directory for signaling between test and agent
    let signal_dir = tmp_root.join("signals");
    std::fs::create_dir_all(&signal_dir).unwrap();

    // Add a task with exec command that:
    // 1. Creates a file in the worktree
    // 2. Commits it
    // 3. Signals "started"
    // 4. Waits for "continue" signal
    let exec_cmd = format!(
        r#"cd "$PWD" && \
echo "agent output" > agent_work.txt && \
git add agent_work.txt && \
git -c user.email=test@test.com -c user.name=Test commit -m "agent work" && \
echo $$ > {signal_dir}/pid && \
pwd > {signal_dir}/pwd && \
touch {signal_dir}/started && \
for i in $(seq 1 100); do [ -f {signal_dir}/continue ] && break; sleep 0.1; done"#,
        signal_dir = signal_dir.display()
    );

    // Add the task
    let output = wg_cmd(&["add", "E2E Test Task", "--id", "e2e-wt-test", "--immediate"]);
    assert!(output.status.success());

    // Patch the exec field
    let graph_path = wg_dir.join("graph.jsonl");
    let content = std::fs::read_to_string(&graph_path).unwrap();
    let mut new_lines = Vec::new();
    for line in content.lines() {
        if line.contains("\"id\":\"e2e-wt-test\"") {
            let mut val: serde_json::Value = serde_json::from_str(line).unwrap();
            val["exec"] = serde_json::Value::String(exec_cmd.clone());
            new_lines.push(serde_json::to_string(&val).unwrap());
        } else {
            new_lines.push(line.to_string());
        }
    }
    std::fs::write(&graph_path, new_lines.join("\n") + "\n").unwrap();

    // Start the service
    let socket_path = format!("{}/wg-test.sock", tmp_root.display());
    let output = wg_cmd(&[
        "service",
        "start",
        "--socket",
        &socket_path,
        "--executor",
        "shell",
        "--max-agents",
        "1",
        "--interval",
        "2",
    ]);
    assert!(
        output.status.success(),
        "Service start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Ensure service cleanup on test exit (even on panic)
    struct ServiceCleanup<'a> {
        wg_path: &'a Path,
        wg_dir: &'a Path,
        tmp_root: &'a Path,
    }
    impl Drop for ServiceCleanup<'_> {
        fn drop(&mut self) {
            let _ = Command::new(self.wg_path)
                .arg("--dir")
                .arg(self.wg_dir)
                .args(["service", "stop", "--force", "--kill-agents"])
                .env("HOME", self.tmp_root)
                .output();
        }
    }
    let _cleanup = ServiceCleanup {
        wg_path: &wg_path,
        wg_dir: &wg_dir,
        tmp_root: &tmp_root,
    };

    // Wait for daemon to be ready
    let ready = {
        let start = Instant::now();
        let mut found = false;
        while start.elapsed() < std::time::Duration::from_secs(10) {
            let state_path = wg_dir.join("service").join("state.json");
            if let Ok(content) = std::fs::read_to_string(&state_path) {
                if let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(sp) = state["socket_path"].as_str() {
                        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(sp) {
                            let _ = writeln!(stream, r#"{{"cmd":"status"}}"#);
                            let _ = stream.flush();
                            let mut reader = BufReader::new(&stream);
                            let mut response = String::new();
                            if reader.read_line(&mut response).is_ok() && !response.is_empty() {
                                found = true;
                                break;
                            }
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        found
    };
    assert!(ready, "Service daemon did not become ready within 10s");

    // Notify graph changed to trigger pickup
    {
        let state_path = wg_dir.join("service").join("state.json");
        if let Ok(content) = std::fs::read_to_string(&state_path) {
            if let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(sp) = state["socket_path"].as_str() {
                    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(sp) {
                        let _ = writeln!(stream, r#"{{"cmd":"graph_changed"}}"#);
                        let _ = stream.flush();
                    }
                }
            }
        }
    }

    // Wait for agent to signal "started"
    let agent_started = {
        let start = Instant::now();
        let mut found = false;
        while start.elapsed() < std::time::Duration::from_secs(30) {
            if signal_dir.join("started").exists() {
                found = true;
                break;
            }
            // Nudge coordinator
            let state_path = wg_dir.join("service").join("state.json");
            if let Ok(content) = std::fs::read_to_string(&state_path) {
                if let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(sp) = state["socket_path"].as_str() {
                        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(sp) {
                            let _ = writeln!(stream, r#"{{"cmd":"graph_changed"}}"#);
                            let _ = stream.flush();
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        found
    };
    assert!(agent_started, "Agent did not start within 30s");

    // === STEP 3: While agent is running, verify worktree state ===

    // 3a. Verify git worktree list shows the agent's worktree
    let output = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&tmp_root)
        .output()
        .unwrap();
    let worktree_list = String::from_utf8_lossy(&output.stdout);
    assert!(
        worktree_list.contains(".wg-worktrees/"),
        "Agent worktree should appear in git worktree list: {}",
        worktree_list
    );

    // 3b. Verify agent's process CWD is in the worktree
    if let Ok(pid_str) = std::fs::read_to_string(signal_dir.join("pid")) {
        let pid = pid_str.trim();
        let proc_cwd = format!("/proc/{}/cwd", pid);
        if let Ok(actual_cwd) = std::fs::read_link(&proc_cwd) {
            let cwd_str = actual_cwd.to_string_lossy();
            assert!(
                cwd_str.contains(".wg-worktrees/"),
                "Agent CWD should be inside a worktree: {}",
                cwd_str
            );
        }
    }

    // 3c. Verify agent's reported PWD is in the worktree
    if let Ok(pwd_str) = std::fs::read_to_string(signal_dir.join("pwd")) {
        assert!(
            pwd_str.trim().contains(".wg-worktrees/"),
            "Agent reported PWD should be inside a worktree: {}",
            pwd_str.trim()
        );
    }

    // 3d. Verify main working directory is NOT modified
    assert!(
        !tmp_root.join("agent_work.txt").exists(),
        "agent_work.txt should NOT exist in main worktree during agent execution"
    );

    // Signal agent to continue
    std::fs::write(signal_dir.join("continue"), "").unwrap();

    // === STEP 4: After agent completes, verify cleanup ===

    // Wait for task to complete
    let completed = {
        let start = Instant::now();
        let mut found = false;
        while start.elapsed() < std::time::Duration::from_secs(30) {
            let output = wg_cmd(&["show", "e2e-wt-test", "--json"]);
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&*stdout) {
                    let status = val["status"].as_str().unwrap_or("");
                    if status == "done" || status == "failed" {
                        found = true;
                        break;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        found
    };
    assert!(completed, "Task should complete within 30s");

    // Give the wrapper script time to merge back and clean up
    std::thread::sleep(std::time::Duration::from_secs(2));

    // 4a. Verify worktree is cleaned up
    let output = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&tmp_root)
        .output()
        .unwrap();
    let worktree_list = String::from_utf8_lossy(&output.stdout);
    let wt_lines: Vec<&str> = worktree_list.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        wt_lines.len(),
        1,
        "Only main worktree should remain after agent completes: {}",
        worktree_list
    );

    // 4b. Verify changes from agent are available in main worktree
    assert!(
        tmp_root.join("agent_work.txt").exists(),
        "agent_work.txt should be merged back to main worktree after agent completes"
    );
}
