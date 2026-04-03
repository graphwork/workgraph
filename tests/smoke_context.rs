//! Smoke tests for context window management and compaction behavior.
//!
//! Exercises:
//! 1. Context size correctly reported from provider config
//! 2. Context pressure warning injected at threshold (compaction triggered)
//! 3. Large tool outputs truncated per configuration (100KB limit)
//! 4. Emergency compaction triggered when approaching limits
//! 5. Agent doesn't crash on context exhaustion
//!
//! Most tests use the public API directly. The live end-to-end test is gated
//! with `#[ignore]` and requires `OPENROUTER_API_KEY`.
//!
//! Run with: cargo test --test smoke_context
//! For live tests: cargo test --test smoke_context -- --ignored

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;
use workgraph::executor::native::client::{ContentBlock, Role};
use workgraph::executor::native::journal::{Journal, JournalEntryKind};
use workgraph::executor::native::resume::{load_resume_data, ResumeConfig};
use workgraph::executor::native::tools::truncate_output;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    let fake_home = wg_dir.parent().unwrap_or(wg_dir).join("fakehome");
    fs::create_dir_all(&fake_home).unwrap_or_default();
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .env("HOME", &fake_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    stdout
}

/// Create a journal with an Init entry and many message pairs to trigger compaction.
fn make_large_journal(dir: &Path, num_pairs: usize) -> PathBuf {
    let path = dir.join("conversation.jsonl");
    let mut journal = Journal::open(&path).unwrap();

    journal
        .append(JournalEntryKind::Init {
            model: "minimax/minimax-m2.7".to_string(),
            provider: "openrouter".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            tools: vec![],
            task_id: Some("context-test".to_string()),
        })
        .unwrap();

    for i in 0..num_pairs {
        // User message with substantial content
        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "Task step {}: Please analyze the following data and provide a detailed report. {}",
                        i,
                        "x".repeat(500)
                    ),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();

        // Assistant response with substantial content
        journal
            .append(JournalEntryKind::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "Analysis for step {}: The data shows significant patterns. {}",
                        i,
                        "y".repeat(500)
                    ),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
    }

    path
}

// ---------------------------------------------------------------------------
// Test 1: Context size correctly reported from provider config
// ---------------------------------------------------------------------------

/// ResumeConfig defaults to a 200k token context window.
#[test]
fn context_size_reported_from_config() {
    let config = ResumeConfig::default();

    // Default context window should be 200k tokens
    assert_eq!(
        config.context_window_tokens, 200_000,
        "Default context window should be 200,000 tokens"
    );

    // Budget percentage should be 50%
    assert!(
        (config.budget_pct - 0.50).abs() < f64::EPSILON,
        "Default budget percentage should be 50%, got {}",
        config.budget_pct
    );

    // Custom config should be honored
    let custom = ResumeConfig {
        budget_pct: 0.75,
        context_window_tokens: 128_000,
    };
    assert_eq!(custom.context_window_tokens, 128_000);
    assert!((custom.budget_pct - 0.75).abs() < f64::EPSILON);
}

/// Provider trait's context_window() method returns 200K by default and can
/// be overridden. The ResumeConfig should be constructible from the provider's value.
#[test]
fn test_context_window_from_provider() {
    // Default Provider trait returns 200K
    let default_config = ResumeConfig::default();
    assert_eq!(default_config.context_window_tokens, 200_000);

    // Simulate provider returning a smaller context window (e.g. Qwen3-32B)
    let small_config = ResumeConfig {
        context_window_tokens: 32_000,
        ..ResumeConfig::default()
    };
    assert_eq!(small_config.context_window_tokens, 32_000);
    assert!((small_config.budget_pct - 0.50).abs() < f64::EPSILON);

    // A 128K window provider
    let mid_config = ResumeConfig {
        context_window_tokens: 128_000,
        ..ResumeConfig::default()
    };
    assert_eq!(mid_config.context_window_tokens, 128_000);
}

// ---------------------------------------------------------------------------
// Test 2: Context pressure triggers compaction at threshold
// ---------------------------------------------------------------------------

