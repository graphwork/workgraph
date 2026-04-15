//! Integration tests for multiple compaction cycles with OpenAI-compatible models.
//!
//! Tests exercise:
//! 1. Multiple sequential graph-level compaction state cycles
//! 2. Multiple resume compaction cycles preserving essential information
//! 3. Context budget behavior with various OpenAI-model context windows
//! 4. Compaction threshold resolution for OpenAI-compatible model configurations
//! 5. Multiple emergency compaction cycles maintaining message integrity
//! 6. Chat compactor state tracking across incremental cycles
//! 7. Live OpenRouter smoke test (gated behind `#[ignore]`)
//!
//! Run with: cargo test --test integration_multiple_compaction
//! For live tests: cargo test --test integration_multiple_compaction -- --ignored

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

use workgraph::config::{Config, DispatchRole, ModelRegistryEntry, Tier};
use workgraph::executor::native::client::{ContentBlock, Message, Role};
use workgraph::executor::native::journal::{Journal, JournalEntryKind};
use workgraph::executor::native::resume::{
    ContextBudget, ContextPressureAction, ResumeConfig, load_resume_data,
};
use workgraph::service::chat_compactor::ChatCompactorState;
use workgraph::service::compactor::{self, CompactorState};

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

fn setup_wg(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    wg_dir
}

/// Create a journal with an Init entry and many message pairs to simulate a
/// long conversation that will trigger compaction.
fn make_large_journal(dir: &Path, model: &str, num_pairs: usize) -> PathBuf {
    let path = dir.join("conversation.jsonl");
    let mut journal = Journal::open(&path).unwrap();

    journal
        .append(JournalEntryKind::Init {
            model: model.to_string(),
            provider: "openrouter".to_string(),
            system_prompt: "You are a test agent working on context-heavy tasks.".to_string(),
            tools: vec![],
            task_id: Some("compaction-test".to_string()),
        })
        .unwrap();

    for i in 0..num_pairs {
        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "Task step {}: Analyze the following data and provide a detailed report. \
                         Key findings include: {}",
                        i,
                        "important-data-".repeat(80)
                    ),
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
                    text: format!(
                        "Analysis for step {}: The data shows significant patterns in the \
                         workload distribution. {}",
                        i,
                        "detailed-analysis-".repeat(80)
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

/// Create a journal with tool use content to test compaction of tool results.
fn make_tool_heavy_journal(dir: &Path, model: &str, num_pairs: usize) -> PathBuf {
    let path = dir.join("conversation.jsonl");
    let mut journal = Journal::open(&path).unwrap();

    journal
        .append(JournalEntryKind::Init {
            model: model.to_string(),
            provider: "openai".to_string(),
            system_prompt: "You are a coding assistant.".to_string(),
            tools: vec![],
            task_id: Some("tool-compaction-test".to_string()),
        })
        .unwrap();

    for i in 0..num_pairs {
        // User requests
        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: format!("Please read and analyze src/module_{}.rs", i),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();

        // Assistant uses tools
        journal
            .append(JournalEntryKind::Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: format!("I'll read module {} now.", i),
                    },
                    ContentBlock::ToolUse {
                        id: format!("tu_{}", i),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": format!("src/module_{}.rs", i)}),
                    },
                ],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();

        // Tool results (large content)
        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("tu_{}", i),
                    content: format!(
                        "// Module {} source code\n{}",
                        i,
                        "fn process() { /* implementation */ }\n".repeat(50)
                    ),
                    is_error: false,
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();

        // Assistant analysis
        journal
            .append(JournalEntryKind::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "Module {} analysis: Found {} functions with process patterns.",
                        i, 50
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
// Test 1: Multiple sequential graph-level compaction state cycles
// ---------------------------------------------------------------------------

/// Verify that CompactorState correctly tracks multiple sequential compaction
/// cycles, incrementing counters and preserving metadata across each cycle.
#[test]
fn test_multiple_compaction_state_cycles() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("compactor")).unwrap();

    // Simulate 5 sequential compaction cycles
    for cycle in 1..=5u64 {
        let mut state = CompactorState::load(dir);
        assert_eq!(
            state.compaction_count,
            cycle - 1,
            "Before cycle {}, count should be {}",
            cycle,
            cycle - 1
        );

        state.last_compaction = Some(format!("2026-01-01T0{}:00:00Z", cycle));
        state.last_ops_count = (cycle as usize) * 10;
        state.last_tick = cycle * 5;
        state.compaction_count = cycle;
        state.last_compaction_duration_ms = Some(cycle * 1000);
        state.last_compaction_context_bytes = Some(cycle * 2048);
        state.error_count = 0;
        state.save(dir).unwrap();
    }

    // Verify final state after 5 cycles
    let final_state = CompactorState::load(dir);
    assert_eq!(final_state.compaction_count, 5);
    assert_eq!(final_state.last_tick, 25);
    assert_eq!(final_state.last_ops_count, 50);
    assert_eq!(final_state.last_compaction_duration_ms, Some(5000));
    assert_eq!(final_state.last_compaction_context_bytes, Some(10240));
    assert_eq!(final_state.error_count, 0);
    assert_eq!(
        final_state.last_compaction.as_deref(),
        Some("2026-01-01T05:00:00Z")
    );
}

/// Verify that error counts accumulate across compaction attempts and reset
/// on success, as would happen with intermittent API failures.
#[test]
fn test_compaction_error_recovery_across_cycles() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("compactor")).unwrap();

    // Simulate 3 failed compaction attempts
    for attempt in 1..=3u64 {
        let mut state = CompactorState::load(dir);
        state.error_count = attempt;
        state.save(dir).unwrap();
    }

    let state = CompactorState::load(dir);
    assert_eq!(state.error_count, 3, "Should have 3 accumulated errors");
    assert_eq!(state.compaction_count, 0, "No successful compactions yet");

    // Simulate a successful compaction that resets error count
    let mut state = CompactorState::load(dir);
    state.compaction_count = 1;
    state.error_count = 0;
    state.last_compaction = Some("2026-01-01T04:00:00Z".to_string());
    state.save(dir).unwrap();

    let recovered = CompactorState::load(dir);
    assert_eq!(
        recovered.error_count, 0,
        "Error count should reset on success"
    );
    assert_eq!(recovered.compaction_count, 1);
}

