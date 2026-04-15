//! Reap dead/done/failed agents from the registry
//!
//! Garbage-collects agent entries that are no longer alive, freeing up
//! the registry without affecting task logs or history.
//!
//! Usage:
//!   wg reap                    # Remove all dead/done/failed agents
//!   wg reap --dry-run          # Show what would be reaped
//!   wg reap --older-than 1h    # Only reap agents dead for longer than 1h

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use std::path::Path;
use workgraph::service::{AgentEntry, AgentRegistry, AgentStatus};

/// Statuses considered reapable
fn is_reapable(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Dead | AgentStatus::Done | AgentStatus::Failed
    )
}

/// Parse a duration string like "1h", "30m", "7d", "2w" into a chrono Duration
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("Empty duration string");
    }

    let (num_str, unit) = if let Some(n) = s.strip_suffix('m') {
        (n, 'm')
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 'h')
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 'd')
    } else if let Some(n) = s.strip_suffix('w') {
        (n, 'w')
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 's')
    } else {
        // Default to seconds if no unit
        (s, 's')
    };

    let num: i64 = num_str
        .parse()
        .with_context(|| format!("Invalid number in duration: '{}'", num_str))?;

    match unit {
        's' => Ok(Duration::seconds(num)),
        'm' => Ok(Duration::minutes(num)),
        'h' => Ok(Duration::hours(num)),
        'd' => Ok(Duration::days(num)),
        'w' => Ok(Duration::weeks(num)),
        _ => anyhow::bail!("Unknown duration unit: {}", unit),
    }
}

/// Check if an agent has been in its terminal state long enough to be reaped.
fn is_old_enough(agent: &AgentEntry, min_age: &Duration) -> bool {
    // Use completed_at if available, otherwise fall back to last_heartbeat
    let timestamp = agent
        .completed_at
        .as_deref()
        .unwrap_or(&agent.last_heartbeat);

    if let Ok(parsed) = DateTime::parse_from_rfc3339(timestamp) {
        let age = Utc::now() - parsed.with_timezone(&Utc);
        age >= *min_age
    } else {
        // Can't parse timestamp — be conservative, include it
        true
    }
}

/// Collect agents eligible for reaping
fn collect_reapable(registry: &AgentRegistry, older_than: Option<&Duration>) -> Vec<AgentEntry> {
    registry
        .agents
        .values()
        .filter(|a| {
            is_reapable(a.status) && older_than.map(|d| is_old_enough(a, d)).unwrap_or(true)
        })
        .cloned()
        .collect()
}

/// Run the reap command
pub fn run(dir: &Path, dry_run: bool, older_than: Option<&str>, json: bool) -> Result<()> {
    let older_than_duration = older_than
        .map(parse_duration)
        .transpose()
        .context("Invalid --older-than value")?;

    if dry_run {
        // Read-only path: no lock needed
        let registry = AgentRegistry::load(dir)?;
        let reapable = collect_reapable(&registry, older_than_duration.as_ref());
        print_results(&reapable, dry_run, json);
    } else {
        // Acquire lock for mutation
        let mut locked_registry = AgentRegistry::load_locked(dir)?;
        let reapable = collect_reapable(&locked_registry.registry, older_than_duration.as_ref());

        if reapable.is_empty() {
            print_results(&reapable, dry_run, json);
            return Ok(());
        }

        // Remove each reapable agent
        for agent in &reapable {
            locked_registry.unregister_agent(&agent.id);
        }
        locked_registry.save()?;

        print_results(&reapable, dry_run, json);
    }

    Ok(())
}