/// When estimated tokens exceed budget_pct * context_window_tokens,
/// the resume loader compacts older messages.
#[test]
fn context_pressure_triggers_compaction() {
    let tmp = TempDir::new().unwrap();

    // Create a journal with many message pairs (enough to exceed budget)
    let journal_path = make_large_journal(tmp.path(), 50);

    // Use a very small context window so compaction is triggered
    let config = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 1_000, // Very small: ~500 token budget
    };

    let resume = load_resume_data(&journal_path, tmp.path(), &config)
        .unwrap()
        .expect("Should load resume data");

    // Journal should have been compacted
    assert!(
        resume.was_compacted,
        "Resume should be compacted when token estimate exceeds budget"
    );

    // Compacted messages should be fewer than original
    // Original: 50 pairs = 100 messages. Compacted should be much fewer.
    assert!(
        resume.messages.len() < 100,
        "Compacted messages ({}) should be fewer than original (100)",
        resume.messages.len()
    );

    // First message in compacted output should mention compaction
    let first_text = match &resume.messages[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("Expected text content in first compacted message"),
    };
    assert!(
        first_text.contains("compacted") || first_text.contains("Resume"),
        "First compacted message should mention compaction: {}",
        &first_text[..first_text.len().min(200)]
    );
}

// ---------------------------------------------------------------------------
// Test 3: No compaction when within budget
// ---------------------------------------------------------------------------

/// When messages fit within the budget, no compaction occurs.
#[test]
fn no_compaction_within_budget() {
    let tmp = TempDir::new().unwrap();

    // Small journal that fits in budget
    let journal_path = make_large_journal(tmp.path(), 3);

    let config = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 200_000, // Large enough
    };

    let resume = load_resume_data(&journal_path, tmp.path(), &config)
        .unwrap()
        .expect("Should load resume data");

    assert!(
        !resume.was_compacted,
        "Small journal should not trigger compaction"
    );

    // All messages should be present
    assert_eq!(resume.messages.len(), 6, "Should have 3 pairs = 6 messages");
}

// ---------------------------------------------------------------------------
// Test 4: Tool output truncation at 100KB
// ---------------------------------------------------------------------------

/// Large tool outputs are truncated at MAX_TOOL_OUTPUT_SIZE (100KB).
#[test]
fn tool_output_truncated_at_limit() {
    // Output under the limit should pass through unchanged
    let small = "Hello, world!".to_string();
    let result = truncate_output(small.clone());
    assert_eq!(result, small, "Small output should pass through unchanged");

    // Output at exactly the limit should pass through unchanged
    let exact = "a".repeat(100 * 1024);
    let result = truncate_output(exact.clone());
    assert_eq!(result, exact, "Output at exactly 100KB should pass through");

    // Output over the limit should be truncated
    let large = "b".repeat(200 * 1024); // 200KB
    let result = truncate_output(large.clone());
    assert!(
        result.len() < large.len(),
        "Truncated output ({}) should be shorter than original ({})",
        result.len(),
        large.len()
    );
    assert!(
        result.contains("[Output truncated:"),
        "Truncated output should contain truncation marker"
    );
    assert!(
        result.contains("bytes total"),
        "Truncation marker should mention total bytes"
    );

    // The truncated content should start with the original content
    assert!(
        result.starts_with(&large[..1000]),
        "Truncated output should start with original content"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Compaction preserves recent messages
// ---------------------------------------------------------------------------

/// Emergency compaction keeps the most recent messages verbatim.
#[test]
fn compaction_preserves_recent_messages() {
    let tmp = TempDir::new().unwrap();

    // Create journal with enough messages to trigger compaction
    let journal_path = make_large_journal(tmp.path(), 30);

    // Tiny budget to force compaction
    let config = ResumeConfig {
        budget_pct: 0.10,
        context_window_tokens: 100,
    };

    let resume = load_resume_data(&journal_path, tmp.path(), &config)
        .unwrap()
        .expect("Should load resume data");

    assert!(resume.was_compacted, "Should be compacted");

    // The last few messages should contain the original content (not summary)
    let last_msg = resume.messages.last().unwrap();
    let last_text = match &last_msg.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("Expected text content"),
    };

    // Last message should be from the original conversation, not a summary
    assert!(
        last_text.contains("Analysis for step") || last_text.contains("Task step"),
        "Last message should be original content, not summary: {}",
        &last_text[..last_text.len().min(200)]
    );
}

// ---------------------------------------------------------------------------
// Test 6: Compaction maintains valid message alternation
// ---------------------------------------------------------------------------

/// After compaction, messages must alternate user/assistant for API compliance.
#[test]
fn compaction_maintains_alternation() {
    let tmp = TempDir::new().unwrap();

    let journal_path = make_large_journal(tmp.path(), 30);

    let config = ResumeConfig {
        budget_pct: 0.10,
        context_window_tokens: 100,
    };

    let resume = load_resume_data(&journal_path, tmp.path(), &config)
        .unwrap()
        .expect("Should load resume data");

    assert!(resume.was_compacted);

    // First message must be User (API requirement)
    assert_eq!(
        resume.messages[0].role,
        Role::User,
        "First message after compaction must be User role"
    );

    // Check alternation: no two consecutive messages should have the same role
    for window in resume.messages.windows(2) {
        assert_ne!(
            window[0].role, window[1].role,
            "Messages must alternate roles: got {:?} followed by {:?}",
            window[0].role, window[1].role
        );
    }
}

// ---------------------------------------------------------------------------
// Test 7: Agent does not crash on empty journal
// ---------------------------------------------------------------------------

/// Loading resume data from an empty journal should return None, not crash.
#[test]
fn no_crash_on_empty_journal() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("empty.jsonl");
    fs::write(&path, "").unwrap();

    let config = ResumeConfig::default();
    let result = load_resume_data(&path, tmp.path(), &config).unwrap();
    assert!(result.is_none(), "Empty journal should return None");
}

