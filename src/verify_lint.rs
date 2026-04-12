//! Lint verify commands to detect descriptive text that isn't executable.
//!
//! The `--verify` field is meant for shell commands that validate task completion.
//! Agents and users frequently put descriptive prose like "tests pass for all modules"
//! which causes spawn-die loops when the system tries to execute it.

use std::process::Command;

/// Result of linting a verify command.
#[derive(Debug, Clone)]
pub struct LintResult {
    pub warnings: Vec<LintWarning>,
}

#[derive(Debug, Clone)]
pub struct LintWarning {
    pub kind: LintKind,
    pub message: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintKind {
    DescriptiveText,
    BashSyntaxError,
    UnknownCommand,
}

impl LintResult {
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

/// Descriptive phrases that indicate prose, not a command.
const DESCRIPTIVE_PATTERNS: &[&str] = &[
    "passes for",
    "should be",
    "all files",
    "succeeds without",
    "no errors",
    "no warnings",
    "is correct",
    "are correct",
    "works correctly",
    "runs successfully",
    "completes successfully",
    "build succeeds",
    "tests pass",
    "should pass",
    "should succeed",
    "should work",
    "should compile",
    "should run",
    "must pass",
    "must succeed",
    "without errors",
    "without warnings",
    "without failures",
    "without regressions",
    "no regressions",
    "all modules",
    "all tests",
    "is valid",
    "are valid",
    "has been",
    "have been",
];

/// Commands/tokens commonly seen in valid verify commands.
const KNOWN_VALID_FIRST_TOKENS: &[&str] = &[
    "cargo", "npm", "npx", "yarn", "pnpm", "make", "cmake", "go", "python", "python3", "pytest",
    "ruby", "rake", "bundle", "mvn", "gradle", "ant", "dotnet", "zig", "rustc", "gcc", "g++",
    "clang", "clang++", "javac", "java", "test", "[", "true", "false", "exit", "echo", "printf",
    "cat", "grep", "find", "ls", "diff", "cmp", "wc", "head", "tail", "sort", "uniq", "cut", "tr",
    "sed", "awk", "jq", "yq", "curl", "wget", "ssh", "rsync", "docker", "podman", "kubectl",
    "helm", "git", "gh", "wg", "sh", "bash", "zsh", "typst", "pandoc", "latexmk", "pdflatex",
    "xelatex", "node", "deno", "bun", "tsc", "env", "timeout", "nice", "sudo",
];

/// Shell builtins that are valid as first tokens.
const SHELL_BUILTINS: &[&str] = &[
    "test", "[", "true", "false", "echo", "printf", "exit", "return", "cd", "pwd", "export",
    "unset", "set", "source", ".", "eval", "exec", "command", "builtin", "type", "hash", "which",
    "if", "then", "else", "fi", "for", "do", "done", "while", "until", "case", "esac", "select",
    "function", "time", "coproc", "read", "wait", "kill", "trap", "local", "declare", "typeset",
    "readonly", "let", "shift", "getopts", "break", "continue",
];

/// Lint a verify command string and return warnings.
pub fn lint_verify(cmd: &str) -> LintResult {
    let cmd = cmd.trim();
    let mut warnings = Vec::new();

    if cmd.is_empty() {
        return LintResult { warnings };
    }

    // Check for descriptive text patterns
    check_descriptive_patterns(cmd, &mut warnings);

    // Check bash syntax (only if no descriptive patterns found — prose always fails syntax)
    if warnings.is_empty() {
        check_bash_syntax(cmd, &mut warnings);
    }

    // Check if first token is an executable (only if no other warnings)
    if warnings.is_empty() {
        check_first_token(cmd, &mut warnings);
    }

    LintResult { warnings }
}

/// Check if the command matches known descriptive text patterns.
fn check_descriptive_patterns(cmd: &str, warnings: &mut Vec<LintWarning>) {
    let lower = cmd.to_lowercase();

    for pattern in DESCRIPTIVE_PATTERNS {
        if lower.contains(pattern) {
            let suggestion = suggest_replacement(&lower);
            warnings.push(LintWarning {
                kind: LintKind::DescriptiveText,
                message: format!(
                    "verify command looks like descriptive text (matched '{}'), not an executable command",
                    pattern
                ),
                suggestion,
            });
            return; // One descriptive pattern warning is enough
        }
    }
}

/// Check bash syntax using `bash -n`.
fn check_bash_syntax(cmd: &str, warnings: &mut Vec<LintWarning>) {
    match Command::new("bash").args(["-n", "-c", cmd]).output() {
        Ok(output) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warnings.push(LintWarning {
                    kind: LintKind::BashSyntaxError,
                    message: format!("verify command has bash syntax errors: {}", stderr.trim()),
                    suggestion: None,
                });
            }
        }
        Err(_) => {
            // Can't run bash — skip this check silently
        }
    }
}

