//! End-to-end integration test: model management workflow.
//!
//! Covers the six scenarios from the task description:
//! 1. New user setup: endpoint add → key set → model add → model set-default → task uses correct model
//! 2. Per-task override: default is sonnet, one task uses `--model custom-alias` → correct model
//! 3. Key validation: `wg key check` returns meaningful status
//! 4. Error paths: missing key, invalid model alias, unreachable endpoint
//! 5. Backward compat: tier aliases (haiku/sonnet/opus) work without endpoint/key config
//! 6. Config persistence: settings survive save/reload cycle

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use workgraph::config::{
    CLAUDE_HAIKU_MODEL_ID, CLAUDE_SONNET_MODEL_ID, Config, DispatchRole, EndpointConfig,
    EndpointsConfig, ModelRegistryEntry, Tier,
};
use workgraph::graph::WorkGraph;
use workgraph::parser::save_graph;

// ===========================================================================
// Helpers
// ===========================================================================

/// Create a minimal workgraph directory with empty graph and default config.
fn setup_workgraph_dir() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    // Create graph
    let graph_path = dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();

    // Create default config
    let config = Config::default();
    config.save(dir).unwrap();

    tmp
}

/// Add an endpoint to config directly.
fn add_endpoint(dir: &Path, ep: EndpointConfig) {
    let mut config = Config::load(dir).unwrap();
    // Clear default on others if this one is default
    if ep.is_default {
        for existing in &mut config.llm_endpoints.endpoints {
            existing.is_default = false;
        }
    }
    config.llm_endpoints.endpoints.push(ep);
    config.save(dir).unwrap();
}

/// Add a model registry entry to config directly.
fn add_registry_entry(dir: &Path, entry: ModelRegistryEntry) {
    let mut config = Config::load(dir).unwrap();
    // Remove existing entry with same id
    config.model_registry.retain(|e| e.id != entry.id);
    config.model_registry.push(entry);
    config.save(dir).unwrap();
}

/// Set the default model in config.
fn set_default_model(dir: &Path, alias: &str) {
    let mut config = Config::load(dir).unwrap();
    config.models.set_model(DispatchRole::Default, alias);
    config.save(dir).unwrap();
}

// ===========================================================================
// Scenario 1: New user setup flow
// endpoint add → key set → model add → model set-default → verify model on task
// ===========================================================================

mod model_management_new_user_setup {
    use super::*;

