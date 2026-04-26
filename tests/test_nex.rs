//! Tests for the nex interactive REPL.
//!
//! Uses mock providers to verify multi-turn conversation, tool calling,
//! and streaming behavior in the interactive agent loop.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use async_trait::async_trait;
use tempfile::TempDir;

use workgraph::executor::native::agent::AgentLoop;
use workgraph::executor::native::client::{
    ContentBlock, MessagesRequest, MessagesResponse, StopReason, Usage,
};
use workgraph::executor::native::provider::Provider;
use workgraph::executor::native::tools::ToolRegistry;

struct EchoProvider {
    call_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for EchoProvider {
    fn name(&self) -> &str {
        "mock-echo"
    }
    fn model(&self) -> &str {
        "echo-model"
    }
    fn max_tokens(&self) -> u32 {
        1024
    }
    async fn send(&self, req: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        let user_text = req
            .messages
            .last()
            .and_then(|m| {
                m.content.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| "no input".to_string());
        Ok(MessagesResponse {
            id: format!("msg_echo_{}", count),
            content: vec![ContentBlock::Text {
                text: format!("Echo #{}: {}", count, user_text),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 15,
                ..Default::default()
            },
        })
    }
    async fn send_streaming(
        &self,
        req: &MessagesRequest,
        on_text: &(dyn Fn(String) + Send + Sync),
    ) -> anyhow::Result<MessagesResponse> {
        let resp = self.send(req).await?;
        for block in &resp.content {
            if let ContentBlock::Text { text } = block {
                on_text(text.clone());
            }
        }
        Ok(resp)
    }
}

struct ToolUseProvider {
    call_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for ToolUseProvider {
    fn name(&self) -> &str {
        "mock-tool"
    }
    fn model(&self) -> &str {
        "tool-model"
    }
    fn max_tokens(&self) -> u32 {
        1024
    }
    async fn send(&self, _req: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            Ok(MessagesResponse {
                id: "msg_tool_0".to_string(),
                content: vec![
                    ContentBlock::Text {
                        text: "Let me read that file.".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "tool_call_1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({
                            "file_path": "/nonexistent/test/path.txt"
                        }),
                    },
                ],
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 30,
                    ..Default::default()
                },
            })
        } else {
            Ok(MessagesResponse {
                id: "msg_tool_1".to_string(),
                content: vec![ContentBlock::Text {
                    text: "The file was not found. Done.".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage {
                    input_tokens: 25,
                    output_tokens: 20,
                    ..Default::default()
                },
            })
        }
    }
    async fn send_streaming(
        &self,
        req: &MessagesRequest,
        on_text: &(dyn Fn(String) + Send + Sync),
    ) -> anyhow::Result<MessagesResponse> {
        let resp = self.send(req).await?;
        for block in &resp.content {
            if let ContentBlock::Text { text } = block {
                on_text(text.clone());
            }
        }
        Ok(resp)
    }
}

#[tokio::test]
async fn test_nex_interactive_single_turn() {
    let tmp = TempDir::new().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = Box::new(EchoProvider {
        call_count: call_count.clone(),
    });

    let mut agent = AgentLoop::new(
        provider,
        ToolRegistry::new(),
        "You are a test assistant.".to_string(),
        10,
        tmp.path().join("test.ndjson"),
    );

    let result = agent.run_interactive(Some("Hello, world!")).await.unwrap();

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert!(result.final_text.contains("Echo #0"));
    assert!(result.turns >= 1);
    assert_eq!(result.total_usage.input_tokens, 10);
    assert_eq!(result.total_usage.output_tokens, 15);
}

#[tokio::test]
async fn test_nex_interactive_tool_calling() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = Box::new(ToolUseProvider {
        call_count: call_count.clone(),
    });

    let mut tools = ToolRegistry::new();
    workgraph::executor::native::tools::file::register_file_tools(&mut tools);

    let mut agent = AgentLoop::new(
        provider,
        tools,
        "You are a test assistant with file tools.".to_string(),
        10,
        tmp.path().join("test.ndjson"),
    );

    let result = agent
        .run_interactive(Some("Read /nonexistent/test/path.txt"))
        .await
        .unwrap();

    assert_eq!(call_count.load(Ordering::SeqCst), 2);
    assert!(result.final_text.contains("Done"));
    assert!(!result.tool_calls.is_empty());
    assert_eq!(result.tool_calls[0].name, "read_file");
    assert!(result.tool_calls[0].is_error);
    assert_eq!(result.total_usage.input_tokens, 45);
    assert_eq!(result.total_usage.output_tokens, 50);
}

#[tokio::test]
async fn test_nex_interactive_streaming_called() {
    let tmp = TempDir::new().unwrap();
    let streaming_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let streaming_called_clone = streaming_called.clone();

    struct StreamCheckProvider {
        streaming_called: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl Provider for StreamCheckProvider {
        fn name(&self) -> &str {
            "stream-check"
        }
        fn model(&self) -> &str {
            "stream-model"
        }
        fn max_tokens(&self) -> u32 {
            1024
        }
        async fn send(&self, _: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
            Ok(MessagesResponse {
                id: "msg".to_string(),
                content: vec![ContentBlock::Text {
                    text: "non-streaming".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
        async fn send_streaming(
            &self,
            _: &MessagesRequest,
            on_text: &(dyn Fn(String) + Send + Sync),
        ) -> anyhow::Result<MessagesResponse> {
            self.streaming_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            on_text("streamed!".to_string());
            Ok(MessagesResponse {
                id: "msg".to_string(),
                content: vec![ContentBlock::Text {
                    text: "streamed!".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
    }

    let provider = Box::new(StreamCheckProvider {
        streaming_called: streaming_called_clone,
    });

    let mut agent = AgentLoop::new(
        provider,
        ToolRegistry::new(),
        "Test.".to_string(),
        10,
        tmp.path().join("test.ndjson"),
    );

    let _result = agent.run_interactive(Some("test")).await.unwrap();
    assert!(streaming_called.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn test_nex_interactive_max_tokens_continuation() {
    let tmp = TempDir::new().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    struct MaxTokensProvider {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for MaxTokensProvider {
        fn name(&self) -> &str {
            "max-tokens"
        }
        fn model(&self) -> &str {
            "max-model"
        }
        fn max_tokens(&self) -> u32 {
            1024
        }
        async fn send(&self, _: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                Ok(MessagesResponse {
                    id: "msg_0".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "Truncated...".to_string(),
                    }],
                    stop_reason: Some(StopReason::MaxTokens),
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 100,
                        ..Default::default()
                    },
                })
            } else {
                Ok(MessagesResponse {
                    id: "msg_1".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "Continuation complete.".to_string(),
                    }],
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage {
                        input_tokens: 15,
                        output_tokens: 20,
                        ..Default::default()
                    },
                })
            }
        }
        async fn send_streaming(
            &self,
            req: &MessagesRequest,
            on_text: &(dyn Fn(String) + Send + Sync),
        ) -> anyhow::Result<MessagesResponse> {
            let resp = self.send(req).await?;
            for b in &resp.content {
                if let ContentBlock::Text { text } = b {
                    on_text(text.clone());
                }
            }
            Ok(resp)
        }
    }

    let provider = Box::new(MaxTokensProvider {
        call_count: call_count.clone(),
    });

    let mut agent = AgentLoop::new(
        provider,
        ToolRegistry::new(),
        "Test.".to_string(),
        10,
        tmp.path().join("test.ndjson"),
    );

    let result = agent
        .run_interactive(Some("generate something long"))
        .await
        .unwrap();

    assert_eq!(call_count.load(Ordering::SeqCst), 2);
    assert!(result.final_text.contains("Continuation complete"));
}

#[tokio::test]
async fn test_nex_interactive_parallel_tool_calls() {
    let tmp = TempDir::new().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    struct ParallelToolProvider {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for ParallelToolProvider {
        fn name(&self) -> &str {
            "parallel-tool"
        }
        fn model(&self) -> &str {
            "parallel-model"
        }
        fn max_tokens(&self) -> u32 {
            1024
        }
        async fn send(&self, _: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                Ok(MessagesResponse {
                    id: "msg_0".to_string(),
                    content: vec![
                        ContentBlock::ToolUse {
                            id: "call_a".to_string(),
                            name: "read_file".to_string(),
                            input: serde_json::json!({"file_path": "/tmp/a.txt"}),
                        },
                        ContentBlock::ToolUse {
                            id: "call_b".to_string(),
                            name: "read_file".to_string(),
                            input: serde_json::json!({"file_path": "/tmp/b.txt"}),
                        },
                    ],
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 30,
                        ..Default::default()
                    },
                })
            } else {
                Ok(MessagesResponse {
                    id: "msg_1".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "Read both files.".to_string(),
                    }],
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                })
            }
        }
        async fn send_streaming(
            &self,
            req: &MessagesRequest,
            on_text: &(dyn Fn(String) + Send + Sync),
        ) -> anyhow::Result<MessagesResponse> {
            let resp = self.send(req).await?;
            for b in &resp.content {
                if let ContentBlock::Text { text } = b {
                    on_text(text.clone());
                }
            }
            Ok(resp)
        }
    }

    let provider = Box::new(ParallelToolProvider {
        call_count: call_count.clone(),
    });

    let mut tools = ToolRegistry::new();
    workgraph::executor::native::tools::file::register_file_tools(&mut tools);

    let mut agent = AgentLoop::new(
        provider,
        tools,
        "Test.".to_string(),
        10,
        tmp.path().join("test.ndjson"),
    );

    let result = agent
        .run_interactive(Some("read both files"))
        .await
        .unwrap();

    assert_eq!(call_count.load(Ordering::SeqCst), 2);
    assert_eq!(result.tool_calls.len(), 2);
    assert_eq!(result.tool_calls[0].name, "read_file");
    assert_eq!(result.tool_calls[1].name, "read_file");
}

// ---------------------------------------------------------------------------
// Two-message roundtrip test (regression for wg-nex-native)
// ---------------------------------------------------------------------------

/// A conversation surface that delivers a fixed sequence of user messages,
/// then signals EOF.  Used to simulate the TUI's chat inbox in tests.
struct QueueSurface {
    messages: std::sync::Mutex<std::collections::VecDeque<String>>,
    turns_completed: Arc<AtomicUsize>,
}

impl QueueSurface {
    fn new(messages: Vec<&str>) -> Self {
        Self {
            messages: std::sync::Mutex::new(messages.into_iter().map(String::from).collect()),
            turns_completed: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl workgraph::executor::native::surface::ConversationSurface for QueueSurface {
    async fn next_user_input(
        &mut self,
    ) -> Option<workgraph::executor::native::surface::UserTurn> {
        let msg = self.messages.lock().unwrap().pop_front()?;
        Some(workgraph::executor::native::surface::UserTurn::plain(msg))
    }

    fn on_turn_end(&mut self) {
        self.turns_completed.fetch_add(1, Ordering::SeqCst);
    }

    fn stream_sink(&self) -> Arc<dyn Fn(&str) + Send + Sync> {
        Arc::new(|_| {})
    }
}

/// Regression test: two messages back-to-back against a mock OAI-compat
/// endpoint (simulated by a Provider that echoes). The second message
/// must produce a response — the bug was that the agent loop broke after
/// the first EndTurn when run through a chat surface.
#[tokio::test]
async fn test_nex_two_message_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    struct TwoTurnEchoProvider {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for TwoTurnEchoProvider {
        fn name(&self) -> &str {
            "two-turn-echo"
        }
        fn model(&self) -> &str {
            "echo-model"
        }
        fn max_tokens(&self) -> u32 {
            1024
        }
        async fn send(&self, req: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            let user_text = req
                .messages
                .iter()
                .rev()
                .find_map(|m| {
                    m.content.iter().find_map(|b| match b {
                        ContentBlock::Text { text } if m.role == workgraph::executor::native::client::Role::User => {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                })
                .unwrap_or_else(|| "no input".to_string());

            // Verify message structure: no consecutive user messages
            let mut prev_role: Option<workgraph::executor::native::client::Role> = None;
            for msg in &req.messages {
                if let Some(prev) = prev_role {
                    // Consecutive user messages are invalid in OAI format.
                    // Tool results (role=User with ToolResult blocks) legitimately
                    // follow an assistant tool_use, but two text-only user messages
                    // back-to-back should never happen.
                    let is_tool_result = msg.role == workgraph::executor::native::client::Role::User
                        && msg.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }));
                    if prev == workgraph::executor::native::client::Role::User
                        && msg.role == workgraph::executor::native::client::Role::User
                        && !is_tool_result
                    {
                        return Err(anyhow::anyhow!(
                            "Invalid message sequence: consecutive user messages at turn {}",
                            count
                        ));
                    }
                }
                prev_role = Some(msg.role);
            }

            Ok(MessagesResponse {
                id: format!("msg_echo_{}", count),
                content: vec![ContentBlock::Text {
                    text: format!("Response #{}: {}", count, user_text),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 15,
                    ..Default::default()
                },
            })
        }
        async fn send_streaming(
            &self,
            req: &MessagesRequest,
            on_text: &(dyn Fn(String) + Send + Sync),
        ) -> anyhow::Result<MessagesResponse> {
            let resp = self.send(req).await?;
            for b in &resp.content {
                if let ContentBlock::Text { text } = b {
                    on_text(text.clone());
                }
            }
            Ok(resp)
        }
    }

    let provider = Box::new(TwoTurnEchoProvider {
        call_count: call_count.clone(),
    });

    let surface = QueueSurface::new(vec!["Hello, first message", "Hello, second message"]);
    let turns_completed = surface.turns_completed.clone();

    let mut agent = AgentLoop::new(
        provider,
        ToolRegistry::new(),
        "You are a test assistant.".to_string(),
        20,
        tmp.path().join("test.ndjson"),
    )
    .with_surface(Box::new(surface));

    let result = agent.run_interactive(None).await.unwrap();

    // Both messages should have been processed
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "Expected exactly 2 LLM calls (one per user message)"
    );
    assert_eq!(
        turns_completed.load(Ordering::SeqCst),
        2,
        "Expected on_turn_end called twice"
    );
    // The final text should be from the second response
    assert!(
        result.final_text.contains("Response #1"),
        "Final text should contain second response, got: {}",
        result.final_text
    );
    assert_eq!(result.turns, 2);
}

/// Same as above but with tool use on the first turn, verifying that the
/// conversation history is valid on the second turn (tool results followed
/// by assistant response followed by new user message).
#[tokio::test]
async fn test_nex_two_message_roundtrip_with_tool_use() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));

    struct ToolThenTextProvider {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for ToolThenTextProvider {
        fn name(&self) -> &str {
            "tool-then-text"
        }
        fn model(&self) -> &str {
            "tool-model"
        }
        fn max_tokens(&self) -> u32 {
            1024
        }
        async fn send(&self, req: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);

            // Validate: no consecutive user text messages
            let mut prev_role: Option<workgraph::executor::native::client::Role> = None;
            for msg in &req.messages {
                let is_tool_result = msg.role == workgraph::executor::native::client::Role::User
                    && msg.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }));
                if let Some(prev) = prev_role {
                    if prev == workgraph::executor::native::client::Role::User
                        && msg.role == workgraph::executor::native::client::Role::User
                        && !is_tool_result
                    {
                        return Err(anyhow::anyhow!(
                            "Invalid: consecutive text-only user messages at call {}",
                            count
                        ));
                    }
                }
                prev_role = Some(msg.role);
            }

            match count {
                0 => {
                    // First turn: tool call
                    Ok(MessagesResponse {
                        id: "msg_0".to_string(),
                        content: vec![
                            ContentBlock::Text {
                                text: "Let me check.".to_string(),
                            },
                            ContentBlock::ToolUse {
                                id: "call_1".to_string(),
                                name: "read_file".to_string(),
                                input: serde_json::json!({"file_path": "/nonexistent.txt"}),
                            },
                        ],
                        stop_reason: Some(StopReason::ToolUse),
                        usage: Usage { input_tokens: 10, output_tokens: 20, ..Default::default() },
                    })
                }
                1 => {
                    // After tool result: end turn with text
                    Ok(MessagesResponse {
                        id: "msg_1".to_string(),
                        content: vec![ContentBlock::Text {
                            text: "File not found. Done with first request.".to_string(),
                        }],
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Usage { input_tokens: 15, output_tokens: 25, ..Default::default() },
                    })
                }
                2 => {
                    // Second user message
                    Ok(MessagesResponse {
                        id: "msg_2".to_string(),
                        content: vec![ContentBlock::Text {
                            text: "Got your second message!".to_string(),
                        }],
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Usage { input_tokens: 20, output_tokens: 30, ..Default::default() },
                    })
                }
                _ => {
                    Ok(MessagesResponse {
                        id: format!("msg_{}", count),
                        content: vec![ContentBlock::Text {
                            text: "unexpected call".to_string(),
                        }],
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Usage::default(),
                    })
                }
            }
        }
        async fn send_streaming(
            &self,
            req: &MessagesRequest,
            on_text: &(dyn Fn(String) + Send + Sync),
        ) -> anyhow::Result<MessagesResponse> {
            let resp = self.send(req).await?;
            for b in &resp.content {
                if let ContentBlock::Text { text } = b {
                    on_text(text.clone());
                }
            }
            Ok(resp)
        }
    }

    let provider = Box::new(ToolThenTextProvider {
        call_count: call_count.clone(),
    });

    let surface = QueueSurface::new(vec!["Check a file for me", "Thanks, now tell me a joke"]);
    let turns_completed = surface.turns_completed.clone();

    let mut tools = ToolRegistry::new();
    workgraph::executor::native::tools::file::register_file_tools(&mut tools);

    let mut agent = AgentLoop::new(
        provider,
        tools,
        "You are a test assistant.".to_string(),
        20,
        tmp.path().join("test.ndjson"),
    )
    .with_surface(Box::new(surface));

    let result = agent.run_interactive(None).await.unwrap();

    // 3 LLM calls: tool_use, end_turn after tool result, end_turn on second message
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        3,
        "Expected 3 LLM calls (tool_use + end_turn + second_message)"
    );
    assert_eq!(
        turns_completed.load(Ordering::SeqCst),
        2,
        "Expected on_turn_end called twice (once per user message)"
    );
    assert!(
        result.final_text.contains("second message"),
        "Final text should reference second message, got: {}",
        result.final_text
    );
}

// ---------------------------------------------------------------------------
// Inline-URL provider-prefix stripping (regression for wg-nex-native, attempt 3)
// ---------------------------------------------------------------------------
//
// Reproduces the user-visible failure observed in ~/autohaiku and ~/household:
//
//     $ wg init -m qwen3-coder -e https://lambda01.tail334fe6.ts.net:30000 -x nex
//     $ wg nex --no-mcp -m local:qwen3-coder -e https://lambda01.tail334fe6.ts.net:30000 hi
//     [native-agent] LLM request failed: Streaming request failed after retries:
//     API error 400: ... "LoRA adapter 'qwen3-coder' was requested, but LoRA is
//     not enabled" ...
//
// SGLang interprets `model: "<base>:<lora>"` as a LoRA adapter request. `wg
// init` stores the model as `local:qwen3-coder` (provider-prefixed canonical
// form), and the inline-URL shortcut path in `create_provider_ext` was
// passing that string to the OAI wire layer verbatim — so the request body
// carried `"model": "local:qwen3-coder"` and SGLang rejected the call as
// requesting a LoRA adapter named "qwen3-coder" on a base model "local"
// that does not exist.
//
// The fix: strip the known provider-prefix before constructing the inline-
// URL client, the same way the non-inline path already does.

/// Spin up a one-shot mock OAI-compat server that captures the raw
/// request body and replies with a small SSE stream. Returns the base
/// URL plus an Arc<Mutex<Vec<request bodies>>> the test inspects after
/// the call completes.
fn start_recording_oai_stub(num_requests: usize) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());
    let bodies: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let bodies_clone = Arc::clone(&bodies);

