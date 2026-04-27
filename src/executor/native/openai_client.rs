//! OpenAI-compatible HTTP client for chat completions.
//!
//! Supports OpenRouter, direct OpenAI, and any API that implements the
//! OpenAI chat completions format (Ollama, vLLM, Together, etc.).
//!
//! Translates between the canonical Anthropic-style types used by the
//! agent loop and the OpenAI wire format.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use log;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use super::client::{
    ContentBlock, Message, MessagesRequest, MessagesResponse, Role, StopReason, ToolDefinition,
    Usage,
};
use crate::config::ModelRegistryEntry;

// ── OpenAI wire format types ────────────────────────────────────────────

/// OpenAI-format tool definition.
#[derive(Debug, Clone, Serialize)]
struct OaiToolDef {
    #[serde(rename = "type")]
    tool_type: String,
    function: OaiFunctionDef,
}

#[derive(Debug, Clone, Serialize)]
struct OaiFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// OpenAI-format message for the request.
#[derive(Debug, Clone, Serialize)]
struct OaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// Pass reasoning_details back verbatim for models that require it
    /// (e.g., DeepSeek R1 returns 400 without reasoning context between tool calls).
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_details: Option<Vec<serde_json::Value>>,
}

/// OpenAI-format request body.
#[derive(Debug, Clone, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OaiToolDef>,
    /// Controls how the model selects tool calls. Must be `"auto"` when tools are
    /// present — many OpenRouter-proxied models silently ignore tools without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    stream: bool,
    /// When streaming, request that the API include usage data in the final chunk.
    /// OpenAI requires `{"include_usage": true}` to report token counts in streaming mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OaiStreamOptions>,
    /// OpenRouter cache_control — triggers auto-caching for Anthropic/Gemini models.
    /// When set, OpenRouter applies cache_control to the last cacheable content block.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<serde_json::Value>,
    /// OpenRouter reasoning parameter — enables reasoning/thinking token capture.
    /// When set, models that support reasoning will return thinking tokens in the
    /// `reasoning` and `reasoning_details` response fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<serde_json::Value>,
    /// Legacy OpenRouter reasoning toggle — deprecated in favor of `reasoning`.
    #[serde(skip_serializing_if = "Option::is_none")]
    include_reasoning: Option<bool>,
}

/// Options for streaming mode.
#[derive(Debug, Clone, Serialize)]
struct OaiStreamOptions {
    include_usage: bool,
}

/// OpenAI-format response body.
#[derive(Debug, Clone, Deserialize)]
struct OaiResponse {
    #[allow(dead_code)]
    id: String,
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Debug, Clone, Deserialize)]
struct OaiChoice {
    message: OaiResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OaiResponseMessage {
    #[allow(dead_code)]
    role: String,
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiToolCall>>,
    /// Plaintext reasoning/thinking content (OpenRouter unified field).
    #[serde(default)]
    reasoning: Option<String>,
    /// Structured reasoning details (OpenRouter unified field).
    /// Passed back verbatim in subsequent requests to preserve reasoning context.
    #[serde(default)]
    reasoning_details: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OaiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OaiToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OaiToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OaiPromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_write_tokens: Option<u32>,
    /// Cost reduction from caching (parsed but not yet used — no price table).
    #[serde(default)]
    #[allow(dead_code)]
    cache_discount: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct OaiOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct OaiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<OaiPromptTokenDetails>,
    /// Output token breakdown — includes reasoning token count.
    #[serde(default)]
    completion_tokens_details: Option<OaiOutputTokenDetails>,
}

/// OpenAI-format error response.
#[derive(Debug, Clone, Deserialize)]
struct OaiErrorResponse {
    error: OaiErrorDetail,
}

#[derive(Debug, Clone, Deserialize)]
struct OaiErrorDetail {
    message: String,
    #[serde(default)]
    #[allow(dead_code)]
    code: Option<serde_json::Value>,
}

// ── OpenAI streaming chunk types ─────────────────────────────────────────

/// A single SSE chunk from an OpenAI-compatible streaming response.
#[derive(Debug, Clone, Deserialize)]
struct OaiStreamChunk {
    #[serde(default)]
    id: String,
    choices: Vec<OaiStreamChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Debug, Clone, Deserialize)]
struct OaiStreamChoice {
    #[serde(default)]
    delta: OaiStreamDelta,
    finish_reason: Option<String>,
}

/// Delta content within a streaming chunk.
#[derive(Debug, Clone, Default, Deserialize)]
struct OaiStreamDelta {
    #[serde(default)]
    #[allow(dead_code)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiStreamToolCall>>,
    /// Reasoning/thinking content delta (OpenRouter streaming).
    #[serde(default)]
    reasoning: Option<String>,
    /// Structured reasoning details delta (OpenRouter streaming).
    #[serde(default)]
    reasoning_details: Option<Vec<serde_json::Value>>,
}

/// Tool call delta in a streaming chunk.
///
/// Fields are optional because only the first chunk for a tool call
/// includes `id` and `type`; subsequent chunks only carry `function.arguments`.
#[derive(Debug, Clone, Deserialize)]
struct OaiStreamToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<OaiStreamToolCallFunction>,
}

#[derive(Debug, Clone, Deserialize)]
struct OaiStreamToolCallFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

// ── Client ──────────────────────────────────────────────────────────────

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_MAX_TOKENS: u32 = 16384;

/// OpenAI-compatible chat completions client.
///
/// Works with OpenRouter, direct OpenAI API, and any compatible endpoint.
#[derive(Debug)]
pub struct OpenAiClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    pub model: String,
    pub max_tokens: u32,
    /// Provider hint for provider-specific behavior (e.g. "openrouter", "openai", "local").
    provider_hint: Option<String>,
    /// Whether to use SSE streaming for requests.
    use_streaming: bool,
    context_window_tokens: usize,
    /// Optional registry entry for cost estimation.
    registry_entry: Option<ModelRegistryEntry>,
}

