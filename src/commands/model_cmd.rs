//! `wg model` — Model registry and routing management.
//!
//! Wraps existing config_cmd operations with a simplified, discoverable CLI.

use anyhow::Result;
use std::path::Path;

use super::config_cmd::{self, ConfigScope};

/// `wg model list` — show all models in the effective registry.
pub fn run_list(dir: &Path, tier: Option<&str>, json: bool) -> Result<()> {
    if let Some(tier_str) = tier {
        // Validate tier string early
        let _: workgraph::config::Tier = tier_str.parse()?;
    }

    let config = workgraph::config::Config::load_merged(dir)?;
    let entries = config.effective_registry();

    let filtered: Vec<_> = if let Some(tier_str) = tier {
        let tier: workgraph::config::Tier = tier_str.parse()?;
        entries.into_iter().filter(|e| e.tier == tier).collect()
    } else {
        entries
    };

    if json {
        let val: Vec<serde_json::Value> = filtered
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "provider": e.provider,
                    "model": e.model,
                    "tier": e.tier.to_string(),
                    "endpoint": e.endpoint,
                    "context_window": e.context_window,
                    "cost_per_input_mtok": e.cost_per_input_mtok,
                    "cost_per_output_mtok": e.cost_per_output_mtok,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    if filtered.is_empty() {
        println!("No model registry entries.");
        return Ok(());
    }

    // Determine the default model
    let default_model = config
        .models
        .get_role(workgraph::config::DispatchRole::Default)
        .and_then(|r| r.model.as_deref());

    println!(
        "  {:<12} {:<12} {:<30} {:<10} COST (in/out per MTok)",
        "ID", "PROVIDER", "MODEL", "TIER"
    );
    println!("  {}", "-".repeat(85));

    for entry in &filtered {
        let is_default = default_model == Some(&entry.id);
        let marker = if is_default { "*" } else { " " };
        let cost = if entry.cost_per_input_mtok > 0.0 || entry.cost_per_output_mtok > 0.0 {
            format!(
                "${:.2}/${:.2}",
                entry.cost_per_input_mtok, entry.cost_per_output_mtok
            )
        } else {
            "-".to_string()
        };
        println!(
            "{} {:<12} {:<12} {:<30} {:<10} {}",
            marker, entry.id, entry.provider, entry.model, entry.tier, cost,
        );
    }

    if let Some(dm) = default_model {
        println!("\n  * = default model ({})", dm);
    }

    Ok(())
}

/// `wg model add <alias>` — add or update a registry entry.
#[allow(clippy::too_many_arguments)]
pub fn run_add(
    dir: &Path,
    alias: &str,
    provider: &str,
    model_id: Option<&str>,
    tier: &str,
    endpoint: Option<&str>,
    context_window: Option<u64>,
    cost_in: Option<f64>,
    cost_out: Option<f64>,
    global: bool,
) -> Result<()> {
    let scope = if global {
        ConfigScope::Global
    } else {
        ConfigScope::Local
    };
    // model_id defaults to the alias if not specified
    let model = model_id.unwrap_or(alias);
    config_cmd::add_registry_entry(
        dir,
        scope,
        alias,
        provider,
        model,
        tier,
        endpoint,
        context_window,
        cost_in,
        cost_out,
    )
}

/// `wg model remove <alias>` — remove a registry entry.
pub fn run_remove(dir: &Path, alias: &str, force: bool, global: bool, json: bool) -> Result<()> {
    let scope = if global {
        ConfigScope::Global
    } else {
        ConfigScope::Local
    };
    config_cmd::remove_registry_entry(dir, scope, alias, force, json)
}

/// `wg model set-default <alias>` — set the default model for agent dispatch.
pub fn run_set_default(dir: &Path, alias: &str, global: bool) -> Result<()> {
    use workgraph::config::{Config, DispatchRole};

    let scope = if global {
        ConfigScope::Global
    } else {
        ConfigScope::Local
    };

    // Validate alias exists in the effective registry
    let merged = Config::load_merged(dir)?;
    if merged.registry_lookup(alias).is_none() {
        anyhow::bail!(
            "Model '{}' not found in the registry. Add it first with: wg model add {} --provider <provider>",
            alias,
            alias,
        );
    }

    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(dir)?,
    };

    config.models.set_model(DispatchRole::Default, alias);

    match scope {
        ConfigScope::Global => config.save_global()?,
        ConfigScope::Local => config.save(dir)?,
    }

    println!("Set default model to: {}", alias);
    Ok(())
}

/// `wg model routing` — show per-role model routing.
pub fn run_routing(dir: &Path, json: bool) -> Result<()> {
    config_cmd::show_model_routing(dir, json)
}

