//! Integration tests for agent resume from conversation journal.
//!
//! Tests the resume protocol through the native executor agent loop using mock providers.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;

use workgraph::executor::native::agent::AgentLoop;
use workgraph::executor::native::client::{
    ContentBlock, MessagesRequest, MessagesResponse, StopReason, Usage,
};
use workgraph::executor::native::journal::{self, Journal, JournalEntryKind};
use workgraph::executor::native::provider::Provider;
use workgraph::executor::native::resume::{ResumeConfig, load_resume_data};
use workgraph::executor::native::tools::ToolRegistry;

/// A mock provider that returns pre-scripted responses.
struct MockProvider {
    responses: Vec<MessagesResponse>,
    call_count: Arc<AtomicUsize>,
}

impl MockProvider {
    fn new(responses: Vec<MessagesResponse>) -> Self {
        Self {
            responses,
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn simple_text(text: &str) -> Self {
        Self::new(vec![MessagesResponse {
            id: "msg-resume-001".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                ..Usage::default()
            },
        }])
    }

    fn with_tool_call(tool_name: &str, tool_input: serde_json::Value, final_text: &str) -> Self {
        Self::new(vec![
            MessagesResponse {
                id: "msg-resume-tc-001".to_string(),
                content: vec![ContentBlock::ToolUse {
                    id: "tu-resume-1".to_string(),
                    name: tool_name.to_string(),
                    input: tool_input,
                }],
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 30,
                    ..Usage::default()
                },
            },
            MessagesResponse {
                id: "msg-resume-tc-002".to_string(),
                content: vec![ContentBlock::Text {
                    text: final_text.to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage {
                    input_tokens: 200,
                    output_tokens: 60,
                    ..Usage::default()
                },
            },
        ])
    }

    #[allow(dead_code)]
    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn model(&self) -> &str {
        "mock-model-v1"
    }

    fn max_tokens(&self) -> u32 {
        4096
    }

    async fn send(&self, _request: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        if idx < self.responses.len() {
            Ok(self.responses[idx].clone())
        } else {
            Ok(MessagesResponse {
                id: format!("msg-fallback-{}", idx),
                content: vec![ContentBlock::Text {
                    text: "[mock exhausted]".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
    }
}

fn setup_workgraph(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let graph_path = dir.join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();
}

/// Helper: run an agent to completion, simulating a first session.
async fn run_first_session(wg_dir: &Path, task_id: &str, provider: MockProvider) {
    let j_path = journal::journal_path(wg_dir, task_id);
    let registry = ToolRegistry::default_all(wg_dir, wg_dir.parent().unwrap());
    let output_log = wg_dir.join("test.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path, task_id.to_string())
    .with_resume(false); // First session: no resume

    agent.run("Start the task.").await.unwrap();
}

// ── Test: agent resumes from journal and continues ─────────────────────

#[tokio::test]
async fn test_agent_resumes_from_journal() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "resume-basic";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // === First session: agent runs and completes ===
    run_first_session(
        &wg_dir,
        task_id,
        MockProvider::simple_text("First session done."),
    )
    .await;

    let entries_after_first = Journal::read_all(&j_path).unwrap();
    assert!(
        entries_after_first.len() >= 3,
        "Should have Init + messages + End"
    );

    // === Simulate crash: remove End entry by writing new entries without it ===
    // (In practice, a crash means no End entry. We'll create a new journal
    // that looks like a mid-flight crash.)
    let crash_journal_path = journal::journal_path(&wg_dir, "resume-crash");
    {
        let mut journal = Journal::open(&crash_journal_path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "mock-model-v1".to_string(),
                provider: "mock".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                task_id: Some("resume-crash".to_string()),
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::User,
                content: vec![ContentBlock::Text {
                    text: "Start the task.".to_string(),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I'll start by reading the code...".to_string(),
                }],
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Usage::default()
                }),
                response_id: Some("resp-1".to_string()),
                stop_reason: Some(StopReason::ToolUse),
            })
            .unwrap();
        // No End entry — simulates crash
    }

    // === Second session: agent resumes from the crashed journal ===
    let provider = MockProvider::simple_text("Resumed and completed the task.");
    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("resume-test.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(crash_journal_path.clone(), "resume-crash".to_string())
    .with_resume(true)
    .with_working_dir(tmp.path().to_path_buf());

    let result = agent.run("Continue the task.").await.unwrap();
    assert_eq!(result.turns, 1);
    assert_eq!(result.final_text, "Resumed and completed the task.");

    // Verify the journal now has the resume session entries appended
    let entries = Journal::read_all(&crash_journal_path).unwrap();

    // Original: Init + User + Assistant (3 entries)
    // Resume session: Init + User (resume msg) + Assistant + End (4 entries)
    assert!(
        entries.len() >= 6,
        "Should have original + resume entries, got {}",
        entries.len()
    );

    // The resume user message should mention "RESUMED"
    let resume_msg = entries.iter().find(|e| {
        if let JournalEntryKind::Message { role, content, .. } = &e.kind {
            *role == workgraph::executor::native::client::Role::User
                && content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("RESUMED"),
                    _ => false,
                })
        } else {
            false
        }
    });
    assert!(
        resume_msg.is_some(),
        "Should have a resume annotation message"
    );
}

// ── Test: large journal gets compacted before resume ───────────────────

#[tokio::test]
async fn test_large_journal_compacted_on_resume() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "resume-compact";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a journal with many large messages that exceed the budget
    {
        let mut journal = Journal::open(&j_path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "mock-model-v1".to_string(),
                provider: "mock".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                task_id: Some(task_id.to_string()),
            })
            .unwrap();

        // Write 40 large message pairs (user + assistant) to exceed budget
        for i in 0..40 {
            let large_text = format!("Message {} with lots of content: {}", i, "x".repeat(5000));
            journal
                .append(JournalEntryKind::Message {
                    role: workgraph::executor::native::client::Role::User,
                    content: vec![ContentBlock::Text {
                        text: large_text.clone(),
                    }],
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                })
                .unwrap();
            journal
                .append(JournalEntryKind::Message {
                    role: workgraph::executor::native::client::Role::Assistant,
                    content: vec![ContentBlock::Text { text: large_text }],
                    usage: Some(Usage {
                        input_tokens: 1000,
                        output_tokens: 500,
                        ..Usage::default()
                    }),
                    response_id: Some(format!("resp-{}", i)),
                    stop_reason: if i == 39 {
                        Some(StopReason::ToolUse)
                    } else {
                        Some(StopReason::EndTurn)
                    },
                })
                .unwrap();
        }
        // No End entry — simulates crash
    }

    // Load resume data with a small context window to force compaction
    let config = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 1000, // Very small to force compaction
    };

