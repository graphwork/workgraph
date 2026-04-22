//! Integration test for prompt logging debug functionality
//!
//! Tests that the WG_DEBUG_PROMPTS environment variable correctly
//! enables debug logging of prompt content to /tmp/wg_debug_prompts.log

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (copied from other integration tests)
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

fn wg_cmd_with_env(
    wg_dir: &Path,
    args: &[&str],
    env_key: &str,
    env_value: &str,
) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .env(env_key, env_value)
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

/// Initialize a workgraph in a temp directory and return the .workgraph path.
fn init_wg() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path();
    let wg_dir = project_root.join(".workgraph");

    // Initialize git repository (required for worktree creation)
    Command::new("git")
        .args(["init"])
        .current_dir(project_root)
        .output()
        .expect("Failed to init git repo");

    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(project_root)
        .output()
        .expect("Failed to set git email");

    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(project_root)
        .output()
        .expect("Failed to set git name");

    // Create an initial commit (required for worktree creation)
    std::fs::write(project_root.join("README.md"), "# Test Project")
        .expect("Failed to write README");

    Command::new("git")
        .args(["add", "."])
        .current_dir(project_root)
        .output()
        .expect("Failed to git add");

    Command::new("git")
        .args(["commit", "-m", "initial commit"])
        .current_dir(project_root)
        .output()
        .expect("Failed to git commit");

    wg_ok(&wg_dir, &["init"]);
    (tmp, wg_dir)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_debug_prompt_logging_enabled() {
    let (_tmp, wg_dir) = init_wg();
    let debug_log_path = "/tmp/wg_debug_prompts.log";

    // Clean up any existing debug log
    let _ = fs::remove_file(debug_log_path);

    // Create a test task
    wg_ok(&wg_dir, &["add", "Test debug logging"]);

    // Spawn with debug enabled using WG_DEBUG_PROMPTS=1
    let output = wg_cmd_with_env(
        &wg_dir,
        &["spawn", "test-debug-logging", "--executor", "native"],
        "WG_DEBUG_PROMPTS",
        "1",
    );

    // Check that spawn succeeded
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("Spawn output:\nstdout: {}\nstderr: {}", stdout, stderr);
        // For native executor, we might need to run the spawn differently
        // Let's also try with shell executor instead
        let output2 = wg_cmd_with_env(
            &wg_dir,
            &["spawn", "test-debug-logging", "--executor", "shell"],
            "WG_DEBUG_PROMPTS",
            "1",
        );
        if !output2.status.success() {
            let stdout2 = String::from_utf8_lossy(&output2.stdout);
            let stderr2 = String::from_utf8_lossy(&output2.stderr);
            panic!(
                "Both native and shell executors failed:\nNative - stdout: {}\nstderr: {}\nShell - stdout: {}\nstderr: {}",
                stdout, stderr, stdout2, stderr2
            );
        }
    }

    // Check that the debug log file was created and contains expected content
    assert!(
        Path::new(debug_log_path).exists(),
        "Debug log file should be created when WG_DEBUG_PROMPTS=1"
    );

    let log_content =
        fs::read_to_string(debug_log_path).expect("Should be able to read debug log file");

    // Verify the log contains the expected spawn metadata
    assert!(
        log_content.contains("=== WG DEBUG: Spawning Agent ==="),
        "Debug log should contain spawn metadata header"
    );
    assert!(
        log_content.contains("Task ID: test-debug-logging"),
        "Debug log should contain the task ID"
    );
    assert!(
        log_content.contains("Executor:"),
        "Debug log should contain executor information"
    );

    // Verify the log contains the expected prompt content
    assert!(
        log_content.contains("=== WG DEBUG: Assembled Prompt for Task test-debug-logging ==="),
        "Debug log should contain prompt header for our task"
    );
    assert!(
        log_content.contains("Prompt length:"),
        "Debug log should contain prompt length information"
    );
    assert!(
        log_content.contains("Prompt content:"),
        "Debug log should contain prompt content marker"
    );

    // Clean up
    let _ = fs::remove_file(debug_log_path);
}

#[test]
fn test_debug_prompt_logging_disabled() {
    let (_tmp, wg_dir) = init_wg();
    let debug_log_path = "/tmp/wg_debug_prompts.log";

    // Clean up any existing debug log
    let _ = fs::remove_file(debug_log_path);

    // Create a test task
    wg_ok(&wg_dir, &["add", "Test no debug logging"]);

    // Spawn WITHOUT debug enabled (no WG_DEBUG_PROMPTS env var)
    let output = wg_cmd(
        &wg_dir,
        &["spawn", "test-no-debug-logging", "--executor", "native"],
    );

    // Check that spawn succeeded or try shell executor
    if !output.status.success() {
        let _output2 = wg_cmd(
            &wg_dir,
            &["spawn", "test-no-debug-logging", "--executor", "shell"],
        );
        // We don't care if spawn fails here, we're just testing that debug logging doesn't happen
    }

    // Verify no debug log file was created (or if it exists, it doesn't contain our task)
    if Path::new(debug_log_path).exists() {
        let log_content =
            fs::read_to_string(debug_log_path).expect("Should be able to read debug log file");
        assert!(
            !log_content.contains("Task ID: test-no-debug-logging"),
            "Debug log should not contain our task when WG_DEBUG_PROMPTS is not set"
        );
    }
}
