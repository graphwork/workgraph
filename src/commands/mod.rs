pub mod abandon;
pub mod add;
pub mod agency_stats;
pub mod agent;
pub mod agent_crud;
pub mod agents;
pub mod aging;
pub mod analyze;
pub mod approve;
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
pub mod reject;
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
pub mod submit;
pub mod trajectory;
pub mod velocity;
pub mod viz;
pub mod why_blocked;
pub mod workload;

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
