//! Tests that the legacy `[coordinator] poll_interval` key continues to be
//! accepted by the deserializer (via serde alias on `safety_interval`) and
//! that `detect_deprecated_keys` surfaces it for a one-shot daemon-side
//! deprecation warning.

use workgraph::config::{Config, detect_deprecated_keys};

#[test]
fn legacy_poll_interval_still_loads() {
    // Older configs use `poll_interval`. We must still accept the value.
    let toml_str = r#"
[coordinator]
poll_interval = 17
"#;
    let cfg: Config = toml::from_str(toml_str).expect("legacy poll_interval must deserialize");
    assert_eq!(
        cfg.coordinator.poll_interval, 17,
        "legacy poll_interval value must be preserved"
    );
}

#[test]
fn new_safety_interval_alias_loads() {
    // New configs may use `safety_interval` as the canonical key.
    let toml_str = r#"
[coordinator]
safety_interval = 23
"#;
    let cfg: Config = toml::from_str(toml_str).expect("safety_interval alias must deserialize");
    assert_eq!(
        cfg.coordinator.poll_interval, 23,
        "safety_interval value must be honored (it aliases poll_interval)"
    );
}

#[test]
fn detect_deprecated_keys_flags_legacy_poll_interval() {
    let toml_str = r#"
[coordinator]
poll_interval = 17
"#;
    let val: toml::Value = toml::from_str(toml_str).unwrap();
    let deprecated = detect_deprecated_keys(&val);
    assert_eq!(deprecated.len(), 1, "expected one deprecation hit");
    assert_eq!(deprecated[0].path, "coordinator.poll_interval");
    assert_eq!(deprecated[0].replacement, "coordinator.safety_interval");
}

#[test]
fn detect_deprecated_keys_silent_when_using_new_name() {
    let toml_str = r#"
[coordinator]
safety_interval = 23
"#;
    let val: toml::Value = toml::from_str(toml_str).unwrap();
    assert!(
        detect_deprecated_keys(&val).is_empty(),
        "no deprecation when only the new key is used"
    );
}

#[test]
fn detect_deprecated_keys_works_for_dispatcher_section() {
    let toml_str = r#"
[dispatcher]
poll_interval = 5
"#;
    let val: toml::Value = toml::from_str(toml_str).unwrap();
    let deprecated = detect_deprecated_keys(&val);
    assert_eq!(deprecated.len(), 1);
    assert_eq!(deprecated[0].path, "dispatcher.poll_interval");
}

#[test]
fn default_safety_interval_is_30_seconds() {
    let cfg = Config::default();
    assert_eq!(
        cfg.coordinator.poll_interval, 30,
        "Default safety timer interval should be 30s (task spec)"
    );
}

#[test]
fn graph_watch_enabled_by_default() {
    let cfg = Config::default();
    assert!(
        cfg.coordinator.graph_watch_enabled,
        "Graph filesystem watching should be enabled by default"
    );
    assert!(
        cfg.coordinator.graph_watch_debounce_ms >= 50,
        "Debounce should default to a sane value (>= 50ms), got {}",
        cfg.coordinator.graph_watch_debounce_ms
    );
    assert!(
        cfg.coordinator.graph_watch_debounce_ms <= 200,
        "Debounce should default to a sane value (<= 200ms), got {}",
        cfg.coordinator.graph_watch_debounce_ms
    );
}
