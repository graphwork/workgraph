//! Integration tests for `wg init` executor selection requirement.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

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

fn wg_cmd_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

// ---------------------------------------------------------------------------
// test_init_requires_executor
// ---------------------------------------------------------------------------

/// `wg init` with no --executor flag must fail with a message containing
/// "executor" and at least one example invocation.
#[test]
fn test_init_requires_executor() {
    let tmp = TempDir::new().unwrap();

    let output = wg_cmd_in(tmp.path(), &["init"]);

    assert!(
        !output.status.success(),
        "wg init should fail when --executor is omitted"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stderr, stdout);

    assert!(
        combined.contains("executor"),
        "error must mention 'executor'. Got:\n{}",
        combined
    );

    // Should show at least one example invocation.
    assert!(
        combined.contains("--executor claude") || combined.contains("--executor"),
        "error must show an example command. Got:\n{}",
        combined
    );
}

// ---------------------------------------------------------------------------
// test_init_with_executor_claude_succeeds
// ---------------------------------------------------------------------------

/// `wg init --executor claude` must succeed and write coordinator.executor = "claude"
/// to config.toml.
#[test]
fn test_init_with_executor_claude_succeeds() {
    let tmp = TempDir::new().unwrap();

    let output = wg_cmd_in(tmp.path(), &["init", "--executor", "claude"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wg init --executor claude should succeed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    let wg_dir = tmp.path().join(".wg");
    assert!(wg_dir.exists(), ".wg directory should be created");

    let config =
        workgraph::config::Config::load(&wg_dir).expect("config.toml should be loadable");
    assert_eq!(
        config.coordinator.executor.as_deref(),
        Some("claude"),
        "coordinator.executor should be set to 'claude' in config.toml"
    );
}

// ---------------------------------------------------------------------------
// test_init_endpoint_only_still_requires_executor
// ---------------------------------------------------------------------------

/// `wg init -e https://example.com` (endpoint only, no executor) must still
/// fail with the missing-executor error.
#[test]
fn test_init_endpoint_only_still_requires_executor() {
    let tmp = TempDir::new().unwrap();

    let output = wg_cmd_in(tmp.path(), &["init", "-e", "https://example.com"]);

    assert!(
        !output.status.success(),
        "wg init with only -e (no --executor) should fail"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stderr, stdout);

    assert!(
        combined.contains("executor"),
        "error must mention 'executor'. Got:\n{}",
        combined
    );
}

// ---------------------------------------------------------------------------
// test_init_executor_and_endpoint_succeeds
// ---------------------------------------------------------------------------

/// `wg init --executor claude -e https://example.com` must succeed and
/// store both executor and endpoint in config.toml.
#[test]
fn test_init_executor_and_endpoint_succeeds() {
    let tmp = TempDir::new().unwrap();

    let output = wg_cmd_in(
        tmp.path(),
        &[
            "init",
            "--executor",
            "shell",
            "-e",
            "http://127.0.0.1:9999",
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wg init --executor shell -e http://... should succeed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    let wg_dir = tmp.path().join(".wg");
    let config =
        workgraph::config::Config::load(&wg_dir).expect("config.toml should be loadable");

    assert_eq!(
        config.coordinator.executor.as_deref(),
        Some("shell"),
        "coordinator.executor should be 'shell'"
    );

    let default_ep = config
        .llm_endpoints
        .endpoints
        .iter()
        .find(|e| e.is_default)
        .expect("a default endpoint should be written");
    assert_eq!(
        default_ep.url.as_deref(),
        Some("http://127.0.0.1:9999"),
        "endpoint URL should be persisted"
    );
}
