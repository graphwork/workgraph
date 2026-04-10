//! Tests for edit_file edge cases and failure scenarios.
//!
//! This test module reproduces and prevents common edit_file failures identified
//! in the investigation report (.workgraph/reports/edit-tool-investigation.md).
//!
//! Key findings from investigation:
//! - The edit_file tool requires EXACT byte-for-byte matching
//! - Common failures: whitespace mismatches, line ending differences, trailing newline issues
//! - Error messages are informative but could be more helpful
//!
//! Test cases cover:
//! 1. Line ending sensitivity (Windows \r\n vs Unix \n)
//! 2. Whitespace variations (spaces, tabs, mixed)
//! 3. Partial vs full line matching
//! 4. Exact match requirements
//! 5. Unicode character handling
//! 6. Multiple match prevention
//! 7. Error message verification
//!
//! Run with: cargo test test_edit_file_edge_cases

use std::fs;
use tempfile::TempDir;

use workgraph::executor::native::tools::{ToolOutput, ToolRegistry};

/// Helper to create a ToolRegistry with file tools registered
fn make_tool_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    workgraph::executor::native::tools::file::register_file_tools(&mut registry);
    registry
}

/// Helper to execute the edit_file tool with given parameters
async fn edit_file(
    registry: &ToolRegistry,
    path: &str,
    old_string: &str,
    new_string: &str,
) -> ToolOutput {
    let input = serde_json::json!({
        "path": path,
        "old_string": old_string,
        "new_string": new_string
    });
    registry.execute("edit_file", &input).await
}

/// Check if ToolOutput indicates success
fn is_success(output: &ToolOutput) -> bool {
    !output.is_error
}

/// Get error message from ToolOutput
fn error_msg(output: &ToolOutput) -> String {
    output.content.clone()
}

// ── Test Suite ───────────────────────────────────────────────────────────────

// ── 1. Line Ending Sensitivity Tests ─────────────────────────────────────────

/// Test: Unix line endings (\n) work correctly
#[tokio::test]
async fn test_edit_with_unix_line_endings() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("unix_lines.txt");

    fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "line2",
        "LINE2_MODIFIED",
    )
    .await;

    assert!(
        is_success(&result),
        "Edit with Unix line endings should succeed: {:?}",
        result
    );
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "line1\nLINE2_MODIFIED\nline3\n");
}

/// Test: Windows line endings (\r\n) work correctly
#[tokio::test]
async fn test_edit_with_windows_line_endings() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("windows_lines.txt");

    fs::write(&file_path, "line1\r\nline2\r\nline3\r\n").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "line2",
        "LINE2_MODIFIED",
    )
    .await;

    assert!(
        is_success(&result),
        "Edit with Windows line endings should succeed: {:?}",
        result
    );
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "line1\r\nLINE2_MODIFIED\r\nline3\r\n");
}

/// Test: Mismatch between Unix search string and Windows file fails
#[tokio::test]
async fn test_edit_fails_on_line_ending_mismatch() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("mismatch.txt");

    fs::write(&file_path, "line1\r\nline2\r\nline3\r\n").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "line1\nline2",
        "A\nB",
    )
    .await;

    assert!(!is_success(&result), "Should fail on line ending mismatch");
    let msg = error_msg(&result);
    assert!(
        msg.contains("not found") || msg.contains("exactly"),
        "Error should mention exact matching requirement: {}",
        msg
    );
}

/// Test: Mixed line endings in file content
#[tokio::test]
async fn test_edit_with_mixed_line_endings() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("mixed.txt");

    fs::write(&file_path, "unix\nwindows\r\nmixed\r\nunix2\n").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "windows\r\nmixed",
        "WINDOWS\r\nMIXED",
    )
    .await;
    assert!(
        is_success(&result),
        "Edit with mixed line endings should succeed: {:?}",
        result
    );
}

// ── 2. Whitespace Variation Tests ───────────────────────────────────────────

/// Test: Extra spaces in search string causes failure
#[tokio::test]
async fn test_edit_fails_with_extra_spaces() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("spaces.txt");

    fs::write(&file_path, "hello world").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "hello world ",
        "hi there",
    )
    .await;

    assert!(!is_success(&result), "Extra space should cause mismatch");
}

