//! Integration tests for session summary extraction and resume.

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use tempfile::TempDir;

use workgraph::executor::native::agent::{AgentLoop, DEFAULT_SUMMARY_INTERVAL_TURNS};
use workgraph::executor::native::client::{
    ContentBlock, Message, MessagesRequest, MessagesResponse, Role, StopReason, Usage,
};
use workgraph::executor::native::provider::Provider;
use workgraph::executor::native::resume::{
    extract_session_summary, load_session_summary, store_session_summary,
};
use workgraph::executor::native::tools::ToolRegistry;

/// A mock provider that returns a configurable sequence of responses.
struct MockProvider {
    model_name: String,
    call_count: AtomicUsize,
    /// If true, first call returns tool use, second returns end turn.
    multi_turn: bool,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            model_name: "test-model".to_string(),
            call_count: AtomicUsize::new(0),
            multi_turn: false,
        }
    }

    #[allow(dead_code)]
    fn multi_turn() -> Self {
        Self {
            model_name: "test-model".to_string(),
            call_count: AtomicUsize::new(0),
            multi_turn: true,
        }
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn model(&self) -> &str {
        &self.model_name
    }

    fn max_tokens(&self) -> u32 {
        1024
    }

    async fn send(
        &self,
        _request: &MessagesRequest,
    ) -> anyhow::Result<MessagesResponse> {
        let count = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        if self.multi_turn && count == 0 {
            // First call: return a tool use (simulates multi-turn)
            Ok(MessagesResponse {
                id: format!("resp-{}", count),
                content: vec![ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "echo hello"}),
                }],
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
            })
        } else {
            // Return a final text response
            Ok(MessagesResponse {
                id: format!("resp-{}", count),
                content: vec![ContentBlock::Text {
                    text: format!("Final response #{}", count),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
            })
        }
    }
}

fn make_test_agent(temp_dir: &TempDir, summary_path: Option<PathBuf>) -> AgentLoop {
    let tools = ToolRegistry::new();
    let provider = Box::new(MockProvider::new());
    let output_log = temp_dir.path().join("output.jsonl");

    let mut agent = AgentLoop::new(
        provider,
        tools,
        "You are a test agent.".to_string(),
        100, // max turns
        output_log,
    );

    if let Some(path) = summary_path {
        agent = agent.with_session_summary_path(path);
    }

    agent
}

// ── Extraction tests ──────────────────────────────────────────────────

#[test]
fn test_session_summary_extraction() {
    // Build a realistic set of messages
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Implement the config parser".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "I decided to use TOML for the config format. I will modify src/config.rs."
                    .to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "write_file".to_string(),
                input: serde_json::json!({"path": "src/config.rs", "content": "use toml;"}),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu-1".to_string(),
                content: "File written successfully".to_string(),
                is_error: false,
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu-2".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "cargo build"}),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu-2".to_string(),
                content: "Build successful".to_string(),
                is_error: false,
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "ERROR: Missing import for serde".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu-3".to_string(),
                name: "edit_file".to_string(),
                input: serde_json::json!({"path": "src/lib.rs", "old": "use toml;", "new": "use toml;\nuse serde;"}),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu-3".to_string(),
                content: "File edited".to_string(),
                is_error: false,
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Fixed the import. The config parser is now complete.".to_string(),
            }],
        },
    ];

    let summary = extract_session_summary(&messages);

    // Verify structure
    assert!(summary.contains("# Session Summary"), "Missing header");
    assert!(
        summary.contains("Files Modified"),
        "Missing files modified section"
    );
    assert!(
        summary.contains("src/config.rs"),
        "Missing config.rs in files"
    );
    assert!(summary.contains("src/lib.rs"), "Missing lib.rs in files");
    assert!(
        summary.contains("Tool Usage"),
        "Missing tool usage section"
    );
    assert!(summary.contains("write_file"), "Missing write_file tool");
    assert!(summary.contains("bash"), "Missing bash tool");

    // Verify word count is within limit
    let word_count = summary.split_whitespace().count();
    assert!(
        word_count <= 500,
        "Summary exceeds 500 words: {} words",
        word_count
    );

    // Verify key findings contains the error
    assert!(
        summary.contains("ERROR") || summary.contains("Key Findings"),
        "Should capture errors as findings"
    );

    // Verify decisions are extracted
    assert!(
        summary.contains("decided") || summary.contains("Decisions"),
        "Should capture decisions"
    );
}

#[test]
fn test_session_summary_extraction_empty() {
    let messages: Vec<Message> = vec![];
    let summary = extract_session_summary(&messages);
    assert!(summary.contains("# Session Summary"));
    let word_count = summary.split_whitespace().count();
    assert!(word_count <= 500);
}

#[test]
fn test_session_summary_extraction_word_limit() {
    // Create a huge conversation to test truncation
    let mut messages = Vec::new();
    for i in 0..100 {
        messages.push(Message {
            role: if i % 2 == 0 { Role::User } else { Role::Assistant },
            content: vec![ContentBlock::Text {
                text: format!(
                    "Found important finding number {}. This is a long message with many words \
                     to test the word limit enforcement. The agent decided to take approach {} \
                     which will affect many files in the project.",
                    i, i
                ),
            }],
        });
    }

    let summary = extract_session_summary(&messages);
    let word_count = summary.split_whitespace().count();
    assert!(
        word_count <= 501, // 500 + possible "[...truncated]"
        "Summary should be capped at ~500 words, got {}",
        word_count
    );
}

