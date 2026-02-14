pub mod init;
pub mod add;
pub mod done;
pub mod submit;
pub mod approve;
pub mod reject;
pub mod fail;
pub mod abandon;
pub mod retry;
pub mod claim;
pub mod reclaim;
pub mod ready;
pub mod blocked;
pub mod check;
pub mod list;
pub mod graph;
pub mod cost;
pub mod resource;
pub mod coordinate;
pub mod plan;
pub mod reschedule;
pub mod impact;
pub mod loops;
pub mod structure;
pub mod why_blocked;
pub mod bottlenecks;
pub mod velocity;
pub mod aging;
pub mod forecast;
pub mod workload;
pub mod resources;
pub mod critical_path;
pub mod analyze;
pub mod archive;
pub mod log;
pub mod show;
pub mod viz;
pub mod skills;
pub mod match_cmd;
pub mod heartbeat;
pub mod artifact;
pub mod context;
pub mod next;
pub mod trajectory;
pub mod exec;
pub mod agent;
pub mod agent_crud;
pub mod config_cmd;
pub mod spawn;
pub mod evaluate;
pub mod agency_stats;
pub mod dead_agents;
pub mod agents;
pub mod kill;
pub mod service;
pub mod quickstart;
pub mod status;
pub mod role;
pub mod motivation;
pub mod assign;
pub mod evolve;
pub mod edit;
#[cfg(any(feature = "matrix", feature = "matrix-lite"))]
pub mod notify;
#[cfg(any(feature = "matrix", feature = "matrix-lite"))]
pub mod matrix;

use std::path::Path;

pub fn graph_path(dir: &Path) -> std::path::PathBuf {
    dir.join("graph.jsonl")
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
                eprintln!("Service: running (paused). New tasks won't be dispatched until resumed. Use `wg service resume`.");
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
