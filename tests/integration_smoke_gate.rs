//! Smoke gate integration tests
//!
//! These exercise `wg done` end-to-end via the real `wg` binary, with a
//! synthetic smoke manifest pointing at scripts that pass / fail / skip.
//! They lock in the rule:
//!
//!   * a task cannot be marked done while it owns a failing smoke scenario
//!   * a task IS marked done when every owned scenario passes (or skips loud)
//!
//! We deliberately avoid touching the real `tests/smoke/manifest.toml` so the
//! tests pass in any environment.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::Status;
use workgraph::parser::load_graph;

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current exe path");
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

fn wg_cmd_with_env(
    wg_dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = Command::new(wg_binary());
    cmd.arg("--dir").arg(wg_dir).args(args);
    // Make sure no ambient WG_AGENT_ID leaks in (parent test runner may have
    // set it). Tests opt into agent context explicitly via env.
    cmd.env_remove("WG_AGENT_ID");
    cmd.env_remove("WG_SMOKE_AGENT_OVERRIDE");
    cmd.env_remove("WG_SMOKE_MANIFEST");
    // Also strip any inherited worktree context — when this suite runs from
    // inside an agent's worktree, WG_WORKTREE_PATH/BRANCH/PROJECT_ROOT point
    // at the *agent's* worktree, and `wg done`'s worktree-merge codepath
    // would look at that worktree's git status (not the temp-dir fixture).
    cmd.env_remove("WG_WORKTREE_PATH");
    cmd.env_remove("WG_BRANCH");
    cmd.env_remove("WG_PROJECT_ROOT");
    cmd.env_remove("WG_WORKTREE_ACTIVE");
    cmd.env_remove("WG_TASK_ID");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perm = fs::metadata(path).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(path, perm).expect("chmod script");
}

fn make_pass_script(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    write_executable(&p, "#!/usr/bin/env bash\nexit 0\n");
    p
}

fn make_fail_script(dir: &Path, name: &str, msg: &str) -> PathBuf {
    let p = dir.join(name);
    let body = format!(
        "#!/usr/bin/env bash\necho '{}' 1>&2\nexit 7\n",
        msg.replace('\'', "'\\''")
    );
    write_executable(&p, &body);
    p
}

fn make_skip_script(dir: &Path, name: &str, reason: &str) -> PathBuf {
    let p = dir.join(name);
    let body = format!(
        "#!/usr/bin/env bash\necho '{}' 1>&2\nexit 77\n",
        reason.replace('\'', "'\\''")
    );
    write_executable(&p, &body);
    p
}

fn init_with_task(tmp: &Path, task_id: &str) -> PathBuf {
    let wg_dir = tmp.join(".workgraph");
    let init = wg_cmd_with_env(&wg_dir, &["init", "--executor", "shell"], &[]);
    assert!(
        init.status.success(),
        "wg init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let claim_id = wg_cmd_with_env(
        &wg_dir,
        &[
            "add",
            "Task under test",
            "--id",
            task_id,
            "--immediate",
        ],
        &[],
    );
    assert!(
        claim_id.status.success(),
        "wg add failed: {}",
        String::from_utf8_lossy(&claim_id.stderr)
    );
    let claim = wg_cmd_with_env(&wg_dir, &["claim", task_id], &[]);
    assert!(
        claim.status.success(),
        "wg claim failed: {}",
        String::from_utf8_lossy(&claim.stderr)
    );
    wg_dir
}

