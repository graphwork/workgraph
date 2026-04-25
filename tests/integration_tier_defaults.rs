//! Integration tests for tier-first dispatch contract.
//!
//! Validates that:
//! - task_agent resolves to standard tier (sonnet), not premium (opus)
//! - `wg config --tiers` shows correct defaults
//! - Tier escalation on retry bumps fast→standard→premium
//! - `wg config --tier` remapping works
//! - Agent prompts reference tier names, not vendor model names

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::config::{Config, DispatchRole, Tier};

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
        args,
        stdout,
        stderr
    );
    stdout
}

fn setup_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();
    wg_dir
}

#[test]
fn test_task_agent_resolves_to_standard_tier() {
    let config = Config::default();
    assert_eq!(
        DispatchRole::TaskAgent.default_tier(),
        Tier::Standard,
        "task_agent should default to standard tier (sonnet), not premium (opus)"
    );
    let resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
    assert!(
        resolved.model.contains("sonnet"),
        "task_agent should resolve to a sonnet model, got: {}",
        resolved.model
    );
}

#[test]
fn test_fast_tier_roles() {
    let fast_roles = [
        DispatchRole::Triage,
        DispatchRole::FlipComparison,
        DispatchRole::Assigner,
        DispatchRole::Compactor,
        DispatchRole::ChatCompactor,
        DispatchRole::CoordinatorEval,
        DispatchRole::Placer,
        DispatchRole::FlipInference,
        DispatchRole::Evaluator,
    ];
    for role in &fast_roles {
        assert_eq!(
            role.default_tier(),
            Tier::Fast,
            "{:?} should default to fast tier",
            role
        );
    }
}

#[test]
fn test_premium_tier_roles() {
    let premium_roles = [
        DispatchRole::Evolver,
        DispatchRole::Creator,
        DispatchRole::Verification,
    ];
    for role in &premium_roles {
        assert_eq!(
            role.default_tier(),
            Tier::Premium,
            "{:?} should default to premium tier",
            role
        );
    }
}

#[test]
fn test_tier_escalation() {
    assert_eq!(Tier::Fast.escalate(), Tier::Standard);
    assert_eq!(Tier::Standard.escalate(), Tier::Premium);
    assert_eq!(Tier::Premium.escalate(), Tier::Premium);
}

#[test]
fn test_config_tiers_effective_defaults() {
    let config = Config::default();
    let tiers = config.effective_tiers_public();
    assert_eq!(
        tiers.fast.as_deref(),
        Some("claude:haiku"),
        "fast tier should default to claude:haiku"
    );
    assert_eq!(
        tiers.standard.as_deref(),
        Some("claude:sonnet"),
        "standard tier should default to claude:sonnet"
    );
    assert_eq!(
        tiers.premium.as_deref(),
        Some("claude:opus"),
        "premium tier should default to claude:opus"
    );
}

#[test]
fn test_config_set_tier_remap() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &["config", "--tier", "standard=openrouter:moonshotai/kimi-k2"],
    );

    let config = Config::load(&wg_dir).unwrap();
    assert_eq!(
        config.tiers.standard.as_deref(),
        Some("openrouter:moonshotai/kimi-k2"),
        "standard tier should be remapped to openrouter:moonshotai/kimi-k2"
    );

    let resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
    assert_eq!(
        resolved.model, "moonshotai/kimi-k2",
        "task_agent should resolve through the remapped standard tier"
    );
}

#[test]
fn test_escalate_on_retry_default_off() {
    let config = Config::default();
    assert!(
        !config.coordinator.escalate_on_retry,
        "escalate_on_retry should be off by default"
    );
}

#[test]
fn test_no_tier_escalation_flag() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Test task",
            "--no-place",
            "--no-tier-escalation",
        ],
    );

    let graph =
        workgraph::parser::load_graph(&wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("test-task").unwrap();
    assert!(
        task.no_tier_escalation,
        "task should have no_tier_escalation flag set"
    );
}

#[test]
fn test_tier_display_roundtrip() {
    assert_eq!(Tier::Fast.to_string(), "fast");
    assert_eq!(Tier::Standard.to_string(), "standard");
    assert_eq!(Tier::Premium.to_string(), "premium");
    assert_eq!("fast".parse::<Tier>().unwrap(), Tier::Fast);
    assert_eq!("standard".parse::<Tier>().unwrap(), Tier::Standard);
    assert_eq!("premium".parse::<Tier>().unwrap(), Tier::Premium);
}

#[test]
fn test_tier_default_alias() {
    assert_eq!(Tier::Fast.default_alias(), "haiku");
    assert_eq!(Tier::Standard.default_alias(), "sonnet");
    assert_eq!(Tier::Premium.default_alias(), "opus");
}
