//! Integration tests for `wg endpoints` CLI commands.
//!
//! Exercises the full add/list/remove/set-default lifecycle through the CLI
//! binary, verifying output format, error messages, and config persistence.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;
use workgraph::graph::WorkGraph;
use workgraph::parser::save_graph;

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
    // Use a fake HOME derived from the wg_dir path so that the user's real
    // ~/.workgraph/config.toml does not bleed into the test (the fake home
    // has no .workgraph/ subdir, so global config is empty).
    let fake_home = wg_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(wg_dir);
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .env("HOME", fake_home)
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

fn wg_fail(wg_dir: &Path, args: &[&str]) -> (String, String) {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "wg {:?} should have failed but succeeded.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    (stdout, stderr)
}

fn setup_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

// ===========================================================================
// 1. wg endpoints add — creates valid config entry
// ===========================================================================

#[test]
fn cli_endpoints_add_creates_config_entry() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "test-ep",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-or-test-123",
            "--model",
            "anthropic/claude-sonnet-4-20250514",
        ],
    );
    assert!(output.contains("Added endpoint 'test-ep'"));
    assert!(output.contains("openrouter"));

    // Verify the config file was written
    let config_path = wg_dir.join("config.toml");
    let config_text = fs::read_to_string(&config_path).unwrap();
    assert!(
        config_text.contains("test-ep"),
        "Config should contain endpoint name, got: {}",
        config_text
    );
    assert!(config_text.contains("openrouter"));
}

#[test]
fn cli_endpoints_add_first_becomes_default() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "first-ep",
            "--provider",
            "openai",
            "--api-key",
            "sk-test",
        ],
    );

    // Verify via JSON output
    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["is_default"], true);
}

#[test]
fn cli_endpoints_add_with_key_file() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let key_file = tmp.path().join("api.key");
    {
        let mut f = fs::File::create(&key_file).unwrap();
        writeln!(f, "sk-or-from-file-test").unwrap();
    }

    let output = wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "file-ep",
            "--provider",
            "openrouter",
            "--api-key-file",
            &key_file.to_string_lossy(),
        ],
    );
    assert!(output.contains("Added endpoint 'file-ep'"));

    // List should show "(from file)" for the key
    let list = wg_ok(&wg_dir, &["endpoints", "list"]);
    assert!(
        list.contains("(from file)"),
        "Expected '(from file)' in list output, got: {}",
        list
    );
}

#[test]
fn cli_endpoints_add_defaults_provider_to_anthropic() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(
        &wg_dir,
        &["endpoints", "add", "bare-ep", "--api-key", "sk-test"],
    );
    assert!(output.contains("anthropic"));

    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    assert_eq!(parsed[0]["provider"], "anthropic");
}

// ===========================================================================
// 2. wg endpoints list — output format
// ===========================================================================

#[test]
fn cli_endpoints_list_empty() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(&wg_dir, &["endpoints", "list"]);
    assert!(
        output.contains("No endpoints configured"),
        "Expected empty message, got: {}",
        output
    );
}

#[test]
fn cli_endpoints_list_json_empty() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed, serde_json::json!([]));
}

#[test]
fn cli_endpoints_list_shows_all_fields() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "full-ep",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-or-test-key-abcdef",
            "--model",
            "anthropic/claude-sonnet-4-20250514",
            "--url",
            "https://openrouter.ai/api/v1",
        ],
    );

    let list = wg_ok(&wg_dir, &["endpoints", "list"]);
    assert!(list.contains("full-ep"));
    assert!(list.contains("openrouter"));
    assert!(list.contains("(default)"));
    assert!(list.contains("anthropic/claude-sonnet-4-20250514"));

    // JSON format includes structured fields
    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let ep = &parsed[0];
    assert_eq!(ep["name"], "full-ep");
    assert_eq!(ep["provider"], "openrouter");
    assert_eq!(ep["model"], "anthropic/claude-sonnet-4-20250514");
    assert_eq!(ep["is_default"], true);
    // API key should be masked in output
    let key_str = ep["api_key"].as_str().unwrap();
    assert!(
        key_str.contains("...") || key_str.contains("***") || key_str.len() < 20,
        "API key should be masked in list output, got: {}",
        key_str
    );
}

#[test]
fn cli_endpoints_list_multiple() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "ep-a",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-a",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "ep-b",
            "--provider",
            "openai",
            "--api-key",
            "sk-b",
        ],
    );

    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    let names: Vec<&str> = arr.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"ep-a"));
    assert!(names.contains(&"ep-b"));
}

// ===========================================================================
// 3. wg endpoints remove — cleans up, warns on default
// ===========================================================================

#[test]
fn cli_endpoints_remove_cleans_up() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "rm-ep",
            "--provider",
            "openai",
            "--api-key",
            "sk-rm",
        ],
    );

    let output = wg_ok(&wg_dir, &["endpoints", "remove", "rm-ep"]);
    assert!(output.contains("Removed endpoint 'rm-ep'"));

    let list = wg_ok(&wg_dir, &["endpoints", "list"]);
    assert!(list.contains("No endpoints configured"));
}

#[test]
fn cli_endpoints_remove_nonexistent_errors() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let (stdout, stderr) = wg_fail(&wg_dir, &["endpoints", "remove", "ghost-ep"]);
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("not found"),
        "Expected 'not found' error, got: {}",
        combined
    );
}

