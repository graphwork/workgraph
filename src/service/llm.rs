//! Lightweight LLM dispatch for internal workgraph calls (triage, checkpoint, etc.).
//!
//! Resolves model + provider via `resolve_model_for_role()` and dispatches to either:
//! - Claude CLI (`claude --model X --print --dangerously-skip-permissions PROMPT`)
//! - Native Anthropic API client (when provider is "anthropic" and native executor is configured)
//! - Native OpenAI-compatible API client (when provider is "openai"/"openrouter")

use std::process;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{Config, DispatchRole, ModelRegistryEntry};
use crate::graph::TokenUsage;

/// Result of a lightweight LLM call, including both the text response and token usage.
#[derive(Debug, Clone)]
pub struct LlmCallResult {
    pub text: String,
    pub token_usage: Option<TokenUsage>,
}

/// Maximum output tokens for lightweight LLM calls.
///
/// Triage calls produce short text (~200 tokens) but evaluation and FLIP calls
/// produce structured JSON with multiple dimensions, notes, and reasoning that
/// can easily exceed 1024 tokens. 4096 provides comfortable headroom.
const LIGHTWEIGHT_MAX_TOKENS: u32 = 4096;

/// Run a lightweight (no tool-use) LLM call for an internal dispatch role.
///
/// Resolves the model and provider for the given role, then dispatches via:
/// 1. If `provider` is set to a native provider ("anthropic", "openai", "openrouter"),
///    attempts a direct API call using the native client.
/// 2. Falls back to shelling out to `claude` CLI.
///
/// Returns both the text response and token usage when available.
pub fn run_lightweight_llm_call(
    config: &Config,
    role: DispatchRole,
    prompt: &str,
    timeout_secs: u64,
) -> Result<LlmCallResult> {
    let resolved = config.resolve_model_for_role(role);
    let model = &resolved.model;
    let provider = resolved.provider.as_deref();
    let registry_entry = resolved.registry_entry.as_ref();
    let endpoint_name = resolved.endpoint.as_deref();

    // Try native API call if provider is explicitly configured
    if let Some(prov) = provider {
        match prov {
            "anthropic" => {
                if let Ok(result) = call_anthropic_native(
                    config,
                    prov,
                    model,
                    prompt,
                    timeout_secs,
                    registry_entry,
                    endpoint_name,
                ) {
                    return Ok(result);
                }
            }
            "oai-compat" | "openai" | "openrouter" | "local" => {
                if let Ok(result) = call_openai_native(
                    config,
                    prov,
                    model,
                    prompt,
                    timeout_secs,
                    registry_entry,
                    endpoint_name,
                ) {
                    return Ok(result);
                }
            }
            _ => {}
        }
    }

    call_claude_cli(model, prompt, timeout_secs)
}

/// Estimate cost in USD from token counts and registry pricing data.
fn estimate_cost(entry: &ModelRegistryEntry, usage: &TokenUsage) -> f64 {
    let input_cost = (usage.input_tokens as f64 / 1_000_000.0) * entry.cost_per_input_mtok;
    let output_cost = (usage.output_tokens as f64 / 1_000_000.0) * entry.cost_per_output_mtok;
    let cache_read_cost = if entry.prompt_caching && entry.cache_read_discount > 0.0 {
        (usage.cache_read_input_tokens as f64 / 1_000_000.0)
            * entry.cost_per_input_mtok
            * entry.cache_read_discount
    } else {
        0.0
    };
    let cache_write_cost = if entry.prompt_caching && entry.cache_write_premium > 0.0 {
        (usage.cache_creation_input_tokens as f64 / 1_000_000.0)
            * entry.cost_per_input_mtok
            * entry.cache_write_premium
    } else {
        0.0
    };
    input_cost + output_cost + cache_read_cost + cache_write_cost
}

fn call_claude_cli(model: &str, prompt: &str, timeout_secs: u64) -> Result<LlmCallResult> {
    use std::io::Write as _;

    // Pipe the prompt via stdin instead of passing it as a CLI argument.
    // Eval prompts can be very large (30KB+ with diffs, logs, artifacts) and
    // passing them as arguments can hit OS arg-length limits or cause the
    // `timeout` wrapper to fail with exit 124 before the API call even starts.
    let mut child = process::Command::new("timeout")
        .arg(format!("{}s", timeout_secs))
        .arg("claude")
        .arg("--model")
        .arg(model)
        .arg("--print")
        .arg("--output-format")
        .arg("json")
        .arg("--dangerously-skip-permissions")
        // Strip CLAUDECODE env var so the CLI doesn't refuse to run
        // when invoked from within a Claude Code session (e.g. daemon
        // spawned by a coordinator agent). This is a headless --print
        // call, not an interactive nested session.
        .env_remove("CLAUDECODE")
        .stdin(process::Stdio::piped())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()
        .context("Failed to spawn claude CLI for lightweight LLM call")?;

    // Write prompt to stdin and close the pipe to signal EOF.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .context("Failed to write prompt to claude CLI stdin")?;
    }

    let output = child
        .wait_with_output()
        .context("Failed to wait for claude CLI output")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Claude CLI call failed (exit {:?}): {}",
            output.status.code(),
            stderr.chars().take(500).collect::<String>()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(stdout.trim())
        .context("Failed to parse JSON output from claude CLI")?;
    let text = val
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let token_usage = extract_json_usage(&val);

    if text.is_empty() {
        anyhow::bail!("Empty response from claude CLI");
    }
    Ok(LlmCallResult { text, token_usage })
}

