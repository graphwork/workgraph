pub mod init;
pub mod add;
pub mod done;
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
pub mod actor;
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
pub mod config_cmd;
pub mod spawn;
pub mod dead_agents;
pub mod agents;
pub mod kill;
pub mod service;

use std::path::Path;

pub fn graph_path(dir: &Path) -> std::path::PathBuf {
    dir.join("graph.jsonl")
}
