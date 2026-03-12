//! Anthropic Messages API client and canonical request/response types.
//!
//! Defines the canonical types (`Message`, `ContentBlock`, `ToolDefinition`, etc.)
//! used by the agent loop. The `AnthropicClient` implements the `Provider` trait
//! (defined in `provider.rs`) for the Anthropic Messages API.
//! See `openai_client.rs` for the OpenAI-compatible implementation.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

// ── Request / Response types ────────────────────────────────────────────

/// Role in a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// A conversation message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// Tool definition sent to the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Stop reason returned by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

/// Token usage information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
}

impl Usage {
    /// Accumulate usage from another response.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        if let Some(v) = other.cache_creation_input_tokens {
            *self.cache_creation_input_tokens.get_or_insert(0) += v;
        }
        if let Some(v) = other.cache_read_input_tokens {
            *self.cache_read_input_tokens.get_or_insert(0) += v;
        }
    }
}
/// Request body for POST /v1/messages.
#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
}

/// Response body from POST /v1/messages (non-streaming).
#[derive(Debug, Clone, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
}

/// Error response from the API.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorResponse {
    #[serde(rename = "type")]
    pub error_type: String,
    pub error: ApiErrorDetail,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorDetail {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// ── Streaming event types ───────────────────────────────────────────────

/// Streaming SSE events from the Messages API.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: MessagesResponse,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: ContentDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDelta,
        usage: Usage,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiErrorDetail,
    },
}

/// Delta within a content block.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
}

/// Delta at the message level (stop reason).
#[derive(Debug, Clone, Deserialize)]
pub struct MessageDelta {
    pub stop_reason: Option<StopReason>,
}

// ── Client ──────────────────────────────────────────────────────────────

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 16384;

/// Anthropic Messages API client.
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    pub model: String,
    pub max_tokens: u32,
}

impl AnthropicClient {
    /// Create a client using the `ANTHROPIC_API_KEY` environment variable.
    pub fn from_env(model: &str) -> Result<Self> {
        let api_key = resolve_api_key()?;
        Self::new(api_key, model)
    }

    /// Create a client with an explicit API key.
    pub fn from_config(api_key: &str, model: &str) -> Result<Self> {
        Self::new(api_key.to_string(), model)
    }

    pub fn new(api_key: String, model: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            http,
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
        })
    }

    /// Override the base URL (useful for proxies or testing).
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.trim_end_matches('/').to_string();
        self
    }

    /// Override max tokens per response.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Send a non-streaming messages request.
    pub async fn messages(&self, request: &MessagesRequest) -> Result<MessagesResponse> {
        let url = format!("{}/v1/messages", self.base_url);

        // Non-streaming request
        let mut req = request.clone();
        req.stream = false;

        let response = self
            .send_with_retry(&url, &req)
            .await
            .context("Messages API request failed")?;

        Ok(response)
    }

    /// Send a streaming messages request and accumulate into a single response.
    ///
    /// Consumes the SSE stream internally and returns the fully assembled
    /// `MessagesResponse`. For real-time progress, use `messages_stream`.
    pub async fn messages_streaming(&self, request: &MessagesRequest) -> Result<MessagesResponse> {
        let events = self.messages_stream_raw(request).await?;
        assemble_stream_response(events).await
    }

    /// Send a streaming request and collect raw SSE events.
    async fn messages_stream_raw(&self, request: &MessagesRequest) -> Result<Vec<StreamEvent>> {
        let url = format!("{}/v1/messages", self.base_url);

        let mut req = request.clone();
        req.stream = true;

        let headers = self.build_headers();
        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .json(&req)
            .send()
            .await
            .context("Failed to send streaming request")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(api_error(status.as_u16(), &body));
        }

        let mut events = Vec::new();
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Error reading SSE chunk")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Parse SSE events from the buffer
            while let Some(event) = parse_next_sse_event(&mut buffer) {
                events.push(event);
            }
        }

        Ok(events)
    }

    /// Send a request with retry logic for transient errors.
    async fn send_with_retry(
        &self,
        url: &str,
        request: &MessagesRequest,
    ) -> Result<MessagesResponse> {
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
                        let msg: MessagesResponse =
                            serde_json::from_str(&body).with_context(|| {
                                format!("Failed to parse API response: {}", truncate(&body, 500))
                            })?;
                        return Ok(msg);
                    }

                    let status_code = status.as_u16();
                    let body = response.text().await.unwrap_or_default();

                    // Retry on transient errors
                    if is_retryable(status_code) && retry_count < max_retries {
                        retry_count += 1;
                        let wait = parse_retry_after(&body).unwrap_or(backoff_ms);
                        eprintln!(
                            "[native-executor] Retryable error {} (attempt {}/{}), waiting {}ms",
                            status_code, retry_count, max_retries, wait
                        );
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        backoff_ms = (backoff_ms * 2).min(60_000);
                        continue;
                    }

                    return Err(api_error(status_code, &body));
                }
                Err(e) => {
                    if retry_count < max_retries {
                        retry_count += 1;
                        eprintln!(
                            "[native-executor] Network error (attempt {}/{}): {}",
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

    fn build_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&self.api_key).expect("invalid api key header"),
        );
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers
    }
}

#[async_trait::async_trait]
impl super::provider::Provider for AnthropicClient {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse> {
        self.messages(request).await
    }
}

// ── API key resolution ──────────────────────────────────────────────────

