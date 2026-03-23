//! Integration tests for Anthropic Messages API SSE streaming.
//!
//! Uses mock HTTP servers to validate streaming content accumulation,
//! tool call assembly, non-streaming fallback, and model routing for
//! the `anthropic/` prefix. No real API calls are made.

use std::io::{Read, Write};
use std::net::TcpListener;

use workgraph::executor::native::client::{
    AnthropicClient, ContentBlock, Message, MessagesRequest, Role, StopReason, ToolDefinition,
};
use workgraph::executor::native::provider::Provider;

// ---------------------------------------------------------------------------
// Helpers: mock HTTP server
// ---------------------------------------------------------------------------

/// Start a TCP server that accepts one connection and responds with `body`.
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

/// Build an Anthropic SSE body from event type + data pairs.
fn build_anthropic_sse(events: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (event_type, data) in events {
        body.push_str(&format!("event: {}\ndata: {}\n\n", event_type, data));
    }
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
// 1. SSE text content accumulation (streaming)
// ===========================================================================

#[tokio::test]
async fn anthropic_streaming_text_content_accumulation() {
    let events = vec![
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_test1","type":"message","role":"assistant","content":[],"model":"claude-haiku-4-5","stop_reason":null,"usage":{"input_tokens":15,"output_tokens":0}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];

    let sse_body = build_anthropic_sse(&events);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = AnthropicClient::new("test-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client.send(&test_request("claude-haiku-4-5")).await.unwrap();

    assert_eq!(response.id, "msg_test1");
    assert!(!response.content.is_empty());
    let text = match &response.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("Expected Text block, got: {:?}", other),
    };
    assert_eq!(text, "Hello world!");
    assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(response.usage.input_tokens, 15);
    assert_eq!(response.usage.output_tokens, 5);
}

// ===========================================================================
// 2. SSE tool call assembly (streaming)
// ===========================================================================

#[tokio::test]
async fn anthropic_streaming_tool_call_assembly() {
    let events = vec![
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_tool1","type":"message","role":"assistant","content":[],"model":"claude-haiku-4-5","stop_reason":null,"usage":{"input_tokens":20,"output_tokens":0}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_abc123","name":"bash","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"and\":"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"ls -la\"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":15}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];

    let sse_body = build_anthropic_sse(&events);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = AnthropicClient::new("test-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client
        .send(&test_request_with_tools("claude-haiku-4-5"))
        .await
        .unwrap();

    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    let tool_block = response
        .content
        .iter()
        .find(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .expect("Expected a ToolUse block");

    match tool_block {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "toolu_abc123");
            assert_eq!(name, "bash");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("ls -la")
            );
        }
        _ => unreachable!(),
    }
}

// ===========================================================================
// 3. Streaming with text + tool call (mixed content)
// ===========================================================================

#[tokio::test]
async fn anthropic_streaming_text_and_tool_call() {
    let events = vec![
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_mixed","type":"message","role":"assistant","content":[],"model":"claude-haiku-4-5","stop_reason":null,"usage":{"input_tokens":25,"output_tokens":0}}}"#,
        ),
        // Text block first
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me check."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        // Tool use block
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_mixed1","name":"bash","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];

    let sse_body = build_anthropic_sse(&events);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    let client = AnthropicClient::new("test-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let response = client
        .send(&test_request_with_tools("claude-haiku-4-5"))
        .await
        .unwrap();

    assert_eq!(response.content.len(), 2);

    // First block: text
    match &response.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Let me check."),
        other => panic!("Expected Text, got: {:?}", other),
    }

    // Second block: tool use
    match &response.content[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "toolu_mixed1");
            assert_eq!(name, "bash");
            assert_eq!(
                input.get("command").and_then(|v| v.as_str()),
                Some("ls")
            );
        }
        other => panic!("Expected ToolUse, got: {:?}", other),
    }

    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
}

// ===========================================================================
// 4. Non-streaming fallback
// ===========================================================================

#[tokio::test]
async fn anthropic_non_streaming_fallback() {
    let response_body = serde_json::json!({
        "id": "msg_nostre",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "Hello from non-streaming!"}],
        "model": "claude-haiku-4-5",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });

    let base_url = mock_server_one_shot(200, "application/json", response_body.to_string());

    let client = AnthropicClient::new("test-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(false);

    let response = client.send(&test_request("claude-haiku-4-5")).await.unwrap();

    assert_eq!(response.id, "msg_nostre");
    let text = match &response.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("Expected Text block, got: {:?}", other),
    };
    assert_eq!(text, "Hello from non-streaming!");
    assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
}

// ===========================================================================
// 5. Non-streaming tool call
// ===========================================================================

#[tokio::test]
async fn anthropic_non_streaming_tool_call() {
    let response_body = serde_json::json!({
        "id": "msg_tc_ns",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "tool_use", "id": "toolu_ns1", "name": "bash", "input": {"command": "echo hello"}}
        ],
        "model": "claude-haiku-4-5",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 25, "output_tokens": 12}
    });

    let base_url = mock_server_one_shot(200, "application/json", response_body.to_string());

    let client = AnthropicClient::new("test-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(false);

    let response = client
        .send(&test_request_with_tools("claude-haiku-4-5"))
        .await
        .unwrap();

    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    match &response.content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "toolu_ns1");
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
// 6. Streaming is default for AnthropicClient
// ===========================================================================