/// Check if the first token of the command is an actual executable or builtin.
fn check_first_token(cmd: &str, warnings: &mut Vec<LintWarning>) {
    let first_token = extract_first_token(cmd);
    if first_token.is_empty() {
        return;
    }

    // Skip check for compound commands starting with shell keywords
    if SHELL_BUILTINS.contains(&first_token.as_str()) {
        return;
    }

    // Skip if it's a known valid command
    if KNOWN_VALID_FIRST_TOKENS.contains(&first_token.as_str()) {
        return;
    }

    // Skip if it looks like a path (contains /)
    if first_token.contains('/') {
        return;
    }

    // Check if the command exists in PATH using `command -v`
    let exists = Command::new("bash")
        .args(["-c", &format!("command -v {} >/dev/null 2>&1", first_token)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !exists {
        warnings.push(LintWarning {
            kind: LintKind::UnknownCommand,
            message: format!(
                "'{}' is not found in PATH and is not a shell builtin",
                first_token
            ),
            suggestion: Some(
                "verify commands must be executable. Common examples: 'cargo test', 'npm test', 'true'".to_string(),
            ),
        });
    }
}

/// Extract the first token from a command, handling leading env vars and operators.
fn extract_first_token(cmd: &str) -> String {
    let cmd = cmd.trim();

    // Skip leading environment variable assignments (KEY=val cmd ...)
    let mut rest = cmd;
    loop {
        // Match pattern: WORD=... followed by space
        if let Some(eq_pos) = rest.find('=') {
            let before_eq = &rest[..eq_pos];
            // Must be a valid env var name (alphanumeric + underscore, starts with letter/underscore)
            if !before_eq.is_empty()
                && before_eq
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && before_eq
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            {
                // Find end of value (could be quoted)
                let after_eq = &rest[eq_pos + 1..];
                if let Some(space_pos) = find_end_of_value(after_eq) {
                    rest = after_eq[space_pos..].trim_start();
                    continue;
                }
            }
        }
        break;
    }

    // Get first whitespace-delimited token
    rest.split_whitespace().next().unwrap_or("").to_string()
}

/// Find the end of an env var value, handling quotes.
fn find_end_of_value(s: &str) -> Option<usize> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }

    let bytes = s.as_bytes();
    if bytes[0] == b'"' {
        // Double-quoted: find closing quote
        for i in 1..bytes.len() {
            if bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
                return Some(s[..i + 1].len());
            }
        }
        None
    } else if bytes[0] == b'\'' {
        // Single-quoted: find closing quote
        for i in 1..bytes.len() {
            if bytes[i] == b'\'' {
                return Some(s[..i + 1].len());
            }
        }
        None
    } else {
        // Unquoted: up to next whitespace
        s.find(char::is_whitespace)
    }
}

