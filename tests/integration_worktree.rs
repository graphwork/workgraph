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
