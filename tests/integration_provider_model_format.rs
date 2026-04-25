//! Tests for the provider:model format migration.
//!
//! Validates:
//! - `parse_model_spec` (lenient) and `parse_model_spec_strict` parsing
//! - `Config::validate_model_format` enforcement
//! - `ModelEntry::config_spec` output
//! - Config load/save roundtrip with provider:model values
//! - CLI rejection of bare model names

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use workgraph::config::{
    CLAUDE_OPUS_MODEL_ID, Config, KNOWN_PROVIDERS, RoleModelConfig, parse_model_spec,
    parse_model_spec_strict,
};
use workgraph::graph::WorkGraph;
use workgraph::models::{ModelEntry, ModelRegistry, ModelTier};
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
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

// ===========================================================================
// parse_model_spec_strict
// ===========================================================================

#[test]
fn strict_accepts_all_known_providers() {
    for provider in KNOWN_PROVIDERS {
        let input = format!("{}:some-model", provider);
        let spec = parse_model_spec_strict(&input).unwrap_or_else(|e| {
            panic!(
                "parse_model_spec_strict should accept known provider '{}': {}",
                provider, e
            )
        });
        assert_eq!(spec.provider.as_deref(), Some(*provider));
        assert_eq!(spec.model_id, "some-model");
    }
}

#[test]
fn strict_rejects_bare_model_name() {
    let err = parse_model_spec_strict("opus").unwrap_err();
    assert!(
        err.message.contains("provider:model"),
        "Error should mention provider:model format: {}",
        err.message
    );
    assert_eq!(err.input, "opus");
}

#[test]
fn strict_rejects_empty_string() {
    let err = parse_model_spec_strict("").unwrap_err();
    assert!(err.message.contains("empty"), "Error: {}", err.message);
}

#[test]
fn strict_rejects_unknown_provider_prefix() {
    let err = parse_model_spec_strict("foobar:gpt-4").unwrap_err();
    assert!(
        err.message.contains("Unknown provider"),
        "Error: {}",
        err.message
    );
    assert!(
        err.message.contains("foobar"),
        "Error should mention the bad provider: {}",
        err.message
    );
}

#[test]
fn strict_rejects_provider_with_empty_model() {
    let err = parse_model_spec_strict("claude:").unwrap_err();
    assert!(
        err.message.contains("no model name"),
        "Error: {}",
        err.message
    );
}

#[test]
fn strict_handles_model_with_slashes() {
    // e.g., openrouter:deepseek/deepseek-v3.2
    let spec = parse_model_spec_strict("openrouter:deepseek/deepseek-v3.2").unwrap();
    assert_eq!(spec.provider.as_deref(), Some("openrouter"));
    assert_eq!(spec.model_id, "deepseek/deepseek-v3.2");
}

#[test]
fn strict_rejects_legacy_slash_format() {
    // "deepseek/deepseek-v3.2" has no known provider prefix — should be rejected
    let err = parse_model_spec_strict("deepseek/deepseek-v3.2").unwrap_err();
    assert!(
        err.message.contains("provider:model"),
        "Bare slash format should be rejected: {}",
        err.message
    );
}

// ===========================================================================
// parse_model_spec (lenient) edge cases beyond the existing unit tests
// ===========================================================================

#[test]
fn lenient_treats_unknown_prefix_as_bare() {
    // "unknown:model" should treat the whole thing as bare
    let spec = parse_model_spec("unknown:model");
    assert_eq!(spec.provider, None);
    assert_eq!(spec.model_id, "unknown:model");
}

#[test]
fn lenient_empty_string() {
    let spec = parse_model_spec("");
    assert_eq!(spec.provider, None);
    assert_eq!(spec.model_id, "");
}

#[test]
fn lenient_double_colon() {
    // "claude::opus" — first split is "claude" + ":opus"
    let spec = parse_model_spec("claude::opus");
    assert_eq!(spec.provider.as_deref(), Some("claude"));
    assert_eq!(spec.model_id, ":opus");
}

// ===========================================================================
// ModelEntry::config_spec
// ===========================================================================

#[test]
fn config_spec_anthropic_maps_to_claude_prefix() {
    let entry = ModelEntry {
        id: format!("anthropic/{CLAUDE_OPUS_MODEL_ID}"),
        provider: "anthropic".into(),
        cost_per_1m_input: 15.0,
        cost_per_1m_output: 75.0,
        context_window: 200_000,
        capabilities: vec!["tool_use".into()],
        tier: ModelTier::Frontier,
    };
    assert_eq!(
        entry.config_spec(),
        format!("claude:{CLAUDE_OPUS_MODEL_ID}")
    );
}

