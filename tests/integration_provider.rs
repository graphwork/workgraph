//! Integration tests for multi-provider model routing.
//!
//! Tests provider trait routing (bare→Anthropic, prefixed→OpenAI-compatible),
//! endpoint API key resolution, per-role provider routing, model registry
//! lookup, and the fallback chain. Uses mock HTTP servers instead of live APIs.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use tempfile::TempDir;

use workgraph::config::{
    CLAUDE_HAIKU_MODEL_ID, CLAUDE_OPUS_MODEL_ID, CLAUDE_SONNET_MODEL_ID, Config, DispatchRole,
    EndpointConfig, EndpointsConfig, ModelRoutingConfig, RoleModelConfig,
};
use workgraph::executor::native::client::AnthropicClient;
use workgraph::executor::native::openai_client::OpenAiClient;
use workgraph::executor::native::provider::{create_provider, create_provider_ext};
use workgraph::models::{ModelEntry, ModelRegistry, ModelTier};

// ── Mock HTTP helpers ───────────────────────────────────────────────────

/// Anthropic Messages API mock response (minimal valid JSON, non-streaming).
fn anthropic_mock_response(model: &str) -> String {
    format!(
        r#"{{"id":"msg_mock","type":"message","role":"assistant","content":[{{"type":"text","text":"hello from {model}"}}],"model":"{model}","stop_reason":"end_turn","usage":{{"input_tokens":10,"output_tokens":5}}}}"#,
        model = model,
    )
}

/// Anthropic Messages API mock response in SSE streaming format.
fn anthropic_mock_sse_response(model: &str) -> String {
    let msg = format!(
        r#"{{"type":"message_start","message":{{"id":"msg_mock","type":"message","role":"assistant","content":[],"model":"{model}","stop_reason":null,"usage":{{"input_tokens":10,"output_tokens":0}}}}}}"#,
        model = model,
    );
    let text = format!("hello from {model}", model = model,);
    format!(
        "event: message_start\ndata: {msg}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        msg = msg,
        text = text,
    )
}

/// OpenAI Chat Completions mock response (minimal valid JSON).
fn openai_mock_response(model: &str) -> String {
    format!(
        r#"{{"id":"chatcmpl-mock","object":"chat.completion","model":"{model}","choices":[{{"index":0,"message":{{"role":"assistant","content":"hello from {model}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":10,"completion_tokens":5}}}}"#,
        model = model,
    )
}

/// Start a mock HTTP server that responds to any POST with `body`.
/// Returns the base URL (e.g. "http://127.0.0.1:PORT").
/// The server handles exactly `num_requests` requests then stops.
fn start_mock_server(body: String, num_requests: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    thread::spawn(move || {
        for _ in 0..num_requests {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request (we don't care about the content)
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        }
    });

    url
}

/// Record which endpoint path was hit by a request.
fn start_recording_mock_server(
    response_body: String,
    num_requests: usize,
) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());
    let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let paths_clone = Arc::clone(&paths);

    thread::spawn(move || {
        for _ in 0..num_requests {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();

                // Extract path from "POST /v1/messages HTTP/1.1"
                if let Some(first_line) = request.lines().next() {
                    let parts: Vec<&str> = first_line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        paths_clone.lock().unwrap().push(parts[1].to_string());
                    }
                }

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body,
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        }
    });

    (url, paths)
}

fn setup_workgraph_dir() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();
    tmp
}

// ── Provider routing: bare→Anthropic, prefixed→OpenAI ───────────────────

#[test]
fn test_bare_model_routes_to_anthropic() {
    // A bare model name (no slash) should route to Anthropic API
    let mock_body = anthropic_mock_response("claude-sonnet-4-6");
    let (base_url, paths) = start_recording_mock_server(mock_body, 1);

    let client = AnthropicClient::new("test-key".to_string(), "claude-sonnet-4-6")
        .unwrap()
        .with_base_url(&base_url);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let request = workgraph::executor::native::client::MessagesRequest {
        model: "claude-sonnet-4-6".to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![workgraph::executor::native::client::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        }],
        tools: vec![],
        stream: false,
    };

    let response = rt.block_on(client.messages(&request)).unwrap();
    assert_eq!(response.id, "msg_mock");
    assert_eq!(
        paths.lock().unwrap().first().unwrap(),
        "/v1/messages",
        "Anthropic client should hit /v1/messages"
    );
}