fn print_results(reaped: &[AgentEntry], dry_run: bool, json: bool) {
    let dead = reaped
        .iter()
        .filter(|a| a.status == AgentStatus::Dead)
        .count();
    let done = reaped
        .iter()
        .filter(|a| a.status == AgentStatus::Done)
        .count();
    let failed = reaped
        .iter()
        .filter(|a| a.status == AgentStatus::Failed)
        .count();

    if json {
        let output = serde_json::json!({
            "dry_run": dry_run,
            "count": reaped.len(),
            "dead": dead,
            "done": done,
            "failed": failed,
            "agents": reaped.iter().map(|a| serde_json::json!({
                "id": a.id,
                "task_id": a.task_id,
                "status": format!("{:?}", a.status).to_lowercase(),
                "completed_at": a.completed_at,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
        return;
    }

    if reaped.is_empty() {
        println!("No agents to reap.");
        return;
    }

    if dry_run {
        println!("Would reap {} agent(s):", reaped.len());
    } else {
        println!(
            "Reaped {} agent(s) ({} dead, {} done, {} failed)",
            reaped.len(),
            dead,
            done,
            failed
        );
    }

    for agent in reaped {
        let status = format!("{:?}", agent.status).to_lowercase();
        println!("  {} [{}] — task '{}'", agent.id, status, agent.task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_registry_with_agents() -> (TempDir, AgentRegistry) {
        let temp_dir = TempDir::new().unwrap();
        let mut registry = AgentRegistry::new();

        // Working agent (alive — should NOT be reaped)
        registry.register_agent(100, "task-alive", "claude", "/tmp/alive.log");

        // Dead agent
        let id_dead = registry.register_agent(200, "task-dead", "claude", "/tmp/dead.log");
        registry.set_status(&id_dead, AgentStatus::Dead);

        // Done agent
        let id_done = registry.register_agent(300, "task-done", "claude", "/tmp/done.log");
        registry.set_status(&id_done, AgentStatus::Done);

        // Failed agent
        let id_failed = registry.register_agent(400, "task-failed", "shell", "/tmp/failed.log");
        registry.set_status(&id_failed, AgentStatus::Failed);

        // Idle agent (alive — should NOT be reaped)
        let id_idle = registry.register_agent(500, "task-idle", "claude", "/tmp/idle.log");
        registry.set_status(&id_idle, AgentStatus::Idle);

        (temp_dir, registry)
    }

    #[test]
    fn test_is_reapable() {
        assert!(is_reapable(AgentStatus::Dead));
        assert!(is_reapable(AgentStatus::Done));
        assert!(is_reapable(AgentStatus::Failed));
        assert!(!is_reapable(AgentStatus::Working));
        assert!(!is_reapable(AgentStatus::Starting));
        assert!(!is_reapable(AgentStatus::Idle));
        assert!(!is_reapable(AgentStatus::Stopping));
        assert!(!is_reapable(AgentStatus::Frozen));
        assert!(!is_reapable(AgentStatus::Parked));
    }

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::seconds(30));
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::minutes(5));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::hours(1));
    }

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(parse_duration("7d").unwrap(), Duration::days(7));
    }

    #[test]
    fn test_parse_duration_weeks() {
        assert_eq!(parse_duration("2w").unwrap(), Duration::weeks(2));
    }

    #[test]
    fn test_parse_duration_no_unit_defaults_to_seconds() {
        assert_eq!(parse_duration("60").unwrap(), Duration::seconds(60));
    }

    #[test]
    fn test_parse_duration_empty_fails() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn test_parse_duration_invalid_number() {
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn test_collect_reapable_no_filter() {
        let (_tmp, registry) = make_registry_with_agents();
        let reapable = collect_reapable(&registry, None);
        assert_eq!(reapable.len(), 3); // dead + done + failed
        let ids: Vec<&str> = reapable.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"agent-2")); // dead
        assert!(ids.contains(&"agent-3")); // done
        assert!(ids.contains(&"agent-4")); // failed
    }

    #[test]
    fn test_collect_reapable_with_age_filter() {
        let (_tmp, mut registry) = make_registry_with_agents();

        // Set agent-2 (dead) completed_at to 2 hours ago
        if let Some(agent) = registry.get_agent_mut("agent-2") {
            agent.completed_at = Some((Utc::now() - Duration::hours(2)).to_rfc3339());
        }
        // Set agent-3 (done) completed_at to 30 seconds ago
        if let Some(agent) = registry.get_agent_mut("agent-3") {
            agent.completed_at = Some((Utc::now() - Duration::seconds(30)).to_rfc3339());
        }
        // Set agent-4 (failed) completed_at to 3 hours ago
        if let Some(agent) = registry.get_agent_mut("agent-4") {
            agent.completed_at = Some((Utc::now() - Duration::hours(3)).to_rfc3339());
        }

        // Only reap agents older than 1 hour
        let min_age = Duration::hours(1);
        let reapable = collect_reapable(&registry, Some(&min_age));
        assert_eq!(reapable.len(), 2); // agent-2 (2h) and agent-4 (3h)
        let ids: Vec<&str> = reapable.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"agent-2"));
        assert!(ids.contains(&"agent-4"));
        assert!(!ids.contains(&"agent-3")); // too recent
    }

    #[test]
    fn test_collect_reapable_no_reapable_agents() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(100, "task-1", "claude", "/tmp/1.log");
        registry.register_agent(200, "task-2", "claude", "/tmp/2.log");
        // All agents are Working (alive)
        let reapable = collect_reapable(&registry, None);
        assert!(reapable.is_empty());
    }

    #[test]
    fn test_reap_dry_run_does_not_modify_registry() {
        let (tmp, registry) = make_registry_with_agents();
        registry.save(tmp.path()).unwrap();

        let original_count = registry.agents.len();

        // Run reap in dry-run mode
        run(tmp.path(), true, None, false).unwrap();

        // Registry should be unchanged
        let loaded = AgentRegistry::load(tmp.path()).unwrap();
        assert_eq!(loaded.agents.len(), original_count);
    }

    #[test]
    fn test_reap_removes_dead_done_failed() {
        let (tmp, registry) = make_registry_with_agents();
        registry.save(tmp.path()).unwrap();

        // Run reap
        run(tmp.path(), false, None, false).unwrap();

        // Only alive agents should remain
        let loaded = AgentRegistry::load(tmp.path()).unwrap();
        assert_eq!(loaded.agents.len(), 2); // agent-1 (working) + agent-5 (idle)
        assert!(loaded.get_agent("agent-1").is_some()); // working
        assert!(loaded.get_agent("agent-5").is_some()); // idle
        assert!(loaded.get_agent("agent-2").is_none()); // dead — reaped
        assert!(loaded.get_agent("agent-3").is_none()); // done — reaped
        assert!(loaded.get_agent("agent-4").is_none()); // failed — reaped
    }

    #[test]
    fn test_reap_with_older_than_filter() {
        let (tmp, mut registry) = make_registry_with_agents();

        // Make agent-2 (dead) old enough
        if let Some(agent) = registry.get_agent_mut("agent-2") {
            agent.completed_at = Some((Utc::now() - Duration::hours(2)).to_rfc3339());
        }
        // Keep agent-3 (done) recent
        if let Some(agent) = registry.get_agent_mut("agent-3") {
            agent.completed_at = Some(Utc::now().to_rfc3339());
        }
        // Make agent-4 (failed) old enough
        if let Some(agent) = registry.get_agent_mut("agent-4") {
            agent.completed_at = Some((Utc::now() - Duration::hours(5)).to_rfc3339());
        }

        registry.save(tmp.path()).unwrap();

        // Reap only agents older than 1 hour
        run(tmp.path(), false, Some("1h"), false).unwrap();

        let loaded = AgentRegistry::load(tmp.path()).unwrap();
        assert_eq!(loaded.agents.len(), 3); // alive(2) + recent done(1)
        assert!(loaded.get_agent("agent-1").is_some()); // working
        assert!(loaded.get_agent("agent-3").is_some()); // done but too recent
        assert!(loaded.get_agent("agent-5").is_some()); // idle
        assert!(loaded.get_agent("agent-2").is_none()); // dead 2h — reaped
        assert!(loaded.get_agent("agent-4").is_none()); // failed 5h — reaped
    }

    #[test]
    fn test_reap_preserves_next_agent_id() {
        let (tmp, registry) = make_registry_with_agents();
        let original_next_id = registry.next_agent_id;
        registry.save(tmp.path()).unwrap();

        run(tmp.path(), false, None, false).unwrap();

        let loaded = AgentRegistry::load(tmp.path()).unwrap();
        assert_eq!(loaded.next_agent_id, original_next_id);
    }

    #[test]
    fn test_reap_empty_registry() {
        let tmp = TempDir::new().unwrap();
        // No registry file yet — should succeed with "No agents to reap"
        let result = run(tmp.path(), false, None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_reap_json_output() {
        let (tmp, registry) = make_registry_with_agents();
        registry.save(tmp.path()).unwrap();

        // Just verify it doesn't panic with json=true
        run(tmp.path(), true, None, true).unwrap();
        run(tmp.path(), false, None, true).unwrap();
    }
}
