//! Bundle system for the native executor.
//!
//! Bundles define what tools and context an agent gets when running with the
//! native executor. They map to exec_mode tiers (bare/light/full) and can be
//! loaded from `.workgraph/bundles/*.toml` files.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::tools::ToolRegistry;

/// A bundle configuration that defines agent capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    /// Bundle name (e.g., "research", "implementer", "bare").
    pub name: String,
    /// Human-readable description of what this bundle provides.
    #[serde(default)]
    pub description: String,
    /// Tool names this bundle allows. Use `["*"]` for all tools.
    pub tools: Vec<String>,
    /// Context scope for prompt assembly: "clean", "task", "graph", "full".
    #[serde(default = "default_context_scope")]
    pub context_scope: String,
    /// Additional text appended to the system prompt for agents using this bundle.
    #[serde(default)]
    pub system_prompt_suffix: String,
}

fn default_context_scope() -> String {
    "task".to_string()
}

impl Bundle {
    /// Load a bundle from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("Failed to read {:?}", path))?;
        let bundle: Bundle =
            toml::from_str(&content).with_context(|| format!("Failed to parse {:?}", path))?;
        Ok(bundle)
    }

    /// Check if this bundle allows all tools (wildcard).
    pub fn allows_all(&self) -> bool {
        self.tools.iter().any(|t| t == "*")
    }

    /// Apply this bundle's tool filtering to a ToolRegistry.
    ///
    /// If the bundle has a wildcard (`*`), the registry is returned unchanged.
    /// Otherwise, only tools listed in the bundle are kept.
    pub fn filter_registry(&self, registry: ToolRegistry) -> ToolRegistry {
        if self.allows_all() {
            registry
        } else {
            registry.filter(&self.tools)
        }
    }

    /// Built-in "bare" bundle: only workgraph CLI tools.
    pub fn bare() -> Self {
        Bundle {
            name: "bare".to_string(),
            description: "Minimal wg-only agent for synthesis and triage tasks.".to_string(),
            tools: vec![
                "wg_show".to_string(),
                "wg_list".to_string(),
                "wg_add".to_string(),
                "wg_done".to_string(),
                "wg_fail".to_string(),
                "wg_log".to_string(),
                "wg_artifact".to_string(),
            ],
            context_scope: "task".to_string(),
            system_prompt_suffix:
                "You are a lightweight agent. Use only wg tools to inspect and manage tasks."
                    .to_string(),
        }
    }

    /// Built-in "research" bundle: read-only file access + wg tools + bg + web (no delegate).
    pub fn research() -> Self {
        Bundle {
            name: "research".to_string(),
            description: "Read-only research agent with background tasks and web access."
                .to_string(),
            tools: vec![
                "read_file".to_string(),
                "glob".to_string(),
                "grep".to_string(),
                "bash".to_string(),
                "bg".to_string(),
                "web_search".to_string(),
                "web_fetch".to_string(),
                "wg_show".to_string(),
                "wg_list".to_string(),
                "wg_add".to_string(),
                "wg_done".to_string(),
                "wg_fail".to_string(),
                "wg_log".to_string(),
                "wg_artifact".to_string(),
            ],
            context_scope: "graph".to_string(),
            system_prompt_suffix: "You are a research agent. Report findings, do not modify files."
                .to_string(),
        }
    }

    /// Built-in "shell" bundle: bash + bg + wg tools only.
    pub fn shell() -> Self {
        Bundle {
            name: "shell".to_string(),
            description: "Shell executor agent with bash and background tasks.".to_string(),
            tools: vec![
                "bash".to_string(),
                "bg".to_string(),
                "wg_show".to_string(),
                "wg_list".to_string(),
                "wg_add".to_string(),
                "wg_done".to_string(),
                "wg_fail".to_string(),
                "wg_log".to_string(),
                "wg_artifact".to_string(),
            ],
            context_scope: "task".to_string(),
            system_prompt_suffix: "You are a shell agent. Use bash and bg for task execution."
                .to_string(),
        }
    }

    /// Built-in "implementer" bundle: all tools, no filtering.
    pub fn implementer() -> Self {
        Bundle {
            name: "implementer".to_string(),
            description: "Full implementation agent with all tools.".to_string(),
            tools: vec!["*".to_string()],
            context_scope: "full".to_string(),
            system_prompt_suffix: String::new(),
        }
    }
}