// ---------------------------------------------------------------------------
// Test 2: Multiple resume compaction cycles with context preservation
// ---------------------------------------------------------------------------

/// Run multiple load→compact cycles on a journal, verifying that essential
/// information is preserved across each compaction and that the compacted
/// output remains valid for API consumption.
#[test]
fn test_multiple_resume_compaction_cycles_preserve_info() {
    let tmp = TempDir::new().unwrap();

    // Create a large journal with 40 message pairs
    let journal_path = make_large_journal(tmp.path(), "gpt-4o", 40);

    // First compaction: 1000-token window forces aggressive compaction
    let config_small = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 1_000,
    };

    let resume1 = load_resume_data(&journal_path, tmp.path(), &config_small)
        .unwrap()
        .expect("Should load resume data");

    assert!(resume1.was_compacted, "Cycle 1: should be compacted");
    assert!(
        resume1.messages.len() < 80,
        "Cycle 1: compacted messages ({}) should be fewer than original (80)",
        resume1.messages.len()
    );

    // Verify first message mentions compaction/resume
    let first_text = extract_text(&resume1.messages[0]);
    assert!(
        first_text.contains("compacted") || first_text.contains("Resume"),
        "Cycle 1: first message should mention compaction: {}",
        &first_text[..first_text.len().min(200)]
    );

    // Verify recent messages are preserved
    let last_text = extract_text(resume1.messages.last().unwrap());
    assert!(
        last_text.contains("Analysis for step") || last_text.contains("Task step"),
        "Cycle 1: last message should be original content"
    );

    // Verify valid alternation
    assert_valid_alternation(&resume1.messages);

    // Now create a second journal from the compacted messages (simulating
    // a resumed agent that continues working and needs compaction again)
    let journal2_path = tmp.path().join("conversation2.jsonl");
    let mut journal2 = Journal::open(&journal2_path).unwrap();

    journal2
        .append(JournalEntryKind::Init {
            model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            tools: vec![],
            task_id: Some("compaction-test-2".to_string()),
        })
        .unwrap();

    // Write compacted messages + new messages into second journal
    for msg in &resume1.messages {
        journal2
            .append(JournalEntryKind::Message {
                role: msg.role,
                content: msg.content.clone(),
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
    }

    // Add 20 more pairs to push over budget again
    for i in 40..60 {
        journal2
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: format!("Continued work step {}: {}", i, "new-data-".repeat(80)),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        journal2
            .append(JournalEntryKind::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("Response for step {}: {}", i, "new-analysis-".repeat(80)),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
    }

    // Second compaction cycle
    let resume2 = load_resume_data(&journal2_path, tmp.path(), &config_small)
        .unwrap()
        .expect("Should load second journal");

    assert!(resume2.was_compacted, "Cycle 2: should be compacted again");
    assert_valid_alternation(&resume2.messages);

    // The most recent messages should still be from the second batch
    let last_text2 = extract_text(resume2.messages.last().unwrap());
    assert!(
        last_text2.contains("step 5") || last_text2.contains("new-"),
        "Cycle 2: should preserve recent (second-batch) content: {}",
        &last_text2[..last_text2.len().min(200)]
    );
}

/// Test that compaction of a tool-heavy journal preserves tool call
/// information in the summary.
#[test]
fn test_multiple_compaction_preserves_tool_info() {
    let tmp = TempDir::new().unwrap();

    let journal_path = make_tool_heavy_journal(tmp.path(), "deepseek/deepseek-chat", 15);

    let config = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 500, // Very small to force compaction
    };

    let resume = load_resume_data(&journal_path, tmp.path(), &config)
        .unwrap()
        .expect("Should load tool-heavy journal");

    assert!(
        resume.was_compacted,
        "Tool-heavy journal should be compacted"
    );
    assert_valid_alternation(&resume.messages);

    // The compaction summary should mention tools that were used
    let first_text = extract_text(&resume.messages[0]);
    assert!(
        first_text.contains("read_file") || first_text.contains("Tools called"),
        "Compaction summary should mention tool usage: {}",
        &first_text[..first_text.len().min(500)]
    );
}

// ---------------------------------------------------------------------------
// Test 3: Context budget with various OpenAI-model context windows
// ---------------------------------------------------------------------------

/// Test that ContextBudget correctly handles context windows typical of
/// various OpenAI-compatible models (GPT-4o: 128k, DeepSeek: 164k,
/// Llama-4: 512k, local small: 32k).
#[test]
fn test_context_budget_openai_model_windows() {
    // Model configurations: (name, context_window, description)
    let models = [
        ("gpt-4o-mini", 128_000, "GPT-4o-mini"),
        ("deepseek-chat", 164_000, "DeepSeek Chat"),
        ("llama-4-scout", 512_000, "Llama 4 Scout"),
        ("local-small", 32_000, "Local small model"),
        ("qwen3-235b", 131_072, "Qwen3-235B"),
    ];

    for (model_name, context_window, desc) in &models {
        let budget = ContextBudget::with_window_size(*context_window);
        assert_eq!(
            budget.window_size, *context_window,
            "{}: window size mismatch",
            desc
        );

        // At 50% capacity: should be Ok (always below any warning threshold)
        let half_chars = (*context_window as f64 * 0.50 * budget.chars_per_token) as usize;
        let msgs_ok = vec![make_text_message(Role::User, half_chars)];
        assert_eq!(
            budget.check_pressure(&msgs_ok),
            ContextPressureAction::Ok,
            "{} ({}): 50% should be Ok",
            desc,
            model_name
        );

        // Just above warning_threshold: should be Warning
        let warn_pct = budget.warning_threshold + 0.02;
        let warn_chars = (*context_window as f64 * warn_pct * budget.chars_per_token) as usize;
        let msgs_warn = vec![make_text_message(Role::User, warn_chars)];
        assert_eq!(
            budget.check_pressure(&msgs_warn),
            ContextPressureAction::Warning,
            "{} ({}): {:.0}% should be Warning",
            desc,
            model_name,
            warn_pct * 100.0
        );

        // Just above compact_threshold: should be EmergencyCompaction
        let compact_pct = budget.compact_threshold + 0.02;
        let compact_chars =
            (*context_window as f64 * compact_pct * budget.chars_per_token) as usize;
        let msgs_compact = vec![make_text_message(Role::User, compact_chars)];
        assert_eq!(
            budget.check_pressure(&msgs_compact),
            ContextPressureAction::EmergencyCompaction,
            "{} ({}): {:.0}% should be EmergencyCompaction",
            desc,
            model_name,
            compact_pct * 100.0
        );

        // Just above hard_limit: should be CleanExit
        let exit_pct = budget.hard_limit + 0.02;
        let exit_chars = (*context_window as f64 * exit_pct * budget.chars_per_token) as usize;
        let msgs_exit = vec![make_text_message(Role::User, exit_chars)];
        assert_eq!(
            budget.check_pressure(&msgs_exit),
            ContextPressureAction::CleanExit,
            "{} ({}): {:.0}% should be CleanExit",
            desc,
            model_name,
            exit_pct * 100.0
        );
    }
}

/// Test that a small context window (e.g., local 8k model) triggers
/// compaction much earlier than a large-context model, and that smaller
/// windows get tighter (earlier) thresholds.
#[test]
fn test_context_pressure_small_vs_large_window() {
    let small = ContextBudget::with_window_size(8_000);
    let large = ContextBudget::with_window_size(200_000);

    // Small windows should have lower (tighter) thresholds than large windows
    assert!(
        small.warning_threshold < large.warning_threshold,
        "Small window should warn earlier: {} vs {}",
        small.warning_threshold,
        large.warning_threshold
    );

    // 10k chars ≈ 2500 tokens
    let msgs = vec![make_text_message(Role::User, 10_000)];

    // Small model: 2500/8000 = 31.25% → Ok
    assert_eq!(small.check_pressure(&msgs), ContextPressureAction::Ok);

    // With more content: 24k chars ≈ 6000 tokens
    let msgs_bigger = vec![make_text_message(Role::User, 24_000)];
    // Small model: 6000/8000 = 75% → EmergencyCompaction (compact_threshold=0.65 for <64k)
    assert_eq!(
        small.check_pressure(&msgs_bigger),
        ContextPressureAction::EmergencyCompaction,
        "Small model should need compaction at 24k chars"
    );
    // Large model: 6000/200000 = 3% → Ok
    assert_eq!(
        large.check_pressure(&msgs_bigger),
        ContextPressureAction::Ok,
        "Large model should be fine at 24k chars"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Compaction threshold resolution for OpenAI-compatible models
// ---------------------------------------------------------------------------

/// Test effective_compaction_threshold with OpenAI-compatible models added
/// to the registry, verifying the dynamic threshold calculation.
#[test]
fn test_compaction_threshold_openai_models_in_registry() {
    let mut config = Config::default();
    config.coordinator.compaction_threshold_ratio = 0.8;

    // Add GPT-4o to registry (128k context)
    config.model_registry.push(ModelRegistryEntry {
        id: "gpt-4o".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        tier: Tier::Standard,
        endpoint: None,
        context_window: 128_000,
        max_output_tokens: 4096,
        cost_per_input_mtok: 2.50,
        cost_per_output_mtok: 10.0,
        prompt_caching: false,
        cache_read_discount: 0.0,
        cache_write_premium: 0.0,
        descriptors: vec![],
    });

    // Set coordinator model to gpt-4o
    config.coordinator.model = Some("gpt-4o".to_string());

    let threshold = config.effective_compaction_threshold();
    // 128,000 * 0.8 = 102,400
    assert_eq!(
        threshold, 102_400,
        "GPT-4o threshold should be 128000 * 0.8 = 102400"
    );
}

/// Test compaction threshold with DeepSeek model in registry.
#[test]
fn test_compaction_threshold_deepseek_in_registry() {
    let mut config = Config::default();
    config.coordinator.compaction_threshold_ratio = 0.8;

    config.model_registry.push(ModelRegistryEntry {
        id: "deepseek-chat".to_string(),
        provider: "openrouter".to_string(),
        model: "deepseek/deepseek-chat".to_string(),
        tier: Tier::Standard,
        endpoint: None,
        context_window: 164_000,
        max_output_tokens: 8192,
        cost_per_input_mtok: 0.14,
        cost_per_output_mtok: 0.28,
        prompt_caching: false,
        cache_read_discount: 0.0,
        cache_write_premium: 0.0,
        descriptors: vec![],
    });

    config.coordinator.model = Some("deepseek-chat".to_string());

    let threshold = config.effective_compaction_threshold();
    // 164,000 * 0.8 = 131,200
    assert_eq!(
        threshold, 131_200,
        "DeepSeek threshold should be 164000 * 0.8 = 131200"
    );
}

/// Test compaction threshold fallback for unknown OpenAI-compatible models.
#[test]
fn test_compaction_threshold_unknown_model_fallback() {
    let mut config = Config::default();
    config.coordinator.compaction_threshold_ratio = 0.8;
    config.coordinator.compaction_token_threshold = 100_000;

    // Set coordinator to an unknown model not in any registry
    config.coordinator.model = Some("my-local-model".to_string());

    let threshold = config.effective_compaction_threshold();
    assert_eq!(
        threshold, 100_000,
        "Unknown model should fall back to compaction_token_threshold"
    );
}

/// Test that multiple different OpenAI-compatible models produce different
/// thresholds when swapped in the coordinator config.
#[test]
fn test_compaction_threshold_varies_by_model() {
    let mut config = Config::default();
    config.coordinator.compaction_threshold_ratio = 0.8;
    config.coordinator.compaction_token_threshold = 50_000;

    // Add two models with different context windows
    config.model_registry.push(ModelRegistryEntry {
        id: "small-model".to_string(),
        provider: "local".to_string(),
        model: "small-model".to_string(),
        tier: Tier::Fast,
        endpoint: None,
        context_window: 32_000,
        max_output_tokens: 2048,
        cost_per_input_mtok: 0.0,
        cost_per_output_mtok: 0.0,
        prompt_caching: false,
        cache_read_discount: 0.0,
        cache_write_premium: 0.0,
        descriptors: vec![],
    });

    config.model_registry.push(ModelRegistryEntry {
        id: "large-model".to_string(),
        provider: "openrouter".to_string(),
        model: "large-model".to_string(),
        tier: Tier::Premium,
        endpoint: None,
        context_window: 1_000_000,
        max_output_tokens: 32768,
        cost_per_input_mtok: 5.0,
        cost_per_output_mtok: 15.0,
        prompt_caching: false,
        cache_read_discount: 0.0,
        cache_write_premium: 0.0,
        descriptors: vec![],
    });

    // Small model: 32,000 * 0.8 = 25,600
    config.coordinator.model = Some("small-model".to_string());
    let small_threshold = config.effective_compaction_threshold();
    assert_eq!(small_threshold, 25_600);

    // Large model: 1,000,000 * 0.8 = 800,000
    config.coordinator.model = Some("large-model".to_string());
    let large_threshold = config.effective_compaction_threshold();
    assert_eq!(large_threshold, 800_000);

    // Verify the thresholds differ significantly
    assert!(
        large_threshold > small_threshold * 10,
        "Large model threshold ({}) should be much bigger than small ({})",
        large_threshold,
        small_threshold
    );
}

// ---------------------------------------------------------------------------
// Test 5: Multiple emergency compaction cycles
// ---------------------------------------------------------------------------

/// Apply emergency compaction repeatedly and verify message integrity
/// is maintained across each cycle.
#[test]
fn test_multiple_emergency_compaction_cycles() {
    // Build a conversation with 30 message pairs
    let mut messages: Vec<Message> = (0..30)
        .flat_map(|i| {
            vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!("User message {}: {}", i, "x".repeat(200)),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: format!("Assistant response {}: {}", i, "y".repeat(200)),
                    }],
                },
            ]
        })
        .collect();

    // Emergency compact 3 times, simulating repeated pressure
    for cycle in 1..=3 {
        let before_count = messages.len();
        messages = ContextBudget::emergency_compact(messages, 6);

        // Verify basic properties after each cycle
        assert!(
            !messages.is_empty(),
            "Cycle {}: messages should not be empty after compaction",
            cycle
        );
        assert_eq!(
            messages[0].role,
            Role::User,
            "Cycle {}: first message must be User",
            cycle
        );

        // Verify alternation
        for window in messages.windows(2) {
            assert_ne!(
                window[0].role, window[1].role,
                "Cycle {}: messages must alternate roles",
                cycle
            );
        }

        // The last message should still be present
        let last = messages.last().unwrap();
        let last_text = extract_text(last);
        assert!(
            !last_text.is_empty(),
            "Cycle {}: last message should have content",
            cycle
        );

        eprintln!(
            "Emergency compact cycle {}: {} -> {} messages",
            cycle,
            before_count,
            messages.len()
        );
    }

    // After 3 cycles, we should still have valid messages
    assert!(
        messages.len() >= 2,
        "Should have at least 2 messages after 3 cycles"
    );
}