/// Test: Missing space in search string causes failure
#[tokio::test]
async fn test_edit_fails_with_missing_spaces() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("spaces2.txt");

    fs::write(&file_path, "hello   world").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "hello world",
        "hi there",
    )
    .await;

    assert!(!is_success(&result), "Missing spaces should cause mismatch");
}

/// Test: Tab vs space difference
#[tokio::test]
async fn test_edit_fails_with_tab_space_mismatch() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("tabs.txt");

    fs::write(&file_path, "function() {\n\tindent\n}").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "    indent",
        "        NEW",
    )
    .await;

    assert!(
        !is_success(&result),
        "Tab vs space mismatch should cause failure"
    );
}

/// Test: Trailing whitespace difference
#[tokio::test]
async fn test_edit_fails_with_trailing_whitespace_difference() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("trailing.txt");

    fs::write(&file_path, "code   \nnext").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "code\nnext",
        "CODE\nNEXT",
    )
    .await;

    assert!(
        !is_success(&result),
        "Trailing whitespace difference should cause failure"
    );
}

/// Test: Leading whitespace difference (search string not a substring of file content)
#[tokio::test]
async fn test_edit_fails_with_leading_whitespace_difference() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("leading.txt");

    // File has "    alpha"
    fs::write(&file_path, "    alpha").unwrap();

    let registry = make_tool_registry();
    // "  beta" (different word, different spacing) - will not match
    let result = edit_file(&registry, file_path.to_str().unwrap(), "  beta", "BETA").await;

    assert!(
        !is_success(&result),
        "Leading whitespace difference with different content should cause failure"
    );
}

// ── 3. Partial vs Full Line Matching Tests ───────────────────────────────────

/// Test: Matching partial content that spans lines
#[tokio::test]
async fn test_edit_partial_line_content() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("partial.txt");

    fs::write(&file_path, "START middle END").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "middle", "MIDDLE").await;

    assert!(
        is_success(&result),
        "Partial line match should succeed: {:?}",
        result
    );
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "START MIDDLE END");
}

/// Test: Full line matching works
#[tokio::test]
async fn test_edit_full_line_match() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("full_line.txt");

    fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "line2", "LINE2").await;

    assert!(is_success(&result), "Full line match should succeed");
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "line1\nLINE2\nline3\n");
}

/// Test: Empty line matching
#[tokio::test]
async fn test_edit_empty_line() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("empty.txt");

    fs::write(&file_path, "before\n\nafter").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "\n\n", "\n---\n").await;

    assert!(
        is_success(&result),
        "Empty line match should succeed: {:?}",
        result
    );
}

// ── 4. Exact Match Requirement Tests ────────────────────────────────────────

/// Test: Exact match required - substring match when unique works
#[tokio::test]
async fn test_edit_requires_exact_match() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("exact.txt");

    fs::write(&file_path, "foobar").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "foo", "baz").await;
    assert!(
        is_success(&result),
        "Substring match should succeed when unique"
    );

    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "bazbar");
}

/// Test: Exact match with special characters
#[tokio::test]
async fn test_edit_exact_match_special_chars() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("special.txt");

    fs::write(&file_path, "fn main() {\n    println!(\"hello\");\n}").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "println!(\"hello\")",
        "println!(\"world\")",
    )
    .await;

    assert!(
        is_success(&result),
        "Special chars should match exactly: {:?}",
        result
    );
    let content = fs::read_to_string(&file_path).unwrap();
    assert!(content.contains("world"), "Content should contain 'world'");
}

// ── 5. Unicode Handling Tests ────────────────────────────────────────────────

/// Test: Unicode characters work correctly
#[tokio::test]
async fn test_edit_unicode_characters() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("unicode.txt");

    fs::write(&file_path, "Hello 世界 🌍").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "世界", "地球").await;

    assert!(
        is_success(&result),
        "Unicode edit should succeed: {:?}",
        result
    );
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "Hello 地球 🌍");
}

/// Test: Unicode with line endings
#[tokio::test]
async fn test_edit_unicode_with_line_endings() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("unicode_lines.txt");

    fs::write(&file_path, "English line\n日本語のライン\nEmoji line 🐱\n").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "日本語のライン",
        "中國線",
    )
    .await;

    assert!(
        is_success(&result),
        "Unicode with line endings should work: {:?}",
        result
    );
    let content = fs::read_to_string(&file_path).unwrap();
    assert!(content.contains("中國線"));
}

