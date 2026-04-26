//! Tests for the nex interactive REPL.
//!
//! Uses mock providers to verify multi-turn conversation, tool calling,
//! and streaming behavior in the interactive agent loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
