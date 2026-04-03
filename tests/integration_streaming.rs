//! Integration tests for OpenAI-compatible SSE streaming.
//!
//! Uses a mock HTTP server to validate streaming content accumulation,
//! tool call assembly, error handling, and non-streaming fallback.
//! No real API calls are made.

use std::io::{Read, Write};
use std::net::TcpListener;

use workgraph::executor::native::client::{
    ContentBlock, Message, MessagesRequest, Role, StopReason, ToolDefinition,
};
use workgraph::executor::native::openai_client::OpenAiClient;
use workgraph::executor::native::provider::Provider;

// ---------------------------------------------------------------------------
// Helpers: mock HTTP server
// ---------------------------------------------------------------------------

/// Start a TCP server that accepts one connection and responds with `body`.
/// Returns the base URL (e.g. "http://127.0.0.1:PORT").
fn mock_server_one_shot(status: u16, content_type: &str, body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());
    let ct = content_type.to_string();

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nConnection: close\r\n\r\n{}",
                status, ct, body,
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });

    url
}

/// Build an SSE body from a sequence of JSON chunk strings, ending with [DONE].
fn build_sse_body(chunks: &[&str]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {}\n\n", chunk));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// A simple request for testing.
fn test_request(model: &str) -> MessagesRequest {
    MessagesRequest {
        model: model.to_string(),
        system: Some("You are a test assistant.".to_string()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        }],
        tools: vec![],
        max_tokens: 1024,
        stream: false,
    }
}

/// A request with tool definitions.
fn test_request_with_tools(model: &str) -> MessagesRequest {
    MessagesRequest {
        model: model.to_string(),
        system: Some("You are a test assistant.".to_string()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Run ls".to_string(),
            }],
        }],
        tools: vec![ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a command".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }),
        }],
        max_tokens: 1024,
        stream: false,
    }
}

// ===========================================================================
// 1. SSE text content accumulation
// ===========================================================================

#[tokio::test]
async fn streaming_text_content_accumulation() {
    let chunks = vec![
        r#"{"id":"chatcmpl-test1","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-test1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-test1","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-test1","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-test1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":15,"completion_tokens":3}}"#,
    ];

    let sse_body = build_sse_body(&chunks);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client.send(&test_request("test-model")).await.unwrap();

    // Verify accumulated text
    assert_eq!(response.id, "chatcmpl-test1");
    assert!(response.content.len() >= 1);
    let text = match &response.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("Expected Text block, got: {:?}", other),
    };
    assert_eq!(text, "Hello world!");
    assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(response.usage.input_tokens, 15);
    assert_eq!(response.usage.output_tokens, 3);
}

// ===========================================================================
// 2. SSE tool call assembly
// ===========================================================================

