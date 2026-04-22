//! `wg gc --worktrees` — safe garbage collection of orphaned agent worktrees.
//!
//! Counterpart to `wg gc` (task graph GC): removes `.wg-worktrees/agent-*`
//! directories whose owning agents are terminal AND whose owning tasks are
//! terminal. Deliberately conservative — skips anything that might still be
//! live, anything with uncommitted changes, and anything it can't match to a
//! registry entry (unless `--force` is passed).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use workgraph::graph::Status;
use workgraph::parser::load_graph;
use workgraph::service::{AgentEntry, AgentRegistry, AgentStatus};

use super::graph_path;

/// Heartbeat freshness window (seconds) for liveness checks, matching the
/// service worker constant. An agent whose heartbeat is within this window
/// is considered still alive and its worktree untouchable.
const HEARTBEAT_LIVENESS_TIMEOUT_SECS: u64 = 300;

/// A per-worktree classification result.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Passes all safety gates — safe to remove.
    Remove {
        agent_id: String,
        path: PathBuf,
        task_id: Option<String>,
        task_status: Option<Status>,
        agent_status: Option<AgentStatus>,
        reason: String,
    },
    /// Would be safe but has uncommitted changes. `--force` promotes to Remove.
    Uncommitted {
        agent_id: String,
        path: PathBuf,
        task_id: Option<String>,
        reason: String,
    },
    /// Blocked by a safety gate — never removed (even with `--force`).
    Skip {
        agent_id: String,
        path: PathBuf,
        reason: String,
    },
}

impl Decision {
    pub fn agent_id(&self) -> &str {
        match self {
            Decision::Remove { agent_id, .. }
            | Decision::Uncommitted { agent_id, .. }
            | Decision::Skip { agent_id, .. } => agent_id,
        }
    }
    pub fn path(&self) -> &Path {
        match self {
            Decision::Remove { path, .. }
            | Decision::Uncommitted { path, .. }
            | Decision::Skip { path, .. } => path,
        }
    }
}

