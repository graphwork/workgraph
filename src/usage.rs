//! Usage statistics tracking for workgraph commands
//!
//! This module implements a hybrid approach for tracking command usage:
//! - Write path: Append-only log file (O(1) writes, no locking via O_APPEND)
//! - Aggregation: Service daemon periodically aggregates logs to stats.json
//! - Read path: Fast reads from pre-aggregated stats.json
//!
//! File layout:
//! - `.workgraph/usage.log` - Append-only log of command invocations
//! - `.workgraph/stats.json` - Aggregated command counts

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Aggregated usage statistics stored in stats.json
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageStats {
    /// Schema version for future migrations
    pub version: u32,
    /// Command name -> invocation count
    pub counts: HashMap<String, u64>,
}

/// Minimum total invocations before using personalized ordering
pub const MIN_TOTAL_INVOCATIONS: u64 = 20;

/// Tier thresholds for command grouping
pub const TIER_FREQUENT_PCT: f64 = 10.0; // ≥10% of usage
pub const TIER_OCCASIONAL_PCT: f64 = 2.0; // ≥2% of usage

/// Maximum commands to show in default help output
pub const MAX_HELP_COMMANDS: usize = 15;

/// Path to the usage log file
pub fn log_path(dir: &Path) -> std::path::PathBuf {
    dir.join("usage.log")
}

/// Path to the aggregated stats file
pub fn stats_path(dir: &Path) -> std::path::PathBuf {
    dir.join("stats.json")
}

/// Append a command invocation to the usage log.
///
/// This is fire-and-forget: errors are silently ignored to avoid
/// impacting command execution. Uses O_APPEND for atomic writes.
pub fn append_usage_log(dir: &Path, command: &str) {
    // Skip if .workgraph doesn't exist (not initialized)
    if !dir.exists() {
        return;
    }

    let path = log_path(dir);
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = format!("{} {}\n", timestamp, command);

    // Open with O_APPEND for atomic appends (no locking needed on POSIX)
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Aggregate usage log entries and write to stats.json.
///
/// Called by the service daemon on startup and periodically.
/// Returns the number of log entries processed.
pub fn aggregate_usage_stats(dir: &Path) -> anyhow::Result<usize> {
    let log = log_path(dir);
    let stats = stats_path(dir);

    // Load existing stats (or start fresh)
    let mut usage = if stats.exists() {
        let content = fs::read_to_string(&stats)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        UsageStats::default()
    };
    usage.version = 1;

    // Parse log file and count commands
    let mut entries_processed = 0;
    if log.exists() {
        let file = File::open(&log)?;
        let reader = BufReader::new(file);

        for line in reader.lines() {
            if let Ok(line) = line {
                // Format: "{timestamp} {command}"
                if let Some(cmd) = line.split_whitespace().nth(1) {
                    *usage.counts.entry(cmd.to_string()).or_insert(0) += 1;
                    entries_processed += 1;
                }
            }
        }
    }

    // Write updated stats
    let content = serde_json::to_string_pretty(&usage)?;
    fs::write(&stats, content)?;

    // Truncate the log file (entries are now aggregated)
    if entries_processed > 0 {
        let _ = File::create(&log); // Truncate by creating empty file
    }

    Ok(entries_processed)
}

/// Load command ordering from stats.json.
///
/// Returns commands sorted by usage count (descending).
/// If stats are missing or insufficient, returns None.
pub fn load_command_order(dir: &Path) -> Option<Vec<(String, u64)>> {
    let path = stats_path(dir);
    if !path.exists() {
        return None;
    }

    let content = fs::read_to_string(&path).ok()?;
    let stats: UsageStats = serde_json::from_str(&content).ok()?;

    // Check for sufficient data
    let total: u64 = stats.counts.values().sum();
    if total < MIN_TOTAL_INVOCATIONS {
        return None;
    }

    // Sort by count descending, then alphabetically for ties
    let mut commands: Vec<(String, u64)> = stats.counts.into_iter().collect();
    commands.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Some(commands)
}

/// Command tier for help display grouping
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Frequent,
    Occasional,
    Rare,
}

/// Determine the tier for a command based on its usage percentage
pub fn tier_for_count(count: u64, total: u64) -> Tier {
    if total == 0 {
        return Tier::Rare;
    }
    let pct = (count as f64 / total as f64) * 100.0;
    if pct >= TIER_FREQUENT_PCT {
        Tier::Frequent
    } else if pct >= TIER_OCCASIONAL_PCT {
        Tier::Occasional
    } else {
        Tier::Rare
    }
}

/// Group commands into tiers based on usage statistics
pub fn group_by_tier(commands: &[(String, u64)]) -> (Vec<&str>, Vec<&str>, Vec<&str>) {
    let total: u64 = commands.iter().map(|(_, c)| *c).sum();
    let mut frequent = Vec::new();
    let mut occasional = Vec::new();
    let mut rare = Vec::new();

    for (cmd, count) in commands {
        match tier_for_count(*count, total) {
            Tier::Frequent => frequent.push(cmd.as_str()),
            Tier::Occasional => occasional.push(cmd.as_str()),
            Tier::Rare => rare.push(cmd.as_str()),
        }
    }

    (frequent, occasional, rare)
}

/// Default command ordering for cold start (before we have usage data)
pub const DEFAULT_ORDER: &[&str] = &[
    // Tier 1: Essential viewing (what you run first)
    "list",
    "ready",
    "status",
    "show",
    // Tier 2: Task lifecycle (common workflow)
    "add",
    "done",
    "claim",
    "fail",
    "log",
    // Tier 3: Working on tasks
    "artifact",
    "context",
    "submit",
    // Tier 4: Setup & structure
    "init",
    "quickstart",
    // Tier 5: Automation
    "spawn",
    "agents",
    "service",
    // Tier 6: Advanced
    "analyze",
    "config",
];

