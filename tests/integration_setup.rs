//! Integration tests for the `wg setup` wizard.
//!
//! Tests cover:
//! 1. Fresh setup produces valid config.toml via CLI
//! 2. Re-running setup preserves existing values (idempotency)
//! 3. Configured values propagate to spawned agent env vars
//! 4. Non-interactive CLI mode end-to-end
//!
//! All tests use temporary directories as fake HOME so the real user's
//! `~/.workgraph/config.toml` is never read or modified.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use workgraph::config::Config;

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

/// Run `wg setup` with a fake HOME and custom .workgraph dir, capturing output.
fn wg_setup_cmd(fake_home: &Path, wg_dir: &Path, extra_args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(wg_binary());
    cmd.arg("--dir").arg(wg_dir);
    cmd.arg("setup");
    cmd.args(extra_args);
    cmd.env("HOME", fake_home);
    // Ensure no real API keys leak into tests
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("OPENROUTER_API_KEY");
    cmd.env_remove("OPENAI_API_KEY");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output()
        .unwrap_or_else(|e| panic!("Failed to run wg setup: {}", e))
}

/// Set up a temporary directory structure with a fake HOME and an initialized
/// .workgraph graph. Returns (fake_home_path, wg_dir_path).
fn setup_env(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let fake_home = tmp.path().join("fakehome");
    let project = tmp.path().join("project");
    let wg_dir = project.join(".workgraph");
    fs::create_dir_all(&fake_home).unwrap();
    fs::create_dir_all(&wg_dir).unwrap();

    // Create minimal graph.jsonl
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

    (fake_home, wg_dir)
}

/// Write a global config at fake_home/.workgraph/config.toml.
fn write_global_config(fake_home: &Path, content: &str) {
    let global_dir = fake_home.join(".workgraph");
    fs::create_dir_all(&global_dir).unwrap();
    fs::write(global_dir.join("config.toml"), content).unwrap();
}

/// Read and parse global config from fake_home/.workgraph/config.toml.
fn load_global_config(fake_home: &Path) -> Config {
    let path = fake_home.join(".workgraph/config.toml");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read config at {:?}: {}", path, e));
    toml::from_str(&content).unwrap_or_else(|e| {
        panic!("Failed to parse config.toml:\n{}\nError: {}", content, e)
    })
}

// ===========================================================================
// 1. Fresh setup produces valid config.toml
// ===========================================================================

#[test]
fn fresh_setup_creates_valid_config() {
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &["--provider", "anthropic", "--model", "sonnet", "--skip-validation"],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wg setup should succeed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr,
    );

    // Config file should exist
    let config_path = fake_home.join(".workgraph/config.toml");
    assert!(
        config_path.exists(),
        "config.toml should be created at {:?}",
        config_path
    );

    // Should be valid TOML that deserializes to Config
    let config = load_global_config(&fake_home);
    assert_eq!(config.coordinator.executor, Some("claude".to_string()));
    assert!(
        config.agent.model.contains("sonnet"),
        "model should contain 'sonnet', got: {}",
        config.agent.model
    );
}

#[test]
fn fresh_setup_default_anthropic() {
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    // Minimal flags — just provider and skip-validation
    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &["--provider", "anthropic", "--skip-validation"],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "wg setup --provider anthropic --skip-validation should succeed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Should print summary info
    assert!(
        stdout.contains("Provider") || stdout.contains("Configuration") || stdout.contains("Summary"),
        "stdout should contain summary output, got: {}",
        stdout
    );

    // Config file should be valid and have sane defaults
    let config = load_global_config(&fake_home);
    assert_eq!(
        config.coordinator.executor,
        Some("claude".to_string()),
        "Anthropic provider should default to claude executor"
    );
    // Default model for anthropic is "sonnet"
    assert!(
        config.agent.model.contains("sonnet"),
        "default model should be sonnet, got: {}",
        config.agent.model
    );
}

#[test]
fn fresh_setup_config_roundtrips_through_toml() {
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &["--provider", "anthropic", "--model", "opus", "--skip-validation"],
    );
    assert!(output.status.success());

    let config = load_global_config(&fake_home);

    // Re-serialize and re-parse to verify round-trip integrity
    let toml_str = toml::to_string_pretty(&config).unwrap();
    let reloaded: Config =
        toml::from_str(&toml_str).expect("Re-serialized config should be valid TOML");

    assert_eq!(reloaded.coordinator.executor, config.coordinator.executor);
    assert_eq!(reloaded.agent.model, config.agent.model);
    assert_eq!(reloaded.coordinator.max_agents, config.coordinator.max_agents);
}

// ===========================================================================
// 2. Re-running setup preserves existing values (idempotency)
// ===========================================================================

#[test]
fn rerun_setup_preserves_project_name() {
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    // Pre-populate global config with custom fields
    write_global_config(
        &fake_home,
        r#"
[project]
name = "preserve-me"

[coordinator]
executor = "claude"
max_agents = 2

[agent]
executor = "claude"
model = "claude:haiku"
"#,
    );

    // Run setup to change model
    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &["--provider", "anthropic", "--model", "opus", "--skip-validation"],
    );
    assert!(
        output.status.success(),
        "second setup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = load_global_config(&fake_home);

    // Updated by wizard
    assert!(
        config.agent.model.contains("opus"),
        "model should be updated to opus, got: {}",
        config.agent.model
    );

    // project.name should survive
    assert_eq!(
        config.project.name,
        Some("preserve-me".to_string()),
        "project.name should be preserved across setup runs"
    );
}