    #[test]
    fn e2e_new_user_setup_flow() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Step 1: Add an endpoint (like `wg endpoint add`)
        add_endpoint(
            dir,
            EndpointConfig {
                name: "my-openrouter".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: None,
                api_key: Some("sk-or-test-key-12345".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        // Verify endpoint was created and is default
        let config = Config::load(dir).unwrap();
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.name, "my-openrouter");
        assert_eq!(ep.provider, "openrouter");
        assert!(ep.is_default);
        assert_eq!(ep.api_key.as_deref(), Some("sk-or-test-key-12345"));

        // Step 2: Update key to use env var reference (like `wg key set`)
        {
            let mut config = Config::load(dir).unwrap();
            let ep = config
                .llm_endpoints
                .endpoints
                .iter_mut()
                .find(|e| e.provider == "openrouter")
                .unwrap();
            ep.api_key = None;
            ep.api_key_env = Some("OPENROUTER_API_KEY".to_string());
            config.save(dir).unwrap();
        }

        // Verify key source switched to env var
        let config = Config::load(dir).unwrap();
        let ep = config
            .llm_endpoints
            .endpoints
            .iter()
            .find(|e| e.provider == "openrouter")
            .unwrap();
        assert_eq!(ep.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert!(ep.api_key.is_none(), "inline key should be cleared");

        // Step 3: Add a custom model (like `wg model add`)
        add_registry_entry(
            dir,
            ModelRegistryEntry {
                id: "my-fast".to_string(),
                provider: "openrouter".to_string(),
                model: format!("anthropic/{CLAUDE_HAIKU_MODEL_ID}"),
                tier: Tier::Fast,
                endpoint: Some("my-openrouter".to_string()),
                context_window: 200_000,
                max_output_tokens: 0,
                cost_per_input_mtok: 0.80,
                cost_per_output_mtok: 4.0,
                prompt_caching: false,
                cache_read_discount: 0.0,
                cache_write_premium: 0.0,
                descriptors: vec![],
            },
        );

        // Verify model in registry
        let config = Config::load(dir).unwrap();
        let entry = config
            .model_registry
            .iter()
            .find(|e| e.id == "my-fast")
            .unwrap();
        assert_eq!(entry.provider, "openrouter");
        assert_eq!(entry.model, format!("anthropic/{CLAUDE_HAIKU_MODEL_ID}"));
        assert_eq!(entry.endpoint.as_deref(), Some("my-openrouter"));

        // Step 4: Set default model (like `wg model set-default`)
        set_default_model(dir, "openrouter:my-fast");

        // Verify default was set
        let config = Config::load(dir).unwrap();
        let default_role = config.models.get_role(DispatchRole::Default).unwrap();
        assert_eq!(default_role.model.as_deref(), Some("openrouter:my-fast"));

        // Step 5: Verify the full lookup chain works
        let merged = Config::load_merged(dir).unwrap();
        let resolved = merged.registry_lookup("my-fast").unwrap();
        assert_eq!(resolved.model, format!("anthropic/{CLAUDE_HAIKU_MODEL_ID}"));
        assert_eq!(resolved.provider, "openrouter");
        assert_eq!(resolved.endpoint.as_deref(), Some("my-openrouter"));

        // Verify the endpoint referenced by the registry entry is resolvable
        let ep_from_registry = merged
            .llm_endpoints
            .find_by_name(resolved.endpoint.as_deref().unwrap())
            .unwrap();
        assert_eq!(ep_from_registry.provider, "openrouter");
    }

    #[test]
    fn e2e_setup_with_key_file() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Create a key file
        let key_dir = dir.join("secrets");
        fs::create_dir_all(&key_dir).unwrap();
        fs::write(key_dir.join("or.key"), "sk-or-from-file-123").unwrap();

        // Add endpoint with key file
        add_endpoint(
            dir,
            EndpointConfig {
                name: "or-file".to_string(),
                provider: "openrouter".to_string(),
                url: None,
                model: None,
                api_key: None,
                api_key_file: Some(key_dir.join("or.key").to_string_lossy().to_string()),
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        // Verify key resolves from file
        let config = Config::load(dir).unwrap();
        let ep = &config.llm_endpoints.endpoints[0];
        let key = ep.resolve_api_key(Some(dir)).unwrap();
        assert_eq!(key, Some("sk-or-from-file-123".to_string()));
        assert_eq!(ep.masked_key(), "(from file)");
    }
}

// ===========================================================================
// Scenario 2: Per-task model override
// ===========================================================================

mod model_management_per_task_override {
    use super::*;

    /// Replicate resolve_model_and_provider from spawn/execution.rs:
    /// Unified model+provider resolution using parse_model_spec at each tier.
    struct ResolvedModelProvider {
        model: Option<String>,
        provider: Option<String>,
    }

    fn resolve_model_and_provider(
        task_model: Option<String>,
        task_provider: Option<String>,
        agent_preferred_model: Option<String>,
        agent_preferred_provider: Option<String>,
        executor_model: Option<String>,
        role_model: Option<String>,
        role_provider: Option<String>,
        coordinator_model: Option<&str>,
        coordinator_provider: Option<String>,
    ) -> ResolvedModelProvider {
        struct Tier {
            model: Option<String>,
            provider: Option<String>,
        }
        impl Tier {
            fn new(model: Option<String>, provider: Option<String>) -> Self {
                if provider.is_some() {
                    return Self { model, provider };
                }
                if let Some(ref m) = model {
                    let spec = workgraph::config::parse_model_spec(m);
                    if let Some(ref p) = spec.provider {
                        return Self {
                            model,
                            provider: Some(
                                workgraph::config::provider_to_native_provider(p).to_string(),
                            ),
                        };
                    }
                }
                Self { model, provider }
            }
        }
        let tiers = [
            Tier::new(task_model, task_provider),
            Tier::new(agent_preferred_model, agent_preferred_provider),
            Tier::new(executor_model, None),
            Tier::new(role_model, role_provider),
            Tier::new(
                coordinator_model.map(|s| s.to_string()),
                coordinator_provider,
            ),
        ];
        ResolvedModelProvider {
            model: tiers.iter().find_map(|t| t.model.clone()),
            provider: tiers.iter().find_map(|t| t.provider.clone()),
        }
    }

    #[test]
    fn per_task_override_uses_custom_alias() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Set up: sonnet as default
        set_default_model(dir, "claude:sonnet");

        // Add a custom model alias
        add_registry_entry(
            dir,
            ModelRegistryEntry {
                id: "my-custom".to_string(),
                provider: "openrouter".to_string(),
                model: "anthropic/claude-3.5-sonnet".to_string(),
                tier: Tier::Standard,
                endpoint: None,
                context_window: 0,
                max_output_tokens: 0,
                cost_per_input_mtok: 0.0,
                cost_per_output_mtok: 0.0,
                prompt_caching: false,
                cache_read_discount: 0.0,
                cache_write_premium: 0.0,
                descriptors: vec![],
            },
        );

        // Simulate task with --model my-custom
        let task_model = Some("my-custom".to_string());
        let resolved = resolve_model_and_provider(
            task_model,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("claude:sonnet"),
            None,
        );
        assert_eq!(resolved.model, Some("my-custom".to_string()));

        // Registry lookup resolves the alias
        let merged = Config::load_merged(dir).unwrap();
        let entry = merged.registry_lookup("my-custom").unwrap();
        assert_eq!(entry.model, "anthropic/claude-3.5-sonnet");
        assert_eq!(entry.provider, "openrouter");
    }

    #[test]
    fn default_model_used_when_no_task_override() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        set_default_model(dir, "claude:sonnet");

        // No task model → falls through to coordinator model
        let resolved = resolve_model_and_provider(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("claude:sonnet"),
            None,
        );
        assert_eq!(resolved.model, Some("claude:sonnet".to_string()));

        // "sonnet" should resolve via builtin registry
        let merged = Config::load_merged(dir).unwrap();
        let entry = merged.registry_lookup("sonnet").unwrap();
        assert_eq!(entry.provider, "anthropic");
    }

    #[test]
    fn task_model_overrides_all_levels() {
        let resolved = resolve_model_and_provider(
            Some("task-override".to_string()),
            None,
            Some("agent-preferred".to_string()),
            None,
            Some("executor-default".to_string()),
            None,
            None,
            Some("coordinator-fallback"),
            None,
        );
        assert_eq!(resolved.model, Some("task-override".to_string()));
    }

    #[test]
    fn agent_preferred_when_no_task() {
        let resolved = resolve_model_and_provider(
            None,
            None,
            Some("agent-preferred".to_string()),
            None,
            Some("executor-default".to_string()),
            None,
            None,
            Some("coordinator-fallback"),
            None,
        );
        assert_eq!(resolved.model, Some("agent-preferred".to_string()));
    }

    #[test]
    fn executor_when_no_agent() {
        let resolved = resolve_model_and_provider(
            None,
            None,
            None,
            None,
            Some("executor-default".to_string()),
            None,
            None,
            Some("coordinator-fallback"),
            None,
        );
        assert_eq!(resolved.model, Some("executor-default".to_string()));
    }

    #[test]
    fn coordinator_fallback() {
        let resolved = resolve_model_and_provider(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("coordinator-fallback"),
            None,
        );
        assert_eq!(resolved.model, Some("coordinator-fallback".to_string()));
    }

    #[test]
    fn all_none_returns_none() {
        let resolved =
            resolve_model_and_provider(None, None, None, None, None, None, None, None, None);
        assert_eq!(resolved.model, None);
        assert_eq!(resolved.provider, None);
    }

    #[test]
    fn task_provider_overrides_all() {
        let resolved = resolve_model_and_provider(
            None,
            Some("openai".to_string()),
            None,
            Some("openrouter".to_string()),
            None,
            None,
            Some("anthropic".to_string()),
            None,
            None,
        );
        assert_eq!(resolved.provider, Some("openai".to_string()));
    }

    #[test]
    fn role_config_model_resolution_with_per_task_override() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Config: evaluator uses opus, default uses sonnet
        let mut config = Config::load(dir).unwrap();
        config
            .models
            .set_model(DispatchRole::Default, "claude:sonnet");
        config
            .models
            .set_model(DispatchRole::Evaluator, "claude:opus");
        config.save(dir).unwrap();

        let loaded = Config::load(dir).unwrap();
        let eval_resolved = loaded.resolve_model_for_role(DispatchRole::Evaluator);
        // Evaluator should resolve to opus-family model
        assert!(
            eval_resolved.model.contains("opus"),
            "Evaluator should resolve to opus, got: {}",
            eval_resolved.model
        );
    }
}

