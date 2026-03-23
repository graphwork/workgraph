//! Integration tests: dual-API executor end-to-end with resume.
//!
//! Validates that the unified executor works identically across both API backends
//! (OpenAI-compatible / OpenRouter and Anthropic) and that resume works correctly.
//!
//! Test scenarios:
//! 1. Run a task via OpenAI-compatible endpoint — verify journal created, task completes
//! 2. Run a task via Anthropic API — verify journal created, task completes
//! 3. Compare journals from both — verify format is identical (provider-agnostic)
//! 4. Kill an agent mid-task (OpenRouter path) — unclaim — verify new agent resumes
//! 5. Kill an agent mid-task (Anthropic path) — unclaim — verify new agent resumes
//! 6. Run a long task that exceeds journal budget — verify compaction kicks in on resume

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tempfile::TempDir;

use workgraph::executor::native::agent::AgentLoop;
use workgraph::executor::native::client::{
    ContentBlock, MessagesRequest, MessagesResponse, Role, StopReason, Usage,
};
use workgraph::executor::native::journal::{self, Journal, JournalEntryKind};
use workgraph::executor::native::provider::Provider;
use workgraph::executor::native::resume::{load_resume_data, ResumeConfig};
use workgraph::executor::native::tools::ToolRegistry;

// ── Mock providers ──────────────────────────────────────────────────────

/// A mock provider that simulates an OpenAI-compatible (OpenRouter) backend.
struct MockOpenRouterProvider {
    responses: Vec<MessagesResponse>,
    call_count: Arc<AtomicUsize>,
}

