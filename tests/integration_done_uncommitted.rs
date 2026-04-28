//! Integration tests for `wg done` refusal when the worktree branch has
//! staged-but-uncommitted tracked changes.
//!
//! Prior bug (wg-done-silent, 2026-04-28): when `commits_ahead == 0` and
//! `uncommitted_files > 0`, `attempt_worktree_merge` returned `NoCommits`
//! and `wg done` silently marked the task done — losing the agent's work.
//!
//! The fix: refuse `wg done` with an actionable error listing the affected
//! files. The task must NOT transition to Done.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

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

fn init_test_repo(path: &Path) {
    Command::new("git")
        .args(["init", "-b", "main"])
        .arg(path)
        .output()
        .expect("git init");

    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(path)
        .output()
        .expect("git config email");
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .expect("git config name");

    fs::write(path.join("README.md"), "initial\n").unwrap();
    Command::new("git")
        .args(["add", "README.md"])
        .current_dir(path)
        .output()
        .expect("git add");
    Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(path)
        .output()
        .expect("git commit");
}

fn create_worktree(project_root: &Path, agent_id: &str, task_id: &str) -> (PathBuf, String) {
    let worktree_dir = project_root.join(".wg-worktrees").join(agent_id);
    let branch = format!("wg/{}/{}", agent_id, task_id);
    fs::create_dir_all(worktree_dir.parent().unwrap()).unwrap();

    let out = Command::new("git")
        .args(["worktree", "add"])
        .arg(&worktree_dir)
        .args(["-b", &branch, "HEAD"])
        .current_dir(project_root)
        .output()
        .expect("git worktree add");
    assert!(
        out.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (worktree_dir, branch)
}

fn setup_graph(project_root: &Path, task_id: &str) -> PathBuf {
    let wg_dir = project_root.join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: task_id.to_string(),
        title: task_id.to_string(),
        status: Status::InProgress,
        ..Task::default()
    }));
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

/// `wg done` MUST refuse when the agent's worktree has staged-but-uncommitted
/// tracked changes — the prior behavior silently dropped them.
#[test]
fn wg_done_refuses_staged_uncommitted_in_worktree() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path().join("project");
    fs::create_dir_all(&project_root).unwrap();
    init_test_repo(&project_root);

    let task_id = "test-task";
    let agent_id = "agent-test-1";
    let (worktree_dir, branch) = create_worktree(&project_root, agent_id, task_id);
    let wg_dir = setup_graph(&project_root, task_id);

    // Agent stages a file but does NOT commit. This is the bug repro.
    let staged_file = "AGENTS.md";
    fs::write(worktree_dir.join(staged_file), "agent work\n").unwrap();
    Command::new("git")
        .args(["add", staged_file])
        .current_dir(&worktree_dir)
        .output()
        .expect("git add in worktree");

    // Sanity: confirm the staging is real and there are 0 commits ahead of main.
    let status_out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&worktree_dir)
        .output()
        .expect("git status");
    assert!(
        String::from_utf8_lossy(&status_out.stdout).contains(staged_file),
        "expected {} to be staged in worktree",
        staged_file
    );

    // Run `wg done` with worktree env vars exactly as the dispatcher would.
    // Clear WG_AGENT_ID so --skip-smoke is permitted (the test is the human, not an agent).
    let output = Command::new(wg_binary())
        .arg("--dir")
        .arg(&wg_dir)
        .args(["done", task_id, "--skip-smoke"])
        .env("WG_WORKTREE_PATH", &worktree_dir)
        .env("WG_BRANCH", &branch)
        .env("WG_PROJECT_ROOT", &project_root)
        .env_remove("WG_AGENT_ID")
        .env_remove("WG_SMOKE_AGENT_OVERRIDE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run wg done");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // 1. Non-zero exit code.
    assert!(
        !output.status.success(),
        "wg done MUST refuse when worktree has uncommitted staged changes.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // 2. Error message names the staged file.
    assert!(
        stderr.contains(staged_file),
        "error message must name the uncommitted file '{}'.\nstderr: {}",
        staged_file,
        stderr
    );

    // 3. Error mentions `git commit` or "uncommitted" so the agent knows the fix.
    let mentions_fix = stderr.to_lowercase().contains("commit")
        || stderr.to_lowercase().contains("uncommitted");
    assert!(
        mentions_fix,
        "error must mention how to fix (commit / uncommitted).\nstderr: {}",
        stderr
    );

    // 4. Task is NOT marked done.
    let graph = load_graph(&wg_dir.join("graph.jsonl")).expect("load graph");
    let task = graph.get_task(task_id).expect("task exists");
    assert_ne!(
        task.status,
        Status::Done,
        "task MUST NOT be marked done when wg done refused"
    );
    assert_ne!(
        task.status,
        Status::PendingEval,
        "task MUST NOT be moved to PendingEval when wg done refused"
    );

    // 5. The staged file did NOT land on main.
    let on_main = Command::new("git")
        .args(["ls-tree", "main", "--", staged_file])
        .current_dir(&project_root)
        .output()
        .expect("git ls-tree");
    assert!(
        on_main.stdout.is_empty(),
        "staged file MUST NOT have been merged to main: {}",
        String::from_utf8_lossy(&on_main.stdout)
    );
}