// ===========================================================================
// Scenario 3: Key validation
// ===========================================================================

mod model_management_key_validation {
    use super::*;

    #[test]
    fn key_resolve_inline_present() {
        let ep = EndpointConfig {
            name: "test-ep".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: Some("sk-test-key-abc".to_string()),
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key, Some("sk-test-key-abc".to_string()));
    }

    #[test]
    fn key_resolve_from_file() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("test.key");
        fs::write(&key_path, "sk-from-file-test\n").unwrap();

        let ep = EndpointConfig {
            name: "file-ep".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key, Some("sk-from-file-test".to_string()));
    }

    #[test]
    fn key_resolve_env_var_reference() {
        let ep = EndpointConfig {
            name: "env-ep".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            is_default: false,
            context_window: None,
        };
        // We can't control env vars in parallel tests, but key_source should reflect the config
        assert_eq!(ep.key_source(), "env: OPENROUTER_API_KEY");
    }

    #[test]
    fn key_source_reports_correctly() {
        // Inline key
        let ep_inline = EndpointConfig {
            name: "a".into(),
            provider: "openai".into(),
            url: None,
            model: None,
            api_key: Some("sk-123".into()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep_inline.key_source(), "inline");

        // File key
        let ep_file = EndpointConfig {
            name: "b".into(),
            provider: "openai".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("/path/to/key".into()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep_file.key_source(), "file: /path/to/key");

        // Env key
        let ep_env = EndpointConfig {
            name: "c".into(),
            provider: "openai".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: Some("MY_KEY".into()),
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep_env.key_source(), "env: MY_KEY");

        // None — use a provider with no env-var fallback so host env doesn't interfere
        let ep_none = EndpointConfig {
            name: "d".into(),
            provider: "custom".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep_none.key_source(), "(not configured)");
    }

    #[test]
    fn key_masked_formats() {
        // Long key
        let ep_long = EndpointConfig {
            name: "a".into(),
            provider: "openai".into(),
            url: None,
            model: None,
            api_key: Some("sk-or-v1-abcdef123456".into()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let masked = ep_long.masked_key();
        assert!(masked.contains("****"));
        assert!(masked.starts_with("sk-"));

        // Short key
        let ep_short = EndpointConfig {
            name: "b".into(),
            provider: "openai".into(),
            url: None,
            model: None,
            api_key: Some("short".into()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep_short.masked_key(), "****");

        // File ref
        let ep_file = EndpointConfig {
            name: "c".into(),
            provider: "openai".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("/path".into()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep_file.masked_key(), "(from file)");

        // No key
        let ep_none = EndpointConfig {
            name: "d".into(),
            provider: "openai".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep_none.masked_key(), "(not set)");
    }

    #[test]
    fn key_inline_beats_file() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("should-not-read.key");
        fs::write(&key_path, "file-key").unwrap();

        let ep = EndpointConfig {
            name: "priority-ep".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: Some("inline-wins".to_string()),
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key, Some("inline-wins".to_string()));
    }

    #[test]
    fn key_resolve_no_source_returns_none() {
        let ep = EndpointConfig {
            name: "nokey".to_string(),
            provider: "local".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        // With provider "local" there are no env var fallbacks
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key, None);
    }

    #[test]
    fn find_for_provider_prefers_default() {
        let endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![
                EndpointConfig {
                    name: "staging".to_string(),
                    provider: "openrouter".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-staging".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
                EndpointConfig {
                    name: "prod".to_string(),
                    provider: "openrouter".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-prod".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: true,
                    context_window: None,
                },
            ],
        };

        let ep = endpoints.find_for_provider("openrouter").unwrap();
        assert_eq!(ep.name, "prod", "should prefer default endpoint");
    }
}

// ===========================================================================
// Scenario 4: Error paths
// ===========================================================================

mod model_management_error_paths {
    use super::*;

    #[test]
    fn missing_key_file_errors() {
        let ep = EndpointConfig {
            name: "bad-ep".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("/nonexistent/path/key.txt".to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };

        let result = ep.resolve_api_key(None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to read API key"),
            "Error should mention reading API key, got: {}",
            err
        );
    }

    #[test]
    fn empty_key_file_errors() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("empty.key");
        fs::write(&key_path, "  \n  ").unwrap();

        let ep = EndpointConfig {
            name: "empty-ep".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };

        let result = ep.resolve_api_key(None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("empty"),
            "Error should mention empty file, got: {}",
            err
        );
    }

    #[test]
    fn invalid_model_alias_not_in_registry() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        let merged = Config::load_merged(dir).unwrap();
        let result = merged.registry_lookup("nonexistent-model-xyz");
        assert!(
            result.is_none(),
            "Unknown model alias should not be in registry"
        );
    }

    #[test]
    fn unknown_provider_has_empty_default_url() {
        let url = EndpointConfig::default_url_for_provider("nonexistent-provider");
        assert_eq!(url, "");
    }

    #[test]
    fn find_by_name_nonexistent() {
        let endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![EndpointConfig {
                name: "existing".to_string(),
                provider: "openrouter".to_string(),
                url: None,
                model: None,
                api_key: Some("key".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            }],
        };

        assert!(endpoints.find_by_name("nonexistent").is_none());
    }

    #[test]
    fn find_for_provider_empty_list() {
        let endpoints = EndpointsConfig { inherit_global: false, endpoints: vec![] };
        assert!(endpoints.find_for_provider("openrouter").is_none());
    }

    #[test]
    fn env_var_names_for_known_providers() {
        let or_vars = EndpointConfig::env_var_names_for_provider("openrouter");
        assert!(or_vars.contains(&"OPENROUTER_API_KEY"));
        assert!(or_vars.contains(&"OPENAI_API_KEY"));

        let oai_vars = EndpointConfig::env_var_names_for_provider("openai");
        assert!(oai_vars.contains(&"OPENAI_API_KEY"));

        let ant_vars = EndpointConfig::env_var_names_for_provider("anthropic");
        assert!(ant_vars.contains(&"ANTHROPIC_API_KEY"));

        let unknown_vars = EndpointConfig::env_var_names_for_provider("unknown");
        assert!(unknown_vars.is_empty());
    }

    #[test]
    fn default_url_for_known_providers() {
        assert_eq!(
            EndpointConfig::default_url_for_provider("openrouter"),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            EndpointConfig::default_url_for_provider("openai"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            EndpointConfig::default_url_for_provider("anthropic"),
            "https://api.anthropic.com"
        );
        assert_eq!(
            EndpointConfig::default_url_for_provider("local"),
            "http://localhost:11434/v1"
        );
    }

    #[test]
    fn config_load_empty_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("config.toml"), "").unwrap();

        let config = Config::load(tmp.path()).unwrap();
        assert!(config.llm_endpoints.endpoints.is_empty());
        assert!(config.model_registry.is_empty());
    }

    #[test]
    fn set_default_to_invalid_model_does_not_validate_at_config_level() {
        // At config level, we can set any string as default — validation
        // happens at the command layer. Verify config doesn't crash.
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        set_default_model(dir, "claude:nonexistent");

        let loaded = Config::load(dir).unwrap();
        let default = loaded.models.get_role(DispatchRole::Default).unwrap();
        assert_eq!(default.model.as_deref(), Some("claude:nonexistent"));

        // But registry_lookup won't find it
        let merged = Config::load_merged(dir).unwrap();
        assert!(merged.registry_lookup("nonexistent").is_none());
    }
}

// ===========================================================================
// Scenario 5: Backward compatibility — tier aliases work without endpoint/key config
// ===========================================================================

mod model_management_backward_compat {
    use super::*;

    #[test]
    fn builtin_haiku_in_registry() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        let merged = Config::load_merged(dir).unwrap();
        let entry = merged.registry_lookup("haiku").unwrap();
        assert_eq!(entry.provider, "anthropic");
        assert!(
            entry.model.contains("haiku"),
            "haiku should map to a haiku model, got: {}",
            entry.model
        );
    }

    #[test]
    fn builtin_sonnet_in_registry() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        let merged = Config::load_merged(dir).unwrap();
        let entry = merged.registry_lookup("sonnet").unwrap();
        assert_eq!(entry.provider, "anthropic");
        assert!(
            entry.model.contains("sonnet"),
            "sonnet should map to a sonnet model, got: {}",
            entry.model
        );
    }

    #[test]
    fn builtin_opus_in_registry() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        let merged = Config::load_merged(dir).unwrap();
        let entry = merged.registry_lookup("opus").unwrap();
        assert_eq!(entry.provider, "anthropic");
        assert!(
            entry.model.contains("opus"),
            "opus should map to an opus model, got: {}",
            entry.model
        );
    }

    #[test]
    fn tier_aliases_resolve_without_any_endpoint_config() {
        // Fresh config with NO endpoints configured
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let graph_path = dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_path).unwrap();
        Config::default().save(dir).unwrap();

        let config = Config::load(dir).unwrap();
        assert!(
            config.llm_endpoints.endpoints.is_empty(),
            "Should have no endpoints"
        );

        // But tier aliases should still resolve via builtin registry
        let merged = Config::load_merged(dir).unwrap();
        assert!(merged.registry_lookup("haiku").is_some());
        assert!(merged.registry_lookup("sonnet").is_some());
        assert!(merged.registry_lookup("opus").is_some());
    }

    #[test]
    fn effective_registry_contains_builtins() {
        let config = Config::default();
        let effective = config.effective_registry();
        assert!(
            effective.iter().any(|e| e.id == "haiku"),
            "haiku should be in effective registry"
        );
        assert!(
            effective.iter().any(|e| e.id == "sonnet"),
            "sonnet should be in effective registry"
        );
        assert!(
            effective.iter().any(|e| e.id == "opus"),
            "opus should be in effective registry"
        );
    }

    #[test]
    fn resolve_model_for_role_uses_tier_defaults() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        let config = Config::load(dir).unwrap();

        // Triage role defaults to Fast tier (haiku)
        let triage = config.resolve_model_for_role(DispatchRole::Triage);
        assert!(
            triage.model.contains("haiku"),
            "Triage should default to haiku-family, got: {}",
            triage.model
        );

        // Evaluator role defaults to Fast tier (haiku)
        let evaluator = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert!(
            evaluator.model.contains("haiku"),
            "Evaluator should default to haiku, got: {}",
            evaluator.model
        );
    }

    #[test]
    fn model_set_default_with_builtin_alias() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        set_default_model(dir, "claude:opus");

        let config = Config::load(dir).unwrap();
        let default = config.models.get_role(DispatchRole::Default).unwrap();
        assert_eq!(default.model.as_deref(), Some("claude:opus"));

        // Resolve role should work and find opus
        let resolved = config.resolve_model_for_role(DispatchRole::Default);
        assert!(
            resolved.model.contains("opus"),
            "Default role should resolve to an opus model, got: {}",
            resolved.model
        );
    }

    #[test]
    fn custom_model_does_not_shadow_builtins() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Adding a custom model with a different name should not break builtins
        add_registry_entry(
            dir,
            ModelRegistryEntry {
                id: "my-fast".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                tier: Tier::Fast,
                endpoint: None,
                context_window: 0,
                max_output_tokens: 0,
                cost_per_input_mtok: 0.0,
                cost_per_output_mtok: 0.0,
                prompt_caching: false,
                cache_read_discount: 0.0,
                cache_write_premium: 0.0,
                descriptors: vec![],
            },
        );

        let merged = Config::load_merged(dir).unwrap();
        // Builtins still work
        assert!(merged.registry_lookup("haiku").is_some());
        assert!(merged.registry_lookup("sonnet").is_some());
        assert!(merged.registry_lookup("opus").is_some());
        // Custom also works
        let custom = merged.registry_lookup("my-fast").unwrap();
        assert_eq!(custom.model, "gpt-4o-mini");
    }

    #[test]
    fn user_entry_overrides_builtin_with_same_id() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Override the built-in "haiku" with a custom entry
        add_registry_entry(
            dir,
            ModelRegistryEntry {
                id: "haiku".to_string(),
                provider: "openai".to_string(),
                model: "custom-haiku-replacement".to_string(),
                tier: Tier::Fast,
                endpoint: None,
                context_window: 0,
                max_output_tokens: 0,
                cost_per_input_mtok: 0.0,
                cost_per_output_mtok: 0.0,
                prompt_caching: false,
                cache_read_discount: 0.0,
                cache_write_premium: 0.0,
                descriptors: vec![],
            },
        );

        let merged = Config::load_merged(dir).unwrap();
        let entry = merged.registry_lookup("haiku").unwrap();
        assert_eq!(
            entry.model, "custom-haiku-replacement",
            "User entry should override builtin"
        );
        assert_eq!(entry.provider, "openai");
    }
}

