//! Integration tests for the canonical-config UX:
//!   `wg config init [--global|--local]`
//!   `wg migrate config`
//!   built-in defaults (no `~/.wg/config.toml` required)
//!
//! These tests exercise the real `wg` binary so they catch CLI plumbing
//! regressions (subcommand parsing, dispatch, file writes) — not just
//! the underlying Rust functions.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::WorkGraph;
use workgraph::parser::save_graph;

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(path.exists(), "wg binary not found at {:?}", path);
    path
}

fn wg(wg_dir: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .env("HOME", home)
        .env_remove("WG_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn wg")
}

fn wg_ok(wg_dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = wg(wg_dir, home, args);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr,
    );
    stdout
}

fn fresh_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph = WorkGraph::new();
    save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();
    wg_dir
}

// ---------------------------------------------------------------------------
// test_defaults_no_user_config
// ---------------------------------------------------------------------------

#[test]
fn defaults_no_user_config_run_claude_opus() {
    // With NO ~/.wg/config.toml at all, `wg config --merged` must show
    // claude executor + opus model. Otherwise the binary's defaults are
    // not the canonical ones the design picked.
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    fs::create_dir_all(&home).unwrap();
    let wg_dir = fresh_workgraph(&tmp);

    // Sanity — no config file written yet.
    assert!(!home.join(".wg/config.toml").exists());
    assert!(!home.join(".workgraph/config.toml").exists());
    assert!(!wg_dir.join("config.toml").exists());

    let out = wg_ok(&wg_dir, &home, &["config", "--merged"]);
    assert!(
        out.contains("claude:opus"),
        "default agent.model should be claude:opus; got:\n{}",
        out,
    );
    assert!(
        out.contains("\"claude\""),
        "default executor should be claude; got:\n{}",
        out,
    );
}

// ---------------------------------------------------------------------------
// test_config_init_global_writes_minimal
// ---------------------------------------------------------------------------

#[test]
fn config_init_global_writes_minimal_canonical() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    fs::create_dir_all(&home).unwrap();
    let wg_dir = fresh_workgraph(&tmp);

    let stdout = wg_ok(&wg_dir, &home, &["config", "init", "--global"]);
    assert!(
        stdout.contains("Wrote minimal global config"),
        "init should announce what it wrote; got:\n{}",
        stdout,
    );

    let path = home.join(".wg/config.toml");
    assert!(
        path.exists(),
        "init --global should create ~/.wg/config.toml; got nothing at {:?}",
        path,
    );
    let body = fs::read_to_string(&path).unwrap();

    // The minimal claude-cli global config must contain:
    //   [agent] model = "claude:opus"
    //   [tiers] fast/standard/premium
    //   [models.evaluator] model
    //   [models.assigner] model
    assert!(body.contains("[agent]"), "missing [agent]; got:\n{}", body);
    assert!(body.contains("model = \"claude:opus\""), "missing claude:opus; got:\n{}", body);
    assert!(body.contains("[tiers]"));
    assert!(body.contains("fast = \"claude:haiku\""));
    assert!(body.contains("standard = \"claude:sonnet\""));
    assert!(body.contains("premium = \"claude:opus\""));
    assert!(body.contains("[models.evaluator]"));
    assert!(body.contains("[models.assigner]"));

    // The minimal config must NOT carry restated defaults:
    // no [dispatcher] section, no [chat], no [help], no [guardrails], etc.
    assert!(
        !body.contains("[dispatcher]"),
        "minimal global config should not restate dispatcher defaults; got:\n{}",
        body,
    );
    assert!(
        !body.contains("[guardrails]"),
        "minimal global config should not restate guardrails defaults; got:\n{}",
        body,
    );
    assert!(
        !body.contains("verify_autospawn_enabled"),
        "minimal global config should not contain deprecated keys; got:\n{}",
        body,
    );
    assert!(
        !body.contains("agent.executor") && !body.contains("\nexecutor ="),
        "minimal global config should not contain agent.executor (deprecated); got:\n{}",
        body,
    );
}