    thread::spawn(move || {
        for _ in 0..num_requests {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };

            // Read until we have the full body (Content-Length-driven).
            let mut buf = Vec::with_capacity(16 * 1024);
            let mut tmp = [0u8; 4096];
            let mut content_length: Option<usize> = None;
            let mut header_end: Option<usize> = None;
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if header_end.is_none() {
                            if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                header_end = Some(idx + 4);
                                let header_str =
                                    String::from_utf8_lossy(&buf[..idx]).to_lowercase();
                                for line in header_str.lines() {
                                    if let Some(rest) = line.strip_prefix("content-length:") {
                                        content_length = rest.trim().parse().ok();
                                    }
                                }
                            }
                        }
                        if let (Some(he), Some(cl)) = (header_end, content_length)
                            && buf.len() >= he + cl
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            if let Some(he) = header_end {
                let body = String::from_utf8_lossy(&buf[he..]).to_string();
                bodies_clone.lock().unwrap().push(body);
            }

            // Reply with a minimal SSE stream that the OpenAI client
            // accepts: a content delta, a finish_reason chunk, then
            // [DONE]. Usage is omitted (defaults to zero), which is
            // valid when stream_options is not set.
            let chunks = [
                r#"{"id":"x","object":"chat.completion.chunk","model":"stub","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                r#"{"id":"x","object":"chat.completion.chunk","model":"stub","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}"#,
                r#"{"id":"x","object":"chat.completion.chunk","model":"stub","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ];
            let mut sse = String::new();
            for c in &chunks {
                sse.push_str("data: ");
                sse.push_str(c);
                sse.push_str("\n\n");
            }
            sse.push_str("data: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                sse.len(),
                sse,
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (url, bodies)
}

/// Regression: `wg nex -m local:qwen3-coder -e <url>` (and its
/// programmatic equivalent through `create_provider_ext`) must send
/// `"model": "qwen3-coder"` in the OAI request body — NOT
/// `"model": "local:qwen3-coder"`. The provider-prefix is for our
/// internal routing; downstream OAI servers (SGLang, vLLM, llama.cpp,
/// Ollama) interpret a colon in the model field as a LoRA adapter
/// reference and reject the request with HTTP 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_nex_inline_url_strips_local_provider_prefix() {
    let tmp = TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();

    let (base_url, bodies) = start_recording_oai_stub(1);

    // This is exactly what `wg nex -e <url> -m local:qwen3-coder` does:
    // pass the full provider-prefixed model string and an inline URL
    // through to create_provider_ext.
    let provider = workgraph::executor::native::provider::create_provider_ext(
        tmp.path(),
        "local:qwen3-coder",
        None,
        Some(base_url.as_str()),
        None,
    )
    .expect("create_provider_ext should accept inline URL + prefixed model");

    // The agent loop uses `self.client.model().to_string()` to fill
    // `request.model` (see src/executor/native/agent.rs:1623), so this
    // is the value that actually ends up on the wire.
    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: 64,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        }],
        tools: vec![],
        stream: true,
    };

    let on_text = |_: String| {};
    let _resp = provider
        .send_streaming(&request, &on_text)
        .await
        .expect("streaming send should succeed against the stub");

    let captured = bodies.lock().unwrap();
    assert_eq!(captured.len(), 1, "stub should have received one POST");

    let body = &captured[0];
    let parsed: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|e| panic!("body not JSON ({}): {}", e, body));
    let model_field = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .expect("body must have a `model` string field");

    assert_eq!(
        model_field, "qwen3-coder",
        "inline-URL provider must strip the `local:` prefix before sending — \
         downstream OAI servers (SGLang etc.) treat a colon in the model \
         field as a LoRA adapter reference and reject the request with HTTP \
         400. Got `{}` in the request body.",
        model_field
    );
}