/// Suggest a replacement command for common descriptive text patterns.
fn suggest_replacement(lower: &str) -> Option<String> {
    if lower.contains("test") || lower.contains("pass") {
        if lower.contains("cargo") || lower.contains("rust") {
            return Some("cargo test".to_string());
        }
        if lower.contains("npm") || lower.contains("node") || lower.contains("js") {
            return Some("npm test".to_string());
        }
        if lower.contains("python") || lower.contains("pytest") {
            return Some("pytest".to_string());
        }
        return Some("cargo test".to_string());
    }
    if lower.contains("build") || lower.contains("compile") {
        if lower.contains("cargo") || lower.contains("rust") {
            return Some("cargo build".to_string());
        }
        if lower.contains("npm") || lower.contains("node") {
            return Some("npm run build".to_string());
        }
        if lower.contains("typst") {
            return Some("typst compile <file>".to_string());
        }
        return Some("cargo build".to_string());
    }
    None
}

/// Print verify lint warnings to stderr. Returns true if warnings were printed.
pub fn print_warnings(cmd: &str) -> bool {
    let result = lint_verify(cmd);
    if !result.has_warnings() {
        return false;
    }

    eprintln!();
    eprintln!("WARNING: Verify command appears to be descriptive text, not an executable command");
    eprintln!("  Command: {}", cmd);
    for w in &result.warnings {
        eprintln!("  Reason:  {}", w.message);
        if let Some(ref suggestion) = w.suggestion {
            eprintln!("  Suggest: {}", suggestion);
        }
    }
    eprintln!();
    true
}

/// Auto-correct malformed verify commands by extracting the valid command prefix.
/// Returns Some(corrected_command) if the command was malformed and could be fixed,
/// or None if the command is already valid or cannot be auto-corrected.
pub fn auto_correct_verify_command(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }

    // First check for descriptive text patterns that indicate malformation
    let lint_result = lint_verify(cmd);
    for warning in &lint_result.warnings {
        if warning.kind == LintKind::DescriptiveText {
            // Found descriptive text, try to extract valid command prefix
            return extract_valid_command_prefix(cmd);
        }
    }

    // Check for specific trailing descriptive words that the existing patterns might miss
    if has_trailing_descriptive_words(cmd) {
        return extract_valid_command_prefix(cmd);
    }

    // Also check for bash syntax errors (though many "malformed" commands are syntactically valid)
    let has_bash_error = match Command::new("bash").args(["-n", "-c", cmd]).output() {
        Ok(output) => !output.status.success(),
        Err(_) => false, // Can't run bash, assume it's ok
    };

    if has_bash_error {
        // Try to extract valid command prefix by removing common descriptive patterns
        return extract_valid_command_prefix(cmd);
    }

    None // Command appears to be valid
}

/// Extract a valid command prefix from a malformed verify command by removing
/// common descriptive text patterns.
fn extract_valid_command_prefix(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();

    // Common patterns to strip from the end of commands
    let strip_patterns = &[
        "passes",
        "succeeds",
        "passes without errors",
        "succeeds without errors",
        "passes without warnings",
        "succeeds without warnings",
        "with no errors",
        "with no warnings",
        "with no regressions",
        "without errors",
        "without warnings",
        "without regressions",
        "compiles without warnings",
        "compiles without errors",
        "runs without errors",
        "works correctly",
        "runs successfully",
        "completes successfully",
    ];

    // Try to find and strip each pattern from the end
    for pattern in strip_patterns {
        if let Some(corrected) = strip_trailing_pattern(cmd, pattern) {
            // Check if the corrected command has valid bash syntax
            if is_valid_bash_syntax(&corrected) {
                return Some(corrected);
            }
        }
    }

    // Try a more aggressive approach: find the longest prefix that's valid bash
    find_longest_valid_prefix(cmd)
}

/// Strip a specific pattern from the end of a command string.
fn strip_trailing_pattern(cmd: &str, pattern: &str) -> Option<String> {
    let lower_cmd = cmd.to_lowercase();
    let lower_pattern = pattern.to_lowercase();

    if let Some(pos) = lower_cmd.rfind(&lower_pattern) {
        // Make sure this is at the end (allowing for whitespace)
        let after_pattern = &lower_cmd[pos + lower_pattern.len()..];
        if after_pattern.trim().is_empty() {
            let before_pattern = cmd[..pos].trim();
            if !before_pattern.is_empty() {
                return Some(before_pattern.to_string());
            }
        }
    }
    None
}