/// Classify every `.wg-worktrees/agent-*` directory against the safety
/// predicate. Returns a deterministic (agent-id sorted) list of decisions.
///
/// Safety predicate (all must hold for Remove):
///   1. Not the currently running agent (`self_agent_id`).
///   2. Registry entry must either be missing (require `--force` at run-time)
///      or its agent must not be live by `AgentEntry::is_live`.
///   3. Task (looked up from registry.task_id) must be terminal OR
///      missing-from-graph.
///   4. Worktree must not contain uncommitted changes (or `--force`).
pub fn plan(
    workgraph_dir: &Path,
    self_agent_id: Option<&str>,
    now_secs: u64,
) -> Result<Vec<Decision>> {
    let project_root = workgraph_dir
        .parent()
        .context("Cannot determine project root from workgraph dir")?
        .to_path_buf();
    let worktrees_dir = project_root.join(".wg-worktrees");
    if !worktrees_dir.exists() {
        return Ok(Vec::new());
    }

    let registry = AgentRegistry::load(workgraph_dir).unwrap_or_default();
    let graph_file = graph_path(workgraph_dir);
    let graph = load_graph(&graph_file).ok();

    let mut decisions: Vec<Decision> = Vec::new();

    for entry in std::fs::read_dir(&worktrees_dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("agent-") {
            continue;
        }
        let path = entry.path();

        // Gate 1: never touch the running agent's own worktree.
        if let Some(self_id) = self_agent_id
            && self_id == name
        {
            decisions.push(Decision::Skip {
                agent_id: name,
                path,
                reason: "this is the currently running agent".to_string(),
            });
            continue;
        }

        // Gate 2 + 3: registry + task state.
        let agent = registry.agents.get(&name);
        if let Some(a) = agent
            && is_live_at(a, now_secs, HEARTBEAT_LIVENESS_TIMEOUT_SECS)
        {
            decisions.push(Decision::Skip {
                agent_id: name,
                path,
                reason: format!(
                    "live agent (status={:?}, pid={}, heartbeat within {}s)",
                    a.status, a.pid, HEARTBEAT_LIVENESS_TIMEOUT_SECS
                ),
            });
            continue;
        }

        let (task_id, task_status) = match agent {
            Some(a) => {
                let status = graph
                    .as_ref()
                    .and_then(|g| g.get_task(&a.task_id))
                    .map(|t| t.status);
                (Some(a.task_id.clone()), status)
            }
            None => (None, None),
        };

        if let Some(status) = task_status
            && !status.is_terminal()
        {
            decisions.push(Decision::Skip {
                agent_id: name,
                path,
                reason: format!(
                    "task '{}' is non-terminal ({})",
                    task_id.as_deref().unwrap_or("?"),
                    status
                ),
            });
            continue;
        }

        // Gate 2b: if the agent is completely missing from the registry,
        // be conservative — flag as Skip (not Uncommitted) because we
        // don't know what's on disk.
        if agent.is_none() {
            decisions.push(Decision::Skip {
                agent_id: name,
                path,
                reason: "no registry entry — conservative skip (use --force to remove)"
                    .to_string(),
            });
            continue;
        }

        // Gate 4: uncommitted changes.
        if has_uncommitted_changes(&path) {
            decisions.push(Decision::Uncommitted {
                agent_id: name.clone(),
                path,
                task_id: task_id.clone(),
                reason: "uncommitted changes present (use --force to discard)".to_string(),
            });
            continue;
        }

        // All gates passed.
        let agent_status = agent.map(|a| a.status);
        let reason = format!(
            "agent_status={} task={} task_status={}",
            agent_status
                .map(|s| format!("{:?}", s).to_lowercase())
                .unwrap_or_else(|| "none".to_string()),
            task_id.as_deref().unwrap_or("missing-from-registry"),
            task_status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "missing-from-graph".to_string()),
        );
        decisions.push(Decision::Remove {
            agent_id: name,
            path,
            task_id,
            task_status,
            agent_status,
            reason,
        });
    }

    decisions.sort_by(|a, b| a.agent_id().cmp(b.agent_id()));
    Ok(decisions)
}

