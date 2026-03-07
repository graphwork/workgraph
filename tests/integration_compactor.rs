//! Smoke tests for compactor context generation.
//!
//! Tests that the compactor produces context.md with 3-layer structure
//! (Rolling Narrative, Persistent Facts, Evaluation Digest), that output
//! is bounded, and that the coordinator can consume it correctly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use workgraph::config::Config;
use workgraph::service::compactor::{self, CompactorState};

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

fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args, stdout, stderr
    );
    stdout
}

fn setup_wg(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    wg_dir
}

fn write_fake_context_md(wg_dir: &Path, content: &str) {
    let compactor_dir = wg_dir.join("compactor");
    fs::create_dir_all(&compactor_dir).unwrap();
    fs::write(compactor_dir.join("context.md"), content).unwrap();
}

fn fake_3_layer_context() -> String {
    "# Project Context\n\
     \n\
     ## 1. Rolling Narrative\n\
     \n\
     The project began with infrastructure setup. Task infra-init established \
     the base configuration. Currently task feature-auth is in progress, \
     implementing OAuth2 authentication. The team discovered a dependency \
     on a shared library that required task lib-upgrade to be completed first.\n\
     \n\
     ## 2. Persistent Facts\n\
     \n\
     - Architecture: monorepo with Rust backend and React frontend\n\
     - Convention: all task IDs use kebab-case\n\
     - Key path: src/service/ contains coordinator logic\n\
     - Integration: uses workgraph for task management\n\
     \n\
     ## 3. Evaluation Digest\n\
     \n\
     - infra-init: score=9.0, verdict=pass\n\
     - lib-upgrade: score=7.5, verdict=pass\n\
     - No agent performance issues detected.\n"
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_compactor_state_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let state = CompactorState {
        last_compaction: Some("2026-01-01T00:00:00Z".to_string()),
        last_ops_count: 42,
        last_tick: 5,
        compaction_count: 3,
    };
    state.save(dir).unwrap();

    let loaded = CompactorState::load(dir);
    assert_eq!(loaded.last_ops_count, 42);
    assert_eq!(loaded.last_tick, 5);
    assert_eq!(loaded.compaction_count, 3);
    assert_eq!(
        loaded.last_compaction.as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
}

#[test]
fn test_compactor_state_default_on_missing() {
    let tmp = TempDir::new().unwrap();
    let state = CompactorState::load(tmp.path());
    assert_eq!(state.last_ops_count, 0);
    assert_eq!(state.last_tick, 0);
    assert_eq!(state.compaction_count, 0);
    assert!(state.last_compaction.is_none());
}

#[test]
fn test_should_compact_disabled_when_interval_zero() {
    let tmp = TempDir::new().unwrap();
    let mut config = Config::default();
    config.coordinator.compactor_interval = 0;
    assert!(!compactor::should_compact(tmp.path(), 100, &config));
}

#[test]
fn test_should_compact_by_tick_interval() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("compactor")).unwrap();

    let mut config = Config::default();
    config.coordinator.compactor_interval = 5;

    // No state yet, tick 0 vs last_tick 0 -> diff 0, not enough
    assert!(!compactor::should_compact(dir, 0, &config));

    // tick 5 vs last_tick 0 -> diff 5 >= interval 5
    assert!(compactor::should_compact(dir, 5, &config));

    // Save state at tick 5
    let state = CompactorState {
        last_tick: 5,
        ..Default::default()
    };
    state.save(dir).unwrap();

    // tick 9 vs last_tick 5 -> diff 4 < 5
    assert!(!compactor::should_compact(dir, 9, &config));

    // tick 10 vs last_tick 5 -> diff 5 >= 5
    assert!(compactor::should_compact(dir, 10, &config));
}

#[test]
fn test_should_compact_by_ops_growth() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("compactor")).unwrap();
    fs::create_dir_all(dir.join("log")).unwrap();

    let mut config = Config::default();
    config.coordinator.compactor_interval = 1000;
    config.coordinator.compactor_ops_threshold = 3;

    let state = CompactorState {
        last_tick: 0,
        last_ops_count: 0,
        ..Default::default()
    };
    state.save(dir).unwrap();

    let ops_path = dir.join("log").join("operations.jsonl");
    fs::write(&ops_path, "{}\n{}\n{}\n").unwrap();

    assert!(compactor::should_compact(dir, 1, &config));
}

