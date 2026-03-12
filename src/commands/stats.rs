//! `wg stats` — display time investment statistics for the workgraph.

use anyhow::Result;
use std::path::Path;

use crate::commands::service::{ServiceState, is_service_alive};
use workgraph::AgentRegistry;

/// Format seconds into a compact human-readable duration.
fn fmt_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m > 0 {
            format!("{}h{}m", h, m)
        } else {
            format!("{}h", h)
        }
    }
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let now = chrono::Utc::now();

    // Service uptime
    let service_uptime_secs: Option<u64> =
        ServiceState::load(dir).ok().flatten().and_then(|state| {
            if !is_service_alive(state.pid) {
                return None;
            }
            chrono::DateTime::parse_from_rfc3339(&state.started_at)
                .ok()
                .map(|started| {
                    (now - started.with_timezone(&chrono::Utc))
                        .num_seconds()
                        .max(0) as u64
                })
        });

    // Agent time computation
    let registry = AgentRegistry::load_or_warn(dir);
    let mut cumulative_secs: i64 = 0;
    let mut active_secs: i64 = 0;
    let mut active_count: usize = 0;
    let mut total_agents = 0;

    for agent in registry.agents.values() {
        total_agents += 1;
        let start = chrono::DateTime::parse_from_rfc3339(&agent.started_at)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc));
        let Some(start) = start else { continue };

        if agent.is_alive() {
            let elapsed = (now - start).num_seconds().max(0);
            cumulative_secs += elapsed;
            active_secs += elapsed;
            active_count += 1;
        } else if let Some(ref end_str) = agent.completed_at {
            if let Ok(end) = chrono::DateTime::parse_from_rfc3339(end_str) {
                let elapsed = (end.with_timezone(&chrono::Utc) - start)
                    .num_seconds()
                    .max(0);
                cumulative_secs += elapsed;
            }
        } else if let Ok(hb) = chrono::DateTime::parse_from_rfc3339(&agent.last_heartbeat) {
            let elapsed = (hb.with_timezone(&chrono::Utc) - start)
                .num_seconds()
                .max(0);
            cumulative_secs += elapsed;
        }
    }

    let cumulative_secs = cumulative_secs as u64;
    let active_secs = active_secs as u64;

    if json {
        let output = serde_json::json!({
            "service_uptime_secs": service_uptime_secs,
            "cumulative_agent_secs": cumulative_secs,
            "active_agent_secs": active_secs,
            "active_agent_count": active_count,
            "total_agents": total_agents,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Workgraph Time Statistics");
        println!("=========================");
        println!();

        // Service uptime
        match service_uptime_secs {
            Some(secs) => println!("  \u{2191} Service uptime:     {}", fmt_duration(secs)),
            None => println!("  \u{2191} Service uptime:     (not running)"),
        }

        // Cumulative walltime
        println!(
            "  \u{03A3} Cumulative walltime: {} ({} agents total)",
            fmt_duration(cumulative_secs),
            total_agents,
        );

        // Active agent time
        if active_count > 0 {
            println!(
                "  \u{26A1} Active agent time:   {} ({} agents)",
                fmt_duration(active_secs),
                active_count,
            );
        } else {
            println!("  \u{26A1} Active agent time:   (no active agents)");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_duration() {
        assert_eq!(fmt_duration(0), "0s");
        assert_eq!(fmt_duration(45), "45s");
        assert_eq!(fmt_duration(90), "1m30s");
        assert_eq!(fmt_duration(3600), "1h");
        assert_eq!(fmt_duration(3661), "1h1m");
        assert_eq!(fmt_duration(7200), "2h");
        assert_eq!(fmt_duration(86400), "24h");
    }

    #[test]
    fn test_run_empty_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let wg_dir = temp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        // Should not error even with no data
        let result = run(&wg_dir, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_json() {
        let temp = tempfile::TempDir::new().unwrap();
        let wg_dir = temp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let result = run(&wg_dir, true);
        assert!(result.is_ok());
    }
}
