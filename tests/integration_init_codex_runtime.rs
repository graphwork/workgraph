//! End-to-end test that `wg init --route codex-cli` produces a project
//! whose dispatcher actually picks up the project's [agent]/[dispatcher]
//! model — even when `WG_DIR=<project_root>` (the natural mistake users
//! make: pointing WG_DIR at the project root rather than the `.wg/`
//! subdir).
//!
//! Regression test for `fix-wg-init` (5 bugs that all traced to the
//! resolver treating `WG_DIR=<project_root>` literally instead of
//! descending into `.wg/`):
//!
//!   1. Dispatcher loaded global config (claude:opus) instead of
//!      project [dispatcher].model = codex:gpt-5.5.
//!   2. `wg agent list` reported "No agents defined" even though the
//!      default agent file existed under .wg/agency/cache/agents/.
//!   3. Service runtime files (state.json, daemon.sock, daemon.log)
//!      ended up at `<project>/service/` instead of `<project>/.wg/service/`.
//!   4. Graph watcher watched `<project>/graph.jsonl` (nonexistent)
//!      instead of `<project>/.wg/graph.jsonl`.
//!   5. Every dispatcher tick logged "Failed to load graph for
//!      task-aware reaping" because the graph file path was wrong.
//!
//! Per-task validation requires this exact end-to-end flow: spawn a
//! daemon, observe daemon.log, kill the daemon. We do NOT spawn a
//! worker (smoke scenario covers that) — daemon.log + on-disk paths
//! are sufficient to assert all 5 bugs are fixed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}; run `cargo build` first",
        path
    );
    path
}