/// Companion regression: `oai-compat:` is the canonical alias and must
/// also be stripped on the inline-URL path. Same failure mode as
/// `local:` — any colon in the model field gets misread by downstream
/// servers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_nex_inline_url_strips_oai_compat_provider_prefix() {
    let tmp = TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();

    let (base_url, bodies) = start_recording_oai_stub(1);

    let provider = workgraph::executor::native::provider::create_provider_ext(
        tmp.path(),
        "oai-compat:llama-3-70b",
        None,
        Some(base_url.as_str()),
        None,
    )
    .expect("create_provider_ext should accept inline URL + oai-compat prefix");

    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: 64,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        }],
        tools: vec![],
        stream: true,
    };

    let _resp = provider
        .send_streaming(&request, &|_: String| {})
        .await
        .expect("streaming send should succeed against the stub");

    let body = bodies.lock().unwrap()[0].clone();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let model_field = parsed.get("model").and_then(|v| v.as_str()).unwrap();
    assert_eq!(
        model_field, "llama-3-70b",
        "oai-compat:<model> must be stripped on the inline-URL path"
    );
}

/// Regression: a bare model name (no provider prefix) should be passed
/// through unchanged on the inline-URL path. This guards against
/// over-aggressive stripping that might break the common case
/// `wg nex -m qwen3-coder-30b -e http://localhost:11434`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_nex_inline_url_passes_bare_model_through() {
    let tmp = TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();

    let (base_url, bodies) = start_recording_oai_stub(1);

    let provider = workgraph::executor::native::provider::create_provider_ext(
        tmp.path(),
        "qwen3-coder-30b",
        None,
        Some(base_url.as_str()),
        None,
    )
    .unwrap();

    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: 64,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        }],
        tools: vec![],
        stream: true,
    };

    let _resp = provider
        .send_streaming(&request, &|_: String| {})
        .await
        .unwrap();

    let body = bodies.lock().unwrap()[0].clone();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let model_field = parsed.get("model").and_then(|v| v.as_str()).unwrap();
    assert_eq!(
        model_field, "qwen3-coder-30b",
        "bare model names with no provider prefix must pass through unchanged"
    );
}

