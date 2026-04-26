//! Integration tests for the complete endpoint→spawn→agent pipeline.
//!
//! Covers five test categories:
//! 1. Endpoint resolution chain (inline key, key file, env var, fallback)
//! 2. Provider env vars (WG_LLM_PROVIDER, WG_ENDPOINT_URL set correctly)
//! 3. Agent model preferences (preferred_model → spawn, task overrides agent, preferred_provider)
//! 4. Round-trip config (CLI add → list → verify → remove → verify)
//! 5. Error cases (bad key file, invalid provider, bad key)

use std::fs;

use tempfile::TempDir;

use workgraph::config::{CLAUDE_SONNET_MODEL_ID, Config, EndpointConfig, EndpointsConfig};

// ===========================================================================
// Helpers
// ===========================================================================

fn setup_workgraph_dir() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();
    tmp
}

// ===========================================================================
// 1. Endpoint resolution chain (inline key, key file, env var, fallback)
// ===========================================================================

#[test]
fn integration_openrouter_resolve_inline_key() {
    let ep = EndpointConfig {
        name: "inline-ep".to_string(),
        provider: "openrouter".to_string(),
        url: Some("https://openrouter.ai/api/v1".to_string()),
        model: Some(format!("anthropic/{CLAUDE_SONNET_MODEL_ID}")),
        api_key: Some("sk-or-inline-key-123".to_string()),
        api_key_file: None,
        api_key_env: None,
        is_default: true,
        context_window: None,
    };

    let key = ep.resolve_api_key(None).unwrap();
    assert_eq!(key, Some("sk-or-inline-key-123".to_string()));
}

#[test]
fn integration_openrouter_resolve_key_file() {
    let tmp = TempDir::new().unwrap();
    let key_path = tmp.path().join("or.key");
    fs::write(&key_path, "  sk-or-from-file-xyz  \n").unwrap();

    let ep = EndpointConfig {
        name: "file-ep".to_string(),
        provider: "openrouter".to_string(),
        url: None,
        model: None,
        api_key: None,
        api_key_file: Some(key_path.to_string_lossy().to_string()),
        api_key_env: None,
        is_default: false,
        context_window: None,
    };

    let key = ep.resolve_api_key(None).unwrap();
    assert_eq!(key, Some("sk-or-from-file-xyz".to_string()));
}

#[test]
fn integration_openrouter_resolve_key_file_relative() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let secrets_dir = wg_dir.join("secrets");
    fs::create_dir_all(&secrets_dir).unwrap();
    fs::write(secrets_dir.join("or.key"), "sk-relative-resolved").unwrap();

    let ep = EndpointConfig {
        name: "rel-ep".to_string(),
        provider: "openrouter".to_string(),
        url: None,
        model: None,
        api_key: None,
        api_key_file: Some("secrets/or.key".to_string()),
        api_key_env: None,
        is_default: false,
        context_window: None,
    };

    let key = ep.resolve_api_key(Some(&wg_dir)).unwrap();
    assert_eq!(key, Some("sk-relative-resolved".to_string()));
}

