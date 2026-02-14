pub mod abandon;
pub mod add;
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
pub mod plan;
pub mod quickstart;
pub mod ready;
pub mod reclaim;
pub mod reschedule;
pub mod resource;
pub mod resources;
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
use std::path::Path;

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

/// Best-effort notification to the service daemon that the graph has changed.
/// Silently ignores all errors (daemon not running, socket unavailable, etc.)
pub fn notify_graph_changed(dir: &Path) {
    let _ = service::send_request(dir, service::IpcRequest::GraphChanged);
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