/// Untracked files (`??` in git status) must NOT block `wg done` — they're
/// noise from cargo target dirs, editor scratch files, etc. Only staged or
/// modified tracked files indicate lost work.
#[test]
fn wg_done_does_not_block_on_untracked_files() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path().join("project");
    fs::create_dir_all(&project_root).unwrap();
    init_test_repo(&project_root);

    let task_id = "untracked-ok";
    let agent_id = "agent-untracked";
    let (worktree_dir, branch) = create_worktree(&project_root, agent_id, task_id);
    let wg_dir = setup_graph(&project_root, task_id);

    // Drop an untracked file in the worktree — but do NOT stage or commit it.
    fs::write(worktree_dir.join("scratch.tmp"), "scratch\n").unwrap();

    let output = Command::new(wg_binary())
        .arg("--dir")
        .arg(&wg_dir)
        .args(["done", task_id, "--skip-smoke"])
        .env("WG_WORKTREE_PATH", &worktree_dir)
        .env("WG_BRANCH", &branch)
        .env("WG_PROJECT_ROOT", &project_root)
        .env_remove("WG_AGENT_ID")
        .env_remove("WG_SMOKE_AGENT_OVERRIDE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run wg done");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wg done MUST succeed when only untracked files are present (NoCommits path).\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Task should be Done (no eval task exists, so it goes straight to Done).
    let graph = load_graph(&wg_dir.join("graph.jsonl")).expect("load graph");
    let task = graph.get_task(task_id).expect("task exists");
    assert_eq!(task.status, Status::Done);
}

/// The genuine NoCommits case (clean worktree, branch even with main) MUST
/// emit a `[merge]` log line so the agent sees what happened — no more silent
/// no-op.
#[test]
fn wg_done_logs_no_commits_branch() {
    let temp = TempDir::new().unwrap();
    let project_root = temp.path().join("project");
    fs::create_dir_all(&project_root).unwrap();
    init_test_repo(&project_root);

    let task_id = "no-commits-task";
    let agent_id = "agent-clean";
    let (worktree_dir, branch) = create_worktree(&project_root, agent_id, task_id);
    let wg_dir = setup_graph(&project_root, task_id);

    let output = Command::new(wg_binary())
        .arg("--dir")
        .arg(&wg_dir)
        .args(["done", task_id, "--skip-smoke"])
        .env("WG_WORKTREE_PATH", &worktree_dir)
        .env("WG_BRANCH", &branch)
        .env("WG_PROJECT_ROOT", &project_root)
        .env_remove("WG_AGENT_ID")
        .env_remove("WG_SMOKE_AGENT_OVERRIDE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run wg done");

    let stderr_for_msg = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg done should succeed on clean NoCommits worktree.\nstderr: {}",
        stderr_for_msg
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[merge]") && stderr.to_lowercase().contains("no commits"),
        "expected `[merge] No commits ...` log line in stderr.\nstderr: {}",
        stderr
    );
}