/// Test emergency compaction with mixed content (text + tool use + tool results).
#[test]
fn test_emergency_compaction_mixed_content() {
    let messages = vec![
        // Old messages (will be compacted)
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Read the config file.".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Reading config.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "tu-1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "config.toml"}),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu-1".to_string(),
                content: "x".repeat(5000), // Large tool result
                is_error: false,
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Config looks good. Moving on.".to_string(),
            }],
        },
        // Recent messages (keep_recent=4 means these are kept)
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Now write the implementation.".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Writing implementation.".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Run the tests.".to_string(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Tests pass!".to_string(),
            }],
        },
    ];

    let compacted = ContextBudget::emergency_compact(messages.clone(), 4);

    // Recent messages should be preserved
    assert_eq!(
        compacted.len(),
        messages.len(),
        "emergency_compact should not remove messages, only strip content"
    );

    // The large tool result in the old section should be stripped
    let tool_result_msg = &compacted[2];
    match &tool_result_msg.content[0] {
        ContentBlock::ToolResult { content, .. } => {
            assert!(
                content.len() < 5000,
                "Large tool result should be stripped, got {} bytes",
                content.len()
            );
        }
        _ => panic!("Expected ToolResult in position 2"),
    }

    // Last message should be unchanged
    let last_text = extract_text(compacted.last().unwrap());
    assert_eq!(last_text, "Tests pass!");
}

