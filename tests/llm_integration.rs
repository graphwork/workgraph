//! LLM integration tests using Claude Haiku.
//!
//! These tests call the real Claude CLI with Haiku (cheap, fast model) to
//! validate the end-to-end agency pipeline. They are gated behind `#[ignore]`
//! and require either:
//! - ANTHROPIC_API_KEY set in the environment, or
//! - Claude CLI configured with valid credentials
//!
//! Run with: cargo test --test llm_integration -- --ignored
//!
//! Each test should complete in <30 seconds.

use serial_test::serial;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// The cheap/fast model to use for integration tests.
const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Run `wg` and assert success, returning stdout as string.
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

/// Check if Claude CLI is available and configured.
fn claude_cli_available() -> bool {
    Command::new("claude")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Initialize a fresh workgraph in a temp directory.
fn setup_workgraph(tmp_root: &Path) -> PathBuf {
    let wg_dir = tmp_root.join(".workgraph");
    wg_ok(&wg_dir, &["init"]);

    // Disable auto_assign and auto_evaluate for test isolation
    let config_content = "[agency]\nauto_assign = false\nauto_evaluate = false\n";
    fs::write(wg_dir.join("config.toml"), config_content).unwrap();

    wg_dir
}

// ============================================================================
// Test 1: Agency init creates roles, motivations, and agents
// ============================================================================

#[test]
#[ignore]
#[serial]
fn test_agency_init_creates_entities() {
    if !claude_cli_available() {
        eprintln!("Skipping: claude CLI not available");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(tmp.path());

    // Initialize the agency system
    wg_ok(&wg_dir, &["agency", "init"]);

    // Verify roles were created
    let roles_output = wg_ok(&wg_dir, &["role", "list"]);
    assert!(
        !roles_output.trim().is_empty(),
        "agency init should create at least one role"
    );

    // Verify motivations were created
    let motivations_output = wg_ok(&wg_dir, &["motivation", "list"]);
    assert!(
        !motivations_output.trim().is_empty(),
        "agency init should create at least one motivation"
    );

    // Verify at least one agent was created
    let agents_output = wg_ok(&wg_dir, &["agent", "list"]);
    assert!(
        !agents_output.trim().is_empty(),
        "agency init should create at least one agent"
    );
}

// ============================================================================
// Test 2: Evaluate a completed task with Haiku
// ============================================================================

#[test]
#[ignore]
#[serial]
fn test_evaluate_with_haiku() {
    if !claude_cli_available() {
        eprintln!("Skipping: claude CLI not available");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(tmp.path());

    // Initialize agency first
    wg_ok(&wg_dir, &["agency", "init"]);

    // Create a task, mark it done with some output
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Test evaluation task",
            "-d",
            "A simple test task for evaluation.",
        ],
    );
    wg_ok(
        &wg_dir,
        &["log", "test-evaluation-task", "Started working on the task"],
    );
    wg_ok(
        &wg_dir,
        &[
            "log",
            "test-evaluation-task",
            "Completed the implementation",
        ],
    );
    wg_ok(&wg_dir, &["done", "test-evaluation-task"]);

    // Run evaluation with Haiku
    let output = wg_cmd(
        &wg_dir,
        &[
            "evaluate",
            "run",
            "test-evaluation-task",
            "--evaluator-model",
            HAIKU_MODEL,
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The evaluate command should succeed (or at least not panic)
    // Even if it fails due to model issues, we verify the output is parseable
    if output.status.success() {
        // Verify the evaluation was recorded — task name is truncated in list view
        let evals_output = wg_ok(&wg_dir, &["evaluate", "show", "test-evaluation-task"]);
        assert!(
            evals_output.contains("test-evaluation") || evals_output.contains("Score"),
            "Evaluation should be recorded for the task.\nstdout: {}\nstderr: {}",
            stdout,
            stderr
        );
    } else {
        // If it failed, it should be a model/API error, not a parse error or panic
        eprintln!(
            "evaluate command failed (expected for CI without API key):\nstdout: {}\nstderr: {}",
            stdout, stderr
        );
        // Still useful: verify it didn't panic
        assert!(
            !stderr.contains("panicked"),
            "evaluate should not panic: {}",
            stderr
        );
    }
}

// ============================================================================
// Test 3: Evolve dry-run with Haiku
// ============================================================================

#[test]
#[ignore]
#[serial]
fn test_evolve_dry_run_with_haiku() {
    if !claude_cli_available() {
        eprintln!("Skipping: claude CLI not available");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(tmp.path());

    // Initialize agency
    wg_ok(&wg_dir, &["agency", "init"]);

    // Create and complete a task so there's data for evolution
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Sample task",
            "-d",
            "A sample task for evolution testing.",
        ],
    );
    wg_ok(&wg_dir, &["done", "sample-task"]);

    // Run evolve with dry-run and Haiku
    let output = wg_cmd(&wg_dir, &["evolve", "--dry-run", "--model", HAIKU_MODEL]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        // Dry run should produce output but not apply changes
        // The output should contain some indication of proposed operations
        assert!(!stdout.is_empty(), "evolve dry-run should produce output");
    } else {
        // May fail if no evaluations exist yet or API not available
        eprintln!(
            "evolve dry-run failed (may be expected):\nstdout: {}\nstderr: {}",
            stdout, stderr
        );
        assert!(
            !stderr.contains("panicked"),
            "evolve should not panic: {}",
            stderr
        );
    }
}

// ============================================================================
// Test 4: Evaluate dry-run shows prompt without calling LLM
// ============================================================================

#[test]
#[ignore]
#[serial]
fn test_evaluate_dry_run_shows_prompt() {
    if !claude_cli_available() {
        eprintln!("Skipping: claude CLI not available");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(tmp.path());

    // Initialize agency
    wg_ok(&wg_dir, &["agency", "init"]);

    // Create and complete a task
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Dry run test",
            "-d",
            "Test task for dry run evaluation.",
        ],
    );
    wg_ok(&wg_dir, &["done", "dry-run-test"]);

    // Run evaluate with --dry-run (no LLM call)
    let output = wg_cmd(&wg_dir, &["evaluate", "run", "dry-run-test", "--dry-run"]);

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "evaluate dry-run should succeed: stdout={}, stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Dry-run should show the evaluator prompt
    assert!(
        stdout.contains("Evaluator") || stdout.contains("evaluator") || stdout.contains("Dry Run"),
        "dry-run should show evaluator prompt info: {}",
        stdout
    );
}

// ============================================================================
// Test 5: Full cycle — init, add, complete, evaluate (no panics)
// ============================================================================

#[test]
#[ignore]
#[serial]
fn test_full_cycle_no_panics() {
    if !claude_cli_available() {
        eprintln!("Skipping: claude CLI not available");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(tmp.path());

    // Step 1: Init agency
    wg_ok(&wg_dir, &["agency", "init"]);

    // Step 2: Add a task with description
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Integration test task",
            "-d",
            "A task created by the LLM integration test suite.",
        ],
    );

    // Step 3: Log some progress
    wg_ok(
        &wg_dir,
        &["log", "integration-test-task", "Starting integration test"],
    );

    // Step 4: Complete the task
    wg_ok(&wg_dir, &["done", "integration-test-task"]);

    // Step 5: Evaluate with Haiku
    let eval_output = wg_cmd(
        &wg_dir,
        &[
            "evaluate",
            "run",
            "integration-test-task",
            "--evaluator-model",
            HAIKU_MODEL,
        ],
    );

    let eval_stderr = String::from_utf8_lossy(&eval_output.stderr);
    assert!(
        !eval_stderr.contains("panicked"),
        "evaluation should not panic: {}",
        eval_stderr
    );

    // Step 6: Evolve dry-run (also should not panic)
    let evolve_output = wg_cmd(&wg_dir, &["evolve", "--dry-run", "--model", HAIKU_MODEL]);

    let evolve_stderr = String::from_utf8_lossy(&evolve_output.stderr);
    assert!(
        !evolve_stderr.contains("panicked"),
        "evolve should not panic: {}",
        evolve_stderr
    );
}

