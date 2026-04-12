//! Test for verify command auto-correction functionality.

/// Test that malformed verify commands are auto-corrected
#[test]
fn test_verify() {
    // Test auto-correction functionality
    let corrected = workgraph::verify_lint::auto_correct_verify_command("cargo test passes");
    assert_eq!(corrected, Some("cargo test".to_string()));

    // Test other malformed commands
    let corrected2 =
        workgraph::verify_lint::auto_correct_verify_command("cargo build succeeds without errors");
    assert_eq!(corrected2, Some("cargo build".to_string()));

    // Test valid commands are left unchanged
    let unchanged = workgraph::verify_lint::auto_correct_verify_command("cargo test");
    assert_eq!(unchanged, None);

    let unchanged2 =
        workgraph::verify_lint::auto_correct_verify_command("cargo test specific_test");
    assert_eq!(unchanged2, None);
}

/// Test stripping various descriptive patterns
#[test]
fn test_verify_pattern_stripping() {
    let test_cases = vec![
        ("cargo test passes", Some("cargo test")),
        ("npm test succeeds", Some("npm test")),
        ("cargo build succeeds without errors", Some("cargo build")),
        ("cargo test with no warnings", Some("cargo test")),
        ("cargo test with no regressions", Some("cargo test")),
        ("cargo test passes without errors", Some("cargo test")),
        ("make test runs successfully", Some("make test")),
        ("python -m pytest works correctly", Some("python -m pytest")),
        // Valid commands should return None (no correction needed)
        ("cargo test", None),
        ("npm test", None),
        ("true", None),
        ("test -f file.txt", None),
    ];

    for (input, expected) in test_cases {
        let result = workgraph::verify_lint::auto_correct_verify_command(input);
        assert_eq!(
            result,
            expected.map(String::from),
            "Failed for input: {}",
            input
        );
    }
}

/// Test bash syntax validation
#[test]
fn test_verify_bash_syntax_validation() {
    use workgraph::verify_lint::auto_correct_verify_command;

    // These should be detected as syntax errors and potentially corrected
    let syntax_error_cases = vec![
        "cargo test passes for all modules",
        "build succeeds with no errors",
        "tests should pass",
    ];

    for case in syntax_error_cases {
        let result = auto_correct_verify_command(case);
        // Should either return a correction or None, but shouldn't panic
        println!("Case '{}' -> {:?}", case, result);
    }

    // Valid bash commands should return None (no correction needed)
    let valid_cases = vec![
        "cargo test",
        "true",
        "test -f file.txt",
        "cargo build && cargo test",
    ];

    for case in valid_cases {
        let result = auto_correct_verify_command(case);
        assert_eq!(
            result, None,
            "Valid command '{}' should not be corrected",
            case
        );
    }
}