impl OpenAiClient {
    /// Create a client with explicit configuration.
    pub fn new(api_key: String, model: &str, base_url: Option<&str>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            http,
            api_key,
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            model: model.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            provider_hint: None,
            use_streaming: false,
            context_window_tokens: 128_000,
            registry_entry: None,
        })
    }

    /// Create from environment variables.
    ///
    /// Checks `OPENROUTER_API_KEY`, `OPENAI_API_KEY` in that order.
    /// Uses `OPENAI_BASE_URL` or `OPENROUTER_BASE_URL` for the endpoint.
    pub fn from_env(model: &str) -> Result<Self> {
        let api_key = resolve_openai_api_key()?;
        let base_url = std::env::var("OPENAI_BASE_URL")
            .or_else(|_| std::env::var("OPENROUTER_BASE_URL"))
            .ok();
        Self::new(api_key, model, base_url.as_deref())
    }

    /// Create from an [`EndpointConfig`](crate::config::EndpointConfig) with full key resolution.
    ///
    /// Uses `EndpointConfig::resolve_api_key()` which checks inline key, key file,
    /// then environment variable fallback based on provider.
    pub fn from_endpoint(
        endpoint: &crate::config::EndpointConfig,
        model: &str,
        workgraph_dir: Option<&std::path::Path>,
    ) -> Result<Self> {
        let api_key = endpoint.resolve_api_key(workgraph_dir)?.ok_or_else(|| {
            let env_vars =
                crate::config::EndpointConfig::env_var_names_for_provider(&endpoint.provider);
            let env_hint = if env_vars.is_empty() {
                String::new()
            } else {
                format!(" Set {} environment variable,", env_vars[0])
            };
            anyhow!(
                "No API key found for endpoint '{}'.{} or configure api_key / api_key_file.",
                endpoint.name,
                env_hint,
            )
        })?;
        let base_url = endpoint.url.as_deref().unwrap_or_else(|| {
            crate::config::EndpointConfig::default_url_for_provider(&endpoint.provider)
        });
        let base_url = if base_url.is_empty() {
            None
        } else {
            Some(base_url)
        };
        let mut client = Self::new(api_key, model, base_url)?;
        client = client.with_provider_hint(&endpoint.provider);
        Ok(client)
    }

    /// Override the base URL.
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.trim_end_matches('/').to_string();
        self
    }

    /// Resolved base URL (post-`with_base_url` and defaults). Handy
    /// for logging + tests of the URL resolution path.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Override max tokens per response.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Override context window size in tokens.
    ///
    /// When the window is small enough that `max_tokens` would consume more than
    /// half the window (leaving too little room for input), `max_tokens` is
    /// automatically capped to `context_window / 4`. This prevents servers like
    /// SGLang from rejecting requests with HTTP 400 because
    /// `input_tokens + max_tokens > context_window`.
    pub fn with_context_window(mut self, tokens: usize) -> Self {
        self.context_window_tokens = tokens;
        // Cap max_tokens so it doesn't crowd out input.  SGLang (and other
        // OpenAI-compatible servers) enforce input + max_tokens ≤ context_window
        // *before* inference.  With the default 16 384 max_tokens and a 32 k
        // window, any input above ~16 k triggers a 400.  Capping at window/4
        // leaves 75 % of the window for input while still allowing generous
        // output (8 192 tokens for a 32 k window, 32 768 for 128 k).
        let cap = (tokens / 4) as u32;
        if cap > 0 && self.max_tokens > cap {
            log::debug!(
                "[openai-client] Capping max_tokens from {} to {} (context_window={})",
                self.max_tokens,
                cap,
                tokens
            );
            self.max_tokens = cap;
        }
        self
    }

    /// Set a provider hint for provider-specific behavior.
    ///
    /// When set to `"openrouter"`, adds `HTTP-Referer` and `X-Title` attribution
    /// headers to requests, and enables SSE streaming by default.
    pub fn with_provider_hint(mut self, hint: &str) -> Self {
        self.provider_hint = Some(hint.to_string());
        // Enable streaming by default for OpenRouter (reliable SSE support)
        if hint == "openrouter" {
            self.use_streaming = true;
        }
        self
    }

    /// Enable or disable SSE streaming for requests.
    ///
    /// When enabled, the client sends `stream: true` and parses the SSE
    /// response, correctly accumulating partial tool call arguments across
    /// chunks. Streaming is auto-enabled for OpenRouter via
    /// `with_provider_hint("openrouter")`. Call this to override.
    pub fn with_streaming(mut self, enabled: bool) -> Self {
        self.use_streaming = enabled;
        self
    }

    /// Set a registry entry for cost estimation.
    ///
    /// When set, the client will calculate estimated cost per request
    /// based on the model's pricing data from the registry.
    pub fn with_registry_entry(mut self, entry: ModelRegistryEntry) -> Self {
        self.registry_entry = Some(entry);
        self
    }

    /// Convert canonical tool definitions to OpenAI format.
    fn translate_tools(tools: &[ToolDefinition]) -> Vec<OaiToolDef> {
        tools
            .iter()
            .map(|t| OaiToolDef {
                tool_type: "function".to_string(),
                function: OaiFunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect()
    }

    /// Convert canonical messages to OpenAI format.
    fn translate_messages(system: &Option<String>, messages: &[Message]) -> Vec<OaiMessage> {
        let mut oai_messages = Vec::new();

        // System message first
        if let Some(sys) = system {
            oai_messages.push(OaiMessage {
                role: "system".to_string(),
                content: Some(sys.clone()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_details: None,
            });
        }

        for msg in messages {
            match msg.role {
                Role::User => {
                    // User messages may contain text or tool results
                    let has_tool_results = msg
                        .content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));

                    if has_tool_results {
                        // Each tool result becomes a separate message with role "tool"
                        for block in &msg.content {
                            match block {
                                ContentBlock::ToolResult {
                                    tool_use_id,
                                    content,
                                    ..
                                } => {
                                    oai_messages.push(OaiMessage {
                                        role: "tool".to_string(),
                                        content: Some(content.clone()),
                                        tool_calls: None,
                                        tool_call_id: Some(tool_use_id.clone()),
                                        reasoning_details: None,
                                    });
                                }
                                ContentBlock::Text { text } => {
                                    // Merge into the previous user message if
                                    // one exists to avoid consecutive same-role
                                    // messages (which OAI servers reject).
                                    if let Some(prev) = oai_messages.last_mut()
                                        && prev.role == "user"
                                        && prev.tool_call_id.is_none()
                                    {
                                        let existing =
                                            prev.content.get_or_insert_with(String::new);
                                        existing.push('\n');
                                        existing.push_str(text);
                                    } else {
                                        oai_messages.push(OaiMessage {
                                            role: "user".to_string(),
                                            content: Some(text.clone()),
                                            tool_calls: None,
                                            tool_call_id: None,
                                            reasoning_details: None,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else {
                        // Regular text message — merge into previous user
                        // message when one already exists at the tail to
                        // avoid consecutive same-role violations. This
                        // can happen when the agent loop appends context
                        // warnings or inbox-drain messages adjacent to a
                        // user turn.
                        let text: String = msg
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if let Some(prev) = oai_messages.last_mut()
                            && prev.role == "user"
                            && prev.tool_call_id.is_none()
                        {
                            let existing = prev.content.get_or_insert_with(String::new);
                            existing.push('\n');
                            existing.push_str(&text);
                        } else {
                            oai_messages.push(OaiMessage {
                                role: "user".to_string(),
                                content: Some(text),
                                tool_calls: None,
                                tool_call_id: None,
                                reasoning_details: None,
                            });
                        }
                    }
                }
                Role::Assistant => {
                    // Collect text, tool_calls, and reasoning from content blocks
                    let mut text_parts = Vec::new();
                    let mut tool_calls = Vec::new();
                    let mut reasoning_details: Option<Vec<serde_json::Value>> = None;

                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => {
                                text_parts.push(text.clone());
                            }
                            ContentBlock::Thinking {
                                reasoning_details: Some(rd),
                                ..
                            } => {
                                // Pass back reasoning_details verbatim for models that need it
                                reasoning_details = Some(rd.clone());
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                tool_calls.push(OaiToolCall {
                                    id: id.clone(),
                                    call_type: "function".to_string(),
                                    function: OaiToolCallFunction {
                                        name: name.clone(),
                                        arguments: serde_json::to_string(input).unwrap_or_default(),
                                    },
                                });
                            }
                            _ => {}
                        }
                    }

                    let content = if text_parts.is_empty() {
                        None
                    } else {
                        Some(text_parts.join("\n"))
                    };

                    let tc = if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    };

                    oai_messages.push(OaiMessage {
                        role: "assistant".to_string(),
                        content,
                        tool_calls: tc,
                        tool_call_id: None,
                        reasoning_details,
                    });
                }
            }
        }

        oai_messages
    }

    /// Convert an OpenAI response to canonical format.
    fn translate_response(oai: OaiResponse) -> Result<MessagesResponse> {
        let choice = oai
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Empty choices in API response"))?;

        let mut content_blocks = Vec::new();
        let has_structured_tool_calls = choice
            .message
            .tool_calls
            .as_ref()
            .is_some_and(|tc| !tc.is_empty());

        // Add thinking/reasoning content FIRST (before text/tool blocks)
        if let Some(ref reasoning) = choice.message.reasoning {
            if !reasoning.is_empty() {
                content_blocks.push(ContentBlock::Thinking {
                    thinking: reasoning.clone(),
                    reasoning_details: choice.message.reasoning_details.clone(),
                });
            }
        } else if let Some(ref rd) = choice.message.reasoning_details {
            // Some models only return reasoning_details without the plaintext reasoning field
            if !rd.is_empty() {
                let thinking_text = rd
                    .iter()
                    .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !thinking_text.is_empty() {
                    content_blocks.push(ContentBlock::Thinking {
                        thinking: thinking_text,
                        reasoning_details: Some(rd.clone()),
                    });
                }
            }
        }

        // Add text content if present
        if let Some(text) = choice.message.content
            && !text.is_empty()
        {
            // Check for inline <think>...</think> tags (MiniMax, DeepSeek, Qwen)
            let (clean_text, inline_thinking) = extract_inline_thinking(&text);

            let has_inline_thinking = inline_thinking.is_some();
            if let Some(thinking) = inline_thinking {
                // Only add if we don't already have a Thinking block from the API fields
                if !content_blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Thinking { .. }))
                {
                    content_blocks.push(ContentBlock::Thinking {
                        thinking,
                        reasoning_details: None,
                    });
                }
            }

            let text_to_process = if has_inline_thinking {
                clean_text
            } else {
                text
            };

            if !text_to_process.is_empty() {
                // If there are no structured tool calls, check for text-based tool calls
                if !has_structured_tool_calls {
                    let (remaining, extracted) = extract_tool_calls_from_text(&text_to_process);
                    if !extracted.is_empty() {
                        eprintln!(
                            "[openai-client] Extracted {} tool call(s) from text output (model used text-based format)",
                            extracted.len()
                        );
                        if !remaining.is_empty() {
                            content_blocks.push(ContentBlock::Text { text: remaining });
                        }
                        content_blocks.extend(extracted);
                    } else {
                        content_blocks.push(ContentBlock::Text {
                            text: text_to_process,
                        });
                    }
                } else {
                    content_blocks.push(ContentBlock::Text {
                        text: text_to_process,
                    });
                }
            }
        }

        // Add tool calls if present
        if let Some(tool_calls) = choice.message.tool_calls {
            for tc in tool_calls {
                let input: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
                    Ok(v) => v,
                    Err(e) => make_parse_error_input(&tc.function.arguments, &e.to_string()),
                };
                content_blocks.push(ContentBlock::ToolUse {
                    id: tc.id,
                    name: tc.function.name,
                    input,
                });
            }
        }

        // If no content at all, add empty text
        if content_blocks.is_empty() {
            content_blocks.push(ContentBlock::Text {
                text: String::new(),
            });
        }

        // Determine stop reason — override to ToolUse if we extracted tool calls
        let has_tool_use = content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
        let stop_reason =
            if has_tool_use && !matches!(choice.finish_reason.as_deref(), Some("tool_calls")) {
                // Model produced tool calls (possibly text-extracted) but finish_reason
                // wasn't "tool_calls" — override so the agent loop processes them.
                Some(StopReason::ToolUse)
            } else {
                match choice.finish_reason.as_deref() {
                    Some("stop") => Some(StopReason::EndTurn),
                    Some("tool_calls") => Some(StopReason::ToolUse),
                    Some("length") => Some(StopReason::MaxTokens),
                    Some("content_filter") => Some(StopReason::StopSequence),
                    _ => None,
                }
            };

        let usage = oai
            .usage
            .map(|u| {
                let (cache_read, cache_creation) = u
                    .prompt_tokens_details
                    .map(|d| (d.cached_tokens, d.cache_write_tokens))
                    .unwrap_or((None, None));
                let reasoning_tokens = u.completion_tokens_details.and_then(|d| d.reasoning_tokens);
                Usage {
                    input_tokens: u.prompt_tokens,
                    output_tokens: u.completion_tokens,
                    cache_creation_input_tokens: cache_creation,
                    cache_read_input_tokens: cache_read,
                    reasoning_tokens,
                }
            })
            .unwrap_or_default();

        Ok(MessagesResponse {
            id: oai.id,
            content: content_blocks,
            stop_reason,
            usage,
        })
    }

    /// Returns the reasoning request value for OpenRouter.
    ///
    /// When using OpenRouter, sends `{}` (empty object = enable with defaults)
    /// to capture reasoning/thinking tokens from models that support them.
    fn reasoning_value(&self) -> Option<serde_json::Value> {
        if self.provider_hint.as_deref() == Some("openrouter") {
            Some(serde_json::json!({}))
        } else {
            None
        }
    }

    /// Send a non-streaming request.
    async fn chat_completion(&self, request: &MessagesRequest) -> Result<MessagesResponse> {
        let tools = Self::translate_tools(&request.tools);
        let tool_choice = if tools.is_empty() {
            None
        } else {
            Some("auto".to_string())
        };
        let reasoning = self.reasoning_value();
        let include_reasoning =
            if reasoning.is_none() && self.provider_hint.as_deref() == Some("openrouter") {
                Some(true)
            } else {
                None
            };
        let oai_request = OaiRequest {
            model: request.model.clone(),
            messages: Self::translate_messages(&request.system, &request.messages),
            max_tokens: Some(request.max_tokens),
            tools,
            tool_choice,
            stream: false,
            stream_options: None,
            cache_control: self.cache_control_value(),
            reasoning,
            include_reasoning,
        };

        let url = format!("{}/chat/completions", self.base_url);
        self.send_with_retry(&url, &oai_request).await
    }

    /// Send a streaming request and assemble the full response.
    ///
    /// Uses SSE (Server-Sent Events) to receive incremental chunks,
    /// accumulating text content and tool call arguments across chunks.
    /// Returns a complete `MessagesResponse` once the stream ends.
    ///
    /// Includes retry logic for transient failures (connection drops, 5xx errors).
    /// Malformed SSE chunks are skipped with a warning rather than causing a crash.
    async fn chat_completion_streaming(
        &self,
        request: &MessagesRequest,
    ) -> Result<MessagesResponse> {
        let tools = Self::translate_tools(&request.tools);
        let tool_choice = if tools.is_empty() {
            None
        } else {
            Some("auto".to_string())
        };
        let reasoning = self.reasoning_value();
        let include_reasoning =
            if reasoning.is_none() && self.provider_hint.as_deref() == Some("openrouter") {
                Some(true)
            } else {
                None
            };
        let oai_request = OaiRequest {
            model: request.model.clone(),
            messages: Self::translate_messages(&request.system, &request.messages),
            max_tokens: Some(request.max_tokens),
            tools,
            tool_choice,
            stream: true,
            stream_options: if self.supports_stream_options() {
                Some(OaiStreamOptions { include_usage: true })
            } else {
                None
            },
            cache_control: self.cache_control_value(),
            reasoning,
            include_reasoning,
        };

        let url = format!("{}/chat/completions", self.base_url);
        let max_retries = 3;
        let mut retry_count = 0;
        let mut backoff_ms = 1000u64;

        loop {
            match self.streaming_attempt(&url, &oai_request).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // Don't retry deterministic client errors (400, 401, 403, 404, etc.)
                    // — only retry transient/network errors.
                    let is_client_error = e.downcast_ref::<ApiError>().is_some_and(|ae| {
                        ae.status >= 400 && ae.status < 500 && !is_retryable(ae.status)
                    });
                    if !is_client_error && retry_count < max_retries {
                        retry_count += 1;
                        let wait = jittered_backoff(backoff_ms);
                        eprintln!(
                            "[openai-client] Streaming error (attempt {}/{}): {}. Retrying in {}ms",
                            retry_count, max_retries, e, wait
                        );
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                    return Err(e).context("Streaming request failed after retries");
                }
            }
        }
    }

    /// Execute a single streaming attempt: send request, parse SSE, accumulate response.
    async fn streaming_attempt(
        &self,
        url: &str,
        oai_request: &OaiRequest,
    ) -> Result<MessagesResponse> {
        let headers = self.build_headers();
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .json(oai_request)
            .send()
            .await
            .context("Failed to send streaming request")?;

        let status = resp.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            // Surface retryable errors so the caller can retry
            if is_retryable(status_code) {
                let wait_hint = parse_retry_after_oai(&body).unwrap_or(0);
                if wait_hint > 0 {
                    tokio::time::sleep(Duration::from_millis(wait_hint)).await;
                }
            }
            return Err(oai_api_error(status_code, &body));
        }

        // Parse SSE stream and accumulate into a response
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        let mut response_id = String::new();
        let mut text_content = String::new();
        let mut reasoning_content = String::new();
        let mut reasoning_details: Vec<serde_json::Value> = Vec::new();
        // Accumulated tool calls: index → (id, name, arguments)
        let mut tool_calls: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<OaiUsage> = None;
        let mut chunk_count: u32 = 0;

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    // Connection dropped mid-stream
                    if chunk_count == 0 {
                        return Err(anyhow!(
                            "Stream connection failed before receiving data: {}",
                            e
                        ));
                    }
                    eprintln!(
                        "[openai-client] Stream interrupted after {} chunks: {}",
                        chunk_count, e
                    );
                    // If we have a finish_reason, the response is likely complete
                    if finish_reason.is_some() {
                        break;
                    }
                    return Err(anyhow!(
                        "Stream interrupted after {} chunks: {}",
                        chunk_count,
                        e
                    ));
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE data lines from the buffer
            while let Some(data) = parse_next_oai_sse_data(&mut buffer) {
                if data == "[DONE]" {
                    break;
                }

                let parsed_chunk: OaiStreamChunk = match serde_json::from_str(&data) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "[openai-client] Skipping malformed SSE chunk: {} (data: {})",
                            e,
                            truncate(&data, 200)
                        );
                        continue;
                    }
                };

                chunk_count += 1;

                if response_id.is_empty() && !parsed_chunk.id.is_empty() {
                    response_id = parsed_chunk.id;
                }

                // Capture usage from the final chunk (if present)
                if let Some(u) = parsed_chunk.usage {
                    usage = Some(u);
                }

                for choice in &parsed_chunk.choices {
                    // Accumulate text content
                    if let Some(ref text) = choice.delta.content {
                        text_content.push_str(text);
                    }

                    // Accumulate reasoning content
                    if let Some(ref reasoning) = choice.delta.reasoning {
                        reasoning_content.push_str(reasoning);
                    }
                    if let Some(ref rd) = choice.delta.reasoning_details {
                        reasoning_details.extend(rd.iter().cloned());
                    }

                    // Accumulate tool calls
                    if let Some(ref tcs) = choice.delta.tool_calls {
                        for tc in tcs {
                            let entry = tool_calls
                                .entry(tc.index)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(ref id) = tc.id {
                                entry.0 = id.clone();
                            }
                            if let Some(ref func) = tc.function {
                                if let Some(ref name) = func.name {
                                    entry.1 = name.clone();
                                }
                                if let Some(ref args) = func.arguments {
                                    entry.2.push_str(args);
                                }
                            }
                        }
                    }

                    // Capture finish_reason
                    if let Some(ref fr) = choice.finish_reason {
                        finish_reason = Some(fr.clone());
                    }
                }
            }
        }

        // Log streaming completion summary
        let reasoning_info = if !reasoning_content.is_empty() {
            format!(", {} reasoning chars", reasoning_content.len())
        } else {
            String::new()
        };

        // Use standard logging instead of eprintln to avoid console clutter
        log::info!(
            "[openai-client] Stream complete: {} chunks, {} text chars, {} tool calls{}",
            chunk_count,
            text_content.len(),
            tool_calls.len(),
            reasoning_info,
        );

        // Assemble the response
        assemble_oai_stream_response(
            response_id,
            text_content,
            reasoning_content,
            reasoning_details,
            tool_calls,
            finish_reason,
            usage,
        )
    }

    /// Execute a single streaming attempt with a text callback for progressive display.
    async fn streaming_attempt_with_callback(
        &self,
        url: &str,
        oai_request: &OaiRequest,
        on_text: &(dyn Fn(String) + Send + Sync),
    ) -> Result<MessagesResponse> {
        let headers = self.build_headers();
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .json(oai_request)
            .send()
            .await
            .context("Failed to send streaming request")?;

        let status = resp.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            if is_retryable(status_code) {
                let wait_hint = parse_retry_after_oai(&body).unwrap_or(0);
                if wait_hint > 0 {
                    tokio::time::sleep(Duration::from_millis(wait_hint)).await;
                }
            }
            return Err(oai_api_error(status_code, &body));
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut response_id = String::new();
        let mut text_content = String::new();
        let mut reasoning_content = String::new();
        let mut reasoning_details: Vec<serde_json::Value> = Vec::new();
        let mut tool_calls: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<OaiUsage> = None;
        let mut chunk_count: u32 = 0;

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    if chunk_count == 0 {
                        return Err(anyhow!(
                            "Stream connection failed before receiving data: {}",
                            e
                        ));
                    }
                    eprintln!(
                        "[openai-client] Stream interrupted after {} chunks: {}",
                        chunk_count, e
                    );
                    if finish_reason.is_some() {
                        break;
                    }
                    return Err(anyhow!(
                        "Stream interrupted after {} chunks: {}",
                        chunk_count,
                        e
                    ));
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(data) = parse_next_oai_sse_data(&mut buffer) {
                if data == "[DONE]" {
                    break;
                }

                let parsed_chunk: OaiStreamChunk = match serde_json::from_str(&data) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "[openai-client] Skipping malformed SSE chunk: {} (data: {})",
                            e,
                            truncate(&data, 200)
                        );
                        continue;
                    }
                };

                chunk_count += 1;

                if response_id.is_empty() && !parsed_chunk.id.is_empty() {
                    response_id = parsed_chunk.id;
                }

                if let Some(u) = parsed_chunk.usage {
                    usage = Some(u);
                }

                for choice in &parsed_chunk.choices {
                    if let Some(ref text) = choice.delta.content {
                        text_content.push_str(text);
                        on_text(text.clone());
                    }

                    // Accumulate reasoning content
                    if let Some(ref reasoning) = choice.delta.reasoning {
                        reasoning_content.push_str(reasoning);
                    }
                    if let Some(ref rd) = choice.delta.reasoning_details {
                        reasoning_details.extend(rd.iter().cloned());
                    }

                    if let Some(ref tcs) = choice.delta.tool_calls {
                        for tc in tcs {
                            let entry = tool_calls
                                .entry(tc.index)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(ref id) = tc.id {
                                entry.0 = id.clone();
                            }
                            if let Some(ref func) = tc.function {
                                if let Some(ref name) = func.name {
                                    entry.1 = name.clone();
                                }
                                if let Some(ref args) = func.arguments {
                                    entry.2.push_str(args);
                                }
                            }
                        }
                    }

                    if let Some(ref fr) = choice.finish_reason {
                        finish_reason = Some(fr.clone());
                    }
                }
            }
        }

        let reasoning_info = if !reasoning_content.is_empty() {
            format!(", {} reasoning chars", reasoning_content.len())
        } else {
            String::new()
        };

        // Use standard logging instead of eprintln to avoid console clutter
        log::info!(
            "[openai-client] Stream complete: {} chunks, {} text chars, {} tool calls{}",
            chunk_count,
            text_content.len(),
            tool_calls.len(),
            reasoning_info,
        );

        assemble_oai_stream_response(
            response_id,
            text_content,
            reasoning_content,
            reasoning_details,
            tool_calls,
            finish_reason,
            usage,
        )
    }

    /// Streaming completion with text callback and retry logic.
    async fn chat_completion_streaming_with_callback(
        &self,
        request: &MessagesRequest,
        on_text: &(dyn Fn(String) + Send + Sync),
    ) -> Result<MessagesResponse> {
        let tools = Self::translate_tools(&request.tools);
        let tool_choice = if tools.is_empty() {
            None
        } else {
            Some("auto".to_string())
        };
        let reasoning = self.reasoning_value();
        let include_reasoning =
            if reasoning.is_none() && self.provider_hint.as_deref() == Some("openrouter") {
                Some(true)
            } else {
                None
            };
        let oai_request = OaiRequest {
            model: request.model.clone(),
            messages: Self::translate_messages(&request.system, &request.messages),
            max_tokens: Some(request.max_tokens),
            tools,
            tool_choice,
            stream: true,
            stream_options: if self.supports_stream_options() {
                Some(OaiStreamOptions { include_usage: true })
            } else {
                None
            },
            cache_control: self.cache_control_value(),
            reasoning,
            include_reasoning,
        };

        let url = format!("{}/chat/completions", self.base_url);
        let max_retries = 3;
        let mut retry_count = 0;
        let mut backoff_ms = 1000u64;

        loop {
            match self
                .streaming_attempt_with_callback(&url, &oai_request, on_text)
                .await
            {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // Don't retry deterministic client errors (400, 401, 403, 404, etc.)
                    // — only retry transient/network errors.
                    let is_client_error = e.downcast_ref::<ApiError>().is_some_and(|ae| {
                        ae.status >= 400 && ae.status < 500 && !is_retryable(ae.status)
                    });
                    if !is_client_error && retry_count < max_retries {
                        retry_count += 1;
                        let wait = jittered_backoff(backoff_ms);
                        eprintln!(
                            "[openai-client] Streaming error (attempt {}/{}): {}. Retrying in {}ms",
                            retry_count, max_retries, e, wait
                        );
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                    return Err(e).context("Streaming request failed after retries");
                }
            }
        }
    }

    /// Send a request with retry logic.
    async fn send_with_retry(&self, url: &str, request: &OaiRequest) -> Result<MessagesResponse> {
        let network_max_retries: usize = 5;
        let mut retry_count: usize = 0;
        let mut backoff_ms = 1000u64;

        loop {
            let headers = self.build_headers();
            let resp = self
                .http
                .post(url)
                .headers(headers)
                .json(request)
                .send()
                .await;

            match resp {
                Ok(response) => {
                    let status = response.status();

                    if status.is_success() {
                        let body = response
                            .text()
                            .await
                            .context("Failed to read response body")?;
                        let oai_resp: OaiResponse =
                            serde_json::from_str(&body).with_context(|| {
                                format!("Failed to parse API response: {}", truncate(&body, 500))
                            })?;
                        return Self::translate_response(oai_resp);
                    }

                    let status_code = status.as_u16();
                    let body = response.text().await.unwrap_or_default();

                    let allowed_retries = max_retries_for_status(status_code);
                    if is_retryable(status_code) && retry_count < allowed_retries {
                        retry_count += 1;
                        let wait = parse_retry_after_oai(&body)
                            .map(jittered_backoff)
                            .unwrap_or_else(|| jittered_backoff(backoff_ms));
                        eprintln!(
                            "[openai-client] Retryable error {} (attempt {}/{}), waiting {}ms",
                            status_code, retry_count, allowed_retries, wait
                        );
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        backoff_ms = (backoff_ms * 2).min(60_000);
                        continue;
                    }

                    return Err(oai_api_error(status_code, &body));
                }
                Err(e) => {
                    if retry_count < network_max_retries {
                        retry_count += 1;
                        let wait = jittered_backoff(backoff_ms);
                        eprintln!(
                            "[openai-client] Network error (attempt {}/{}): {}. Retrying in {}ms",
                            retry_count, network_max_retries, e, wait
                        );
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        backoff_ms = (backoff_ms * 2).min(60_000);
                        continue;
                    }
                    return Err(e).context("Network error after retries");
                }
            }
        }
    }

    /// Returns the cache_control value for OpenRouter requests.
    ///
    /// When using OpenRouter, sends `{"type": "ephemeral"}` to enable auto-caching.
    /// OpenRouter applies this to the last cacheable content block, enabling prompt
    /// caching for Anthropic and Gemini models with zero per-message configuration.
    /// For providers with automatic caching (OpenAI, DeepSeek), this field is ignored.
    fn cache_control_value(&self) -> Option<serde_json::Value> {
        if self.provider_hint.as_deref() == Some("openrouter") {
            Some(serde_json::json!({"type": "ephemeral"}))
        } else {
            None
        }
    }

    /// Whether to include `stream_options: {include_usage: true}` in
    /// streaming requests. Known providers (OpenRouter, OpenAI) support
    /// this; local servers (vLLM, llama.cpp, SGLang) may not. Omitting
    /// it is safe — usage just defaults to zeros.
    fn supports_stream_options(&self) -> bool {
        matches!(
            self.provider_hint.as_deref(),
            Some("openrouter") | Some("openai")
        )
    }

    fn build_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", self.api_key))
                .expect("invalid api key header"),
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        // OpenRouter attribution headers
        if self.provider_hint.as_deref() == Some("openrouter") {
            headers.insert(
                "http-referer",
                HeaderValue::from_static("https://github.com/anthropics/workgraph"),
            );
            headers.insert("x-title", HeaderValue::from_static("workgraph"));
        }

        headers
    }
}

