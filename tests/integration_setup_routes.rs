//! Integration tests for the 5-route `wg setup` / `wg init` flow and the
//! `wg config reset` command. Validation criteria from
//! `wg-setup-5-smooth-2`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::config::Config;
use workgraph::config_defaults::{config_for_route, RouteParams, SetupRoute};

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

fn run_wg_in_isolation(fake_home: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(wg_binary());
    cmd.args(args);
    cmd.env("HOME", fake_home);
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("OPENROUTER_API_KEY");
    cmd.env_remove("OPENAI_API_KEY");
    cmd.env_remove("WG_DIR");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output()
        .unwrap_or_else(|e| panic!("Failed to run wg: {}", e))
}

fn load_global_config(fake_home: &Path) -> Config {
    let path = fake_home.join(".workgraph/config.toml");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read config at {:?}: {}", path, e));
    toml::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse config.toml:\n{}\nError: {}", content, e))
}

// ---------------------------------------------------------------------------
// Per-route config completeness — pure-Rust tests of config_for_route.
// (Same names as the validation checklist — also covered in lib unit tests.)
// ---------------------------------------------------------------------------

#[test]
fn test_route_openrouter_complete_config() {
    let cfg = config_for_route(SetupRoute::Openrouter, RouteParams::default());
    assert_eq!(cfg.coordinator.executor.as_deref(), Some("native"));
    assert_eq!(cfg.agent.executor, "native");
    assert!(cfg.tiers.fast.is_some());
    assert!(cfg.tiers.standard.is_some());
    assert!(cfg.tiers.premium.is_some());
    assert_eq!(cfg.llm_endpoints.endpoints.len(), 1);
    assert_eq!(cfg.llm_endpoints.endpoints[0].provider, "openrouter");
    // Round-trip
    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    let _: Config = toml::from_str(&toml_str).unwrap();
}

#[test]
fn test_route_claude_cli_complete_config() {
    let cfg = config_for_route(SetupRoute::ClaudeCli, RouteParams::default());
    assert_eq!(cfg.coordinator.executor.as_deref(), Some("claude"));
    assert_eq!(cfg.agent.executor, "claude");
    assert!(cfg.tiers.fast.is_some());
    assert!(cfg.tiers.standard.is_some());
    assert!(cfg.tiers.premium.is_some());
    // Claude CLI doesn't need an endpoint.
    assert!(cfg.llm_endpoints.endpoints.is_empty());
    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    let _: Config = toml::from_str(&toml_str).unwrap();
}

#[test]
fn test_route_codex_cli_complete_config() {
    let cfg = config_for_route(SetupRoute::CodexCli, RouteParams::default());
    assert_eq!(cfg.coordinator.executor.as_deref(), Some("codex"));
    assert_eq!(cfg.agent.executor, "codex");
    assert!(cfg.tiers.fast.is_some());
    assert!(cfg.tiers.standard.is_some());
    assert!(cfg.tiers.premium.is_some());
    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    let _: Config = toml::from_str(&toml_str).unwrap();
}

#[test]
fn test_route_local_complete_config() {
    let cfg = config_for_route(
        SetupRoute::Local,
        RouteParams {
            url: Some("http://localhost:11434/v1".to_string()),
            model: Some("qwen3:4b".to_string()),
            ..Default::default()
        },
    );
    assert_eq!(cfg.coordinator.executor.as_deref(), Some("native"));
    assert!(cfg.tiers.fast.is_some());
    assert!(cfg.tiers.standard.is_some());
    assert!(cfg.tiers.premium.is_some());
    assert_eq!(cfg.llm_endpoints.endpoints.len(), 1);
    assert_eq!(cfg.llm_endpoints.endpoints[0].provider, "local");
    assert!(cfg.llm_endpoints.endpoints[0].api_key_env.is_none());
    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    let _: Config = toml::from_str(&toml_str).unwrap();
}

#[test]
fn test_route_nex_custom_complete_config() {
    let cfg = config_for_route(
        SetupRoute::NexCustom,
        RouteParams {
            url: Some("https://example.com/v1".to_string()),
            api_key_env: Some("MY_KEY".to_string()),
            model: Some("foo".to_string()),
            ..Default::default()
        },
    );
    assert_eq!(cfg.coordinator.executor.as_deref(), Some("native"));
    assert!(cfg.tiers.fast.is_some());
    assert!(cfg.tiers.standard.is_some());
    assert!(cfg.tiers.premium.is_some());
    assert_eq!(cfg.llm_endpoints.endpoints.len(), 1);
    assert_eq!(cfg.llm_endpoints.endpoints[0].provider, "oai-compat");
    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    let _: Config = toml::from_str(&toml_str).unwrap();
}

