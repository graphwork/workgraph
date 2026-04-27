//! Regression tests for Bug A — orphan chat supervisor.
//!
//! History:
//! 1. The original fix made the daemon boot path enumerate `.chat-N` tasks
//!    from the graph instead of hardcoding `coordinator-0`.
//! 2. A subsequent rename agent silently REVERTED that fix; daemon-boot
//!    once again hardcoded "spawn coordinator-0 with task `.chat-0`", so a
//!    fresh `wg init` (no `.chat-0` task) burned its restart budget chasing
//!    a phantom forever.
//! 3. No regression test pinned the original fix → no signal when the
//!    revert landed.
//!
//! These tests pin the orphan-guard invariants:
//!   - boot enumerates from graph (no `.chat-N` task → zero supervisors)
//!   - `.chat-3` in graph → exactly one supervisor for chat_id=3
//!   - legacy `.coordinator-1` in graph → loaded with deprecation warning,
//!     supervisor for chat_id=1 spawned
//!
//! The pure-function tests below run cheaply on every `cargo test` invocation
//! and exercise the helper that the daemon boot path calls. The
//! `daemon_boot_*` tests at the bottom drive the full subprocess path
//! (`wg service start` against a tempdir) and assert daemon.log content.
//!
//! Wired into `scripts/smoke/wave-1-smoke.sh` so any future PR that reverts
//! the orphan-guard logic surfaces in smoke output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serial_test::serial;

use workgraph::chat_id::{CHAT_LOOP_TAG, LEGACY_COORDINATOR_LOOP_TAG};
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::service::{
    ChatSupervisorBootSpec, enumerate_chat_supervisors_from_graph,
};

// ---------------------------------------------------------------------------
// Pure-function tests — exercise the helper directly. These are the load-bearing
// regression assertions: they will fail to compile or fail at assert time if
// `enumerate_chat_supervisors_from_graph` regresses to "always spawn coordinator-0".
// ---------------------------------------------------------------------------

fn chat_task(id: &str, tag: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: id.to_string(),
        status,
        tags: vec![tag.to_string()],
        ..Default::default()
    }
}

#[test]
fn test_dispatcher_boot_no_chat_tasks_spawns_no_supervisor() {
    // Fresh `wg init` shape: graph exists but contains no chat-loop task.
    // Boot enumeration MUST return zero supervisors. Hardcoding
    // `[ChatSupervisorBootSpec { chat_id: 0, .. }]` here would have been
    // the bug the rename agent reintroduced.
    let g = WorkGraph::new();
    let out = enumerate_chat_supervisors_from_graph(&g);
    assert!(
        out.is_empty(),
        "fresh graph must yield zero chat supervisors (Bug A regression: would otherwise spawn phantom coordinator-0). got: {:?}",
        out
    );

    // Even with non-chat tasks present, no supervisors should be spawned.
    let mut g2 = WorkGraph::new();
    g2.add_node(Node::Task(Task {
        id: "real-task".to_string(),
        title: "real".to_string(),
        status: Status::Open,
        ..Default::default()
    }));
    let out2 = enumerate_chat_supervisors_from_graph(&g2);
    assert!(
        out2.is_empty(),
        "non-chat tasks must not produce chat supervisors. got: {:?}",
        out2
    );
}

#[test]
fn test_dispatcher_boot_enumerates_chat_tasks_from_graph() {
    // `.chat-3` exists → exactly one supervisor with chat_id=3.
    // The bug version of this code would (wrongly) ignore chat_id=3 and
    // spawn chat_id=0 instead, so this test catches both halves of the
    // regression: enumerate from graph AND use the right id.
    let mut g = WorkGraph::new();
    g.add_node(Node::Task(chat_task(
        ".chat-3",
        CHAT_LOOP_TAG,
        Status::InProgress,
    )));
    let out = enumerate_chat_supervisors_from_graph(&g);
    assert_eq!(
        out,
        vec![ChatSupervisorBootSpec {
            chat_id: 3,
            is_legacy: false
        }],
        "exactly one supervisor with chat_id=3, non-legacy"
    );
}

#[test]
fn test_legacy_coordinator_prefix_loaded() {
    // `.coordinator-1` legacy task → loaded as a supervisor with `is_legacy=true`
    // so the boot path can emit a one-time deprecation warning when it
    // spawns the supervisor.
    let mut g = WorkGraph::new();
    g.add_node(Node::Task(chat_task(
        ".coordinator-1",
        LEGACY_COORDINATOR_LOOP_TAG,
        Status::InProgress,
    )));
    let out = enumerate_chat_supervisors_from_graph(&g);
    assert_eq!(
        out,
        vec![ChatSupervisorBootSpec {
            chat_id: 1,
            is_legacy: true
        }],
        "legacy `.coordinator-1` must produce a supervisor flagged is_legacy=true \
         so the daemon emits a deprecation warning at spawn time"
    );
}