#[test]
fn test_prefixed_model_routes_to_openai() {
    // A model with slash (provider/model) should route to OpenAI-compatible API
    let mock_body = openai_mock_response("openai/gpt-4o");
    let (base_url, paths) = start_recording_mock_server(mock_body, 1);

    let client =
        OpenAiClient::new("test-key".to_string(), "openai/gpt-4o", Some(&base_url)).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let request = workgraph::executor::native::client::MessagesRequest {
        model: "openai/gpt-4o".to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![workgraph::executor::native::client::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        }],
        tools: vec![],
        stream: false,
    };

    use workgraph::executor::native::provider::Provider;
    let response = rt.block_on(client.send(&request)).unwrap();
    assert_eq!(response.id, "chatcmpl-mock");
    assert_eq!(
        paths.lock().unwrap().first().unwrap(),
        "/chat/completions",
        "OpenAI client should hit /chat/completions (base_url includes /v1 when set)"
    );
}

#[test]
fn test_create_provider_bare_is_anthropic() {
    // create_provider with a bare model name should choose Anthropic
    let tmp = setup_workgraph_dir();

    // Unset WG_LLM_PROVIDER to avoid environment interference in CI/CI-like envs
    // Unset WG_LLM_PROVIDER to avoid environment interference in CI/CI-like envs
    unsafe { std::env::remove_var("WG_LLM_PROVIDER") };

    // Write config with endpoint pointing to our mock server
    let mock_body = anthropic_mock_response("claude-haiku-4-5");
    let base_url = start_mock_server(mock_body, 1);

    let config_content = format!(
        r#"
[[llm_endpoints.endpoints]]
name = "test-anthropic"
provider = "anthropic"
url = "{base_url}"
api_key = "test-anthropic-key"
is_default = true

[[llm_endpoints.endpoints]]
name = "test-openai"
provider = "openai"
url = "{base_url}"
api_key = "test-openai-key"
"#,
        base_url = base_url,
    );
    std::fs::write(tmp.path().join("config.toml"), config_content).unwrap();

    let provider = create_provider(tmp.path(), "claude-haiku-4-5").unwrap();
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(provider.model(), "claude-haiku-4-5");
}

#[test]
fn test_create_provider_prefixed_is_openai() {
    // create_provider with a prefixed model should choose OpenAI-compatible
    let tmp = setup_workgraph_dir();

    let mock_body = openai_mock_response("deepseek/deepseek-chat");
    let base_url = start_mock_server(mock_body, 1);

    let config_content = format!(
        r#"
[native_executor]
provider = "openai"

[[llm_endpoints.endpoints]]
name = "test-openai"
provider = "openai"
url = "{base_url}"
api_key = "test-openai-key"
is_default = true
"#,
        base_url = base_url,
    );
    std::fs::write(tmp.path().join("config.toml"), config_content).unwrap();

    let provider = create_provider(tmp.path(), "deepseek/deepseek-chat").unwrap();
    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.model(), "deepseek/deepseek-chat");
}

#[test]
fn test_create_provider_ext_override() {
    // create_provider_ext with explicit provider_override should use that
    let tmp = setup_workgraph_dir();

    let mock_body = openai_mock_response("my-custom-model");
    let base_url = start_mock_server(mock_body, 1);

    let config_content = format!(
        r#"
[[llm_endpoints.endpoints]]
name = "test-openai"
provider = "openai"
url = "{base_url}"
api_key = "test-openai-key"
"#,
        base_url = base_url,
    );
    std::fs::write(tmp.path().join("config.toml"), config_content).unwrap();

    // Even though "my-custom-model" has no slash, forcing openai provider
    let provider =
        create_provider_ext(tmp.path(), "my-custom-model", Some("openai"), None, None).unwrap();
    assert_eq!(provider.name(), "openai");
}

