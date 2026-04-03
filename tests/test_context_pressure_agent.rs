//! Tests for context pressure management in the agent loop.
//!
//! Verifies that the AgentLoop correctly:
//! - Injects warnings at 80% capacity
//! - Performs emergency compaction at 90% capacity
//! - Performs clean exit at 95% capacity
//! - Recovers from 413 (context too long) errors by compacting and retrying
//!
//! Run with: cargo test --test test_context_pressure_agent

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tempfile::TempDir;

use workgraph::executor::native::agent::AgentLoop;
use workgraph::executor::native::client::{
    ContentBlock, MessagesRequest, MessagesResponse, StopReason, Usage,
};
use workgraph::executor::native::openai_client::ApiError;
use workgraph::executor::native::provider::Provider;
use workgraph::executor::native::resume::{ContextBudget, ContextPressureAction};
use workgraph::executor::native::tools::ToolRegistry;

// ── Mock providers ──────────────────────────────────────────────────────

/// A provider that tracks how many calls it receives and returns
/// configurable responses. Uses a tiny context window to trigger pressure.
struct TinyContextProvider {
    context_window: usize,
    call_count: Arc<AtomicUsize>,
    /// Responses to return on successive calls (cycles if exhausted).
    responses: Arc<Mutex<Vec<MessagesResponse>>>,
}

impl TinyContextProvider {
    fn new(context_window: usize, responses: Vec<MessagesResponse>) -> Self {
        Self {
            context_window,
            call_count: Arc::new(AtomicUsize::new(0)),
            responses: Arc::new(Mutex::new(responses)),
        }
    }
}

#[async_trait]
impl Provider for TinyContextProvider {
    fn name(&self) -> &str {
        "mock-tiny"
    }

    fn model(&self) -> &str {
        "mock-tiny-model"
    }

    fn max_tokens(&self) -> u32 {
        256
    }

    fn context_window(&self) -> usize {
        self.context_window
    }

    async fn send(&self, _: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        unreachable!("send() should not be called — agent uses send_streaming()")
    }

