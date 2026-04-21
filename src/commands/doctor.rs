//! `wg doctor` — environment diagnostic.
//!
//! A single command that walks the list of things workgraph needs to run
//! and reports each check's status. Aimed at the "I installed wg, something
//! doesn't work, why" case: surfacing the actual missing piece instead of
//! leaving users to diff obscure error messages against the docs.
//!
//! Most of the surface here is Windows-port adjacent — that's where the
//! gotchas live (bash vs TIMEOUT.EXE, OAuth-token-in-wrong-env-var, extended
//! path prefixes). On Unix the same checks still run but almost always go
//! green.
//!
//! Exit codes:
//!   0 — all green
//!   1 — one or more warnings but no hard errors
//!   2 — one or more errors; workgraph probably won't function correctly

use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Err,
    /// Informational — no judgement, just a fact the user might want to know.
    Info,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Ok, detail: detail.into(), hint: None }
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Warn, detail: detail.into(), hint: Some(hint.into()) }
    }
    fn err(name: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Err, detail: detail.into(), hint: Some(hint.into()) }
    }
    fn info(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: Status::Info, detail: detail.into(), hint: None }
    }
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    platform: String,
    wg_version: String,
    checks: Vec<Check>,
    summary: Summary,
}

#[derive(Debug, Serialize)]
struct Summary {
    ok: usize,
    warn: usize,
    err: usize,
    info: usize,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let mut checks: Vec<Check> = Vec::new();

    checks.extend(check_host_tools());
    checks.extend(check_auth(dir));
    checks.extend(check_workgraph_dir(dir));
    checks.extend(check_daemon(dir));
    #[cfg(windows)]
    checks.extend(check_windows_specific());

    let summary = Summary {
        ok: checks.iter().filter(|c| c.status == Status::Ok).count(),
        warn: checks.iter().filter(|c| c.status == Status::Warn).count(),
        err: checks.iter().filter(|c| c.status == Status::Err).count(),
        info: checks.iter().filter(|c| c.status == Status::Info).count(),
    };

    let exit_code = if summary.err > 0 { 2 } else if summary.warn > 0 { 1 } else { 0 };

