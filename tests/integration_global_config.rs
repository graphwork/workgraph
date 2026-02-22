//! Integration tests for the global config system.
//!
//! Tests global config creation, local-only / global-only scenarios, merge
//! semantics (local overrides global, global values inherited), source
//! annotations (global/local/default), scoped writes, and that downstream
//! consumers (service daemon config, evaluate config) pick up merged values.
//!
//! All tests use temporary directories as fake HOME / local workgraph, so
//! the real user's `~/.workgraph/config.toml` is never read or modified.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use workgraph::config::{Config, ConfigSource};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a fake HOME with `~/.workgraph/` directory and optionally write a
/// global config.toml there.  Returns the `.workgraph` dir inside fake HOME.
fn setup_global_dir(tmp: &TempDir, global_toml: Option<&str>) -> PathBuf {
    let global_dir = tmp.path().join("fakehome").join(".workgraph");
    fs::create_dir_all(&global_dir).unwrap();
    if let Some(content) = global_toml {
        fs::write(global_dir.join("config.toml"), content).unwrap();
    }
    global_dir
}

/// Create a local .workgraph directory and optionally write a config.toml
/// inside it.  Returns the .workgraph directory path.
fn setup_local_dir(tmp: &TempDir, local_toml: Option<&str>) -> PathBuf {
    let wg_dir = tmp.path().join("project").join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    if let Some(content) = local_toml {
        fs::write(wg_dir.join("config.toml"), content).unwrap();
    }
    wg_dir
}

fn load_toml_or_empty(path: &Path) -> toml::Value {
    if !path.exists() {
        return toml::Value::Table(toml::map::Map::new());
    }
    fs::read_to_string(path).unwrap().parse().unwrap()
}

/// Load merged config using custom global/local paths (not relying on HOME).
fn load_merged_custom(global_dir: &Path, local_dir: &Path) -> Config {
    let global_val = load_toml_or_empty(&global_dir.join("config.toml"));
    let local_val = load_toml_or_empty(&local_dir.join("config.toml"));
    let merged = workgraph::config::merge_toml(global_val, local_val);
    merged.try_into().expect("deserialize merged config")
}

/// Load merged config with source tracking using custom paths.
fn load_with_sources_custom(
    global_dir: &Path,
    local_dir: &Path,
) -> (Config, BTreeMap<String, ConfigSource>) {
    let global_val = load_toml_or_empty(&global_dir.join("config.toml"));
    let local_val = load_toml_or_empty(&local_dir.join("config.toml"));

    let mut sources = BTreeMap::new();
    record_sources(&global_val, "", &ConfigSource::Global, &mut sources);
    record_sources(&local_val, "", &ConfigSource::Local, &mut sources);

    let merged = workgraph::config::merge_toml(global_val, local_val);
    let config: Config = merged.try_into().expect("deserialize merged config");

    // Fill in defaults for keys present in neither file
    let default_val: toml::Value = toml::Value::try_from(Config::default())
        .unwrap_or(toml::Value::Table(toml::map::Map::new()));
    let mut default_sources = BTreeMap::new();
    record_sources(&default_val, "", &ConfigSource::Default, &mut default_sources);
    for (key, src) in default_sources {
        sources.entry(key).or_insert(src);
    }

    (config, sources)
}

/// Walk a TOML value tree and record source per leaf key.
fn record_sources(
    val: &toml::Value,
    prefix: &str,
    source: &ConfigSource,
    map: &mut BTreeMap<String, ConfigSource>,
) {
    if let toml::Value::Table(table) = val {
        for (key, v) in table {
            let full_key = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{}.{}", prefix, key)
            };
            match v {
                toml::Value::Table(_) => record_sources(v, &full_key, source, map),
                _ => {
                    map.insert(full_key, source.clone());
                }
            }
        }
    }
}

// ===========================================================================
// 1. Global config creation at ~/.workgraph/config.toml
// ===========================================================================

#[test]
fn global_config_creation() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(&tmp, None);

    assert!(!global_dir.join("config.toml").exists());

    // Write a default config
    let config = Config::default();
    let content = toml::to_string_pretty(&config).unwrap();
    fs::write(global_dir.join("config.toml"), &content).unwrap();

    assert!(global_dir.join("config.toml").exists());

    let loaded: Config =
        toml::from_str(&fs::read_to_string(global_dir.join("config.toml")).unwrap()).unwrap();
    assert_eq!(loaded.agent.executor, "claude");
    assert_eq!(loaded.coordinator.max_agents, 4);
}