#[test]
fn test_done_blocks_when_smoke_scenario_fails() {
    let tmp = TempDir::new().unwrap();
    let task_id = "fence-task";
    let wg_dir = init_with_task(tmp.path(), task_id);

    // Set up manifest with a scenario owned by our task that always FAILS.
    let manifest_dir = tmp.path().join("smoke");
    fs::create_dir_all(&manifest_dir).unwrap();
    make_fail_script(&manifest_dir, "always_fails.sh", "intentional smoke failure");
    let manifest_path = manifest_dir.join("manifest.toml");
    fs::write(
        &manifest_path,
        format!(
            r#"
[[scenario]]
name = "always_fails"
script = "always_fails.sh"
owners = ["{}"]
description = "Test scenario that intentionally fails"
timeout_seconds = 10
"#,
            task_id
        ),
    )
    .unwrap();

    let out = wg_cmd_with_env(
        &wg_dir,
        &["done", task_id],
        &[("WG_SMOKE_MANIFEST", manifest_path.to_str().unwrap())],
    );
    assert!(
        !out.status.success(),
        "wg done should refuse when smoke scenario fails. stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Smoke gate refused"),
        "stderr should mention 'Smoke gate refused', got: {}",
        stderr
    );
    assert!(
        stderr.contains("always_fails"),
        "stderr should name the broken scenario, got: {}",
        stderr
    );

    // Task must remain in-progress, not done.
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task(task_id).unwrap();
    assert_eq!(
        task.status,
        Status::InProgress,
        "task should remain in-progress when smoke gate refuses"
    );
}