// ── Endpoint API key resolution ─────────────────────────────────────────

#[test]
fn test_endpoint_api_key_resolution() {
    // find_for_provider should return the endpoint with matching provider
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
                name: "openai-prod".to_string(),
                provider: "openai".to_string(),
                url: Some("https://api.openai.com".to_string()),
                api_key: Some("sk-oai-prod".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: false,
                context_window: None,
            },
            EndpointConfig {
                name: "openrouter".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api".to_string()),
                api_key: Some("sk-or-key".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: false,
                context_window: None,
            },
        ],
    };

    let anthropic = endpoints.find_for_provider("anthropic").unwrap();
    assert_eq!(anthropic.api_key.as_deref(), Some("sk-ant-prod"));
    assert!(anthropic.is_default);

    let openai = endpoints.find_for_provider("openai").unwrap();
    assert_eq!(openai.api_key.as_deref(), Some("sk-oai-prod"));

    let openrouter = endpoints.find_for_provider("openrouter").unwrap();
    assert_eq!(openrouter.api_key.as_deref(), Some("sk-or-key"));

    assert!(endpoints.find_for_provider("local").is_none());
}

#[test]
fn test_endpoint_default_selection() {
    // When multiple endpoints share a provider, is_default=true wins
    let endpoints = EndpointsConfig {
        inherit_global: false,
        endpoints: vec![
            EndpointConfig {
                name: "anthropic-staging".to_string(),
                provider: "anthropic".to_string(),
                url: Some("https://staging.anthropic.com".to_string()),
                api_key: Some("sk-staging".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: false,
                context_window: None,
            },
            EndpointConfig {
                name: "anthropic-prod".to_string(),
                provider: "anthropic".to_string(),
                url: Some("https://api.anthropic.com".to_string()),
                api_key: Some("sk-prod".to_string()),
                api_key_file: None,
                api_key_env: None,
                model: None,
                is_default: true,
                context_window: None,
            },
        ],
    };

    let ep = endpoints.find_for_provider("anthropic").unwrap();
    assert_eq!(ep.name, "anthropic-prod");
    assert_eq!(ep.api_key.as_deref(), Some("sk-prod"));
}

// ── Per-role provider routing ───────────────────────────────────────────

#[test]
fn test_per_role_different_providers() {
    // Configure different providers per DispatchRole
    let mut config = Config::default();

    // Triage → OpenAI (budget model)
    config.models.set_model(DispatchRole::Triage, "gpt-4o-mini");
    config.models.set_provider(DispatchRole::Triage, "openai");

    // Evaluator → Anthropic (high-capability model)
    config
        .models
        .set_model(DispatchRole::Evaluator, "claude-sonnet-4-6");
    config
        .models
        .set_provider(DispatchRole::Evaluator, "anthropic");

    // TaskAgent → OpenRouter (frontier model)
    config.models.set_model(
        DispatchRole::TaskAgent,
        &format!("anthropic/{CLAUDE_OPUS_MODEL_ID}"),
    );
    config
        .models
        .set_provider(DispatchRole::TaskAgent, "openrouter");

    // Verify each role resolves to its own provider
    let triage = config.resolve_model_for_role(DispatchRole::Triage);
    assert_eq!(triage.model, "gpt-4o-mini");
    assert_eq!(triage.provider, Some("openai".to_string()));

    let evaluator = config.resolve_model_for_role(DispatchRole::Evaluator);
    assert_eq!(evaluator.model, "claude-sonnet-4-6");
    assert_eq!(evaluator.provider, Some("anthropic".to_string()));

    let task_agent = config.resolve_model_for_role(DispatchRole::TaskAgent);
    assert_eq!(
        task_agent.model,
        format!("anthropic/{CLAUDE_OPUS_MODEL_ID}")
    );
    assert_eq!(task_agent.provider, Some("openrouter".to_string()));
}

#[test]
fn test_per_role_isolation() {
    // Setting provider on one role should not affect other roles
    let mut config = Config::default();

    config.models.set_model(DispatchRole::Triage, "gpt-4o-mini");
    config.models.set_provider(DispatchRole::Triage, "openai");

    // Evaluator should not inherit Triage's provider (gets its own from tier resolution)
    let evaluator = config.resolve_model_for_role(DispatchRole::Evaluator);
    assert_ne!(
        evaluator.provider,
        Some("openai".to_string()),
        "Evaluator should not inherit Triage's provider"
    );
}

#[test]
fn test_per_role_with_mock_servers() {
    // End-to-end: create providers for different roles using mock servers
    let tmp = setup_workgraph_dir();

    let anthropic_body = anthropic_mock_sse_response("claude-sonnet-4-6");
    let openai_body = openai_mock_response("gpt-4o-mini");

    let anthropic_url = start_mock_server(anthropic_body.clone(), 1);
    let openai_url = start_mock_server(openai_body.clone(), 1);

    let config_content = format!(
        r#"
[[llm_endpoints.endpoints]]
name = "anthropic"
provider = "anthropic"
url = "{anthropic_url}"
api_key = "test-ant-key"
is_default = true

[[llm_endpoints.endpoints]]
name = "openai"
provider = "openai"
url = "{openai_url}"
api_key = "test-oai-key"
is_default = true

[models.evaluator]
model = "native:claude-sonnet-4-6"

[models.triage]
model = "openai:gpt-4o-mini"
"#,
    );
    std::fs::write(tmp.path().join("config.toml"), config_content).unwrap();

    // The evaluator should route to Anthropic
    let eval_provider = create_provider_ext(
        tmp.path(),
        "claude-sonnet-4-6",
        Some("anthropic"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(eval_provider.name(), "anthropic");

    // The triage should route to OpenAI
    let triage_provider =
        create_provider_ext(tmp.path(), "gpt-4o-mini", Some("openai"), None, None).unwrap();
    assert_eq!(triage_provider.name(), "openai");

    // Verify they actually hit different API endpoints
    let rt = tokio::runtime::Runtime::new().unwrap();
    let request = workgraph::executor::native::client::MessagesRequest {
        model: "test".to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![workgraph::executor::native::client::ContentBlock::Text {
                text: "test".to_string(),
            }],
        }],
        tools: vec![],
        stream: false,
    };

    let resp = rt.block_on(eval_provider.send(&request)).unwrap();
    assert!(resp.content.iter().any(|b| matches!(b,
        workgraph::executor::native::client::ContentBlock::Text { text } if text.contains("claude-sonnet")
    )));

    let resp = rt.block_on(triage_provider.send(&request)).unwrap();
    assert!(resp.content.iter().any(|b| matches!(b,
        workgraph::executor::native::client::ContentBlock::Text { text } if text.contains("gpt-4o")
    )));
}

// ── Fallback chain ──────────────────────────────────────────────────────

#[test]
fn test_fallback_chain_role_to_default_to_agent() {
    // Resolution: role-specific → models.default → agent.model
    let mut config = Config::default();

    // 1. No config at all → resolves via Premium tier → opus registry entry
    let resolved = config.resolve_model_for_role(DispatchRole::Evolver);
    assert_eq!(
        resolved.model, CLAUDE_OPUS_MODEL_ID,
        "Without any config, Evolver should resolve via Premium tier"
    );
    assert_eq!(resolved.provider, Some("anthropic".to_string()));

    // 2. Set models.default → tier resolution still takes priority for Evolver (Premium tier)
    //    but default provider cascades through tier resolution
    config.models.default = Some(RoleModelConfig {
        model: Some("default-model".to_string()),
        provider: Some("openrouter".to_string()),
        tier: None,
        endpoint: None,
    });
    let resolved = config.resolve_model_for_role(DispatchRole::Evolver);
    assert_eq!(
        resolved.model, CLAUDE_OPUS_MODEL_ID,
        "Tier resolution (step 4) takes priority over models.default (step 5)"
    );
    assert_eq!(
        resolved.provider,
        Some("openrouter".to_string()),
        "Default provider should cascade through tier resolution"
    );

    // 3. Set role-specific → should override default
    config
        .models
        .set_model(DispatchRole::Evolver, "evolver-model");
    config
        .models
        .set_provider(DispatchRole::Evolver, "anthropic");
    let resolved = config.resolve_model_for_role(DispatchRole::Evolver);
    assert_eq!(resolved.model, "evolver-model");
    assert_eq!(resolved.provider, Some("anthropic".to_string()));
}

#[test]
fn test_fallback_tier_defaults() {
    // Some roles have built-in tier defaults (between legacy and models.default)
    let config = Config::default();

    // Triage → Fast tier → haiku
    assert_eq!(
        config.resolve_model_for_role(DispatchRole::Triage).model,
        CLAUDE_HAIKU_MODEL_ID
    );

    // Compactor → Fast tier → haiku
    assert_eq!(
        config.resolve_model_for_role(DispatchRole::Compactor).model,
        CLAUDE_HAIKU_MODEL_ID
    );

    // FlipInference → Fast tier → haiku
    assert_eq!(
        config
            .resolve_model_for_role(DispatchRole::FlipInference)
            .model,
        CLAUDE_HAIKU_MODEL_ID
    );

    // Verification → Standard tier → sonnet
    assert_eq!(
        config
            .resolve_model_for_role(DispatchRole::Verification)
            .model,
        CLAUDE_SONNET_MODEL_ID
    );

    // Evaluator → Fast tier → haiku
    assert_eq!(
        config.resolve_model_for_role(DispatchRole::Evaluator).model,
        CLAUDE_HAIKU_MODEL_ID
    );
}

#[test]
fn test_models_section_overrides_tier() {
    // [models.triage] should override tier defaults
    let mut config = Config::default();
    config.models.set_model(DispatchRole::Triage, "new-model");
    config.models.set_provider(DispatchRole::Triage, "openai");

    let resolved = config.resolve_model_for_role(DispatchRole::Triage);
    assert_eq!(resolved.model, "new-model");
    assert_eq!(resolved.provider, Some("openai".to_string()));
}

// ── Model registry lookup ───────────────────────────────────────────────

#[test]
fn test_model_registry_default_models() {
    let registry = ModelRegistry::with_defaults();

    // Should contain models from multiple providers
    assert!(
        registry
            .get(&format!("anthropic/{CLAUDE_OPUS_MODEL_ID}"))
            .is_some()
    );
    assert!(registry.get("openai/gpt-4o").is_some());
    assert!(registry.get("deepseek/deepseek-chat").is_some());
    assert!(registry.get("google/gemini-2.5-pro").is_some());
}

#[test]
fn test_model_registry_tier_classification() {
    let registry = ModelRegistry::with_defaults();

    let opus = registry
        .get(&format!("anthropic/{CLAUDE_OPUS_MODEL_ID}"))
        .unwrap();
    assert_eq!(opus.tier, ModelTier::Frontier);

    let haiku = registry.get("anthropic/claude-haiku-4-5").unwrap();
    assert_eq!(haiku.tier, ModelTier::Budget);

    let sonnet = registry.get("anthropic/claude-sonnet-4-6").unwrap();
    assert_eq!(sonnet.tier, ModelTier::Mid);
}

#[test]
fn test_model_registry_persistence() {
    let tmp = TempDir::new().unwrap();
    let mut registry = ModelRegistry::with_defaults();

    // Add custom model
    registry.add(ModelEntry {
        id: "custom/test-model".to_string(),
        provider: "custom".to_string(),
        cost_per_1m_input: 1.0,
        cost_per_1m_output: 2.0,
        context_window: 32_000,
        capabilities: vec!["coding".to_string()],
        tier: ModelTier::Mid,
    });

    registry.save(tmp.path()).unwrap();
    let loaded = ModelRegistry::load(tmp.path()).unwrap();

    assert!(loaded.get("custom/test-model").is_some());
    assert_eq!(loaded.get("custom/test-model").unwrap().provider, "custom");
}

#[test]
fn test_model_registry_provider_filtering() {
    let registry = ModelRegistry::with_defaults();

    let frontier = registry.list(Some(&ModelTier::Frontier));
    assert!(
        frontier.len() >= 2,
        "Should have at least 2 frontier models"
    );
    for m in &frontier {
        assert_eq!(m.tier, ModelTier::Frontier);
    }

    let budget = registry.list(Some(&ModelTier::Budget));
    assert!(budget.len() >= 3, "Should have at least 3 budget models");
    for m in &budget {
        assert_eq!(m.tier, ModelTier::Budget);
    }
}

// ── Provider send via mock servers ──────────────────────────────────────

#[tokio::test]
async fn test_anthropic_provider_send_via_mock() {
    let mock_body = anthropic_mock_sse_response("claude-haiku-4-5");
    let base_url = start_mock_server(mock_body, 1);

    let client = AnthropicClient::new("mock-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&base_url);

    use workgraph::executor::native::provider::Provider;
    let request = workgraph::executor::native::client::MessagesRequest {
        model: "claude-haiku-4-5".to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![workgraph::executor::native::client::ContentBlock::Text {
                text: "test".to_string(),
            }],
        }],
        tools: vec![],
        stream: false,
    };

    let response = client.send(&request).await.unwrap();
    assert_eq!(response.id, "msg_mock");
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
}