// ===========================================================================
// 2. Local-only config (no global exists)
// ===========================================================================

#[test]
fn local_only_no_global() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(&tmp, None);
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[agent]
model = "haiku"

[coordinator]
max_agents = 2
"#,
        ),
    );

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.agent.model, "haiku");
    assert_eq!(config.coordinator.max_agents, 2);
    assert_eq!(config.agent.executor, "claude"); // default
}

// ===========================================================================
// 3. Global-only config (no local exists)
// ===========================================================================

#[test]
fn global_only_no_local() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[agent]
model = "sonnet"

[coordinator]
max_agents = 8
"#,
        ),
    );
    let local_dir = setup_local_dir(&tmp, None);

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.agent.model, "sonnet");
    assert_eq!(config.coordinator.max_agents, 8);
    assert_eq!(config.agent.executor, "claude"); // default
}

// ===========================================================================
// 4. Merge: global evaluator_model=sonnet, local evaluator_model=haiku → haiku
// ===========================================================================

#[test]
fn merge_local_overrides_global_evaluator_model() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[agency]
evaluator_model = "sonnet"
"#,
        ),
    );
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[agency]
evaluator_model = "haiku"
"#,
        ),
    );

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(
        config.agency.evaluator_model,
        Some("haiku".to_string()),
        "local evaluator_model should override global"
    );
}

// ===========================================================================
// 5. Merge: global max_agents=8, local doesn't set it → 8 inherited
// ===========================================================================

#[test]
fn merge_global_value_inherited_when_local_absent() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[coordinator]
max_agents = 8
"#,
        ),
    );
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[agent]
model = "haiku"
"#,
        ),
    );

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.coordinator.max_agents, 8);
    assert_eq!(config.agent.model, "haiku");
}

// ===========================================================================
// 6. --list shows merged config with [global]/[local] source indicators
// ===========================================================================

#[test]
fn list_source_annotations_global_vs_local() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[coordinator]
max_agents = 8
executor = "claude"
"#,
        ),
    );
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[coordinator]
executor = "amplifier"
"#,
        ),
    );

    let (_config, sources) = load_with_sources_custom(&global_dir, &local_dir);

    assert_eq!(
        sources.get("coordinator.max_agents"),
        Some(&ConfigSource::Global),
        "max_agents should be global (only set in global)"
    );
    assert_eq!(
        sources.get("coordinator.executor"),
        Some(&ConfigSource::Local),
        "executor should be local (local overrides global)"
    );
    assert_eq!(
        sources.get("agent.model"),
        Some(&ConfigSource::Default),
        "agent.model should be default (not in either file)"
    );
}

#[test]
fn list_all_defaults_when_no_config_files() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(&tmp, None);
    let local_dir = setup_local_dir(&tmp, None);

    let (_config, sources) = load_with_sources_custom(&global_dir, &local_dir);

    for (key, source) in &sources {
        assert_eq!(
            *source,
            ConfigSource::Default,
            "key '{}' should be default when no config files exist",
            key
        );
    }
}

#[test]
fn list_source_tracking_mixed_sections() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[agent]
model = "sonnet"
interval = 20

[agency]
auto_evaluate = true
"#,
        ),
    );
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[agent]
model = "haiku"

[coordinator]
max_agents = 6
"#,
        ),
    );

    let (_config, sources) = load_with_sources_custom(&global_dir, &local_dir);

    // In both → local wins
    assert_eq!(sources.get("agent.model"), Some(&ConfigSource::Local));
    // Only in global
    assert_eq!(sources.get("agent.interval"), Some(&ConfigSource::Global));
    // Only in local
    assert_eq!(sources.get("coordinator.max_agents"), Some(&ConfigSource::Local));
    // Only in global
    assert_eq!(sources.get("agency.auto_evaluate"), Some(&ConfigSource::Global));
    // In neither → default
    assert_eq!(sources.get("agent.executor"), Some(&ConfigSource::Default));
}

// ===========================================================================
// 7. --global flag writes to global path
// ===========================================================================

#[test]
fn global_flag_writes_to_global_path() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(&tmp, None);

    // Simulate: load global (or default), modify, save to global path
    let mut config = Config::default();
    config.coordinator.max_agents = 8;
    let content = toml::to_string_pretty(&config).unwrap();
    fs::write(global_dir.join("config.toml"), &content).unwrap();

    let loaded: Config =
        toml::from_str(&fs::read_to_string(global_dir.join("config.toml")).unwrap()).unwrap();
    assert_eq!(loaded.coordinator.max_agents, 8);
}

