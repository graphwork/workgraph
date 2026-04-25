//! Integration tests for tiered context pressure management.
//!
//! Tests the ContextBudget struct and its threshold-based actions:
//! - 80% capacity → Warning injection
//! - 90% capacity → Emergency compaction
//! - 95% capacity → Clean exit signal
//!
//! Run with: cargo test --test integration_context_pressure

use workgraph::executor::native::client::{ContentBlock, Message, Role};
use workgraph::executor::native::resume::{ContextBudget, ContextPressureAction};

/// Helper: create a message with text of a given byte length.
fn make_text_message(role: Role, size: usize) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: "x".repeat(size),
        }],
    }
}

/// Helper: create a message with a tool result of a given byte length.
fn make_tool_result_message(size: usize) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tu-1".to_string(),
            content: "r".repeat(size),
            is_error: false,
        }],
    }
}

// ---------------------------------------------------------------------------
// Test: Warning injection at 80% threshold
// ---------------------------------------------------------------------------

/// When estimated tokens are between 80% and 90% of the context window,
/// ContextBudget::check_pressure returns Warning.
#[test]
fn test_context_pressure_warning() {
    // Window: 1000 tokens, chars_per_token=4 → 4000 chars = 1000 tokens.
    // 80% threshold = 800 tokens = 3200 chars.
    // 90% threshold = 900 tokens = 3600 chars.
    let budget = ContextBudget {
        window_size: 1000,
        chars_per_token: 4.0,
        model: None,
        warning_threshold: 0.80,
        compact_threshold: 0.90,
        hard_limit: 0.95,
        overhead_tokens: 0,
    };

    // Below warning threshold: 3000 chars = 750 tokens = 75%
    let msgs_ok = vec![make_text_message(Role::User, 3000)];
    assert_eq!(budget.check_pressure(&msgs_ok), ContextPressureAction::Ok);

    // At warning threshold: 3200 chars = 800 tokens = 80%
    let msgs_warn = vec![make_text_message(Role::User, 3200)];
    assert_eq!(
        budget.check_pressure(&msgs_warn),
        ContextPressureAction::Warning
    );

    // Between warning and compact: 3400 chars = 850 tokens = 85%
    let msgs_mid = vec![make_text_message(Role::User, 3400)];
    assert_eq!(
        budget.check_pressure(&msgs_mid),
        ContextPressureAction::Warning
    );

    // Verify the warning message contains useful information
    let warning = budget.warning_message(&msgs_warn);
    assert!(
        warning.contains("80%") || warning.contains("CONTEXT PRESSURE"),
        "Warning should mention threshold or context pressure: {}",
        warning
    );
    assert!(
        warning.contains("800"),
        "Warning should mention estimated token count: {}",
        warning
    );
}

// ---------------------------------------------------------------------------
// Test: Emergency compaction at 90% threshold
// ---------------------------------------------------------------------------

/// When estimated tokens are between 90% and 95% of the context window,
/// ContextBudget::check_pressure returns EmergencyCompaction, and
/// emergency_compact strips large tool results from older messages.
#[test]
fn test_context_pressure_compaction() {
    let budget = ContextBudget {
        window_size: 1000,
        chars_per_token: 4.0,
        model: None,
        warning_threshold: 0.80,
        compact_threshold: 0.90,
        hard_limit: 0.95,
        overhead_tokens: 0,
    };

    // 3600 chars = 900 tokens = 90% → EmergencyCompaction
    let msgs_compact = vec![make_text_message(Role::User, 3600)];
    assert_eq!(
        budget.check_pressure(&msgs_compact),
        ContextPressureAction::EmergencyCompaction
    );

    // 3700 chars = 925 tokens = 92.5% → still EmergencyCompaction
    let msgs_mid = vec![make_text_message(Role::User, 3700)];
    assert_eq!(
        budget.check_pressure(&msgs_mid),
        ContextPressureAction::EmergencyCompaction
    );

    // Verify emergency_compact actually strips large tool results from old messages
    let messages = vec![
        // Old messages (will be compacted)
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "src/main.rs"}),
            }],
        },
        make_tool_result_message(5000), // Large tool result — should be stripped
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "I found the issue.".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Great, fix it.".to_string(),
            }],
        },
        // Recent messages (keep_recent=2 → these are kept verbatim)
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Fixed it.".to_string(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Thanks!".to_string(),
            }],
        },
    ];

    let compacted = ContextBudget::emergency_compact(messages.clone(), 0);

    // Recent messages preserved
    assert_eq!(compacted.len(), messages.len());
    let last = &compacted[compacted.len() - 1];
    match &last.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Thanks!"),
        _ => panic!("Expected text in last message"),
    }

    // Old large tool result should be replaced with a summary
    let old_tool_result = &compacted[1];
    match &old_tool_result.content[0] {
        ContentBlock::ToolResult { content, .. } => {
            assert!(
                content.len() < 5000,
                "Large tool result should have been compacted, but is {} bytes",
                content.len()
            );
            assert!(
                content.contains("Tool result removed") || content.contains("bytes"),
                "Compacted tool result should mention removal: {}",
                content
            );
        }
        _ => panic!("Expected tool result in compacted message"),
    }
}

// ---------------------------------------------------------------------------
// Test: Clean exit at 95% threshold
// ---------------------------------------------------------------------------

/// When estimated tokens reach 95%+ of the context window,
/// ContextBudget::check_pressure returns CleanExit.
#[test]
fn test_context_pressure_clean_exit() {
    let budget = ContextBudget {
        window_size: 1000,
        chars_per_token: 4.0,
        model: None,
        warning_threshold: 0.80,
        compact_threshold: 0.90,
        hard_limit: 0.95,
        overhead_tokens: 0,
    };

    // 3800 chars = 950 tokens = 95% → CleanExit
    let msgs_exit = vec![make_text_message(Role::User, 3800)];
    assert_eq!(
        budget.check_pressure(&msgs_exit),
        ContextPressureAction::CleanExit
    );

    // 4000 chars = 1000 tokens = 100% → still CleanExit
    let msgs_full = vec![make_text_message(Role::User, 4000)];
    assert_eq!(
        budget.check_pressure(&msgs_full),
        ContextPressureAction::CleanExit
    );

    // 5000 chars = 1250 tokens = 125% (over limit) → CleanExit
    let msgs_over = vec![make_text_message(Role::User, 5000)];
    assert_eq!(
        budget.check_pressure(&msgs_over),
        ContextPressureAction::CleanExit
    );

    // Verify thresholds are correctly ordered with multiple messages
    let budget_large = ContextBudget::with_window_size(10_000);
    // 10_000 tokens * 4 chars/token = 40_000 chars total capacity
    // 95% = 9500 tokens = 38_000 chars

    // Build up messages to approach 95%
    let msgs: Vec<Message> = (0..19)
        .map(|i| {
            make_text_message(
                if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                2000, // 19 * 2000 = 38000 chars = 9500 tokens = 95%
            )
        })
        .collect();

    assert_eq!(
        budget_large.check_pressure(&msgs),
        ContextPressureAction::CleanExit
    );

    // Default ContextBudget should have correct thresholds
    let default_budget = ContextBudget::default();
    assert_eq!(default_budget.window_size, 200_000);
    assert!((default_budget.warning_threshold - 0.70).abs() < f64::EPSILON);
    assert!((default_budget.compact_threshold - 0.75).abs() < f64::EPSILON);
    assert!((default_budget.hard_limit - 0.95).abs() < f64::EPSILON);
}