#[tokio::test]
async fn test_openai_provider_send_via_mock() {
    let mock_body = openai_mock_response("gpt-4o-mini");
    let base_url = start_mock_server(mock_body, 1);

    let client = OpenAiClient::new("mock-key".to_string(), "gpt-4o-mini", Some(&base_url)).unwrap();

    use workgraph::executor::native::provider::Provider;
    let request = workgraph::executor::native::client::MessagesRequest {
        model: "gpt-4o-mini".to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![workgraph::executor::native::client::ContentBlock::Text {
                text: "test".to_string(),
            }],
        }],
        tools: vec![],
        stream: false,
    };

    let response = client.send(&request).await.unwrap();
    assert_eq!(response.id, "chatcmpl-mock");
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
}

// ── All dispatch roles resolve ──────────────────────────────────────────

#[test]
fn test_all_dispatch_roles_resolve_without_panic() {
    let config = Config::default();

    for role in DispatchRole::ALL {
        let resolved = config.resolve_model_for_role(*role);
        assert!(
            !resolved.model.is_empty(),
            "Role {:?} should resolve to a non-empty model",
            role
        );
    }
}

#[test]
fn test_all_dispatch_roles_with_full_config() {
    let mut config = Config::default();

    // Set every role to a different model+provider
    // (input_model, provider) pairs for setting config
    let assignments: &[(DispatchRole, &str, &str)] = &[
        (DispatchRole::TaskAgent, "opus", "anthropic"),
        (DispatchRole::Evaluator, "sonnet", "anthropic"),
        (DispatchRole::FlipInference, "gpt-4o", "openai"),
        (DispatchRole::FlipComparison, "haiku", "anthropic"),
        (DispatchRole::Assigner, "gpt-4o-mini", "openai"),
        (DispatchRole::Evolver, "deepseek-r1", "openrouter"),
        (DispatchRole::Verification, "opus", "anthropic"),
        (DispatchRole::Triage, "haiku", "anthropic"),
        (DispatchRole::Creator, "sonnet", "anthropic"),
        (DispatchRole::Compactor, "gpt-4o-mini", "openai"),
    ];

    for &(role, model, provider) in assignments {
        config.models.set_model(role, model);
        config.models.set_provider(role, provider);
    }

    // Registry IDs get resolved to full API model names; non-registry models pass through
    let expected: &[(DispatchRole, &str, &str)] = &[
        (DispatchRole::TaskAgent, CLAUDE_OPUS_MODEL_ID, "anthropic"),
        (DispatchRole::Evaluator, CLAUDE_SONNET_MODEL_ID, "anthropic"),
        (DispatchRole::FlipInference, "gpt-4o", "openai"),
        (
            DispatchRole::FlipComparison,
            CLAUDE_HAIKU_MODEL_ID,
            "anthropic",
        ),
        (DispatchRole::Assigner, "gpt-4o-mini", "openai"),
        (DispatchRole::Evolver, "deepseek-r1", "openrouter"),
        (
            DispatchRole::Verification,
            CLAUDE_OPUS_MODEL_ID,
            "anthropic",
        ),
        (DispatchRole::Triage, CLAUDE_HAIKU_MODEL_ID, "anthropic"),
        (DispatchRole::Creator, CLAUDE_SONNET_MODEL_ID, "anthropic"),
        (DispatchRole::Compactor, "gpt-4o-mini", "openai"),
    ];

    for &(role, expected_model, expected_provider) in expected {
        let resolved = config.resolve_model_for_role(role);
        assert_eq!(
            resolved.model, expected_model,
            "Role {:?} model mismatch",
            role
        );
        assert_eq!(
            resolved.provider.as_deref(),
            Some(expected_provider),
            "Role {:?} provider mismatch",
            role
        );
    }
}