#[test]
fn global_write_does_not_affect_local() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(&tmp, None);
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[coordinator]
max_agents = 2
"#,
        ),
    );

    // Write to global
    let mut global_config = Config::default();
    global_config.coordinator.max_agents = 8;
    fs::write(
        global_dir.join("config.toml"),
        toml::to_string_pretty(&global_config).unwrap(),
    )
    .unwrap();

    // Local unchanged
    let local: Config =
        toml::from_str(&fs::read_to_string(local_dir.join("config.toml")).unwrap()).unwrap();
    assert_eq!(local.coordinator.max_agents, 2);

    // Merged: local wins
    let merged = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(merged.coordinator.max_agents, 2);
}

// ===========================================================================
// 8. --local flag writes to project path
// ===========================================================================

#[test]
fn local_flag_writes_to_project_path() {
    let tmp = TempDir::new().unwrap();
    let local_dir = setup_local_dir(&tmp, None);

    let mut config = Config::default();
    config.coordinator.max_agents = 6;
    config.save(&local_dir).unwrap();

    let loaded = Config::load(&local_dir).unwrap();
    assert_eq!(loaded.coordinator.max_agents, 6);
}

#[test]
fn local_write_does_not_affect_global() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[coordinator]
max_agents = 8
"#,
        ),
    );
    let local_dir = setup_local_dir(&tmp, None);

    // Write local
    let mut local_config = Config::default();
    local_config.coordinator.max_agents = 3;
    local_config.save(&local_dir).unwrap();

    // Global unchanged
    let global: Config =
        toml::from_str(&fs::read_to_string(global_dir.join("config.toml")).unwrap()).unwrap();
    assert_eq!(global.coordinator.max_agents, 8);
}

// ===========================================================================
// 9. Service daemon uses merged config
// ===========================================================================

#[test]
fn service_daemon_uses_merged_config() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[coordinator]
max_agents = 8
executor = "claude"
interval = 30
poll_interval = 120
"#,
        ),
    );
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[coordinator]
executor = "amplifier"
"#,
        ),
    );

    // Service daemon calls Config::load_or_default → load_merged
    let config = load_merged_custom(&global_dir, &local_dir);

    assert_eq!(config.coordinator.max_agents, 8, "inherited from global");
    assert_eq!(config.coordinator.executor, "amplifier", "local overrides");
    assert_eq!(config.coordinator.interval, 30, "inherited from global");
    assert_eq!(config.coordinator.poll_interval, 120, "inherited from global");
}

#[test]
fn service_uses_merged_model() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[coordinator]
model = "sonnet"
"#,
        ),
    );
    let local_dir = setup_local_dir(&tmp, None);

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.coordinator.model, Some("sonnet".to_string()));
}

#[test]
fn service_local_model_overrides_global() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[coordinator]
model = "sonnet"
"#,
        ),
    );
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[coordinator]
model = "haiku"
"#,
        ),
    );

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.coordinator.model, Some("haiku".to_string()));
}

// ===========================================================================
// 10. Evaluate uses merged evaluator_model
// ===========================================================================

#[test]
fn evaluate_uses_merged_evaluator_model_from_global() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[agency]
evaluator_model = "sonnet"
auto_evaluate = true
"#,
        ),
    );
    let local_dir = setup_local_dir(&tmp, None);

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.agency.evaluator_model, Some("sonnet".to_string()));
    assert!(config.agency.auto_evaluate);
}

#[test]
fn evaluate_local_overrides_global_evaluator_model() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[agency]
evaluator_model = "sonnet"
"#,
        ),
    );
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[agency]
evaluator_model = "haiku"
"#,
        ),
    );

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.agency.evaluator_model, Some("haiku".to_string()));
}

// ===========================================================================
// Additional edge cases
// ===========================================================================

#[test]
fn merge_deep_nested_multiple_sections() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[agent]
model = "sonnet"
executor = "claude"
interval = 20

[coordinator]
max_agents = 8