#[async_trait::async_trait]
impl super::provider::Provider for OpenAiClient {
    fn name(&self) -> &str {
        self.provider_hint.as_deref().unwrap_or("oai-compat")
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    fn context_window(&self) -> usize {
        self.context_window_tokens
    }

    async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse> {
        if self.use_streaming {
            self.chat_completion_streaming(request).await
        } else {
            self.chat_completion(request).await
        }
    }

    async fn send_streaming(
        &self,
        request: &MessagesRequest,
        on_text: &(dyn Fn(String) + Send + Sync),
    ) -> Result<MessagesResponse> {
        self.chat_completion_streaming_with_callback(request, on_text)
            .await
    }
}

// ── API key resolution ──────────────────────────────────────────────────

/// Resolve an OpenAI-compatible API key.
///
/// Delegates to `Config::resolve_api_key_for_provider("openrouter", ...)` which checks:
/// 1. `[llm_endpoints]` — matching endpoint's api_key / api_key_file / key_env
/// 2. Environment variables (OPENROUTER_API_KEY, OPENAI_API_KEY)
/// 3. `[native_executor]` api_key (legacy path)
fn resolve_openai_api_key() -> Result<String> {
    let workgraph_dir = std::path::Path::new(".workgraph");
    resolve_openai_api_key_from_dir(workgraph_dir)
}

/// Resolve API key from a specific workgraph directory.
///
/// Loads config and delegates to `Config::resolve_api_key_for_provider`.
pub fn resolve_openai_api_key_from_dir(workgraph_dir: &std::path::Path) -> Result<String> {
    use crate::config::Config;
    let config = Config::load_merged(workgraph_dir).unwrap_or_default();
    config.resolve_api_key_for_provider("openrouter", workgraph_dir)
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503)
}

pub fn is_retryable_status(status: u16) -> bool {
    is_retryable(status)
}

/// Check whether an API error indicates the context/prompt is too long.
///
/// Returns true for HTTP 413 (payload too large) or HTTP 400 when the error
/// message mentions context length, token limits, or prompt size.
pub fn is_context_too_long(error: &anyhow::Error) -> bool {
    if let Some(api_err) = error.downcast_ref::<ApiError>() {
        if api_err.status == 413 {
            return true;
        }
        if api_err.status == 400 {
            let msg = api_err.message.to_lowercase();
            return msg.contains("context")
                || msg.contains("too long")
                || msg.contains("too large")
                || msg.contains("token")
                || msg.contains("maximum")
                || msg.contains("prompt");
        }
    }
    false
}

pub fn max_retries_for_status(status: u16) -> usize {
    match status {
        429 => 5,
        500 | 502 | 503 => 3,
        _ => 0,
    }
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            401 => write!(
                f,
                "Authentication failed (HTTP 401): {}. Check your API key configuration.",
                self.message
            ),
            403 => write!(
                f,
                "Access denied (HTTP 403): {}. Check your API key permissions.",
                self.message
            ),
            _ => write!(f, "API error {}: {}", self.status, self.message),
        }
    }
}

impl std::error::Error for ApiError {}

fn oai_api_error(status: u16, body: &str) -> anyhow::Error {
    let message = if let Ok(err) = serde_json::from_str::<OaiErrorResponse>(body) {
        err.error.message
    } else {
        truncate(body, 500).to_string()
    };
    ApiError { status, message }.into()
}

fn parse_retry_after_oai(body: &str) -> Option<u64> {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(secs) = val
            .get("error")
            .and_then(|e| e.get("metadata"))
            .and_then(|m| m.get("retry_after"))
            .and_then(|v| v.as_f64())
    {
        return Some((secs * 1000.0) as u64);
    }
    None
}

/// Add jitter to a backoff duration to prevent thundering herd.
///
/// Returns `base_ms ± 25%` using a cheap pseudo-random source (no `rand` crate needed).
/// The jitter is deterministic per call-site but varies across retries and threads.
fn jittered_backoff(base_ms: u64) -> u64 {
    // Use current time nanos as a cheap entropy source
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    // Mix in thread id for cross-thread variance
    let tid = std::thread::current().id();
    let hash = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(format!("{:?}", tid).len() as u64);
    // ±25% jitter
    let jitter_range = base_ms / 4;
    if jitter_range == 0 {
        return base_ms;
    }
    let offset = hash % (jitter_range * 2);
    base_ms.saturating_sub(jitter_range).saturating_add(offset)
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..s.floor_char_boundary(max)]
    }
}

// ── Inline thinking token extraction ────────────────────────────────
//
// Models like MiniMax M2.7, DeepSeek R1, and Qwen QwQ may embed thinking
// content as <think>...</think> tags inline in the content field (especially
// via the "content-string" return mechanism). This extracts them into a
// separate thinking block.

/// Extract inline `<think>...</think>` tags from text content.
///
/// Returns `(remaining_text, Option<thinking_content>)`. If no thinking tags
/// are found, the original text is returned unchanged and thinking is None.
fn extract_inline_thinking(text: &str) -> (String, Option<String>) {
    let mut thinking_parts = Vec::new();
    let mut remaining = text.to_string();

    loop {
        let Some(start) = remaining.find("<think>") else {
            break;
        };
        let search_from = start + "<think>".len();
        let Some(end_offset) = remaining[search_from..].find("</think>") else {
            // Unclosed <think> tag — treat the rest as thinking content
            let thinking = remaining[search_from..].trim().to_string();
            if !thinking.is_empty() {
                thinking_parts.push(thinking);
            }
            remaining = remaining[..start].trim_end().to_string();
            break;
        };
        let end = search_from + end_offset;
        let thinking = remaining[search_from..end].trim().to_string();
        if !thinking.is_empty() {
            thinking_parts.push(thinking);
        }
        remaining = format!(
            "{}{}",
            remaining[..start].trim_end(),
            remaining[end + "</think>".len()..].trim_start()
        );
    }

    if thinking_parts.is_empty() {
        (remaining, None)
    } else {
        let combined = thinking_parts.join("\n\n");
        (remaining.trim().to_string(), Some(combined))
    }
}

// ── Text-based tool call extraction ──────────────────────────────────
//
// Some models (especially via OpenRouter) output tool calls as text instead of
// using the structured `tool_calls` response field.  Common formats:
//
//   <tool_call>{"name": "bash", "arguments": {"command": "ls"}}</tool_call>
//   <function=bash>{"command": "ls"}</function>
//   ```json\n{"name": "bash", "arguments": {"command": "ls"}}\n```
//
// This fallback parser detects these, extracts valid tool calls, and converts
// them to `ContentBlock::ToolUse` so the agent loop can execute them.

/// Try to extract tool calls from text content.
///
/// Returns `(remaining_text, extracted_tool_calls)`.  If no tool calls are found
/// the original text is returned unchanged and the tool calls vec is empty.
fn extract_tool_calls_from_text(text: &str) -> (String, Vec<ContentBlock>) {
    let mut tool_calls = Vec::new();
    let mut remaining = text.to_string();
    let mut call_counter = 0u32;

    // Pattern 1: XML-style <tool_call>...</tool_call> (Hermes / ChatML format)
    loop {
        let Some(start) = remaining.find("<tool_call>") else {
            break;
        };
        let search_from = start + "<tool_call>".len();
        let Some(end_offset) = remaining[search_from..].find("</tool_call>") else {
            break;
        };
        let end = search_from + end_offset;
        let inner = remaining[search_from..end].trim();

        if let Some(tc) = parse_tool_call_json(inner, &mut call_counter) {
            tool_calls.push(tc);
        }
        // Remove the whole tag from remaining text
        remaining = format!(
            "{}{}",
            remaining[..start].trim_end(),
            remaining[end + "</tool_call>".len()..].trim_start()
        );
    }

    // Pattern 2: <function=name>...</function> (Llama / Fireworks format)
    loop {
        let Some(start) = remaining.find("<function=") else {
            break;
        };
        let after_eq = start + "<function=".len();
        let Some(gt) = remaining[after_eq..].find('>') else {
            break;
        };
        let name = remaining[after_eq..after_eq + gt].to_string();
        let body_start = after_eq + gt + 1;
        let Some(end_offset) = remaining[body_start..].find("</function>") else {
            break;
        };
        let body_end = body_start + end_offset;
        let body = remaining[body_start..body_end].trim();

        if let Ok(args) = serde_json::from_str::<serde_json::Value>(body) {
            call_counter += 1;
            tool_calls.push(ContentBlock::ToolUse {
                id: format!("text_call_{}", call_counter),
                name,
                input: args,
            });
        }
        remaining = format!(
            "{}{}",
            remaining[..start].trim_end(),
            remaining[body_end + "</function>".len()..].trim_start()
        );
    }

    // Pattern 3: provider-specific tags like <|tool_call|>...<|/tool_call|>
    // or </minimax:tool_call> variants
    loop {
        // Match <*tool_call*>...</*tool_call*> with optional provider prefix
        let tag_start = remaining
            .find("<|tool_call|>")
            .or_else(|| remaining.find("<tool_call "))
            .or_else(|| {
                // Match <provider:tool_call>
                let re_start = remaining.find(":tool_call>");
                re_start.and_then(|pos| {
                    // Walk back to find the '<'
                    remaining[..pos].rfind('<')
                })
            });
        let Some(start) = tag_start else {
            break;
        };

        // Find where the opening tag ends (the '>') so we can search for the
        // closing tag AFTER it. This prevents matching the opening tag's own
        // ':tool_call>' suffix as the close tag (e.g. <minimax:tool_call>).
        let open_end = remaining[start..]
            .find('>')
            .map(|p| start + p + 1)
            .unwrap_or(start);

        // Find the matching closing tag — search from after the opening tag.
        let close_patterns = ["</tool_call>", "<|/tool_call|>", "<|tool_call_end|>"];
        let close_match = close_patterns.iter().find_map(|pat| {
            remaining[open_end..]
                .find(pat)
                .map(|offset| (open_end + offset, pat.len()))
        });
        // Also check for :tool_call> closing (e.g., </minimax:tool_call>)
        let close_match = close_match.or_else(|| {
            remaining[open_end..].find(":tool_call>").map(|offset| {
                // Walk back to find '</' or '<'
                let tag_start = remaining[open_end..open_end + offset]
                    .rfind('<')
                    .map(|p| open_end + p)
                    .unwrap_or(open_end + offset.saturating_sub(1));
                (
                    tag_start,
                    offset + ":tool_call>".len() - (tag_start - open_end),
                )
            })
        });

        let Some((close_start, close_len)) = close_match else {
            break;
        };

        // Extract content between open and close tags
        let inner = remaining[open_end..close_start].trim();

        if let Some(tc) = parse_tool_call_json(inner, &mut call_counter) {
            tool_calls.push(tc);
        }
        remaining = format!(
            "{}{}",
            remaining[..start].trim_end(),
            remaining[close_start + close_len..].trim_start()
        );
    }

    // Pattern 4: Hermes/Qwen3 <|plugin|>...<|/plugin|> format
    loop {
        let Some(start) = remaining.find("<|plugin|>") else {
            break;
        };
        let search_from = start + "<|plugin|>".len();
        let Some(end_offset) = remaining[search_from..].find("<|/plugin|>") else {
            break;
        };
        let end = search_from + end_offset;
        let inner = remaining[search_from..end].trim();

        if let Some(tc) = parse_tool_call_json(inner, &mut call_counter) {
            tool_calls.push(tc);
        }
        // Remove the whole tag from remaining text, preserving newline between segments
        let before = remaining[..start].trim_end();
        let after = remaining[end + "<|/plugin|>".len()..].trim_start();
        remaining = if !before.is_empty() && !after.is_empty() {
            format!("{}\n{}", before, after)
        } else {
            format!("{}{}", before, after)
        };
    }

    // Trim the remaining text
    let remaining = remaining.trim().to_string();

    (remaining, tool_calls)
}

/// Parse a JSON string as a tool call. Accepts these formats:
/// - `{"name": "tool", "arguments": {...}}`
/// - `{"name": "tool", "parameters": {...}}`
/// - `{"tool": "name", "arguments": {...}}`  (some models swap field names)
fn parse_tool_call_json(json_str: &str, counter: &mut u32) -> Option<ContentBlock> {
    let val: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let obj = val.as_object()?;

    let name = obj
        .get("name")
        .or_else(|| obj.get("tool"))
        .or_else(|| obj.get("function"))
        .and_then(|v| v.as_str())
        .map(String::from)?;

    let input = obj
        .get("arguments")
        .or_else(|| obj.get("parameters"))
        .or_else(|| obj.get("input"))
        .cloned()
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

    // If arguments is a string, try to parse it as JSON
    let input = if let serde_json::Value::String(s) = &input {
        serde_json::from_str(s).unwrap_or(input)
    } else {
        input
    };

    *counter += 1;
    Some(ContentBlock::ToolUse {
        id: format!("text_call_{}", counter),
        name,
        input,
    })
}

/// Attempt to recover valid JSON from malformed input.
///
/// Tries multiple strategies in order:
/// 1. Strip markdown code fences (```json ... ```)
/// 2. Extract first JSON object from surrounding text
/// 3. Complete truncated JSON by closing open braces/brackets
///
/// Returns `Ok(value)` if any strategy succeeds, or `Err` if all fail.
fn try_recover_json(raw: &str) -> Result<serde_json::Value, String> {
    let trimmed = raw.trim();

    // Strategy 1: Strip markdown code fences
    if let Some(inner) = strip_markdown_json(trimmed)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(inner.trim())
    {
        return Ok(v);
    }

    // Strategy 2: Extract first JSON object from surrounding text
    if let Some(start) = trimmed.find('{') {
        // Find the matching closing brace
        let candidate = &trimmed[start..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
            return Ok(v);
        }
        // Try finding a balanced substring
        if let Some(balanced) = find_balanced_json(candidate)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(balanced)
        {
            return Ok(v);
        }
    }

    // Strategy 3: Complete truncated JSON
    if let Some(completed) = complete_truncated_json(trimmed)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&completed)
    {
        return Ok(v);
    }

    Err(format!(
        "all recovery strategies failed for: {}",
        truncate(raw, 200)
    ))
}

/// Strip markdown code fences: ```json\n...\n``` or ```\n...\n```
fn strip_markdown_json(s: &str) -> Option<&str> {
    let s = s.trim();
    if s.starts_with("```") {
        // Find end of first line (skip ```json or ```)
        let after_fence = s.get(3..)?;
        let content_start = after_fence.find('\n').map(|i| 3 + i + 1)?;
        // Find closing fence
        let content = s.get(content_start..)?;
        if let Some(end) = content.rfind("```") {
            return Some(content.get(..end)?.trim());
        }
        // No closing fence — treat rest as content
        return Some(content.trim());
    }
    None
}