/// Resolve a bundle for the given exec_mode.
///
/// Resolution order:
/// 1. Look for a bundle TOML file matching the exec_mode mapping in the bundles directory.
/// 2. Fall back to built-in defaults.
///
/// Exec mode → bundle mapping:
/// - "shell" → shell bundle (bash + bg + wg)
/// - "bare"  → bare bundle (wg tools only)
/// - "light" → research bundle (read-only + bg + web + wg)
/// - "full"  → implementer bundle (all tools)
pub fn resolve_bundle(exec_mode: &str, workgraph_dir: &Path) -> Option<Bundle> {
    let bundle_name = match exec_mode {
        "shell" => "shell",
        "bare" => "bare",
        "light" => "research",
        "full" => "implementer",
        other => other, // Allow custom exec modes to map to custom bundles
    };

    let bundles_dir = workgraph_dir.join("bundles");

    // Try loading from file first
    let bundle_path = bundles_dir.join(format!("{}.toml", bundle_name));
    if bundle_path.exists() {
        match Bundle::load(&bundle_path) {
            Ok(bundle) => return Some(bundle),
            Err(e) => {
                eprintln!(
                    "[native-executor] Warning: failed to load bundle {:?}: {}",
                    bundle_path, e
                );
                // Fall through to built-in default
            }
        }
    }

    // Fall back to built-in defaults
    match bundle_name {
        "bare" => Some(Bundle::bare()),
        "shell" => Some(Bundle::shell()),
        "research" => Some(Bundle::research()),
        "implementer" => Some(Bundle::implementer()),
        _ => {
            eprintln!(
                "[native-executor] Warning: no bundle found for exec_mode '{}', using full access",
                exec_mode
            );
            Some(Bundle::implementer())
        }
    }
}

/// Load all bundles from the bundles directory.
pub fn load_all_bundles(workgraph_dir: &Path) -> Vec<Bundle> {
    let bundles_dir = workgraph_dir.join("bundles");
    if !bundles_dir.exists() {
        return Vec::new();
    }

    let mut bundles = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&bundles_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                match Bundle::load(&path) {
                    Ok(bundle) => bundles.push(bundle),
                    Err(e) => {
                        eprintln!(
                            "[native-executor] Warning: failed to load bundle {:?}: {}",
                            path, e
                        );
                    }
                }
            }
        }
    }
    bundles
}