/// Parse stream-json output from Claude CLI to extract text content and token usage.
///
/// Stream-json lines include `type=assistant` (with content) and `type=result` (with usage).
/// Retained for potential future use with --output-format stream-json.
#[cfg(test)]
fn parse_stream_json_output(stdout: &str) -> (String, Option<TokenUsage>) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut token_usage: Option<TokenUsage> = None;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let event_type = match val.get("type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };

        match event_type {
            "assistant" => {
                // Extract text from message.content[] blocks
                if let Some(content) = val
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                text_parts.push(t.to_string());
                            }
                        }
                    }
                }
            }
            "result" => {
                // Extract token usage from the result line
                let cost_usd = val
                    .get("total_cost_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let usage = val.get("usage");

                let input_tokens = usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output_tokens = usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cache_read = usage
                    .and_then(|u| {
                        u.get("cache_read_input_tokens")
                            .or_else(|| u.get("cacheReadInputTokens"))
                    })
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cache_creation = usage
                    .and_then(|u| {
                        u.get("cache_creation_input_tokens")
                            .or_else(|| u.get("cacheCreationInputTokens"))
                    })
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                token_usage = Some(TokenUsage {
                    cost_usd,
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens: cache_read,
                    cache_creation_input_tokens: cache_creation,
                });
            }
            _ => {}
        }
    }

    (text_parts.join("").trim().to_string(), token_usage)
}

/// Extract token usage from a `--output-format json` result object.
fn extract_json_usage(val: &serde_json::Value) -> Option<TokenUsage> {
    let cost_usd = val
        .get("total_cost_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let usage = val.get("usage");

    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = usage
        .and_then(|u| {
            u.get("cache_read_input_tokens")
                .or_else(|| u.get("cacheReadInputTokens"))
        })
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation = usage
        .and_then(|u| {
            u.get("cache_creation_input_tokens")
                .or_else(|| u.get("cacheCreationInputTokens"))
        })
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Some(TokenUsage {
        cost_usd,
        input_tokens,
        output_tokens,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation,
    })
}

fn call_anthropic_native(
    config: &Config,
    provider_name: &str,
    model: &str,
    prompt: &str,
    timeout_secs: u64,
    registry_entry: Option<&ModelRegistryEntry>,
    endpoint_name: Option<&str>,
) -> Result<LlmCallResult> {
    use crate::executor::native::client::{
        AnthropicClient, ContentBlock, Message, MessagesRequest, Role,
    };
    use crate::executor::native::provider::Provider;

    // Look up endpoint: by name first, then by provider
    let endpoint = endpoint_name
        .and_then(|name| config.llm_endpoints.find_by_name(name))
        .or_else(|| config.llm_endpoints.find_for_provider(provider_name));
    let endpoint_key = endpoint.and_then(|ep| ep.resolve_api_key(None).ok().flatten());
    let endpoint_url = endpoint.and_then(|ep| ep.url.clone());

    // Resolve API key. Priority: env var > endpoint config > from_env fallbacks
    let env_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    let mut client = if let Some(key) = env_key {
        AnthropicClient::new(key, model)
    } else if let Some(key) = endpoint_key {
        AnthropicClient::new(key, model)
    } else {
        AnthropicClient::from_env(model)
    }
    .context("Failed to create Anthropic client for lightweight call")?;
    if let Some(url) = endpoint_url {
        client = client.with_base_url(&url);
    }

    let request = MessagesRequest {
        model: model.to_string(),
        max_tokens: LIGHTWEIGHT_MAX_TOKENS,
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
        }],
        tools: vec![],
        stream: false,
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime")?;

    let response = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(timeout_secs), client.send(&request))
            .await
            .context("Native Anthropic call timed out")?
    })?;

    let mut usage = TokenUsage {
        cost_usd: 0.0,
        input_tokens: u64::from(response.usage.input_tokens),
        output_tokens: u64::from(response.usage.output_tokens),
        cache_read_input_tokens: response
            .usage
            .cache_read_input_tokens
            .map(u64::from)
            .unwrap_or(0),
        cache_creation_input_tokens: response
            .usage
            .cache_creation_input_tokens
            .map(u64::from)
            .unwrap_or(0),
    };
    if let Some(entry) = registry_entry {
        usage.cost_usd = estimate_cost(entry, &usage);
    }
    let token_usage = Some(usage);

    let text: String = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    let text = text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("Empty response from native Anthropic call");
    }
    Ok(LlmCallResult { text, token_usage })
}