/// Find a balanced JSON object starting from the beginning of `s`.
fn find_balanced_json(s: &str) -> Option<&str> {
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, ch) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Try to complete truncated JSON by closing open braces/brackets and strings.
fn complete_truncated_json(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        return None;
    }
    // Only attempt if we have at least one key-value pair
    if !trimmed.contains(':') {
        return None;
    }

    let mut result = trimmed.to_string();
    let mut depth_brace = 0i32;
    let mut depth_bracket = 0i32;
    let mut in_string = false;
    let mut escape = false;

    for ch in trimmed.chars() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => depth_brace += 1,
            '}' => depth_brace -= 1,
            '[' => depth_bracket += 1,
            ']' => depth_bracket -= 1,
            _ => {}
        }
    }

    // Close open string if needed
    if in_string {
        result.push('"');
    }

    // Close open brackets then braces
    for _ in 0..depth_bracket {
        result.push(']');
    }
    for _ in 0..depth_brace {
        result.push('}');
    }

    if depth_brace > 0 || depth_bracket > 0 || in_string {
        Some(result)
    } else {
        None
    }
}

/// Build a structured error input for when tool arguments fail to parse.
///
/// First attempts JSON recovery (markdown stripping, object extraction,
/// truncation completion). Falls back to a `__parse_error` object that the
/// agent loop detects and surfaces as a tool error to the model.
fn make_parse_error_input(raw_arguments: &str, _error_message: &str) -> serde_json::Value {
    // Try to recover before giving up
    if let Ok(recovered) = try_recover_json(raw_arguments) {
        eprintln!(
            "[openai-client] Recovered malformed JSON tool arguments (len={})",
            raw_arguments.len()
        );
        return recovered;
    }

    serde_json::json!({
        "__parse_error": _error_message,
        "__raw_arguments": raw_arguments,
    })
}

// ── OpenRouter key status ───────────────────────────────────────────────

/// OpenRouter API key status returned by the `/api/v1/key` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterKeyStatus {
    /// Total credit limit in USD
    #[serde(default)]
    pub limit: f64,
    /// Remaining credit limit in USD
    #[serde(default)]
    pub limit_remaining: f64,
    /// Total usage in USD
    #[serde(default)]
    pub usage: f64,
    /// Daily usage in USD
    #[serde(default)]
    pub usage_daily: f64,
    /// Weekly usage in USD
    #[serde(default)]
    pub usage_weekly: f64,
    /// Monthly usage in USD
    #[serde(default)]
    pub usage_monthly: f64,
    /// Whether this is a free tier key
    #[serde(default)]
    pub is_free_tier: bool,
}

impl OpenRouterKeyStatus {
    /// Calculate the usage percentage of the total limit
    pub fn usage_percentage(&self) -> f64 {
        if self.limit <= 0.0 {
            0.0
        } else {
            (self.usage / self.limit) * 100.0
        }
    }

    /// Check if usage is above the given threshold percentage
    pub fn is_above_threshold(&self, threshold_percent: f64) -> bool {
        self.usage_percentage() >= threshold_percent
    }

    /// Check if the key is approaching or at the limit
    pub fn is_near_limit(&self, buffer: f64) -> bool {
        self.limit_remaining <= buffer
    }
}

// ── OpenRouter model discovery ──────────────────────────────────────────