/// Ensure default bundle files exist in the bundles directory.
pub fn ensure_default_bundles(workgraph_dir: &Path) -> Result<()> {
    let bundles_dir = workgraph_dir.join("bundles");
    if !bundles_dir.exists() {
        std::fs::create_dir_all(&bundles_dir)
            .with_context(|| format!("Failed to create bundles directory: {:?}", bundles_dir))?;
    }

    let defaults = [
        ("bare.toml", Bundle::bare()),
        ("shell.toml", Bundle::shell()),
        ("research.toml", Bundle::research()),
        ("implementer.toml", Bundle::implementer()),
    ];

    for (filename, bundle) in &defaults {
        let path = bundles_dir.join(filename);
        if !path.exists() {
            let content = toml::to_string_pretty(bundle)
                .with_context(|| format!("Failed to serialize bundle {:?}", filename))?;
            std::fs::write(&path, content)
                .with_context(|| format!("Failed to write bundle {:?}", path))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_bundle_bare() {
        let bundle = Bundle::bare();
        assert_eq!(bundle.name, "bare");
        assert!(!bundle.allows_all());
        assert!(bundle.tools.contains(&"wg_show".to_string()));
        assert!(bundle.tools.contains(&"wg_done".to_string()));
        assert!(!bundle.tools.contains(&"read_file".to_string()));
    }

    #[test]
    fn test_bundle_research() {
        let bundle = Bundle::research();
        assert_eq!(bundle.name, "research");
        assert!(!bundle.allows_all());
        assert!(bundle.tools.contains(&"read_file".to_string()));
        assert!(bundle.tools.contains(&"grep".to_string()));
        assert!(bundle.tools.contains(&"wg_show".to_string()));
        assert!(!bundle.tools.contains(&"write_file".to_string()));
    }

    #[test]
    fn test_bundle_implementer() {
        let bundle = Bundle::implementer();
        assert_eq!(bundle.name, "implementer");
        assert!(bundle.allows_all());
    }

    #[test]
    fn test_resolve_bundle_shell() {
        let tmp = TempDir::new().unwrap();
        let bundle = resolve_bundle("shell", tmp.path()).unwrap();
        assert_eq!(bundle.name, "shell");
        assert!(bundle.tools.contains(&"bash".to_string()));
        assert!(bundle.tools.contains(&"bg".to_string()));
        assert!(!bundle.tools.contains(&"read_file".to_string()));
        assert!(!bundle.tools.contains(&"web_search".to_string()));
        assert!(!bundle.tools.contains(&"delegate".to_string()));
    }

    #[test]
    fn test_resolve_bundle_bare() {
        let tmp = TempDir::new().unwrap();
        let bundle = resolve_bundle("bare", tmp.path()).unwrap();
        assert_eq!(bundle.name, "bare");
    }

    #[test]
    fn test_resolve_bundle_light() {
        let tmp = TempDir::new().unwrap();
        let bundle = resolve_bundle("light", tmp.path()).unwrap();
        assert_eq!(bundle.name, "research");
    }

    #[test]
    fn test_resolve_bundle_full() {
        let tmp = TempDir::new().unwrap();
        let bundle = resolve_bundle("full", tmp.path()).unwrap();
        assert_eq!(bundle.name, "implementer");
        assert!(bundle.allows_all());
    }

    #[test]
    fn test_load_bundle_from_toml() {
        let tmp = TempDir::new().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(&bundles_dir).unwrap();

        let content = r#"
name = "custom"
description = "Custom test bundle"
tools = ["read_file", "wg_show"]
context_scope = "task"
system_prompt_suffix = "You are a custom agent."
"#;
        std::fs::write(bundles_dir.join("custom.toml"), content).unwrap();

        let bundle = Bundle::load(&bundles_dir.join("custom.toml")).unwrap();
        assert_eq!(bundle.name, "custom");
        assert_eq!(bundle.tools, vec!["read_file", "wg_show"]);
        assert_eq!(bundle.context_scope, "task");
        assert_eq!(bundle.system_prompt_suffix, "You are a custom agent.");
    }

    #[test]
    fn test_resolve_bundle_from_file() {
        let tmp = TempDir::new().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(&bundles_dir).unwrap();

        // Override the research bundle with a custom one
        let content = r#"
name = "research"
description = "Custom research bundle"
tools = ["read_file", "grep"]
context_scope = "graph"
"#;
        std::fs::write(bundles_dir.join("research.toml"), content).unwrap();

        let bundle = resolve_bundle("light", tmp.path()).unwrap();
        assert_eq!(bundle.name, "research");
        // Should use the file version, which only has 2 tools
        assert_eq!(bundle.tools.len(), 2);
    }

    #[test]
    fn test_ensure_default_bundles() {
        let tmp = TempDir::new().unwrap();
        ensure_default_bundles(tmp.path()).unwrap();

        let bundles_dir = tmp.path().join("bundles");
        assert!(bundles_dir.join("bare.toml").exists());
        assert!(bundles_dir.join("shell.toml").exists());
        assert!(bundles_dir.join("research.toml").exists());
        assert!(bundles_dir.join("implementer.toml").exists());

        // Should be parseable
        let bare = Bundle::load(&bundles_dir.join("bare.toml")).unwrap();
        assert_eq!(bare.name, "bare");
    }

    #[test]
    fn test_ensure_default_bundles_idempotent() {
        let tmp = TempDir::new().unwrap();
        ensure_default_bundles(tmp.path()).unwrap();
        // Write custom content to one file
        std::fs::write(
            tmp.path().join("bundles/bare.toml"),
            "name = \"bare\"\ntools = [\"wg_done\"]\n",
        )
        .unwrap();
        // Second call should not overwrite
        ensure_default_bundles(tmp.path()).unwrap();
        let bare = Bundle::load(&tmp.path().join("bundles/bare.toml")).unwrap();
        assert_eq!(bare.tools, vec!["wg_done"]); // Custom content preserved
    }

    #[test]
    fn test_filter_registry() {
        let tmp = TempDir::new().unwrap();
        let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
        let defs_before = registry.definitions().len();
        assert!(defs_before > 0);

        let bundle = Bundle::bare();
        let filtered = bundle.filter_registry(registry);
        let defs_after = filtered.definitions().len();
        assert!(defs_after < defs_before);
        // Should only have wg tools
        for def in filtered.definitions() {
            assert!(
                def.name.starts_with("wg_"),
                "Expected wg tool, got: {}",
                def.name
            );
        }
    }

    #[test]
    fn test_filter_registry_wildcard() {
        let tmp = TempDir::new().unwrap();
        let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
        let defs_before = registry.definitions().len();

        let bundle = Bundle::implementer();
        let filtered = bundle.filter_registry(registry);
        assert_eq!(filtered.definitions().len(), defs_before);
    }

    #[test]
    fn test_load_all_bundles() {
        let tmp = TempDir::new().unwrap();
        ensure_default_bundles(tmp.path()).unwrap();

        let bundles = load_all_bundles(tmp.path());
        assert_eq!(bundles.len(), 4);
    }

    #[test]
    fn test_load_all_bundles_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let bundles = load_all_bundles(tmp.path());
        assert!(bundles.is_empty());
    }

    #[test]
    fn test_bundle_research_has_bg_and_web() {
        let bundle = Bundle::research();
        assert!(
            bundle.tools.contains(&"bg".to_string()),
            "research should include bg"
        );
        assert!(
            bundle.tools.contains(&"web_search".to_string()),
            "research should include web_search"
        );
        assert!(
            bundle.tools.contains(&"web_fetch".to_string()),
            "research should include web_fetch"
        );
        assert!(
            !bundle.tools.contains(&"delegate".to_string()),
            "research should NOT include delegate"
        );
    }

    #[test]
    fn test_bundle_shell_has_bg_only() {
        let bundle = Bundle::shell();
        assert!(
            bundle.tools.contains(&"bash".to_string()),
            "shell should include bash"
        );
        assert!(
            bundle.tools.contains(&"bg".to_string()),
            "shell should include bg"
        );
        assert!(
            !bundle.tools.contains(&"web_search".to_string()),
            "shell should NOT include web_search"
        );
        assert!(
            !bundle.tools.contains(&"web_fetch".to_string()),
            "shell should NOT include web_fetch"
        );
        assert!(
            !bundle.tools.contains(&"delegate".to_string()),
            "shell should NOT include delegate"
        );
    }

    #[test]
    fn test_bundle_bare_no_new_tools() {
        let bundle = Bundle::bare();
        assert!(
            !bundle.tools.contains(&"bg".to_string()),
            "bare should NOT include bg"
        );
        assert!(
            !bundle.tools.contains(&"web_search".to_string()),
            "bare should NOT include web_search"
        );
        assert!(
            !bundle.tools.contains(&"web_fetch".to_string()),
            "bare should NOT include web_fetch"
        );
        assert!(
            !bundle.tools.contains(&"delegate".to_string()),
            "bare should NOT include delegate"
        );
    }

    #[test]
    fn test_bundle_implementer_allows_all() {
        let bundle = Bundle::implementer();
        // Implementer uses wildcard so all tools (bg, web, delegate) are included
        assert!(bundle.allows_all());
    }

    #[test]
    fn test_filter_registry_research_includes_bg_web() {
        let tmp = TempDir::new().unwrap();
        let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
        let bundle = Bundle::research();
        let filtered = bundle.filter_registry(registry);
        let names: Vec<String> = filtered
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert!(
            names.contains(&"bg".to_string()),
            "filtered research should have bg"
        );
        assert!(
            names.contains(&"web_search".to_string()),
            "filtered research should have web_search"
        );
        assert!(
            names.contains(&"web_fetch".to_string()),
            "filtered research should have web_fetch"
        );
        assert!(
            !names.contains(&"delegate".to_string()),
            "filtered research should NOT have delegate"
        );
    }

    #[test]
    fn test_filter_registry_shell_includes_bg() {
        let tmp = TempDir::new().unwrap();
        let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
        let bundle = Bundle::shell();
        let filtered = bundle.filter_registry(registry);
        let names: Vec<String> = filtered
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert!(
            names.contains(&"bash".to_string()),
            "filtered shell should have bash"
        );
        assert!(
            names.contains(&"bg".to_string()),
            "filtered shell should have bg"
        );
        assert!(
            !names.contains(&"web_search".to_string()),
            "filtered shell should NOT have web_search"
        );
        assert!(
            !names.contains(&"delegate".to_string()),
            "filtered shell should NOT have delegate"
        );
    }

    #[test]
    fn test_filter_registry_bare_no_new_tools() {
        let tmp = TempDir::new().unwrap();
        let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
        let bundle = Bundle::bare();
        let filtered = bundle.filter_registry(registry);
        let names: Vec<String> = filtered
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert!(
            !names.contains(&"bg".to_string()),
            "filtered bare should NOT have bg"
        );
        assert!(
            !names.contains(&"web_search".to_string()),
            "filtered bare should NOT have web_search"
        );
        assert!(
            !names.contains(&"delegate".to_string()),
            "filtered bare should NOT have delegate"
        );
    }
}