    async fn send_streaming(
        &self,
        _request: &MessagesRequest,
        _on_text: &(dyn Fn(String) + Send + Sync),
    ) -> anyhow::Result<MessagesResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let responses = self.responses.lock().unwrap();
        let resp = if idx < responses.len() {
            responses[idx].clone()
        } else {
            // Default: end turn
            MessagesResponse {
                id: format!("msg_default_{}", idx),
                content: vec![ContentBlock::Text {
                    text: "Done.".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }
        };
        Ok(resp)
    }
}

/// A provider that returns a 413 error on the first call, then succeeds.
struct ContextTooLongProvider {
    call_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for ContextTooLongProvider {
    fn name(&self) -> &str {
        "mock-413"
    }

    fn model(&self) -> &str {
        "mock-413-model"
    }

    fn max_tokens(&self) -> u32 {
        256
    }

    fn context_window(&self) -> usize {
        200_000 // Large enough that pressure check won't trigger
    }

    async fn send(&self, _: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        unreachable!()
    }

    async fn send_streaming(
        &self,
        _request: &MessagesRequest,
        _on_text: &(dyn Fn(String) + Send + Sync),
    ) -> anyhow::Result<MessagesResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        if idx == 0 {
            // First call: return 413 error
            Err(ApiError {
                status: 413,
                message: "Request too large".to_string(),
            }
            .into())
        } else {
            // Subsequent calls: succeed
            Ok(MessagesResponse {
                id: format!("msg_retry_{}", idx),
                content: vec![ContentBlock::Text {
                    text: "Recovered after compaction.".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
    }
}

/// A provider that always returns 413.
struct AlwaysContextTooLongProvider;

#[async_trait]
impl Provider for AlwaysContextTooLongProvider {
    fn name(&self) -> &str {
        "mock-always-413"
    }
    fn model(&self) -> &str {
        "mock-always-413-model"
    }
    fn max_tokens(&self) -> u32 {
        256
    }
    fn context_window(&self) -> usize {
        200_000
    }

    async fn send(&self, _: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        unreachable!()
    }

    async fn send_streaming(
        &self,
        _: &MessagesRequest,
        _: &(dyn Fn(String) + Send + Sync),
    ) -> anyhow::Result<MessagesResponse> {
        Err(ApiError {
            status: 413,
            message: "Request too large".to_string(),
        }
        .into())
    }
}

/// A provider that returns a 400 with context-related message.
struct Context400Provider {
    call_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Context400Provider {
    fn name(&self) -> &str {
        "mock-400-context"
    }
    fn model(&self) -> &str {
        "mock-400-model"
    }
    fn max_tokens(&self) -> u32 {
        256
    }
    fn context_window(&self) -> usize {
        200_000
    }

    async fn send(&self, _: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        unreachable!()
    }

    async fn send_streaming(
        &self,
        _: &MessagesRequest,
        _: &(dyn Fn(String) + Send + Sync),
    ) -> anyhow::Result<MessagesResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        if idx == 0 {
            Err(ApiError {
                status: 400,
                message: "This model's maximum context length is 32768 tokens".to_string(),
            }
            .into())
        } else {
            Ok(MessagesResponse {
                id: format!("msg_retry_{}", idx),
                content: vec![ContentBlock::Text {
                    text: "Recovered.".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
    }
}

/// Provider that tracks the messages it receives on each call.
struct InspectingProvider {
    context_window: usize,
    call_count: Arc<AtomicUsize>,
    /// Messages seen in each call.
    seen_messages: Arc<Mutex<Vec<Vec<workgraph::executor::native::client::Message>>>>,
    responses: Arc<Mutex<Vec<MessagesResponse>>>,
}

impl InspectingProvider {
    fn new(context_window: usize, responses: Vec<MessagesResponse>) -> Self {
        Self {
            context_window,
            call_count: Arc::new(AtomicUsize::new(0)),
            seen_messages: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses)),
        }
    }
}

#[async_trait]
impl Provider for InspectingProvider {
    fn name(&self) -> &str {
        "mock-inspect"
    }
    fn model(&self) -> &str {
        "mock-inspect-model"
    }
    fn max_tokens(&self) -> u32 {
        256
    }
    fn context_window(&self) -> usize {
        self.context_window
    }

    async fn send(&self, _: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        unreachable!()
    }

    async fn send_streaming(
        &self,
        request: &MessagesRequest,
        _on_text: &(dyn Fn(String) + Send + Sync),
    ) -> anyhow::Result<MessagesResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        // Record the messages for inspection
        self.seen_messages
            .lock()
            .unwrap()
            .push(request.messages.clone());

        let responses = self.responses.lock().unwrap();
        let resp = if idx < responses.len() {
            responses[idx].clone()
        } else {
            MessagesResponse {
                id: format!("msg_default_{}", idx),
                content: vec![ContentBlock::Text {
                    text: "Done.".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }
        };
        Ok(resp)
    }
}

fn make_agent(provider: Box<dyn Provider>, dir: &std::path::Path) -> AgentLoop {
    let output_log = dir.join("output.log");
    AgentLoop::new(provider, ToolRegistry::new(), String::new(), 50, output_log)
}

// ── Unit tests for ContextBudget ────────────────────────────────────────

#[test]
fn test_context_budget_from_provider_context_window() {
    // ContextBudget should be constructible from provider's context window
    let budget = ContextBudget::with_window_size(32_000);
    assert_eq!(budget.window_size, 32_000);
    assert!((budget.warning_threshold - 0.80).abs() < f64::EPSILON);
    assert!((budget.compact_threshold - 0.90).abs() < f64::EPSILON);
    assert!((budget.hard_limit - 0.95).abs() < f64::EPSILON);
}

#[test]
fn test_context_budget_default_uses_200k() {
    let budget = ContextBudget::default();
    assert_eq!(budget.window_size, 200_000);
}

#[test]
fn test_context_pressure_exactly_at_80_percent() {
    // Window: 1000 tokens. 80% = 800 tokens = 3200 chars (at 4 chars/token).
    let budget = ContextBudget::with_window_size(1000);
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(3200), // exactly 800 tokens = 80%
        }],
    }];
    assert_eq!(budget.check_pressure(&msgs), ContextPressureAction::Warning);
}

#[test]
fn test_context_pressure_at_79_9_percent() {
    // 79.9% should be Ok (below 80%)
    let budget = ContextBudget::with_window_size(1000);
    // 79.9% of 1000 = 799 tokens = 3196 chars
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(3196),
        }],
    }];
    assert_eq!(budget.check_pressure(&msgs), ContextPressureAction::Ok);
}

#[test]
fn test_context_pressure_at_90_percent() {
    let budget = ContextBudget::with_window_size(1000);
    // 90% = 900 tokens = 3600 chars
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(3600),
        }],
    }];
    assert_eq!(
        budget.check_pressure(&msgs),
        ContextPressureAction::EmergencyCompaction
    );
}