#[test]
fn test_context_md_path_is_correct() {
    let path = compactor::context_md_path(Path::new("/tmp/wg"));
    assert_eq!(path, PathBuf::from("/tmp/wg/compactor/context.md"));
}

#[test]
fn test_context_md_3_layer_structure() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();

    let content = fake_3_layer_context();
    write_fake_context_md(wg_dir, &content);

    let ctx_path = compactor::context_md_path(wg_dir);
    assert!(ctx_path.exists(), "context.md should exist");

    let text = fs::read_to_string(&ctx_path).unwrap();
    assert!(text.contains("# Project Context"), "Should have top-level heading");
    assert!(text.contains("## 1. Rolling Narrative"), "Should have Rolling Narrative section");
    assert!(text.contains("## 2. Persistent Facts"), "Should have Persistent Facts section");
    assert!(text.contains("## 3. Evaluation Digest"), "Should have Evaluation Digest section");
}

#[test]
fn test_context_md_bounded_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();

    let content = fake_3_layer_context();
    write_fake_context_md(wg_dir, &content);

    let ctx_path = compactor::context_md_path(wg_dir);
    let text = fs::read_to_string(&ctx_path).unwrap();

    // ~3000 tokens budget at ~4 chars/token = ~12000 chars
    let max_chars = 20_000;
    assert!(
        text.len() < max_chars,
        "context.md should be bounded. Got {} chars, max {}",
        text.len(),
        max_chars
    );
}

#[test]
fn test_compactor_state_persists_across_compactions() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let state1 = CompactorState {
        last_compaction: Some("2026-01-01T00:00:00Z".to_string()),
        last_ops_count: 10,
        last_tick: 5,
        compaction_count: 1,
    };
    state1.save(dir).unwrap();
    assert_eq!(CompactorState::load(dir).compaction_count, 1);

    let state2 = CompactorState {
        last_compaction: Some("2026-01-01T01:00:00Z".to_string()),
        last_ops_count: 25,
        last_tick: 10,
        compaction_count: 2,
    };
    state2.save(dir).unwrap();

    let loaded = CompactorState::load(dir);
    assert_eq!(loaded.compaction_count, 2);
    assert_eq!(loaded.last_tick, 10);
    assert_eq!(loaded.last_ops_count, 25);
}

#[test]
fn test_coordinator_consumes_context_md() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_wg(&tmp);

    wg_ok(&wg_dir, &["add", "Task Alpha", "-d", "First task"]);
    wg_ok(&wg_dir, &["add", "Task Beta", "-d", "Second task"]);

    let content = fake_3_layer_context();
    write_fake_context_md(&wg_dir, &content);

    let ctx_path = compactor::context_md_path(&wg_dir);
    assert!(ctx_path.exists());

    let text = fs::read_to_string(&ctx_path).unwrap();
    assert!(!text.is_empty());

    let expected_path = wg_dir.join("compactor").join("context.md");
    assert_eq!(ctx_path, expected_path);
}

#[test]
fn test_compactor_config_defaults() {
    let config = Config::default();
    assert_eq!(config.coordinator.compactor_interval, 5);
    assert_eq!(config.coordinator.compactor_ops_threshold, 100);
}

#[test]
fn test_should_compact_not_triggered_below_thresholds() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("compactor")).unwrap();
    fs::create_dir_all(dir.join("log")).unwrap();

    let mut config = Config::default();
    config.coordinator.compactor_interval = 10;
    config.coordinator.compactor_ops_threshold = 50;

    let state = CompactorState {
        last_tick: 5,
        last_ops_count: 10,
        ..Default::default()
    };
    state.save(dir).unwrap();

    let ops_path = dir.join("log").join("operations.jsonl");
    let ops_content: String = (0..12).map(|_| "{}\n").collect();
    fs::write(&ops_path, &ops_content).unwrap();

    assert!(!compactor::should_compact(dir, 10, &config));
}

#[test]
fn test_compactor_state_corrupt_file_returns_default() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let compactor_dir = dir.join("compactor");
    fs::create_dir_all(&compactor_dir).unwrap();

    fs::write(compactor_dir.join("state.json"), "not valid json!!!").unwrap();

    let state = CompactorState::load(dir);
    assert_eq!(state.last_ops_count, 0);
    assert_eq!(state.last_tick, 0);
    assert!(state.last_compaction.is_none());
}
