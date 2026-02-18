pub mod abandon;
pub mod add;
pub mod agency_init;
pub mod agency_stats;
pub mod agent;
pub mod agent_crud;
pub mod agents;
pub mod aging;
pub mod analyze;
pub mod archive;
pub mod artifact;
pub mod assign;
pub mod blocked;
pub mod bottlenecks;
pub mod check;
pub mod claim;
pub mod config_cmd;
pub mod context;
pub mod coordinate;
pub mod cost;
pub mod critical_path;
pub mod dead_agents;
pub mod done;
pub mod edit;
pub mod evaluate;
pub mod evolve;
pub mod exec;
pub mod fail;
pub mod forecast;
pub mod gc;
pub mod graph;
pub mod heartbeat;
pub mod impact;
pub mod init;
pub mod kill;
pub mod list;
pub mod log;
pub mod loops;
pub mod match_cmd;
#[cfg(any(feature = "matrix", feature = "matrix-lite"))]
pub mod matrix;
pub mod motivation;
pub mod next;
#[cfg(any(feature = "matrix", feature = "matrix-lite"))]
pub mod notify;
pub mod pause;
pub mod plan;
pub mod quickstart;
pub mod ready;
pub mod reclaim;
pub mod reschedule;
pub mod resource;
pub mod resources;
pub mod resume;
pub mod retry;
pub mod role;
pub mod service;
pub mod show;
pub mod skills;
pub mod spawn;
pub mod status;
pub mod structure;
pub mod trajectory;
pub mod velocity;
pub mod viz;
pub mod why_blocked;
pub mod workload;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use workgraph::parser::load_graph;

/// Load the workgraph (immutable) from the given directory.
/// Returns the graph and the path to the graph file (needed for save_graph).
pub fn load_workgraph(dir: &Path) -> Result<(workgraph::graph::WorkGraph, PathBuf)> {
    let path = graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }
    let graph = load_graph(&path).context("Failed to load graph")?;
    Ok((graph, path))
}

/// Load the workgraph (mutable) from the given directory.
/// Returns the graph and the path to the graph file (needed for save_graph).
pub fn load_workgraph_mut(dir: &Path) -> Result<(workgraph::graph::WorkGraph, PathBuf)> {
    load_workgraph(dir)
}

/// Check if a process with the given PID is alive.
///
/// Uses `kill(pid, 0)` on Unix to probe without sending a signal.
/// On non-Unix platforms, conservatively assumes the process is alive.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    true
}

pub fn graph_path(dir: &Path) -> std::path::PathBuf {
    dir.join("graph.jsonl")
}

/// Collect all transitive dependents of a task using a reverse dependency index.
///
/// Given a `reverse_index` mapping task IDs to their direct dependents,
/// recursively collects all tasks that transitively depend on `task_id`.
/// Results are accumulated in `visited` (which also prevents cycles).
pub fn collect_transitive_dependents(
    reverse_index: &HashMap<String, Vec<String>>,
    task_id: &str,
    visited: &mut HashSet<String>,
) {
    if let Some(dependents) = reverse_index.get(task_id) {
        for dep_id in dependents {
            if visited.insert(dep_id.clone()) {
                collect_transitive_dependents(reverse_index, dep_id, visited);
            }
        }
    }
}

/// Best-effort notification to the service daemon that the graph has changed.
/// Silently ignores all errors (daemon not running, socket unavailable, etc.)
pub fn notify_graph_changed(dir: &Path) {
    let _ = service::send_request(dir, &service::IpcRequest::GraphChanged);
}

/// Check service status and print a hint for the user/agent.
/// Returns true if the service is running.
pub fn print_service_hint(dir: &Path) -> bool {
    match service::ServiceState::load(dir) {
        Ok(Some(state)) if service::is_service_alive(state.pid) => {
            if service::is_service_paused(dir) {
                eprintln!(
                    "Service: running (paused). New tasks won't be dispatched until resumed. Use `wg service resume`."
                );
            } else {
                eprintln!("Service: running. The coordinator will dispatch this automatically.");
            }
            true
        }
        _ => {
            eprintln!("Warning: No service running. Tasks won't be dispatched automatically.");
            eprintln!("  Start the coordinator with: wg service start");
            false
        }
    }
}

#[cfg(test)]
mod provenance_coverage_tests {
    use std::path::Path;
    use tempfile::TempDir;
    use workgraph::graph::WorkGraph;
    use workgraph::parser::save_graph;
    use workgraph::provenance::read_all_operations;