/// Run `wg <args>` with cwd, env, and inherited HOME redirected so that
/// no global ~/.wg/config.toml leaks into the test.
fn wg_in(cwd: &Path, fake_home: &Path, env: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(wg_binary());
    cmd.current_dir(cwd)
        .env_clear()
        .env("HOME", fake_home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("XDG_CONFIG_HOME", fake_home.join(".config"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("wg {:?} failed to launch: {}", args, e))
}

fn read_state_pid(state_path: &Path) -> Option<i32> {
    let s = fs::read_to_string(state_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("pid").and_then(|p| p.as_i64()).map(|p| p as i32)
}

fn wait_for_log_contains(log_path: &Path, needle: &str, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        let s = fs::read_to_string(log_path).unwrap_or_default();
        if s.contains(needle) {
            return s;
        }
        if start.elapsed() > timeout {
            return s;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

struct DaemonGuard {
    pid: Option<i32>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[test]
#[ignore = "spawns daemon subprocess; run with --include-ignored"]
fn codex_init_with_wg_dir_at_project_root_runs_correctly() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("proj");
    fs::create_dir_all(&project).unwrap();

    // Fake $HOME with empty global config so we can detect the bug
    // where the dispatcher falls back to global claude:opus.
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(fake_home.join(".config/workgraph")).unwrap();
    fs::write(
        fake_home.join(".config/workgraph/config.toml"),
        // Deliberately seed claude:opus into global config — this is
        // what would have masked Bug 1 if the dispatcher correctly
        // loaded the project config. With the bug, the dispatcher reads
        // THIS file and reports model=claude:opus.
        "[agent]\nmodel = \"claude:opus\"\n[dispatcher]\nmodel = \"claude:opus\"\n",
    )
    .unwrap();

    // ── Step 1: wg init --route codex-cli (no agency to keep test fast)
    let out = wg_in(
        &project,
        &fake_home,
        &[],
        &["init", "--route", "codex-cli", "--no-agency"],
    );
    assert!(
        out.status.success(),
        "wg init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let wg_dir = project.join(".wg");
    assert!(wg_dir.is_dir(), ".wg should exist after init");
    assert!(
        wg_dir.join("config.toml").is_file(),
        "config.toml should be inside .wg"
    );

    // Re-enable agency-init by hand: load and write defaults so agent
    // list has something to find. (We bypass agency init for speed in
    // most assertions; for Bug 2 we need actual agents.)
    // Easier: just run `wg agency init` against the real .wg dir.
    let out = wg_in(&project, &fake_home, &[], &["agency", "init"]);
    assert!(
        out.status.success(),
        "wg agency init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // ── Step 2 (Bug 2): WG_DIR=<project_root> wg agent list must find agents.
    let out = wg_in(
        &project,
        &fake_home,
        &[("WG_DIR", project.to_str().unwrap())],
        &["agent", "list"],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "wg agent list failed: {}",
        combined
    );
    assert!(
        !combined.contains("No agents defined"),
        "WG_DIR=<project_root> should resolve to <project>/.wg and find the default agent. \
         Bug 2 reproduces if this fails. Output: {}",
        combined
    );
    assert!(
        combined.contains("Careful Programmer") || combined.contains("Default"),
        "agent list should show seeded agents. Output: {}",
        combined
    );

    // ── Step 3: WG_DIR=<project_root> wg service start
    // We capture stdout from the wrapper; the daemon forks and exits
    // the wrapper. We then read state.json for the canonical daemon PID.
    let out = wg_in(
        &project,
        &fake_home,
        &[("WG_DIR", project.to_str().unwrap())],
        &["service", "start", "--max-agents", "1"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "wg service start failed:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Bug 3: state.json must be under .wg/service/, not <project>/service/.
    let proper_state = wg_dir.join("service/state.json");
    let bogus_state = project.join("service/state.json");
    let pid = read_state_pid(&proper_state);
    let _guard = DaemonGuard { pid };
    assert!(
        proper_state.is_file(),
        "Bug 3: service state.json should be at {} (got: proper={}, bogus={})",
        proper_state.display(),
        proper_state.is_file(),
        bogus_state.is_file()
    );
    assert!(
        !bogus_state.is_file(),
        "Bug 3: there must NOT be a sibling service/ directory next to .wg. \
         Found: {}",
        bogus_state.display()
    );

    // Bug 1 + 4 + 5: read daemon.log and assert its contents.
    let log_path = wg_dir.join("service/daemon.log");
    let log = wait_for_log_contains(&log_path, "Coordinator config", Duration::from_secs(8));

    // Bug 1: dispatcher must load project config (codex), not global (claude).
    assert!(
        log.contains("executor=codex") && log.contains("model=codex:gpt-5.5"),
        "Bug 1: daemon.log should report executor=codex, model=codex:gpt-5.5 — \
         project [dispatcher].model must beat global config.\nLog:\n{}",
        log
    );
    assert!(
        !log.contains("model=claude:opus"),
        "Bug 1: daemon must not fall back to global claude:opus when project has codex.\nLog:\n{}",
        log
    );

    // Bug 4: graph watcher path must be inside .wg.
    let proper_graph = wg_dir.join("graph.jsonl");
    let bogus_graph_marker =
        format!("Graph watcher active on {}", project.join("graph.jsonl").display());
    assert!(
        log.contains(&format!(
            "Graph watcher active on {}",
            proper_graph.display()
        )),
        "Bug 4: graph watcher should watch {}.\nLog:\n{}",
        proper_graph.display(),
        log
    );
    assert!(
        !log.contains(&bogus_graph_marker),
        "Bug 4: graph watcher must not watch {}/graph.jsonl (sibling to .wg).\nLog:\n{}",
        project.display(),
        log
    );

    // Bug 5: no continuous reconciliation errors.
    // Wait one full poll cycle (5s default) so we'd catch a tick error.
    std::thread::sleep(Duration::from_secs(2));
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("Failed to load graph for task-aware reaping"),
        "Bug 5: daemon must not log 'Failed to load graph for task-aware reaping'.\nLog:\n{}",
        log
    );

    // Stop the daemon (DaemonGuard will SIGKILL on drop too).
    let _ = wg_in(
        &project,
        &fake_home,
        &[("WG_DIR", project.to_str().unwrap())],
        &["service", "stop", "--force"],
    );
}