    if json {
        let report = DoctorReport {
            platform: platform_string(),
            wg_version: env!("CARGO_PKG_VERSION").to_string(),
            checks,
            summary,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_plain(&checks, &summary);
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn platform_string() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn print_plain(checks: &[Check], summary: &Summary) {
    println!("=== wg doctor ===");
    println!("  Platform: {}", platform_string());
    println!("  wg version: {}", env!("CARGO_PKG_VERSION"));
    if let Ok(p) = std::env::current_exe() {
        println!("  wg exe: {}", p.display());
    }
    println!();

    for c in checks {
        let marker = match c.status {
            Status::Ok => "[OK]",
            Status::Warn => "[WARN]",
            Status::Err => "[ERR]",
            Status::Info => "[INFO]",
        };
        println!("  {:6} {}", marker, c.name);
        for line in c.detail.lines() {
            println!("           {}", line);
        }
        if let Some(h) = &c.hint {
            println!("           → {}", h);
        }
    }
    println!();
    println!(
        "  Summary: {} ok, {} warn, {} err ({} info)",
        summary.ok, summary.warn, summary.err, summary.info,
    );
}

// ── host tools ────────────────────────────────────────────────────────

fn check_host_tools() -> Vec<Check> {
    let mut out = Vec::new();

    // claude CLI
    match run_capture("claude", &["--version"]) {
        Some((true, stdout, _)) => {
            out.push(Check::ok(
                "claude CLI",
                format!("found: {}", stdout.lines().next().unwrap_or("").trim()),
            ));
        }
        Some((false, _, stderr)) => {
            out.push(Check::err(
                "claude CLI",
                format!("found but `claude --version` failed: {}", stderr.trim()),
                "Reinstall or reauthenticate via `claude login`.",
            ));
        }
        None => {
            out.push(Check::err(
                "claude CLI",
                "not found on PATH".into(),
                "Install Claude Code, or run `claude setup-token` for a headless token.".into(),
            ));
        }
    }

    // bash
    match run_capture("bash", &["--version"]) {
        Some((true, stdout, _)) => {
            let first = stdout.lines().next().unwrap_or("").trim().to_string();
            // Git-for-Windows bash identifies as msys or mingw.
            #[cfg(windows)]
            if first.contains("msys") || first.contains("mingw") {
                out.push(Check::ok("bash", format!("found (Git for Windows): {}", first)));
            } else {
                out.push(Check::warn(
                    "bash",
                    format!("found but not Git-for-Windows: {}", first),
                    "Install Git for Windows — workgraph spawns wrapper scripts via its bash. \
                     WSL's bash can't see Windows paths the same way."
                        .into(),
                ));
            }
            #[cfg(not(windows))]
            out.push(Check::ok("bash", format!("found: {}", first)));
        }
        _ => {
            out.push(Check::err(
                "bash",
                "not found on PATH".into(),
                "Install Git for Windows (on Windows) — workgraph wrappers require bash.".into(),
            ));
        }
    }

    // git
    match run_capture("git", &["--version"]) {
        Some((true, stdout, _)) => out.push(Check::ok(
            "git",
            format!("found: {}", stdout.trim()),
        )),
        _ => out.push(Check::err(
            "git",
            "not found on PATH".into(),
            "Install Git — workgraph uses `git worktree` for agent isolation.".into(),
        )),
    }

    // GNU timeout vs Windows TIMEOUT.EXE.
    // On Windows, the one on PATH may be either. The Windows one is an
    // interactive pause utility; the GNU one is a command wrapper.
    // Workgraph's wrapper scripts rely on the GNU behavior.
    #[cfg(windows)]
    {
        match run_capture("timeout", &["--help"]) {
            Some((_, stdout, stderr)) => {
                let combined = format!("{}{}", stdout, stderr);
                if combined.contains("Usage: timeout") || combined.contains("--kill-after") {
                    out.push(Check::ok("timeout(1)", "GNU coreutils `timeout` is first on PATH".into()));
                } else if combined.contains("TIMEOUT") && combined.contains("Default option") {
                    out.push(Check::err(
                        "timeout(1)",
                        "Windows TIMEOUT.EXE is first on PATH (not the GNU command wrapper)".into(),
                        "Put Git for Windows' `usr\\bin` ahead of `C:\\Windows\\System32` on the \
                         daemon's PATH, or wait for the upstream cross-platform timeout helper to land (#22)."
                            .into(),
                    ));
                } else {
                    out.push(Check::warn(
                        "timeout(1)",
                        format!("found but couldn't identify: {}",
                            combined.lines().next().unwrap_or("").trim()),
                        "Verify `timeout` is the GNU command wrapper from coreutils.".into(),
                    ));
                }
            }
            None => {
                out.push(Check::err(
                    "timeout(1)",
                    "not found on PATH".into(),
                    "Install Git for Windows (ships GNU coreutils including timeout).".into(),
                ));
            }
        }
    }
    #[cfg(unix)]
    {
        if run_capture("timeout", &["--help"]).is_some() {
            out.push(Check::ok("timeout(1)", "found".into()));
        }
    }

    out
}

// ── auth ──────────────────────────────────────────────────────────────

fn check_auth(dir: &Path) -> Vec<Check> {
    let mut out = Vec::new();

    // Detect the classic env-var trap first: sk-ant-oat01-… tokens in
    // ANTHROPIC_API_KEY. The CLI sends them with the wrong header and
    // 401s every time, silently; the user sees synthetic placeholder
    // "Invalid API key" responses.
    if let Ok(v) = std::env::var("ANTHROPIC_API_KEY") {
        let v = v.trim().to_string();
        if v.is_empty() {
            // empty is fine, handled by the fall-through checks below
        } else if v.starts_with("sk-ant-oat01-") {
            out.push(Check::err(
                "ANTHROPIC_API_KEY",
                "is set to an `sk-ant-oat01-…` OAuth token".into(),
                "OAuth tokens use Bearer auth; `ANTHROPIC_API_KEY` is sent as `x-api-key` and will \
                 always 401. Move it to `CLAUDE_CODE_OAUTH_TOKEN` instead."
                    .into(),
            ));
        } else if v.starts_with("sk-ant-api") {
            out.push(Check::ok(
                "ANTHROPIC_API_KEY",
                "set to a console API key".into(),
            ));
        } else {
            out.push(Check::warn(
                "ANTHROPIC_API_KEY",
                format!("set but doesn't match known prefixes: starts with `{}…`", &v[..v.len().min(12)]),
                "Double-check the token format.".into(),
            ));
        }
    }

    // OAuth token
    let oauth_env = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok().filter(|v| !v.is_empty());
    if let Some(v) = oauth_env.as_ref() {
        if v.starts_with("sk-ant-oat01-") {
            out.push(Check::ok(
                "CLAUDE_CODE_OAUTH_TOKEN",
                "set to an `sk-ant-oat01-…` OAuth token".into(),
            ));
        } else {
            out.push(Check::warn(
                "CLAUDE_CODE_OAUTH_TOKEN",
                "set but doesn't look like an `sk-ant-oat01-…` token".into(),
                "Verify the token format; OAuth tokens use the `sk-ant-oat01-` prefix.".into(),
            ));
        }
    }

    // MANAGED_BY_HOST / SDK refresh — the Claude-Code internal env vars
    // that leak through and misroute auth when the daemon was started from
    // inside a Claude Code session.
    if std::env::var("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST").is_ok() {
        out.push(Check::warn(
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
            "is set in the current shell".into(),
            "This env var tells `claude` to prefer the host bridge (Claude Code's auth IPC) over \
             any token you configure. Fine if you're running interactively inside Claude Code, \
             bad if a detached daemon inherits it — the daemon will silently 401 on every call. \
             Start the daemon from a shell where this isn't set, or upgrade to a workgraph build \
             that strips it from spawned children (#30)."
                .into(),
        ));
    }

    // credentials.json from `claude login`
    if let Some(home) = dirs::home_dir() {
        let creds = home.join(".claude").join("credentials.json");
        if creds.exists() {
            out.push(Check::ok(
                "~/.claude/credentials.json",
                format!("present at {}", creds.display()),
            ));
        } else if oauth_env.is_none() {
            // Only flag as warning if there's no other auth source
            out.push(Check::warn(
                "claude login",
                "no `credentials.json` and no `CLAUDE_CODE_OAUTH_TOKEN` in env".into(),
                "Run `claude login` for a refreshable credential, or set `CLAUDE_CODE_OAUTH_TOKEN`, \
                 or configure `[auth]` in `.workgraph/config.toml`."
                    .into(),
            ));
        }
    }

    // [auth] config
    let cfg_path = dir.join("config.toml");
    if cfg_path.exists()
        && let Ok(content) = std::fs::read_to_string(&cfg_path)
    {
        if content.contains("claude_code_oauth_token_file") {
            out.push(Check::ok("[auth] config", "token-file reference in .workgraph/config.toml".into()));
        } else if content.contains("claude_code_oauth_token") {
            out.push(Check::warn(
                "[auth] config",
                "inline token in .workgraph/config.toml".into(),
                "Prefer `claude_code_oauth_token_file` so the token doesn't live in a file that \
                 might be committed. Make sure `.workgraph/config.toml` is gitignored if you keep \
                 it inline."
                    .into(),
            ));
        }
    }

    out
}

// ── workgraph dir ─────────────────────────────────────────────────────

fn check_workgraph_dir(dir: &Path) -> Vec<Check> {
    let mut out = Vec::new();

    if !dir.exists() {
        out.push(Check::err(
            ".workgraph dir",
            format!("{} does not exist", dir.display()),
            "Run `wg init` in your project root.".into(),
        ));
        return out;
    }

    let graph = dir.join("graph.jsonl");
    if graph.exists() {
        // Rough count of task lines
        let line_count = std::fs::read_to_string(&graph)
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        out.push(Check::ok(
            "graph.jsonl",
            format!("{} entries", line_count),
        ));
    } else {
        out.push(Check::warn(
            "graph.jsonl",
            format!("missing at {}", graph.display()),
            "Run `wg init` to create an empty graph, or `wg add` to start.".into(),
        ));
    }

    out
}

// ── service daemon ────────────────────────────────────────────────────

fn check_daemon(dir: &Path) -> Vec<Check> {
    let mut out = Vec::new();

    let state_path = dir.join("service").join("state.json");
    if !state_path.exists() {
        out.push(Check::info("service daemon", "not running (no state.json)".into()));
        return out;
    }

    match std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    {
        Some(v) => {
            let pid = v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0);
            let socket = v.get("socket_path").and_then(|s| s.as_str()).unwrap_or("?").to_string();
            if pid > 0 {
                out.push(Check::ok(
                    "service daemon",
                    format!("PID {} (state.json)\nsocket: {}", pid, socket),
                ));
            } else {
                out.push(Check::warn(
                    "service daemon",
                    "state.json present but no PID".into(),
                    "Try `wg service stop --force` then `wg service start`.".into(),
                ));
            }
        }
        None => {
            out.push(Check::warn(
                "service daemon",
                "state.json unreadable or malformed".into(),
                "Remove `.workgraph/service/state.json` and restart with `wg service start`.".into(),
            ));
        }
    }

    out
}

// ── windows specifics ─────────────────────────────────────────────────

#[cfg(windows)]
fn check_windows_specific() -> Vec<Check> {
    let mut out = Vec::new();

    // `cargo install --path .` puts wg.exe in ~/.cargo/bin; report if
    // current_exe looks like the verbatim-path form so users know what
    // they're seeing in logs.
    if let Ok(p) = std::env::current_exe() {
        let s = p.to_string_lossy();
        if s.starts_with(r"\\?\") {
            out.push(Check::info(
                "extended-length path",
                format!("`current_exe` is {}", s),
            ));
        }
    }

    // Which `bash` did `where` return first?
    if let Some((true, stdout, _)) = run_capture("where", &["bash"]) {
        let first = stdout.lines().next().unwrap_or("").trim().to_string();
        if !first.is_empty() {
            out.push(Check::info("bash.exe path", first));
        }
    }

    out
}

// ── helpers ───────────────────────────────────────────────────────────

/// Run a command; return `(success, stdout, stderr)` or `None` if the
/// binary wasn't found on PATH.
fn run_capture(program: &str, args: &[&str]) -> Option<(bool, String, String)> {
    let output = Command::new(program).args(args).output().ok()?;
    Some((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

// Silence "unused import" on non-Windows where some helpers only apply to
// the Windows branches above.
#[allow(dead_code)]
fn _unused_import_suppressor(_p: &PathBuf) {}
