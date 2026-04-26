//! Tests for removal of `--validation` CLI flag.
//!
//! Per the `remove-validation-cli` task: the `--validation` flag is
//! removed from `wg add` / `wg edit` (kept as a hidden, no-op deprecation
//! for one release). Quickstart text and agent prompts must no longer
//! advertise the flag — validation criteria belong in the `## Validation`
//! section of task descriptions and are scored by the agency evaluator.

use std::process::Command;

fn wg_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // /deps
    if p.ends_with("deps") {
        p.pop();
    }
    p.push("wg");
    p
}

/// `wg add --help` must not advertise `--validation` (or `--validator-*`).
#[test]
fn test_cli_add_no_validation_flag() {
    let bin = wg_bin();
    if !bin.exists() {
        eprintln!("wg binary not built; skipping test");
        return;
    }
    let out = Command::new(&bin)
        .args(["add", "--help"])
        .output()
        .expect("run wg add --help");
    let help = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);

    assert!(
        !help.contains("--validation"),
        "wg add --help must not mention --validation flag, got:\n{}",
        help
    );
    assert!(
        !help.contains("--validator-agent"),
        "wg add --help must not mention --validator-agent flag"
    );
    assert!(
        !help.contains("--validator-model"),
        "wg add --help must not mention --validator-model flag"
    );
}

/// `wg edit --help` must not advertise `--validation`.
#[test]
fn test_cli_edit_no_validation_flag() {
    let bin = wg_bin();
    if !bin.exists() {
        eprintln!("wg binary not built; skipping test");
        return;
    }
    let out = Command::new(&bin)
        .args(["edit", "--help"])
        .output()
        .expect("run wg edit --help");
    let help = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);

    assert!(
        !help.contains("--validation"),
        "wg edit --help must not mention --validation flag"
    );
}

/// Quickstart output must mention the `## Validation` section convention but
/// must NOT advertise the `--validation` CLI flag.
#[test]
fn test_quickstart_no_validation_flag() {
    let bin = wg_bin();
    if !bin.exists() {
        eprintln!("wg binary not built; skipping test");
        return;
    }
    let out = Command::new(&bin)
        .arg("quickstart")
        .output()
        .expect("run wg quickstart");
    let text = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);

    assert!(
        !text.contains("--validation"),
        "wg quickstart must not advertise --validation flag, got:\n{}",
        text
    );
    // Still mentions the markdown section by name
    assert!(
        text.contains("## Validation") || text.contains("Validation section"),
        "wg quickstart must still mention the ## Validation section convention; got:\n{}",
        text
    );
}

/// Prompts assembled for spawned agents must not contain the `--validation`
/// flag string. Validation criteria flow through the `## Validation` section
/// of task descriptions, read by the agency evaluator.
#[test]
fn test_executor_prompt_no_validation_flag() {
    let guide = workgraph::service::executor::DEFAULT_WG_GUIDE;
    assert!(
        !guide.contains("--validation"),
        "DEFAULT_WG_GUIDE must not contain --validation flag, got:\n{}",
        guide
    );

    let guidance =
        workgraph::service::executor::build_decomposition_guidance("multi-step task", "task-1", 10, 8);
    assert!(
        !guidance.contains("--validation"),
        "build_decomposition_guidance output must not contain --validation flag, got:\n{}",
        guidance
    );
}

/// `wg add 'test' --validation=llm` either errors with unknown-flag OR is
/// accepted as a no-op with a deprecation warning. Either is acceptable
/// per the task spec.
#[test]
fn test_cli_add_validation_flag_is_noop_or_unknown() {
    let bin = wg_bin();
    if !bin.exists() {
        eprintln!("wg binary not built; skipping test");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    // Initialize workgraph so flag-acceptance path can succeed without
    // hitting the "Workgraph not initialized" gate.
    let init = Command::new(&bin)
        .current_dir(tmp.path())
        .args(["init", "--executor", "shell"])
        .output()
        .expect("wg init");
    assert!(
        init.status.success(),
        "wg init failed: stderr={}",
        String::from_utf8_lossy(&init.stderr)
    );

    let out = Command::new(&bin)
        .current_dir(tmp.path())
        .args(["add", "smoke-test", "--validation=llm"])
        .output()
        .expect("run wg add --validation=llm");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{}{}", stdout, stderr);

    let unknown_flag =
        combined.contains("unexpected argument") || combined.contains("unrecognized");
    let deprecation_warning =
        combined.to_lowercase().contains("deprecated") || combined.contains("ignored");

    assert!(
        unknown_flag || deprecation_warning,
        "expected either unknown-flag error or deprecation warning, got:\nstdout={}\nstderr={}",
        stdout,
        stderr
    );
}