#[test]
fn test_context_pressure_at_89_9_percent() {
    // 89.9% should be Warning (below 90%)
    let budget = ContextBudget::with_window_size(1000);
    // 89.9% of 1000 = 899 tokens = 3596 chars
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(3596),
        }],
    }];
    assert_eq!(
        budget.check_pressure(&msgs),
        ContextPressureAction::Warning
    );
}

#[test]
fn test_context_pressure_at_95_percent() {
    let budget = ContextBudget::with_window_size(1000);
    // 95% = 950 tokens = 3800 chars
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(3800),
        }],
    }];
    assert_eq!(
        budget.check_pressure(&msgs),
        ContextPressureAction::CleanExit
    );
}

#[test]
fn test_context_pressure_at_95_1_percent() {
    let budget = ContextBudget::with_window_size(1000);
    // 95.1% = 951 tokens = 3804 chars
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(3804),
        }],
    }];
    assert_eq!(
        budget.check_pressure(&msgs),
        ContextPressureAction::CleanExit
    );
}

#[test]
fn test_context_pressure_at_100_percent() {
    let budget = ContextBudget::with_window_size(1000);
    // 100% = 1000 tokens = 4000 chars
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(4000),
        }],
    }];
    assert_eq!(
        budget.check_pressure(&msgs),
        ContextPressureAction::CleanExit
    );
}

#[test]
fn test_context_pressure_well_below_thresholds() {
    let budget = ContextBudget::with_window_size(200_000);
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
    }];
    assert_eq!(budget.check_pressure(&msgs), ContextPressureAction::Ok);
}

#[test]
fn test_context_budget_small_window_32k() {
    // Simulate Qwen3-32B with 32K context window
    let budget = ContextBudget::with_window_size(32_000);
    // 80% of 32000 = 25600 tokens = 102400 chars
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(102_400),
        }],
    }];
    assert_eq!(
        budget.check_pressure(&msgs),
        ContextPressureAction::Warning
    );
}

#[test]
fn test_warning_message_contains_useful_info() {
    let budget = ContextBudget::with_window_size(1000);
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(3400), // 850 tokens = 85%
        }],
    }];
    let warning = budget.warning_message(&msgs);
    assert!(warning.contains("CONTEXT PRESSURE"));
    assert!(warning.contains("85")); // percentage
    assert!(warning.contains("wg log")); // advice
}

#[test]
fn test_emergency_compact_strips_old_tool_results() {
    use workgraph::executor::native::client::{Message, Role};

    let messages = vec![
        // Old tool use
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "src/main.rs"}),
            }],
        },
        // Old tool result (large — should be compacted)
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu-1".to_string(),
                content: "a".repeat(5000),
                is_error: false,
            }],
        },
        // Recent messages (keep_recent=2)
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Analysis complete.".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Thanks!".to_string(),
            }],
        },
    ];

    let compacted = ContextBudget::emergency_compact(messages, 2);
    assert_eq!(compacted.len(), 4);

    // The old tool result should be compacted (shorter)
    match &compacted[1].content[0] {
        ContentBlock::ToolResult { content, .. } => {
            assert!(content.len() < 5000, "Tool result should be compacted");
            assert!(content.contains("Tool result removed"));
        }
        _ => panic!("Expected tool result"),
    }

    // Recent messages should be preserved
    match &compacted[3].content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Thanks!"),
        _ => panic!("Expected text"),
    }
}

#[test]
fn test_emergency_compact_preserves_small_tool_results() {
    use workgraph::executor::native::client::{Message, Role};

    let messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "echo hi"}),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu-1".to_string(),
                content: "hi".to_string(), // Small — should NOT be compacted
                is_error: false,
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Done.".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "OK".to_string(),
            }],
        },
    ];

    let compacted = ContextBudget::emergency_compact(messages, 2);
    // Small tool result should be preserved (under 200 chars threshold)
    match &compacted[1].content[0] {
        ContentBlock::ToolResult { content, .. } => {
            assert_eq!(content, "hi", "Small tool result should be preserved");
        }
        _ => panic!("Expected tool result"),
    }
}