// ---------------------------------------------------------------------------
// Named-endpoint path regression: the coordinator-spawned `wg nex` does NOT
// pass `-e <url>` (the inline-URL shortcut) — instead the URL comes from the
// `[[llm_endpoints.endpoints]]` config block written by `wg init`. The path
// resolution lives in `create_provider_ext`'s named-endpoint branch (no
// `endpoint_name` override).
//
// Reproduces the user-visible failure observed in `wg tui` chat after a
// fresh `wg init -m qwen3-coder -e https://lambda01...:30000 -x nex`:
//
//     [native-agent] LLM request failed: Streaming request failed after
//     retries: API error 404: {"detail":"Not Found"}
//
// Root cause: agent-62 fixed only the inline-URL shortcut. The named-
// endpoint path still passes the URL to `with_base_url(...)` verbatim,
// without appending the `/v1` path segment that `OpenAiClient` expects
// (it constructs `{base_url}/chat/completions`). When the user's stored
// endpoint URL is `https://lambda01...:30000` the wire request goes to
// `https://lambda01...:30000/chat/completions` instead of the OAI-spec
// `/v1/chat/completions`, so SGLang/vLLM/llama.cpp respond 404.

/// Recording stub that captures the HTTP request line (method + path)
/// in addition to the body. Used by the named-endpoint URL-normalization
/// regression below; the existing `start_recording_oai_stub` only kept
/// bodies and we don't want to break its three call sites.
fn start_recording_oai_stub_with_paths(
    num_requests: usize,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<(String, String)>>>, // (request_line, body)
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());
    let captured: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);

    thread::spawn(move || {
        for _ in 0..num_requests {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };

            let mut buf = Vec::with_capacity(16 * 1024);
            let mut tmp = [0u8; 4096];
            let mut content_length: Option<usize> = None;
            let mut header_end: Option<usize> = None;
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if header_end.is_none()
                            && let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            header_end = Some(idx + 4);
                            let header_str = String::from_utf8_lossy(&buf[..idx]).to_lowercase();
                            for line in header_str.lines() {
                                if let Some(rest) = line.strip_prefix("content-length:") {
                                    content_length = rest.trim().parse().ok();
                                }
                            }
                        }
                        if let (Some(he), Some(cl)) = (header_end, content_length)
                            && buf.len() >= he + cl
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            if let Some(he) = header_end {
                // First line of the request is the request-line ("POST /v1/chat/completions HTTP/1.1").
                let header_str = String::from_utf8_lossy(&buf[..he.saturating_sub(4)]).to_string();
                let request_line = header_str.lines().next().unwrap_or("").to_string();
                let body = String::from_utf8_lossy(&buf[he..]).to_string();
                captured_clone.lock().unwrap().push((request_line, body));
            }

            let chunks = [
                r#"{"id":"x","object":"chat.completion.chunk","model":"stub","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                r#"{"id":"x","object":"chat.completion.chunk","model":"stub","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}"#,
                r#"{"id":"x","object":"chat.completion.chunk","model":"stub","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ];
            let mut sse = String::new();
            for c in &chunks {
                sse.push_str("data: ");
                sse.push_str(c);
                sse.push_str("\n\n");
            }
            sse.push_str("data: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                sse.len(),
                sse,
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (url, captured)
}

/// Regression for the `wg tui` chat 404 the user reported on autohaiku.
///
/// `wg init -e https://host:30000` stores the bare host (no `/v1`) in
/// `[[llm_endpoints.endpoints]]`. The coordinator spawns `wg nex --chat
/// .coordinator-N -m local:qwen3-coder` (no `-e`), which routes through
/// the named-endpoint branch of `create_provider_ext`. That branch must
/// normalize the endpoint URL by appending `/v1` when missing — same as
/// the inline-URL shortcut already does — otherwise `OpenAiClient`
/// posts to `{host}/chat/completions` and the OAI-compat server (SGLang
/// etc.) returns 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_named_endpoint_url_gets_v1_path_appended() {
    let tmp = TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();

    let (base_url, captured) = start_recording_oai_stub_with_paths(1);

    // Mirror what `wg init -m qwen3-coder -e <url> -x nex` writes:
    // an `[[llm_endpoints.endpoints]]` block with provider="local" and
    // the bare URL (no /v1).
    let config_toml = format!(
        r#"
[[llm_endpoints.endpoints]]
name = "default"
provider = "local"
url = "{}"
"#,
        base_url
    );
    std::fs::write(tmp.path().join("config.toml"), config_toml).unwrap();

    // Mirror what `wg spawn-task .coordinator-N` does: pass model only
    // (no endpoint override). The provider must be resolved via the
    // named-endpoint config we just wrote.
    let provider = workgraph::executor::native::provider::create_provider_ext(
        tmp.path(),
        "local:qwen3-coder",
        None,
        None, // <-- the critical bit: no -e override, must use endpoint config
        None,
    )
    .expect("create_provider_ext should resolve via named-endpoint config");

    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: 64,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        }],
        tools: vec![],
        stream: true,
    };

    let _resp = provider
        .send_streaming(&request, &|_: String| {})
        .await
        .expect("streaming send should succeed against the stub");

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1, "stub should have received one POST");
    let (request_line, body) = &captured[0];

    assert!(
        request_line.starts_with("POST /v1/chat/completions"),
        "named-endpoint path must POST to `/v1/chat/completions` (got `{}`). \
         Without the `/v1` segment the wire URL becomes `{{host}}/chat/completions`, \
         which OAI-compat servers (SGLang, vLLM, llama.cpp) answer with 404 — \
         exactly the fault the user reported in `wg tui` chat.",
        request_line
    );

    // Sanity: we still want the model field to be the prefix-stripped form.
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    let model_field = parsed.get("model").and_then(|v| v.as_str()).unwrap();
    assert_eq!(
        model_field, "qwen3-coder",
        "the named-endpoint path must also strip the `local:` prefix"
    );
}

