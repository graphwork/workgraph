//! Provider trait and model-based routing.
//!
//! The `Provider` trait abstracts over LLM API wire formats (Anthropic Messages,
//! OpenAI Chat Completions). Implementations handle headers, request/response
//! serialization, and tool call encoding while the agent loop works with a
//! uniform interface.
//!
//! Use `create_provider()` to route a model string to the appropriate backend:
//! - Bare name (`claude-sonnet-4-5-20250514`) → Anthropic native API
//! - Prefixed (`openai/gpt-4o`, `deepseek/deepseek-chat-v3`) → OpenAI-compatible

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

    /// Send a completion request and return the response.
    ///
    /// The provider translates between the canonical message format and
    /// its wire protocol.
    async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse>;
}

/// Backward-compatible wrapper: routes by model string only.
pub fn create_provider(workgraph_dir: &Path, model: &str) -> Result<Box<dyn Provider>> {
    create_provider_ext(workgraph_dir, model, None)
}

/// Create a provider, optionally overriding the provider name.
///
/// Resolution order for API key and base URL:
/// 1. Matching `[[llm_endpoints]]` entry in config (by provider name)
/// 2. `[native_executor]` section in config (`api_base`)
/// 3. Environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.)
pub fn create_provider_ext(
    workgraph_dir: &Path,
    model: &str,
    provider_override: Option<&str>,
) -> Result<Box<dyn Provider>> {
    let config = crate::config::Config::load(workgraph_dir).unwrap_or_default();

    let config_path = workgraph_dir.join("config.toml");
    let config_val: Option<toml::Value> = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|c| toml::from_str(&c).ok());

    let native_cfg = config_val.as_ref().and_then(|v| v.get("native_executor"));

    // Resolve provider name: override > config > env var > model heuristic
    let provider_name = provider_override
        .map(String::from)
        .or_else(|| {
            native_cfg
                .and_then(|c| c.get("provider"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| std::env::var("WG_LLM_PROVIDER").ok())
        .unwrap_or_else(|| {
            if model.contains('/') {
                "openai".to_string()
            } else {
                "anthropic".to_string()
            }
        });

    // Look up endpoint config for this provider
    let endpoint = config.llm_endpoints.find_for_provider(&provider_name);
    let endpoint_key = endpoint.and_then(|ep| ep.api_key.clone());
    let endpoint_url = endpoint.and_then(|ep| ep.url.clone());

    let api_base: Option<String> = endpoint_url.or_else(|| {
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
            let mut client = if let Some(key) = endpoint_key {
                OpenAiClient::new(key, model, None)
            } else if provider_name == "local" {
                // Local providers (Ollama, vLLM) don't require auth
                OpenAiClient::from_env(model)
                    .or_else(|_| OpenAiClient::new("local".to_string(), model, None))
            } else {
                OpenAiClient::from_env(model).or_else(|_| {
                    let key = super::client::resolve_api_key_from_dir(workgraph_dir)?;
                    OpenAiClient::new(key, model, None)
                })
            }
            .context("Failed to initialize OpenAI-compatible client")?;
            client = client.with_provider_hint(&provider_name);
            if let Some(base) = api_base {
                client = client.with_base_url(&base);
            }
            if let Some(mt) = max_tokens {
                client = client.with_max_tokens(mt);
            }
            eprintln!(
                "[native-exec] Using OpenAI-compatible provider ({})",
                client.model
            );
            Ok(Box::new(client))
        }
        _ => {
            let mut client = if let Some(key) = endpoint_key {
                AnthropicClient::new(key, model)
            } else {
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