// ── Config serialization round-trip ─────────────────────────────────────

#[test]
fn test_model_routing_config_toml_roundtrip() {
    let toml_str = r#"
[models.default]
model = "sonnet"
provider = "anthropic"

[models.triage]
model = "gpt-4o-mini"
provider = "openai"

[models.evaluator]
model = "opus"
provider = "anthropic"
"#;

    #[derive(serde::Deserialize)]
    struct Wrapper {
        models: ModelRoutingConfig,
    }

    let wrapper: Wrapper = toml::from_str(toml_str).unwrap();

    let default = wrapper.models.get_role(DispatchRole::Default).unwrap();
    assert_eq!(default.model.as_deref(), Some("sonnet"));
    assert_eq!(default.provider.as_deref(), Some("anthropic"));

    let triage = wrapper.models.get_role(DispatchRole::Triage).unwrap();
    assert_eq!(triage.model.as_deref(), Some("gpt-4o-mini"));
    assert_eq!(triage.provider.as_deref(), Some("openai"));

    let evaluator = wrapper.models.get_role(DispatchRole::Evaluator).unwrap();
    assert_eq!(evaluator.model.as_deref(), Some("opus"));
    assert_eq!(evaluator.provider.as_deref(), Some("anthropic"));
}

// ── API key override in create_provider_ext ─────────────────────────────