// ===========================================================================
// Scenario 6: Config persistence — settings survive save/reload cycle
// ===========================================================================

mod model_management_config_persistence {
    use super::*;

    #[test]
    fn endpoint_config_persists_across_reload() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        add_endpoint(
            dir,
            EndpointConfig {
                name: "persist-ep".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: Some(format!("anthropic/{CLAUDE_SONNET_MODEL_ID}")),
                api_key: Some("sk-persist-key".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        // Reload and verify
        let sonnet_model = format!("anthropic/{CLAUDE_SONNET_MODEL_ID}");
        let loaded = Config::load(dir).unwrap();
        let ep = loaded.llm_endpoints.find_by_name("persist-ep").unwrap();
        assert_eq!(ep.provider, "openrouter");
        assert_eq!(ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(ep.model.as_deref(), Some(sonnet_model.as_str()));
        assert_eq!(ep.api_key.as_deref(), Some("sk-persist-key"));
        assert!(ep.is_default);
    }

    #[test]
    fn model_registry_persists_across_reload() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        add_registry_entry(
            dir,
            ModelRegistryEntry {
                id: "persist-model".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
                tier: Tier::Standard,
                endpoint: None,
                context_window: 128_000,
                max_output_tokens: 16_384,
                cost_per_input_mtok: 2.5,
                cost_per_output_mtok: 10.0,
                prompt_caching: false,
                cache_read_discount: 0.0,
                cache_write_premium: 0.0,
                descriptors: vec![],
            },
        );

        // Reload
        let loaded = Config::load(dir).unwrap();
        let entry = loaded
            .model_registry
            .iter()
            .find(|e| e.id == "persist-model")
            .unwrap();
        assert_eq!(entry.provider, "openai");
        assert_eq!(entry.model, "gpt-4o");
        assert_eq!(entry.context_window, 128_000);
        assert!((entry.cost_per_input_mtok - 2.5).abs() < 0.01);
        assert!((entry.cost_per_output_mtok - 10.0).abs() < 0.01);
    }

    #[test]
    fn default_model_persists_across_reload() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        set_default_model(dir, "claude:opus");

        let loaded = Config::load(dir).unwrap();
        let default = loaded.models.get_role(DispatchRole::Default).unwrap();
        assert_eq!(default.model.as_deref(), Some("claude:opus"));
    }

    #[test]
    fn key_env_ref_persists_across_reload() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Create endpoint with env key reference
        add_endpoint(
            dir,
            EndpointConfig {
                name: "env-ep".to_string(),
                provider: "openrouter".to_string(),
                url: None,
                model: None,
                api_key: None,
                api_key_file: None,
                api_key_env: Some("MY_CUSTOM_KEY".to_string()),
                is_default: true,
                context_window: None,
            },
        );

        let loaded = Config::load(dir).unwrap();
        let ep = loaded
            .llm_endpoints
            .endpoints
            .iter()
            .find(|e| e.provider == "openrouter")
            .unwrap();
        assert_eq!(ep.api_key_env.as_deref(), Some("MY_CUSTOM_KEY"));
    }