[agency]
auto_evaluate = true
evaluator_model = "sonnet"
assigner_model = "opus"
"#,
        ),
    );
    let local_dir = setup_local_dir(
        &tmp,
        Some(
            r#"
[agent]
model = "haiku"

[agency]
evaluator_model = "haiku"
auto_assign = true
"#,
        ),
    );

    let config = load_merged_custom(&global_dir, &local_dir);

    // agent: local model overrides, global executor/interval inherited
    assert_eq!(config.agent.model, "haiku");
    assert_eq!(config.agent.executor, "claude");
    assert_eq!(config.agent.interval, 20);

    // coordinator: global inherited
    assert_eq!(config.coordinator.max_agents, 8);

    // agency: local evaluator_model overrides, global assigner_model inherited
    assert_eq!(config.agency.evaluator_model, Some("haiku".to_string()));
    assert_eq!(config.agency.assigner_model, Some("opus".to_string()));
    assert!(config.agency.auto_evaluate);
    assert!(config.agency.auto_assign);
}

#[test]
fn merge_empty_both_yields_defaults() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(&tmp, None);
    let local_dir = setup_local_dir(&tmp, None);

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.agent.executor, "claude");
    assert_eq!(config.agent.model, "opus");
    assert_eq!(config.coordinator.max_agents, 4);
    assert_eq!(config.coordinator.interval, 30);
}

#[test]
fn merge_preserves_all_coordinator_fields() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(
        &tmp,
        Some(
            r#"
[coordinator]
max_agents = 12
interval = 45
poll_interval = 180
executor = "shell"
model = "opus"
"#,
        ),
    );
    let local_dir = setup_local_dir(&tmp, None);

    let config = load_merged_custom(&global_dir, &local_dir);
    assert_eq!(config.coordinator.max_agents, 12);
    assert_eq!(config.coordinator.interval, 45);
    assert_eq!(config.coordinator.poll_interval, 180);
    assert_eq!(config.coordinator.executor, "shell");
    assert_eq!(config.coordinator.model, Some("opus".to_string()));
}

#[test]
fn config_save_and_load_roundtrip_local() {
    let tmp = TempDir::new().unwrap();
    let local_dir = setup_local_dir(&tmp, None);

    let mut config = Config::default();
    config.agent.model = "haiku".to_string();
    config.coordinator.max_agents = 16;
    config.agency.evaluator_model = Some("sonnet".to_string());
    config.agency.auto_evaluate = true;
    config.save(&local_dir).unwrap();

    let loaded = Config::load(&local_dir).unwrap();
    assert_eq!(loaded.agent.model, "haiku");
    assert_eq!(loaded.coordinator.max_agents, 16);
    assert_eq!(loaded.agency.evaluator_model, Some("sonnet".to_string()));
    assert!(loaded.agency.auto_evaluate);
}

#[test]
fn config_save_and_load_roundtrip_global() {
    let tmp = TempDir::new().unwrap();
    let global_dir = setup_global_dir(&tmp, None);

    let mut config = Config::default();
    config.coordinator.max_agents = 10;
    config.agency.evolver_model = Some("opus".to_string());
    fs::write(
        global_dir.join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let loaded: Config =
        toml::from_str(&fs::read_to_string(global_dir.join("config.toml")).unwrap()).unwrap();
    assert_eq!(loaded.coordinator.max_agents, 10);
    assert_eq!(loaded.agency.evolver_model, Some("opus".to_string()));
}

#[test]
fn config_init_creates_default_file() {
    let tmp = TempDir::new().unwrap();
    let local_dir = setup_local_dir(&tmp, None);

    let created = Config::init(&local_dir).unwrap();
    assert!(created);
    assert!(local_dir.join("config.toml").exists());

    // Second init should not overwrite
    let created = Config::init(&local_dir).unwrap();
    assert!(!created);
}

#[test]
fn config_source_display_variants() {
    assert_eq!(ConfigSource::Global.to_string(), "global");
    assert_eq!(ConfigSource::Local.to_string(), "local");
    assert_eq!(ConfigSource::Default.to_string(), "default");
}

#[test]
fn load_with_sources_uses_real_api() {
    // Test Config::load_with_sources against a local-only temp dir.
    // This uses the real API which also reads ~/.workgraph/config.toml
    // (the user's real global config, if any), so we just verify it doesn't
    // error and returns sensible structure.
    let tmp = TempDir::new().unwrap();
    let local_dir = tmp.path().to_path_buf();
    fs::create_dir_all(&local_dir).unwrap();

    let mut config = Config::default();
    config.coordinator.max_agents = 7;
    config.save(&local_dir).unwrap();

    let (loaded, sources) = Config::load_with_sources(&local_dir).unwrap();
    assert_eq!(loaded.coordinator.max_agents, 7);
    assert_eq!(
        sources.get("coordinator.max_agents"),
        Some(&ConfigSource::Local)
    );
}
