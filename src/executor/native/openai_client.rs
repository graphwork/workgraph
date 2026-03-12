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
    stream: bool,
    /// OpenRouter cache_control — triggers auto-caching for Anthropic/Gemini models.
    /// When set, OpenRouter applies cache_control to the last cacheable content block.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<serde_json::Value>,
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

        // Add text content if present
        if let Some(text) = choice.message.content
            && !text.is_empty()
        {
            content_blocks.push(ContentBlock::Text { text });
        }

        // Add tool calls if present
        if let Some(tool_calls) = choice.message.tool_calls {
            for tc in tool_calls {
                let input: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();
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

        let stop_reason = match choice.finish_reason.as_deref() {
            Some("stop") => Some(StopReason::EndTurn),
            Some("tool_calls") => Some(StopReason::ToolUse),
            Some("length") => Some(StopReason::MaxTokens),
            Some("content_filter") => Some(StopReason::StopSequence),
            _ => None,
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
        let oai_request = OaiRequest {
            model: request.model.clone(),
            messages: Self::translate_messages(&request.system, &request.messages),
            max_tokens: Some(request.max_tokens),
            tools: Self::translate_tools(&request.tools),
            stream: false,
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
    async fn chat_completion_streaming(
        &self,
        request: &MessagesRequest,
    ) -> Result<MessagesResponse> {
        let oai_request = OaiRequest {
            model: request.model.clone(),
            messages: Self::translate_messages(&request.system, &request.messages),
            max_tokens: Some(request.max_tokens),
            tools: Self::translate_tools(&request.tools),
            stream: true,
            cache_control: self.cache_control_value(),
        };

        let url = format!("{}/chat/completions", self.base_url);
        let headers = self.build_headers();
        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .json(&oai_request)
            .send()
            .await
            .context("Failed to send streaming request")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(oai_api_error(status.as_u16(), &body));
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

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Error reading SSE chunk")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE data lines from the buffer
            while let Some(data) = parse_next_oai_sse_data(&mut buffer) {
                if data == "[DONE]" {
                    break;
                }

                let chunk: OaiStreamChunk = match serde_json::from_str(&data) {
                    Ok(c) => c,
                    Err(_) => continue, // Skip unparseable chunks
                };

                if response_id.is_empty() && !chunk.id.is_empty() {
                    response_id = chunk.id;
                }

                // Capture usage from the final chunk (if present)
                if let Some(u) = chunk.usage {
                    usage = Some(u);
                }

                for choice in &chunk.choices {
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

        // Assemble the response
        assemble_oai_stream_response(response_id, text_content, tool_calls, finish_reason, usage)
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

    async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse> {
        if self.use_streaming {
            self.chat_completion_streaming(request).await
        } else {
            self.chat_completion(request).await
        }
    }
}

// ── API key resolution ──────────────────────────────────────────────────

/// Resolve an OpenAI-compatible API key.
///
/// Priority: OPENROUTER_API_KEY > OPENAI_API_KEY > config file
fn resolve_openai_api_key() -> Result<String> {
    for var in &["OPENROUTER_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(key) = std::env::var(var)
            && !key.is_empty()
        {
            return Ok(key);
        }
    }

    // Try config file
    if let Ok(content) = std::fs::read_to_string(".workgraph/config.toml")
        && let Ok(val) = toml::from_str::<toml::Value>(&content)
        && let Some(key) = val
            .get("native_executor")
            .and_then(|v| v.get("api_key"))
            .and_then(|v| v.as_str())
        && !key.is_empty()
    {
        return Ok(key.to_string());
    }

    Err(anyhow!(
        "No OpenAI-compatible API key found. Set OPENROUTER_API_KEY or OPENAI_API_KEY \
         environment variable, or add [native_executor] api_key to .workgraph/config.toml"
    ))
}

/// Resolve API key from a specific workgraph directory.
pub fn resolve_openai_api_key_from_dir(workgraph_dir: &std::path::Path) -> Result<String> {
    for var in &["OPENROUTER_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(key) = std::env::var(var)
            && !key.is_empty()
        {
            return Ok(key);
        }
    }

    let config_path = workgraph_dir.join("config.toml");
    if let Ok(content) = std::fs::read_to_string(&config_path)
        && let Ok(val) = toml::from_str::<toml::Value>(&content)
        && let Some(key) = val
            .get("native_executor")
            .and_then(|v| v.get("api_key"))
            .and_then(|v| v.as_str())
        && !key.is_empty()
    {
        return Ok(key.to_string());
    }

    Err(anyhow!(
        "No OpenAI-compatible API key found. Set OPENROUTER_API_KEY or OPENAI_API_KEY \
         environment variable, or add [native_executor] api_key to .workgraph/config.toml"
    ))
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

    if !text_content.is_empty() {
        content_blocks.push(ContentBlock::Text { text: text_content });
    }

    for (_index, (id, name, arguments)) in tool_calls {
        let input: serde_json::Value =
            serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null);
        content_blocks.push(ContentBlock::ToolUse { id, name, input });
    }

    if content_blocks.is_empty() {
        content_blocks.push(ContentBlock::Text {
            text: String::new(),
        });
    }

    let stop_reason = match finish_reason.as_deref() {
        Some("stop") => Some(StopReason::EndTurn),
        Some("tool_calls") => Some(StopReason::ToolUse),
        Some("length") => Some(StopReason::MaxTokens),
        Some("content_filter") => Some(StopReason::StopSequence),
        _ => None,
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
            stream: false,
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
            stream: false,
            cache_control: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("cache_control"));
    }
}