/// Resolve the API key using the standard priority chain.
fn resolve_api_key() -> Result<String> {
    // 1. Environment variable
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        return Ok(key);
    }

    // 2. .workgraph/config.toml [native_executor] api_key
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

    // 3. ~/.config/anthropic/api_key file
    if let Some(config_dir) = dirs::config_dir() {
        let key_path = config_dir.join("anthropic").join("api_key");
        if let Ok(key) = std::fs::read_to_string(&key_path) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Ok(key);
            }
        }
    }

    Err(anyhow!(
        "No Anthropic API key found. Set ANTHROPIC_API_KEY environment variable, \
         add [native_executor] api_key to .workgraph/config.toml, \
         or create ~/.config/anthropic/api_key"
    ))
}

/// Resolve the API key, optionally looking in a specific workgraph directory.
pub fn resolve_api_key_from_dir(workgraph_dir: &Path) -> Result<String> {
    // 1. Environment variable
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        return Ok(key);
    }

    // 2. Workgraph config
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

    // 3. ~/.config/anthropic/api_key
    if let Some(config_dir) = dirs::config_dir() {
        let key_path = config_dir.join("anthropic").join("api_key");
        if let Ok(key) = std::fs::read_to_string(&key_path) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Ok(key);
            }
        }
    }

    Err(anyhow!(
        "No Anthropic API key found. Set ANTHROPIC_API_KEY environment variable, \
         add [native_executor] api_key to .workgraph/config.toml, \
         or create ~/.config/anthropic/api_key"
    ))
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 529)
}

fn api_error(status: u16, body: &str) -> anyhow::Error {
    if let Ok(err) = serde_json::from_str::<ApiErrorResponse>(body) {
        anyhow!(
            "Anthropic API error {}: {} ({})",
            status,
            err.error.message,
            err.error.error_type
        )
    } else {
        anyhow!("Anthropic API error {}: {}", status, truncate(body, 500))
    }
}

fn parse_retry_after(body: &str) -> Option<u64> {
    // Try to parse retry-after from error response
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(secs) = val.get("retry_after").and_then(|v| v.as_f64())
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

/// Parse a single SSE event from the buffer, consuming it.
fn parse_next_sse_event(buffer: &mut String) -> Option<StreamEvent> {
    // SSE events are separated by double newlines
    let sep = buffer.find("\n\n")?;
    let event_text = buffer[..sep].to_string();
    buffer.drain(..sep + 2);

    let mut event_type = String::new();
    let mut data_lines = Vec::new();

    for line in event_text.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            data_lines.push(rest.to_string());
        } else if line.starts_with(":") {
            // Comment, skip
        }
    }

    if data_lines.is_empty() {
        return None;
    }

    let data = data_lines.join("\n");

    // Parse based on event type
    match event_type.as_str() {
        "message_start" => serde_json::from_str(&data).ok(),
        "content_block_start" => serde_json::from_str(&data).ok(),
        "content_block_delta" => serde_json::from_str(&data).ok(),
        "content_block_stop" => serde_json::from_str(&data).ok(),
        "message_delta" => serde_json::from_str(&data).ok(),
        "message_stop" => Some(StreamEvent::MessageStop),
        "ping" => Some(StreamEvent::Ping),
        "error" => serde_json::from_str(&data).ok(),
        _ => {
            // Try generic parse
            serde_json::from_str(&data).ok()
        }
    }
}

/// Assemble a complete response from streaming events.
async fn assemble_stream_response(events: Vec<StreamEvent>) -> Result<MessagesResponse> {
    let mut response_id = String::new();
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut stop_reason = None;
    let mut usage = Usage::default();
    let mut json_accumulators: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();

    for event in events {
        match event {
            StreamEvent::MessageStart { message } => {
                response_id = message.id;
                usage.add(&message.usage);
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                // Grow content_blocks to fit
                while content_blocks.len() <= index {
                    content_blocks.push(ContentBlock::Text {
                        text: String::new(),
                    });
                }
                content_blocks[index] = content_block;
            }
            StreamEvent::ContentBlockDelta { index, delta } => match delta {
                ContentDelta::TextDelta { text } => {
                    if let Some(ContentBlock::Text { text: t }) = content_blocks.get_mut(index) {
                        t.push_str(&text);
                    }
                }
                ContentDelta::InputJsonDelta { partial_json } => {
                    json_accumulators
                        .entry(index)
                        .or_default()
                        .push_str(&partial_json);
                }
            },
            StreamEvent::ContentBlockStop { index } => {
                // Finalize JSON accumulator for tool_use blocks
                if let Some(json_str) = json_accumulators.remove(&index)
                    && let Some(ContentBlock::ToolUse { input, .. }) = content_blocks.get_mut(index)
                {
                    *input = serde_json::from_str(&json_str).unwrap_or(serde_json::Value::Null);
                }
            }
            StreamEvent::MessageDelta {
                delta: md,
                usage: u,
            } => {
                if let Some(sr) = md.stop_reason {
                    stop_reason = Some(sr);
                }
                usage.add(&u);
            }
            StreamEvent::MessageStop | StreamEvent::Ping => {}
            StreamEvent::Error { error } => {
                return Err(anyhow!("Stream error: {}", error.message));
            }
        }
    }

    Ok(MessagesResponse {
        id: response_id,
        content: content_blocks,
        stop_reason,
        usage,
    })
}