    let resume_data = load_resume_data(&j_path, tmp.path(), &config)
        .unwrap()
        .expect("Should load resume data");

    assert!(
        resume_data.was_compacted,
        "Journal should have been compacted"
    );
    assert!(
        resume_data.messages.len() < 80,
        "Compacted messages ({}) should be fewer than original (80)",
        resume_data.messages.len()
    );

    // First message should be the compaction summary
    match &resume_data.messages[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(
                text.contains("compacted"),
                "Summary should mention compaction: {}",
                text
            );
        }
        _ => panic!("Expected text content in compaction summary"),
    }

    // Run the agent with resume to verify it works end-to-end
    let provider = MockProvider::simple_text("Completed after compacted resume.");
    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("compact-test.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path, task_id.to_string())
    .with_resume(true)
    .with_working_dir(tmp.path().to_path_buf());

    let result = agent.run("Continue.").await.unwrap();
    assert_eq!(result.final_text, "Completed after compacted resume.");
}

// ── Test: stale tool results are detected and annotated ────────────────

#[tokio::test]
async fn test_stale_tool_results_detected() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "resume-stale";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a file that the prior session "read"
    let test_file = tmp.path().join("src").join("lib.rs");
    fs::create_dir_all(test_file.parent().unwrap()).unwrap();
    fs::write(&test_file, "fn original() {}").unwrap();

    // Create a journal that recorded reading this file
    {
        let mut journal = Journal::open(&j_path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "mock-model-v1".to_string(),
                provider: "mock".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                task_id: Some(task_id.to_string()),
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::User,
                content: vec![ContentBlock::Text {
                    text: "Read the file.".to_string(),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tu-1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": test_file.to_str().unwrap()}),
                }],
                usage: Some(Usage {
                    input_tokens: 50,
                    output_tokens: 20,
                    ..Usage::default()
                }),
                response_id: Some("resp-1".to_string()),
                stop_reason: Some(StopReason::ToolUse),
            })
            .unwrap();
        journal
            .append(JournalEntryKind::ToolExecution {
                tool_use_id: "tu-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": test_file.to_str().unwrap()}),
                output: "fn original() {}".to_string(),
                is_error: false,
                duration_ms: 5,
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tu-1".to_string(),
                    content: "fn original() {}".to_string(),
                    is_error: false,
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        // Crash — no End entry
    }

    // Now modify the file (simulating another agent or human changing it)
    fs::write(&test_file, "fn modified() { todo!() }").unwrap();

    // Load resume data — should detect stale state
    let config = ResumeConfig::default();
    let resume_data = load_resume_data(&j_path, tmp.path(), &config)
        .unwrap()
        .expect("Should load resume data");

    assert!(
        !resume_data.stale_annotations.is_empty(),
        "Should detect stale file"
    );
    assert!(
        resume_data.stale_annotations[0].contains("STALE"),
        "Annotation should mention STALE: {}",
        resume_data.stale_annotations[0]
    );
    assert!(
        resume_data.stale_annotations[0].contains("lib.rs"),
        "Annotation should mention the file: {}",
        resume_data.stale_annotations[0]
    );

    // Run the agent with resume — the stale annotation should be in the resume message
    let provider = MockProvider::simple_text("Noted the stale state and re-read files.");
    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("stale-test.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path.clone(), task_id.to_string())
    .with_resume(true)
    .with_working_dir(tmp.path().to_path_buf());

    let result = agent.run("Continue.").await.unwrap();
    assert_eq!(
        result.final_text,
        "Noted the stale state and re-read files."
    );

    // Verify the resume message in the journal contains the stale annotation
    let entries = Journal::read_all(&j_path).unwrap();
    let has_stale_annotation = entries.iter().any(|e| {
        if let JournalEntryKind::Message { content, .. } = &e.kind {
            content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("STALE") && text.contains("lib.rs"),
                _ => false,
            })
        } else {
            false
        }
    });
    assert!(
        has_stale_annotation,
        "Journal should contain stale annotation"
    );
}