#[test]
fn config_spec_openrouter_stays_openrouter() {
    let entry = ModelEntry {
        id: "deepseek/deepseek-chat-v3".into(),
        provider: "openrouter".into(),
        cost_per_1m_input: 0.27,
        cost_per_1m_output: 1.10,
        context_window: 64_000,
        capabilities: vec!["tool_use".into()],
        tier: ModelTier::Budget,
    };
    assert_eq!(entry.config_spec(), "openrouter:deepseek-chat-v3");
}

#[test]
fn config_spec_openai_stays_openai() {
    let entry = ModelEntry {
        id: "openai/gpt-4o".into(),
        provider: "openai".into(),
        cost_per_1m_input: 2.50,
        cost_per_1m_output: 10.0,
        context_window: 128_000,
        capabilities: vec!["tool_use".into()],
        tier: ModelTier::Frontier,
    };
    assert_eq!(entry.config_spec(), "openai:gpt-4o");
}

#[test]
fn config_spec_roundtrips_through_parse() {
    // Every built-in registry entry's config_spec should parse back correctly
    let reg = ModelRegistry::with_defaults();
    for entry in reg.models.values() {
        let spec_str = entry.config_spec();
        let parsed = parse_model_spec_strict(&spec_str).unwrap_or_else(|e| {
            panic!(
                "config_spec '{}' for model '{}' should be valid: {}",
                spec_str, entry.id, e
            )
        });
        assert_eq!(
            parsed.model_id,
            entry.short_name(),
            "model_id mismatch for {}",
            entry.id
        );
        assert!(
            parsed.provider.is_some(),
            "config_spec should always include a provider for {}",
            entry.id
        );
    }
}

// ===========================================================================
// Config::validate_model_format
// ===========================================================================

#[test]
fn validate_format_accepts_default_config() {
    let config = Config::default();
    config
        .validate_model_format()
        .expect("Default config should pass validate_model_format");
}

#[test]
fn validate_format_rejects_bare_agent_model() {
    let mut config = Config::default();
    config.agent.model = "opus".to_string();
    let err = config.validate_model_format().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("agent.model"),
        "Should identify the field: {}",
        msg
    );
    assert!(
        msg.contains("provider:model"),
        "Should suggest migration: {}",
        msg
    );
}

#[test]
fn validate_format_rejects_bare_coordinator_model() {
    let mut config = Config::default();
    config.coordinator.model = Some("haiku".to_string());
    let err = config.validate_model_format().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("coordinator.model"),
        "Should identify the field: {}",
        msg
    );
}

#[test]
fn validate_format_rejects_deprecated_coordinator_provider() {
    let mut config = Config::default();
    config.coordinator.provider = Some("openrouter".to_string());
    let err = config.validate_model_format().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("coordinator.provider") && msg.contains("deprecated"),
        "Should flag deprecated provider field: {}",
        msg
    );
}

#[test]
fn validate_format_rejects_bare_tier_value() {
    let mut config = Config::default();
    config.tiers.fast = Some("haiku".to_string());
    let err = config.validate_model_format().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("tiers.fast"),
        "Should identify the field: {}",
        msg
    );
}

#[test]
fn validate_format_accepts_provider_prefix_in_tiers() {
    let mut config = Config::default();
    config.tiers.fast = Some("claude:haiku".to_string());
    config.tiers.standard = Some("openai:gpt-4o".to_string());
    config.tiers.premium = Some("claude:opus".to_string());
    config
        .validate_model_format()
        .expect("provider:model tier values should pass");
}

#[test]
fn validate_format_rejects_bare_role_model() {
    let mut config = Config::default();
    config.models.default = Some(RoleModelConfig {
        model: Some("sonnet".to_string()),
        provider: None,
        tier: None,
        endpoint: None,
    });
    let err = config.validate_model_format().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("models.default.model"),
        "Should identify the field: {}",
        msg
    );
}

#[test]
fn validate_format_rejects_deprecated_role_provider() {
    let mut config = Config::default();
    config.models.evaluator = Some(RoleModelConfig {
        model: Some("claude:opus".to_string()),
        provider: Some("anthropic".to_string()), // deprecated
        tier: None,
        endpoint: None,
    });
    let err = config.validate_model_format().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("deprecated"),
        "Should flag deprecated provider field: {}",
        msg
    );
}