    fn setup_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();
        let graph_path = dir.join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &graph_path).unwrap();
        tmp
    }

    fn ops_with_type(dir: &Path, op: &str) -> Vec<workgraph::provenance::OperationEntry> {
        read_all_operations(dir)
            .unwrap()
            .into_iter()
            .filter(|e| e.op == op)
            .collect()
    }

    #[test]
    fn provenance_add_records_entry() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Test task", Some("prov-add"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        let entries = ops_with_type(dir, "add_task");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task_id.as_deref(), Some("prov-add"));
    }

    #[test]
    fn provenance_edit_records_field_changes() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Edit target", Some("prov-edit"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        super::edit::run(
            dir, "prov-edit", Some("New Title"), None,
            &[], &[], &[], &[], None, &[], &[], None, None, None, None, None, None,
        ).unwrap();

        let entries = ops_with_type(dir, "edit");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task_id.as_deref(), Some("prov-edit"));
        let fields = entries[0].detail.get("fields").unwrap().as_array().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0]["field"], "title");
    }

    #[test]
    fn provenance_claim_unclaim_records_entries() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Claim target", Some("prov-claim"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        super::claim::claim(dir, "prov-claim", Some("agent-1")).unwrap();
        let entries = ops_with_type(dir, "claim");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor.as_deref(), Some("agent-1"));
        assert_eq!(entries[0].detail["prev_status"], "Open");

        super::claim::unclaim(dir, "prov-claim").unwrap();
        let entries = ops_with_type(dir, "unclaim");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].detail["prev_assigned"], "agent-1");
    }

    #[test]
    fn provenance_done_records_entry() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Done target", Some("prov-done"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        super::done::run(dir, "prov-done").unwrap();
        let entries = ops_with_type(dir, "done");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task_id.as_deref(), Some("prov-done"));
    }

    #[test]
    fn provenance_fail_records_entry() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Fail target", Some("prov-fail"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        super::fail::run(dir, "prov-fail", Some("timeout")).unwrap();
        let entries = ops_with_type(dir, "fail");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].detail["reason"], "timeout");
    }

    #[test]
    fn provenance_abandon_records_entry() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Abandon target", Some("prov-abandon"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        super::abandon::run(dir, "prov-abandon", Some("no longer needed")).unwrap();
        let entries = ops_with_type(dir, "abandon");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].detail["reason"], "no longer needed");
    }

    #[test]
    fn provenance_retry_records_entry() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Retry target", Some("prov-retry"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        super::fail::run(dir, "prov-retry", Some("compile error")).unwrap();
        super::retry::run(dir, "prov-retry").unwrap();

        let entries = ops_with_type(dir, "retry");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].detail["attempt"], 2); // retry_count was 1 after fail, so attempt = 2
        assert_eq!(entries[0].detail["prev_failure_reason"], "compile error");
    }

    #[test]
    fn provenance_pause_resume_records_entries() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Pause target", Some("prov-pause"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        super::pause::run(dir, "prov-pause").unwrap();
        let entries = ops_with_type(dir, "pause");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task_id.as_deref(), Some("prov-pause"));

        super::resume::run(dir, "prov-pause").unwrap();
        let entries = ops_with_type(dir, "resume");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task_id.as_deref(), Some("prov-pause"));
    }

    #[test]
    fn provenance_artifact_add_remove_records_entries() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Artifact target", Some("prov-art"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();

        super::artifact::run_add(dir, "prov-art", "output.txt").unwrap();
        let entries = ops_with_type(dir, "artifact_add");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].detail["path"], "output.txt");

        super::artifact::run_remove(dir, "prov-art", "output.txt").unwrap();
        let entries = ops_with_type(dir, "artifact_rm");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].detail["path"], "output.txt");
    }

    #[test]
    fn provenance_archive_records_entry() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "Archive target", Some("prov-archive"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();
        super::done::run(dir, "prov-archive").unwrap();

        super::archive::run(dir, false, None, false, false).unwrap();
        let entries = ops_with_type(dir, "archive");
        assert_eq!(entries.len(), 1);
        let task_ids = entries[0].detail["task_ids"].as_array().unwrap();
        assert!(task_ids.iter().any(|id| id == "prov-archive"));
    }

    #[test]
    fn provenance_gc_records_entry() {
        let tmp = setup_dir();
        let dir = tmp.path();
        super::add::run(
            dir, "GC target", Some("prov-gc"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();
        super::fail::run(dir, "prov-gc", Some("oops")).unwrap();
        super::abandon::run(dir, "prov-gc", Some("giving up")).unwrap();

        super::gc::run(dir, false, false).unwrap();
        let entries = ops_with_type(dir, "gc");
        assert_eq!(entries.len(), 1);
        let removed = entries[0].detail["removed"].as_array().unwrap();
        assert!(removed.iter().any(|r| r["id"] == "prov-gc"));
    }

    #[test]
    fn provenance_full_lifecycle_all_ops_recorded() {
        let tmp = setup_dir();
        let dir = tmp.path();

        // add
        super::add::run(
            dir, "Lifecycle task", Some("lifecycle"), None,
            &[], None, None, None, &[], &[], &[], &[], None, None, None, None, None, None, None,
        ).unwrap();
        // edit
        super::edit::run(
            dir, "lifecycle", Some("Renamed"), None,
            &[], &[], &["tag1".to_string()], &[], None, &[], &[], None, None, None, None, None, None,
        ).unwrap();
        // pause
        super::pause::run(dir, "lifecycle").unwrap();
        // resume
        super::resume::run(dir, "lifecycle").unwrap();
        // claim
        super::claim::claim(dir, "lifecycle", Some("worker")).unwrap();
        // artifact add
        super::artifact::run_add(dir, "lifecycle", "result.txt").unwrap();
        // unclaim
        super::claim::unclaim(dir, "lifecycle").unwrap();
        // fail
        super::fail::run(dir, "lifecycle", Some("timeout")).unwrap();
        // retry
        super::retry::run(dir, "lifecycle").unwrap();
        // done
        super::done::run(dir, "lifecycle").unwrap();

        let all = read_all_operations(dir).unwrap();
        let ops: Vec<&str> = all.iter().map(|e| e.op.as_str()).collect();

        assert!(ops.contains(&"add_task"), "missing add_task, got: {:?}", ops);
        assert!(ops.contains(&"edit"), "missing edit, got: {:?}", ops);
        assert!(ops.contains(&"pause"), "missing pause, got: {:?}", ops);
        assert!(ops.contains(&"resume"), "missing resume, got: {:?}", ops);
        assert!(ops.contains(&"claim"), "missing claim, got: {:?}", ops);
        assert!(ops.contains(&"artifact_add"), "missing artifact_add, got: {:?}", ops);
        assert!(ops.contains(&"unclaim"), "missing unclaim, got: {:?}", ops);
        assert!(ops.contains(&"fail"), "missing fail, got: {:?}", ops);
        assert!(ops.contains(&"retry"), "missing retry, got: {:?}", ops);
        assert!(ops.contains(&"done"), "missing done, got: {:?}", ops);

        // All entries should have task_id = "lifecycle"
        for entry in &all {
            if let Some(ref tid) = entry.task_id {
                assert_eq!(tid, "lifecycle");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_transitive_dependents_empty_index() {
        let index = HashMap::new();
        let mut visited = HashSet::new();
        collect_transitive_dependents(&index, "a", &mut visited);
        assert!(visited.is_empty());
    }

    #[test]
    fn collect_transitive_dependents_direct() {
        let mut index = HashMap::new();
        index.insert("a".into(), vec!["b".into(), "c".into()]);
        let mut visited = HashSet::new();
        collect_transitive_dependents(&index, "a", &mut visited);
        assert_eq!(visited.len(), 2);
        assert!(visited.contains("b"));
        assert!(visited.contains("c"));
    }

    #[test]
    fn collect_transitive_dependents_chain() {
        let mut index = HashMap::new();
        index.insert("a".into(), vec!["b".into()]);
        index.insert("b".into(), vec!["c".into()]);
        index.insert("c".into(), vec!["d".into()]);
        let mut visited = HashSet::new();
        collect_transitive_dependents(&index, "a", &mut visited);
        assert_eq!(visited.len(), 3);
        assert!(visited.contains("b"));
        assert!(visited.contains("c"));
        assert!(visited.contains("d"));
    }

    #[test]
    fn collect_transitive_dependents_handles_cycles() {
        let mut index = HashMap::new();
        index.insert("a".into(), vec!["b".into()]);
        index.insert("b".into(), vec!["a".into()]); // cycle
        let mut visited = HashSet::new();
        collect_transitive_dependents(&index, "a", &mut visited);
        assert_eq!(visited.len(), 2);
        assert!(visited.contains("a"));
        assert!(visited.contains("b"));
    }

    #[test]
    fn collect_transitive_dependents_diamond() {
        let mut index = HashMap::new();
        index.insert("a".into(), vec!["b".into(), "c".into()]);
        index.insert("b".into(), vec!["d".into()]);
        index.insert("c".into(), vec!["d".into()]);
        let mut visited = HashSet::new();
        collect_transitive_dependents(&index, "a", &mut visited);
        assert_eq!(visited.len(), 3);
        assert!(visited.contains("b"));
        assert!(visited.contains("c"));
        assert!(visited.contains("d"));
    }

    #[test]
    fn graph_path_joins_correctly() {
        let dir = Path::new("/tmp/test-wg");
        assert_eq!(
            graph_path(dir),
            std::path::PathBuf::from("/tmp/test-wg/graph.jsonl")
        );
    }
}
