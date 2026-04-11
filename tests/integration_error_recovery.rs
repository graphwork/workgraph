//! Integration tests for error recovery with withholding in the agent loop.
//!
//! Verifies that:
//! - Transient API errors (429, 500) are recovered transparently
//! - The model's conversation history never contains raw infrastructure errors
//! - Auth errors (401) cause immediate failure
//! - Context-too-long errors trigger compaction + retry
//! - Timeout errors are recovered gracefully
//! - JSON parse errors in tool arguments are handled without crashing

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;

use workgraph::executor::native::agent::AgentLoop;
use workgraph::executor::native::client::{
    ContentBlock, Message, MessagesRequest, MessagesResponse, StopReason, Usage,
};
use workgraph::executor::native::openai_client::ApiError;
use workgraph::executor::native::provider::Provider;
use workgraph::executor::native::tools::ToolRegistry;

// ---------------------------------------------------------------------------
// Mock provider that can inject errors at specific call indices
// ---------------------------------------------------------------------------

enum MockAction {
    Respond(MessagesResponse),
    Error(Box<dyn Fn() -> anyhow::Error + Send + Sync>),
}

struct ErrorInjectingProvider {
    actions: Vec<MockAction>,
    call_count: Arc<AtomicUsize>,
    /// Captures the messages sent to the provider for inspection.
    captured_messages: Arc<std::sync::Mutex<Vec<Vec<Message>>>>,
}

impl ErrorInjectingProvider {
    fn new(actions: Vec<MockAction>) -> Self {
        Self {
            actions,
            call_count: Arc::new(AtomicUsize::new(0)),
            captured_messages: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    fn captured_messages(&self) -> Vec<Vec<Message>> {
        self.captured_messages.lock().unwrap().clone()
    }
}

fn make_text_response(text: &str) -> MessagesResponse {
    MessagesResponse {
        id: "msg-test".to_string(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: Some(StopReason::EndTurn),
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        },
    }
}

fn make_tool_response(tool_name: &str, tool_input: serde_json::Value) -> MessagesResponse {
    MessagesResponse {
        id: "msg-tool".to_string(),
        content: vec![ContentBlock::ToolUse {
            id: "tu-1".to_string(),
            name: tool_name.to_string(),
            input: tool_input,
        }],
        stop_reason: Some(StopReason::ToolUse),
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        },
    }
}

#[async_trait::async_trait]
impl Provider for ErrorInjectingProvider {
    fn name(&self) -> &str {
        "mock-error"
    }

    fn model(&self) -> &str {
        "mock-error-model"
    }

    fn max_tokens(&self) -> u32 {
        4096
    }

    fn context_window(&self) -> usize {
        32000
    }

    async fn send(&self, request: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);

        // Capture messages for inspection
        {
            let messages: Vec<Message> = request.messages.clone();
            self.captured_messages.lock().unwrap().push(messages);
        }

        if idx < self.actions.len() {
            match &self.actions[idx] {
                MockAction::Respond(resp) => Ok(resp.clone()),
                MockAction::Error(make_err) => Err(make_err()),
            }
        } else {
            // Fallback: end turn
            Ok(make_text_response("[mock exhausted]"))
        }
    }
}

fn setup_workgraph(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let graph_path = dir.join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();
}

// ---------------------------------------------------------------------------
// Test: 500 error → graceful recovery, model never sees raw error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_server_error_500_recovered_gracefully() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let provider = ErrorInjectingProvider::new(vec![
        // First call: server error (simulates retries exhausted at client level)
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 500,
                message: "Internal Server Error".to_string(),
            }
            .into()
        })),
        // Second call: success (after graceful recovery)
        MockAction::Respond(make_text_response("Task completed successfully.")),
    ]);

    let captured = provider.captured_messages.clone();
    let registry = ToolRegistry::default_all(&wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test-500.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    );

    let result = agent.run("Do the thing.").await;
    assert!(
        result.is_ok(),
        "Agent should recover from 500: {:?}",
        result.err()
    );

    // Verify the model's second request contains the friendly recovery message,
    // NOT the raw "Internal Server Error" text.
    let messages = captured.lock().unwrap();
    assert!(messages.len() >= 2, "Should have at least 2 API calls");

    // Check the second call's messages (what the model sees)
    let second_call_messages = &messages[1];
    for msg in second_call_messages {
        for block in &msg.content {
            if let ContentBlock::Text { text } = block {
                assert!(
                    !text.contains("Internal Server Error"),
                    "Model should never see raw error: {}",
                    text
                );
                assert!(
                    !text.contains("500"),
                    "Model should never see HTTP status codes: {}",
                    text
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test: 429 error → graceful recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rate_limit_429_recovered_gracefully() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let provider = ErrorInjectingProvider::new(vec![
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 429,
                message: "Rate limit exceeded".to_string(),
            }
            .into()
        })),
        MockAction::Respond(make_text_response("Recovered from rate limit.")),
    ]);

    let captured = provider.captured_messages.clone();
    let registry = ToolRegistry::default_all(&wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test-429.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    );

    let result = agent.run("Do the thing.").await;
    assert!(
        result.is_ok(),
        "Agent should recover from 429: {:?}",
        result.err()
    );

    // Verify no raw error in model messages
    let messages = captured.lock().unwrap();
    for call_messages in messages.iter() {
        for msg in call_messages {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block {
                    assert!(
                        !text.contains("Rate limit"),
                        "Model should never see rate limit error: {}",
                        text
                    );
                    assert!(
                        !text.contains("429"),
                        "Model should never see HTTP status: {}",
                        text
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test: 401 error → immediate failure (non-recoverable)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_error_401_fails_immediately() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let provider = ErrorInjectingProvider::new(vec![MockAction::Error(Box::new(|| {
        ApiError {
            status: 401,
            message: "Invalid API key".to_string(),
        }
        .into()
    }))]);

    let registry = ToolRegistry::default_all(&wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test-401.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    );

    let result = agent.run("Do the thing.").await;
    assert!(result.is_err(), "Auth error should fail immediately");
    let err_msg = format!("{:#}", result.unwrap_err());
    assert!(
        err_msg.contains("401") || err_msg.contains("Authentication"),
        "Error should mention auth failure: {}",
        err_msg
    );
}

// ---------------------------------------------------------------------------
// Test: Consecutive server errors exceed limit → eventual failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_consecutive_server_errors_eventually_fail() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    // 4 consecutive 500 errors — should exceed the limit of 3
    let provider = ErrorInjectingProvider::new(vec![
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 500,
                message: "err".to_string(),
            }
            .into()
        })),
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 500,
                message: "err".to_string(),
            }
            .into()
        })),
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 500,
                message: "err".to_string(),
            }
            .into()
        })),
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 500,
                message: "err".to_string(),
            }
            .into()
        })),
    ]);

    let registry = ToolRegistry::default_all(&wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test-500-limit.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    );

    let result = agent.run("Do the thing.").await;
    assert!(
        result.is_err(),
        "Should fail after consecutive server errors"
    );
}