#[test]
fn validate_format_collects_multiple_errors() {
    let mut config = Config::default();
    config.agent.model = "opus".to_string();
    config.coordinator.model = Some("haiku".to_string());
    config.tiers.fast = Some("sonnet".to_string());
    let err = config.validate_model_format().unwrap_err();
    let msg = err.to_string();
    // All three fields should be mentioned
    assert!(msg.contains("agent.model"), "Missing agent.model: {}", msg);
    assert!(
        msg.contains("coordinator.model"),
        "Missing coordinator.model: {}",
        msg
    );
    assert!(msg.contains("tiers.fast"), "Missing tiers.fast: {}", msg);
}

// ===========================================================================
// Config load/save roundtrip with provider:model values
// ===========================================================================

#[test]
fn config_save_load_roundtrip_provider_model() {
    let tmp = TempDir::new().unwrap();
    let mut config = Config::default();
    config.agent.model = "openrouter:deepseek/deepseek-chat".to_string();
    config.coordinator.model = Some("claude:sonnet".to_string());
    config.save(tmp.path()).unwrap();

    let loaded = Config::load(tmp.path()).unwrap();
    assert_eq!(loaded.agent.model, "openrouter:deepseek/deepseek-chat");
    assert_eq!(loaded.coordinator.model.as_deref(), Some("claude:sonnet"));
}

#[test]
fn config_load_rejects_bare_model_in_toml() {
    let tmp = TempDir::new().unwrap();
    let toml = r#"
[agent]
model = "opus"
"#;
    fs::write(tmp.path().join("config.toml"), toml).unwrap();
    let err = Config::load(tmp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("provider:model"),
        "Config load should reject bare model: {}",
        msg
    );
}

#[test]
fn config_load_rejects_deprecated_provider_field() {
    let tmp = TempDir::new().unwrap();
    let toml = r#"
[agent]
model = "claude:opus"

[coordinator]
provider = "openrouter"
model = "openrouter:deepseek/deepseek-chat"
"#;
    fs::write(tmp.path().join("config.toml"), toml).unwrap();
    let err = Config::load(tmp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("coordinator.provider") && msg.contains("deprecated"),
        "Should reject deprecated coordinator.provider: {}",
        msg
    );
}

// ===========================================================================
// model_choices returns provider:model format
// ===========================================================================

#[test]
fn model_choices_use_provider_prefix() {
    let reg = ModelRegistry::with_defaults();
    let choices = reg.model_choices();
    for choice in &choices {
        assert!(
            choice.contains(':'),
            "model_choices entry '{}' should use provider:model format",
            choice
        );
        // Should be parseable by strict parser
        parse_model_spec_strict(choice).unwrap_or_else(|e| {
            panic!(
                "model_choices entry '{}' should be valid provider:model: {}",
                choice, e
            )
        });
    }
}

#[test]
fn model_choices_with_descriptions_use_provider_prefix() {
    let reg = ModelRegistry::with_defaults();
    let choices = reg.model_choices_with_descriptions();
    for (spec, _desc) in &choices {
        assert!(
            spec.contains(':'),
            "model choice '{}' should use provider:model format",
            spec
        );
    }
}

// ===========================================================================
// CLI integration: wg config --model rejects bare names
// ===========================================================================

#[test]
fn cli_config_set_model_accepts_provider_format() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(&wg_dir, &["config", "--model", "claude:haiku"]);

    // Verify it persisted
    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("claude:haiku"),
        "Config should contain claude:haiku: {}",
        show
    );
}

#[test]
fn cli_config_set_model_rejects_bare_name() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let output = wg_cmd(&wg_dir, &["config", "--model", "opus"]);
    assert!(
        !output.status.success(),
        "wg config --model opus should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("provider:model") || stderr.contains("provider"),
        "Error should mention provider:model format: {}",
        stderr
    );
}

#[test]
fn cli_config_set_model_accepts_openrouter_format() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(
        &wg_dir,
        &["config", "--model", "openrouter:deepseek/deepseek-chat"],
    );
}

// ===========================================================================
// Default config uses provider:model
// ===========================================================================

#[test]
fn default_agent_model_has_provider_prefix() {
    let config = Config::default();
    let spec = parse_model_spec_strict(&config.agent.model).unwrap_or_else(|e| {
        panic!(
            "Default agent.model '{}' should be valid provider:model: {}",
            config.agent.model, e
        )
    });
    assert_eq!(spec.provider.as_deref(), Some("claude"));
    assert_eq!(spec.model_id, "opus");
}

// ===========================================================================
// load_model_choices fallback uses provider:model
// ===========================================================================