#[test]
fn rerun_setup_preserves_log_rotation() {
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    write_global_config(
        &fake_home,
        r#"
[coordinator]
executor = "claude"
max_agents = 4

[agent]
executor = "claude"
model = "claude:sonnet"

[log]
rotation_threshold = 9999999
"#,
    );

    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &["--provider", "anthropic", "--model", "haiku", "--skip-validation"],
    );
    assert!(output.status.success());

    let config = load_global_config(&fake_home);

    // Model should be updated
    assert!(
        config.agent.model.contains("haiku"),
        "model should be haiku, got: {}",
        config.agent.model
    );

    // Log rotation should survive
    assert_eq!(
        config.log.rotation_threshold, 9999999,
        "log.rotation_threshold should be preserved"
    );
}

#[test]
fn setup_twice_produces_valid_config_each_time() {
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    // First run
    let out1 = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &["--provider", "anthropic", "--model", "sonnet", "--skip-validation"],
    );
    assert!(
        out1.status.success(),
        "first setup should succeed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );

    let config1 = load_global_config(&fake_home);
    assert!(config1.agent.model.contains("sonnet"));

    // Second run — change model
    let out2 = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &["--provider", "anthropic", "--model", "opus", "--skip-validation"],
    );
    assert!(
        out2.status.success(),
        "second setup should succeed: {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let config2 = load_global_config(&fake_home);
    assert!(
        config2.agent.model.contains("opus"),
        "model should be updated to opus, got: {}",
        config2.agent.model
    );

    // Both configs should be valid TOML
    let toml_str = toml::to_string_pretty(&config2).unwrap();
    let _: Config = toml::from_str(&toml_str).expect("Re-serialized config should be valid");
}

// ===========================================================================
// 3. Configured values appear in spawned agent environment
// ===========================================================================

#[test]
fn anthropic_setup_produces_expected_spawn_values() {
    // When setup runs with --provider anthropic, the config should contain
    // the values that spawn will read to set WG_EXECUTOR_TYPE and WG_MODEL.
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &["--provider", "anthropic", "--model", "opus", "--skip-validation"],
    );
    assert!(output.status.success());

    let config = load_global_config(&fake_home);

    // spawn/execution.rs sets WG_EXECUTOR_TYPE from config.agent.executor
    assert_eq!(
        config.agent.executor, "claude",
        "Anthropic setup should set executor to 'claude'"
    );

    // spawn/execution.rs sets WG_MODEL from config.coordinator.model or config.agent.model
    let model = config
        .coordinator
        .model
        .as_deref()
        .unwrap_or(&config.agent.model);
    assert!(
        model.contains("opus"),
        "WG_MODEL source should contain 'opus', got: {}",
        model
    );

    // No endpoints for Anthropic (uses built-in claude CLI)
    assert!(
        config.llm_endpoints.endpoints.is_empty(),
        "Anthropic setup should not create endpoints"
    );
}

#[test]
fn openrouter_setup_produces_expected_spawn_values() {
    // OpenRouter setup: native executor, endpoint with URL and API key env.
    // We can't actually call OpenRouter API, so use --skip-validation.
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &[
            "--provider", "openrouter",
            "--model", "anthropic/claude-sonnet-4",
            "--skip-validation",
        ],
    );
    assert!(
        output.status.success(),
        "openrouter setup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = load_global_config(&fake_home);

    // WG_EXECUTOR_TYPE
    assert_eq!(
        config.agent.executor, "native",
        "OpenRouter should use native executor"
    );

    // WG_MODEL — should have the model identifier
    let model = config
        .coordinator
        .model
        .as_deref()
        .unwrap_or(&config.agent.model);
    assert!(
        model.contains("claude-sonnet-4") || model.contains("sonnet"),
        "model should reference the selected model, got: {}",
        model
    );
}

#[test]
fn local_setup_produces_expected_spawn_values() {
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &[
            "--provider", "local",
            "--model", "llama3",
            "--url", "http://localhost:11434/v1",
            "--skip-validation",
        ],
    );
    assert!(
        output.status.success(),
        "local setup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = load_global_config(&fake_home);

    // WG_EXECUTOR_TYPE
    assert_eq!(
        config.agent.executor, "native",
        "Local provider should use native executor"
    );

    // WG_MODEL
    assert!(
        config.agent.model.contains("llama3"),
        "model should contain llama3, got: {}",
        config.agent.model
    );
}

// ===========================================================================
// 4. Edge cases
// ===========================================================================

#[test]
fn setup_with_custom_model_id() {
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    let output = wg_setup_cmd(
        &fake_home,
        &wg_dir,
        &[
            "--provider", "openai",
            "--model", "gpt-4o",
            "--skip-validation",
        ],
    );
    assert!(
        output.status.success(),
        "openai setup should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = load_global_config(&fake_home);
    assert!(
        config.agent.model.contains("gpt-4o"),
        "model should contain gpt-4o, got: {}",
        config.agent.model
    );
    assert_eq!(config.agent.executor, "native");
}

#[test]
fn setup_no_provider_in_non_tty_defaults_to_anthropic() {
    // Without --provider and without a TTY, setup falls through to
    // non-interactive mode which defaults to anthropic provider.
    let tmp = TempDir::new().unwrap();
    let (fake_home, wg_dir) = setup_env(&tmp);

    let output = wg_setup_cmd(&fake_home, &wg_dir, &["--skip-validation"]);
    assert!(
        output.status.success(),
        "setup without --provider should default to anthropic: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = load_global_config(&fake_home);
    assert_eq!(
        config.coordinator.executor,
        Some("claude".to_string()),
        "should default to claude executor (anthropic provider)"
    );
}