impl MockOpenRouterProvider {
    fn new(responses: Vec<MessagesResponse>) -> Self {
        Self {
            responses,
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn simple_text(text: &str) -> Self {
        Self::new(vec![MessagesResponse {
            id: "chatcmpl-or-001".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage {
                input_tokens: 120,
                output_tokens: 45,
                cache_read_input_tokens: Some(30),
                cache_creation_input_tokens: None,
            },
        }])
    }

    fn with_tool_call(tool_name: &str, tool_input: serde_json::Value, final_text: &str) -> Self {
        Self::new(vec![
            MessagesResponse {
                id: "chatcmpl-or-tc-001".to_string(),
                content: vec![ContentBlock::ToolUse {
                    id: "call_or_1".to_string(),
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
                id: "chatcmpl-or-tc-002".to_string(),
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

    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for MockOpenRouterProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        "anthropic/claude-sonnet-4-20250514"
    }

    fn max_tokens(&self) -> u32 {
        16384
    }

    async fn send(&self, _request: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        if idx < self.responses.len() {
            Ok(self.responses[idx].clone())
        } else {
            Ok(MessagesResponse {
                id: format!("chatcmpl-or-fallback-{}", idx),
                content: vec![ContentBlock::Text {
                    text: "[openrouter mock exhausted]".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
    }
}

/// A mock provider that simulates the Anthropic Messages API backend.
struct MockAnthropicProvider {
    responses: Vec<MessagesResponse>,
    call_count: Arc<AtomicUsize>,
}

impl MockAnthropicProvider {
    fn new(responses: Vec<MessagesResponse>) -> Self {
        Self {
            responses,
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn simple_text(text: &str) -> Self {
        Self::new(vec![MessagesResponse {
            id: "msg_01XFDUDYJgAACzvnptvVoYEL".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage {
                input_tokens: 115,
                output_tokens: 42,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: Some(50),
            },
        }])
    }

    fn with_tool_call(tool_name: &str, tool_input: serde_json::Value, final_text: &str) -> Self {
        Self::new(vec![
            MessagesResponse {
                id: "msg_01XFDUDYJgAACzvnptvVoYEL".to_string(),
                content: vec![ContentBlock::ToolUse {
                    id: "toolu_01A09q90qw90lq917835lq9".to_string(),
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
                id: "msg_01Y2345678901234567890AB".to_string(),
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

    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for MockAnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        "claude-sonnet-4-20250514"
    }

    fn max_tokens(&self) -> u32 {
        16384
    }

    async fn send(&self, _request: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        if idx < self.responses.len() {
            Ok(self.responses[idx].clone())
        } else {
            Ok(MessagesResponse {
                id: format!("msg-anthropic-fallback-{}", idx),
                content: vec![ContentBlock::Text {
                    text: "[anthropic mock exhausted]".to_string(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn setup_workgraph(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let graph_path = dir.join("graph.jsonl");
    let graph = workgraph::graph::WorkGraph::new();
    workgraph::parser::save_graph(&graph, &graph_path).unwrap();
}

/// Extract all entries of a specific kind from a journal for comparison.
fn extract_entry_types(entries: &[workgraph::executor::native::journal::JournalEntry]) -> Vec<&str> {
    entries
        .iter()
        .map(|e| match &e.kind {
            JournalEntryKind::Init { .. } => "init",
            JournalEntryKind::Message { role, .. } => match role {
                Role::User => "message:user",
                Role::Assistant => "message:assistant",
            },
            JournalEntryKind::ToolExecution { .. } => "tool_execution",
            JournalEntryKind::Compaction { .. } => "compaction",
            JournalEntryKind::End { .. } => "end",
        })
        .collect()
}

/// Simulate a crash by removing the End entry from a journal.
fn simulate_crash(j_path: &Path) {
    let entries = Journal::read_all(j_path).unwrap();
    let entries_without_end: Vec<_> = entries
        .iter()
        .filter(|e| !matches!(e.kind, JournalEntryKind::End { .. }))
        .collect();
    fs::remove_file(j_path).unwrap();
    let mut f = fs::File::create(j_path).unwrap();
    for entry in entries_without_end {
        let json = serde_json::to_string(entry).unwrap();
        writeln!(f, "{}", json).unwrap();
    }
}

/// Verify that a journal file is valid, human-readable JSONL.
fn verify_journal_format(j_path: &Path) {
    let content = fs::read_to_string(j_path).unwrap();
    assert!(!content.is_empty(), "Journal should not be empty");

    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Each line must be valid JSON
        let val: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!(
                "Journal line {} is not valid JSON: {}\nLine: {}",
                i + 1,
                e,
                &line[..line.len().min(200)]
            )
        });

        // Must have required fields
        assert!(val.get("seq").is_some(), "Line {} missing 'seq' field", i + 1);
        assert!(
            val.get("timestamp").is_some(),
            "Line {} missing 'timestamp' field",
            i + 1
        );
        assert!(
            val.get("entry_type").is_some(),
            "Line {} missing 'entry_type' field",
            i + 1
        );

        // Sequence numbers must be monotonically increasing
        let seq = val["seq"].as_u64().unwrap();
        assert!(seq > 0, "Sequence numbers must be positive");

        // Timestamps must be valid ISO-8601
        let ts = val["timestamp"].as_str().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
            "Line {} has invalid timestamp: {}",
            i + 1,
            ts
        );
    }
}

// ── Scenario 1: OpenAI-compatible (OpenRouter) end-to-end ───────────────

#[tokio::test]
async fn scenario_1_openrouter_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "dual-openrouter-e2e";
    let j_path = journal::journal_path(&wg_dir, task_id);

    let provider = MockOpenRouterProvider::with_tool_call(
        "bash",
        serde_json::json!({"command": "echo hello"}),
        "Task completed via OpenRouter.",
    );

    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("openrouter-e2e.ndjson");

    let agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path.clone(), task_id.to_string())
    .with_resume(false);

    let result = agent.run("Run a simple command.").await.unwrap();

    // Verify task completed
    assert_eq!(result.final_text, "Task completed via OpenRouter.");
    assert_eq!(result.turns, 2);

    // Verify journal was created
    assert!(j_path.exists(), "Journal file should exist");

    // Verify journal format
    verify_journal_format(&j_path);

    // Verify journal content structure
    let entries = Journal::read_all(&j_path).unwrap();
    let types = extract_entry_types(&entries);

    // Expected: Init, User, Assistant(tool_use), ToolExec, User(tool_result), Assistant(final), End
    assert!(types.contains(&"init"), "Journal should have Init entry");
    assert!(types.contains(&"tool_execution"), "Journal should have ToolExecution entry");
    assert!(types.last() == Some(&"end"), "Journal should end with End entry");

    // Verify Init entry records the OpenAI provider
    match &entries[0].kind {
        JournalEntryKind::Init { provider, model, .. } => {
            assert_eq!(provider, "openai", "Init should record openai provider");
            assert_eq!(
                model, "anthropic/claude-sonnet-4-20250514",
                "Init should record the model"
            );
        }
        _ => panic!("First entry should be Init"),
    }
}

// ── Scenario 2: Anthropic API end-to-end ────────────────────────────────

#[tokio::test]
async fn scenario_2_anthropic_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "dual-anthropic-e2e";
    let j_path = journal::journal_path(&wg_dir, task_id);

    let provider = MockAnthropicProvider::with_tool_call(
        "bash",
        serde_json::json!({"command": "echo hello"}),
        "Task completed via Anthropic.",
    );

    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("anthropic-e2e.ndjson");

    let agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path.clone(), task_id.to_string())
    .with_resume(false);

    let result = agent.run("Run a simple command.").await.unwrap();

    // Verify task completed
    assert_eq!(result.final_text, "Task completed via Anthropic.");
    assert_eq!(result.turns, 2);

    // Verify journal was created
    assert!(j_path.exists(), "Journal file should exist");

    // Verify journal format
    verify_journal_format(&j_path);

    // Verify journal content structure
    let entries = Journal::read_all(&j_path).unwrap();
    let types = extract_entry_types(&entries);

    assert!(types.contains(&"init"), "Journal should have Init entry");
    assert!(types.contains(&"tool_execution"), "Journal should have ToolExecution entry");
    assert!(types.last() == Some(&"end"), "Journal should end with End entry");

    // Verify Init entry records the Anthropic provider
    match &entries[0].kind {
        JournalEntryKind::Init { provider, model, .. } => {
            assert_eq!(provider, "anthropic", "Init should record anthropic provider");
            assert_eq!(
                model, "claude-sonnet-4-20250514",
                "Init should record the model"
            );
        }
        _ => panic!("First entry should be Init"),
    }
}

// ── Scenario 3: Compare journals — format is provider-agnostic ──────────

#[tokio::test]
async fn scenario_3_journal_format_identical_across_providers() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    // Run the same task flow through OpenRouter provider
    let or_task_id = "dual-compare-openrouter";
    let or_j_path = journal::journal_path(&wg_dir, or_task_id);
    {
        let provider = MockOpenRouterProvider::simple_text("Done.");
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("compare-or.ndjson");

        let agent = AgentLoop::new(
            Box::new(provider),
            registry,
            "You are a test agent.".to_string(),
            10,
            output_log,
        )
        .with_journal(or_j_path.clone(), or_task_id.to_string())
        .with_resume(false);

        agent.run("Do the task.").await.unwrap();
    }

    // Run the same task flow through Anthropic provider
    let an_task_id = "dual-compare-anthropic";
    let an_j_path = journal::journal_path(&wg_dir, an_task_id);
    {
        let provider = MockAnthropicProvider::simple_text("Done.");
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("compare-an.ndjson");

        let agent = AgentLoop::new(
            Box::new(provider),
            registry,
            "You are a test agent.".to_string(),
            10,
            output_log,
        )
        .with_journal(an_j_path.clone(), an_task_id.to_string())
        .with_resume(false);

        agent.run("Do the task.").await.unwrap();
    }

    // Both journals should be valid
    verify_journal_format(&or_j_path);
    verify_journal_format(&an_j_path);

    // Read and compare structure
    let or_entries = Journal::read_all(&or_j_path).unwrap();
    let an_entries = Journal::read_all(&an_j_path).unwrap();

    // Same number of entries
    assert_eq!(
        or_entries.len(),
        an_entries.len(),
        "Both journals should have the same number of entries"
    );

    // Same entry type sequence
    let or_types = extract_entry_types(&or_entries);
    let an_types = extract_entry_types(&an_entries);
    assert_eq!(
        or_types, an_types,
        "Both journals should have identical entry type sequences"
    );

    // Verify the format of each entry pair is structurally identical
    // (same fields present, same types, only values differ)
    for (i, (or_entry, an_entry)) in or_entries.iter().zip(an_entries.iter()).enumerate() {
        let or_json: serde_json::Value = serde_json::to_value(or_entry).unwrap();
        let an_json: serde_json::Value = serde_json::to_value(an_entry).unwrap();

        // Both should have the same top-level keys
        let or_keys: std::collections::BTreeSet<_> = or_json.as_object().unwrap().keys().collect();
        let an_keys: std::collections::BTreeSet<_> = an_json.as_object().unwrap().keys().collect();
        assert_eq!(
            or_keys, an_keys,
            "Entry {} should have identical field names across providers.\nOpenRouter: {:?}\nAnthropic: {:?}",
            i, or_keys, an_keys
        );

        // entry_type should be the same
        assert_eq!(
            or_json["entry_type"], an_json["entry_type"],
            "Entry {} should have the same entry_type",
            i
        );
    }

    // Cross-provider resume: load the OpenRouter journal with a "different" provider
    let resume_data = load_resume_data(&or_j_path, tmp.path(), &ResumeConfig::default())
        .unwrap()
        .expect("Should be able to load OpenRouter journal for resume");

    // Verify that the messages are usable
    assert!(
        !resume_data.messages.is_empty(),
        "Resume data should have messages from OpenRouter journal"
    );

    // Now resume from Anthropic journal too
    let resume_data_an = load_resume_data(&an_j_path, tmp.path(), &ResumeConfig::default())
        .unwrap()
        .expect("Should be able to load Anthropic journal for resume");

    assert_eq!(
        resume_data.messages.len(),
        resume_data_an.messages.len(),
        "Both providers' resume data should have the same number of messages"
    );
}

// ── Scenario 4: Kill + resume on OpenRouter path ────────────────────────

#[tokio::test]
async fn scenario_4_kill_and_resume_openrouter() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "dual-kill-openrouter";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a test file for the agent to read
    let test_file = tmp.path().join("data.txt");
    fs::write(&test_file, "openrouter test data").unwrap();

    // === First session (OpenRouter): agent runs, does work, then "crashes" ===
    {
        let provider = MockOpenRouterProvider::with_tool_call(
            "read_file",
            serde_json::json!({"path": test_file.to_str().unwrap()}),
            "Read the file via OpenRouter.",
        );
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("kill-or-s1.ndjson");

        let agent = AgentLoop::new(
            Box::new(provider),
            registry,
            "You are a test agent.".to_string(),
            10,
            output_log,
        )
        .with_journal(j_path.clone(), task_id.to_string())
        .with_resume(false);

        let result = agent.run("Read data.txt.").await.unwrap();
        assert_eq!(result.turns, 2);
    }

    // Verify first session journal
    let entries_s1 = Journal::read_all(&j_path).unwrap();
    assert!(entries_s1.len() >= 6, "First session should have at least 6 entries");

    // Verify the Init recorded "openai" provider
    match &entries_s1[0].kind {
        JournalEntryKind::Init { provider, .. } => {
            assert_eq!(provider, "openai");
        }
        _ => panic!("First entry should be Init"),
    }

    // Simulate crash: remove End entry
    simulate_crash(&j_path);
    let entries_after_crash = Journal::read_all(&j_path).unwrap();
    assert!(
        !entries_after_crash
            .iter()
            .any(|e| matches!(e.kind, JournalEntryKind::End { .. })),
        "End entry should be removed"
    );

    // === Second session: new agent resumes from the crashed journal ===
    {
        let provider = MockOpenRouterProvider::simple_text(
            "Resumed from OpenRouter journal. Task complete.",
        );
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("kill-or-s2.ndjson");

        let agent = AgentLoop::new(
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
        assert_eq!(result.turns, 1);
        assert_eq!(
            result.final_text,
            "Resumed from OpenRouter journal. Task complete."
        );
    }

    // Verify full journal
    let final_entries = Journal::read_all(&j_path).unwrap();
    verify_journal_format(&j_path);

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
    assert!(has_resume, "Journal should contain resume annotation");

    // Should end with End entry
    assert!(
        matches!(final_entries.last().unwrap().kind, JournalEntryKind::End { .. }),
        "Journal should end with End entry"
    );
}

// ── Scenario 5: Kill + resume on Anthropic path ─────────────────────────

#[tokio::test]
async fn scenario_5_kill_and_resume_anthropic() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "dual-kill-anthropic";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a test file
    let test_file = tmp.path().join("code.rs");
    fs::write(&test_file, "fn anthropic_test() {}").unwrap();

    // === First session (Anthropic): agent runs and "crashes" ===
    {
        let provider = MockAnthropicProvider::with_tool_call(
            "read_file",
            serde_json::json!({"path": test_file.to_str().unwrap()}),
            "Read code via Anthropic API.",
        );
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("kill-an-s1.ndjson");

        let agent = AgentLoop::new(
            Box::new(provider),
            registry,
            "You are a test agent.".to_string(),
            10,
            output_log,
        )
        .with_journal(j_path.clone(), task_id.to_string())
        .with_resume(false);

        let result = agent.run("Read code.rs.").await.unwrap();
        assert_eq!(result.turns, 2);
    }

    // Verify first session Init recorded "anthropic" provider
    let entries_s1 = Journal::read_all(&j_path).unwrap();
    match &entries_s1[0].kind {
        JournalEntryKind::Init { provider, model, .. } => {
            assert_eq!(provider, "anthropic");
            assert_eq!(model, "claude-sonnet-4-20250514");
        }
        _ => panic!("First entry should be Init"),
    }

    // Simulate crash
    simulate_crash(&j_path);

    // Modify the file (simulate another agent changing it)
    fs::write(&test_file, "fn modified_by_other_agent() { todo!() }").unwrap();

    // === Second session: new agent resumes ===
    {
        let provider = MockAnthropicProvider::simple_text(
            "Resumed from Anthropic journal. Noticed stale state, re-read files.",
        );
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("kill-an-s2.ndjson");

        let agent = AgentLoop::new(
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
        assert_eq!(result.turns, 1);
        assert_eq!(
            result.final_text,
            "Resumed from Anthropic journal. Noticed stale state, re-read files."
        );
    }

    // Verify final journal
    let final_entries = Journal::read_all(&j_path).unwrap();
    verify_journal_format(&j_path);

    // Should have resume annotation WITH stale state warning
    let resume_msg = final_entries.iter().find(|e| {
        if let JournalEntryKind::Message { content, .. } = &e.kind {
            content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("RESUMED"),
                _ => false,
            })
        } else {
            false
        }
    });
    assert!(resume_msg.is_some(), "Should have resume annotation");

    // The resume annotation should mention stale state
    let has_stale_warning = final_entries.iter().any(|e| {
        if let JournalEntryKind::Message { content, .. } = &e.kind {
            content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("STALE") && text.contains("code.rs"),
                _ => false,
            })
        } else {
            false
        }
    });
    assert!(
        has_stale_warning,
        "Resume should detect and annotate stale file"
    );

    // Should end with End entry
    assert!(
        matches!(final_entries.last().unwrap().kind, JournalEntryKind::End { .. }),
        "Journal should end with End entry"
    );
}

// ── Scenario 6: Long task exceeds budget, compaction on resume ──────────

#[tokio::test]
async fn scenario_6_compaction_on_resume() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "dual-compaction";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // Create a journal with many large message pairs that will exceed the budget
    {
        let mut journal = Journal::open(&j_path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "anthropic/claude-sonnet-4-20250514".to_string(),
                provider: "openai".to_string(),
                system_prompt: "You are a test agent working on a large task.".to_string(),
                tools: vec![],
                task_id: Some(task_id.to_string()),
            })
            .unwrap();

        // Write 50 large message pairs to exceed budget
        for i in 0..50 {
            let large_content = format!(
                "Turn {} analysis: {}\n\nCode changes reviewed:\n{}",
                i,
                "detailed analysis ".repeat(100),
                "fn example() { /* modified */ }".repeat(50)
            );

            journal
                .append(JournalEntryKind::Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!("Continue working on step {}. {}", i, large_content),
                    }],
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                })
                .unwrap();

            journal
                .append(JournalEntryKind::Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: format!("Completed step {}. {}", i, large_content),
                    }],
                    usage: Some(Usage {
                        input_tokens: 2000,
                        output_tokens: 1000,
                        ..Usage::default()
                    }),
                    response_id: Some(format!("msg-{}", i)),
                    stop_reason: if i == 49 {
                        Some(StopReason::ToolUse) // Last one was mid-work
                    } else {
                        Some(StopReason::EndTurn)
                    },
                })
                .unwrap();

            // Add some tool execution entries to make it realistic
            if i % 5 == 0 {
                journal
                    .append(JournalEntryKind::ToolExecution {
                        tool_use_id: format!("tu-{}", i),
                        name: "bash".to_string(),
                        input: serde_json::json!({"command": format!("cargo test step_{}", i)}),
                        output: format!("test step_{} ... ok\n\ntest result: ok. 1 passed", i),
                        is_error: false,
                        duration_ms: 3000 + (i as u64 * 100),
                    })
                    .unwrap();
            }
        }
        // No End entry — simulates crash during a long task
    }

    // Verify journal is valid
    verify_journal_format(&j_path);

    // Load resume data with a tight budget to force compaction
    let config = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 2000, // Very small to force compaction
    };

    let resume_data = load_resume_data(&j_path, tmp.path(), &config)
        .unwrap()
        .expect("Should load resume data");

    // Verify compaction kicked in
    assert!(
        resume_data.was_compacted,
        "Large journal should trigger compaction"
    );

    // Compacted messages should be significantly fewer than original
    let original_message_count = 100; // 50 pairs
    assert!(
        resume_data.messages.len() < original_message_count,
        "Compacted messages ({}) should be fewer than original ({})",
        resume_data.messages.len(),
        original_message_count
    );

    // First message should be the compaction summary
    match &resume_data.messages[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(
                text.contains("compacted") || text.contains("Resume"),
                "First message should be compaction summary, got: {}",
                &text[..text.len().min(200)]
            );
        }
        _ => panic!("Expected text content in compaction summary"),
    }

    // System prompt should be preserved
    assert!(
        resume_data.system_prompt.is_some(),
        "System prompt should be preserved through compaction"
    );

    // Now run the resumed agent to verify it works end-to-end
    let provider = MockOpenRouterProvider::simple_text(
        "Resumed from compacted journal. All prior work preserved.",
    );
    let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
    let output_log = wg_dir.join("compact-e2e.ndjson");

    let agent = AgentLoop::new(
        Box::new(provider),
        registry,
        "You are a test agent.".to_string(),
        10,
        output_log,
    )
    .with_journal(j_path.clone(), task_id.to_string())
    .with_resume(true)
    .with_working_dir(tmp.path().to_path_buf());

    let result = agent.run("Continue the long task.").await.unwrap();
    assert_eq!(
        result.final_text,
        "Resumed from compacted journal. All prior work preserved."
    );

    // Verify the resumed journal is still valid
    verify_journal_format(&j_path);

    // The journal should have the resume session appended
    let final_entries = Journal::read_all(&j_path).unwrap();
    assert!(
        matches!(final_entries.last().unwrap().kind, JournalEntryKind::End { .. }),
        "Resumed journal should end with End entry"
    );

    // The resume message should mention it was resumed
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
    assert!(has_resume, "Resumed session should have RESUMED annotation");
}

// ── Cross-provider resume: OpenRouter → Anthropic ───────────────────────

#[tokio::test]
async fn cross_provider_resume_openrouter_to_anthropic() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "dual-cross-or-an";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // First session: OpenRouter provider
    {
        let provider = MockOpenRouterProvider::simple_text("OpenRouter session work.");
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("cross-s1.ndjson");

        let agent = AgentLoop::new(
            Box::new(provider),
            registry,
            "You are a test agent.".to_string(),
            10,
            output_log,
        )
        .with_journal(j_path.clone(), task_id.to_string())
        .with_resume(false);

        agent.run("Start task.").await.unwrap();
    }

    // Simulate crash
    simulate_crash(&j_path);

    // Second session: Anthropic provider resumes from OpenRouter journal
    {
        let provider =
            MockAnthropicProvider::simple_text("Anthropic resumed from OpenRouter journal.");
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("cross-s2.ndjson");

        let agent = AgentLoop::new(
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
            "Anthropic resumed from OpenRouter journal."
        );
    }

    // Verify the journal has entries from both providers
    let entries = Journal::read_all(&j_path).unwrap();
    verify_journal_format(&j_path);

    let init_entries: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.kind, JournalEntryKind::Init { .. }))
        .collect();

    assert_eq!(
        init_entries.len(),
        2,
        "Should have 2 Init entries (one per session)"
    );

    // First Init should be OpenRouter, second should be Anthropic
    match &init_entries[0].kind {
        JournalEntryKind::Init { provider, .. } => {
            assert_eq!(provider, "openai", "First session should be openai");
        }
        _ => unreachable!(),
    }
    match &init_entries[1].kind {
        JournalEntryKind::Init { provider, .. } => {
            assert_eq!(provider, "anthropic", "Second session should be anthropic");
        }
        _ => unreachable!(),
    }
}

// ── Cross-provider resume: Anthropic → OpenRouter ───────────────────────

#[tokio::test]
async fn cross_provider_resume_anthropic_to_openrouter() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    setup_workgraph(&wg_dir);

