//! Integration test for the handler-stdout-pristine contract.
//!
//! The chat protocol streams (stdin/stdout for handler binaries, and the
//! json-line outbox file populated transitively by config-loading code)
//! must never carry diagnostic text. Deprecation warnings, config-load
//! chatter, and any other diagnostic output MUST go to stderr (or the
//! daemon log) — never stdout.
//!
//! This test is the regression lock for the bug where `wg nex --chat`
//! crashed on the second message because a `Deprecated: [coordinator]
//! table is now [dispatcher]` warning landed on stdout and corrupted
//! the json-line stream.
//!
//! Strategy:
//! 1. Initialise a workgraph in a tempdir.
//! 2. Write a `config.toml` containing the deprecated `[coordinator]`
//!    section (canonical form is `[dispatcher]`). Loading this config
//!    must emit a deprecation warning.
//! 3. Run a `wg` subcommand that loads the config and prints data to
//!    stdout (e.g., `wg show <id>`).
//! 4. Capture stdout and stderr separately.
//! 5. Assert that stderr contains the warning text (proves the warning
//!    fired) and that stdout contains zero warning text — every line on
//!    stdout that came from the warning path must have been routed to
//!    stderr.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

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
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
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

fn init_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init", "--executor", "shell"]);
    wg_dir
}

/// Write a config.toml that contains the deprecated `[coordinator]`
/// table. Loading this config must emit:
///   `Deprecated: [coordinator] table is now [dispatcher]; please rename in <path>`
/// on stderr (per `src/config.rs::emit_legacy_warnings`).
fn write_deprecated_config(wg_dir: &Path) {
    let config_path = wg_dir.join("config.toml");
    let body = r#"
[coordinator]
poll_interval_ms = 500
max_agents = 2
"#;
    std::fs::write(&config_path, body).expect("write config.toml");
}

/// The deprecation warning landing on stdout corrupts the chat protocol's
/// json-line stream. This test asserts:
///   - stderr DOES contain the warning text (proves the deprecation path fired)
///   - stdout does NOT contain the warning text (proves the warning was
///     correctly routed off the protocol stream)
#[test]
fn test_handler_stdout_pristine_with_warning_config() {
    let tmp = TempDir::new().expect("tempdir");
    let wg_dir = init_workgraph(&tmp);
    write_deprecated_config(&wg_dir);

    // `wg status` loads merged config (so the legacy-section migration
    // path runs and the deprecation warning fires) and prints status
    // summary to stdout. This is the same code path that fires inside
    // any handler binary that calls Config::load_*.
    let output = wg_cmd(&wg_dir, &["status"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "wg list with deprecated config failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // The warning MUST appear somewhere — otherwise the test isn't
    // exercising the deprecation code path and gives a false pass.
    assert!(
        stderr.contains("Deprecated:") && stderr.contains("[coordinator]"),
        "expected deprecation warning on stderr; got:\nstderr: {}",
        stderr
    );

    // Stdout MUST NOT carry the warning text. Any of the following
    // substrings would mean the warning leaked into the protocol
    // stream — exactly the bug we're locking down.
    let stdout_pollutants = [
        "Deprecated:",
        "[coordinator] table is now [dispatcher]",
        "warning: Deprecated",
    ];
    for needle in stdout_pollutants {
        assert!(
            !stdout.contains(needle),
            "stdout must be pristine — found {:?} in stdout. \
             Diagnostic text on stdout corrupts handler chat protocol streams.\n\
             stdout: {}",
            needle,
            stdout
        );
    }
}

/// Even when no command-specific output is produced (e.g., listing an
/// empty graph), stdout must remain pristine. This guards the "silent
/// stdout" promise: if a handler is invoked and writes nothing to its
/// stdout protocol channel, no warning text from the config-load layer
/// can sneak in either.
#[test]
fn test_warning_routes_to_stderr_not_stdout() {
    let tmp = TempDir::new().expect("tempdir");
    let wg_dir = init_workgraph(&tmp);
    write_deprecated_config(&wg_dir);

    // `wg status` loads merged config (firing the deprecation warning)
    // and prints status to stdout.
    let output = wg_cmd(&wg_dir, &["status"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "wg ready with deprecated config failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    assert!(
        stderr.contains("Deprecated:"),
        "expected deprecation warning on stderr; got: {}",
        stderr
    );
    assert!(
        !stdout.contains("Deprecated:"),
        "warning text leaked onto stdout: {}",
        stdout
    );
    assert!(
        !stdout.contains("warning:"),
        "warning prefix leaked onto stdout: {}",
        stdout
    );
}