#[test]
fn test_done_succeeds_when_all_owned_scenarios_pass() {
    let tmp = TempDir::new().unwrap();
    let task_id = "happy-task";
    let wg_dir = init_with_task(tmp.path(), task_id);

    let manifest_dir = tmp.path().join("smoke");
    fs::create_dir_all(&manifest_dir).unwrap();
    make_pass_script(&manifest_dir, "ok.sh");
    make_skip_script(&manifest_dir, "skipme.sh", "endpoint unreachable");
    // A failing scenario owned by ANOTHER task — must NOT be run for happy-task.
    make_fail_script(&manifest_dir, "other_fail.sh", "should not run");
    let manifest_path = manifest_dir.join("manifest.toml");
    fs::write(
        &manifest_path,
        format!(
            r#"
[[scenario]]
name = "happy_pass"
script = "ok.sh"
owners = ["{task_id}"]

[[scenario]]
name = "happy_skip"
script = "skipme.sh"
owners = ["{task_id}"]

[[scenario]]
name = "other_owner_fail"
script = "other_fail.sh"
owners = ["some-other-task"]
"#,
            task_id = task_id
        ),
    )
    .unwrap();

    let out = wg_cmd_with_env(
        &wg_dir,
        &["done", task_id],
        &[("WG_SMOKE_MANIFEST", manifest_path.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "wg done should succeed when owned scenarios pass/skip. stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("happy_pass"),
        "stderr should record running 'happy_pass', got: {}",
        stderr
    );
    assert!(
        !stderr.contains("other_owner_fail"),
        "stderr must NOT mention non-owned scenario 'other_owner_fail', got: {}",
        stderr
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task(task_id).unwrap();
    assert_eq!(task.status, Status::Done, "task should be marked done");
}

#[test]
fn test_full_smoke_runs_every_scenario_regardless_of_ownership() {
    let tmp = TempDir::new().unwrap();
    let task_id = "all-or-nothing";
    let wg_dir = init_with_task(tmp.path(), task_id);

    let manifest_dir = tmp.path().join("smoke");
    fs::create_dir_all(&manifest_dir).unwrap();
    make_pass_script(&manifest_dir, "ok.sh");
    make_fail_script(&manifest_dir, "other_fail.sh", "from elsewhere");
    let manifest_path = manifest_dir.join("manifest.toml");
    fs::write(
        &manifest_path,
        format!(
            r#"
[[scenario]]
name = "owned_pass"
script = "ok.sh"
owners = ["{task_id}"]

[[scenario]]
name = "foreign_fail"
script = "other_fail.sh"
owners = ["unrelated-task"]
"#,
            task_id = task_id
        ),
    )
    .unwrap();

    // Without --full-smoke, the foreign failure does not run; done succeeds.
    let out = wg_cmd_with_env(
        &wg_dir,
        &["done", task_id],
        &[("WG_SMOKE_MANIFEST", manifest_path.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "owned-only mode should let done pass. stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Reset task to in-progress to test --full-smoke path.
    // (Use claim with --force semantics by re-adding; simpler: create another task.)
    let task_id2 = "all-or-nothing-2";
    let add = wg_cmd_with_env(
        &wg_dir,
        &["add", "Task two", "--id", task_id2, "--immediate"],
        &[],
    );
    assert!(add.status.success());
    let claim = wg_cmd_with_env(&wg_dir, &["claim", task_id2], &[]);
    assert!(claim.status.success());

    let out = wg_cmd_with_env(
        &wg_dir,
        &["done", task_id2, "--full-smoke"],
        &[("WG_SMOKE_MANIFEST", manifest_path.to_str().unwrap())],
    );
    assert!(
        !out.status.success(),
        "--full-smoke must surface the foreign-owned failure. stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("foreign_fail"),
        "stderr should mention foreign_fail, got: {}",
        stderr
    );
}

#[test]
fn test_skip_smoke_blocked_for_agents_without_override() {
    let tmp = TempDir::new().unwrap();
    let task_id = "agent-bypass";
    let wg_dir = init_with_task(tmp.path(), task_id);

    let manifest_dir = tmp.path().join("smoke");
    fs::create_dir_all(&manifest_dir).unwrap();
    make_fail_script(&manifest_dir, "always_fail.sh", "still broken");
    let manifest_path = manifest_dir.join("manifest.toml");
    fs::write(
        &manifest_path,
        format!(
            r#"
[[scenario]]
name = "always_fail"
script = "always_fail.sh"
owners = ["{}"]
"#,
            task_id
        ),
    )
    .unwrap();

    // Simulate agent: WG_AGENT_ID set, no override → --skip-smoke must fail.
    let out = wg_cmd_with_env(
        &wg_dir,
        &["done", task_id, "--skip-smoke"],
        &[
            ("WG_SMOKE_MANIFEST", manifest_path.to_str().unwrap()),
            ("WG_AGENT_ID", "test-agent"),
        ],
    );
    assert!(
        !out.status.success(),
        "agents must not be able to bypass smoke gate without override"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Agents cannot use --skip-smoke"),
        "stderr should mention agent restriction, got: {}",
        stderr
    );
}

#[test]
fn test_skip_smoke_works_for_humans() {
    let tmp = TempDir::new().unwrap();
    let task_id = "human-bypass";
    let wg_dir = init_with_task(tmp.path(), task_id);

    let manifest_dir = tmp.path().join("smoke");
    fs::create_dir_all(&manifest_dir).unwrap();
    make_fail_script(&manifest_dir, "always_fail.sh", "still broken");
    let manifest_path = manifest_dir.join("manifest.toml");
    fs::write(
        &manifest_path,
        format!(
            r#"
[[scenario]]
name = "always_fail"
script = "always_fail.sh"
owners = ["{}"]
"#,
            task_id
        ),
    )
    .unwrap();

    // Human (no WG_AGENT_ID): --skip-smoke should let done succeed.
    let out = wg_cmd_with_env(
        &wg_dir,
        &["done", task_id, "--skip-smoke"],
        &[("WG_SMOKE_MANIFEST", manifest_path.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "human --skip-smoke should let done succeed. stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WARNING") && stderr.contains("--skip-smoke"),
        "human --skip-smoke must warn loudly, got: {}",
        stderr
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task(task_id).unwrap();
    assert_eq!(task.status, Status::Done);
}

#[test]
fn test_no_manifest_means_no_gate() {
    let tmp = TempDir::new().unwrap();
    let task_id = "no-manifest";
    let wg_dir = init_with_task(tmp.path(), task_id);

    // Point WG_SMOKE_MANIFEST at a non-existent path — gate should no-op.
    let bogus = tmp.path().join("no-such-file.toml");
    let out = wg_cmd_with_env(
        &wg_dir,
        &["done", task_id],
        &[("WG_SMOKE_MANIFEST", bogus.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "wg done should succeed when manifest is absent. stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task(task_id).unwrap();
    assert_eq!(task.status, Status::Done);
}