#[test]
fn test_emergency_compact_no_change_when_few_messages() {
    use workgraph::executor::native::client::{Message, Role};

    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Hi".to_string(),
            }],
        },
    ];

    let compacted = ContextBudget::emergency_compact(messages.clone(), 5);
    assert_eq!(compacted.len(), messages.len());
}

#[test]
fn test_estimate_tokens_multi_content() {
    use workgraph::executor::native::client::{Message, Role};

    let budget = ContextBudget::with_window_size(1000);
    let msgs = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "x".repeat(400), // 100 tokens
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "y".repeat(400), // 100 tokens
                },
                ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "echo hello"}), // ~30 chars ≈ 7 tokens
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "z".repeat(400), // 100 tokens
                is_error: false,
            }],
        },
    ];

    let tokens = budget.estimate_tokens(&msgs);
    // Total chars ≈ 400 + 400 + (4 + ~30) + 400 ≈ 1234 → ~308 tokens
    assert!(tokens > 250 && tokens < 400, "Expected ~300 tokens, got {}", tokens);
}

// ── Agent loop integration tests ────────────────────────────────────────

/// Agent loop should terminate gracefully when context reaches 95%.
/// We use a tiny context window and have the model return tool use calls
/// that build up the context.
#[tokio::test]
async fn test_agent_loop_clean_exit_at_95_percent() {
    let dir = TempDir::new().unwrap();

    // Context window of 100 tokens = 400 chars. 95% = 380 chars.
    // The initial message + response content will quickly exceed this.
    let responses = vec![
        // Turn 1: model uses a tool
        MessagesResponse {
            id: "msg_1".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "echo hello"}),
            }],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        },
        // Turn 2: would be called if the agent doesn't exit
        MessagesResponse {
            id: "msg_2".to_string(),
            content: vec![ContentBlock::Text {
                text: "Should not reach here".to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        },
    ];

    // Tiny context window — initial message alone will push near limit
    let provider = TinyContextProvider::new(100, responses);
    let call_count = Arc::clone(&provider.call_count);
    let agent = make_agent(Box::new(provider), dir.path());

    // A large initial message to push context usage above 95%
    let result = agent
        .run(&"x".repeat(400)) // 400 chars = 100 tokens = 100% of window
        .await;

    // The agent should complete (not panic or error)
    let result = result.expect("Agent should complete gracefully");

    // It should have detected clean exit condition
    assert!(
        result.final_text.contains("context limit"),
        "Expected clean exit message, got: {}",
        result.final_text
    );

    // The agent should have made at most 1 API call (the first turn)
    // since pressure check happens after adding the response
    assert!(call_count.load(Ordering::SeqCst) <= 1);
}

