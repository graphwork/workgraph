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