// ---------------------------------------------------------------------------
// Subprocess-level tests — actually start the daemon and assert daemon.log
// content. Marked `#[serial]` because they each spawn a daemon process.
//
// These complement the pure-function tests above: they catch the case where
// somebody reintroduces a hardcoded `CoordinatorAgent::spawn(0)` call AROUND
// the `enumerate_chat_supervisors_for_boot` call (e.g., always spawning 0
// in addition to whatever the helper says). The pure-function tests can't
// catch that — these tests can.
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

fn fake_home_for(wg_dir: &Path) -> PathBuf {
    wg_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| wg_dir.to_path_buf())
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .env("HOME", fake_home_for(wg_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("wg {:?} failed to launch: {}", args, e))
}

fn wg_init(tmp_root: &Path) -> PathBuf {
    let wg_dir = tmp_root.join(".workgraph");
    let out = wg_cmd(&wg_dir, &["init", "--executor", "shell", "--no-agency"]);
    assert!(
        out.status.success(),
        "wg init failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // Disable agency auto-bootstrap to keep the test deterministic.
    fs::write(
        wg_dir.join("config.toml"),
        "[agency]\nauto_assign = false\nauto_evaluate = false\nauto_evolve = false\n",
    )
    .unwrap();
    wg_dir
}

fn append_chat_task_to_graph(wg_dir: &Path, task_id: &str, tag: &str) {
    let gp = wg_dir.join("graph.jsonl");
    let g = workgraph::parser::load_graph(&gp).expect("load graph");
    let mut g = g;
    g.add_node(Node::Task(Task {
        id: task_id.to_string(),
        title: format!("Chat {}", task_id),
        status: Status::InProgress,
        tags: vec![tag.to_string()],
        ..Default::default()
    }));
    workgraph::parser::save_graph(&g, &gp).expect("save graph");
}

fn stop_service(wg_dir: &Path) {
    let _ = wg_cmd(wg_dir, &["service", "stop", "--force", "--kill-agents"]);
}

struct DaemonGuard<'a> {
    wg_dir: &'a Path,
}

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        stop_service(self.wg_dir);
        let state_path = self.wg_dir.join("service").join("state.json");
        if let Ok(content) = fs::read_to_string(&state_path)
            && let Ok(state) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(pid) = state["pid"].as_u64()
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

fn start_daemon_with_chat(wg_dir: &Path) {
    let socket = format!("{}/wg-test.sock", wg_dir.parent().unwrap().display());
    let _ = wg_cmd(
        wg_dir,
        &[
            "service",
            "start",
            "--socket",
            &socket,
            "--executor",
            "shell",
            "--max-agents",
            "1",
            "--interval",
            "60",
        ],
    );
}

fn read_daemon_log(wg_dir: &Path) -> String {
    let p = wg_dir.join("service").join("daemon.log");
    fs::read_to_string(&p).unwrap_or_default()
}