/// Dispatch for `wg gc --worktrees [--apply] [--force]`.
pub fn run(workgraph_dir: &Path, apply: bool, force: bool) -> Result<()> {
    let project_root = workgraph_dir
        .parent()
        .context("Cannot determine project root from workgraph dir")?
        .to_path_buf();
    let worktrees_dir = project_root.join(".wg-worktrees");
    if !worktrees_dir.exists() {
        println!("No worktrees directory found at {}", worktrees_dir.display());
        return Ok(());
    }

    let self_agent = std::env::var("WG_AGENT_ID").ok();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let decisions = plan(workgraph_dir, self_agent.as_deref(), now_secs)?;

    if decisions.is_empty() {
        println!("No worktrees found under {}", worktrees_dir.display());
        return Ok(());
    }

    // Separate into what will be acted on vs. what won't.
    let mut to_remove: Vec<(String, PathBuf, String)> = Vec::new();
    let mut safe_listed: Vec<&Decision> = Vec::new();
    let mut force_listed: Vec<&Decision> = Vec::new();
    let mut uncommitted_skipped: Vec<&Decision> = Vec::new();
    let mut hard_skips: Vec<&Decision> = Vec::new();

    for d in &decisions {
        match d {
            Decision::Remove {
                agent_id,
                path,
                reason,
                ..
            } => {
                safe_listed.push(d);
                to_remove.push((agent_id.clone(), path.clone(), reason.clone()));
            }
            Decision::Uncommitted {
                agent_id,
                path,
                reason,
                ..
            } => {
                if force {
                    force_listed.push(d);
                    to_remove.push((agent_id.clone(), path.clone(), reason.clone()));
                } else {
                    uncommitted_skipped.push(d);
                }
            }
            Decision::Skip { .. } => hard_skips.push(d),
        }
    }

    // Summary header
    println!(
        "Scanned {} worktree(s) under {}:",
        decisions.len(),
        worktrees_dir.display()
    );
    println!("  {} safe to remove", safe_listed.len());
    if force {
        println!(
            "  {} uncommitted → force-remove (--force active, discards work)",
            force_listed.len()
        );
    } else {
        println!(
            "  {} uncommitted (skipped unless --force)",
            uncommitted_skipped.len()
        );
    }
    println!("  {} hard-skipped", hard_skips.len());
    println!();

    // List them
    for d in &safe_listed {
        if let Decision::Remove { agent_id, reason, .. } = d {
            println!("[safe]         {} — {}", agent_id, reason);
        }
    }
    for d in &force_listed {
        if let Decision::Uncommitted { agent_id, reason, .. } = d {
            println!("[force]        {} — {}", agent_id, reason);
        }
    }
    for d in &uncommitted_skipped {
        if let Decision::Uncommitted { agent_id, reason, .. } = d {
            println!("[uncommitted]  {} — {}", agent_id, reason);
        }
    }
    for d in &hard_skips {
        if let Decision::Skip { agent_id, reason, .. } = d {
            println!("[skip]         {} — {}", agent_id, reason);
        }
    }

    if !apply {
        println!();
        println!(
            "Dry-run: {} worktree(s) would be removed. Re-run with --apply to execute.",
            to_remove.len()
        );
        return Ok(());
    }

    println!();
    let mut ok = 0usize;
    let mut failed = 0usize;
    for (agent_id, path, reason) in &to_remove {
        let branch = find_branch_for_agent(&project_root, agent_id)
            .unwrap_or_else(|| format!("wg/{}/unknown", agent_id));
        match crate::commands::spawn::worktree::remove_worktree(&project_root, path, &branch) {
            Ok(()) => {
                ok += 1;
                println!("[removed] {} — {}", agent_id, reason);
            }
            Err(e) => {
                failed += 1;
                eprintln!("[error]   {}: {}", agent_id, e);
            }
        }
    }
    println!();
    println!("Removed {} worktree(s); {} failed.", ok, failed);
    Ok(())
}

/// Replica of `AgentEntry::is_live` that accepts an injected `now` for
/// deterministic testing. Production callers pass `SystemTime::now()`
/// and match the original behavior exactly.
fn is_live_at(agent: &AgentEntry, now_secs: u64, heartbeat_timeout_secs: u64) -> bool {
    if !agent.is_alive() {
        return false;
    }
    if !workgraph::service::is_process_alive(agent.pid) {
        return false;
    }
    let last = match chrono::DateTime::parse_from_rfc3339(&agent.last_heartbeat) {
        Ok(t) => t.timestamp(),
        Err(_) => return false,
    };
    let now_i = now_secs as i64;
    let diff = now_i.saturating_sub(last);
    diff >= 0 && (diff as u64) <= heartbeat_timeout_secs
}

fn has_uncommitted_changes(wt_path: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt_path)
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