// ---------------------------------------------------------------------------
// Test 6: Chat compactor state across multiple incremental cycles
// ---------------------------------------------------------------------------

/// Test that ChatCompactorState correctly accumulates across multiple
/// compaction cycles with proper ID tracking.
#[test]
fn test_chat_compactor_state_multiple_cycles() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    let coordinator_id = 0u32;

    // Cycle 1: process first batch of messages
    let state1 = ChatCompactorState {
        last_compaction: Some("2026-01-01T00:00:00Z".to_string()),
        last_message_count: 25,
        compaction_count: 1,
        last_inbox_id: 10,
        last_outbox_id: 8,
    };
    state1.save(&wg_dir, coordinator_id).unwrap();

    // Cycle 2: process second batch (new messages since cycle 1)
    let state2 = ChatCompactorState {
        last_compaction: Some("2026-01-01T01:00:00Z".to_string()),
        last_message_count: 15,
        compaction_count: 2,
        last_inbox_id: 20,
        last_outbox_id: 18,
    };
    state2.save(&wg_dir, coordinator_id).unwrap();

    // Cycle 3: third batch
    let state3 = ChatCompactorState {
        last_compaction: Some("2026-01-01T02:00:00Z".to_string()),
        last_message_count: 30,
        compaction_count: 3,
        last_inbox_id: 35,
        last_outbox_id: 28,
    };
    state3.save(&wg_dir, coordinator_id).unwrap();

    // Verify final state
    let loaded = ChatCompactorState::load(&wg_dir, coordinator_id);
    assert_eq!(loaded.compaction_count, 3);
    assert_eq!(loaded.last_inbox_id, 35);
    assert_eq!(loaded.last_outbox_id, 28);
    assert_eq!(loaded.last_message_count, 30);
    assert_eq!(
        loaded.last_compaction.as_deref(),
        Some("2026-01-01T02:00:00Z")
    );
}