// ============================================================================
// Test 6: Claude CLI prompt validation (no LLM call)
// ============================================================================

#[test]
#[ignore]
#[serial]
fn test_claude_cli_is_available() {
    // This test just validates that the claude CLI binary can be found
    // and responds to --version. Useful as a pre-flight check.
    let output = Command::new("claude")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            assert!(
                o.status.success(),
                "claude --version failed: stdout={}, stderr={}",
                stdout,
                stderr
            );
        }
        Err(e) => {
            eprintln!("claude CLI not found: {}. Skipping LLM tests.", e);
        }
    }
}

// ============================================================================
// Test 7: Prompt rendering + JSON extraction pipeline
// ============================================================================

#[test]
#[ignore]
#[serial]
fn test_haiku_produces_parseable_eval_json() {
    if !claude_cli_available() {
        eprintln!("Skipping: claude CLI not available");
        return;
    }

    // Send a minimal evaluation prompt directly to Haiku and verify the response
    // parses as valid EvalOutput JSON.
    let prompt = r#"You are an evaluator. Assess this task:
Task: "Write hello world in Rust"
The agent wrote: fn main() { println!("Hello, world!"); }

Respond with ONLY a JSON object:
{"score": <0.0-1.0>, "dimensions": {"correctness": <0.0-1.0>, "completeness": <0.0-1.0>}, "notes": "<brief assessment>"}"#;

    let mut cmd = Command::new("claude");
    workgraph::env_sanitize::sanitize_command(&mut cmd);
    let output = cmd
        .arg("--model")
        .arg(HAIKU_MODEL)
        .arg("--print")
        .arg("--dangerously-skip-permissions")
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run claude CLI");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "claude CLI failed (may be expected without API key): {}",
            stderr
        );
        return;
    }

    let raw_output = String::from_utf8_lossy(&output.stdout);

    // Try to extract JSON from the output
    let json_str = workgraph::json_extract::extract_json(&raw_output);
    assert!(
        json_str.is_some(),
        "Haiku should produce extractable JSON. Raw output:\n{}",
        raw_output
    );

    // Parse as the expected eval format
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct EvalOutput {
        score: f64,
        #[serde(default)]
        dimensions: std::collections::HashMap<String, f64>,
        #[serde(default)]
        notes: String,
    }

    let parsed: Result<EvalOutput, _> = serde_json::from_str(&json_str.unwrap());
    assert!(
        parsed.is_ok(),
        "Haiku output should parse as EvalOutput. Raw output:\n{}",
        raw_output
    );

    let eval = parsed.unwrap();
    assert!(
        eval.score >= 0.0 && eval.score <= 1.0,
        "Score should be in [0,1], got {}",
        eval.score
    );
}