#[test]
fn test_create_provider_ext_api_key_override() {
    // When api_key_override is set, it should be used instead of endpoint config keys
    let tmp = setup_workgraph_dir();

    let mock_body = openai_mock_response("test-model");
    let base_url = start_mock_server(mock_body, 1);

    let config_content = format!(
        r#"
[[llm_endpoints.endpoints]]
name = "test-openai"
provider = "openai"
url = "{base_url}"
api_key = "config-key-should-not-be-used"
is_default = true
"#,
        base_url = base_url,
    );
    std::fs::write(tmp.path().join("config.toml"), config_content).unwrap();

    // The override key should be used (provider is created successfully)
    let provider = create_provider_ext(
        tmp.path(),
        "test/model",
        Some("openai"),
        Some("test-openai"),
        Some("override-api-key"),
    )
    .unwrap();
    assert_eq!(provider.name(), "openai");
}

#[test]
fn test_create_provider_ext_endpoint_name_resolves_key() {
    // When endpoint_name is set, its API key and URL should be used
    let tmp = setup_workgraph_dir();

    let mock_body = openai_mock_response("test-model");
    let base_url = start_mock_server(mock_body, 1);

    let config_content = format!(
        r#"
[[llm_endpoints.endpoints]]
name = "my-endpoint"
provider = "openai"
url = "{base_url}"
api_key = "endpoint-specific-key"
"#,
        base_url = base_url,
    );
    std::fs::write(tmp.path().join("config.toml"), config_content).unwrap();

    let provider = create_provider_ext(
        tmp.path(),
        "test/model",
        Some("openai"),
        Some("my-endpoint"),
        None,
    )
    .unwrap();
    assert_eq!(provider.name(), "openai");
}