// ── Storage tests ─────────────────────────────────────────────────────

#[test]
fn test_session_summary_store_and_load() {
    let temp_dir = TempDir::new().unwrap();
    let summary_path = temp_dir
        .path()
        .join("agents")
        .join("agent-123")
        .join("session-summary.md");

    let summary_text = "# Session Summary\n\n## Key Findings\n- Found a bug in config.rs\n";

    // Store
    store_session_summary(&summary_path, summary_text).unwrap();
    assert!(summary_path.exists(), "Summary file should be created");

    // Load
    let loaded = load_session_summary(&summary_path).unwrap();
    assert_eq!(loaded, Some(summary_text.to_string()));
}

#[test]
fn test_session_summary_load_nonexistent() {
    let result = load_session_summary(std::path::Path::new("/nonexistent/summary.md")).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_session_summary_load_empty() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("empty-summary.md");
    std::fs::write(&path, "").unwrap();

    let result = load_session_summary(&path).unwrap();
    assert!(result.is_none(), "Empty summary should return None");
}

#[test]
fn test_session_summary_store_creates_dirs() {
    let temp_dir = TempDir::new().unwrap();
    let summary_path = temp_dir
        .path()
        .join("deep")
        .join("nested")
        .join("agents")
        .join("summary.md");

    store_session_summary(&summary_path, "test").unwrap();
    assert!(summary_path.exists());
}

// ── Resume integration tests ──────────────────────────────────────────

#[tokio::test]
async fn test_session_summary_resume() {
    let temp_dir = TempDir::new().unwrap();
    let summary_path = temp_dir.path().join("session-summary.md");

    // Pre-write a session summary
    let prior_summary =
        "# Session Summary\n\n## Key Findings\n- Config parser works\n\n## Files Modified\n- `src/config.rs`\n";
    store_session_summary(&summary_path, prior_summary).unwrap();

    // Create an agent that has the summary path configured
    let provider = Box::new(MockProvider::new());
    let tools = ToolRegistry::new();
    let output_log = temp_dir.path().join("output.jsonl");

    let agent = AgentLoop::new(
        provider,
        tools,
        "You are a test agent.".to_string(),
        1, // max 1 turn
        output_log,
    )
    .with_session_summary_path(summary_path.clone())
    .with_resume(true);

    // Run the agent — it should load the session summary and include it in messages
    let result = agent.run("Continue the task").await.unwrap();

    // The agent should have completed (mock provider returns EndTurn)
    assert_eq!(result.turns, 1);

    // After running, the summary file should be updated with the new session state
    let updated_summary = load_session_summary(&summary_path).unwrap();
    assert!(
        updated_summary.is_some(),
        "Summary should be stored after agent run"
    );
}

// ── Builder tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_session_summary_path_builder() {
    let temp_dir = TempDir::new().unwrap();
    let summary_path = temp_dir.path().join("test-summary.md");

    let agent = make_test_agent(&temp_dir, Some(summary_path.clone()));
    assert_eq!(agent.session_summary_path(), Some(&summary_path));
}

#[tokio::test]
async fn test_session_summary_default_disabled() {
    let temp_dir = TempDir::new().unwrap();
    let agent = make_test_agent(&temp_dir, None);

    assert_eq!(agent.summary_interval_turns(), DEFAULT_SUMMARY_INTERVAL_TURNS);
    assert!(agent.session_summary_path().is_none());
}

#[tokio::test]
async fn test_session_summary_interval_builder() {
    let temp_dir = TempDir::new().unwrap();
    let agent = make_test_agent(&temp_dir, None).with_summary_interval(5);
    assert_eq!(agent.summary_interval_turns(), 5);
}

// ── End-to-end: agent run stores summary ──────────────────────────────

#[tokio::test]
async fn test_session_summary_stored_after_run() {
    let temp_dir = TempDir::new().unwrap();
    let summary_path = temp_dir.path().join("session-summary.md");

    let provider = Box::new(MockProvider::new());
    let tools = ToolRegistry::new();
    let output_log = temp_dir.path().join("output.jsonl");

    let agent = AgentLoop::new(
        provider,
        tools,
        "You are a test agent.".to_string(),
        5,
        output_log,
    )
    .with_session_summary_path(summary_path.clone())
    .with_summary_interval(1); // extract every turn for testing

    let result = agent.run("Do something").await.unwrap();
    assert_eq!(result.turns, 1);

    // Verify summary was stored (final summary on completion)
    let summary = load_session_summary(&summary_path).unwrap();
    assert!(
        summary.is_some(),
        "Summary should be written on agent completion"
    );
    let summary_text = summary.unwrap();
    assert!(
        summary_text.contains("# Session Summary"),
        "Summary should have header"
    );
    let word_count = summary_text.split_whitespace().count();
    assert!(
        word_count <= 500,
        "Summary should be <= 500 words, got {}",
        word_count
    );
}