// ── Test: --no-resume flag causes fresh start ──────────────────────────

#[tokio::test]
async fn test_no_resume_flag_fresh_start() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "resume-disabled";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a journal from a "prior session"
    {
        let mut journal = Journal::open(&j_path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "mock-model-v1".to_string(),
                provider: "mock".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                task_id: Some(task_id.to_string()),
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::User,
                content: vec![ContentBlock::Text {
                    text: "Prior session message.".to_string(),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Prior session response.".to_string(),
                }],
                usage: Some(Usage {
                    input_tokens: 50,
                    output_tokens: 25,
                    ..Usage::default()
                }),
                response_id: Some("resp-1".to_string()),
                stop_reason: Some(StopReason::ToolUse),
            })
            .unwrap();
        // No End — simulates crash
    }

    let entries_before = Journal::read_all(&j_path).unwrap();
    assert_eq!(entries_before.len(), 3);

    // Run with --no-resume: should start fresh
    let provider = MockProvider::simple_text("Fresh start, no resume.");
    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("no-resume-test.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path.clone(), task_id.to_string())
    .with_resume(false) // Disabled!
    .with_working_dir(tmp.path().to_path_buf());

    let result = agent.run("Start fresh.").await.unwrap();
    assert_eq!(result.final_text, "Fresh start, no resume.");

    // Verify the journal has the new session entries appended
    let entries = Journal::read_all(&j_path).unwrap();

    // Original: 3 entries
    // New session (no resume): Init + User + Assistant + End = 4 entries
    assert_eq!(entries.len(), 7, "Should have 3 original + 4 new entries");

    // The new user message should NOT mention "RESUMED"
    let has_resume_annotation = entries.iter().any(|e| {
        if let JournalEntryKind::Message { content, .. } = &e.kind {
            content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("RESUMED"),
                _ => false,
            })
        } else {
            false
        }
    });
    assert!(
        !has_resume_annotation,
        "Should NOT have resume annotation when --no-resume is used"
    );
}

// ── Test: kill agent mid-task, unclaim, new agent resumes and completes ─