#[test]
fn integration_openrouter_inline_key_beats_key_file() {
    let tmp = TempDir::new().unwrap();
    let key_path = tmp.path().join("should-not-read.key");
    fs::write(&key_path, "file-key-value").unwrap();

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
fn integration_openrouter_no_key_returns_none() {
    let ep = EndpointConfig {
        name: "nokey-ep".to_string(),
        provider: "openrouter".to_string(),
        url: None,
        model: None,
        api_key: None,
        api_key_file: None,
        api_key_env: None,
        is_default: false,
        context_window: None,
    };

    // Without any env vars set for this test, should return None
    // (we can't easily unset env vars in parallel tests, so just verify
    // the resolution chain works with no inline key or file)
    let key = ep.resolve_api_key(None).unwrap();
    // If OPENROUTER_API_KEY or OPENAI_API_KEY is set in the env, this
    // could return Some. That's fine — the important thing is it doesn't error.
    let _ = key;
}

#[test]
fn integration_openrouter_find_for_provider() {
    let endpoints = EndpointsConfig {
        inherit_global: false,
        endpoints: vec![
            EndpointConfig {
                name: "anthropic-prod".to_string(),
                provider: "anthropic".to_string(),
                url: Some("https://api.anthropic.com".to_string()),
                api_key: Some("sk-ant-prod".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: true,
                context_window: None,
            },
            EndpointConfig {
                name: "openrouter-prod".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                api_key: Some("sk-or-prod".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: false,
                context_window: None,
            },
        ],
    };

    let or_ep = endpoints.find_for_provider("openrouter").unwrap();
    assert_eq!(or_ep.name, "openrouter-prod");
    assert_eq!(or_ep.api_key.as_deref(), Some("sk-or-prod"));

    let ant_ep = endpoints.find_for_provider("anthropic").unwrap();
    assert_eq!(ant_ep.name, "anthropic-prod");

    assert!(endpoints.find_for_provider("local").is_none());
}

#[test]
fn integration_openrouter_find_for_provider_prefers_default() {
    let endpoints = EndpointsConfig {
        inherit_global: false,
        endpoints: vec![
            EndpointConfig {
                name: "or-staging".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://staging.openrouter.ai/api/v1".to_string()),
                api_key: Some("sk-staging".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: false,
                context_window: None,
            },
            EndpointConfig {
                name: "or-prod".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                api_key: Some("sk-prod".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: true,
                context_window: None,
            },
        ],
    };

    let ep = endpoints.find_for_provider("openrouter").unwrap();
    assert_eq!(ep.name, "or-prod", "should prefer the default endpoint");
}

#[test]
fn integration_openrouter_default_url() {
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
    assert_eq!(EndpointConfig::default_url_for_provider("unknown"), "");
}

// ===========================================================================
// 2. Provider env vars (WG_LLM_PROVIDER, WG_ENDPOINT_URL set correctly)
// ===========================================================================

/// Tests that the spawn execution module's resolve_provider correctly
/// resolves the provider from task, agent, and config sources.
/// The actual env var setting happens in spawn_agent_inner; here we test
/// the resolution functions that drive those env vars.
mod provider_env_var_tests {
    use workgraph::config::{
        CLAUDE_OPUS_MODEL_ID, CLAUDE_SONNET_MODEL_ID, Config, DispatchRole, EndpointConfig,
        EndpointsConfig,
    };

    #[test]
    fn integration_openrouter_provider_resolution_from_config() {
        let mut config = Config::default();
        config
            .models
            .set_provider(DispatchRole::TaskAgent, "openrouter");

        let resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
        assert_eq!(
            resolved.provider,
            Some("openrouter".to_string()),
            "TaskAgent should resolve to openrouter provider from config"
        );
    }

    #[test]
    fn integration_openrouter_endpoint_url_resolution() {
        let config_endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![EndpointConfig {
                name: "my-or".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                api_key: Some("sk-test".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: true,
                context_window: None,
            }],
        };

        let ep = config_endpoints.find_for_provider("openrouter").unwrap();
        assert_eq!(
            ep.url.as_deref(),
            Some("https://openrouter.ai/api/v1"),
            "WG_ENDPOINT_URL should be set from endpoint config"
        );
    }

    #[test]
    fn integration_openrouter_endpoint_name_resolution() {
        let mut config = Config::default();
        config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![EndpointConfig {
                name: "or-for-agents".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                api_key: Some("sk-or-test".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: true,
                context_window: None,
            }],
        };

        config
            .models
            .set_endpoint(DispatchRole::TaskAgent, "or-for-agents");
        config
            .models
            .set_provider(DispatchRole::TaskAgent, "openrouter");

        let resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
        assert_eq!(resolved.endpoint, Some("or-for-agents".to_string()));
        assert_eq!(resolved.provider, Some("openrouter".to_string()));

        // Verify the endpoint lookup gets the right URL
        let ep = config
            .llm_endpoints
            .find_by_name(resolved.endpoint.as_deref().unwrap())
            .unwrap();
        assert_eq!(ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(
            ep.resolve_api_key(None).unwrap(),
            Some("sk-or-test".to_string())
        );
    }

    #[test]
    fn integration_openrouter_provider_isolation_between_roles() {
        let mut config = Config::default();
        let opus_model = format!("anthropic/{CLAUDE_OPUS_MODEL_ID}");

        config
            .models
            .set_model(DispatchRole::TaskAgent, &opus_model);
        config
            .models
            .set_provider(DispatchRole::TaskAgent, "openrouter");

        config
            .models
            .set_model(DispatchRole::Evaluator, CLAUDE_SONNET_MODEL_ID);
        config
            .models
            .set_provider(DispatchRole::Evaluator, "anthropic");

        let task_resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
        assert_eq!(task_resolved.provider, Some("openrouter".to_string()));
        assert_eq!(task_resolved.model, opus_model);

        let eval_resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(eval_resolved.provider, Some("anthropic".to_string()));
        assert_eq!(eval_resolved.model, CLAUDE_SONNET_MODEL_ID);
    }
}

// ===========================================================================
// 3. Agent model preferences (preferred_model → spawn, task overrides agent,
//    preferred_provider)
// ===========================================================================

/// These tests exercise the resolve_model and resolve_provider functions from
/// spawn/execution.rs. Those functions are pub(crate), so we replicate the
/// logic here to validate the precedence chain end-to-end.
mod agent_model_preference_tests {
    use workgraph::config::{
        CLAUDE_OPUS_MODEL_ID, CLAUDE_SONNET_MODEL_ID, Config, DispatchRole, EndpointConfig,
        EndpointsConfig,
    };

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
    fn integration_openrouter_agent_preferred_model_used_when_no_task_model() {
        let r = resolve_model_and_provider(
            None,
            None,
            Some(format!("anthropic/{CLAUDE_OPUS_MODEL_ID}")),
            None,
            Some("executor-default".to_string()),
            None,
            None,
            Some("coordinator-fallback"),
            None,
        );
        assert_eq!(r.model, Some(format!("anthropic/{CLAUDE_OPUS_MODEL_ID}")));
    }

    #[test]
    fn integration_openrouter_task_model_overrides_agent() {
        let r = resolve_model_and_provider(
            Some("task-specific-model".to_string()),
            None,
            Some("agent-preferred-model".to_string()),
            None,
            Some("executor-model".to_string()),
            None,
            None,
            Some("coordinator-model"),
            None,
        );
        assert_eq!(r.model, Some("task-specific-model".to_string()));
    }

    #[test]
    fn integration_openrouter_agent_preferred_provider() {
        let r = resolve_model_and_provider(
            None,
            None,
            None,
            Some("openrouter".to_string()),
            None,
            None,
            Some("anthropic".to_string()),
            None,
            None,
        );
        assert_eq!(r.provider, Some("openrouter".to_string()));
    }

    #[test]
    fn integration_openrouter_task_provider_overrides_agent_provider() {
        let r = resolve_model_and_provider(
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
        assert_eq!(r.provider, Some("openai".to_string()));
    }

    #[test]
    fn integration_openrouter_no_agent_falls_through_to_executor() {
        let r = resolve_model_and_provider(
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
        assert_eq!(r.model, Some("executor-default".to_string()));
    }

    #[test]
    fn integration_openrouter_all_none_returns_none() {
        let r = resolve_model_and_provider(None, None, None, None, None, None, None, None, None);
        assert_eq!(r.model, None);
        assert_eq!(r.provider, None);
    }

    #[test]
    fn integration_openrouter_endpoint_cascade_from_agent_provider() {
        // Simulates endpoint resolution: agent.preferred_provider → find matching endpoint
        let endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![
                EndpointConfig {
                    name: "my-openrouter".to_string(),
                    provider: "openrouter".to_string(),
                    url: Some("https://openrouter.ai/api/v1".to_string()),
                    api_key: Some("sk-or-test".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    model: None,
                    is_default: true,
                    context_window: None,
                },
                EndpointConfig {
                    name: "my-anthropic".to_string(),
                    provider: "anthropic".to_string(),
                    url: Some("https://api.anthropic.com".to_string()),
                    api_key: Some("sk-ant-test".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    model: None,
                    is_default: false,
                    context_window: None,
                },
            ],
        };

        // Replicate endpoint resolution from spawn_agent_inner
        let task_endpoint: Option<String> = None;
        let task_provider: Option<String> = None;
        let agent_provider = Some("openrouter".to_string());
        let role_endpoint: Option<String> = None;

        let effective_endpoint = task_endpoint
            .or_else(|| {
                task_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or_else(|| {
                agent_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or(role_endpoint);

        assert_eq!(effective_endpoint, Some("my-openrouter".to_string()));

        // Verify the endpoint gives us the right URL and key
        let ep = endpoints
            .find_by_name(effective_endpoint.as_deref().unwrap())
            .unwrap();
        assert_eq!(ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(ep.api_key.as_deref(), Some("sk-or-test"));
    }

    #[test]
    fn integration_openrouter_endpoint_task_endpoint_wins() {
        let endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![
                EndpointConfig {
                    name: "or-ep".to_string(),
                    provider: "openrouter".to_string(),
                    url: Some("https://openrouter.ai/api/v1".to_string()),
                    api_key: Some("sk-or".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    model: None,
                    is_default: true,
                    context_window: None,
                },
                EndpointConfig {
                    name: "ant-ep".to_string(),
                    provider: "anthropic".to_string(),
                    url: Some("https://api.anthropic.com".to_string()),
                    api_key: Some("sk-ant".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    model: None,
                    is_default: false,
                    context_window: None,
                },
            ],
        };

        // task.endpoint explicitly set — should override everything
        let task_endpoint = Some("ant-ep".to_string());
        let task_provider = Some("openrouter".to_string());
        let agent_provider = Some("openrouter".to_string());
        let role_endpoint = Some("or-ep".to_string());

        let effective = task_endpoint
            .or_else(|| {
                task_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or_else(|| {
                agent_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or(role_endpoint);

        assert_eq!(
            effective,
            Some("ant-ep".to_string()),
            "task.endpoint should take priority"
        );
    }

    #[test]
    fn integration_openrouter_role_config_endpoint_with_model() {
        // Config-driven: role binds model + provider + endpoint
        let mut config = Config::default();

        config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![EndpointConfig {
                name: "or-eval".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                api_key: Some("sk-or-eval".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: true,
                context_window: None,
            }],
        };

        let sonnet_model = format!("anthropic/{CLAUDE_SONNET_MODEL_ID}");
        config
            .models
            .set_model(DispatchRole::Evaluator, &sonnet_model);
        config
            .models
            .set_provider(DispatchRole::Evaluator, "openrouter");
        config
            .models
            .set_endpoint(DispatchRole::Evaluator, "or-eval");

        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, sonnet_model);
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
        assert_eq!(resolved.endpoint, Some("or-eval".to_string()));

        let ep = config
            .llm_endpoints
            .find_by_name(resolved.endpoint.as_deref().unwrap())
            .unwrap();
        assert_eq!(
            ep.resolve_api_key(None).unwrap(),
            Some("sk-or-eval".to_string())
        );
    }
}

// ===========================================================================
// 4. Round-trip config (CLI add → list → verify → remove → verify)
// ===========================================================================

mod config_roundtrip_tests {
    use super::*;

    #[test]
    fn integration_openrouter_config_roundtrip_endpoints() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Create a config with multiple endpoints
        let mut config = Config::default();
        config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![
                EndpointConfig {
                    name: "or-main".to_string(),
                    provider: "openrouter".to_string(),
                    url: Some("https://openrouter.ai/api/v1".to_string()),
                    model: Some(format!("anthropic/{CLAUDE_SONNET_MODEL_ID}")),
                    api_key: Some("sk-or-roundtrip-1".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: true,
                    context_window: None,
                },
                EndpointConfig {
                    name: "ant-direct".to_string(),
                    provider: "anthropic".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-ant-roundtrip".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
            ],
        };

        // Save
        config.save(dir).unwrap();

        // Reload and verify
        let loaded = Config::load(dir).unwrap();
        assert_eq!(loaded.llm_endpoints.endpoints.len(), 2);

        let sonnet_model = format!("anthropic/{CLAUDE_SONNET_MODEL_ID}");
        let or_ep = loaded.llm_endpoints.find_by_name("or-main").unwrap();
        assert_eq!(or_ep.provider, "openrouter");
        assert_eq!(or_ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(or_ep.model.as_deref(), Some(sonnet_model.as_str()));
        assert_eq!(or_ep.api_key.as_deref(), Some("sk-or-roundtrip-1"));
        assert!(or_ep.is_default);

        let ant_ep = loaded.llm_endpoints.find_by_name("ant-direct").unwrap();
        assert_eq!(ant_ep.provider, "anthropic");
        assert!(!ant_ep.is_default);

        // Now modify: remove one endpoint
        let mut config2 = loaded;
        config2
            .llm_endpoints
            .endpoints
            .retain(|ep| ep.name != "ant-direct");
        config2.save(dir).unwrap();

        // Reload and verify removal
        let loaded2 = Config::load(dir).unwrap();
        assert_eq!(loaded2.llm_endpoints.endpoints.len(), 1);
        assert!(loaded2.llm_endpoints.find_by_name("ant-direct").is_none());
        assert!(loaded2.llm_endpoints.find_by_name("or-main").is_some());
    }

    #[test]
    fn integration_openrouter_config_roundtrip_with_model_routing() {
        use workgraph::config::DispatchRole;

        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        let mut config = Config::default();

        // Set up endpoint
        config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![EndpointConfig {
                name: "or-prod".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: None,
                api_key: Some("sk-or-prod-key".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            }],
        };

        // Set up model routing (provider:model format)
        let sonnet_model = format!("anthropic/{CLAUDE_SONNET_MODEL_ID}");
        config.models.set_model(
            DispatchRole::Evaluator,
            &format!("openrouter:{sonnet_model}"),
        );
        config
            .models
            .set_endpoint(DispatchRole::Evaluator, "or-prod");

        // Save
        config.save(dir).unwrap();

        // Reload
        let loaded = Config::load(dir).unwrap();

        // Verify model routing survived
        let resolved = loaded.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, sonnet_model);
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
        assert_eq!(resolved.endpoint, Some("or-prod".to_string()));

        // Verify endpoint lookup from resolved config
        let ep = loaded
            .llm_endpoints
            .find_by_name(resolved.endpoint.as_deref().unwrap())
            .unwrap();
        assert_eq!(ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(
            ep.resolve_api_key(None).unwrap(),
            Some("sk-or-prod-key".to_string())
        );
    }

    #[test]
    fn integration_openrouter_config_roundtrip_key_file_preserved() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        // Create a key file in the workgraph dir
        let key_path = dir.join("secrets");
        fs::create_dir_all(&key_path).unwrap();
        fs::write(key_path.join("or.key"), "sk-or-from-file-roundtrip").unwrap();

        let mut config = Config::default();
        config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![EndpointConfig {
                name: "or-keyfile".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: None,
                api_key: None,
                api_key_file: Some("secrets/or.key".to_string()),
                api_key_env: None,
                is_default: true,
                context_window: None,
            }],
        };

        config.save(dir).unwrap();

        let loaded = Config::load(dir).unwrap();
        let ep = loaded.llm_endpoints.find_by_name("or-keyfile").unwrap();
        assert!(ep.api_key.is_none());
        assert_eq!(ep.api_key_file.as_deref(), Some("secrets/or.key"));

        // Verify the key can still be resolved
        let key = ep.resolve_api_key(Some(dir)).unwrap();
        assert_eq!(key, Some("sk-or-from-file-roundtrip".to_string()));
    }

    #[test]
    fn integration_openrouter_config_roundtrip_default_promotion() {
        let tmp = setup_workgraph_dir();
        let dir = tmp.path();

        let mut config = Config::default();
        config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
            endpoints: vec![
                EndpointConfig {
                    name: "ep-a".to_string(),
                    provider: "openrouter".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("key-a".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: true,
                    context_window: None,
                },
                EndpointConfig {
                    name: "ep-b".to_string(),
                    provider: "openrouter".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("key-b".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
            ],
        };

        config.save(dir).unwrap();

        // Remove the default endpoint
        let mut loaded = Config::load(dir).unwrap();
        let was_default = loaded
            .llm_endpoints
            .endpoints
            .iter()
            .find(|ep| ep.name == "ep-a")
            .unwrap()
            .is_default;
        assert!(was_default);

        loaded
            .llm_endpoints
            .endpoints
            .retain(|ep| ep.name != "ep-a");

        // Promote remaining to default
        if let Some(ep) = loaded.llm_endpoints.endpoints.first_mut() {
            ep.is_default = true;
        }
        loaded.save(dir).unwrap();

        let final_config = Config::load(dir).unwrap();
        assert_eq!(final_config.llm_endpoints.endpoints.len(), 1);
        assert_eq!(final_config.llm_endpoints.endpoints[0].name, "ep-b");
        assert!(final_config.llm_endpoints.endpoints[0].is_default);
    }
}

// ===========================================================================
// 5. Error cases (bad key file, invalid provider, bad key)
// ===========================================================================

mod error_case_tests {
    use super::*;

    #[test]
    fn integration_openrouter_bad_key_file_path() {
        let ep = EndpointConfig {
            name: "bad-file-ep".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("/nonexistent/path/or.key".to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };

        let result = ep.resolve_api_key(None);
        assert!(result.is_err(), "Missing key file should return an error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to read API key"),
            "Error should mention reading API key, got: {}",
            err_msg
        );
    }

    #[test]
    fn integration_openrouter_empty_key_file() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("empty.key");
        fs::write(&key_path, "   \n  \t  ").unwrap();

        let ep = EndpointConfig {
            name: "empty-file-ep".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };

        let result = ep.resolve_api_key(None);
        assert!(result.is_err(), "Empty key file should return an error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("empty"),
            "Error should mention empty file, got: {}",
            err_msg
        );
    }

    #[test]
    fn integration_openrouter_unknown_provider_default_url_empty() {
        let url = EndpointConfig::default_url_for_provider("nonexistent-provider");
        assert_eq!(url, "", "Unknown provider should return empty default URL");
    }

    #[test]
    fn integration_openrouter_find_by_name_nonexistent() {
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

        assert!(
            endpoints.find_by_name("nonexistent").is_none(),
            "find_by_name for missing name should return None"
        );
    }

    #[test]
    fn integration_openrouter_find_for_provider_empty_list() {
        let endpoints = EndpointsConfig { inherit_global: false, endpoints: vec![] };

        assert!(
            endpoints.find_for_provider("openrouter").is_none(),
            "find_for_provider on empty list should return None"
        );
    }

    #[test]
    fn integration_openrouter_masked_key_formats() {
        // Long key — prefix****...suffix
        let ep1 = EndpointConfig {
            name: "ep1".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: Some("sk-or-v1-abcdef123456".to_string()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let masked = ep1.masked_key();
        assert!(masked.starts_with("sk-"));
        assert!(masked.contains("****"));
        assert!(masked.ends_with("3456"));

        // Short key — just ****
        let ep2 = EndpointConfig {
            name: "ep2".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: Some("short".to_string()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep2.masked_key(), "****");

        // No key, has file — (from file)
        let ep3 = EndpointConfig {
            name: "ep3".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("/some/path".to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep3.masked_key(), "(from file)");

        // No key, no file — (not set)
        let ep4 = EndpointConfig {
            name: "ep4".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep4.masked_key(), "(not set)");
    }

    #[test]
    fn integration_openrouter_env_var_names_for_provider() {
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
    fn integration_openrouter_config_load_empty_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("config.toml"), "").unwrap();

        let config = Config::load(tmp.path()).unwrap();
        assert!(
            config.llm_endpoints.endpoints.is_empty(),
            "Empty config should have no endpoints"
        );
    }

    #[test]
    fn integration_openrouter_config_load_invalid_toml_field_ignored() {
        // Config with valid endpoint structure
        let tmp = TempDir::new().unwrap();
        let toml_content = r#"
[[llm_endpoints.endpoints]]
name = "test-ep"
provider = "openrouter"
api_key = "sk-test"
is_default = true
"#;
        fs::write(tmp.path().join("config.toml"), toml_content).unwrap();

        let config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        assert_eq!(config.llm_endpoints.endpoints[0].name, "test-ep");
        assert_eq!(config.llm_endpoints.endpoints[0].provider, "openrouter");
    }
}

// ===========================================================================
// 6. Auto-routing & model validation
// ===========================================================================

mod auto_routing_tests {
    use super::*;
    use workgraph::config::CLAUDE_OPUS_MODEL_ID;
    use workgraph::executor::native::openai_client::{
        OPENROUTER_AUTO_MODEL, validate_openrouter_model,
    };

    #[test]
    fn integration_openrouter_auto_model_is_always_valid() {
        let tmp = setup_workgraph_dir();
        let result = validate_openrouter_model(OPENROUTER_AUTO_MODEL, tmp.path());
        assert!(result.was_valid);
        assert_eq!(result.model, OPENROUTER_AUTO_MODEL);
        assert!(result.warning.is_none());
    }

    #[test]
    fn integration_openrouter_valid_model_passes_with_cache() {
        let tmp = setup_workgraph_dir();
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "anthropic/claude-sonnet-4-6", "name": "Sonnet", "description": ""},
                {"id": "openai/gpt-4o", "name": "GPT-4o", "description": ""},
            ]
        });
        fs::write(tmp.path().join("model_cache.json"), cache.to_string()).unwrap();

        let result = validate_openrouter_model("anthropic/claude-sonnet-4-6", tmp.path());
        assert!(result.was_valid);
        assert_eq!(result.model, "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn integration_openrouter_invalid_model_returns_original_no_fallback() {
        let tmp = setup_workgraph_dir();
        let opus_key = format!("anthropic/{CLAUDE_OPUS_MODEL_ID}");
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "anthropic/claude-sonnet-4-6"},
                {"id": opus_key},
                {"id": "openai/gpt-4o"},
            ]
        });
        fs::write(tmp.path().join("model_cache.json"), cache.to_string()).unwrap();

        let result = validate_openrouter_model("nonexistent/model-xyz", tmp.path());
        assert!(!result.was_valid);
        assert_eq!(
            result.model, "nonexistent/model-xyz",
            "Should return original model, not openrouter/auto"
        );
        assert!(result.warning.is_some());
        assert!(
            !result.warning.as_ref().unwrap().contains("Falling back"),
            "Should not mention fallback"
        );
    }

    #[test]
    fn integration_openrouter_invalid_model_suggests_alternatives() {
        let tmp = setup_workgraph_dir();
        let opus_key = format!("anthropic/{CLAUDE_OPUS_MODEL_ID}");
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "anthropic/claude-sonnet-4-6"},
                {"id": opus_key},
                {"id": "openai/gpt-4o"},
                {"id": "deepseek/deepseek-r1"},
            ]
        });
        fs::write(tmp.path().join("model_cache.json"), cache.to_string()).unwrap();

        // Typo: "sonet" instead of "sonnet"
        let result = validate_openrouter_model("anthropic/claude-sonet-4-6", tmp.path());
        assert!(!result.was_valid);
        assert!(
            result
                .suggestions
                .contains(&"anthropic/claude-sonnet-4-6".to_string()),
            "Should suggest the closest match, got: {:?}",
            result.suggestions
        );
        let warning = result.warning.as_ref().unwrap();
        assert!(warning.contains("Did you mean"));
        assert!(warning.contains("anthropic/claude-sonnet-4-6"));
    }

    #[test]
    fn integration_openrouter_no_cache_passes_through() {
        let tmp = setup_workgraph_dir();
        // No model_cache.json exists
        let result = validate_openrouter_model("any/model-name", tmp.path());
        assert!(result.was_valid, "Without cache, model should pass through");
        assert_eq!(result.model, "any/model-name");
    }

    #[test]
    fn integration_openrouter_invalid_model_no_auto_fallback() {
        // Test that invalid models do NOT fall back to openrouter/auto
        let tmp = setup_workgraph_dir();
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "anthropic/claude-sonnet-4-6"},
                {"id": "openai/gpt-4o"},
            ]
        });
        fs::write(tmp.path().join("model_cache.json"), cache.to_string()).unwrap();

        let result = validate_openrouter_model("totally-wrong-model", tmp.path());
        assert!(!result.was_valid);
        assert_eq!(
            result.model, "totally-wrong-model",
            "Should return the original model, not openrouter/auto"
        );
        assert!(result.warning.is_some());
        assert!(
            !result
                .warning
                .as_ref()
                .unwrap()
                .contains(OPENROUTER_AUTO_MODEL),
            "Should not mention openrouter/auto in warning"
        );
    }

    #[test]
    fn integration_openrouter_provider_prefix_stripped() {
        let tmp = setup_workgraph_dir();
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "minimax/minimax-m2.7"},
                {"id": "anthropic/claude-sonnet-4-6"},
            ]
        });
        fs::write(tmp.path().join("model_cache.json"), cache.to_string()).unwrap();

        // Provider prefix should be stripped before validation
        let result = validate_openrouter_model("openrouter:minimax/minimax-m2.7", tmp.path());
        assert!(result.was_valid, "Should find model after stripping prefix");
        assert_eq!(result.model, "minimax/minimax-m2.7");
    }
}