/// Companion: if the user's endpoint URL already includes `/v1`, we must
/// NOT double it up to `/v1/v1/...`. Same idempotence the inline-URL
/// shortcut already enforces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_named_endpoint_url_with_v1_is_not_doubled() {
    let tmp = TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();

    let (base_url, captured) = start_recording_oai_stub_with_paths(1);

    // User's URL already includes /v1 — endpoint normalization must leave it alone.
    let config_toml = format!(
        r#"
[[llm_endpoints.endpoints]]
name = "default"
provider = "local"
url = "{}/v1"
"#,
        base_url
    );
    std::fs::write(tmp.path().join("config.toml"), config_toml).unwrap();

    let provider = workgraph::executor::native::provider::create_provider_ext(
        tmp.path(),
        "local:qwen3-coder",
        None,
        None,
        None,
    )
    .unwrap();

    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: 64,
        system: None,
        messages: vec![workgraph::executor::native::client::Message {
            role: workgraph::executor::native::client::Role::User,
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        }],
        tools: vec![],
        stream: true,
    };

    let _resp = provider
        .send_streaming(&request, &|_: String| {})
        .await
        .unwrap();

    let captured = captured.lock().unwrap();
    let (request_line, _) = &captured[0];
    assert!(
        request_line.starts_with("POST /v1/chat/completions")
            && !request_line.contains("/v1/v1/"),
        "endpoint URL already ending in /v1 must not double the segment (got `{}`)",
        request_line
    );
}
