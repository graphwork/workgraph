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
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use super::client::{
    ContentBlock, Message, MessagesRequest, MessagesResponse, Role, StopReason, ToolDefinition,
    Usage,
};

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
struct OaiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<OaiPromptTokenDetails>,
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

    /// Override max tokens per response.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Override context window size in tokens.
    pub fn with_context_window(mut self, tokens: usize) -> Self {
        self.context_window_tokens = tokens;
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
                                    });
                                }
                                ContentBlock::Text { text } => {
                                    oai_messages.push(OaiMessage {
                                        role: "user".to_string(),
                                        content: Some(text.clone()),
                                        tool_calls: None,
                                        tool_call_id: None,
                                    });
                                }
                                _ => {}
                            }
                        }
                    } else {
                        // Regular text message
                        let text: String = msg
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        oai_messages.push(OaiMessage {
                            role: "user".to_string(),
                            content: Some(text),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                }
                Role::Assistant => {
                    // Collect text and tool_calls from content blocks
                    let mut text_parts = Vec::new();
                    let mut tool_calls = Vec::new();

                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => {
                                text_parts.push(text.clone());
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

        // Add text content if present
        if let Some(text) = choice.message.content
            && !text.is_empty()
        {
            // If there are no structured tool calls, check for text-based tool calls
            if !has_structured_tool_calls {
                let (remaining, extracted) = extract_tool_calls_from_text(&text);
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
                    content_blocks.push(ContentBlock::Text { text });
                }
            } else {
                content_blocks.push(ContentBlock::Text { text });
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
                Usage {
                    input_tokens: u.prompt_tokens,
                    output_tokens: u.completion_tokens,
                    cache_creation_input_tokens: cache_creation,
                    cache_read_input_tokens: cache_read,
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

    /// Send a non-streaming request.
    async fn chat_completion(&self, request: &MessagesRequest) -> Result<MessagesResponse> {
        let tools = Self::translate_tools(&request.tools);
        let tool_choice = if tools.is_empty() {
            None
        } else {
            Some("auto".to_string())
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
        let oai_request = OaiRequest {
            model: request.model.clone(),
            messages: Self::translate_messages(&request.system, &request.messages),
            max_tokens: Some(request.max_tokens),
            tools,
            tool_choice,
            stream: true,
            stream_options: Some(OaiStreamOptions {
                include_usage: true,
            }),
            cache_control: self.cache_control_value(),
        };

        let url = format!("{}/chat/completions", self.base_url);
        let max_retries = 3;
        let mut retry_count = 0;
        let mut backoff_ms = 1000u64;

        loop {
            match self.streaming_attempt(&url, &oai_request).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    if retry_count < max_retries {
                        retry_count += 1;
                        eprintln!(
                            "[openai-client] Streaming error (attempt {}/{}): {}. Retrying in {}ms",
                            retry_count, max_retries, e, backoff_ms
                        );
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
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
        eprintln!(
            "[openai-client] Stream complete: {} chunks, {} text chars, {} tool calls",
            chunk_count,
            text_content.len(),
            tool_calls.len()
        );

        // Assemble the response
        assemble_oai_stream_response(response_id, text_content, tool_calls, finish_reason, usage)
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

        eprintln!(
            "[openai-client] Stream complete: {} chunks, {} text chars, {} tool calls",
            chunk_count,
            text_content.len(),
            tool_calls.len()
        );

        assemble_oai_stream_response(response_id, text_content, tool_calls, finish_reason, usage)
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
        let oai_request = OaiRequest {
            model: request.model.clone(),
            messages: Self::translate_messages(&request.system, &request.messages),
            max_tokens: Some(request.max_tokens),
            tools,
            tool_choice,
            stream: true,
            stream_options: Some(OaiStreamOptions {
                include_usage: true,
            }),
            cache_control: self.cache_control_value(),
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
                    if retry_count < max_retries {
                        retry_count += 1;
                        eprintln!(
                            "[openai-client] Streaming error (attempt {}/{}): {}. Retrying in {}ms",
                            retry_count, max_retries, e, backoff_ms
                        );
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
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
        let max_retries = 5;
        let mut retry_count = 0;
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

                    if is_retryable(status_code) && retry_count < max_retries {
                        retry_count += 1;
                        let wait = parse_retry_after_oai(&body).unwrap_or(backoff_ms);
                        eprintln!(
                            "[openai-client] Retryable error {} (attempt {}/{}), waiting {}ms",
                            status_code, retry_count, max_retries, wait
                        );
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        backoff_ms = (backoff_ms * 2).min(60_000);
                        continue;
                    }

                    return Err(oai_api_error(status_code, &body));
                }
                Err(e) => {
                    if retry_count < max_retries {
                        retry_count += 1;
                        eprintln!(
                            "[openai-client] Network error (attempt {}/{}): {}",
                            retry_count, max_retries, e
                        );
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
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
        self.provider_hint.as_deref().unwrap_or("openai")
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

fn oai_api_error(status: u16, body: &str) -> anyhow::Error {
    if let Ok(err) = serde_json::from_str::<OaiErrorResponse>(body) {
        anyhow!("OpenAI API error {}: {}", status, err.error.message)
    } else {
        anyhow!("OpenAI API error {}: {}", status, truncate(body, 500))
    }
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

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..s.floor_char_boundary(max)]
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

        // Find the matching closing tag
        let close_patterns = ["</tool_call>", "<|/tool_call|>", "<|tool_call_end|>"];
        let close_match = close_patterns.iter().find_map(|pat| {
            remaining[start..]
                .find(pat)
                .map(|offset| (start + offset, pat.len()))
        });
        // Also check for :tool_call> closing (e.g., </minimax:tool_call>)
        let close_match = close_match.or_else(|| {
            remaining[start + 1..].find(":tool_call>").map(|offset| {
                // Walk back to find '</' or '<'
                let tag_start = remaining[start + 1..start + 1 + offset]
                    .rfind('<')
                    .map(|p| start + 1 + p)
                    .unwrap_or(start + 1 + offset.saturating_sub(1));
                (
                    tag_start,
                    offset + ":tool_call>".len() - (tag_start - start - 1),
                )
            })
        });

        let Some((close_start, close_len)) = close_match else {
            break;
        };

        // Extract content between open and close tags
        let open_end = remaining[start..]
            .find('>')
            .map(|p| start + p + 1)
            .unwrap_or(start);
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

/// Build a structured error input for when tool arguments fail to parse.
///
/// The agent loop checks for `__parse_error` in tool inputs and returns
/// an error tool result, allowing the model to self-correct.
fn make_parse_error_input(raw_arguments: &str, error_message: &str) -> serde_json::Value {
    serde_json::json!({
        "__parse_error": error_message,
        "__raw_arguments": raw_arguments,
    })
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
    tool_calls: std::collections::BTreeMap<usize, (String, String, String)>,
    finish_reason: Option<String>,
    usage: Option<OaiUsage>,
) -> Result<MessagesResponse> {
    let mut content_blocks = Vec::new();
    let has_structured_tool_calls = !tool_calls.is_empty();

    if !text_content.is_empty() {
        // If no structured tool calls came through the stream, check for text-based ones
        if !has_structured_tool_calls {
            let (remaining, extracted) = extract_tool_calls_from_text(&text_content);
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
                content_blocks.push(ContentBlock::Text { text: text_content });
            }
        } else {
            content_blocks.push(ContentBlock::Text { text: text_content });
        }
    }

    for (_index, (id, name, arguments)) in tool_calls {
        let input: serde_json::Value =
            match serde_json::from_str(&arguments) {
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
            Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                cache_creation_input_tokens: cache_creation,
                cache_read_input_tokens: cache_read,
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

    #[test]
    fn test_translate_response_text_only() {
        let oai = OaiResponse {
            id: "chatcmpl-123".to_string(),
            choices: vec![OaiChoice {
                message: OaiResponseMessage {
                    role: "assistant".to_string(),
                    content: Some("Hello!".to_string()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OaiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                prompt_tokens_details: None,
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
        assert_eq!(client.name(), "openai");
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
        let input =
            make_parse_error_input(r#"{"broken":true"#, "expected `}` at line 1 column 14");
        assert!(input.get("__parse_error").is_some());
        assert_eq!(
            input.get("__parse_error").and_then(|v| v.as_str()),
            Some("expected `}` at line 1 column 14")
        );
    }

    // ── Stream assembly tests ───────────────────────────────────────────

    #[test]
    fn test_assemble_stream_text_response() {
        let resp = assemble_oai_stream_response(
            "gen-abc".to_string(),
            "Hello world".to_string(),
            std::collections::BTreeMap::new(),
            Some("stop".to_string()),
            Some(OaiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                prompt_tokens_details: None,
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
            tool_calls,
            Some("tool_calls".to_string()),
            Some(OaiUsage {
                prompt_tokens: 50,
                completion_tokens: 30,
                prompt_tokens_details: None,
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
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OaiUsage {
                prompt_tokens: 100,
                completion_tokens: 20,
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
            std::collections::BTreeMap::new(),
            Some("stop".to_string()),
            Some(OaiUsage {
                prompt_tokens: 200,
                completion_tokens: 40,
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

        let resp =
            assemble_oai_stream_response(response_id, text, tool_calls, finish, usage).unwrap();
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

        let resp =
            assemble_oai_stream_response(response_id, text, tool_calls, finish, usage).unwrap();
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

        let resp =
            assemble_oai_stream_response(response_id, text, tool_calls, finish, usage).unwrap();
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
        let text = r#"<|plugin|>{"name": "read_file", "arguments": {"path": "/etc/hosts"}}<|/plugin|>"#;
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
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OaiUsage {
                prompt_tokens: 50,
                completion_tokens: 30,
                prompt_tokens_details: None,
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
            !result.warning.as_deref().unwrap_or("").contains(OPENROUTER_AUTO_MODEL),
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
}