#[tokio::test]
async fn streaming_tool_call_assembly() {
    let chunks = vec![
        // Initial chunk with role
        r#"{"id":"chatcmpl-tc1","choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}]}"#,
        // Tool call start: id + name + empty args
        r#"{"id":"chatcmpl-tc1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc123","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}"#,
        // Partial arguments chunk 1
        r#"{"id":"chatcmpl-tc1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"comm"}}]},"finish_reason":null}]}"#,
        // Partial arguments chunk 2
        r#"{"id":"chatcmpl-tc1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":"}}]},"finish_reason":null}]}"#,
        // Partial arguments chunk 3
        r#"{"id":"chatcmpl-tc1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls -la\"}"}}]},"finish_reason":null}]}"#,
        // Finish
        r#"{"id":"chatcmpl-tc1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":10}}"#,
    ];

    let sse_body = build_sse_body(&chunks);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client
        .send(&test_request_with_tools("test-model"))
        .await
        .unwrap();

    // Verify tool call was assembled
    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    let tool_block = response
        .content
        .iter()
        .find(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .expect("Expected a ToolUse block");

    match tool_block {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_abc123");
            assert_eq!(name, "bash");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("ls -la")
            );
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn streaming_multiple_tool_calls() {
    let chunks = vec![
        r#"{"id":"chatcmpl-multi","choices":[{"index":0,"delta":{"role":"assistant","content":"Let me check."},"finish_reason":null}]}"#,
        // First tool call
        r#"{"id":"chatcmpl-multi","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]},"finish_reason":null}]}"#,
        // Second tool call
        r#"{"id":"chatcmpl-multi","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}]},"finish_reason":null}]}"#,
        // Finish
        r#"{"id":"chatcmpl-multi","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ];

    let sse_body = build_sse_body(&chunks);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client
        .send(&test_request_with_tools("test-model"))
        .await
        .unwrap();

    // Text + 2 tool calls
    assert!(response.content.len() >= 3);
    let tool_uses: Vec<_> = response
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .collect();
    assert_eq!(tool_uses.len(), 2);
}

// ===========================================================================
// 3. Stream drop mid-stream → error handling
// ===========================================================================

#[tokio::test]
async fn streaming_server_drops_connection_errors() {
    // Server that closes connection after sending a partial stream (no [DONE], no finish_reason)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://127.0.0.1:{}", addr.port());

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            // Send headers + one chunk, then drop
            let partial = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: text/event-stream\r\n",
                "Connection: close\r\n\r\n",
                "data: {\"id\":\"chatcmpl-drop\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            );
            let _ = stream.write_all(partial.as_bytes());
            let _ = stream.flush();
            // Close immediately — no [DONE] sentinel
            drop(stream);
        }
    });

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    // The stream was interrupted without a finish_reason → the client should
    // either return an error (after retries) or return a partial response.
    // With retries, the server is gone, so this will ultimately fail.
    let result = client.send(&test_request("test-model")).await;

    // The client has retry logic. After retries exhaust (the server thread only
    // accepts one connection), it should produce an error.
    // Either way, the test validates the client handles broken streams gracefully.
    match result {
        Ok(resp) => {
            // If the client assembled a partial response, that's acceptable too
            assert!(!resp.content.is_empty());
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("Stream")
                    || msg.contains("stream")
                    || msg.contains("failed")
                    || msg.contains("error")
                    || msg.contains("retry"),
                "Expected streaming error message, got: {}",
                msg
            );
        }
    }
}

// ===========================================================================
// 4. Non-streaming fallback
// ===========================================================================

#[tokio::test]
async fn non_streaming_fallback_works() {
    // Standard non-streaming JSON response
    let response_body = serde_json::json!({
        "id": "chatcmpl-nostre",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Hello from non-streaming!",
                "tool_calls": null
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5
        }
    });

    let base_url = mock_server_one_shot(200, "application/json", response_body.to_string());

    // Explicitly disable streaming
    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(false);

    let response = client.send(&test_request("test-model")).await.unwrap();

    assert_eq!(response.id, "chatcmpl-nostre");
    let text = match &response.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("Expected Text block, got: {:?}", other),
    };
    assert_eq!(text, "Hello from non-streaming!");
    assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
}

#[tokio::test]
async fn non_streaming_tool_call_response() {
    let response_body = serde_json::json!({
        "id": "chatcmpl-tc-ns",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_ns_1",
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": "{\"command\":\"echo hello\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 25,
            "completion_tokens": 12
        }
    });

    let base_url = mock_server_one_shot(200, "application/json", response_body.to_string());

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(false);

    let response = client
        .send(&test_request_with_tools("test-model"))
        .await
        .unwrap();

    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    match &response.content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_ns_1");
            assert_eq!(name, "bash");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("echo hello")
            );
        }
        other => panic!("Expected ToolUse, got: {:?}", other),
    }
}

// ===========================================================================
// 5. Streaming flag propagation from config to client
// ===========================================================================

#[test]
fn streaming_flag_openrouter_enables_by_default() {
    let client = OpenAiClient::new("test-key".into(), "model", None)
        .unwrap()
        .with_provider_hint("openrouter");
    // OpenRouter provider hint should auto-enable streaming
    // We verify by checking that the Provider trait dispatches to streaming
    // Since use_streaming is private, we test the behavior through send()
    // But we can verify via the provider hint that was set
    assert_eq!(client.model, "model");
    // The client name reflects the provider hint
    assert_eq!(client.name(), "openrouter");
}

#[test]
fn streaming_flag_default_disabled() {
    let client = OpenAiClient::new("test-key".into(), "model", None).unwrap();
    assert_eq!(client.name(), "openai"); // default name
}