/// Check if a command has valid bash syntax.
fn is_valid_bash_syntax(cmd: &str) -> bool {
    match Command::new("bash").args(["-n", "-c", cmd]).output() {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Find the longest valid bash prefix of a command.
fn find_longest_valid_prefix(cmd: &str) -> Option<String> {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }

    // Try progressively shorter prefixes
    for i in (1..=words.len()).rev() {
        let prefix = words[..i].join(" ");
        if is_valid_bash_syntax(&prefix) && is_likely_executable_command(&prefix) {
            return Some(prefix);
        }
    }

    None
}

/// Check if a command has trailing descriptive words that suggest it's not a pure command.
fn has_trailing_descriptive_words(cmd: &str) -> bool {
    let cmd = cmd.trim().to_lowercase();

    // Common trailing words that indicate descriptive text
    let trailing_descriptive_words = &["passes", "succeeds", "works", "runs", "compiles"];

    for word in trailing_descriptive_words {
        if cmd.ends_with(word) {
            // Check that it's a separate word, not part of another word
            if let Some(pos) = cmd.rfind(word) {
                let before = &cmd[..pos];
                if before.ends_with(' ') || before.ends_with('\t') {
                    return true;
                }
            }
        }
    }

    false
}

/// Check if a command looks like an executable command (not just valid bash syntax).
fn is_likely_executable_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return false;
    }

    let first_token = extract_first_token(cmd);

    // Must start with a known command or executable
    KNOWN_VALID_FIRST_TOKENS.contains(&first_token.as_str())
        || SHELL_BUILTINS.contains(&first_token.as_str())
        || first_token.contains('/') // Path to executable
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Descriptive text should be flagged ---

    #[test]
    fn test_descriptive_tests_pass() {
        let r = lint_verify("tests pass for all modules");
        assert!(r.has_warnings());
        assert_eq!(r.warnings[0].kind, LintKind::DescriptiveText);
    }

    #[test]
    fn test_descriptive_build_succeeds() {
        let r = lint_verify("build succeeds without errors");
        assert!(r.has_warnings());
        assert_eq!(r.warnings[0].kind, LintKind::DescriptiveText);
    }

    #[test]
    fn test_descriptive_should_be() {
        let r = lint_verify("output should be valid JSON");
        assert!(r.has_warnings());
        assert_eq!(r.warnings[0].kind, LintKind::DescriptiveText);
    }

    #[test]
    fn test_descriptive_no_errors() {
        let r = lint_verify("cargo build produces no errors");
        assert!(r.has_warnings());
        assert_eq!(r.warnings[0].kind, LintKind::DescriptiveText);
    }

    #[test]
    fn test_descriptive_all_tests() {
        let r = lint_verify("all tests pass");
        assert!(r.has_warnings());
        assert_eq!(r.warnings[0].kind, LintKind::DescriptiveText);
    }

    #[test]
    fn test_descriptive_no_regressions() {
        let r = lint_verify("cargo test passes with no regressions");
        assert!(r.has_warnings());
        assert_eq!(r.warnings[0].kind, LintKind::DescriptiveText);
    }

    #[test]
    fn test_descriptive_is_correct() {
        let r = lint_verify("the output format is correct");
        assert!(r.has_warnings());
        assert_eq!(r.warnings[0].kind, LintKind::DescriptiveText);
    }

    // --- Valid commands should NOT be flagged ---

    #[test]
    fn test_valid_cargo_test() {
        let r = lint_verify("cargo test");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_cargo_build() {
        let r = lint_verify("cargo build");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_npm_test() {
        let r = lint_verify("npm test");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_true() {
        let r = lint_verify("true");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_test_f() {
        let r = lint_verify("test -f foo.txt");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_compound_and() {
        let r = lint_verify("test -f foo && cargo build");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_compound_semicolon() {
        let r = lint_verify("cargo build; cargo test");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_cargo_test_specific() {
        let r = lint_verify("cargo test test_feature_x");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_grep_pipeline() {
        let r = lint_verify("cargo test 2>&1 | grep -q 'ok'");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_valid_env_prefix() {
        let r = lint_verify("RUST_LOG=debug cargo test");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_empty_command() {
        let r = lint_verify("");
        assert!(!r.has_warnings());
    }

    #[test]
    fn test_whitespace_only() {
        let r = lint_verify("   ");
        assert!(!r.has_warnings());
    }

    // --- Suggestions ---

    #[test]
    fn test_suggests_cargo_test() {
        let r = lint_verify("tests pass for all modules");
        assert!(r.warnings[0].suggestion.is_some());
        assert!(
            r.warnings[0]
                .suggestion
                .as_ref()
                .unwrap()
                .contains("cargo test")
        );
    }

    #[test]
    fn test_suggests_npm_for_node() {
        let r = lint_verify("npm tests should pass");
        let suggestion = r.warnings[0].suggestion.as_ref().unwrap();
        assert!(suggestion.contains("npm test"));
    }

    // --- extract_first_token ---

    #[test]
    fn test_extract_simple() {
        assert_eq!(extract_first_token("cargo test"), "cargo");
    }

    #[test]
    fn test_extract_with_env() {
        assert_eq!(extract_first_token("RUST_LOG=debug cargo test"), "cargo");
    }

    #[test]
    fn test_extract_path() {
        assert_eq!(
            extract_first_token("/usr/bin/env cargo test"),
            "/usr/bin/env"
        );
    }

    // --- Auto-correction tests ---

    #[test]
    fn test_auto_correct_cargo_test_passes() {
        let corrected = auto_correct_verify_command("cargo test passes");
        assert_eq!(corrected, Some("cargo test".to_string()));
    }

    #[test]
    fn test_auto_correct_cargo_build_succeeds() {
        let corrected = auto_correct_verify_command("cargo build succeeds without errors");
        assert_eq!(corrected, Some("cargo build".to_string()));
    }

    #[test]
    fn test_auto_correct_npm_test_passes() {
        let corrected = auto_correct_verify_command("npm test passes without warnings");
        assert_eq!(corrected, Some("npm test".to_string()));
    }

    #[test]
    fn test_auto_correct_valid_command_unchanged() {
        let corrected = auto_correct_verify_command("cargo test");
        assert_eq!(corrected, None); // Already valid, no correction needed
    }

    #[test]
    fn test_auto_correct_complex_valid_command() {
        let corrected = auto_correct_verify_command("cargo test specific_test");
        assert_eq!(corrected, None); // Valid command, no correction needed
    }

    #[test]
    fn test_strip_trailing_pattern_basic() {
        let stripped = strip_trailing_pattern("cargo test passes", "passes");
        assert_eq!(stripped, Some("cargo test".to_string()));
    }

    #[test]
    fn test_strip_trailing_pattern_with_whitespace() {
        let stripped = strip_trailing_pattern("cargo build succeeds  ", "succeeds");
        assert_eq!(stripped, Some("cargo build".to_string()));
    }

    #[test]
    fn test_strip_trailing_pattern_not_at_end() {
        let stripped = strip_trailing_pattern("cargo passes test", "passes");
        assert_eq!(stripped, None); // Pattern not at end
    }

    #[test]
    fn test_auto_correct_no_regressions() {
        let corrected = auto_correct_verify_command("cargo test with no regressions");
        assert_eq!(corrected, Some("cargo test".to_string()));
    }

    #[test]
    fn test_has_trailing_descriptive_words() {
        assert!(has_trailing_descriptive_words("cargo test passes"));
        assert!(has_trailing_descriptive_words("npm test succeeds"));
        assert!(has_trailing_descriptive_words("make build works"));
        assert!(has_trailing_descriptive_words("python test.py runs"));
        assert!(has_trailing_descriptive_words("gcc main.c compiles"));

        // These should NOT be flagged
        assert!(!has_trailing_descriptive_words("cargo test"));
        assert!(!has_trailing_descriptive_words("true"));
        assert!(!has_trailing_descriptive_words("test -f passes.txt")); // "passes" as filename
    }
}