fn call_openai_native(
    config: &Config,
    provider_name: &str,
    model: &str,
    prompt: &str,
    timeout_secs: u64,
    registry_entry: Option<&ModelRegistryEntry>,
    endpoint_name: Option<&str>,
) -> Result<LlmCallResult> {
    use crate::executor::native::client::{ContentBlock, Message, MessagesRequest, Role};
    use crate::executor::native::openai_client::OpenAiClient;
    use crate::executor::native::provider::Provider;

    // Look up endpoint: by name first, then by provider
    let endpoint = endpoint_name
        .and_then(|name| config.llm_endpoints.find_by_name(name))
        .or_else(|| config.llm_endpoints.find_for_provider(provider_name));
    let endpoint_key = endpoint.and_then(|ep| ep.resolve_api_key(None).ok().flatten());
    let endpoint_url = endpoint.and_then(|ep| ep.url.clone());

    // Resolve API key. Priority: env var > endpoint config > from_env fallbacks
    let env_key = ["OPENROUTER_API_KEY", "OPENAI_API_KEY"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|k| !k.is_empty()));
    let resolved_key = env_key.or(endpoint_key);

    let mut client = if let Some(key) = resolved_key {
        OpenAiClient::new(key, model, None)
            .context("Failed to create OpenAI client for lightweight call")?
    } else if provider_name == "local" {
        // Local providers don't require auth
        OpenAiClient::new("local".to_string(), model, None).expect("infallible with static args")
    } else {
        // Legacy fallback
        OpenAiClient::from_env(model)
            .context("Failed to create OpenAI client for lightweight call")?
    };
    if let Some(url) = endpoint_url {
        client = client.with_base_url(&url);
    }
    client = client.with_provider_hint(provider_name);

    let request = MessagesRequest {
        model: model.to_string(),
        max_tokens: LIGHTWEIGHT_MAX_TOKENS,
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
        }],
        tools: vec![],
        stream: false,
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime")?;

    let response = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(timeout_secs), client.send(&request))
            .await
            .context("Native OpenAI call timed out")?
    })?;

    let mut usage = TokenUsage {
        cost_usd: 0.0,
        input_tokens: u64::from(response.usage.input_tokens),
        output_tokens: u64::from(response.usage.output_tokens),
        cache_read_input_tokens: response
            .usage
            .cache_read_input_tokens
            .map(u64::from)
            .unwrap_or(0),
        cache_creation_input_tokens: response
            .usage
            .cache_creation_input_tokens
            .map(u64::from)
            .unwrap_or(0),
    };
    if let Some(entry) = registry_entry {
        usage.cost_usd = estimate_cost(entry, &usage);
    }
    let token_usage = Some(usage);

    let text: String = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    let text = text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("Empty response from native OpenAI call");
    }
    Ok(LlmCallResult { text, token_usage })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CLAUDE_HAIKU_MODEL_ID, Config, DispatchRole, ModelRegistryEntry, Tier};

    #[test]
    fn test_lightweight_llm_dispatch_resolves_model() {
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, CLAUDE_HAIKU_MODEL_ID);
        assert_eq!(
            resolved.provider,
            Some("anthropic".to_string()),
            "Default triage should resolve via Fast tier registry"
        );
    }

    #[test]
    fn test_lightweight_llm_dispatch_with_provider_override() {
        let mut config = Config::default();
        config.models.set_model(DispatchRole::Triage, "gpt-4o-mini");
        config.models.set_provider(DispatchRole::Triage, "openai");

        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, "gpt-4o-mini");
        assert_eq!(resolved.provider, Some("openai".to_string()));
    }

    #[test]
    fn test_lightweight_llm_parse_stream_json_output() {
        // Simulate Claude CLI stream-json output
        let stdout = format!(
            r#"{{"type":"system","session_id":"abc","model":"{CLAUDE_HAIKU_MODEL_ID}"}}
{{"type":"assistant","message":{{"id":"msg_1","type":"message","role":"assistant","content":[{{"type":"text","text":"The answer is 42."}}],"usage":{{"input_tokens":100,"output_tokens":20}}}}}}
{{"type":"result","total_cost_usd":0.0012,"usage":{{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":50,"cache_creation_input_tokens":10}}}}
"#
        );
        let (text, token_usage) = parse_stream_json_output(&stdout);
        assert_eq!(text, "The answer is 42.");
        let usage = token_usage.expect("should have token usage");
        assert!((usage.cost_usd - 0.0012).abs() < f64::EPSILON);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_input_tokens, 50);
        assert_eq!(usage.cache_creation_input_tokens, 10);
    }

    #[test]
    fn test_lightweight_llm_parse_stream_json_empty() {
        let (text, token_usage) = parse_stream_json_output("");
        assert!(text.is_empty());
        assert!(token_usage.is_none());
    }

    #[test]
    fn test_lightweight_llm_parse_stream_json_no_result() {
        // If the result line is missing, we still get text but no token usage
        let stdout = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":10,"output_tokens":5}}}
"#;
        let (text, token_usage) = parse_stream_json_output(stdout);
        assert_eq!(text, "hello");
        assert!(token_usage.is_none());
    }

    #[test]
    fn test_lightweight_llm_estimate_cost() {
        let entry = ModelRegistryEntry {
            id: "haiku".to_string(),
            provider: "anthropic".to_string(),
            model: CLAUDE_HAIKU_MODEL_ID.to_string(),
            tier: Tier::Fast,
            endpoint: None,
            context_window: 200_000,
            max_output_tokens: 8192,
            cost_per_input_mtok: 0.80,
            cost_per_output_mtok: 4.0,
            prompt_caching: true,
            cache_read_discount: 0.1,
            cache_write_premium: 1.25,
            descriptors: vec![],
        };

        let usage = TokenUsage {
            cost_usd: 0.0,
            input_tokens: 1_000_000, // 1M tokens
            output_tokens: 100_000,  // 100K tokens
            cache_read_input_tokens: 500_000,
            cache_creation_input_tokens: 200_000,
        };

        let cost = estimate_cost(&entry, &usage);
        // input: 1.0 * 0.80 = 0.80
        // output: 0.1 * 4.0 = 0.40
        // cache_read: 0.5 * 0.80 * 0.1 = 0.04
        // cache_write: 0.2 * 0.80 * 1.25 = 0.20
        let expected = 0.80 + 0.40 + 0.04 + 0.20;
        assert!(
            (cost - expected).abs() < 0.001,
            "expected {}, got {}",
            expected,
            cost
        );
    }

    #[test]
    fn test_call_claude_cli_json_parsing() {
        // Simulates the --output-format json output from Claude CLI
        let json_output = r#"{
            "type": "result",
            "result": "The answer is 42.",
            "total_cost_usd": 0.0012,
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 50,
                "cache_creation_input_tokens": 10
            }
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_output).unwrap();
        let text = val
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let token_usage = extract_json_usage(&val);

        assert_eq!(text, "The answer is 42.");
        let usage = token_usage.expect("should have token usage");
        assert!((usage.cost_usd - 0.0012).abs() < f64::EPSILON);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_input_tokens, 50);
        assert_eq!(usage.cache_creation_input_tokens, 10);
    }

    #[test]
    fn test_call_claude_cli_json_no_usage() {
        // JSON result with no usage data
        let json_output = r#"{"type": "result", "result": "hello world"}"#;
        let val: serde_json::Value = serde_json::from_str(json_output).unwrap();
        let text = val
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let token_usage = extract_json_usage(&val);

        assert_eq!(text, "hello world");
        // No usage block → should still return Some with zeroed fields and cost
        let usage = token_usage.expect("should have token usage with defaults");
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn test_lightweight_llm_estimate_cost_no_caching() {
        let entry = ModelRegistryEntry {
            id: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            tier: Tier::Standard,
            endpoint: None,
            context_window: 128_000,
            max_output_tokens: 4096,
            cost_per_input_mtok: 2.50,
            cost_per_output_mtok: 10.0,
            prompt_caching: false,
            cache_read_discount: 0.0,
            cache_write_premium: 0.0,
            descriptors: vec![],
        };

        let usage = TokenUsage {
            cost_usd: 0.0,
            input_tokens: 500,
            output_tokens: 200,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };

        let cost = estimate_cost(&entry, &usage);
        // input: 0.0005 * 2.50 = 0.00125
        // output: 0.0002 * 10.0 = 0.002
        let expected = 0.00125 + 0.002;
        assert!(
            (cost - expected).abs() < 0.0001,
            "expected {}, got {}",
            expected,
            cost
        );
    }
}
