//! Shared helpers for claim-lifecycle plumbing — reused by `wg reset`,
//! `wg retry`, and the dispatcher's lazy reconciler in `sweep`.
//!
//! This module exists because two user-initiated commands (`reset` and
//! `retry`) and one background loop (`reconcile_orphaned_tasks`) all need
//! to walk a closure of related tasks and clear stale `assigned` claims.
//! Without a shared home, the closure walker would either be duplicated
//! across files or pulled in via a `commands::reset -> commands::retry`
//! cross-import that obscures intent.
//!
//! See `design-claim-lifecycle.md` for the rationale ("Both: Eager +
//! Lazy with status-aware reconciler").

use std::collections::HashSet;
use std::path::Path;

use chrono::Utc;

use workgraph::graph::{LogEntry, Status, WorkGraph};
use workgraph::service::registry::{AgentRegistry, AgentStatus};

use super::is_process_alive;

/// Edge direction used when computing the closure.
///
/// Mirrors `commands::reset::Direction` exactly — but kept separate so
/// future callers (e.g. an unclaim-cone command) can use it without
/// importing `reset` for an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow `task.before` edges — everything the seeds block.
    Forward,
    /// Follow `task.after` edges — everything the seeds depend on.
    Backward,
    /// Union of both.
    Both,
}

/// Compute the closure of tasks reachable from `seeds` via the given
/// direction. Seeds are always included (even if they have no edges).
/// System (dot-prefixed) tasks encountered during traversal are NOT
/// added to the closure — they're handled separately by callers that
/// care (e.g. reset's meta-strip path, or retry's eager walk that
/// explicitly skips them).
///
/// Cycle-safe: `visited` guarantees we never revisit, so a back-edge
/// in the graph cannot cause infinite recursion.
pub fn compute_closure(graph: &WorkGraph, seeds: &[String], direction: Direction) -> HashSet<String> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = seeds
        .iter()
        .filter(|s| !workgraph::graph::is_system_task(s))
        .cloned()
        .collect();

    while let Some(id) = stack.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        let task = match graph.get_task(&id) {
            Some(t) => t,
            None => continue,
        };
        let next_ids: Vec<String> = match direction {
            Direction::Forward => task.before.clone(),
            Direction::Backward => task.after.clone(),
            Direction::Both => {
                let mut c = task.before.clone();
                c.extend(task.after.iter().cloned());
                c
            }
        };
        for nid in next_ids {
            if !visited.contains(&nid) && !workgraph::graph::is_system_task(&nid) {
                stack.push(nid);
            }
        }
    }
    visited
}

/// Decide whether a claim referencing `agent_id` is stale enough to
/// clear. Conservative: only true when we have strong evidence the
/// agent is gone (Dead in registry, OR alive-marked but PID is
/// unreachable, OR absent from the registry entirely). Live agents
/// are left alone — the lazy reconciler will pick them up later if/when
/// they actually die.
pub fn is_claim_stale(registry: &AgentRegistry, agent_id: &str) -> bool {
    match registry.get_agent(agent_id) {
        Some(agent) => {
            agent.status == AgentStatus::Dead
                || (agent.is_alive() && !is_process_alive(agent.pid))
        }
        None => true,
    }
}

/// Result of clearing stale claims from a downstream cone.
#[derive(Debug, Default)]
pub struct ClearedDownstream {
    /// Task ids that had their claim cleared.
    pub cleared: Vec<String>,
}