#[test]
fn config_init_refuses_to_clobber_existing_without_force() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    fs::create_dir_all(&home).unwrap();
    let wg_dir = fresh_workgraph(&tmp);

    // Pre-existing global config with custom value.
    fs::create_dir_all(home.join(".wg")).unwrap();
    fs::write(
        home.join(".wg/config.toml"),
        "[agent]\nmodel = \"openrouter:anthropic/claude-opus-4-7\"\n",
    )
    .unwrap();

    let out = wg(&wg_dir, &home, &["config", "init", "--global"]);
    assert!(
        !out.status.success(),
        "init --global should refuse to clobber an existing file"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already exists") || stderr.contains("--force"),
        "error message should mention --force; got:\n{}",
        stderr,
    );
}

#[test]
fn config_init_force_makes_backup() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    fs::create_dir_all(&home).unwrap();
    let wg_dir = fresh_workgraph(&tmp);

    fs::create_dir_all(home.join(".wg")).unwrap();
    fs::write(
        home.join(".wg/config.toml"),
        "# pre-existing\n[agent]\nmodel = \"custom-model\"\n",
    )
    .unwrap();

    wg_ok(&wg_dir, &home, &["config", "init", "--global", "--force"]);
    let backup = home.join(".wg/config.toml.bak");
    assert!(
        backup.exists(),
        "init --global --force should write a .bak; got nothing at {:?}",
        backup,
    );
    let backup_body = fs::read_to_string(&backup).unwrap();
    assert!(backup_body.contains("custom-model"));
}

// ---------------------------------------------------------------------------
// test_migrate_strips_deprecated
// ---------------------------------------------------------------------------

#[test]
fn migrate_strips_deprecated_agent_executor() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    fs::create_dir_all(&home).unwrap();
    let wg_dir = fresh_workgraph(&tmp);

    fs::write(
        wg_dir.join("config.toml"),
        r#"
[agent]
executor = "claude"
model = "claude:opus"
"#,
    )
    .unwrap();

    let stdout = wg_ok(&wg_dir, &home, &["migrate", "config", "--local"]);
    assert!(
        stdout.contains("agent.executor"),
        "migrate should report removing agent.executor; got:\n{}",
        stdout,
    );

    let body = fs::read_to_string(wg_dir.join("config.toml")).unwrap();
    assert!(
        !body.contains("executor"),
        "migrated config must not contain executor; got:\n{}",
        body,
    );
    assert!(
        body.contains("model = \"claude:opus\""),
        "migrated config must keep model; got:\n{}",
        body,
    );
}

// ---------------------------------------------------------------------------
// test_migrate_stale_model
// ---------------------------------------------------------------------------

#[test]
fn migrate_rewrites_stale_openrouter_sonnet_model() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    fs::create_dir_all(&home).unwrap();
    let wg_dir = fresh_workgraph(&tmp);

    fs::write(
        wg_dir.join("config.toml"),
        r#"
[agent]
model = "openrouter:anthropic/claude-sonnet-4"
"#,
    )
    .unwrap();

    let stdout = wg_ok(&wg_dir, &home, &["migrate", "config", "--local"]);
    assert!(
        stdout.contains("openrouter:anthropic/claude-sonnet-4-6"),
        "migrate should announce the rewrite; got:\n{}",
        stdout,
    );

    let body = fs::read_to_string(wg_dir.join("config.toml")).unwrap();
    assert!(body.contains("openrouter:anthropic/claude-sonnet-4-6"));
    assert!(
        !body.contains("\"openrouter:anthropic/claude-sonnet-4\""),
        "old stale string must be gone; got:\n{}",
        body,
    );
}

#[test]
fn migrate_dry_run_does_not_modify() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    fs::create_dir_all(&home).unwrap();
    let wg_dir = fresh_workgraph(&tmp);

    let original = "[agent]\nexecutor = \"claude\"\n";
    fs::write(wg_dir.join("config.toml"), original).unwrap();

    wg_ok(&wg_dir, &home, &["migrate", "config", "--local", "--dry-run"]);
    let after = fs::read_to_string(wg_dir.join("config.toml")).unwrap();
    assert_eq!(
        original, after,
        "dry-run must not touch the config file"
    );
}

// ---------------------------------------------------------------------------
// `wg quickstart` works with no global config (sanity)
// ---------------------------------------------------------------------------

#[test]
fn quickstart_with_no_global_config_does_not_error() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("fakehome");
    fs::create_dir_all(&home).unwrap();
    let wg_dir = fresh_workgraph(&tmp);

    let out = wg(&wg_dir, &home, &["quickstart"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "quickstart with no global config should succeed.\nstdout: {}\nstderr: {}",
        stdout, stderr,
    );
}
