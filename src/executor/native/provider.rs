//! Provider trait and model-based routing.
//!
//! The `Provider` trait abstracts over LLM API wire formats (Anthropic Messages,
//! OpenAI Chat Completions). Implementations handle headers, request/response
//! serialization, and tool call encoding while the agent loop works with a
//! uniform interface.
//!
//! Use `create_provider()` to route a model string to the appropriate backend:
//! - Bare name (`claude-sonnet-4-5-20250514`) → Anthropic native API
//! - Prefixed (`openai/gpt-4o`, `deepseek/deepseek-chat`) → OpenAI-compatible

use std::path::Path;

use anyhow::{Context, Result};

use super::client::{AnthropicClient, MessagesRequest, MessagesResponse};
use super::openai_client::OpenAiClient;

/// Provider-agnostic LLM client trait.
///
/// Both `AnthropicClient` and `OpenAiClient` implement this trait so the
/// agent loop can work with any backend without knowing wire format details.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Provider name for logging (e.g., "anthropic", "openai").
    fn name(&self) -> &str;

    /// The model this provider is configured with.
    fn model(&self) -> &str;

    /// Maximum tokens per response.
    fn max_tokens(&self) -> u32;

    /// Context window size in tokens for this provider/model combination.
    fn context_window(&self) -> usize {
        200_000
    }

    /// Send a completion request and return the response.
    ///
    /// The provider translates between the canonical message format and
    /// its wire protocol.
    async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse>;

    /// Send a streaming completion request with incremental text callbacks.
    ///
    /// `on_text` is called for each text chunk as it arrives from the SSE
    /// stream, enabling progressive display. Returns the full assembled
    /// response as with `send()`. Default: falls back to `send()`.
    async fn send_streaming(
        &self,
        request: &MessagesRequest,
        on_text: &(dyn Fn(String) + Send + Sync),
    ) -> Result<MessagesResponse> {
        let _ = on_text;
        self.send(request).await
    }
}

/// Backward-compatible wrapper: routes by model string only.
pub fn create_provider(workgraph_dir: &Path, model: &str) -> Result<Box<dyn Provider>> {
    create_provider_ext(workgraph_dir, model, None, None, None)
}