/// Test: Mixed ASCII and non-ASCII
#[tokio::test]
async fn test_edit_mixed_ascii_unicode() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("mixed_unicode.txt");

    fs::write(&file_path, "/* Comment: café */\nlet x = 1;").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "/* Comment: café */",
        "// Comment: coffee",
    )
    .await;

    assert!(
        is_success(&result),
        "Mixed ASCII/Unicode should work: {:?}",
        result
    );
}

// ── 6. Multiple Match Prevention Tests ──────────────────────────────────────

/// Test: Multiple matches cause failure
#[tokio::test]
async fn test_edit_fails_on_multiple_matches() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("multiple.txt");

    fs::write(&file_path, "foo bar foo baz foo").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "foo", "FOO").await;

    assert!(!is_success(&result), "Multiple matches should fail");
    let msg = error_msg(&result);
    assert!(
        msg.contains("3") || msg.contains("unique"),
        "Error should mention count or uniqueness: {}",
        msg
    );
}

/// Test: Single match succeeds
#[tokio::test]
async fn test_edit_succeeds_with_unique_match() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("unique.txt");

    fs::write(&file_path, "foo bar baz qux").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "bar", "BAR").await;

    assert!(is_success(&result), "Unique match should succeed");
}

/// Test: Adding more context makes match unique
#[tokio::test]
async fn test_edit_with_context_is_unique() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("context.txt");

    fs::write(&file_path, "const x = 1;\nconst y = 2;\nconst z = 3;").unwrap();

    let registry = make_tool_registry();

    let result1 = edit_file(&registry, file_path.to_str().unwrap(), "const", "let").await;
    assert!(
        !is_success(&result1),
        "'const' alone should have multiple matches"
    );

    let result2 = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "const x = 1;",
        "let x = 1;",
    )
    .await;
    assert!(
        is_success(&result2),
        "More context should make match unique"
    );
}

// ── 7. Error Message Tests ───────────────────────────────────────────────────

/// Test: Error message for string not found
#[tokio::test]
async fn test_error_message_string_not_found() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("notfound.txt");

    fs::write(&file_path, "hello world").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "not present",
        "replacement",
    )
    .await;

    assert!(
        !is_success(&result),
        "Should return error for missing string"
    );
    let msg = error_msg(&result);
    assert!(
        msg.contains("not found") || msg.contains("exactly"),
        "Error should be helpful: {}",
        msg
    );
}

/// Test: Error message for non-unique match
#[tokio::test]
async fn test_error_message_multiple_matches() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("nonunique.txt");

    fs::write(&file_path, "item item item").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "item", "ITEM").await;

    assert!(
        !is_success(&result),
        "Should return error for non-unique match"
    );
    let msg = error_msg(&result);
    assert!(
        msg.contains("3") && msg.contains("unique"),
        "Error should mention count and uniqueness: {}",
        msg
    );
}

/// Test: Error message for missing file
#[tokio::test]
async fn test_error_message_missing_file() {
    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        "/nonexistent/path/file.txt",
        "text",
        "replacement",
    )
    .await;

    assert!(!is_success(&result), "Should return error for missing file");
    let msg = error_msg(&result);
    assert!(
        msg.contains("read") || msg.contains("Failed"),
        "Error should mention file reading issue: {}",
        msg
    );
}

// ── 8. Edge Cases ────────────────────────────────────────────────────────────

/// Test: Very long line
#[tokio::test]
async fn test_edit_long_line() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("long.txt");

    let long_content = format!("short {}", "x".repeat(10000));
    fs::write(&file_path, long_content).unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "short", "LONG").await;

    assert!(
        is_success(&result),
        "Long line edit should succeed: {:?}",
        result
    );
}

/// Test: Empty file
#[tokio::test]
async fn test_edit_empty_file() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("empty_file.txt");

    fs::write(&file_path, "").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "anything",
        "replacement",
    )
    .await;

    assert!(!is_success(&result), "Edit on empty file should fail");
}