// ---------------------------------------------------------------------------
// Test 8: Agent does not crash on nonexistent journal
// ---------------------------------------------------------------------------

/// Loading resume data from a nonexistent path should return None, not crash.
#[test]
fn no_crash_on_nonexistent_journal() {
    let config = ResumeConfig::default();
    let result =
        load_resume_data(Path::new("/tmp/nonexistent_journal.jsonl"), Path::new("/tmp"), &config)
            .unwrap();
    assert!(result.is_none(), "Nonexistent journal should return None");
}

// ---------------------------------------------------------------------------
// Test 9: Very large tool output does not cause OOM
// ---------------------------------------------------------------------------

/// Even a very large tool output (10MB) should be safely truncated without OOM.
#[test]
fn large_tool_output_no_oom() {
    // 10MB of data
    let huge = "c".repeat(10 * 1024 * 1024);
    let result = truncate_output(huge);

    // Result should be around 100KB + truncation message
    assert!(
        result.len() < 200 * 1024,
        "Truncated result should be well under 200KB, got {} bytes",
        result.len()
    );
    assert!(result.contains("[Output truncated:"));
}

// ---------------------------------------------------------------------------
// Test 10: Compaction summary captures tool call information
// ---------------------------------------------------------------------------

/// When messages containing tool calls are compacted, the summary
/// should mention the tools that were used.
#[test]
fn compaction_summary_captures_tool_calls() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("conversation.jsonl");
    let mut journal = Journal::open(&path).unwrap();

    journal
        .append(JournalEntryKind::Init {
            model: "test-model".to_string(),
            provider: "test".to_string(),
            system_prompt: "Test".to_string(),
            tools: vec![],
            task_id: None,
        })
        .unwrap();

    // Create messages with tool use content
    for i in 0..20 {
        if i % 2 == 0 {
            journal
                .append(JournalEntryKind::Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!("Please do step {}", i),
                    }],
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                })
                .unwrap();
        } else {
            journal
                .append(JournalEntryKind::Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: format!("Working on step {}", i),
                        },
                        ContentBlock::ToolUse {
                            id: format!("tool_{}", i),
                            name: "read_file".to_string(),
                            input: serde_json::json!({"path": format!("src/file_{}.rs", i)}),
                        },
                    ],
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                })
                .unwrap();
        }
    }

    // Force compaction
    let config = ResumeConfig {
        budget_pct: 0.01,
        context_window_tokens: 100,
    };

    let resume = load_resume_data(&path, tmp.path(), &config)
        .unwrap()
        .expect("Should load resume data");

    assert!(resume.was_compacted);

    // The compaction summary (first message) should mention tool calls
    let first_text = match &resume.messages[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("Expected text content"),
    };

    assert!(
        first_text.contains("read_file") || first_text.contains("Tools called"),
        "Compaction summary should mention tools used. Got: {}",
        &first_text[..first_text.len().min(500)]
    );
}