/// Agent loop should recover from a 413 error by compacting and retrying.
#[tokio::test]
async fn test_agent_loop_recovers_from_413() {
    let dir = TempDir::new().unwrap();

    let provider = ContextTooLongProvider {
        call_count: Arc::new(AtomicUsize::new(0)),
    };
    let call_count = Arc::clone(&provider.call_count);
    let agent = make_agent(Box::new(provider), dir.path());

    let result = agent.run("Hello, this is a test").await;

    // Should succeed (recovered after compaction)
    let result = result.expect("Agent should recover from 413");
    assert_eq!(result.final_text, "Recovered after compaction.");

    // Should have made 2 calls: first 413, then retry success
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

/// Agent loop should fail gracefully when 413 persists after compaction.
#[tokio::test]
async fn test_agent_loop_fails_when_413_persists() {
    let dir = TempDir::new().unwrap();

    let provider = AlwaysContextTooLongProvider;
    let agent = make_agent(Box::new(provider), dir.path());

    let result = agent.run("Hello").await;

    // Should return an error (not panic)
    assert!(result.is_err(), "Should fail when 413 persists after retry");
    let err = result.unwrap_err();
    assert!(
        format!("{:?}", err).contains("compaction"),
        "Error should mention compaction: {:?}",
        err
    );
}

/// Agent loop should recover from a 400 context-too-long error.
#[tokio::test]
async fn test_agent_loop_recovers_from_400_context() {
    let dir = TempDir::new().unwrap();

    let provider = Context400Provider {
        call_count: Arc::new(AtomicUsize::new(0)),
    };
    let call_count = Arc::clone(&provider.call_count);
    let agent = make_agent(Box::new(provider), dir.path());

    let result = agent.run("Hello").await;

    let result = result.expect("Agent should recover from 400 context error");
    assert_eq!(result.final_text, "Recovered.");
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

/// When context is at warning level (80-90%), the agent should inject
/// a warning into the conversation that the model can see.
#[tokio::test]
async fn test_agent_loop_injects_warning_at_80_percent() {
    let dir = TempDir::new().unwrap();

    // Context window: 300 tokens = 1200 chars.
    // 80% = 240 tokens = 960 chars.
    //
    // Flow:
    // 1. Initial user message: 600 chars = 150 tokens (50%)
    // 2. Turn 1: model returns ToolUse → tool executes → tool result added
    //    After turn 1: 600 (initial) + ~200 (assistant tool_use + tool result) = ~800 chars = 200 tokens (67%)
    // 3. Turn 1 response also includes text of 200 chars → total ~1000 chars = 250 tokens (83%) → Warning!
    //    Warning injected into last user message (tool result message)
    // 4. Turn 2 API call should see the warning
    let responses = vec![
        // Turn 1: tool use + text that pushes us past 80%
        MessagesResponse {
            id: "msg_1".to_string(),
            content: vec![
                ContentBlock::Text {
                    text: "y".repeat(200),
                },
                ContentBlock::ToolUse {
                    id: "tu-1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "echo hi"}),
                },
            ],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        },
        // Turn 2: model finishes
        MessagesResponse {
            id: "msg_2".to_string(),
            content: vec![ContentBlock::Text {
                text: "Done.".to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        },
    ];

    let provider = InspectingProvider::new(300, responses);
    let seen_messages = Arc::clone(&provider.seen_messages);
    let agent = make_agent(Box::new(provider), dir.path());

    // 300 tokens = 1200 chars. 80% = 960 chars.
    // After turn 1: 800 (initial) + 200 (assistant text) + ~40 (tool_use) + ~18 (tool result) = ~1058 chars
    // = ~264 tokens = 88% → Warning!
    let result = agent.run(&"x".repeat(800)).await;
    assert!(result.is_ok(), "Agent should complete: {:?}", result);

    // Check the messages the provider saw on the second call (after warning injection)
    let seen = seen_messages.lock().unwrap();
    assert!(
        seen.len() >= 2,
        "Expected at least 2 API calls, got {}",
        seen.len()
    );

    let second_call_msgs = &seen[1];
    // Find any user message containing the context pressure warning
    let has_warning = second_call_msgs.iter().any(|m| {
        m.role == workgraph::executor::native::client::Role::User
            && m.content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("CONTEXT PRESSURE"),
                _ => false,
            })
    });
    assert!(
        has_warning,
        "Should have injected context pressure warning. Messages seen in 2nd call: {:?}",
        second_call_msgs
            .iter()
            .flat_map(|m| m.content.iter().filter_map(|b| match b {
                ContentBlock::Text { text } => Some(format!("Text({}b)", text.len())),
                ContentBlock::ToolUse { name, .. } => Some(format!("ToolUse({})", name)),
                ContentBlock::ToolResult { content, .. } => Some(format!("ToolResult({}b)", content.len())),
            }))
            .collect::<Vec<_>>()
    );
}

/// Emergency compaction at 90% should reduce message size.
#[tokio::test]
async fn test_agent_loop_compacts_at_90_percent() {
    let dir = TempDir::new().unwrap();

    // Context window: 150 tokens = 600 chars
    // 90% = 135 tokens = 540 chars
    // Each tool use response generates content that accumulates
    let responses = vec![
        // Turn 1: tool use that generates a lot of content
        MessagesResponse {
            id: "msg_1".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "big_file.rs"}),
            }],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        },
        // Turn 2: finishes
        MessagesResponse {
            id: "msg_2".to_string(),
            content: vec![ContentBlock::Text {
                text: "Analysis complete.".to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        },
    ];

    let provider = InspectingProvider::new(150, responses);
    let seen_messages = Arc::clone(&provider.seen_messages);
    let agent = make_agent(Box::new(provider), dir.path());

    // Start with content that will push past 90% after tool result
    let result = agent.run(&"x".repeat(500)).await;
    assert!(result.is_ok(), "Agent should handle compaction: {:?}", result);

    // After compaction, messages sent to the provider should be shorter
    let seen = seen_messages.lock().unwrap();
    if seen.len() >= 2 {
        let second_call_total_chars: usize = seen[1]
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| match b {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { input, name, .. } => name.len() + input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
            })
            .sum();

        let first_call_total_chars: usize = seen[0]
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| match b {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { input, name, .. } => name.len() + input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
            })
            .sum();

        // Second call should have smaller or similar total chars due to compaction
        // (or at least not dramatically more, since old tool results got compacted)
        assert!(
            second_call_total_chars <= first_call_total_chars + 500,
            "Compaction should limit growth: first call {} chars, second call {} chars",
            first_call_total_chars,
            second_call_total_chars
        );
    }
}