fn wait_for_log<F: Fn(&str) -> bool>(wg_dir: &Path, timeout: Duration, pred: F) -> String {
    let start = Instant::now();
    loop {
        let content = read_daemon_log(wg_dir);
        if pred(&content) {
            return content;
        }
        if start.elapsed() > timeout {
            return content;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
#[serial]
#[ignore = "spawns daemon subprocess; run with --include-ignored"]
fn daemon_boot_no_chat_tasks_no_supervisor_in_log() {
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = wg_init(tmp.path());
    let _guard = DaemonGuard { wg_dir: &wg_dir };

    start_daemon_with_chat(&wg_dir);

    // Give the daemon a moment to write its boot messages.
    let log = wait_for_log(&wg_dir, Duration::from_secs(5), |s| {
        s.contains("Coordinator config")
    });

    // Bug A regression: the BROKEN boot path emits "Coordinator agent 0
    // spawned successfully" even on a fresh init with no `.chat-0` task.
    // The fixed boot path emits the "No chat-loop tasks in graph" line.
    let has_phantom_spawn_log = log.contains("Coordinator agent 0 spawned successfully")
        || log.contains("Coordinator-0: spawning via `wg spawn-task .chat-0`");
    assert!(
        !has_phantom_spawn_log,
        "Bug A regression: fresh init must NOT produce a Coordinator-0 spawn log. \
         daemon.log:\n{}",
        log
    );

    // Positive assertion: the fixed code logs an explicit "no chat-loop tasks" line.
    assert!(
        log.contains("No chat-loop tasks in graph"),
        "expected boot path to log 'No chat-loop tasks in graph' on fresh init. \
         daemon.log:\n{}",
        log
    );
}

#[test]
#[serial]
#[ignore = "spawns daemon subprocess; run with --include-ignored"]
fn daemon_boot_with_chat_3_spawns_supervisor_3() {
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = wg_init(tmp.path());
    let _guard = DaemonGuard { wg_dir: &wg_dir };

    append_chat_task_to_graph(&wg_dir, ".chat-3", CHAT_LOOP_TAG);

    start_daemon_with_chat(&wg_dir);

    let log = wait_for_log(&wg_dir, Duration::from_secs(5), |s| {
        s.contains("Spawning") || s.contains("No chat-loop tasks")
    });

    assert!(
        log.contains("Coordinator agent 3 spawned successfully")
            || log.contains("Spawning 1 chat supervisor(s) from graph: .chat-3"),
        "expected supervisor for chat_id=3 to be spawned. daemon.log:\n{}",
        log
    );
    assert!(
        !log.contains("Coordinator agent 0 spawned successfully"),
        "Bug A regression: must NOT spawn coordinator-0 when only .chat-3 exists. \
         daemon.log:\n{}",
        log
    );
}

#[test]
#[serial]
#[ignore = "spawns daemon subprocess; run with --include-ignored"]
fn daemon_boot_with_legacy_coordinator_1_loads_with_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = wg_init(tmp.path());
    let _guard = DaemonGuard { wg_dir: &wg_dir };

    append_chat_task_to_graph(&wg_dir, ".coordinator-1", LEGACY_COORDINATOR_LOOP_TAG);

    start_daemon_with_chat(&wg_dir);

    let log = wait_for_log(&wg_dir, Duration::from_secs(5), |s| {
        s.contains("Coordinator agent 1 spawned successfully")
            || s.contains("Spawning 1 chat supervisor")
    });

    assert!(
        log.contains("Loading legacy `.coordinator-1` task")
            || log.contains("legacy `.coordinator-1`"),
        "expected one-time deprecation warning for legacy `.coordinator-1`. \
         daemon.log:\n{}",
        log
    );
    assert!(
        log.contains("Coordinator agent 1 spawned successfully"),
        "expected supervisor for chat_id=1 to be spawned for the legacy task. \
         daemon.log:\n{}",
        log
    );
}

// ---------------------------------------------------------------------------
// Bonus: pre-flight task-exists check in the supervisor loop. This pins the
// other half of the orphan-guard fix — even if the boot path were buggy and
// asked for a non-existent chat id, the supervisor loop must NOT hot-loop
// `wg spawn-task` against a missing ID.
// ---------------------------------------------------------------------------

#[test]
fn pre_flight_orphan_check_constants_are_observable() {
    // The spec text the supervisor logs when it sees an orphan ID is part of
    // the contract. If someone changes the wording it's their job to update
    // smoke / dashboards too. Pin the substrings that grafana-style alerts
    // and the wave-1 smoke script grep for.
    let needles = ["orphan supervisor", "no restart loop"];
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/commands/service/coordinator_agent.rs"
    ))
    .expect("read coordinator_agent.rs source");
    for needle in needles {
        // We can't easily call `subprocess_coordinator_loop` here without
        // a full Daemon harness — instead, we verify the source string is
        // present in the binary by checking the source file is the source
        // of truth. This is a low-cost canary: if someone removes the
        // pre-flight error message, this assertion fails fast.
        assert!(
            src.contains(needle),
            "pre-flight orphan-supervisor error string '{}' missing from coordinator_agent.rs — \
             Bug A regression: someone may have removed the pre-flight task-exists check.",
            needle
        );
    }
}

/// Source-level canary that pins the BOOT PATH against the specific revert
/// the rename agent did. The bug pattern was:
///
///     coordinator_agent::CoordinatorAgent::spawn(
///         &dir,
///         0,  // hardcoded chat ID
///         ...
///     )
///
/// The fix uses `enumerate_chat_supervisors_for_boot` to drive a loop. This
/// test asserts the source contains the loop and does NOT contain the
/// hardcoded form. It complements the pure-function tests above (which only
/// catch helper logic regressions, not the case where someone removes the
/// helper call entirely).
#[test]
fn boot_path_must_call_enumerate_supervisors_helper() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/commands/service/mod.rs"
    ))
    .expect("read commands/service/mod.rs source");
    assert!(
        src.contains("enumerate_chat_supervisors_for_boot"),
        "Bug A regression: src/commands/service/mod.rs no longer calls \
         `enumerate_chat_supervisors_for_boot`. The boot path must enumerate \
         chat tasks from the graph instead of hardcoding `coordinator-0`. \
         See task `regression-test-for` for context."
    );
    // The historic broken form passed `0` as a literal chat ID into
    // `CoordinatorAgent::spawn` followed by the daemon model arg. Match the
    // exact 2-line shape the rename agent reintroduced. (Spaces tolerated
    // around the comma so a lazy whitespace edit doesn't slip past.)
    let normalized = src
        .lines()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !normalized.contains("CoordinatorAgent::spawn(\n&dir,\n0,"),
        "Bug A regression: src/commands/service/mod.rs hardcodes \
         `CoordinatorAgent::spawn(&dir, 0, ...)` at boot. Use \
         `enumerate_chat_supervisors_for_boot(&dir)` and spawn one supervisor \
         per returned id."
    );
}