// ---------------------------------------------------------------------------
// CLI flow: wg setup --route <name> --yes writes complete configs.
// ---------------------------------------------------------------------------

#[test]
fn test_setup_non_interactive_route_writes_config() {
    // wg setup --route claude-cli --yes produces a config with populated tiers.
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let output = run_wg_in_isolation(
        &fake_home,
        &["setup", "--route", "claude-cli", "--yes"],
    );
    assert!(
        output.status.success(),
        "wg setup --route claude-cli --yes failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let cfg = load_global_config(&fake_home);
    assert_eq!(cfg.coordinator.executor.as_deref(), Some("claude"));
    assert_eq!(cfg.agent.executor, "claude");
    assert!(cfg.tiers.fast.is_some(), "tiers.fast must be populated");
    assert!(
        cfg.tiers.standard.is_some(),
        "tiers.standard must be populated"
    );
    assert!(
        cfg.tiers.premium.is_some(),
        "tiers.premium must be populated"
    );
}

#[test]
fn test_setup_route_openrouter_writes_endpoint_and_tiers() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let output = run_wg_in_isolation(
        &fake_home,
        &[
            "setup",
            "--route",
            "openrouter",
            "--api-key-env",
            "OPENROUTER_API_KEY",
            "--yes",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cfg = load_global_config(&fake_home);
    assert_eq!(cfg.coordinator.executor.as_deref(), Some("native"));
    assert_eq!(cfg.llm_endpoints.endpoints.len(), 1);
    let ep = &cfg.llm_endpoints.endpoints[0];
    assert_eq!(ep.provider, "openrouter");
    assert_eq!(ep.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    assert!(cfg.tiers.fast.is_some());
    assert!(cfg.tiers.standard.is_some());
    assert!(cfg.tiers.premium.is_some());
}

#[test]
fn test_setup_route_local_uses_supplied_model() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let output = run_wg_in_isolation(
        &fake_home,
        &[
            "setup",
            "--route",
            "local",
            "--url",
            "http://localhost:11434/v1",
            "--model",
            "qwen3:4b",
            "--yes",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cfg = load_global_config(&fake_home);
    assert_eq!(cfg.coordinator.executor.as_deref(), Some("native"));
    assert_eq!(cfg.tiers.fast.as_deref(), Some("local:qwen3:4b"));
    assert_eq!(cfg.tiers.standard.as_deref(), Some("local:qwen3:4b"));
    assert_eq!(cfg.tiers.premium.as_deref(), Some("local:qwen3:4b"));
}

#[test]
fn test_setup_route_nex_custom_requires_url_and_model() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    // Missing --url
    let output =
        run_wg_in_isolation(&fake_home, &["setup", "--route", "nex-custom", "--yes"]);
    assert!(
        !output.status.success(),
        "should fail without --url: stdout {} stderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nex-custom") && stderr.contains("--url"),
        "error should mention nex-custom and --url, got: {}",
        stderr,
    );
}

#[test]
fn test_setup_dry_run_does_not_write() {
    // --dry-run prints the would-be config but doesn't touch the filesystem.
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let output = run_wg_in_isolation(
        &fake_home,
        &["setup", "--route", "claude-cli", "--dry-run", "--yes"],
    );
    assert!(output.status.success());

    // No global config should have been created.
    let global = fake_home.join(".workgraph/config.toml");
    assert!(
        !global.exists(),
        "dry-run must not create global config at {}",
        global.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dry-run") || stdout.contains("dispatcher") || stdout.contains("agent"),
        "dry-run output should include the would-be config, got: {}",
        stdout,
    );
}

// ---------------------------------------------------------------------------
// wg init --dry-run: no write
// ---------------------------------------------------------------------------

#[test]
fn test_init_dry_run_no_write() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let wg_dir = project.join(".wg");
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let output = Command::new(wg_binary())
        .arg("--dir")
        .arg(&wg_dir)
        .args(["init", "--route", "claude-cli", "--dry-run"])
        .env("HOME", &fake_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "init --dry-run failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // The .wg directory should NOT have been created.
    assert!(
        !wg_dir.exists(),
        ".wg directory should not exist after --dry-run"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dry-run") || stdout.contains("[dispatcher]") || stdout.contains("[agent]"),
        "stdout should show the would-be config, got: {}",
        stdout,
    );
}

// ---------------------------------------------------------------------------
// wg init -x claude → populated [tiers] (the bug)
// ---------------------------------------------------------------------------

#[test]
fn test_init_with_executor_only_populates_tiers() {
    // The validation criteria say `wg init -x claude` should produce
    // populated [tiers] — this is the bug the spec calls out.
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let wg_dir = project.join(".wg");
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let output = Command::new(wg_binary())
        .arg("--dir")
        .arg(&wg_dir)
        .args(["init", "-x", "claude", "--no-agency"])
        .env("HOME", &fake_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "wg init -x claude failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let cfg_path = wg_dir.join("config.toml");
    let cfg_str = fs::read_to_string(&cfg_path).expect("config.toml must be created");
    let cfg: Config = toml::from_str(&cfg_str).expect("config must parse");

    assert!(
        cfg.tiers.fast.is_some() && cfg.tiers.standard.is_some() && cfg.tiers.premium.is_some(),
        "all three tiers must be populated after `wg init -x claude`. Got: fast={:?}, standard={:?}, premium={:?}",
        cfg.tiers.fast,
        cfg.tiers.standard,
        cfg.tiers.premium,
    );
}

// ---------------------------------------------------------------------------
// wg config reset: backup + --keep-keys
// ---------------------------------------------------------------------------

#[test]
fn test_config_reset_keep_keys_preserves_endpoints() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    let global_dir = fake_home.join(".workgraph");
    fs::create_dir_all(&global_dir).unwrap();

    // Pre-populate a global config with an openrouter endpoint
    let pre = r#"
[dispatcher]
executor = "native"

[agent]
executor = "native"
model = "openrouter:anthropic/claude-sonnet-4"

[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
is_default = true
"#;
    fs::write(global_dir.join("config.toml"), pre).unwrap();

    // Reset to claude-cli with --keep-keys --yes
    let output = run_wg_in_isolation(
        &fake_home,
        &[
            "config",
            "reset",
            "--route",
            "claude-cli",
            "--keep-keys",
            "--yes",
        ],
    );
    assert!(
        output.status.success(),
        "config reset failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cfg = load_global_config(&fake_home);
    assert_eq!(
        cfg.coordinator.executor.as_deref(),
        Some("claude"),
        "executor must change to claude per route"
    );
    // Tiers must be populated by the new route
    assert!(cfg.tiers.fast.is_some());
    assert!(cfg.tiers.standard.is_some());
    assert!(cfg.tiers.premium.is_some());
    // Endpoints preserved by --keep-keys
    assert_eq!(
        cfg.llm_endpoints.endpoints.len(),
        1,
        "openrouter endpoint must be preserved"
    );
    let ep = &cfg.llm_endpoints.endpoints[0];
    assert_eq!(ep.name, "openrouter");
    assert_eq!(ep.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
}

#[test]
fn test_config_reset_creates_backup() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    let global_dir = fake_home.join(".workgraph");
    fs::create_dir_all(&global_dir).unwrap();

    let pre = r#"
[dispatcher]
executor = "claude"

[agent]
executor = "claude"
model = "claude:opus"
"#;
    fs::write(global_dir.join("config.toml"), pre).unwrap();

    let output = run_wg_in_isolation(
        &fake_home,
        &[
            "config",
            "reset",
            "--route",
            "openrouter",
            "--yes",
        ],
    );
    assert!(
        output.status.success(),
        "config reset failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    // A backup file should exist.
    let backups: Vec<_> = fs::read_dir(&global_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.toml.bak-")
        })
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "exactly one backup should be created. Found: {:?}",
        fs::read_dir(&global_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );

    // Backup content matches the pre-reset config
    let backup_content = fs::read_to_string(backups[0].path()).unwrap();
    assert!(backup_content.contains("claude"));
    assert!(backup_content.contains("opus"));
}

#[test]
fn test_config_reset_dry_run_does_not_write() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    let global_dir = fake_home.join(".workgraph");
    fs::create_dir_all(&global_dir).unwrap();

    let pre = r#"
[dispatcher]
executor = "claude"

[agent]
executor = "claude"
model = "claude:sonnet"
"#;
    fs::write(global_dir.join("config.toml"), pre).unwrap();
    let original = fs::read_to_string(global_dir.join("config.toml")).unwrap();

    let output = run_wg_in_isolation(
        &fake_home,
        &[
            "config",
            "reset",
            "--route",
            "openrouter",
            "--dry-run",
        ],
    );
    assert!(output.status.success());

    // Config unchanged
    let after = fs::read_to_string(global_dir.join("config.toml")).unwrap();
    assert_eq!(after, original, "dry-run must not modify the config");

    // No backup file
    let backups: Vec<_> = fs::read_dir(&global_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.toml.bak-")
        })
        .collect();
    assert!(backups.is_empty(), "dry-run must not create a backup");
}