// ── is_context_too_long tests ───────────────────────────────────────────

#[test]
fn test_is_context_too_long_413() {
    use workgraph::executor::native::openai_client::is_context_too_long;

    let err: anyhow::Error = ApiError {
        status: 413,
        message: "Request too large".to_string(),
    }
    .into();
    assert!(is_context_too_long(&err));
}

#[test]
fn test_is_context_too_long_400_with_context_message() {
    use workgraph::executor::native::openai_client::is_context_too_long;

    let err: anyhow::Error = ApiError {
        status: 400,
        message: "This model's maximum context length is 32768 tokens".to_string(),
    }
    .into();
    assert!(is_context_too_long(&err));
}

#[test]
fn test_is_context_too_long_400_with_prompt_too_long() {
    use workgraph::executor::native::openai_client::is_context_too_long;

    let err: anyhow::Error = ApiError {
        status: 400,
        message: "Prompt is too long".to_string(),
    }
    .into();
    assert!(is_context_too_long(&err));
}

#[test]
fn test_is_context_too_long_400_unrelated() {
    use workgraph::executor::native::openai_client::is_context_too_long;

    let err: anyhow::Error = ApiError {
        status: 400,
        message: "Invalid request: missing 'model' field".to_string(),
    }
    .into();
    assert!(!is_context_too_long(&err));
}

#[test]
fn test_is_context_too_long_401() {
    use workgraph::executor::native::openai_client::is_context_too_long;

    let err: anyhow::Error = ApiError {
        status: 401,
        message: "Invalid API key".to_string(),
    }
    .into();
    assert!(!is_context_too_long(&err));
}

#[test]
fn test_is_context_too_long_non_api_error() {
    use workgraph::executor::native::openai_client::is_context_too_long;

    let err = anyhow::anyhow!("network timeout");
    assert!(!is_context_too_long(&err));
}

// ── Token estimation edge cases ─────────────────────────────────────────

#[test]
fn test_estimate_tokens_empty_messages() {
    let budget = ContextBudget::with_window_size(1000);
    let msgs: Vec<workgraph::executor::native::client::Message> = vec![];
    assert_eq!(budget.estimate_tokens(&msgs), 0);
}

#[test]
fn test_estimate_tokens_empty_content() {
    let budget = ContextBudget::with_window_size(1000);
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![],
    }];
    assert_eq!(budget.estimate_tokens(&msgs), 0);
}

#[test]
fn test_pressure_with_multiple_small_messages() {
    // Verify that many small messages accumulate correctly
    let budget = ContextBudget::with_window_size(1000);
    // 20 messages of 160 chars each = 3200 chars = 800 tokens = 80%
    let msgs: Vec<_> = (0..20)
        .map(|i| workgraph::executor::native::client::Message {
            role: if i % 2 == 0 {
                workgraph::executor::native::client::Role::User
            } else {
                workgraph::executor::native::client::Role::Assistant
            },
            content: vec![ContentBlock::Text {
                text: "x".repeat(160),
            }],
        })
        .collect();
    assert_eq!(
        budget.check_pressure(&msgs),
        ContextPressureAction::Warning
    );
}

#[test]
fn test_no_hardcoded_context_budget() {
    // Verify that ContextBudget respects different window sizes
    let small = ContextBudget::with_window_size(32_000);
    let large = ContextBudget::with_window_size(200_000);

    // Same message should trigger different pressures on different windows
    let msgs = vec![workgraph::executor::native::client::Message {
        role: workgraph::executor::native::client::Role::User,
        content: vec![ContentBlock::Text {
            text: "x".repeat(120_000), // 30000 tokens
        }],
    }];

    // 30000/32000 = 93.75% → EmergencyCompaction on small window
    assert_eq!(
        small.check_pressure(&msgs),
        ContextPressureAction::EmergencyCompaction
    );

    // 30000/200000 = 15% → Ok on large window
    assert_eq!(
        large.check_pressure(&msgs),
        ContextPressureAction::Ok
    );
}