fn find_branch_for_agent(project_root: &Path, agent_id: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["branch", "--list", &format!("wg/{}/*", agent_id)])
        .current_dir(project_root)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        let trimmed = line.trim_start_matches(['*', '+', ' ']).trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::service::{AgentEntry, AgentRegistry, AgentStatus};

    /// Build a fixture: project root with `.workgraph/` + `.wg-worktrees/`.
    /// Returns (workgraph_dir, project_root, worktrees_dir).
    fn fixture(tmp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let project_root = tmp.path().to_path_buf();
        let wg_dir = project_root.join(".workgraph");
        let worktrees_dir = project_root.join(".wg-worktrees");
        std::fs::create_dir_all(&wg_dir).unwrap();
        std::fs::create_dir_all(&worktrees_dir).unwrap();
        (wg_dir, project_root, worktrees_dir)
    }

    fn make_worktree_dir(worktrees_dir: &Path, agent_id: &str) -> PathBuf {
        let p = worktrees_dir.join(agent_id);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn agent_entry(
        id: &str,
        task_id: &str,
        status: AgentStatus,
        heartbeat_secs_ago: i64,
    ) -> AgentEntry {
        let now = Utc::now();
        let heartbeat = (now - chrono::Duration::seconds(heartbeat_secs_ago)).to_rfc3339();
        AgentEntry {
            id: id.to_string(),
            // PID 0 is guaranteed to be not-our-running-process on Linux;
            // `is_process_alive(0)` returns false → `is_live_at` returns false.
            pid: 0,
            task_id: task_id.to_string(),
            executor: "claude".to_string(),
            started_at: now.to_rfc3339(),
            last_heartbeat: heartbeat,
            status,
            output_file: String::new(),
            model: None,
            completed_at: None,
        }
    }

    fn write_registry(wg_dir: &Path, entries: Vec<AgentEntry>) {
        let mut reg = AgentRegistry::new();
        for e in entries {
            reg.agents.insert(e.id.clone(), e);
        }
        reg.save(wg_dir).unwrap();
    }

    fn write_graph(wg_dir: &Path, tasks: Vec<Task>) {
        let mut graph = WorkGraph::new();
        for t in tasks {
            graph.add_node(Node::Task(t));
        }
        save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();
    }

    fn make_task(id: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            status,
            ..Task::default()
        }
    }

    // ------------------------------------------------------------------
    // Primary safety tests named per task description.
    // ------------------------------------------------------------------

    #[test]
    fn worktree_gc_skips_in_progress() {
        // A worktree whose owning task is InProgress must NEVER be flagged
        // for removal, no matter how stale the heartbeat looks.
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-1");

        // Heartbeat stale enough to defeat liveness, task status blocks GC.
        write_registry(
            &wg_dir,
            vec![agent_entry("agent-1", "t-open", AgentStatus::Working, 9999)],
        );
        write_graph(&wg_dir, vec![make_task("t-open", Status::InProgress)]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert_eq!(decisions.len(), 1);
        match &decisions[0] {
            Decision::Skip { agent_id, reason, .. } => {
                assert_eq!(agent_id, "agent-1");
                assert!(
                    reason.contains("non-terminal"),
                    "reason should cite non-terminal task, got: {}",
                    reason
                );
            }
            other => panic!("expected Skip for in-progress task, got {:?}", other),
        }
    }

    #[test]
    fn worktree_gc_removes_terminal() {
        // Worktree whose owning agent is terminal (Done) AND owning task is
        // terminal (Done) AND no uncommitted changes → classified Remove.
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-42");

        write_registry(
            &wg_dir,
            vec![agent_entry("agent-42", "t-done", AgentStatus::Done, 9999)],
        );
        write_graph(&wg_dir, vec![make_task("t-done", Status::Done)]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert_eq!(decisions.len(), 1);
        match &decisions[0] {
            Decision::Remove {
                agent_id,
                task_id,
                task_status,
                ..
            } => {
                assert_eq!(agent_id, "agent-42");
                assert_eq!(task_id.as_deref(), Some("t-done"));
                assert_eq!(*task_status, Some(Status::Done));
            }
            other => panic!("expected Remove for terminal task, got {:?}", other),
        }
    }

    // ------------------------------------------------------------------
    // Additional coverage.
    // ------------------------------------------------------------------

    #[test]
    fn worktree_gc_skips_waiting_task() {
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-2");
        write_registry(
            &wg_dir,
            vec![agent_entry("agent-2", "t-wait", AgentStatus::Parked, 9999)],
        );
        write_graph(&wg_dir, vec![make_task("t-wait", Status::Waiting)]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert!(matches!(decisions[0], Decision::Skip { .. }));
    }

    #[test]
    fn worktree_gc_skips_blocked_task() {
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-3");
        write_registry(
            &wg_dir,
            vec![agent_entry("agent-3", "t-blocked", AgentStatus::Idle, 9999)],
        );
        write_graph(&wg_dir, vec![make_task("t-blocked", Status::Blocked)]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert!(matches!(decisions[0], Decision::Skip { .. }));
    }

    #[test]
    fn worktree_gc_skips_pending_validation() {
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-4");
        write_registry(
            &wg_dir,
            vec![agent_entry("agent-4", "t-pv", AgentStatus::Done, 9999)],
        );
        write_graph(&wg_dir, vec![make_task("t-pv", Status::PendingValidation)]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert!(matches!(decisions[0], Decision::Skip { .. }));
    }

    #[test]
    fn worktree_gc_removes_failed_and_abandoned_tasks() {
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-fail");
        make_worktree_dir(&worktrees_dir, "agent-abandon");
        write_registry(
            &wg_dir,
            vec![
                agent_entry("agent-fail", "t-failed", AgentStatus::Failed, 9999),
                agent_entry("agent-abandon", "t-abandon", AgentStatus::Dead, 9999),
            ],
        );
        write_graph(
            &wg_dir,
            vec![
                make_task("t-failed", Status::Failed),
                make_task("t-abandon", Status::Abandoned),
            ],
        );

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert_eq!(decisions.len(), 2);
        for d in &decisions {
            assert!(
                matches!(d, Decision::Remove { .. }),
                "expected Remove, got {:?}",
                d
            );
        }
    }

    #[test]
    fn worktree_gc_skips_self_agent() {
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-self");
        // Even with terminal task, self-agent must not be removed.
        write_registry(
            &wg_dir,
            vec![agent_entry("agent-self", "t-done", AgentStatus::Working, 0)],
        );
        write_graph(&wg_dir, vec![make_task("t-done", Status::Done)]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, Some("agent-self"), now).unwrap();
        assert!(matches!(decisions[0], Decision::Skip { .. }));
        if let Decision::Skip { reason, .. } = &decisions[0] {
            assert!(reason.contains("currently running agent"));
        }
    }

    #[test]
    fn worktree_gc_skips_missing_registry_entry() {
        // Worktree dir exists, but nothing in registry points at agent-X.
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-ghost");
        write_registry(&wg_dir, vec![]);
        write_graph(&wg_dir, vec![]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert_eq!(decisions.len(), 1);
        match &decisions[0] {
            Decision::Skip { reason, .. } => {
                assert!(reason.contains("no registry entry"));
            }
            other => panic!("expected conservative Skip, got {:?}", other),
        }
    }

    #[test]
    fn worktree_gc_removes_when_task_missing_from_graph() {
        // Registry says agent is Done with task-id "t-gone", but the task has
        // already been gc'd from the graph → missing-from-graph is treated
        // as terminal (no data says otherwise).
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        make_worktree_dir(&worktrees_dir, "agent-terminal");
        write_registry(
            &wg_dir,
            vec![agent_entry("agent-terminal", "t-gone", AgentStatus::Done, 9999)],
        );
        write_graph(&wg_dir, vec![]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert_eq!(decisions.len(), 1);
        assert!(matches!(decisions[0], Decision::Remove { .. }));
    }

    #[test]
    fn worktree_gc_ignores_non_agent_directories() {
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, worktrees_dir) = fixture(&tmp);
        std::fs::create_dir_all(worktrees_dir.join("some-random-dir")).unwrap();
        std::fs::write(worktrees_dir.join(".merge-lock"), "").unwrap();
        write_registry(&wg_dir, vec![]);
        write_graph(&wg_dir, vec![]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert!(
            decisions.is_empty(),
            "non-agent-* entries should be ignored, got: {:?}",
            decisions
        );
    }

    #[test]
    fn worktree_gc_empty_worktrees_dir() {
        let tmp = TempDir::new().unwrap();
        let (wg_dir, _proj, _worktrees_dir) = fixture(&tmp);
        write_registry(&wg_dir, vec![]);
        write_graph(&wg_dir, vec![]);

        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert!(decisions.is_empty());
    }

    #[test]
    fn worktree_gc_no_worktrees_dir() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        write_registry(&wg_dir, vec![]);
        write_graph(&wg_dir, vec![]);

        // No .wg-worktrees/ created.
        let now = chrono::Utc::now().timestamp() as u64;
        let decisions = plan(&wg_dir, None, now).unwrap();
        assert!(decisions.is_empty());
    }
}