/// Walk the downstream closure of `seed` and clear `assigned` /
/// `started_at` on any non-terminal task whose claim references a stale
/// (Dead-or-absent-or-unreachable) agent. Used by `wg retry` to
/// propagate the user's "this work needs to redo" intent through the
/// fan-out of downstream tasks that were already-claimed by a now-dead
/// agent.
///
/// Notes:
/// - `seed` itself is excluded; the caller (retry) already cleared its
///   own claim through its primary path. The walk starts from `seed`'s
///   forward neighbours.
/// - System (dot-prefixed) tasks are skipped: those re-generate via the
///   agency pipeline and shouldn't have their claim mutated mid-flight.
/// - Terminal-state tasks (Done, Failed, Abandoned) are skipped: they
///   no longer need a claim, and clearing them would muddy the audit
///   log without changing dispatch behaviour.
/// - Live-agent claims are left alone — see `is_claim_stale`.
///
/// Records a log entry on each cleared task naming `seed_for_log` as
/// the cause (e.g. `"retry of upstream"`).
pub fn clear_stale_downstream_claims(
    graph: &mut WorkGraph,
    registry: &AgentRegistry,
    seed: &str,
    seed_for_log: &str,
) -> ClearedDownstream {
    let now = Utc::now().to_rfc3339();
    let user = workgraph::current_user();

    // Compute the forward closure starting from seed's neighbours.
    // We use `seed` as the closure entry point but exclude it from the
    // mutation step — the retry's primary path already cleared it.
    let closure = compute_closure(graph, &[seed.to_string()], Direction::Forward);

    let mut report = ClearedDownstream::default();

    for tid in &closure {
        if tid == seed {
            continue;
        }
        // Snapshot the bits we need before mutating, to avoid double
        // borrow against the registry.
        let (assigned, status) = {
            let task = match graph.get_task(tid) {
                Some(t) => t,
                None => continue,
            };
            (task.assigned.clone(), task.status)
        };

        // Skip terminal tasks — claim no longer matters there.
        if matches!(status, Status::Done | Status::Failed | Status::Abandoned) {
            continue;
        }

        let agent_id = match assigned {
            Some(a) => a,
            None => continue,
        };

        if !is_claim_stale(registry, &agent_id) {
            continue;
        }

        if let Some(task) = graph.get_task_mut(tid) {
            task.assigned = None;
            task.started_at = None;
            // If the task was somehow InProgress under a dead agent,
            // bring it back to Open so the dispatcher will pick it up.
            if task.status == Status::InProgress {
                task.status = Status::Open;
            }
            task.log.push(LogEntry {
                timestamp: now.clone(),
                actor: Some("claim-lifecycle".to_string()),
                user: Some(user.clone()),
                message: format!(
                    "stale-claim cleared via retry of {} (was assigned to @{} — agent dead/missing)",
                    seed_for_log, agent_id
                ),
            });
            report.cleared.push(tid.clone());
        }
    }

    report.cleared.sort();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};
    use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};

    fn make_task(id: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            status,
            ..Task::default()
        }
    }

    fn save(dir: &std::path::Path, tasks: Vec<Task>) -> PathBuf {
        let path = dir.join("graph.jsonl");
        let mut g = WorkGraph::new();
        for t in tasks {
            g.add_node(Node::Task(t));
        }
        save_graph(&g, &path).unwrap();
        path
    }

    fn dead_agent_registry(dir: &std::path::Path, agent_id: &str) -> AgentRegistry {
        let mut reg = AgentRegistry::new();
        reg.agents.insert(
            agent_id.to_string(),
            AgentEntry {
                id: agent_id.to_string(),
                pid: 99999,
                task_id: "irrelevant".to_string(),
                executor: "claude".to_string(),
                status: AgentStatus::Dead,
                started_at: Utc::now().to_rfc3339(),
                last_heartbeat: "2020-01-01T00:00:00Z".to_string(),
                completed_at: Some(Utc::now().to_rfc3339()),
                output_file: "/tmp/output.log".to_string(),
                model: None,
                worktree_path: None,
            },
        );
        reg.save(dir).unwrap();
        reg
    }

    #[test]
    fn closure_forward_simple_chain() {
        let dir = tempdir().unwrap();
        let mut a = make_task("a", Status::Failed);
        let mut b = make_task("b", Status::Open);
        let mut c = make_task("c", Status::Open);
        a.before = vec!["b".into()];
        b.after = vec!["a".into()];
        b.before = vec!["c".into()];
        c.after = vec!["b".into()];
        save(dir.path(), vec![a, b, c]);
        let g = load_graph(&dir.path().join("graph.jsonl")).unwrap();
        let closure = compute_closure(&g, &["a".to_string()], Direction::Forward);
        let mut ids: Vec<String> = closure.into_iter().collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn closure_skips_system_tasks() {
        let dir = tempdir().unwrap();
        let mut a = make_task("a", Status::Failed);
        let mut sys = make_task(".flip-a", Status::Open);
        a.before = vec![".flip-a".into()];
        sys.after = vec!["a".into()];
        save(dir.path(), vec![a, sys]);
        let g = load_graph(&dir.path().join("graph.jsonl")).unwrap();
        let closure = compute_closure(&g, &["a".to_string()], Direction::Forward);
        assert!(!closure.contains(".flip-a"));
        assert!(closure.contains("a"));
    }

    #[test]
    fn is_claim_stale_returns_true_for_dead() {
        let dir = tempdir().unwrap();
        let reg = dead_agent_registry(dir.path(), "agent-dead-1");
        assert!(is_claim_stale(&reg, "agent-dead-1"));
    }

    #[test]
    fn is_claim_stale_returns_true_for_missing() {
        let dir = tempdir().unwrap();
        let reg = AgentRegistry::new();
        let _ = reg.save(dir.path());
        assert!(is_claim_stale(&reg, "agent-never-existed"));
    }

    #[test]
    fn clear_stale_downstream_clears_dead_claim_in_open_task() {
        // Repro: upstream Failed → downstream Open with dead-agent claim.
        // After clear_stale_downstream_claims, downstream must be unclaimed.
        let dir = tempdir().unwrap();
        let mut up = make_task("upstream", Status::Failed);
        let mut down = make_task("downstream", Status::Open);
        up.before = vec!["downstream".into()];
        down.after = vec!["upstream".into()];
        down.assigned = Some("agent-dead-1".to_string());
        down.started_at = Some("2026-01-01T00:00:00Z".to_string());
        save(dir.path(), vec![up, down]);
        let reg = dead_agent_registry(dir.path(), "agent-dead-1");

        let mut g = load_graph(&dir.path().join("graph.jsonl")).unwrap();
        let report = clear_stale_downstream_claims(&mut g, &reg, "upstream", "upstream");

        assert_eq!(report.cleared, vec!["downstream".to_string()]);
        let down = g.get_task("downstream").unwrap();
        assert!(down.assigned.is_none(), "downstream claim must be cleared");
        assert!(down.started_at.is_none(), "downstream started_at must be cleared");
        assert!(
            down.log.iter().any(|e| e.message.contains("stale-claim cleared via retry of upstream")),
            "log entry should describe the cause: {:?}",
            down.log
        );
    }

    #[test]
    fn clear_stale_downstream_skips_live_agent_claim() {
        // Defensive: a downstream task claimed by a still-alive agent
        // must NOT have its claim cleared. The lazy reconciler is the
        // only path that touches a claim with no user-initiated trigger.
        let dir = tempdir().unwrap();
        let mut up = make_task("upstream", Status::Failed);
        let mut down = make_task("downstream", Status::Open);
        up.before = vec!["downstream".into()];
        down.after = vec!["upstream".into()];
        down.assigned = Some("agent-alive-1".to_string());
        save(dir.path(), vec![up, down]);

        // Registry: agent is Alive with the current process's PID (always alive).
        let mut reg = AgentRegistry::new();
        reg.agents.insert(
            "agent-alive-1".to_string(),
            AgentEntry {
                id: "agent-alive-1".to_string(),
                pid: std::process::id(),
                task_id: "downstream".to_string(),
                executor: "claude".to_string(),
                status: AgentStatus::Working,
                started_at: Utc::now().to_rfc3339(),
                last_heartbeat: Utc::now().to_rfc3339(),
                completed_at: None,
                output_file: "/tmp/output.log".to_string(),
                model: None,
                worktree_path: None,
            },
        );
        reg.save(dir.path()).unwrap();

        let mut g = load_graph(&dir.path().join("graph.jsonl")).unwrap();
        let report = clear_stale_downstream_claims(&mut g, &reg, "upstream", "upstream");
        assert!(report.cleared.is_empty(), "live-agent claim must be preserved");
        assert_eq!(g.get_task("downstream").unwrap().assigned, Some("agent-alive-1".to_string()));
    }

    #[test]
    fn clear_stale_downstream_skips_terminal_tasks() {
        // A Done downstream with a stale claim is irrelevant — leave it.
        let dir = tempdir().unwrap();
        let mut up = make_task("upstream", Status::Failed);
        let mut down = make_task("downstream", Status::Done);
        up.before = vec!["downstream".into()];
        down.after = vec!["upstream".into()];
        down.assigned = Some("agent-dead-1".to_string());
        save(dir.path(), vec![up, down]);
        let reg = dead_agent_registry(dir.path(), "agent-dead-1");

        let mut g = load_graph(&dir.path().join("graph.jsonl")).unwrap();
        let report = clear_stale_downstream_claims(&mut g, &reg, "upstream", "upstream");
        assert!(report.cleared.is_empty());
        assert_eq!(g.get_task("downstream").unwrap().assigned, Some("agent-dead-1".to_string()));
    }

    #[test]
    fn clear_stale_downstream_handles_transitive_chain() {
        // upstream → mid → tail; mid + tail both have dead-agent claims.
        // Both should be cleared.
        let dir = tempdir().unwrap();
        let mut up = make_task("upstream", Status::Failed);
        let mut mid = make_task("mid", Status::Open);
        let mut tail = make_task("tail", Status::Open);
        up.before = vec!["mid".into()];
        mid.after = vec!["upstream".into()];
        mid.before = vec!["tail".into()];
        tail.after = vec!["mid".into()];
        mid.assigned = Some("agent-dead-1".to_string());
        tail.assigned = Some("agent-dead-1".to_string());
        save(dir.path(), vec![up, mid, tail]);
        let reg = dead_agent_registry(dir.path(), "agent-dead-1");

        let mut g = load_graph(&dir.path().join("graph.jsonl")).unwrap();
        let report = clear_stale_downstream_claims(&mut g, &reg, "upstream", "upstream");
        assert_eq!(report.cleared, vec!["mid".to_string(), "tail".to_string()]);
    }

    #[test]
    fn clear_stale_downstream_resets_inprogress_to_open() {
        // Edge case: a downstream task that was somehow InProgress under a
        // dead agent should be reset to Open, not just unclaimed (otherwise
        // the dispatcher's ready-check would still skip it).
        let dir = tempdir().unwrap();
        let mut up = make_task("upstream", Status::Failed);
        let mut down = make_task("downstream", Status::InProgress);
        up.before = vec!["downstream".into()];
        down.after = vec!["upstream".into()];
        down.assigned = Some("agent-dead-1".to_string());
        save(dir.path(), vec![up, down]);
        let reg = dead_agent_registry(dir.path(), "agent-dead-1");

        let mut g = load_graph(&dir.path().join("graph.jsonl")).unwrap();
        let _ = clear_stale_downstream_claims(&mut g, &reg, "upstream", "upstream");
        let down = g.get_task("downstream").unwrap();
        assert_eq!(down.status, Status::Open);
        assert!(down.assigned.is_none());
    }
}