/// A model returned by the OpenRouter `/api/v1/models` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: Option<OpenRouterPricing>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub architecture: Option<OpenRouterArchitecture>,
    #[serde(default)]
    pub top_provider: Option<OpenRouterTopProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterPricing {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub completion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterArchitecture {
    #[serde(default)]
    pub modality: Option<String>,
    #[serde(default)]
    pub tokenizer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterTopProvider {
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    #[serde(default)]
    pub is_moderated: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterModelsResponse {
    data: Vec<OpenRouterModel>,
}

/// Fetch models from an OpenRouter-compatible API.
///
/// Queries `GET {base_url}/models` and returns the full model list.
pub async fn fetch_openrouter_models(
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Vec<OpenRouterModel>> {
    let base = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
    let url = format!("{}/models", base);

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = http
        .get(&url)
        .header("authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .context("Failed to fetch models from API")?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(oai_api_error(status, &body));
    }

    let models_resp: OpenRouterModelsResponse = resp
        .json()
        .await
        .context("Failed to parse models response")?;

    Ok(models_resp.data)
}

/// Blocking version of `fetch_openrouter_models` for CLI use.
pub fn fetch_openrouter_models_blocking(
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Vec<OpenRouterModel>> {
    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
    rt.block_on(fetch_openrouter_models(api_key, base_url))
}

/// Fetch OpenRouter API key status asynchronously.
pub async fn fetch_openrouter_key_status(
    api_key: &str,
    base_url: Option<&str>,
) -> Result<OpenRouterKeyStatus> {
    let base = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');
    let url = format!("{}/key", base);

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = http
        .get(&url)
        .header("authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .context("Failed to fetch key status from OpenRouter API")?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(oai_api_error(status, &body));
    }

    let key_status: OpenRouterKeyStatus = resp
        .json()
        .await
        .context("Failed to parse key status response")?;

    Ok(key_status)
}

/// Fetch OpenRouter API key status synchronously (blocking).
pub fn fetch_openrouter_key_status_blocking(
    api_key: &str,
    base_url: Option<&str>,
) -> Result<OpenRouterKeyStatus> {
    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
    rt.block_on(fetch_openrouter_key_status(api_key, base_url))
}

// ── OpenRouter auto-routing & model validation ──────────────────────────

/// The OpenRouter auto-routing model specifier.
///
/// When used as the model ID, OpenRouter's intelligent routing selects the best
/// model for each request based on the prompt content. This is the recommended
/// default when no specific model is configured for OpenRouter.
pub const OPENROUTER_AUTO_MODEL: &str = "openrouter/auto";

/// Result of validating a model ID against the cached OpenRouter model list.
#[derive(Debug, Clone)]
pub struct ModelValidationResult {
    /// The model to actually use (may differ from the input if fallback was applied).
    pub model: String,
    /// Whether the originally requested model was valid.
    pub was_valid: bool,
    /// Suggested alternatives (populated only when the model was invalid).
    pub suggestions: Vec<String>,
    /// Warning message to display (if any).
    pub warning: Option<String>,
}

/// Validate a model ID against the cached OpenRouter model list.
///
/// If the model is `openrouter/auto`, it is always considered valid.
/// Any `provider:` prefix (e.g., `openrouter:minimax/minimax-m2.7`) is stripped
/// before validation so the bare model ID is checked against the cache.
///
/// If a local cache exists and the model is not found after stripping, the
/// function returns `was_valid: false` with suggestions but does NOT fall back
/// to `openrouter/auto` — callers decide how to handle the failure.
///
/// If no cache is available, the model is assumed valid (we can't validate
/// without data).
pub fn validate_openrouter_model(
    model: &str,
    workgraph_dir: &std::path::Path,
) -> ModelValidationResult {
    // Strip any known provider prefix (e.g., "openrouter:minimax/minimax-m2.7"
    // → "minimax/minimax-m2.7") so validation checks the bare model ID.
    let stripped = {
        let spec = crate::config::parse_model_spec(model);
        if spec.provider.is_some() {
            spec.model_id
        } else {
            model.to_string()
        }
    };
    let model = stripped.as_str();

    // openrouter/auto is always valid
    if model == OPENROUTER_AUTO_MODEL {
        return ModelValidationResult {
            model: model.to_string(),
            was_valid: true,
            suggestions: vec![],
            warning: None,
        };
    }

    // Try to load cache
    let cache_path = workgraph_dir.join("model_cache.json");
    let cache_content = match std::fs::read_to_string(&cache_path) {
        Ok(c) => c,
        Err(_) => {
            // No cache available — can't validate, pass through
            return ModelValidationResult {
                model: model.to_string(),
                was_valid: true,
                suggestions: vec![],
                warning: None,
            };
        }
    };

    #[derive(Deserialize)]
    struct CacheFile {
        models: Vec<CacheModel>,
    }
    #[derive(Deserialize)]
    struct CacheModel {
        id: String,
    }

    let cache: CacheFile = match serde_json::from_str(&cache_content) {
        Ok(c) => c,
        Err(_) => {
            return ModelValidationResult {
                model: model.to_string(),
                was_valid: true,
                suggestions: vec![],
                warning: None,
            };
        }
    };

    let model_ids: Vec<&str> = cache.models.iter().map(|m| m.id.as_str()).collect();

    // Check if model exists in cache
    if model_ids.contains(&model) {
        return ModelValidationResult {
            model: model.to_string(),
            was_valid: true,
            suggestions: vec![],
            warning: None,
        };
    }

    // Model not found — find closest matches
    let suggestions = find_closest_models(model, &model_ids, 3);
    let suggestions_str = if suggestions.is_empty() {
        String::new()
    } else {
        format!(
            "\n  Did you mean one of:\n{}",
            suggestions
                .iter()
                .map(|s| format!("    - {}", s))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    // Do NOT fall back to openrouter/auto — return the stripped model with
    // was_valid: false so callers can fail with a clear error.
    let search_hint = model.split('/').next_back().unwrap_or(model);
    let warning = format!(
        "Model '{}' not found in OpenRouter model list.{}\n  \
         Hint: run `wg models search {}` to find valid alternatives, \
         or `wg models list` to see the local registry.",
        model, suggestions_str, search_hint,
    );

    ModelValidationResult {
        model: model.to_string(),
        was_valid: false,
        suggestions,
        warning: Some(warning),
    }
}

/// Result of resolving a short model name against the cached model list.
#[derive(Debug, Clone)]
pub struct ModelResolutionResult {
    /// The resolved full model ID (e.g., "minimax/minimax-m2.7"), or None if no match.
    pub resolved: Option<String>,
    /// Suggested alternatives if the short name was ambiguous or had no exact match.
    pub suggestions: Vec<String>,
}

/// Resolve a short model name to a full OpenRouter model ID using the cached model list.
///
/// Resolution strategy (in order):
/// 1. Exact match on full ID (e.g., "minimax/minimax-m2.7" matches directly)
/// 2. Suffix match: bare name matches the part after `/` (e.g., "minimax-m2.7" → "minimax/minimax-m2.7")
/// 3. Substring match: bare name appears in the model ID (e.g., "m2.7" → "minimax/minimax-m2.7")
///
/// If multiple candidates match in step 2 or 3, returns `None` with suggestions
/// (ambiguous resolution is not auto-resolved).
///
/// Returns `ModelResolutionResult` with the resolved ID or suggestions.
pub fn resolve_short_model_name(
    model: &str,
    workgraph_dir: &std::path::Path,
) -> ModelResolutionResult {
    // Strip any provider prefix first
    let spec = crate::config::parse_model_spec(model);
    let bare = if spec.provider.is_some() {
        &spec.model_id
    } else {
        model
    };

    // Try to load cache
    let cache_path = workgraph_dir.join("model_cache.json");
    let cache_content = match std::fs::read_to_string(&cache_path) {
        Ok(c) => c,
        Err(_) => {
            return ModelResolutionResult {
                resolved: None,
                suggestions: vec![],
            };
        }
    };

    #[derive(Deserialize)]
    struct CacheFile {
        models: Vec<CacheModel>,
    }
    #[derive(Deserialize)]
    struct CacheModel {
        id: String,
    }

    let cache: CacheFile = match serde_json::from_str(&cache_content) {
        Ok(c) => c,
        Err(_) => {
            return ModelResolutionResult {
                resolved: None,
                suggestions: vec![],
            };
        }
    };

    let model_ids: Vec<&str> = cache.models.iter().map(|m| m.id.as_str()).collect();
    let bare_lower = bare.to_lowercase();

    // 1. Exact match on full ID
    if model_ids.contains(&bare) {
        return ModelResolutionResult {
            resolved: Some(bare.to_string()),
            suggestions: vec![],
        };
    }

    // 2. If bare contains `/`, try provider/model format match
    if bare.contains('/') {
        // Already a full ID but not found — no resolution possible
        return ModelResolutionResult {
            resolved: None,
            suggestions: find_closest_models(bare, &model_ids, 3),
        };
    }

    // 3. Suffix match: bare name matches the part after `/`
    let suffix_matches: Vec<&str> = model_ids
        .iter()
        .filter(|id| {
            id.split('/')
                .next_back()
                .map(|name| name.to_lowercase() == bare_lower)
                .unwrap_or(false)
        })
        .copied()
        .collect();

    if suffix_matches.len() == 1 {
        return ModelResolutionResult {
            resolved: Some(suffix_matches[0].to_string()),
            suggestions: vec![],
        };
    }
    if suffix_matches.len() > 1 {
        return ModelResolutionResult {
            resolved: None,
            suggestions: suffix_matches.iter().map(|s| s.to_string()).collect(),
        };
    }

    // 4. Substring match: bare name appears as a substring in the model name part
    let substring_matches: Vec<&str> = model_ids
        .iter()
        .filter(|id| {
            id.split('/')
                .next_back()
                .map(|name| name.to_lowercase().contains(&bare_lower))
                .unwrap_or(false)
        })
        .copied()
        .collect();

    if substring_matches.len() == 1 {
        return ModelResolutionResult {
            resolved: Some(substring_matches[0].to_string()),
            suggestions: vec![],
        };
    }
    if !substring_matches.is_empty() {
        return ModelResolutionResult {
            resolved: None,
            suggestions: substring_matches.iter().map(|s| s.to_string()).collect(),
        };
    }

    // 5. No match — fall back to Levenshtein suggestions
    ModelResolutionResult {
        resolved: None,
        suggestions: find_closest_models(bare, &model_ids, 3),
    }
}

/// Find the N closest model IDs to the query by edit distance.
fn find_closest_models(query: &str, candidates: &[&str], n: usize) -> Vec<String> {
    let query_lower = query.to_lowercase();
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .map(|c| (levenshtein_distance(&query_lower, &c.to_lowercase()), *c))
        .collect();
    scored.sort_by_key(|(dist, _)| *dist);
    scored
        .into_iter()
        .take(n)
        .filter(|(dist, _)| *dist <= query.len()) // don't suggest wildly different models
        .map(|(_, id)| id.to_string())
        .collect()
}

/// Levenshtein edit distance between two strings.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Parse the next `data:` line from an SSE buffer, consuming it.
///
/// OpenAI SSE format uses bare `data: <json>` lines separated by blank lines.
/// Unlike Anthropic's format, there is no `event:` prefix — all events are
/// typed by their JSON content. The sentinel `data: [DONE]` signals stream end.
fn parse_next_oai_sse_data(buffer: &mut String) -> Option<String> {
    loop {
        let newline_pos = buffer.find('\n')?;
        let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
        buffer.drain(..newline_pos + 1);

        // Skip empty lines (SSE event separators) and comments
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        // Extract data from "data: ..." lines
        if let Some(data) = line.strip_prefix("data: ") {
            return Some(data.to_string());
        }
        if let Some(data) = line.strip_prefix("data:") {
            return Some(data.to_string());
        }

        // Skip unknown SSE fields (event:, id:, retry:, etc.)
    }
}

/// Assemble a `MessagesResponse` from accumulated streaming state.
fn assemble_oai_stream_response(
    response_id: String,
    text_content: String,
    reasoning_content: String,
    reasoning_details: Vec<serde_json::Value>,
    tool_calls: std::collections::BTreeMap<usize, (String, String, String)>,
    finish_reason: Option<String>,
    usage: Option<OaiUsage>,
) -> Result<MessagesResponse> {
    let mut content_blocks = Vec::new();
    let has_structured_tool_calls = !tool_calls.is_empty();

    // Add thinking/reasoning block FIRST if we accumulated reasoning content
    if !reasoning_content.is_empty() {
        let rd = if reasoning_details.is_empty() {
            None
        } else {
            Some(reasoning_details.clone())
        };
        content_blocks.push(ContentBlock::Thinking {
            thinking: reasoning_content,
            reasoning_details: rd,
        });
    } else if !reasoning_details.is_empty() {
        // Only reasoning_details without plaintext — extract text from entries
        let thinking_text = reasoning_details
            .iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        if !thinking_text.is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                thinking: thinking_text,
                reasoning_details: Some(reasoning_details.clone()),
            });
        }
    }

    if !text_content.is_empty() {
        // Check for inline <think>...</think> tags
        let (clean_text, inline_thinking) = extract_inline_thinking(&text_content);

        let has_inline_thinking = inline_thinking.is_some();
        if let Some(thinking) = inline_thinking
            && !content_blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Thinking { .. }))
        {
            content_blocks.push(ContentBlock::Thinking {
                thinking,
                reasoning_details: None,
            });
        }

        let text_to_process = if has_inline_thinking {
            clean_text
        } else {
            text_content
        };

        if !text_to_process.is_empty() {
            // If no structured tool calls came through the stream, check for text-based ones
            if !has_structured_tool_calls {
                let (remaining, extracted) = extract_tool_calls_from_text(&text_to_process);
                if !extracted.is_empty() {
                    eprintln!(
                        "[openai-client] Extracted {} tool call(s) from streamed text output",
                        extracted.len()
                    );
                    if !remaining.is_empty() {
                        content_blocks.push(ContentBlock::Text { text: remaining });
                    }
                    content_blocks.extend(extracted);
                } else {
                    content_blocks.push(ContentBlock::Text {
                        text: text_to_process,
                    });
                }
            } else {
                content_blocks.push(ContentBlock::Text {
                    text: text_to_process,
                });
            }
        }
    }

    for (_index, (id, name, arguments)) in tool_calls {
        let input: serde_json::Value = match serde_json::from_str(&arguments) {
            Ok(v) => v,
            Err(e) => make_parse_error_input(&arguments, &e.to_string()),
        };
        content_blocks.push(ContentBlock::ToolUse { id, name, input });
    }

    if content_blocks.is_empty() {
        content_blocks.push(ContentBlock::Text {
            text: String::new(),
        });
    }

    // Determine stop reason — override to ToolUse if we extracted tool calls
    let has_tool_use = content_blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
    let stop_reason = if has_tool_use && !matches!(finish_reason.as_deref(), Some("tool_calls")) {
        Some(StopReason::ToolUse)
    } else {
        match finish_reason.as_deref() {
            Some("stop") => Some(StopReason::EndTurn),
            Some("tool_calls") => Some(StopReason::ToolUse),
            Some("length") => Some(StopReason::MaxTokens),
            Some("content_filter") => Some(StopReason::StopSequence),
            _ => None,
        }
    };

    let usage = usage
        .map(|u| {
            let (cache_read, cache_creation) = u
                .prompt_tokens_details
                .map(|d| (d.cached_tokens, d.cache_write_tokens))
                .unwrap_or((None, None));
            let reasoning_tokens = u.completion_tokens_details.and_then(|d| d.reasoning_tokens);
            Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                cache_creation_input_tokens: cache_creation,
                cache_read_input_tokens: cache_read,
                reasoning_tokens,
            }
        })
        .unwrap_or_default();

    Ok(MessagesResponse {
        id: response_id,
        content: content_blocks,
        stop_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::native::client::{ContentBlock, Message, Role, ToolDefinition};
    use serde_json::json;

    #[test]
    fn test_translate_tools() {
        let tools = vec![ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a shell command".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }),
        }];

        let oai_tools = OpenAiClient::translate_tools(&tools);
        assert_eq!(oai_tools.len(), 1);
        assert_eq!(oai_tools[0].tool_type, "function");
        assert_eq!(oai_tools[0].function.name, "bash");
    }

    #[test]
    fn test_translate_messages_with_system() {
        let system = Some("You are a helpful assistant.".to_string());
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        }];

        let oai_msgs = OpenAiClient::translate_messages(&system, &messages);
        assert_eq!(oai_msgs.len(), 2);
        assert_eq!(oai_msgs[0].role, "system");
        assert_eq!(
            oai_msgs[0].content.as_deref(),
            Some("You are a helpful assistant.")
        );
        assert_eq!(oai_msgs[1].role, "user");
        assert_eq!(oai_msgs[1].content.as_deref(), Some("Hello"));
    }

    #[test]
    fn test_translate_messages_with_tool_results() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_123".to_string(),
                content: "result data".to_string(),
                is_error: false,
            }],
        }];

        let oai_msgs = OpenAiClient::translate_messages(&None, &messages);
        assert_eq!(oai_msgs.len(), 1);
        assert_eq!(oai_msgs[0].role, "tool");
        assert_eq!(oai_msgs[0].tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(oai_msgs[0].content.as_deref(), Some("result data"));
    }

    #[test]
    fn test_translate_messages_with_assistant_tool_calls() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Let me run that.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_456".to_string(),
                    name: "bash".to_string(),
                    input: json!({"command": "ls"}),
                },
            ],
        }];

        let oai_msgs = OpenAiClient::translate_messages(&None, &messages);
        assert_eq!(oai_msgs.len(), 1);
        assert_eq!(oai_msgs[0].role, "assistant");
        assert_eq!(oai_msgs[0].content.as_deref(), Some("Let me run that."));
        let tc = oai_msgs[0].tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_456");
        assert_eq!(tc[0].function.name, "bash");
    }

    /// Regression test: a two-turn conversation with tool use on turn 1
    /// must produce a valid OAI message sequence with no consecutive same-
    /// role messages (except tool results after assistant tool_calls).
    #[test]
    fn test_translate_messages_two_turn_with_tool_use() {
        let system = Some("You are helpful.".to_string());
        let messages = vec![
            // Turn 1: user message
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Check a file".to_string(),
                }],
            },
            // Turn 1: assistant responds with tool call
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Let me check.".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        input: json!({"file_path": "/tmp/test.txt"}),
                    },
                ],
            },
            // Turn 1: tool result
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "file contents here".to_string(),
                    is_error: false,
                }],
            },
            // Turn 1: assistant end turn
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Found the file.".to_string(),
                }],
            },
            // Turn 2: new user message
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Now do something else".to_string(),
                }],
            },
        ];

        let oai_msgs = OpenAiClient::translate_messages(&system, &messages);

        // Expected: system, user, assistant(tool_calls), tool, assistant, user
        assert_eq!(oai_msgs.len(), 6);
        assert_eq!(oai_msgs[0].role, "system");
        assert_eq!(oai_msgs[1].role, "user");
        assert_eq!(oai_msgs[2].role, "assistant");
        assert!(oai_msgs[2].tool_calls.is_some());
        assert_eq!(oai_msgs[3].role, "tool");
        assert_eq!(oai_msgs[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(oai_msgs[4].role, "assistant");
        assert_eq!(oai_msgs[5].role, "user");
        assert_eq!(
            oai_msgs[5].content.as_deref(),
            Some("Now do something else")
        );

        // Verify no invalid consecutive same-role pairs (except tool after assistant)
        for i in 1..oai_msgs.len() {
            let prev = &oai_msgs[i - 1].role;
            let curr = &oai_msgs[i].role;
            if prev == curr && curr != "tool" {
                panic!(
                    "Invalid consecutive same-role messages at index {}: {} → {}",
                    i, prev, curr
                );
            }
        }
    }

    /// Regression: conversation with thinking blocks from inline <think> tags
    /// must not produce invalid OAI messages when replayed on the second turn.
    #[test]
    fn test_translate_messages_thinking_block_dropped_cleanly() {
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "let me think about this".to_string(),
                        reasoning_details: None, // inline <think> tags have no reasoning_details
                    },
                    ContentBlock::Text {
                        text: "Hi there!".to_string(),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "second message".to_string(),
                }],
            },
        ];

        let oai_msgs = OpenAiClient::translate_messages(&None, &messages);

        // Thinking block without reasoning_details is dropped (correct).
        // Should produce: user, assistant, user — 3 messages.
        assert_eq!(oai_msgs.len(), 3);
        assert_eq!(oai_msgs[0].role, "user");
        assert_eq!(oai_msgs[1].role, "assistant");
        assert_eq!(oai_msgs[1].content.as_deref(), Some("Hi there!"));
        assert_eq!(oai_msgs[2].role, "user");
    }

    #[test]
    fn test_translate_response_text_only() {
        let oai = OaiResponse {
            id: "chatcmpl-123".to_string(),
            choices: vec![OaiChoice {
                message: OaiResponseMessage {
                    role: "assistant".to_string(),
                    content: Some("Hello!".to_string()),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_details: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OaiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        };

        let resp = OpenAiClient::translate_response(oai).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "Hello!"));
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn test_translate_response_with_tool_calls() {
        let oai = OaiResponse {
            id: "chatcmpl-456".to_string(),
            choices: vec![OaiChoice {
                message: OaiResponseMessage {
                    role: "assistant".to_string(),
                    content: None,
                    reasoning: None,
                    reasoning_details: None,
                    tool_calls: Some(vec![OaiToolCall {
                        id: "call_789".to_string(),
                        call_type: "function".to_string(),
                        function: OaiToolCallFunction {
                            name: "bash".to_string(),
                            arguments: r#"{"command":"ls -la"}"#.to_string(),
                        },
                    }]),
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: Some(OaiUsage {
                prompt_tokens: 20,
                completion_tokens: 15,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        };

        let resp = OpenAiClient::translate_response(oai).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(
            &resp.content[0],
            ContentBlock::ToolUse { id, name, input }
            if id == "call_789" && name == "bash" && input.get("command").is_some()
        ));
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn test_translate_response_max_tokens() {
        let oai = OaiResponse {
            id: "chatcmpl-max".to_string(),
            choices: vec![OaiChoice {
                message: OaiResponseMessage {
                    role: "assistant".to_string(),
                    content: Some("partial...".to_string()),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_details: None,
                },
                finish_reason: Some("length".to_string()),
            }],
            usage: None,
        };

        let resp = OpenAiClient::translate_response(oai).unwrap();
        assert_eq!(resp.stop_reason, Some(StopReason::MaxTokens));
    }

    // ── Provider hint and header tests ──────────────────────────────────

    #[test]
    fn test_openrouter_headers_included() {
        use super::super::provider::Provider;

        let client = OpenAiClient::new("test-key".into(), "test/model", None)
            .unwrap()
            .with_provider_hint("openrouter");
        let headers = client.build_headers();
        assert!(headers.contains_key("http-referer"));
        assert!(headers.contains_key("x-title"));
        assert_eq!(client.name(), "openrouter");
    }

    #[test]
    fn test_non_openrouter_no_extra_headers() {
        use super::super::provider::Provider;

        let client = OpenAiClient::new("test-key".into(), "gpt-4o", None)
            .unwrap()
            .with_provider_hint("openai");
        let headers = client.build_headers();
        assert!(!headers.contains_key("http-referer"));
        assert!(!headers.contains_key("x-title"));
        assert_eq!(client.name(), "openai");
    }

    #[test]
    fn test_provider_hint_default_name() {
        use super::super::provider::Provider;

        let client = OpenAiClient::new("test-key".into(), "gpt-4o", None).unwrap();
        assert_eq!(client.name(), "oai-compat");
    }

    #[test]
    fn test_openrouter_enables_streaming() {
        let client = OpenAiClient::new("test-key".into(), "model", None)
            .unwrap()
            .with_provider_hint("openrouter");
        assert!(client.use_streaming);
    }

    #[test]
    fn test_default_no_streaming() {
        let client = OpenAiClient::new("test-key".into(), "model", None).unwrap();
        assert!(!client.use_streaming);
    }

    #[test]
    fn test_streaming_override() {
        let client = OpenAiClient::new("test-key".into(), "model", None)
            .unwrap()
            .with_provider_hint("openrouter")
            .with_streaming(false);
        assert!(!client.use_streaming);
    }

    #[test]
    fn test_openrouter_url_construction() {
        let client = OpenAiClient::new("test-key".into(), "minimax/minimax-m2.5", None).unwrap();
        assert!(client.base_url.ends_with("/v1"));
        let expected = format!("{}/chat/completions", client.base_url);
        assert_eq!(expected, "https://openrouter.ai/api/v1/chat/completions");
    }

    // ── SSE parsing tests ───────────────────────────────────────────────

    #[test]
    fn test_parse_oai_sse_data_basic() {
        let mut buf = "data: {\"id\":\"chatcmpl-1\"}\n\n".to_string();
        let data = parse_next_oai_sse_data(&mut buf).unwrap();
        assert_eq!(data, r#"{"id":"chatcmpl-1"}"#);
    }

    #[test]
    fn test_parse_oai_sse_data_done_sentinel() {
        let mut buf = "data: [DONE]\n\n".to_string();
        let data = parse_next_oai_sse_data(&mut buf).unwrap();
        assert_eq!(data, "[DONE]");
    }

    #[test]
    fn test_parse_oai_sse_skips_comments_and_blanks() {
        let mut buf = ": keep-alive\n\ndata: {\"ok\":true}\n".to_string();
        let data = parse_next_oai_sse_data(&mut buf).unwrap();
        assert_eq!(data, r#"{"ok":true}"#);
    }

    #[test]
    fn test_parse_oai_sse_no_space_after_data() {
        let mut buf = "data:{\"x\":1}\n".to_string();
        let data = parse_next_oai_sse_data(&mut buf).unwrap();
        assert_eq!(data, r#"{"x":1}"#);
    }

    #[test]
    fn test_parse_oai_sse_incomplete_returns_none() {
        let mut buf = "data: {\"partial".to_string();
        assert!(parse_next_oai_sse_data(&mut buf).is_none());
        assert_eq!(buf, "data: {\"partial");
    }

    #[test]
    fn test_parse_oai_sse_multiple_events() {
        let mut buf = "data: first\ndata: second\n".to_string();
        assert_eq!(parse_next_oai_sse_data(&mut buf).unwrap(), "first");
        assert_eq!(parse_next_oai_sse_data(&mut buf).unwrap(), "second");
        assert!(parse_next_oai_sse_data(&mut buf).is_none());
    }

    #[test]
    fn test_parse_oai_sse_crlf_line_endings() {
        let mut buf = "data: {\"cr\":true}\r\n\r\n".to_string();
        let data = parse_next_oai_sse_data(&mut buf).unwrap();
        assert_eq!(data, r#"{"cr":true}"#);
    }

    // ── Parse error handling tests ────────────────────────────────────────

    #[test]
    fn test_json_parse_malformed_returns_error() {
        let mut tool_calls = std::collections::BTreeMap::new();
        tool_calls.insert(
            0,
            (
                "call_bad".to_string(),
                "bash".to_string(),
                r#"{invalid json here}"#.to_string(),
            ),
        );

        let resp = assemble_oai_stream_response(
            "gen-parse-err".to_string(),
            String::new(),
            String::new(),
            vec![],
            tool_calls,
            Some("tool_calls".to_string()),
            None,
        )
        .unwrap();

        assert_eq!(resp.content.len(), 1);
        let ContentBlock::ToolUse { id, name, input } = &resp.content[0] else {
            panic!("Expected ToolUse block");
        };
        assert_eq!(id, "call_bad");
        assert_eq!(name, "bash");
        assert!(input.get("__parse_error").is_some());
        assert!(input.get("__raw_arguments").is_some());
        assert_eq!(
            input.get("__raw_arguments").and_then(|v| v.as_str()),
            Some("{invalid json here}")
        );
    }

    #[test]
    fn test_make_parse_error_input() {
        // Truncated JSON should be recovered by completing the braces
        let input = make_parse_error_input(r#"{"broken":true"#, "expected `}` at line 1 column 14");
        // Recovery should succeed: {"broken":true} is valid
        assert_eq!(input.get("broken").and_then(|v| v.as_bool()), Some(true));
        assert!(
            input.get("__parse_error").is_none(),
            "should have recovered"
        );

        // Truly unrecoverable input falls back to __parse_error
        let bad_input = make_parse_error_input("not json at all", "expected value at line 1");
        assert!(bad_input.get("__parse_error").is_some());
    }

    // ── Stream assembly tests ───────────────────────────────────────────

    #[test]
    fn test_assemble_stream_text_response() {
        let resp = assemble_oai_stream_response(
            "gen-abc".to_string(),
            "Hello world".to_string(),
            String::new(),
            vec![],
            std::collections::BTreeMap::new(),
            Some("stop".to_string()),
            Some(OaiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        )
        .unwrap();

        assert_eq!(resp.id, "gen-abc");
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "Hello world"));
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn test_assemble_stream_tool_call_response() {
        let mut tool_calls = std::collections::BTreeMap::new();
        tool_calls.insert(
            0,
            (
                "call_abc".to_string(),
                "bash".to_string(),
                r#"{"command":"ls -la"}"#.to_string(),
            ),
        );

        let resp = assemble_oai_stream_response(
            "gen-xyz".to_string(),
            String::new(),
            String::new(),
            vec![],
            tool_calls,
            Some("tool_calls".to_string()),
            None,
        )
        .unwrap();

        assert_eq!(resp.content.len(), 1);
        assert!(matches!(
            &resp.content[0],
            ContentBlock::ToolUse { id, name, input }
            if id == "call_abc" && name == "bash" && input.get("command").is_some()
        ));
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn test_assemble_stream_text_with_multiple_tool_calls() {
        let mut tool_calls = std::collections::BTreeMap::new();
        tool_calls.insert(
            0,
            (
                "call_1".to_string(),
                "read_file".to_string(),
                r#"{"path":"/tmp/x"}"#.to_string(),
            ),
        );
        tool_calls.insert(
            1,
            (
                "call_2".to_string(),
                "bash".to_string(),
                r#"{"command":"echo hi"}"#.to_string(),
            ),
        );

        let resp = assemble_oai_stream_response(
            "gen-multi".to_string(),
            "Let me check.".to_string(),
            String::new(),
            vec![],
            tool_calls,
            Some("tool_calls".to_string()),
            Some(OaiUsage {
                prompt_tokens: 50,
                completion_tokens: 30,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        )
        .unwrap();

        assert_eq!(resp.content.len(), 3);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "Let me check."));
        assert!(
            matches!(&resp.content[1], ContentBlock::ToolUse { name, .. } if name == "read_file")
        );
        assert!(matches!(&resp.content[2], ContentBlock::ToolUse { name, .. } if name == "bash"));
    }

    #[test]
    fn test_assemble_stream_empty_response() {
        let resp = assemble_oai_stream_response(
            "gen-empty".to_string(),
            String::new(),
            String::new(),
            vec![],
            std::collections::BTreeMap::new(),
            None,
            None,
        )
        .unwrap();

        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text.is_empty()));
        assert_eq!(resp.stop_reason, None);
        assert_eq!(resp.usage.input_tokens, 0);
    }

    // ── Stream chunk deserialization tests ───────────────────────────────

    #[test]
    fn test_stream_chunk_deserialization_text() {
        let json = r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk: OaiStreamChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.id, "chatcmpl-1");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hello"));
        assert!(chunk.choices[0].finish_reason.is_none());
    }

    #[test]
    fn test_stream_chunk_deserialization_tool_call_start() {
        let json = r#"{"id":"chatcmpl-2","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_xyz","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}"#;
        let chunk: OaiStreamChunk = serde_json::from_str(json).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_xyz"));
        assert_eq!(tc.function.as_ref().unwrap().name.as_deref(), Some("bash"));
    }

    #[test]
    fn test_stream_chunk_deserialization_tool_call_partial_args() {
        let json = r#"{"id":"chatcmpl-2","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"com"}}]},"finish_reason":null}]}"#;
        let chunk: OaiStreamChunk = serde_json::from_str(json).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert!(tc.id.is_none());
        assert_eq!(
            tc.function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"com")
        );
    }

    #[test]
    fn test_stream_chunk_deserialization_finish_with_usage() {
        let json = r#"{"id":"chatcmpl-2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        let chunk: OaiStreamChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(chunk.usage.as_ref().unwrap().prompt_tokens, 10);
        assert_eq!(chunk.usage.as_ref().unwrap().completion_tokens, 5);
    }

    #[test]
    fn test_stream_chunk_openrouter_gen_prefix() {
        let json = r#"{"id":"gen-abc123","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let chunk: OaiStreamChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.id, "gen-abc123");
    }

    #[test]
    fn test_tool_call_argument_accumulation() {
        let mut tool_calls: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();

        let entry = tool_calls
            .entry(0)
            .or_insert_with(|| (String::new(), String::new(), String::new()));
        entry.0 = "call_123".to_string();
        entry.1 = "bash".to_string();

        for partial in [r#"{"comm"#, r#"and":"#, r#""ls -la"}"#] {
            tool_calls.get_mut(&0).unwrap().2.push_str(partial);
        }

        let (id, name, args) = tool_calls.get(&0).unwrap();
        assert_eq!(id, "call_123");
        assert_eq!(name, "bash");
        assert_eq!(args, r#"{"command":"ls -la"}"#);

        let input: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(input.get("command").unwrap().as_str().unwrap(), "ls -la");
    }

    #[test]
    fn test_stream_chunk_empty_delta() {
        let json = r#"{"id":"chatcmpl-3","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#;
        let chunk: OaiStreamChunk = serde_json::from_str(json).unwrap();
        assert!(chunk.choices[0].delta.content.is_none());
        assert!(chunk.choices[0].delta.tool_calls.is_none());
    }

    #[test]
    fn test_stream_chunk_with_role_delta() {
        let json = r#"{"id":"chatcmpl-4","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#;
        let chunk: OaiStreamChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.choices[0].delta.role.as_deref(), Some("assistant"));
    }

    // ── Cache tracking tests ────────────────────────────────────────────

    #[test]
    fn test_translate_response_with_cache_fields() {
        let oai = OaiResponse {
            id: "chatcmpl-cache".to_string(),
            choices: vec![OaiChoice {
                message: OaiResponseMessage {
                    role: "assistant".to_string(),
                    content: Some("cached response".to_string()),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_details: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OaiUsage {
                prompt_tokens: 100,
                completion_tokens: 20,
                completion_tokens_details: None,
                prompt_tokens_details: Some(OaiPromptTokenDetails {
                    cached_tokens: Some(80),
                    cache_write_tokens: Some(15),
                    cache_discount: Some(0.5),
                }),
            }),
        };

        let resp = OpenAiClient::translate_response(oai).unwrap();
        assert_eq!(resp.usage.input_tokens, 100);
        assert_eq!(resp.usage.output_tokens, 20);
        assert_eq!(resp.usage.cache_read_input_tokens, Some(80));
        assert_eq!(resp.usage.cache_creation_input_tokens, Some(15));
    }

    #[test]
    fn test_assemble_stream_response_with_cache_fields() {
        let resp = assemble_oai_stream_response(
            "gen-cache".to_string(),
            "streamed cached".to_string(),
            String::new(),
            vec![],
            std::collections::BTreeMap::new(),
            Some("stop".to_string()),
            Some(OaiUsage {
                prompt_tokens: 200,
                completion_tokens: 40,
                completion_tokens_details: None,
                prompt_tokens_details: Some(OaiPromptTokenDetails {
                    cached_tokens: Some(150),
                    cache_write_tokens: Some(30),
                    cache_discount: None,
                }),
            }),
        )
        .unwrap();

        assert_eq!(resp.usage.input_tokens, 200);
        assert_eq!(resp.usage.output_tokens, 40);
        assert_eq!(resp.usage.cache_read_input_tokens, Some(150));
        assert_eq!(resp.usage.cache_creation_input_tokens, Some(30));
    }

    #[test]
    fn test_oai_usage_deserialization_with_prompt_tokens_details() {
        let json = r#"{
            "prompt_tokens": 100,
            "completion_tokens": 25,
            "prompt_tokens_details": {
                "cached_tokens": 60,
                "cache_write_tokens": 10,
                "cache_discount": 0.75
            }
        }"#;
        let usage: OaiUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 25);
        let details = usage.prompt_tokens_details.unwrap();
        assert_eq!(details.cached_tokens, Some(60));
        assert_eq!(details.cache_write_tokens, Some(10));
        assert_eq!(details.cache_discount, Some(0.75));
    }

    #[test]
    fn test_oai_usage_deserialization_without_prompt_tokens_details() {
        let json = r#"{"prompt_tokens": 50, "completion_tokens": 10}"#;
        let usage: OaiUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.prompt_tokens, 50);
        assert!(usage.prompt_tokens_details.is_none());
    }

    // ── cache_control request tests ────────────────────────────────────

    #[test]
    fn test_openrouter_request_includes_cache_control() {
        let client = OpenAiClient::new("test-key".into(), "anthropic/claude-sonnet-4-6", None)
            .unwrap()
            .with_provider_hint("openrouter");
        let cc = client.cache_control_value();
        assert!(cc.is_some());
        let val = cc.unwrap();
        assert_eq!(val["type"], "ephemeral");
    }

    #[test]
    fn test_non_openrouter_request_no_cache_control() {
        let client = OpenAiClient::new("test-key".into(), "gpt-4o", None)
            .unwrap()
            .with_provider_hint("openai");
        assert!(client.cache_control_value().is_none());
    }

    #[test]
    fn test_cache_control_serialized_in_request() {
        let request = OaiRequest {
            model: "anthropic/claude-sonnet-4-6".to_string(),
            messages: vec![],
            max_tokens: Some(1024),
            tools: vec![],
            tool_choice: None,
            stream: false,
            stream_options: None,
            cache_control: Some(serde_json::json!({"type": "ephemeral"})),
            reasoning: None,
            include_reasoning: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("cache_control"));
        assert!(json.contains("ephemeral"));
    }

    #[test]
    fn test_cache_control_omitted_when_none() {
        let request = OaiRequest {
            model: "gpt-4o".to_string(),
            messages: vec![],
            max_tokens: Some(1024),
            tools: vec![],
            tool_choice: None,
            stream: false,
            stream_options: None,
            cache_control: None,
            reasoning: None,
            include_reasoning: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("cache_control"));
    }

    // ── from_endpoint tests ────────────────────────────────────────────

    #[test]
    fn test_from_endpoint_inline_key() {
        let ep = crate::config::EndpointConfig {
            name: "test-openai".to_string(),
            provider: "openai".to_string(),
            url: Some("https://api.openai.com/v1".to_string()),
            model: None,
            api_key: Some("sk-test-inline".to_string()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let client = OpenAiClient::from_endpoint(&ep, "gpt-4o", None).unwrap();
        assert_eq!(client.model, "gpt-4o");
        assert_eq!(client.base_url, "https://api.openai.com/v1");
        assert_eq!(client.provider_hint.as_deref(), Some("openai"));
    }

    #[test]
    fn test_from_endpoint_key_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("api.key");
        std::fs::write(&key_path, "sk-from-file\n").unwrap();
        let ep = crate::config::EndpointConfig {
            name: "test-or".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let client = OpenAiClient::from_endpoint(&ep, "anthropic/claude-sonnet-4-6", None).unwrap();
        assert_eq!(client.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(client.provider_hint.as_deref(), Some("openrouter"));
        assert!(client.use_streaming); // OpenRouter enables streaming
    }

    #[test]
    fn test_from_endpoint_no_key_errors() {
        // Use "local" provider — no env var fallback, so this should fail gracefully
        // unless "local" has a special case. Actually local doesn't error on from_endpoint
        // since env_var_names_for_provider returns empty. Let's use a custom unknown provider.
        let ep = crate::config::EndpointConfig {
            name: "test-nokey".to_string(),
            provider: "custom".to_string(),
            url: Some("https://example.com/v1".to_string()),
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let result = OpenAiClient::from_endpoint(&ep, "some-model", None);
        assert!(result.is_err());
        let msg = format!("{}", result.err().unwrap());
        assert!(msg.contains("No API key found"));
        assert!(msg.contains("test-nokey"));
    }

    #[test]
    fn test_from_endpoint_default_url_for_provider() {
        let ep = crate::config::EndpointConfig {
            name: "test-or".to_string(),
            provider: "openrouter".to_string(),
            url: None, // Should use default
            model: None,
            api_key: Some("sk-test".to_string()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let client = OpenAiClient::from_endpoint(&ep, "model", None).unwrap();
        assert_eq!(client.base_url, "https://openrouter.ai/api/v1");
    }

    // ── Streaming integration tests ──────────────────────────────────────

    #[test]
    fn test_streaming_request_includes_stream_options() {
        let request = OaiRequest {
            model: "anthropic/claude-sonnet-4-6".to_string(),
            messages: vec![],
            max_tokens: Some(1024),
            tools: vec![],
            tool_choice: None,
            stream: true,
            stream_options: Some(OaiStreamOptions {
                include_usage: true,
            }),
            cache_control: None,
            reasoning: None,
            include_reasoning: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"stream\":true"));
        assert!(json.contains("stream_options"));
        assert!(json.contains("include_usage"));
    }

    #[test]
    fn test_streaming_stream_options_omitted_when_not_streaming() {
        let request = OaiRequest {
            model: "gpt-4o".to_string(),
            messages: vec![],
            max_tokens: Some(1024),
            tools: vec![],
            tool_choice: None,
            stream: false,
            stream_options: None,
            cache_control: None,
            reasoning: None,
            include_reasoning: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"stream\":false"));
        assert!(!json.contains("stream_options"));
    }

    #[test]
    fn test_streaming_full_sse_text_flow() {
        // Simulate a complete SSE text response flow
        let mut buffer = concat!(
            "data: {\"id\":\"gen-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();

        let mut response_id = String::new();
        let mut text = String::new();
        let tool_calls: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut finish = None;
        let mut usage = None;

        while let Some(data) = parse_next_oai_sse_data(&mut buffer) {
            if data == "[DONE]" {
                break;
            }
            let chunk: OaiStreamChunk = serde_json::from_str(&data).unwrap();
            if response_id.is_empty() && !chunk.id.is_empty() {
                response_id = chunk.id;
            }
            if let Some(u) = chunk.usage {
                usage = Some(u);
            }
            for choice in &chunk.choices {
                if let Some(ref t) = choice.delta.content {
                    text.push_str(t);
                }
                if let Some(ref fr) = choice.finish_reason {
                    finish = Some(fr.clone());
                }
            }
        }

        let resp = assemble_oai_stream_response(
            response_id,
            text,
            String::new(),
            Vec::new(),
            tool_calls,
            finish,
            usage,
        )
        .unwrap();
        assert_eq!(resp.id, "gen-1");
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "Hello world"));
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 2);
    }

    #[test]
    fn test_streaming_full_sse_tool_call_flow() {
        // Simulate tool call arriving across multiple SSE chunks
        let mut buffer = concat!(
            "data: {\"id\":\"gen-2\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"com\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"mand\\\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"ls\\\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();

        let mut response_id = String::new();
        let text = String::new();
        let mut tool_calls: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut finish = None;
        let mut usage = None;

        while let Some(data) = parse_next_oai_sse_data(&mut buffer) {
            if data == "[DONE]" {
                break;
            }
            let chunk: OaiStreamChunk = serde_json::from_str(&data).unwrap();
            if response_id.is_empty() && !chunk.id.is_empty() {
                response_id = chunk.id;
            }
            if let Some(u) = chunk.usage {
                usage = Some(u);
            }
            for choice in &chunk.choices {
                if let Some(ref tcs) = choice.delta.tool_calls {
                    for tc in tcs {
                        let entry = tool_calls
                            .entry(tc.index)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(ref id) = tc.id {
                            entry.0 = id.clone();
                        }
                        if let Some(ref func) = tc.function {
                            if let Some(ref name) = func.name {
                                entry.1 = name.clone();
                            }
                            if let Some(ref args) = func.arguments {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
                if let Some(ref fr) = choice.finish_reason {
                    finish = Some(fr.clone());
                }
            }
        }

        let resp = assemble_oai_stream_response(
            response_id,
            text,
            String::new(),
            Vec::new(),
            tool_calls,
            finish,
            usage,
        )
        .unwrap();
        assert_eq!(resp.id, "gen-2");
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(
            &resp.content[0],
            ContentBlock::ToolUse { id, name, input }
            if id == "call_abc" && name == "bash" && input["command"] == "ls"
        ));
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(resp.usage.input_tokens, 20);
        assert_eq!(resp.usage.output_tokens, 15);
    }

    #[test]
    fn test_streaming_multiple_tool_calls_accumulation() {
        // Two tool calls arriving interleaved in the stream
        let mut buffer = concat!(
            "data: {\"id\":\"gen-3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Running checks.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-3\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-3\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-3\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-3\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"/tmp/x\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-3\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();

        let mut response_id = String::new();
        let mut text = String::new();
        let mut tool_calls: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut finish = None;
        let mut usage = None;

        while let Some(data) = parse_next_oai_sse_data(&mut buffer) {
            if data == "[DONE]" {
                break;
            }
            let chunk: OaiStreamChunk = serde_json::from_str(&data).unwrap();
            if response_id.is_empty() && !chunk.id.is_empty() {
                response_id = chunk.id;
            }
            if let Some(u) = chunk.usage {
                usage = Some(u);
            }
            for choice in &chunk.choices {
                if let Some(ref t) = choice.delta.content {
                    text.push_str(t);
                }
                if let Some(ref tcs) = choice.delta.tool_calls {
                    for tc in tcs {
                        let entry = tool_calls
                            .entry(tc.index)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(ref id) = tc.id {
                            entry.0 = id.clone();
                        }
                        if let Some(ref func) = tc.function {
                            if let Some(ref name) = func.name {
                                entry.1 = name.clone();
                            }
                            if let Some(ref args) = func.arguments {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
                if let Some(ref fr) = choice.finish_reason {
                    finish = Some(fr.clone());
                }
            }
        }

        let resp = assemble_oai_stream_response(
            response_id,
            text,
            String::new(),
            Vec::new(),
            tool_calls,
            finish,
            usage,
        )
        .unwrap();
        assert_eq!(resp.content.len(), 3); // text + 2 tool calls
        assert!(
            matches!(&resp.content[0], ContentBlock::Text { text } if text == "Running checks.")
        );
        assert!(
            matches!(&resp.content[1], ContentBlock::ToolUse { id, name, .. } if id == "call_1" && name == "bash")
        );
        assert!(
            matches!(&resp.content[2], ContentBlock::ToolUse { id, name, input } if id == "call_2" && name == "read_file" && input["path"] == "/tmp/x")
        );
    }

    #[test]
    fn test_streaming_malformed_chunk_skipped() {
        // One good chunk, one malformed, one good — malformed should be skipped
        let mut buffer = concat!(
            "data: {\"id\":\"gen-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"A\"},\"finish_reason\":null}]}\n\n",
            "data: {not valid json}\n\n",
            "data: {\"id\":\"gen-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"B\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"gen-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string();

        let mut text = String::new();
        let mut chunks_parsed = 0u32;
        let mut malformed = 0u32;

        while let Some(data) = parse_next_oai_sse_data(&mut buffer) {
            if data == "[DONE]" {
                break;
            }
            match serde_json::from_str::<OaiStreamChunk>(&data) {
                Ok(chunk) => {
                    chunks_parsed += 1;
                    for choice in &chunk.choices {
                        if let Some(ref t) = choice.delta.content {
                            text.push_str(t);
                        }
                    }
                }
                Err(_) => {
                    malformed += 1;
                }
            }
        }

        assert_eq!(text, "AB");
        assert_eq!(chunks_parsed, 3); // A, B, finish
        assert_eq!(malformed, 1);
    }

    #[test]
    fn test_streaming_no_usage_without_stream_options() {
        // When the API doesn't return usage (stream_options not set), usage should default
        let resp = assemble_oai_stream_response(
            "gen-nousage".to_string(),
            "Hello".to_string(),
            String::new(),
            vec![],
            std::collections::BTreeMap::new(),
            Some("stop".to_string()),
            None,
        )
        .unwrap();

        assert_eq!(resp.usage.input_tokens, 0);
        assert_eq!(resp.usage.output_tokens, 0);
    }

    // ── tool_choice serialization tests ──────────────────────────────────

    #[test]
    fn test_tool_choice_serialized_when_present() {
        let request = OaiRequest {
            model: "test".to_string(),
            messages: vec![],
            max_tokens: Some(1024),
            tools: vec![OaiToolDef {
                tool_type: "function".to_string(),
                function: OaiFunctionDef {
                    name: "bash".to_string(),
                    description: "Run a command".to_string(),
                    parameters: json!({}),
                },
            }],
            tool_choice: Some("auto".to_string()),
            stream: false,
            stream_options: None,
            cache_control: None,
            reasoning: None,
            include_reasoning: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains(r#""tool_choice":"auto""#));
    }

    #[test]
    fn test_tool_choice_omitted_when_none() {
        let request = OaiRequest {
            model: "test".to_string(),
            messages: vec![],
            max_tokens: Some(1024),
            tools: vec![],
            tool_choice: None,
            stream: false,
            stream_options: None,
            cache_control: None,
            reasoning: None,
            include_reasoning: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("tool_choice"));
    }

    // ── Text-based tool call extraction tests ────────────────────────────

    #[test]
    fn test_extract_xml_tool_call() {
        let text = r#"I'll run this command for you.
<tool_call>
{"name": "bash", "arguments": {"command": "ls -la"}}
</tool_call>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert!(
            matches!(&calls[0], ContentBlock::ToolUse { name, input, .. }
                if name == "bash" && input["command"] == "ls -la")
        );
        assert_eq!(remaining, "I'll run this command for you.");
    }

    #[test]
    fn test_extract_multiple_xml_tool_calls() {
        let text = r#"<tool_call>{"name": "bash", "arguments": {"command": "ls"}}</tool_call>
<tool_call>{"name": "bash", "arguments": {"command": "pwd"}}</tool_call>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 2);
        assert!(matches!(&calls[0], ContentBlock::ToolUse { name, .. } if name == "bash"));
        assert!(matches!(&calls[1], ContentBlock::ToolUse { name, .. } if name == "bash"));
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_function_tag_tool_call() {
        let text = r#"<function=bash>{"command": "echo hello"}</function>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert!(
            matches!(&calls[0], ContentBlock::ToolUse { name, input, .. }
                if name == "bash" && input["command"] == "echo hello")
        );
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_no_tool_calls_returns_original() {
        let text = "This is just plain text with no tool calls.";
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert!(calls.is_empty());
        assert_eq!(remaining, text);
    }

    #[test]
    fn test_extract_tool_calls_hermes_format() {
        // Hermes/LLaMA-3 style with <|plugin|>...<|/plugin|> tags
        let text = r#"Let me run that command.
<|plugin|>{"name": "bash", "arguments": {"command": "ls -la"}}<|/plugin|>
Done."#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert!(
            matches!(&calls[0], ContentBlock::ToolUse { name, input, .. }
                if name == "bash" && input["command"] == "ls -la")
        );
        assert_eq!(remaining.trim(), "Let me run that command.\nDone.");
    }

    #[test]
    fn test_extract_tool_calls_plugin_format() {
        // Qwen3/Hermes style <|plugin|> format
        let text =
            r#"<|plugin|>{"name": "read_file", "arguments": {"path": "/etc/hosts"}}<|/plugin|>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert!(
            matches!(&calls[0], ContentBlock::ToolUse { name, input, .. }
                if name == "read_file" && input["path"] == "/etc/hosts")
        );
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_multiple_plugin_format_calls() {
        // Multiple tool calls in <|plugin|> format
        let text = r#"<|plugin|>{"name": "bash", "arguments": {"command": "pwd"}}<|/plugin|>
<|plugin|>{"name": "bash", "arguments": {"command": "ls"}}<|/plugin|>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 2);
        assert!(matches!(&calls[0], ContentBlock::ToolUse { name, .. } if name == "bash"));
        assert!(matches!(&calls[1], ContentBlock::ToolUse { name, .. } if name == "bash"));
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_tool_calls_mixed_formats() {
        // Mix of <tool_call> and <|plugin|> formats
        let text = r#"<tool_call>{"name": "bash", "arguments": {"command": "echo a"}}</tool_call>
<|plugin|>{"name": "bash", "arguments": {"command": "echo b"}}<|/plugin|>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 2);
        assert!(matches!(&calls[0], ContentBlock::ToolUse { name, .. } if name == "bash"));
        assert!(matches!(&calls[1], ContentBlock::ToolUse { name, .. } if name == "bash"));
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_tool_call_with_parameters_key() {
        let text = r#"<tool_call>{"name": "read_file", "parameters": {"path": "/tmp/test.txt"}}</tool_call>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert!(
            matches!(&calls[0], ContentBlock::ToolUse { name, input, .. }
                if name == "read_file" && input["path"] == "/tmp/test.txt")
        );
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_tool_call_with_string_arguments() {
        let text =
            r#"<tool_call>{"name": "bash", "arguments": "{\"command\": \"ls\"}"}</tool_call>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert!(
            matches!(&calls[0], ContentBlock::ToolUse { name, input, .. }
                if name == "bash" && input["command"] == "ls")
        );
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_minimax_tool_call_format() {
        // Regression test: minimax model uses <minimax:tool_call>...</minimax:tool_call> format.
        // The ':tool_call>' in the OPENING tag must not be matched as the closing tag,
        // which previously caused a slice bounds panic (begin > end).
        let text = r#"<minimax:tool_call>{"name": "bash", "arguments": {"command": "ls"}}</minimax:tool_call>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert!(
            matches!(&calls[0], ContentBlock::ToolUse { name, input, .. }
                if name == "bash" && input["command"] == "ls")
        );
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_minimax_tool_call_no_panic_on_invoke_text() {
        // Regression test: text containing <invoke> XML format (used by some models)
        // with ':tool_call>' absent should not panic. Pattern 3 should not match.
        let text = r#"<invoke name="wg_msg_read">
<parameter name="task_id">some-task</parameter>
</invoke>"#;
        // This should not panic — extract_tool_calls_from_text must handle
        // text that does not contain ':tool_call>' gracefully.
        let (_remaining, calls) = extract_tool_calls_from_text(text);
        // <invoke> format is not a recognized Pattern 3 variant, so 0 calls is fine.
        // The important thing is no panic.
        let _ = calls;
    }

    #[test]
    fn test_translate_response_extracts_text_tool_calls() {
        // Simulate a model that outputs tool calls as text instead of structured tool_calls
        let oai = OaiResponse {
            id: "chatcmpl-text-tools".to_string(),
            choices: vec![OaiChoice {
                message: OaiResponseMessage {
                    role: "assistant".to_string(),
                    content: Some(
                        "Let me check.\n<tool_call>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"wg list\"}}\n</tool_call>"
                            .to_string(),
                    ),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_details: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OaiUsage {
                prompt_tokens: 50,
                completion_tokens: 30,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        };

        let resp = OpenAiClient::translate_response(oai).unwrap();
        // Should have extracted the tool call AND overridden stop_reason
        assert!(
            resp.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "bash"))
        );
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn test_translate_response_no_extraction_when_structured_tools_present() {
        // When structured tool calls are present, text should NOT be parsed for additional tools
        let oai = OaiResponse {
            id: "chatcmpl-structured".to_string(),
            choices: vec![OaiChoice {
                message: OaiResponseMessage {
                    role: "assistant".to_string(),
                    content: Some("Running command.".to_string()),
                    reasoning: None,
                    reasoning_details: None,
                    tool_calls: Some(vec![OaiToolCall {
                        id: "call_real".to_string(),
                        call_type: "function".to_string(),
                        function: OaiToolCallFunction {
                            name: "bash".to_string(),
                            arguments: r#"{"command":"ls"}"#.to_string(),
                        },
                    }]),
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };

        let resp = OpenAiClient::translate_response(oai).unwrap();
        // Should have 2 blocks: text + structured tool call
        assert_eq!(resp.content.len(), 2);
        assert!(
            matches!(&resp.content[0], ContentBlock::Text { text } if text == "Running command.")
        );
        assert!(matches!(&resp.content[1], ContentBlock::ToolUse { id, .. } if id == "call_real"));
    }

    #[test]
    fn test_assemble_stream_extracts_text_tool_calls() {
        // Streaming response where model outputs tool calls in text
        let resp = assemble_oai_stream_response(
            "gen-text-tools".to_string(),
            "<tool_call>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"wg status\"}}\n</tool_call>".to_string(),
            String::new(),
            vec![],
            std::collections::BTreeMap::new(), // no structured tool calls
            Some("stop".to_string()),
            None,
        )
        .unwrap();

        assert!(
            resp.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "bash"))
        );
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn test_stop_reason_overridden_for_text_extracted_tools() {
        // When text-based tools are extracted but finish_reason was "stop",
        // the stop_reason should be overridden to ToolUse
        let oai = OaiResponse {
            id: "chatcmpl-override".to_string(),
            choices: vec![OaiChoice {
                message: OaiResponseMessage {
                    role: "assistant".to_string(),
                    content: Some(
                        "<tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"echo hi\"}}</tool_call>"
                            .to_string(),
                    ),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_details: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };

        let resp = OpenAiClient::translate_response(oai).unwrap();
        assert_eq!(
            resp.stop_reason,
            Some(StopReason::ToolUse),
            "stop_reason should be overridden to ToolUse when text-based tool calls are extracted"
        );
    }

    #[test]
    fn test_extract_invalid_json_ignored() {
        let text = "<tool_call>\nnot valid json at all\n</tool_call>";
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert!(calls.is_empty());
        // Invalid JSON tag is still removed from text
        assert!(remaining.is_empty() || !remaining.contains("tool_call"));
    }

    #[test]
    fn test_extract_pipe_delimited_tool_call() {
        let text = r#"<|tool_call|>
{"name": "bash", "arguments": {"command": "pwd"}}
<|/tool_call|>"#;
        let (remaining, calls) = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert!(
            matches!(&calls[0], ContentBlock::ToolUse { name, input, .. }
                if name == "bash" && input["command"] == "pwd")
        );
        assert!(remaining.is_empty());
    }

    // ── Auto-routing & model validation tests ───────────────────────────

    #[test]
    fn test_openrouter_auto_model_constant() {
        assert_eq!(OPENROUTER_AUTO_MODEL, "openrouter/auto");
    }

    #[test]
    fn test_validate_openrouter_auto_always_valid() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = validate_openrouter_model("openrouter/auto", dir.path());
        assert!(result.was_valid);
        assert_eq!(result.model, "openrouter/auto");
        assert!(result.suggestions.is_empty());
        assert!(result.warning.is_none());
    }

    #[test]
    fn test_validate_no_cache_passes_through() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = validate_openrouter_model("some-unknown/model", dir.path());
        assert!(result.was_valid);
        assert_eq!(result.model, "some-unknown/model");
        assert!(result.warning.is_none());
    }

    #[test]
    fn test_validate_model_found_in_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "anthropic/claude-sonnet-4-6", "name": "Sonnet", "description": ""},
                {"id": "openai/gpt-4o", "name": "GPT-4o", "description": ""},
            ]
        });
        std::fs::write(dir.path().join("model_cache.json"), cache.to_string()).unwrap();

        let result = validate_openrouter_model("anthropic/claude-sonnet-4-6", dir.path());
        assert!(result.was_valid);
        assert_eq!(result.model, "anthropic/claude-sonnet-4-6");
        assert!(result.warning.is_none());
    }

    #[test]
    fn test_validate_invalid_model_suggests_without_fallback() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "anthropic/claude-sonnet-4-6", "name": "Sonnet"},
                {"id": "anthropic/claude-opus-4-6", "name": "Opus"},
                {"id": "openai/gpt-4o", "name": "GPT-4o"},
                {"id": "deepseek/deepseek-r1", "name": "R1"},
                {"id": "meta-llama/llama-4-maverick", "name": "Llama 4"},
            ]
        });
        std::fs::write(dir.path().join("model_cache.json"), cache.to_string()).unwrap();

        let result = validate_openrouter_model("anthropic/claude-sonet-4-6", dir.path());
        assert!(!result.was_valid);
        // Should return the original (invalid) model, NOT openrouter/auto
        assert_eq!(result.model, "anthropic/claude-sonet-4-6");
        assert!(!result.suggestions.is_empty());
        assert!(
            result
                .suggestions
                .contains(&"anthropic/claude-sonnet-4-6".to_string()),
            "suggestions should include close match, got: {:?}",
            result.suggestions
        );
        let warning = result.warning.unwrap();
        assert!(warning.contains("not found"));
        // Should NOT mention falling back to openrouter/auto
        assert!(
            !warning.contains("Falling back"),
            "warning should not mention fallback, got: {}",
            warning
        );
        assert!(
            !warning.contains(OPENROUTER_AUTO_MODEL),
            "warning should not mention openrouter/auto, got: {}",
            warning
        );
        assert!(
            warning.contains("wg models search"),
            "warning should suggest `wg models search`, got: {}",
            warning
        );
        assert!(
            warning.contains("wg models list"),
            "warning should suggest `wg models list`, got: {}",
            warning
        );
    }

    #[test]
    fn test_validate_strips_provider_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "minimax/minimax-m2.7", "name": "Minimax M2.7"},
                {"id": "anthropic/claude-sonnet-4-6", "name": "Sonnet"},
            ]
        });
        std::fs::write(dir.path().join("model_cache.json"), cache.to_string()).unwrap();

        // With provider prefix, should strip and find the model
        let result = validate_openrouter_model("openrouter:minimax/minimax-m2.7", dir.path());
        assert!(result.was_valid);
        assert_eq!(result.model, "minimax/minimax-m2.7");
        assert!(result.warning.is_none());
    }

    #[test]
    fn test_validate_strips_prefix_not_found_no_fallback() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "minimax/minimax-m2.7", "name": "Minimax M2.7"},
                {"id": "minimax/minimax-m2.5", "name": "Minimax M2.5"},
            ]
        });
        std::fs::write(dir.path().join("model_cache.json"), cache.to_string()).unwrap();

        // With provider prefix but model not in cache — should strip, fail, no fallback
        let result = validate_openrouter_model("openrouter:minimax/minimax-m9.9", dir.path());
        assert!(!result.was_valid);
        // Should return the stripped model, NOT openrouter/auto
        assert_eq!(result.model, "minimax/minimax-m9.9");
        assert!(!result.suggestions.is_empty());
        assert!(
            !result
                .warning
                .as_deref()
                .unwrap_or("")
                .contains(OPENROUTER_AUTO_MODEL),
            "should not fall back to openrouter/auto"
        );
    }

    #[test]
    fn test_validate_suggestions_limited_to_3() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = serde_json::json!({
            "fetched_at": "2026-03-25T12:00:00Z",
            "models": [
                {"id": "a/model-1"},
                {"id": "a/model-2"},
                {"id": "a/model-3"},
                {"id": "a/model-4"},
                {"id": "a/model-5"},
            ]
        });
        std::fs::write(dir.path().join("model_cache.json"), cache.to_string()).unwrap();

        let result = validate_openrouter_model("a/model-x", dir.path());
        assert!(!result.was_valid);
        assert!(result.suggestions.len() <= 3);
    }

    #[test]
    fn test_levenshtein_distance_basic() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("abc", "abd"), 1);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("food", "foo"), 1);
    }

    #[test]
    fn test_find_closest_models() {
        let candidates = vec![
            "anthropic/claude-sonnet-4-6",
            "anthropic/claude-opus-4-6",
            "openai/gpt-4o",
            "deepseek/deepseek-r1",
        ];
        let closest = find_closest_models("anthropic/claude-sonet-4-6", &candidates, 3);
        assert!(!closest.is_empty());
        assert_eq!(closest[0], "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn test_validate_corrupt_cache_passes_through() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("model_cache.json"), "not json").unwrap();

        let result = validate_openrouter_model("some/model", dir.path());
        assert!(result.was_valid);
        assert_eq!(result.model, "some/model");
    }

    /// HARD GATE: HTTP 400 must NOT retry. Documents the policy from
    /// `wg-nex-resume-311` (autohaiku 311-message resume → endpoint
    /// flooded with 400s because the upper layers had no `is_retryable`
    /// classification). These three predicates are what every retry
    /// site reads to decide whether to back off vs. abort.
    #[test]
    fn test_nex_400_no_retry_loop() {
        // 400 (and 422) are deterministic client errors — retrying the
        // exact same payload re-produces the exact same error, just at
        // higher network cost.
        assert!(!is_retryable(400), "400 must be classified non-retryable");
        assert!(!is_retryable(422), "422 must be classified non-retryable");
        assert_eq!(max_retries_for_status(400), 0);
        assert_eq!(max_retries_for_status(422), 0);
        // Exposed predicate — call sites use this directly.
        assert!(!is_retryable_status(400));

        // The error type carries the status verbatim so the upper
        // layers can match on it.
        let err = oai_api_error(400, r#"{"error":{"message":"Bad Request"}}"#);
        let api_err = err.downcast_ref::<ApiError>().expect("ApiError");
        assert_eq!(api_err.status, 400);
    }

    /// HARD GATE: HTTP 429 (rate limit) MUST be retryable, with backoff.
    /// Task description requires "max 3 retries"; existing policy is 5
    /// which exceeds that minimum. Plus `Retry-After` parsing must
    /// honour the header.
    #[test]
    fn test_nex_429_backoff() {
        assert!(is_retryable(429));
        assert!(
            max_retries_for_status(429) >= 3,
            "429 must allow at least 3 retries, got {}",
            max_retries_for_status(429),
        );
        // Exponential backoff converges quickly under the 60s ceiling.
        let mut backoff = 1000u64;
        let mut waits = Vec::new();
        for _ in 0..max_retries_for_status(429) {
            waits.push(backoff);
            backoff = (backoff * 2).min(60_000);
        }
        assert!(
            waits.windows(2).all(|w| w[1] >= w[0]),
            "backoff must be monotonically non-decreasing, got {:?}",
            waits,
        );
        // Retry-After in response body wins over computed backoff.
        let body = r#"{"error":{"message":"rate limited","metadata":{"retry_after":2.5}}}"#;
        assert_eq!(parse_retry_after_oai(body), Some(2500));
    }

    #[test]
    fn test_error_recovery_429_backoff() {
        assert!(is_retryable(429));
        assert_eq!(max_retries_for_status(429), 5);
        let mut backoff = 1000u64;
        for _ in 0..5 {
            backoff = (backoff * 2).min(60_000);
        }
        assert!(backoff <= 60_000);
        let body = r#"{"error":{"message":"rate limited","metadata":{"retry_after":2.5}}}"#;
        assert_eq!(parse_retry_after_oai(body), Some(2500));
        let err = oai_api_error(429, r#"{"error":{"message":"Rate limit exceeded"}}"#);
        let api_err = err.downcast_ref::<ApiError>().expect("ApiError");
        assert_eq!(api_err.status, 429);
    }

    #[test]
    fn test_error_recovery_500_retry() {
        assert!(is_retryable(500));
        assert!(is_retryable(502));
        assert!(is_retryable(503));
        assert_eq!(max_retries_for_status(500), 3);
        assert_eq!(max_retries_for_status(502), 3);
        assert_eq!(max_retries_for_status(503), 3);
        let err = oai_api_error(500, r#"{"error":{"message":"Internal server error"}}"#);
        let api_err = err.downcast_ref::<ApiError>().expect("ApiError");
        assert_eq!(api_err.status, 500);
        assert_eq!(max_retries_for_status(400), 0);
    }

    #[test]
    fn test_error_recovery_401_immediate_fail() {
        assert!(!is_retryable(401));
        assert_eq!(max_retries_for_status(401), 0);
        let err = oai_api_error(401, r#"{"error":{"message":"Invalid API key"}}"#);
        let api_err = err.downcast_ref::<ApiError>().expect("ApiError");
        assert_eq!(api_err.status, 401);
        let display = format!("{}", api_err);
        assert!(display.contains("Authentication failed"), "{}", display);
        assert!(display.contains("API key"), "{}", display);
        assert!(!is_retryable(403));
        let err403 = oai_api_error(403, r#"{"error":{"message":"Forbidden"}}"#);
        let api403 = err403.downcast_ref::<ApiError>().expect("ApiError");
        let d403 = format!("{}", api403);
        assert!(d403.contains("Access denied"), "{}", d403);
    }

    // ── JSON recovery tests ─────────────────────────────────────────────

    #[test]
    fn test_json_recovery_markdown_wrapped() {
        let raw = "```json\n{\"command\": \"ls -la\"}\n```";
        let result = try_recover_json(raw);
        assert!(result.is_ok(), "should recover markdown-wrapped JSON");
        let val = result.unwrap();
        assert_eq!(val.get("command").and_then(|v| v.as_str()), Some("ls -la"));
    }

    #[test]
    fn test_json_recovery_markdown_no_lang_tag() {
        let raw = "```\n{\"key\": \"value\"}\n```";
        let result = try_recover_json(raw);
        assert!(result.is_ok(), "should recover markdown without lang tag");
        assert_eq!(
            result.unwrap().get("key").and_then(|v| v.as_str()),
            Some("value")
        );
    }

    #[test]
    fn test_json_recovery_markdown_no_closing_fence() {
        let raw = "```json\n{\"key\": \"value\"}";
        let result = try_recover_json(raw);
        assert!(
            result.is_ok(),
            "should recover markdown without closing fence"
        );
    }

    #[test]
    fn test_json_recovery_embedded_object() {
        let raw = "Here is the tool call: {\"command\": \"echo hello\"} and some trailing text";
        let result = try_recover_json(raw);
        assert!(result.is_ok(), "should extract embedded JSON object");
        assert_eq!(
            result.unwrap().get("command").and_then(|v| v.as_str()),
            Some("echo hello")
        );
    }

    #[test]
    fn test_json_recovery_truncated_simple() {
        // Truncated JSON missing closing brace
        let raw = r#"{"command": "ls""#;
        let result = try_recover_json(raw);
        assert!(
            result.is_ok(),
            "should complete truncated JSON: {:?}",
            result
        );
        assert_eq!(
            result.unwrap().get("command").and_then(|v| v.as_str()),
            Some("ls")
        );
    }

    #[test]
    fn test_json_recovery_truncated_nested() {
        let raw = r#"{"args": ["a", "b""#;
        let result = try_recover_json(raw);
        assert!(result.is_ok(), "should complete nested truncated JSON");
    }

    #[test]
    fn test_json_recovery_truncated_mid_string() {
        // Cut off in the middle of a string value
        let raw = r#"{"path": "/home/user/fi"#;
        let result = try_recover_json(raw);
        assert!(result.is_ok(), "should complete truncated string");
        let val = result.unwrap();
        assert!(val.get("path").is_some());
    }

    #[test]
    fn test_json_recovery_totally_invalid() {
        let raw = "not json at all";
        let result = try_recover_json(raw);
        assert!(result.is_err(), "should fail on non-JSON");
    }

    #[test]
    fn test_json_recovery_empty() {
        assert!(try_recover_json("").is_err());
        assert!(try_recover_json("   ").is_err());
    }

    #[test]
    fn test_json_recovery_valid_json_passthrough() {
        // Valid JSON should work through the first strategy (strip markdown
        // won't apply, embedded object extraction will find it)
        let raw = r#"{"command": "echo test"}"#;
        let result = try_recover_json(raw);
        assert!(result.is_ok());
    }

    #[test]
    fn test_find_balanced_json_basic() {
        assert_eq!(find_balanced_json(r#"{"a":1} extra"#), Some(r#"{"a":1}"#));
    }

    #[test]
    fn test_find_balanced_json_nested() {
        let s = r#"{"a":{"b":1}} tail"#;
        assert_eq!(find_balanced_json(s), Some(r#"{"a":{"b":1}}"#));
    }

    #[test]
    fn test_find_balanced_json_with_string_braces() {
        let s = r#"{"a":"}"} more"#;
        assert_eq!(find_balanced_json(s), Some(r#"{"a":"}"}"#));
    }

    #[test]
    fn test_find_balanced_json_unbalanced() {
        assert_eq!(find_balanced_json(r#"{"a":1"#), None);
    }

    #[test]
    fn test_strip_markdown_json_variants() {
        assert_eq!(strip_markdown_json("```json\n{}\n```"), Some("{}"));
        assert_eq!(strip_markdown_json("```\nfoo\n```"), Some("foo"));
        assert_eq!(strip_markdown_json("no fences"), None);
    }

    #[test]
    fn test_complete_truncated_json_balanced_returns_none() {
        // Already balanced — returns None
        assert!(complete_truncated_json(r#"{"a":1}"#).is_none());
    }

    #[test]
    fn test_complete_truncated_json_no_colon_returns_none() {
        // No key-value separator — not worth trying
        assert!(complete_truncated_json("{abc").is_none());
    }

    // ── Jitter tests ────────────────────────────────────────────────────

    #[test]
    fn test_jittered_backoff_stays_in_range() {
        for base in [100, 500, 1000, 5000, 30_000, 60_000] {
            for _ in 0..20 {
                let result = jittered_backoff(base);
                let lower = base * 3 / 4; // -25%
                let upper = base * 5 / 4; // +25%
                assert!(
                    result >= lower && result <= upper,
                    "jittered_backoff({}) = {} not in [{}, {}]",
                    base,
                    result,
                    lower,
                    upper
                );
            }
        }
    }

    #[test]
    fn test_jittered_backoff_zero_base() {
        // 0 base should return 0 (jitter_range is 0)
        assert_eq!(jittered_backoff(0), 0);
    }

    #[test]
    fn test_jittered_backoff_small_base() {
        // Very small base where jitter_range rounds to 0
        let result = jittered_backoff(3);
        assert_eq!(result, 3);
    }

    // ── Context-too-long detection tests ─────────────────────────────────

    #[test]
    fn test_is_context_too_long_413() {
        let err = oai_api_error(413, r#"{"error":{"message":"Payload too large"}}"#);
        assert!(is_context_too_long(&err));
    }

    #[test]
    fn test_is_context_too_long_400_with_context_keywords() {
        for msg in [
            "This model's maximum context length is 8192 tokens",
            "Request too long",
            "Prompt is too large",
            "Maximum token limit exceeded",
            "context window exceeded",
        ] {
            let body = format!(r#"{{"error":{{"message":"{}"}}}}"#, msg);
            let err = oai_api_error(400, &body);
            assert!(
                is_context_too_long(&err),
                "should detect context-too-long for: {}",
                msg
            );
        }
    }

    #[test]
    fn test_is_context_too_long_400_unrelated() {
        let err = oai_api_error(400, r#"{"error":{"message":"Invalid parameter"}}"#);
        assert!(
            !is_context_too_long(&err),
            "unrelated 400 should not be context-too-long"
        );
    }

    #[test]
    fn test_is_context_too_long_non_api_error() {
        let err = anyhow::anyhow!("some random error");
        assert!(!is_context_too_long(&err));
    }

    // ── Retry-after parsing tests ───────────────────────────────────────

    #[test]
    fn test_parse_retry_after_oai_variants() {
        // Standard format
        let body = r#"{"error":{"message":"rate limited","metadata":{"retry_after":5.0}}}"#;
        assert_eq!(parse_retry_after_oai(body), Some(5000));

        // Fractional seconds
        let body = r#"{"error":{"message":"limited","metadata":{"retry_after":0.5}}}"#;
        assert_eq!(parse_retry_after_oai(body), Some(500));

        // No retry_after
        let body = r#"{"error":{"message":"rate limited"}}"#;
        assert_eq!(parse_retry_after_oai(body), None);

        // Invalid body
        assert_eq!(parse_retry_after_oai("not json"), None);
    }

    // ── Short model name resolution tests ──────────────────────────────

    fn write_test_cache(dir: &std::path::Path) {
        let cache = serde_json::json!({
            "fetched_at": "2026-04-01T00:00:00Z",
            "models": [
                {"id": "minimax/minimax-m2.7", "name": "Minimax M2.7"},
                {"id": "anthropic/claude-sonnet-4-6", "name": "Claude Sonnet 4.6"},
                {"id": "openai/gpt-4o", "name": "GPT-4o"},
                {"id": "deepseek/deepseek-r1", "name": "DeepSeek R1"},
                {"id": "meta-llama/llama-4-maverick", "name": "Llama 4 Maverick"},
            ]
        });
        std::fs::write(dir.join("model_cache.json"), cache.to_string()).unwrap();
    }

    #[test]
    fn test_resolve_short_name_exact_suffix_match() {
        let dir = tempfile::TempDir::new().unwrap();
        write_test_cache(dir.path());

        let result = resolve_short_model_name("minimax-m2.7", dir.path());
        assert_eq!(result.resolved, Some("minimax/minimax-m2.7".to_string()));
        assert!(result.suggestions.is_empty());
    }

    #[test]
    fn test_resolve_short_name_full_id_passthrough() {
        let dir = tempfile::TempDir::new().unwrap();
        write_test_cache(dir.path());

        let result = resolve_short_model_name("minimax/minimax-m2.7", dir.path());
        assert_eq!(result.resolved, Some("minimax/minimax-m2.7".to_string()));
    }

    #[test]
    fn test_resolve_short_name_with_provider_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        write_test_cache(dir.path());

        let result = resolve_short_model_name("openrouter:minimax/minimax-m2.7", dir.path());
        assert_eq!(result.resolved, Some("minimax/minimax-m2.7".to_string()));
    }

    #[test]
    fn test_resolve_short_name_no_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        // No cache file

        let result = resolve_short_model_name("minimax-m2.7", dir.path());
        assert!(result.resolved.is_none());
        assert!(result.suggestions.is_empty());
    }

    #[test]
    fn test_resolve_short_name_no_match() {
        let dir = tempfile::TempDir::new().unwrap();
        write_test_cache(dir.path());

        let result = resolve_short_model_name("nonexistent-model", dir.path());
        assert!(result.resolved.is_none());
        // Should have Levenshtein suggestions
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_resolve_short_name_case_insensitive() {
        let dir = tempfile::TempDir::new().unwrap();
        write_test_cache(dir.path());

        let result = resolve_short_model_name("GPT-4o", dir.path());
        assert_eq!(result.resolved, Some("openai/gpt-4o".to_string()));
    }

    #[test]
    fn test_resolve_short_name_full_id_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        write_test_cache(dir.path());

        let result = resolve_short_model_name("minimax/minimax-m9.9", dir.path());
        assert!(result.resolved.is_none());
        // Should have suggestions
        assert!(!result.suggestions.is_empty());
    }

    #[test]
    fn test_with_context_window_caps_max_tokens() {
        // Default max_tokens is 16384.  A 32k window should cap it to 32768/4 = 8192.
        let client = OpenAiClient::new("key".into(), "model", None)
            .unwrap()
            .with_context_window(32768);
        assert_eq!(client.max_tokens, 8192);
        assert_eq!(client.context_window_tokens, 32768);
    }

    #[test]
    fn test_with_context_window_no_cap_for_large_window() {
        // A 128k window → cap = 32768, which is larger than default 16384 → no cap.
        let client = OpenAiClient::new("key".into(), "model", None)
            .unwrap()
            .with_context_window(128_000);
        assert_eq!(client.max_tokens, DEFAULT_MAX_TOKENS); // unchanged
        assert_eq!(client.context_window_tokens, 128_000);
    }

    #[test]
    fn test_with_context_window_explicit_max_tokens_then_window() {
        // Explicit max_tokens=4096 then a 32k window → cap = 8192, but 4096 < 8192 → no change.
        let client = OpenAiClient::new("key".into(), "model", None)
            .unwrap()
            .with_max_tokens(4096)
            .with_context_window(32768);
        assert_eq!(client.max_tokens, 4096);
    }

    #[test]
    fn test_with_context_window_explicit_max_tokens_capped() {
        // Explicit max_tokens=20000 then a 32k window → cap = 8192, 20000 > 8192 → capped.
        let client = OpenAiClient::new("key".into(), "model", None)
            .unwrap()
            .with_max_tokens(20000)
            .with_context_window(32768);
        assert_eq!(client.max_tokens, 8192);
    }
}