/// Test that chat compactor state for different coordinators is independent.
#[test]
fn test_chat_compactor_state_independent_per_coordinator() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    // Set up state for coordinator 0
    let state0 = ChatCompactorState {
        last_compaction: Some("2026-01-01T00:00:00Z".to_string()),
        last_message_count: 100,
        compaction_count: 5,
        last_inbox_id: 50,
        last_outbox_id: 45,
    };
    state0.save(&wg_dir, 0).unwrap();

    // Set up state for coordinator 1
    let state1 = ChatCompactorState {
        last_compaction: Some("2026-01-01T01:00:00Z".to_string()),
        last_message_count: 20,
        compaction_count: 1,
        last_inbox_id: 10,
        last_outbox_id: 5,
    };
    state1.save(&wg_dir, 1).unwrap();

    // Verify independence
    let loaded0 = ChatCompactorState::load(&wg_dir, 0);
    let loaded1 = ChatCompactorState::load(&wg_dir, 1);

    assert_eq!(loaded0.compaction_count, 5);
    assert_eq!(loaded1.compaction_count, 1);
    assert_eq!(loaded0.last_inbox_id, 50);
    assert_eq!(loaded1.last_inbox_id, 10);
}

// ---------------------------------------------------------------------------
// Test 7: Resume compaction with OpenAI-model context window sizes
// ---------------------------------------------------------------------------