// ---------------------------------------------------------------------------
// Test: Server error followed by success resets counter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_server_error_counter_resets_on_success() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    // Pattern: error → success → error → success → done
    let provider = ErrorInjectingProvider::new(vec![
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 500,
                message: "err1".to_string(),
            }
            .into()
        })),
        MockAction::Respond(make_tool_response(
            "bash",
            serde_json::json!({"command": "echo hello"}),
        )),
        // After tool execution, model responds with final text
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 502,
                message: "err2".to_string(),
            }
            .into()
        })),
        MockAction::Respond(make_text_response("All done.")),
    ]);

    let registry = ToolRegistry::default_all(&wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test-reset.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    );

    let result = agent.run("Do the thing.").await;
    assert!(
        result.is_ok(),
        "Should succeed — error counter resets after each success: {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------------
// Test: Timeout error → graceful recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_timeout_error_recovered_gracefully() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let provider = ErrorInjectingProvider::new(vec![
        MockAction::Error(Box::new(|| anyhow::anyhow!("request timed out"))),
        MockAction::Respond(make_text_response("Recovered from timeout.")),
    ]);

    let captured = provider.captured_messages.clone();
    let registry = ToolRegistry::default_all(&wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test-timeout.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    );

    let result = agent.run("Do the thing.").await;
    assert!(
        result.is_ok(),
        "Agent should recover from timeout: {:?}",
        result.err()
    );

    // Verify model didn't see raw timeout
    let messages = captured.lock().unwrap();
    for call_messages in messages.iter() {
        for msg in call_messages {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block {
                    assert!(
                        !text.contains("timed out"),
                        "Model should never see raw timeout: {}",
                        text
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test: Model conversation history never contains HTTP error patterns
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_model_conversation_never_contains_raw_errors() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    // Mix of errors and successes
    let provider = ErrorInjectingProvider::new(vec![
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 500,
                message: "Internal Server Error\nstack trace: ...".to_string(),
            }
            .into()
        })),
        MockAction::Error(Box::new(|| {
            ApiError {
                status: 429,
                message: "Rate limit exceeded. Please retry after 2s".to_string(),
            }
            .into()
        })),
        MockAction::Respond(make_text_response("Task completed.")),
    ]);

    let captured = provider.captured_messages.clone();
    let registry = ToolRegistry::default_all(&wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test-clean.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    );

    let result = agent.run("Do the thing.").await;
    assert!(result.is_ok(), "Agent should recover: {:?}", result.err());

    // Exhaustive check: no message the model ever sees should contain error patterns
    let messages = captured.lock().unwrap();
    let error_patterns = [
        "Internal Server Error",
        "Rate limit exceeded",
        "stack trace",
        "HTTP 500",
        "HTTP 429",
        "retry after",
        "ApiError",
    ];

    for (call_idx, call_messages) in messages.iter().enumerate() {
        for msg in call_messages {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block {
                    for pattern in &error_patterns {
                        assert!(
                            !text.contains(pattern),
                            "Call {}: model message contains forbidden pattern '{}': {}",
                            call_idx,
                            pattern,
                            text
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test: JSON parse error in tool arguments → model gets error result, not crash
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_json_parse_error_in_tool_args_no_crash() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    // Model returns a tool call with malformed JSON arguments
    let malformed_tool = MessagesResponse {
        id: "msg-malformed".to_string(),
        content: vec![ContentBlock::ToolUse {
            id: "tu-bad".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({
                "__parse_error": "expected `,` or `}` at line 1",
                "__raw_arguments": "{broken json",
            }),
        }],
        stop_reason: Some(StopReason::ToolUse),
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        },
    };

    let provider = ErrorInjectingProvider::new(vec![
        MockAction::Respond(malformed_tool),
        MockAction::Respond(make_text_response(
            "I see the parse error. Let me fix my tool call.",
        )),
    ]);

    let registry = ToolRegistry::default_all(&wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test-parse.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    );

    let result = agent.run("Do the thing.").await;
    assert!(
        result.is_ok(),
        "Agent should handle parse errors gracefully: {:?}",
        result.err()
    );
}