#[tokio::test]
async fn anthropic_streaming_is_default() {
    // By default, AnthropicClient should use streaming — verified by sending
    // an SSE-format response which only parses correctly in streaming mode.
    let events = vec![
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_default_stream","type":"message","role":"assistant","content":[],"model":"claude-haiku-4-5","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"streaming works"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];

    let sse_body = build_anthropic_sse(&events);
    let base_url = mock_server_one_shot(200, "text/event-stream", sse_body);

    // No .with_streaming() call — should default to streaming
    let client = AnthropicClient::new("test-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&base_url);

    let response = client.send(&test_request("claude-haiku-4-5")).await.unwrap();
    assert_eq!(response.id, "msg_default_stream");
    let text = match &response.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("Expected Text, got: {:?}", other),
    };
    assert_eq!(text, "streaming works");
}

// ===========================================================================
// 7. Model routing: anthropic/ prefix
// ===========================================================================

#[test]
fn anthropic_prefix_routes_to_anthropic_provider() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    std::fs::create_dir_all(tmp.path()).unwrap();
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();

    // Mock server returning Anthropic format
    let mock_body = format!(
        r#"{{"id":"msg_route","type":"message","role":"assistant","content":[{{"type":"text","text":"hello"}}],"model":"claude-sonnet-4-20250514","stop_reason":"end_turn","usage":{{"input_tokens":10,"output_tokens":5}}}}"#,
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://127.0.0.1:{}", addr.port());

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                mock_body,
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });

    let config_content = format!(
        r#"
[[llm_endpoints.endpoints]]
name = "test-anthropic"
provider = "anthropic"
url = "{base_url}"
api_key = "test-key"
is_default = true
"#,
    );
    std::fs::write(tmp.path().join("config.toml"), config_content).unwrap();

    // "anthropic/claude-sonnet-4-20250514" should route to Anthropic, stripping prefix
    let provider = workgraph::executor::native::provider::create_provider(
        tmp.path(),
        "anthropic/claude-sonnet-4-20250514",
    )
    .unwrap();
    assert_eq!(provider.name(), "anthropic");
    // Model should have prefix stripped
    assert_eq!(provider.model(), "claude-sonnet-4-20250514");
}

// ===========================================================================
// 8. Streaming error propagation
// ===========================================================================

#[tokio::test]
async fn anthropic_streaming_api_error_propagates() {
    let error_body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "authentication_error",
            "message": "Invalid API key"
        }
    });

    let base_url = mock_server_one_shot(401, "application/json", error_body.to_string());

    let client = AnthropicClient::new("bad-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&base_url)
        .with_streaming(true);

    let result = client.send(&test_request("claude-haiku-4-5")).await;
    assert!(result.is_err());
    let err_msg = format!("{:#}", result.unwrap_err());
    assert!(
        err_msg.contains("401")
            || err_msg.contains("Invalid API key")
            || err_msg.contains("authentication")
            || err_msg.contains("failed"),
        "Expected auth error, got: {}",
        err_msg
    );
}

// ===========================================================================
// 9. Conversation journal format consistency (canonical types)
// ===========================================================================
// Both Anthropic and OpenAI paths use the same canonical types (Message,
// ContentBlock, Usage) from client.rs. This test verifies that responses
// from both providers serialize to identical JSON structures, ensuring
// journal format compatibility.

#[tokio::test]
async fn anthropic_and_openai_produce_identical_canonical_format() {
    use workgraph::executor::native::openai_client::OpenAiClient;

    // Anthropic mock: streaming SSE
    let anthropic_events = vec![
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_journal","type":"message","role":"assistant","content":[],"model":"claude-haiku-4-5","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ];
    let anthropic_sse = build_anthropic_sse(&anthropic_events);
    let anthropic_url = mock_server_one_shot(200, "text/event-stream", anthropic_sse);

    // OpenAI mock: non-streaming JSON
    let openai_body = serde_json::json!({
        "id": "chatcmpl-journal",
        "choices": [{
            "message": {"role": "assistant", "content": "Hello"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 1}
    });
    let openai_url = mock_server_one_shot(200, "application/json", openai_body.to_string());

    let anthropic_client = AnthropicClient::new("test-key".to_string(), "claude-haiku-4-5")
        .unwrap()
        .with_base_url(&anthropic_url);

    let openai_client =
        OpenAiClient::new("test-key".to_string(), "test-model", Some(&openai_url)).unwrap();

    let request = test_request("test");

    let anthropic_resp = anthropic_client.send(&request).await.unwrap();
    let openai_resp = openai_client.send(&request).await.unwrap();

    // Both should produce the same canonical content structure
    assert_eq!(anthropic_resp.content.len(), openai_resp.content.len());

    // Both have a Text content block with "Hello"
    let a_text = match &anthropic_resp.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("Anthropic: expected Text, got: {:?}", other),
    };
    let o_text = match &openai_resp.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("OpenAI: expected Text, got: {:?}", other),
    };
    assert_eq!(a_text, o_text, "Both providers should produce identical text content");

    // Both have EndTurn stop reason
    assert_eq!(anthropic_resp.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(openai_resp.stop_reason, Some(StopReason::EndTurn));

    // Both have matching token counts
    assert_eq!(anthropic_resp.usage.input_tokens, openai_resp.usage.input_tokens);
    assert_eq!(anthropic_resp.usage.output_tokens, openai_resp.usage.output_tokens);

    // The canonical Message type serializes identically for both
    let a_msg = Message {
        role: Role::Assistant,
        content: anthropic_resp.content.clone(),
    };
    let o_msg = Message {
        role: Role::Assistant,
        content: openai_resp.content.clone(),
    };
    let a_json = serde_json::to_string(&a_msg).unwrap();
    let o_json = serde_json::to_string(&o_msg).unwrap();
    assert_eq!(a_json, o_json, "Canonical Message JSON must be identical across providers");
}