#[test]
fn load_model_choices_all_use_provider_prefix() {
    let tmp = TempDir::new().unwrap();
    let choices = workgraph::models::load_model_choices(tmp.path());
    assert!(!choices.is_empty(), "Should have at least one model choice");
    for choice in &choices {
        parse_model_spec_strict(choice).unwrap_or_else(|e| {
            panic!(
                "Model choice '{}' should be valid provider:model: {}",
                choice, e
            )
        });
    }
}

#[test]
fn load_model_choices_with_descriptions_fallback() {
    let tmp = TempDir::new().unwrap();
    let choices = workgraph::models::load_model_choices_with_descriptions(tmp.path());
    for (spec, _desc) in &choices {
        assert!(
            spec.contains(':'),
            "Fallback choice '{}' should have provider prefix",
            spec
        );
    }
}

// ===========================================================================
// Compaction threshold resolves provider:model correctly
// ===========================================================================

// ===========================================================================
// CLI: wg config --coordinator-model rejects bare names
// ===========================================================================

#[test]
fn cli_config_set_coordinator_model_rejects_bare_name() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let output = wg_cmd(&wg_dir, &["config", "--coordinator-model", "haiku"]);
    assert!(
        !output.status.success(),
        "wg config --coordinator-model haiku should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("provider:model") || stderr.contains("provider"),
        "Error should mention provider:model format: {}",
        stderr
    );
}

#[test]
fn cli_config_set_coordinator_model_accepts_provider_format() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(&wg_dir, &["config", "--coordinator-model", "claude:sonnet"]);

    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("claude:sonnet"),
        "Config should contain claude:sonnet: {}",
        show
    );
}

// ===========================================================================
// CLI: wg config --set-model rejects bare names
// ===========================================================================

#[test]
fn cli_config_set_model_role_rejects_bare_name() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let output = wg_cmd(&wg_dir, &["config", "--set-model", "default", "sonnet"]);
    assert!(
        !output.status.success(),
        "wg config --set-model default sonnet should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("provider:model") || stderr.contains("provider"),
        "Error should mention provider:model format: {}",
        stderr
    );
}

#[test]
fn cli_config_set_model_role_accepts_provider_format() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(
        &wg_dir,
        &["config", "--set-model", "default", "claude:opus"],
    );
}

// ===========================================================================
// Strict parser rejects common bare model names exhaustively
// ===========================================================================

#[test]
fn strict_rejects_common_bare_model_names() {
    let opus_bare = CLAUDE_OPUS_MODEL_ID.to_string();
    let bare_names = [
        "opus",
        "sonnet",
        "haiku",
        "gpt-4o",
        "gpt-4o-mini",
        "deepseek-chat-v3",
        "gemini-2.5-pro",
        opus_bare.as_str(),
        "claude-sonnet-4-latest",
    ];
    for name in &bare_names {
        let result = parse_model_spec_strict(name);
        assert!(
            result.is_err(),
            "Bare model name '{}' should be rejected by strict parser",
            name
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("provider:model"),
            "Error for '{}' should mention provider:model format: {}",
            name,
            err.message
        );
    }
}

// ===========================================================================
// Strict parser rejects legacy slash-only format (no provider prefix)
// ===========================================================================

#[test]
fn strict_rejects_various_legacy_slash_formats() {
    let opus_legacy = format!("anthropic/{CLAUDE_OPUS_MODEL_ID}");
    let legacy_formats = [
        opus_legacy.as_str(),
        "openai/gpt-4o",
        "google/gemini-2.5-pro",
        "deepseek/deepseek-chat-v3",
        "meta-llama/llama-4-maverick",
    ];
    for name in &legacy_formats {
        let result = parse_model_spec_strict(name);
        assert!(
            result.is_err(),
            "Legacy slash format '{}' should be rejected by strict parser",
            name
        );
    }
}

// ===========================================================================
// Compaction threshold resolves provider:model correctly
// ===========================================================================

#[test]
fn compaction_threshold_parses_provider_prefix() {
    let mut config = Config::default();
    config.coordinator.model = Some("claude:haiku".to_string());
    config.coordinator.compaction_threshold_ratio = 0.8;
    let threshold = config.effective_compaction_threshold();
    // haiku has 200_000 context window, 80% = 160_000
    assert_eq!(threshold, 160_000);
}

#[test]
fn compaction_threshold_from_agent_model_with_prefix() {
    let config = Config::default();
    // Default agent.model is "claude:opus" → registry lookup "opus" → 200_000
    let threshold = config.effective_compaction_threshold();
    assert_eq!(threshold, 160_000);
}