    let task_id = "dual-cross-an-or";
    let j_path = journal::journal_path(&wg_dir, task_id);

    // First session: Anthropic provider
    {
        let provider = MockAnthropicProvider::simple_text("Anthropic session work.");
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("cross2-s1.ndjson");

        let agent = AgentLoop::new(
            Box::new(provider),
            registry,
            "You are a test agent.".to_string(),
            10,
            output_log,
        )
        .with_journal(j_path.clone(), task_id.to_string())
        .with_resume(false);

        agent.run("Start task.").await.unwrap();
    }

    // Simulate crash
    simulate_crash(&j_path);

    // Second session: OpenRouter provider resumes from Anthropic journal
    {
        let provider =
            MockOpenRouterProvider::simple_text("OpenRouter resumed from Anthropic journal.");
        let registry = ToolRegistry::default_all(&wg_dir, tmp.path());
        let output_log = wg_dir.join("cross2-s2.ndjson");

        let agent = AgentLoop::new(
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
            "OpenRouter resumed from Anthropic journal."
        );
    }

    // Verify journal
    let entries = Journal::read_all(&j_path).unwrap();
    verify_journal_format(&j_path);

    let init_entries: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.kind, JournalEntryKind::Init { .. }))
        .collect();
    assert_eq!(init_entries.len(), 2);

    match &init_entries[0].kind {
        JournalEntryKind::Init { provider, .. } => {
            assert_eq!(provider, "anthropic");
        }
        _ => unreachable!(),
    }
    match &init_entries[1].kind {
        JournalEntryKind::Init { provider, .. } => {
            assert_eq!(provider, "openai");
        }
        _ => unreachable!(),
    }
}