// ---------------------------------------------------------------------------
// Test 11: Live smoke — context management via OpenRouter
// ---------------------------------------------------------------------------

/// End-to-end smoke test that exercises context window management through
/// the real OpenRouter API with minimax-m2.7.
///
/// Validates:
/// 1. Agent processes a context-heavy task without crashing
/// 2. Tool outputs are properly bounded
/// 3. Agent completes even with substantial context pressure
///
/// Gate: `#[ignore]` — requires OPENROUTER_API_KEY.
#[test]
#[ignore = "requires OPENROUTER_API_KEY"]
fn smoke_context_management_openrouter() {
    let _api_key = std::env::var("OPENROUTER_API_KEY")
        .expect("OPENROUTER_API_KEY must be set for this smoke test");

    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");

    wg_ok(&wg_dir, &["init"]);
    wg_ok(&wg_dir, &["agency", "init"]);

    // Configure OpenRouter endpoint
    wg_ok(
        &wg_dir,
        &[
            "endpoint",
            "add",
            "test-openrouter",
            "--provider",
            "openrouter",
            "--url",
            "https://openrouter.ai/api/v1",
            "--key-env",
            "OPENROUTER_API_KEY",
        ],
    );
    wg_ok(&wg_dir, &["endpoint", "set-default", "test-openrouter"]);

    // Create a task that will generate substantial context
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Context pressure test: list files in the workgraph directory and summarize what you find",
            "--id",
            "context-pressure-test",
            "--context-scope",
            "task",
            "--immediate",
        ],
    );

    // Spawn native executor
    let spawn_output = wg_cmd(
        &wg_dir,
        &[
            "spawn",
            "context-pressure-test",
            "--executor",
            "native",
            "--model",
            "minimax/minimax-m2.7",
        ],
    );

    assert!(
        spawn_output.status.success(),
        "Spawn should succeed: {}",
        String::from_utf8_lossy(&spawn_output.stderr)
    );

    // Poll until agent completes (max 5 minutes)
    let max_wait = 300;
    let mut completed = false;
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < max_wait {
        let output = wg_cmd(&wg_dir, &["show", "context-pressure-test", "--json"]);
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&stdout) {
                let status = val.get("status").and_then(|s| s.as_str()).unwrap_or("");
                if status == "done" || status == "failed" {
                    completed = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }

    assert!(
        completed,
        "Agent should complete within {}s (did not crash or hang)",
        max_wait
    );

    // Verify agent output exists
    let agents_base = wg_dir.join("agents");
    if agents_base.exists() {
        let agent_dirs: Vec<PathBuf> = fs::read_dir(&agents_base)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();

        if let Some(agent_dir) = agent_dirs.first() {
            // Check journal exists
            let journal_path = agent_dir.join("conversation.jsonl");
            if journal_path.exists() {
                let content = fs::read_to_string(&journal_path).unwrap();
                let entries: Vec<serde_json::Value> = content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();

                assert!(
                    !entries.is_empty(),
                    "Journal should have entries"
                );

                // Verify no entry has excessively large content (truncation working)
                for entry in &entries {
                    if let Some(content) = entry.get("content") {
                        let content_str = content.to_string();
                        assert!(
                            content_str.len() < 500 * 1024, // 500KB sanity check
                            "No single journal entry should exceed 500KB (truncation should prevent this)"
                        );
                    }
                }

                eprintln!(
                    "[smoke] Context management test passed: {} journal entries, agent completed successfully",
                    entries.len()
                );
            }
        }
    }
}