#[tokio::test]
async fn test_kill_and_resume_integration() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "kill-resume";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a test file
    let test_file = tmp.path().join("data.txt");
    fs::write(&test_file, "initial content").unwrap();

    // === First session: agent runs, does some work, then "crashes" ===
    {
        let provider = MockProvider::with_tool_call(
            "read_file",
            serde_json::json!({"path": test_file.to_str().unwrap()}),
            "I read the file successfully.",
        );
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("session1.ndjson");

        let mut agent = AgentLoop::new(
            Box::new(provider),
            registry,
            "You are a test agent.".to_string(),
            10,
            output_log,
        )
        .with_journal(j_path.clone(), task_id.to_string())
        .with_resume(false);

        let result = agent.run("Read data.txt and process it.").await.unwrap();
        assert_eq!(result.turns, 2);
    }

    let entries_after_session1 = Journal::read_all(&j_path).unwrap();
    // Init + User + Assistant(tool_use) + ToolExec + User(tool_result) + Assistant(final) + End = 7
    assert!(entries_after_session1.len() >= 6);

    // Simulate crash: truncate the journal by removing the End entry
    // (rewrite without the last entry)
    {
        let entries = Journal::read_all(&j_path).unwrap();
        let entries_without_end: Vec<_> = entries
            .iter()
            .filter(|e| !matches!(e.kind, JournalEntryKind::End { .. }))
            .collect();
        // Rewrite the journal
        fs::remove_file(&j_path).unwrap();
        let mut f = fs::File::create(&j_path).unwrap();
        for entry in entries_without_end {
            let json = serde_json::to_string(entry).unwrap();
            writeln!(f, "{}", json).unwrap();
        }
    }

    // === Second session: new agent picks up from journal ===
    {
        let provider = MockProvider::simple_text("Resumed! Completed the remaining work.");
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("session2.ndjson");

        let mut agent = AgentLoop::new(
            Box::new(provider),
            registry,
            "You are a test agent.".to_string(),
            10,
            output_log,
        )
        .with_journal(j_path.clone(), task_id.to_string())
        .with_resume(true) // Resume enabled
        .with_working_dir(tmp.path().to_path_buf());

        let result = agent.run("Continue the task.").await.unwrap();
        assert_eq!(result.turns, 1);
        assert_eq!(result.final_text, "Resumed! Completed the remaining work.");
    }

    // Verify the full journal
    let final_entries = Journal::read_all(&j_path).unwrap();

    // Should have: original entries (without End) + resume Init + resume User + resume Assistant + End
    assert!(
        final_entries.len() >= 9,
        "Expected at least 9 entries in final journal, got {}",
        final_entries.len()
    );

    // Should have a resume annotation
    let has_resume = final_entries.iter().any(|e| {
        if let JournalEntryKind::Message { content, .. } = &e.kind {
            content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("RESUMED"),
                _ => false,
            })
        } else {
            false
        }
    });
    assert!(has_resume, "Should have resume annotation in the journal");

    // Should end with an End entry
    assert!(
        matches!(
            final_entries.last().unwrap().kind,
            JournalEntryKind::End { .. }
        ),
        "Journal should end with End entry"
    );
}

// ── Test: resume with empty journal (no messages) ──────────────────────

#[tokio::test]
async fn test_resume_empty_journal() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "resume-empty";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a journal with only an Init entry (agent crashed immediately)
    {
        let mut journal = Journal::open(&j_path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "mock-model-v1".to_string(),
                provider: "mock".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                task_id: Some(task_id.to_string()),
            })
            .unwrap();
    }

    // Resume should behave as fresh start (no messages to resume from)
    let provider = MockProvider::simple_text("Started fresh after empty journal.");
    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("empty-test.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path, task_id.to_string())
    .with_resume(true)
    .with_working_dir(tmp.path().to_path_buf());

    let result = agent.run("Do the task.").await.unwrap();
    assert_eq!(result.final_text, "Started fresh after empty journal.");
}

// ── Test: resume works for both provider paths (provider-agnostic) ─────

#[tokio::test]
async fn test_resume_provider_agnostic() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "resume-agnostic";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a journal that looks like it came from an "openai" provider
    {
        let mut journal = Journal::open(&j_path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "gpt-4o".to_string(),
                provider: "openai".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                task_id: Some(task_id.to_string()),
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::User,
                content: vec![ContentBlock::Text {
                    text: "Hello from OpenAI session.".to_string(),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: workgraph::executor::native::client::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Working on it...".to_string(),
                }],
                usage: Some(Usage {
                    input_tokens: 50,
                    output_tokens: 25,
                    ..Usage::default()
                }),
                response_id: Some("chatcmpl-abc".to_string()),
                stop_reason: Some(StopReason::ToolUse),
            })
            .unwrap();
    }

    // Resume with a "mock" provider (different from journal's "openai") — should work
    let provider = MockProvider::simple_text("Resumed across provider boundary.");
    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("agnostic-test.ndjson");

    let mut agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path, task_id.to_string())
    .with_resume(true)
    .with_working_dir(tmp.path().to_path_buf());

    let result = agent.run("Continue.").await.unwrap();
    assert_eq!(result.final_text, "Resumed across provider boundary.");
}