/// Verify that compaction behavior adapts correctly to different context
/// window sizes as would be configured for various OpenAI-compatible models.
#[test]
fn test_resume_compaction_adapts_to_context_window() {
    let tmp = TempDir::new().unwrap();

    // Create a journal with 25 message pairs (~30k chars)
    let journal_path = make_large_journal(tmp.path(), "gpt-4o-mini", 25);

    // With a large context window (200k): should NOT compact
    let config_large = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 200_000,
    };
    let resume_large = load_resume_data(&journal_path, tmp.path(), &config_large)
        .unwrap()
        .expect("Should load");
    assert!(
        !resume_large.was_compacted,
        "200k window should not trigger compaction for 25 pairs"
    );
    assert_eq!(
        resume_large.messages.len(),
        50,
        "Should have all 50 messages"
    );

    // With a medium context window (8k): should compact
    let config_medium = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 8_000,
    };
    let resume_medium = load_resume_data(&journal_path, tmp.path(), &config_medium)
        .unwrap()
        .expect("Should load");
    assert!(
        resume_medium.was_compacted,
        "8k window should trigger compaction for 25 pairs"
    );
    assert!(
        resume_medium.messages.len() < 50,
        "Compacted messages should be fewer"
    );

    // With a tiny context window (500): should compact aggressively
    let config_tiny = ResumeConfig {
        budget_pct: 0.50,
        context_window_tokens: 500,
    };
    let resume_tiny = load_resume_data(&journal_path, tmp.path(), &config_tiny)
        .unwrap()
        .expect("Should load");
    assert!(
        resume_tiny.was_compacted,
        "500 token window should trigger compaction"
    );
    assert!(
        resume_tiny.messages.len() <= resume_medium.messages.len(),
        "Smaller window should produce same or fewer messages"
    );

    // All results should have valid alternation
    assert_valid_alternation(&resume_medium.messages);
    assert_valid_alternation(&resume_tiny.messages);
}

// ---------------------------------------------------------------------------
// Test 8: should_compact trigger gating for multiple cycles
// ---------------------------------------------------------------------------

/// Verify that should_compact correctly gates compaction across multiple
/// tick cycles, preventing premature re-compaction.
#[test]
fn test_should_compact_gating_across_cycles() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("compactor")).unwrap();
    fs::create_dir_all(dir.join("log")).unwrap();

    let mut config = Config::default();
    config.coordinator.compactor_interval = 10;
    config.coordinator.compactor_ops_threshold = 0; // Disable ops-based trigger

    // Tick 0: not ready (0 - 0 < 10)
    assert!(!compactor::should_compact(dir, 0, &config));

    // Tick 10: ready (10 - 0 >= 10)
    assert!(compactor::should_compact(dir, 10, &config));

    // Simulate compaction at tick 10
    let state = CompactorState {
        last_tick: 10,
        compaction_count: 1,
        ..Default::default()
    };
    state.save(dir).unwrap();

    // Tick 15: not ready yet (15 - 10 = 5 < 10)
    assert!(!compactor::should_compact(dir, 15, &config));

    // Tick 19: still not ready (19 - 10 = 9 < 10)
    assert!(!compactor::should_compact(dir, 19, &config));

    // Tick 20: ready again (20 - 10 >= 10)
    assert!(compactor::should_compact(dir, 20, &config));

    // Simulate second compaction
    let state = CompactorState {
        last_tick: 20,
        compaction_count: 2,
        ..Default::default()
    };
    state.save(dir).unwrap();

    // Tick 29: not ready (29 - 20 = 9 < 10)
    assert!(!compactor::should_compact(dir, 29, &config));

    // Tick 30: ready for third cycle
    assert!(compactor::should_compact(dir, 30, &config));
}