/// Get command order for help display.
///
/// Returns (commands, is_personalized) where is_personalized indicates
/// whether the ordering is based on actual usage data.
pub fn get_help_order(dir: &Path) -> (Vec<String>, bool) {
    if let Some(commands) = load_command_order(dir) {
        let names: Vec<String> = commands.into_iter().map(|(name, _)| name).collect();
        (names, true)
    } else {
        // Use default ordering
        let names: Vec<String> = DEFAULT_ORDER.iter().map(|s| s.to_string()).collect();
        (names, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_append_usage_log() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        append_usage_log(dir, "list");
        append_usage_log(dir, "done");
        append_usage_log(dir, "list");

        let log = log_path(dir);
        let content = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with(" list"));
        assert!(lines[1].ends_with(" done"));
        assert!(lines[2].ends_with(" list"));
    }

    #[test]
    fn test_aggregate_usage_stats() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Write some log entries
        append_usage_log(dir, "list");
        append_usage_log(dir, "list");
        append_usage_log(dir, "done");
        append_usage_log(dir, "list");
        append_usage_log(dir, "add");

        // Aggregate
        let processed = aggregate_usage_stats(dir).unwrap();
        assert_eq!(processed, 5);

        // Check stats.json
        let stats_file = stats_path(dir);
        let content = fs::read_to_string(&stats_file).unwrap();
        let stats: UsageStats = serde_json::from_str(&content).unwrap();
        assert_eq!(stats.counts.get("list"), Some(&3));
        assert_eq!(stats.counts.get("done"), Some(&1));
        assert_eq!(stats.counts.get("add"), Some(&1));

        // Log should be truncated
        let log = log_path(dir);
        let log_content = fs::read_to_string(&log).unwrap();
        assert!(log_content.is_empty());
    }

    #[test]
    fn test_aggregate_incremental() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // First batch
        append_usage_log(dir, "list");
        append_usage_log(dir, "list");
        aggregate_usage_stats(dir).unwrap();

        // Second batch
        append_usage_log(dir, "list");
        append_usage_log(dir, "done");
        aggregate_usage_stats(dir).unwrap();

        // Check cumulative counts
        let content = fs::read_to_string(stats_path(dir)).unwrap();
        let stats: UsageStats = serde_json::from_str(&content).unwrap();
        assert_eq!(stats.counts.get("list"), Some(&3));
        assert_eq!(stats.counts.get("done"), Some(&1));
    }

    #[test]
    fn test_load_command_order_insufficient_data() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Only a few invocations (< MIN_TOTAL_INVOCATIONS)
        for _ in 0..10 {
            append_usage_log(dir, "list");
        }
        aggregate_usage_stats(dir).unwrap();

        // Should return None due to insufficient data
        assert!(load_command_order(dir).is_none());
    }

    #[test]
    fn test_load_command_order_sufficient_data() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Generate enough invocations
        for _ in 0..15 {
            append_usage_log(dir, "list");
        }
        for _ in 0..5 {
            append_usage_log(dir, "done");
        }
        for _ in 0..3 {
            append_usage_log(dir, "add");
        }
        aggregate_usage_stats(dir).unwrap();

        let order = load_command_order(dir).unwrap();
        assert_eq!(order[0].0, "list");
        assert_eq!(order[1].0, "done");
        assert_eq!(order[2].0, "add");
    }

    #[test]
    fn test_tier_classification() {
        // 50 total, 15 for cmd1 (30%), 5 for cmd2 (10%), 2 for cmd3 (4%), 28 for others
        assert_eq!(tier_for_count(15, 50), Tier::Frequent);
        assert_eq!(tier_for_count(5, 50), Tier::Frequent);
        assert_eq!(tier_for_count(2, 50), Tier::Occasional);
        assert_eq!(tier_for_count(1, 100), Tier::Rare);
        assert_eq!(tier_for_count(0, 100), Tier::Rare);
        assert_eq!(tier_for_count(10, 0), Tier::Rare); // edge case: total=0
    }

    #[test]
    fn test_group_by_tier() {
        let commands = vec![
            ("list".to_string(), 50),
            ("done".to_string(), 20),
            ("add".to_string(), 10),
            ("show".to_string(), 5),
            ("config".to_string(), 1),
        ];

        let (frequent, occasional, rare) = group_by_tier(&commands);
        // Total = 86
        // list: 58% -> frequent
        // done: 23% -> frequent
        // add: 11.6% -> frequent
        // show: 5.8% -> occasional
        // config: 1.1% -> rare
        assert!(frequent.contains(&"list"));
        assert!(frequent.contains(&"done"));
        assert!(frequent.contains(&"add"));
        assert!(occasional.contains(&"show"));
        assert!(rare.contains(&"config"));
    }

    #[test]
    fn test_get_help_order_default() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        let (order, is_personalized) = get_help_order(dir);
        assert!(!is_personalized);
        assert!(!order.is_empty());
        assert_eq!(order[0], "list");
    }

    #[test]
    fn test_skip_nonexistent_dir() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent = temp_dir.path().join("does-not-exist");

        // Should silently skip - no panic or error
        append_usage_log(&nonexistent, "list");

        // Log file should not exist
        assert!(!log_path(&nonexistent).exists());
    }
}