/// Create a provider, optionally overriding the provider name, endpoint, and/or API key.
///
/// Resolution order for API key:
/// 1. `api_key_override` parameter (pre-resolved by spawn path)
/// 2. Environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.)
/// 3. Matching endpoint entry in config (by name if `endpoint_name` is set, otherwise by provider)
/// 4. `[native_executor]` section in config (legacy fallback)
///
/// Resolution order for base URL:
/// 1. Matching endpoint entry's `url` field
/// 2. Environment variables (`OPENAI_BASE_URL`, etc.) — OpenAI-family only
/// 3. `[native_executor]` section's `api_base` field
pub fn create_provider_ext(
    workgraph_dir: &Path,
    model: &str,
    provider_override: Option<&str>,
    endpoint_name: Option<&str>,
    api_key_override: Option<&str>,
) -> Result<Box<dyn Provider>> {
    let config = crate::config::Config::load(workgraph_dir).unwrap_or_default();

    let config_path = workgraph_dir.join("config.toml");
    let config_val: Option<toml::Value> = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|c| toml::from_str(&c).ok());

    let native_cfg = config_val.as_ref().and_then(|v| v.get("native_executor"));

    // Parse unified provider:model spec (e.g. "openrouter:deepseek/deepseek-v3.2").
    // When a known provider prefix is present, it takes priority over all other
    // provider resolution paths.
    let spec = crate::config::parse_model_spec(model);
    // Keep the original prefix for URL resolution (e.g., "ollama" → localhost:11434)
    let original_prefix = spec.provider.clone();
    let spec_provider = spec
        .provider
        .as_deref()
        .map(crate::config::provider_to_native_provider)
        .map(String::from);

    // Resolve provider name: spec prefix > override > config > env var > model heuristic
    let provider_name = spec_provider
        .or_else(|| provider_override.map(String::from))
        .or_else(|| {
            native_cfg
                .and_then(|c| c.get("provider"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| std::env::var("WG_LLM_PROVIDER").ok())
        .unwrap_or_else(|| {
            // Legacy heuristic fallback for bare model names
            if spec.model_id.starts_with("anthropic/") {
                "anthropic".to_string()
            } else if spec.model_id.contains('/') {
                "openai".to_string()
            } else {
                "anthropic".to_string()
            }
        });

    // Use the parsed model ID (provider prefix stripped) for API calls.
    // Also strip legacy "anthropic/" prefix for backward compatibility.
    let model = if provider_name == "anthropic" {
        spec.model_id
            .strip_prefix("anthropic/")
            .unwrap_or(&spec.model_id)
    } else {
        &spec.model_id
    };

    // Look up endpoint config: by name first, then by provider, then default endpoint
    let endpoint = endpoint_name
        .and_then(|name| config.llm_endpoints.find_by_name(name))
        .or_else(|| config.llm_endpoints.find_for_provider(&provider_name))
        .or_else(|| config.llm_endpoints.find_default());
    let endpoint_key =
        endpoint.and_then(|ep| ep.resolve_api_key(Some(workgraph_dir)).ok().flatten());
    let endpoint_url = endpoint.and_then(|ep| ep.url.clone());
    let endpoint_context_window = endpoint.and_then(|ep| ep.context_window);

    // Resolve context window: endpoint config > model registry > provider default
    let registry_context_window = config
        .effective_registry()
        .into_iter()
        .find(|e| e.model == spec.model_id || e.id == spec.model_id)
        .and_then(|e| if e.context_window > 0 { Some(e.context_window) } else { None });
    let resolved_context_window = endpoint_context_window.or(registry_context_window);

    let api_base: Option<String> = endpoint_url
        .or_else(|| std::env::var("WG_ENDPOINT_URL").ok())
        .or_else(|| {
            // OpenAI-family env var base URLs
            if matches!(provider_name.as_str(), "openai" | "openrouter" | "local") {
                std::env::var("OPENAI_BASE_URL")
                    .or_else(|_| std::env::var("OPENROUTER_BASE_URL"))
                    .ok()
            } else {
                None
            }
        })
        .or_else(|| {
            native_cfg
                .and_then(|c| c.get("api_base"))
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    let max_tokens = native_cfg
        .and_then(|c| c.get("max_tokens"))
        .and_then(|v| v.as_integer())
        .map(|v| v as u32);

    match provider_name.as_str() {
        "openai" | "openrouter" | "local" => {
            // Resolve API key. Priority: override > env var > endpoint config > native_executor (legacy)
            let env_key = ["WG_API_KEY", "OPENROUTER_API_KEY", "OPENAI_API_KEY"]
                .iter()
                .find_map(|v| std::env::var(v).ok().filter(|k| !k.is_empty()));
            let resolved_key = api_key_override
                .map(String::from)
                .or(env_key)
                .or(endpoint_key);

            let mut client = if let Some(key) = resolved_key {
                OpenAiClient::new(key, model, None)
            } else if provider_name == "local" {
                // Local providers (Ollama, vLLM) don't require auth
                OpenAiClient::new("local".to_string(), model, None)
            } else {
                // Legacy fallback: native_executor api_key
                OpenAiClient::from_env(model).or_else(|_| {
                    let key = super::client::resolve_api_key_from_dir(workgraph_dir)?;
                    OpenAiClient::new(key, model, None)
                })
            }
            .context("Failed to initialize OpenAI-compatible client")?;
            client = client.with_provider_hint(&provider_name);
            if let Some(base) = api_base {
                client = client.with_base_url(&base);
            } else {
                // Fall back to the provider's known default URL so that non-OpenRouter
                // providers (e.g. "openai", "local") don't silently hit the OpenRouter
                // endpoint via OpenAiClient's DEFAULT_BASE_URL.
                // Use the original provider prefix (e.g., "ollama", "gemini") for URL
                // lookup, falling back to the resolved provider_name.
                let url_lookup = original_prefix.as_deref().unwrap_or(&provider_name);
                let default_url =
                    crate::config::EndpointConfig::default_url_for_provider(url_lookup);
                if !default_url.is_empty() {
                    client = client.with_base_url(default_url);
                }
            }
            if let Some(mt) = max_tokens {
                client = client.with_max_tokens(mt);
            }
            if let Some(cw) = resolved_context_window {
                client = client.with_context_window(cw as usize);
            }
            // Validate model against cached OpenRouter model list (openrouter only)
            if provider_name == "openrouter" {
                let validation =
                    super::openai_client::validate_openrouter_model(&client.model, workgraph_dir);
                if let Some(ref warning) = validation.warning {
                    eprintln!("[native-exec] WARNING: {}", warning);
                }
                if !validation.was_valid {
                    anyhow::bail!(
                        "Model '{}' not found in OpenRouter model list. {}",
                        client.model,
                        validation.warning.as_deref().unwrap_or(
                            "Run `wg models search <name>` to find valid alternatives."
                        )
                    );
                }
            }
            eprintln!(
                "[native-exec] Using OpenAI-compatible provider ({})",
                client.model
            );
            Ok(Box::new(client))
        }
        _ => {
            // Resolve API key. Priority: override > env var > endpoint config > from_env fallbacks
            let env_key = std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|k| !k.is_empty());
            let mut client = if let Some(key) = api_key_override {
                AnthropicClient::new(key.to_string(), model)
            } else if let Some(key) = env_key {
                AnthropicClient::new(key, model)
            } else if let Some(key) = endpoint_key {
                AnthropicClient::new(key, model)
            } else {
                // Legacy fallback: ~/.config/anthropic/api_key, etc.
                AnthropicClient::from_env(model)
            }
            .context("Failed to initialize Anthropic client")?;
            if let Some(base) = api_base {
                client = client.with_base_url(&base);
            }
            if let Some(mt) = max_tokens {
                client = client.with_max_tokens(mt);
            }
            eprintln!("[native-exec] Using Anthropic provider ({})", client.model);
            Ok(Box::new(client))
        }
    }
}