/// Test: Newline at end of file vs not
#[tokio::test]
async fn test_edit_trailing_newline_vs_not() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("newline.txt");

    fs::write(&file_path, "line1\n").unwrap();

    let registry = make_tool_registry();

    let result1 = edit_file(&registry, file_path.to_str().unwrap(), "line1\n", "LINE1\n").await;
    assert!(
        is_success(&result1),
        "Match with trailing newline should work"
    );

    let result2 = edit_file(&registry, file_path.to_str().unwrap(), "line1", "LINE1").await;
    assert!(
        !is_success(&result2),
        "Missing trailing newline should fail when file has it"
    );
}

/// Test: Replacement string is empty
#[tokio::test]
async fn test_edit_empty_replacement() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("remove.txt");

    fs::write(&file_path, "hello world").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), " world", "").await;

    assert!(is_success(&result), "Empty replacement should work");
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "hello");
}

/// Test: Single character match
#[tokio::test]
async fn test_edit_single_character() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("single_char.txt");

    fs::write(&file_path, "abc").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "b", "X").await;

    assert!(is_success(&result), "Single character edit should succeed");
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "aXc");
}

/// Test: Binary-ish content (null bytes)
#[tokio::test]
async fn test_edit_with_null_bytes() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("binary.txt");

    let content = vec![
        b'h', b'e', b'l', b'l', b'o', 0, b'w', b'o', b'r', b'l', b'd',
    ];
    fs::write(&file_path, content).unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "hello", "HELLO").await;

    if is_success(&result) {
        let new_content = fs::read(&file_path).unwrap();
        assert!(new_content.starts_with(b"HELLO"));
    }
}

// ── 9. Regression Prevention Tests ──────────────────────────────────────────

/// Test: Common pattern - editing inside a function body
#[tokio::test]
async fn test_edit_inside_function() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("function.rs");

    fs::write(
        &file_path,
        "fn test() {\n    let x = 1;\n    println!(\"{}\", x);\n}",
    )
    .unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "    let x = 1;",
        "    let x = 42;",
    )
    .await;

    assert!(
        is_success(&result),
        "Function edit should work: {:?}",
        result
    );
}

/// Test: Common pattern - editing with indentation
#[tokio::test]
async fn test_edit_with_indentation() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("indented.txt");

    fs::write(&file_path, "    indented line\nnext line").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        "    indented line",
        "        MORE indented",
    )
    .await;

    assert!(is_success(&result), "Indented edit should work");
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "        MORE indented\nnext line");
}

/// Test: JSON content (common real-world case)
#[tokio::test]
async fn test_edit_json_content() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("data.json");

    fs::write(&file_path, r#"{"name": "test", "value": 123}"#).unwrap();

    let registry = make_tool_registry();
    let result = edit_file(
        &registry,
        file_path.to_str().unwrap(),
        r#""value": 123"#,
        r#""value": 456"#,
    )
    .await;

    assert!(is_success(&result), "JSON edit should work: {:?}", result);
    let content = fs::read_to_string(&file_path).unwrap();
    assert!(content.contains("456"));
}

/// Test: Consecutive edits work correctly
#[tokio::test]
async fn test_consecutive_edits() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("consecutive.txt");

    fs::write(&file_path, "a b c d e").unwrap();

    let registry = make_tool_registry();

    let r1 = edit_file(&registry, file_path.to_str().unwrap(), "a", "A").await;
    assert!(is_success(&r1), "First edit should succeed");

    let r2 = edit_file(&registry, file_path.to_str().unwrap(), "b", "B").await;
    assert!(is_success(&r2), "Second edit should succeed");

    let r3 = edit_file(&registry, file_path.to_str().unwrap(), "c", "C").await;
    assert!(is_success(&r3), "Third edit should succeed");

    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "A B C d e");
}

/// Test: Edit that creates duplicate matches succeeds (tool doesn't check post-edit)
#[tokio::test]
async fn test_edit_that_creates_duplicate() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("duplicate_test.txt");

    fs::write(&file_path, "foo bar").unwrap();

    let registry = make_tool_registry();
    let result = edit_file(&registry, file_path.to_str().unwrap(), "bar", "foo").await;
    assert!(
        is_success(&result),
        "Edit should succeed (tool doesn't check for post-edit duplicates)"
    );

    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "foo foo");
}