/// `wg model set <role> <model>` — set model for a dispatch role.
pub fn run_set(
    dir: &Path,
    role: &str,
    model: &str,
    provider: Option<&str>,
    endpoint: Option<&str>,
    global: bool,
) -> Result<()> {
    let scope = if global {
        ConfigScope::Global
    } else {
        ConfigScope::Local
    };

    let set_model_args = vec![role.to_string(), model.to_string()];
    let set_provider_args = provider.map(|p| vec![role.to_string(), p.to_string()]);
    let set_endpoint_args = endpoint.map(|e| vec![role.to_string(), e.to_string()]);

    config_cmd::update_model_routing(
        dir,
        scope,
        Some(&set_model_args),
        set_provider_args.as_deref(),
        set_endpoint_args.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::config::Config;
    use workgraph::parser::save_graph;

    fn setup_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();
        // Create empty graph
        let graph_path = dir.join("graph.jsonl");
        let graph = workgraph::graph::WorkGraph::new();
        save_graph(&graph, &graph_path).unwrap();
        // Create default config
        let config = Config::default();
        config.save(dir).unwrap();
        tmp
    }

    #[test]
    fn test_model_add_and_list() {
        let tmp = setup_dir();
        let dir = tmp.path();

        // Add a model
        run_add(
            dir,
            "test-model",
            "openrouter",
            Some("anthropic/claude-3.5-sonnet"),
            "standard",
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();

        // Verify it exists in config
        let config = Config::load(dir).unwrap();
        assert!(config.model_registry.iter().any(|e| e.id == "test-model"));
        let entry = config
            .model_registry
            .iter()
            .find(|e| e.id == "test-model")
            .unwrap();
        assert_eq!(entry.provider, "openrouter");
        assert_eq!(entry.model, "anthropic/claude-3.5-sonnet");
    }

    #[test]
    fn test_model_add_defaults_model_id_to_alias() {
        let tmp = setup_dir();
        let dir = tmp.path();

        run_add(
            dir, "gpt-4o", "openai", None, "standard", None, None, None, None, false,
        )
        .unwrap();

        let config = Config::load(dir).unwrap();
        let entry = config
            .model_registry
            .iter()
            .find(|e| e.id == "gpt-4o")
            .unwrap();
        assert_eq!(entry.model, "gpt-4o");
    }

    #[test]
    fn test_model_remove() {
        let tmp = setup_dir();
        let dir = tmp.path();

        // Add then remove
        run_add(
            dir,
            "ephemeral",
            "openai",
            None,
            "fast",
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
        run_remove(dir, "ephemeral", false, false, false).unwrap();

        let config = Config::load(dir).unwrap();
        assert!(!config.model_registry.iter().any(|e| e.id == "ephemeral"));
    }

    #[test]
    fn test_model_remove_builtin_fails() {
        let tmp = setup_dir();
        let dir = tmp.path();

        let result = run_remove(dir, "haiku", false, false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("built-in"), "error was: {}", err);
    }

    #[test]
    fn test_model_set_default() {
        let tmp = setup_dir();
        let dir = tmp.path();

        // "sonnet" is a built-in, so it should be available
        run_set_default(dir, "sonnet", false).unwrap();

        let config = Config::load(dir).unwrap();
        let default = config
            .models
            .get_role(workgraph::config::DispatchRole::Default)
            .unwrap();
        assert_eq!(default.model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn test_model_set_default_unknown_fails() {
        let tmp = setup_dir();
        let dir = tmp.path();

        let result = run_set_default(dir, "nonexistent-model", false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"), "error was: {}", err);
    }

    #[test]
    fn test_model_set_role() {
        let tmp = setup_dir();
        let dir = tmp.path();

        run_set(dir, "evaluator", "opus", None, None, false).unwrap();

        let config = Config::load(dir).unwrap();
        let role = config
            .models
            .get_role(workgraph::config::DispatchRole::Evaluator)
            .unwrap();
        assert_eq!(role.model.as_deref(), Some("opus"));
    }

    #[test]
    fn test_model_list_with_tier_filter() {
        let tmp = setup_dir();
        let dir = tmp.path();

        // Just verify it doesn't panic; output goes to stdout
        run_list(dir, Some("fast"), false).unwrap();
        run_list(dir, Some("premium"), false).unwrap();
        run_list(dir, None, false).unwrap();
    }

    #[test]
    fn test_model_list_json() {
        let tmp = setup_dir();
        let dir = tmp.path();

        // Should not panic
        run_list(dir, None, true).unwrap();
    }

    #[test]
    fn test_builtin_tier_aliases_still_work() {
        let tmp = setup_dir();
        let dir = tmp.path();

        // haiku, sonnet, opus should all be in the effective registry
        let config = Config::load_merged(dir).unwrap();
        assert!(config.registry_lookup("haiku").is_some());
        assert!(config.registry_lookup("sonnet").is_some());
        assert!(config.registry_lookup("opus").is_some());
    }

    #[test]
    fn test_model_remove_default_force() {
        let tmp = setup_dir();
        let dir = tmp.path();

        // Add a model and set it as default
        run_add(
            dir, "my-model", "openai", None, "standard", None, None, None, None, false,
        )
        .unwrap();
        run_set_default(dir, "my-model", false).unwrap();

        // Verify it is the default
        let config = Config::load(dir).unwrap();
        let default = config
            .models
            .get_role(workgraph::config::DispatchRole::Default)
            .unwrap();
        assert_eq!(default.model.as_deref(), Some("my-model"));

        // Removing with --force should succeed (the non-force path calls process::exit
        // which can't be tested in-process)
        run_remove(dir, "my-model", true, false, false).unwrap();
        let config = Config::load(dir).unwrap();
        assert!(!config.model_registry.iter().any(|e| e.id == "my-model"));
    }
}