#[test]
fn cli_endpoints_remove_default_promotes_next() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Add two endpoints; first becomes default
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "primary",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-p",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "secondary",
            "--provider",
            "openai",
            "--api-key",
            "sk-s",
        ],
    );

    // Remove the default
    wg_ok(&wg_dir, &["endpoints", "remove", "primary"]);

    // Secondary should now be default
    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "secondary");
    assert_eq!(arr[0]["is_default"], true);
}

// ===========================================================================
// 4. wg endpoints set-default — updates config
// ===========================================================================

#[test]
fn cli_endpoints_set_default_switches() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "alpha",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-a",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "beta",
            "--provider",
            "openai",
            "--api-key",
            "sk-b",
        ],
    );

    let output = wg_ok(&wg_dir, &["endpoints", "set-default", "beta"]);
    assert!(output.contains("Set 'beta' as default"));

    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let arr = parsed.as_array().unwrap();
    let alpha = arr.iter().find(|v| v["name"] == "alpha").unwrap();
    let beta = arr.iter().find(|v| v["name"] == "beta").unwrap();
    assert_eq!(alpha["is_default"], false);
    assert_eq!(beta["is_default"], true);
}

#[test]
fn cli_endpoints_set_default_nonexistent_errors() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let (stdout, stderr) = wg_fail(&wg_dir, &["endpoints", "set-default", "nope"]);
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("not found"),
        "Expected 'not found' error, got: {}",
        combined
    );
}

// ===========================================================================
// 5. Duplicate endpoint name → error
// ===========================================================================

#[test]
fn cli_endpoints_add_duplicate_errors() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "dup-ep",
            "--provider",
            "openai",
            "--api-key",
            "sk-1",
        ],
    );

    let (stdout, stderr) = wg_fail(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "dup-ep",
            "--provider",
            "openai",
            "--api-key",
            "sk-2",
        ],
    );
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("already exists"),
        "Expected 'already exists' error, got: {}",
        combined
    );
}

// ===========================================================================
// 6. Full CRUD lifecycle
// ===========================================================================

#[test]
fn cli_endpoints_full_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Add two endpoints
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "ep-one",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-or-1",
            "--model",
            "anthropic/claude-sonnet-4-20250514",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "ep-two",
            "--provider",
            "openai",
            "--api-key",
            "sk-oai-2",
            "--model",
            "gpt-4o",
        ],
    );

    // List
    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 2);

    // Switch default
    wg_ok(&wg_dir, &["endpoints", "set-default", "ep-two"]);

    // Remove first
    wg_ok(&wg_dir, &["endpoints", "remove", "ep-one"]);

    // Verify only ep-two remains and is default
    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "ep-two");
    assert_eq!(arr[0]["is_default"], true);

    // Remove last
    wg_ok(&wg_dir, &["endpoints", "remove", "ep-two"]);
    let list = wg_ok(&wg_dir, &["endpoints", "list"]);
    assert!(list.contains("No endpoints configured"));
}

// ===========================================================================
// 7. wg endpoints update — patches existing endpoint in place
// ===========================================================================

#[test]
fn cli_endpoints_update_patches_api_key_file() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints", "add", "upd-ep",
            "--provider", "openai",
            "--api-key", "sk-old",
            "--model", "gpt-4o",
        ],
    );

    let key_file = tmp.path().join("newkey.txt");
    fs::write(&key_file, "sk-new-from-file\n").unwrap();

    let output = wg_ok(
        &wg_dir,
        &[
            "endpoints", "update", "upd-ep",
            "--api-key-file", &key_file.to_string_lossy(),
        ],
    );
    assert!(output.contains("Updated endpoint 'upd-ep'"));
    assert!(output.contains("api_key_file"));

    // Verify provider and model unchanged, key source changed
    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let ep = &parsed[0];
    assert_eq!(ep["provider"], "openai");
    assert_eq!(ep["model"], "gpt-4o");
    let key_source = ep["key_source"].as_str().unwrap();
    assert!(
        key_source.starts_with("file"),
        "Expected key_source to start with 'file', got: {}",
        key_source
    );
}

#[test]
fn cli_endpoints_update_patches_provider() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints", "add", "upd-ep2",
            "--provider", "openai",
            "--api-key", "sk-test",
        ],
    );

    let output = wg_ok(
        &wg_dir,
        &["endpoints", "update", "upd-ep2", "--provider", "anthropic"],
    );
    assert!(output.contains("Updated endpoint 'upd-ep2'"));
    assert!(output.contains("provider"));

    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    assert_eq!(parsed[0]["provider"], "anthropic");
}

#[test]
fn cli_endpoints_update_nonexistent_errors() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let (stdout, stderr) = wg_fail(
        &wg_dir,
        &["endpoints", "update", "ghost-ep", "--provider", "openai"],
    );
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("not found"),
        "Expected 'not found' error, got: {}",
        combined
    );
}

#[test]
fn cli_endpoints_update_no_fields_errors() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "endpoints", "add", "upd-ep3",
            "--provider", "openai",
            "--api-key", "sk-test",
        ],
    );

    let (stdout, stderr) = wg_fail(&wg_dir, &["endpoints", "update", "upd-ep3"]);
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("No fields specified"),
        "Expected 'No fields specified' error, got: {}",
        combined
    );
}