    #[test]
    fn key_file_ref_persists_and_resolves() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        let key_path = dir.join("my.key");
        fs::write(&key_path, "sk-persist-file-key").unwrap();

        add_endpoint(
            dir,
            EndpointConfig {
                name: "file-ep".to_string(),
                provider: "anthropic".to_string(),
                url: None,
                model: None,
                api_key: None,
                api_key_file: Some(key_path.to_string_lossy().to_string()),
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        let loaded = Config::load(dir).unwrap();
        let ep = loaded
            .llm_endpoints
            .endpoints
            .iter()
            .find(|e| e.provider == "anthropic")
            .unwrap();
        assert!(ep.api_key_file.is_some());
        let key = ep.resolve_api_key(Some(dir)).unwrap();
        assert_eq!(key, Some("sk-persist-file-key".to_string()));
    }

    #[test]
    fn full_config_roundtrip_all_components() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Set up endpoint
        add_endpoint(
            dir,
            EndpointConfig {
                name: "round-ep".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: None,
                api_key: Some("sk-roundtrip".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        // Add custom model with endpoint reference
        add_registry_entry(
            dir,
            ModelRegistryEntry {
                id: "round-model".to_string(),
                provider: "openrouter".to_string(),
                model: format!("anthropic/{CLAUDE_SONNET_MODEL_ID}"),
                tier: Tier::Standard,
                endpoint: Some("round-ep".to_string()),
                context_window: 200_000,
                max_output_tokens: 0,
                cost_per_input_mtok: 3.0,
                cost_per_output_mtok: 15.0,
                prompt_caching: false,
                cache_read_discount: 0.0,
                cache_write_premium: 0.0,
                descriptors: vec![],
            },
        );

        // Set as default
        set_default_model(dir, "openrouter:round-model");

        // Set per-role model
        {
            let mut config = Config::load(dir).unwrap();
            config
                .models
                .set_model(DispatchRole::Evaluator, "claude:opus");
            config.save(dir).unwrap();
        }

        // ---- Reload and verify everything ----
        let loaded = Config::load(dir).unwrap();

        // Endpoint
        let ep = loaded.llm_endpoints.find_by_name("round-ep").unwrap();
        assert_eq!(ep.provider, "openrouter");
        assert!(ep.is_default);
        assert_eq!(ep.api_key.as_deref(), Some("sk-roundtrip"));

        // Custom model in registry
        let entry = loaded
            .model_registry
            .iter()
            .find(|e| e.id == "round-model")
            .unwrap();
        let sonnet_model = format!("anthropic/{CLAUDE_SONNET_MODEL_ID}");
        assert_eq!(entry.model, sonnet_model);
        assert_eq!(entry.endpoint.as_deref(), Some("round-ep"));

        // Default model
        let default = loaded.models.get_role(DispatchRole::Default).unwrap();
        assert_eq!(default.model.as_deref(), Some("openrouter:round-model"));

        // Per-role model
        let eval_role = loaded.models.get_role(DispatchRole::Evaluator).unwrap();
        assert_eq!(eval_role.model.as_deref(), Some("claude:opus"));

        // Verify registry lookup still works after reload
        let merged = Config::load_merged(dir).unwrap();
        let resolved = merged.registry_lookup("round-model").unwrap();
        assert_eq!(resolved.model, sonnet_model);
        assert_eq!(resolved.endpoint.as_deref(), Some("round-ep"));

        // Verify endpoint lookup from registry entry
        let ep_from_registry = loaded
            .llm_endpoints
            .find_by_name(resolved.endpoint.as_deref().unwrap())
            .unwrap();
        assert_eq!(
            ep_from_registry.resolve_api_key(Some(dir)).unwrap(),
            Some("sk-roundtrip".to_string())
        );
    }

    #[test]
    fn model_remove_persists() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        add_registry_entry(
            dir,
            ModelRegistryEntry {
                id: "ephemeral".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                tier: Tier::Fast,
                endpoint: None,
                context_window: 0,
                max_output_tokens: 0,
                cost_per_input_mtok: 0.0,
                cost_per_output_mtok: 0.0,
                prompt_caching: false,
                cache_read_discount: 0.0,
                cache_write_premium: 0.0,
                descriptors: vec![],
            },
        );

        // Verify added
        let config = Config::load(dir).unwrap();
        assert!(config.model_registry.iter().any(|e| e.id == "ephemeral"));

        // Remove
        {
            let mut config = Config::load(dir).unwrap();
            config.model_registry.retain(|e| e.id != "ephemeral");
            config.save(dir).unwrap();
        }

        // Verify removed
        let loaded = Config::load(dir).unwrap();
        assert!(!loaded.model_registry.iter().any(|e| e.id == "ephemeral"));
    }

    #[test]
    fn endpoint_remove_persists() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        add_endpoint(
            dir,
            EndpointConfig {
                name: "temp-ep".to_string(),
                provider: "openai".to_string(),
                url: None,
                model: None,
                api_key: None,
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        assert_eq!(Config::load(dir).unwrap().llm_endpoints.endpoints.len(), 1);

        // Remove
        {
            let mut config = Config::load(dir).unwrap();
            config
                .llm_endpoints
                .endpoints
                .retain(|ep| ep.name != "temp-ep");
            config.save(dir).unwrap();
        }

        assert!(
            Config::load(dir)
                .unwrap()
                .llm_endpoints
                .endpoints
                .is_empty()
        );
    }

    #[test]
    fn model_routing_per_role_persists() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Set up per-role model routing
        {
            let mut config = Config::load(dir).unwrap();
            config
                .models
                .set_model(DispatchRole::Evaluator, "openrouter:opus");
            config.models.set_endpoint(DispatchRole::Evaluator, "my-ep");
            config
                .models
                .set_model(DispatchRole::Triage, "claude:haiku");
            config.save(dir).unwrap();
        }

        // Reload and verify
        let loaded = Config::load(dir).unwrap();

        let eval = loaded.models.get_role(DispatchRole::Evaluator).unwrap();
        assert_eq!(eval.model.as_deref(), Some("openrouter:opus"));
        assert_eq!(eval.endpoint.as_deref(), Some("my-ep"));

        let triage = loaded.models.get_role(DispatchRole::Triage).unwrap();
        assert_eq!(triage.model.as_deref(), Some("claude:haiku"));
    }
}

// ===========================================================================
// 7. Unified API key resolution through endpoint system
// ===========================================================================
mod unified_key_resolution {
    use super::*;
    use serial_test::serial;
    use workgraph::executor::native::openai_client::resolve_openai_api_key_from_dir;

    /// Isolate from real global config by pointing HOME at a temp dir.
    /// Returns saved HOME value for restoration.
    fn isolate_home(tmp: &TempDir) -> Option<String> {
        let saved = std::env::var("HOME").ok();
        let fake_home = tmp.path().join("fakehome");
        std::fs::create_dir_all(&fake_home).unwrap();
        unsafe { std::env::set_var("HOME", &fake_home) };
        saved
    }

    fn restore_home(saved: Option<String>) {
        match saved {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// When llm_endpoints has an inline api_key and no env vars are set,
    /// resolve_openai_api_key_from_dir should find the key.
    #[test]
    #[serial]
    fn test_endpoint_key_used_when_no_env_vars() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();
        let saved_home = isolate_home(&tmp);

        // Add an openrouter endpoint with an inline key
        add_endpoint(
            dir,
            EndpointConfig {
                name: "openrouter".into(),
                provider: "openrouter".into(),
                url: Some("https://openrouter.ai/api/v1".into()),
                model: None,
                api_key: Some("sk-or-endpoint-key".into()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        // Clear env vars
        let saved_or = std::env::var("OPENROUTER_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::remove_var("OPENROUTER_API_KEY") };
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        let key = resolve_openai_api_key_from_dir(dir).unwrap();
        assert_eq!(key, "sk-or-endpoint-key");

        // Restore env
        match saved_or {
            Some(v) => unsafe { std::env::set_var("OPENROUTER_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENROUTER_API_KEY") },
        }
        match saved_oai {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        restore_home(saved_home);
    }

    /// When llm_endpoints has api_key_file and no env vars are set,
    /// resolve_openai_api_key_from_dir should read the key from file.
    #[test]
    #[serial]
    fn test_endpoint_key_file_used_when_no_env_vars() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();
        let saved_home = isolate_home(&tmp);

        // Write key to a file
        let key_file = dir.join("api.key");
        fs::write(&key_file, "sk-or-from-file\n").unwrap();

        add_endpoint(
            dir,
            EndpointConfig {
                name: "openrouter".into(),
                provider: "openrouter".into(),
                url: None,
                model: None,
                api_key: None,
                api_key_file: Some(key_file.to_string_lossy().into_owned()),
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        // Clear env vars
        let saved_or = std::env::var("OPENROUTER_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::remove_var("OPENROUTER_API_KEY") };
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        let key = resolve_openai_api_key_from_dir(dir).unwrap();
        assert_eq!(key, "sk-or-from-file");

        // Restore env
        match saved_or {
            Some(v) => unsafe { std::env::set_var("OPENROUTER_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENROUTER_API_KEY") },
        }
        match saved_oai {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        restore_home(saved_home);
    }

    /// Env var fallback still works when no endpoints are configured.
    #[test]
    #[serial]
    fn test_env_var_fallback_still_works() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();
        let saved_home = isolate_home(&tmp);

        // No endpoints added — just default config
        let saved_or = std::env::var("OPENROUTER_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENROUTER_API_KEY", "sk-or-env-fallback") };
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        let key = resolve_openai_api_key_from_dir(dir).unwrap();
        assert_eq!(key, "sk-or-env-fallback");

        // Restore env
        match saved_or {
            Some(v) => unsafe { std::env::set_var("OPENROUTER_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENROUTER_API_KEY") },
        }
        match saved_oai {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        restore_home(saved_home);
    }

    /// Config::resolve_api_key_for_provider works end-to-end via Config::load_merged.
    #[test]
    #[serial]
    fn test_config_resolve_api_key_for_provider_end_to_end() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();
        let saved_home = isolate_home(&tmp);

        add_endpoint(
            dir,
            EndpointConfig {
                name: "my-ep".into(),
                provider: "openrouter".into(),
                url: None,
                model: None,
                api_key: Some("sk-or-e2e-test".into()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            },
        );

        let saved_or = std::env::var("OPENROUTER_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::remove_var("OPENROUTER_API_KEY") };
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        let config = Config::load_merged(dir).unwrap();
        let key = config
            .resolve_api_key_for_provider("openrouter", dir)
            .unwrap();
        assert_eq!(key, "sk-or-e2e-test");

        // Restore env
        match saved_or {
            Some(v) => unsafe { std::env::set_var("OPENROUTER_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENROUTER_API_KEY") },
        }
        match saved_oai {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        restore_home(saved_home);
    }
}