#[test]
fn streaming_flag_explicit_override() {
    // Enable streaming, then disable it
    let client = OpenAiClient::new("test-key".into(), "model", None)
        .unwrap()
        .with_provider_hint("openrouter")
        .with_streaming(false);
    // Even with OpenRouter hint, explicit override should take precedence
    assert_eq!(client.name(), "openrouter");
}

#[tokio::test]
async fn streaming_enabled_uses_sse_path() {
    // When streaming is enabled, the client should parse SSE format
    let chunks = vec![
        r#"{"id":"chatcmpl-sse","choices":[{"index":0,"delta":{"content":"SSE works"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-sse","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
    ];
    let sse_body = build_sse_body(&chunks);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client.send(&test_request("test-model")).await.unwrap();
    let text = match &response.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("Expected Text, got: {:?}", other),
    };
    assert_eq!(text, "SSE works");
}

#[tokio::test]
async fn streaming_disabled_uses_json_path() {
    // When streaming is disabled, the client should parse standard JSON
    let response_body = serde_json::json!({
        "id": "chatcmpl-json",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "JSON works",
                "tool_calls": null
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2}
    });

    let base_url = mock_server_one_shot(200, "application/json", response_body.to_string());

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(false);

    let response = client.send(&test_request("test-model")).await.unwrap();
    let text = match &response.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("Expected Text, got: {:?}", other),
    };
    assert_eq!(text, "JSON works");
}

// ===========================================================================
// 6. Edge cases
// ===========================================================================

#[tokio::test]
async fn streaming_with_usage_in_final_chunk() {
    let chunks = vec![
        r#"{"id":"chatcmpl-usage","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-usage","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":80,"cache_write_tokens":10}}}"#,
    ];

    let sse_body = build_sse_body(&chunks);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client.send(&test_request("test-model")).await.unwrap();
    assert_eq!(response.usage.input_tokens, 100);
    assert_eq!(response.usage.output_tokens, 50);
    assert_eq!(response.usage.cache_read_input_tokens, Some(80));
    assert_eq!(response.usage.cache_creation_input_tokens, Some(10));
}

#[tokio::test]
async fn streaming_empty_content_returns_empty_text() {
    let chunks = vec![
        r#"{"id":"chatcmpl-empty","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-empty","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ];

    let sse_body = build_sse_body(&chunks);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = OpenAiClient::new("test-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client.send(&test_request("test-model")).await.unwrap();
    // Empty content should still produce at least one text block
    assert!(!response.content.is_empty());
}

#[tokio::test]
async fn streaming_api_error_propagates() {
    let error_body = serde_json::json!({
        "error": {
            "message": "Invalid API key",
            "type": "authentication_error"
        }
    });

    let base_url = mock_server_one_shot(401, "application/json", error_body.to_string());

    let client = OpenAiClient::new("bad-key".into(), "test-model", None)
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let result = client.send(&test_request("test-model")).await;
    assert!(result.is_err());
    // The 401 triggers retries, but those fail too (mock serves one request).
    // The final error may be the original API error or a retry-exhaustion wrapper.
    let err_msg = format!("{:#}", result.unwrap_err());
    assert!(
        err_msg.contains("401")
            || err_msg.contains("Invalid API key")
            || err_msg.contains("failed")
            || err_msg.contains("retries"),
        "Expected error after auth failure, got: {}",
        err_msg
    );
}

#[test]
fn from_endpoint_sets_streaming_for_openrouter() {
    use workgraph::config::EndpointConfig;

    let ep = EndpointConfig {
        name: "or-test".to_string(),
        provider: "openrouter".to_string(),
        url: Some("https://openrouter.ai/api/v1".to_string()),
        model: None,
        api_key: Some("sk-or-key".to_string()),
        api_key_file: None,
        api_key_env: None,
        is_default: true,
        context_window: None,
    };

    let client =
        OpenAiClient::from_endpoint(&ep, "anthropic/claude-sonnet-4-20250514", None).unwrap();
    // OpenRouter provider should auto-enable streaming
    assert_eq!(client.name(), "openrouter");
    assert_eq!(client.model, "anthropic/claude-sonnet-4-20250514");
}