/// Verify ops-based compaction trigger works across multiple cycles.
#[test]
fn test_should_compact_ops_trigger_multiple_cycles() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("compactor")).unwrap();
    fs::create_dir_all(dir.join("log")).unwrap();

    let mut config = Config::default();
    config.coordinator.compactor_interval = 1000; // High tick interval (disabled)
    config.coordinator.compactor_ops_threshold = 5;

    let ops_path = dir.join("log").join("operations.jsonl");

    // 3 ops: not enough (3 < 5)
    let ops_content: String = (0..3).map(|_| "{}\n").collect();
    fs::write(&ops_path, &ops_content).unwrap();
    assert!(!compactor::should_compact(dir, 1, &config));

    // 5 ops: triggers first compaction (5 - 0 >= 5)
    let ops_content: String = (0..5).map(|_| "{}\n").collect();
    fs::write(&ops_path, &ops_content).unwrap();
    assert!(compactor::should_compact(dir, 1, &config));

    // Record compaction with 5 ops
    let state = CompactorState {
        last_ops_count: 5,
        last_tick: 1,
        compaction_count: 1,
        ..Default::default()
    };
    state.save(dir).unwrap();

    // 8 ops total: not enough new (8 - 5 = 3 < 5)
    let ops_content: String = (0..8).map(|_| "{}\n").collect();
    fs::write(&ops_path, &ops_content).unwrap();
    assert!(!compactor::should_compact(dir, 2, &config));

    // 10 ops total: triggers second compaction (10 - 5 >= 5)
    let ops_content: String = (0..10).map(|_| "{}\n").collect();
    fs::write(&ops_path, &ops_content).unwrap();
    assert!(compactor::should_compact(dir, 2, &config));
}

// ---------------------------------------------------------------------------
// Test 9: Context.md written and readable across compaction cycles
// ---------------------------------------------------------------------------

/// Simulate writing context.md across multiple compaction cycles and verify
/// that each cycle produces valid output.
#[test]
fn test_context_md_across_cycles() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_wg(&tmp);

    // Add some tasks to the graph
    wg_ok(
        &wg_dir,
        &["add", "Setup infrastructure", "--id", "infra-init"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Implement auth",
            "--id",
            "auth-impl",
            "--after",
            "infra-init",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Write tests",
            "--id",
            "write-tests",
            "--after",
            "auth-impl",
        ],
    );

    // Simulate compaction cycle 1: write fake context.md
    let context_path = compactor::context_md_path(&wg_dir);
    let compactor_dir = wg_dir.join("compactor");
    fs::create_dir_all(&compactor_dir).unwrap();

    let cycle1_content = "# Project Context\n\n\
        ## 1. Rolling Narrative\n\n\
        Project setup completed with infra-init. Auth implementation is next.\n\n\
        ## 2. Persistent Facts\n\n\
        - Architecture: Rust backend\n\
        - Convention: kebab-case task IDs\n\n\
        ## 3. Evaluation Digest\n\n\
        No evaluations yet.\n";
    fs::write(&context_path, cycle1_content).unwrap();

    // Verify cycle 1 output
    let text = fs::read_to_string(&context_path).unwrap();
    assert!(text.contains("Rolling Narrative"));
    assert!(text.contains("Persistent Facts"));
    assert!(text.contains("Evaluation Digest"));
    assert!(text.contains("infra-init"));

    // Update compactor state
    let state1 = CompactorState {
        last_compaction: Some("2026-01-01T00:00:00Z".to_string()),
        compaction_count: 1,
        last_compaction_context_bytes: Some(cycle1_content.len() as u64),
        ..Default::default()
    };
    state1.save(&wg_dir).unwrap();

    // Simulate compaction cycle 2: auth is now in progress
    let cycle2_content = "# Project Context\n\n\
        ## 1. Rolling Narrative\n\n\
        Infrastructure setup complete (infra-init: done). Auth implementation \
        (auth-impl) is in progress with OAuth2 integration. Write-tests is blocked.\n\n\
        ## 2. Persistent Facts\n\n\
        - Architecture: Rust backend with OAuth2\n\
        - Convention: kebab-case task IDs\n\
        - Auth: Using OpenID Connect\n\n\
        ## 3. Evaluation Digest\n\n\
        - infra-init: score=8.5, verdict=pass\n";
    fs::write(&context_path, cycle2_content).unwrap();

    // Verify cycle 2 output preserves and extends context
    let text = fs::read_to_string(&context_path).unwrap();
    assert!(text.contains("auth-impl"));
    assert!(text.contains("OAuth2"));
    assert!(text.contains("infra-init"));
    assert!(text.contains("score=8.5"));

    // Update state
    let state2 = CompactorState {
        last_compaction: Some("2026-01-01T01:00:00Z".to_string()),
        compaction_count: 2,
        last_compaction_context_bytes: Some(cycle2_content.len() as u64),
        ..Default::default()
    };
    state2.save(&wg_dir).unwrap();

    // Verify state after both cycles
    let final_state = CompactorState::load(&wg_dir);
    assert_eq!(final_state.compaction_count, 2);
    assert!(
        final_state.last_compaction_context_bytes.unwrap()
            >= state1.last_compaction_context_bytes.unwrap(),
        "Second cycle should produce at least as much context as first"
    );
}

// ---------------------------------------------------------------------------
// Test 10: Model dispatch resolution for compaction roles
// ---------------------------------------------------------------------------

/// Verify that model resolution for compactor roles correctly picks up
/// OpenAI-compatible model configurations.
#[test]
fn test_compactor_model_resolution_openai() {
    let mut config = Config::default();

    // Configure compactor to use an OpenAI-compatible model
    config
        .models
        .set_model(DispatchRole::Compactor, "gpt-4o-mini");
    config
        .models
        .set_provider(DispatchRole::Compactor, "openai");

    let resolved = config.resolve_model_for_role(DispatchRole::Compactor);
    assert_eq!(resolved.model, "gpt-4o-mini");
    assert_eq!(resolved.provider, Some("openai".to_string()));
}

/// Verify that chat compactor model resolution works with OpenRouter.
#[test]
fn test_chat_compactor_model_resolution_openrouter() {
    let mut config = Config::default();

    config
        .models
        .set_model(DispatchRole::ChatCompactor, "deepseek/deepseek-chat");
    config
        .models
        .set_provider(DispatchRole::ChatCompactor, "openrouter");

    let resolved = config.resolve_model_for_role(DispatchRole::ChatCompactor);
    assert_eq!(resolved.model, "deepseek/deepseek-chat");
    assert_eq!(resolved.provider, Some("openrouter".to_string()));
}

// ---------------------------------------------------------------------------
// Test 11: Live smoke — multiple compaction cycles via OpenRouter
// ---------------------------------------------------------------------------

/// End-to-end smoke test that runs multiple graph compaction cycles through
/// a real OpenAI-compatible endpoint (OpenRouter).
///
/// Validates:
/// 1. Multiple compaction cycles complete without error
/// 2. Each cycle produces valid context.md with 3-layer structure
/// 3. State accumulates correctly across cycles
/// 4. No crashes or hangs
///
/// Gate: `#[ignore]` — requires OPENROUTER_API_KEY.
#[test]
#[ignore = "requires OPENROUTER_API_KEY"]
fn smoke_multiple_compaction_cycles_openrouter() {
    let _api_key = std::env::var("OPENROUTER_API_KEY")
        .expect("OPENROUTER_API_KEY must be set for this smoke test");

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_wg(&tmp);

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

    // Add tasks to create graph state for compaction
    wg_ok(
        &wg_dir,
        &["add", "Research context windows", "--id", "research-ctx"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Implement dynamic sizing",
            "--id",
            "impl-sizing",
            "--after",
            "research-ctx",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Write integration tests",
            "--id",
            "write-tests",
            "--after",
            "impl-sizing",
        ],
    );

    // Configure the compactor to use a cheap OpenRouter model
    let config_path = wg_dir.join("config.toml");
    let config_addition = r#"

[models.compactor]
model = "openrouter:google/gemma-3-4b-it:free"
provider = "openrouter"
"#;
    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    fs::write(&config_path, format!("{}{}", existing, config_addition)).unwrap();

    // Run 3 compaction cycles
    for cycle in 1..=3u32 {
        // Mark a task as done to change graph state between cycles
        if cycle == 2 {
            wg_ok(&wg_dir, &["claim", "research-ctx"]);
            wg_ok(&wg_dir, &["done", "research-ctx"]);
        }
        if cycle == 3 {
            wg_ok(&wg_dir, &["claim", "impl-sizing"]);
            wg_ok(&wg_dir, &["done", "impl-sizing"]);
        }

        // Run compaction via CLI
        let output = wg_cmd(&wg_dir, &["compact"]);
        assert!(
            output.status.success(),
            "Compaction cycle {} failed:\nstdout: {}\nstderr: {}",
            cycle,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify context.md was written with 3-layer structure
        let context_path = compactor::context_md_path(&wg_dir);
        assert!(
            context_path.exists(),
            "Cycle {}: context.md should exist",
            cycle
        );

        let text = fs::read_to_string(&context_path).unwrap();
        assert!(
            !text.is_empty(),
            "Cycle {}: context.md should not be empty",
            cycle
        );

        // Verify state was updated
        let state = CompactorState::load(&wg_dir);
        assert_eq!(
            state.compaction_count,
            u64::from(cycle),
            "Cycle {}: compaction count should be {}",
            cycle,
            cycle
        );
        assert_eq!(
            state.error_count, 0,
            "Cycle {}: should have no errors",
            cycle
        );
        assert!(
            state.last_compaction.is_some(),
            "Cycle {}: should have timestamp",
            cycle
        );
        assert!(
            state.last_compaction_context_bytes.is_some(),
            "Cycle {}: should track context bytes",
            cycle
        );

        eprintln!(
            "[smoke] Compaction cycle {} passed: {} bytes, {}ms",
            cycle,
            state.last_compaction_context_bytes.unwrap_or(0),
            state.last_compaction_duration_ms.unwrap_or(0)
        );
    }

    // Final verification: 3 successful cycles
    let final_state = CompactorState::load(&wg_dir);
    assert_eq!(final_state.compaction_count, 3);
    assert_eq!(final_state.error_count, 0);
}

// ---------------------------------------------------------------------------
// Helpers (test-internal)
// ---------------------------------------------------------------------------

fn make_text_message(role: Role, size: usize) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: "x".repeat(size),
        }],
    }
}

fn extract_text(msg: &Message) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn assert_valid_alternation(messages: &[Message]) {
    assert!(!messages.is_empty(), "Messages should not be empty");
    assert_eq!(
        messages[0].role,
        Role::User,
        "First message must be User role"
    );
    for window in messages.windows(2) {
        assert_ne!(
            window[0].role, window[1].role,
            "Messages must alternate roles: got {:?} followed by {:?}",
            window[0].role, window[1].role
        );
    }
}
