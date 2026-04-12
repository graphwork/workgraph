//! `wg metrics` — display cleanup and monitoring metrics.

use anyhow::Result;
use std::path::Path;

use workgraph::metrics::get_metrics_snapshot;

/// Run the metrics command to display cleanup monitoring statistics.
pub fn run(_dir: &Path, json: bool) -> Result<()> {
    let metrics = get_metrics_snapshot();

    if json {
        println!("{}", serde_json::to_string_pretty(&metrics)?);
    } else {
        println!("=== Workgraph Cleanup Metrics ===");
        println!();

        // Success/failure statistics
        let total_cleanups = metrics.cleanup_success + metrics.cleanup_failure;
        println!("Cleanup Operations:");
        println!("  ✓ Successful: {}", metrics.cleanup_success);
        println!("  ✗ Failed:     {}", metrics.cleanup_failure);
        println!("  📊 Total:     {}", total_cleanups);
        if total_cleanups > 0 {
            println!("  📈 Success Rate: {:.1}%", metrics.success_rate_percent);
        }
        println!();

        // Cleanup types
        println!("Cleanup Types:");
        println!("  🔥 Dead Agents: {}", metrics.dead_agent_cleanups);
        println!("  👻 Orphaned:    {}", metrics.orphaned_cleanups);
        println!("  🌿 Recovery Branches: {}", metrics.recovery_branches);
        println!();

        // Timing statistics
        if metrics.timing.timed_operations > 0 {
            println!("Timing Statistics:");
            println!(
                "  ⏱️  Average Duration: {:.1}ms",
                metrics.timing.avg_cleanup_duration_ms
            );
            println!(
                "  ⚡ Fastest:          {}ms",
                metrics.timing.min_cleanup_duration_ms
            );
            println!(
                "  🐌 Slowest:          {}ms",
                metrics.timing.max_cleanup_duration_ms
            );
            println!("  🔢 Total Operations: {}", metrics.timing.timed_operations);
            println!();

            // Resource recovery
            println!("Resource Recovery:");
            println!(
                "  📁 Worktrees Removed: {}",
                metrics.timing.resource_stats.worktrees_removed
            );
            println!(
                "  🔗 Symlinks Cleaned:  {}",
                metrics.timing.resource_stats.symlinks_cleaned
            );
            println!(
                "  📂 Directories:       {}",
                metrics.timing.resource_stats.directories_removed
            );
            println!(
                "  🌳 Branches Pruned:   {}",
                metrics.timing.resource_stats.branches_pruned
            );

            let bytes = metrics.timing.resource_stats.disk_space_recovered_bytes;
            if bytes > 0 {
                println!("  💾 Disk Space Recovered: {}", format_bytes(bytes));
            }
        } else {
            println!("No cleanup timing data available.");
        }
    }

    Ok(())
}

/// Format bytes into a human-readable string.
fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;

    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }

    if unit_idx == 0 {
        format!("{:.0}{}", size, UNITS[unit_idx])
    } else {
        format!("{:.1}{}", size, UNITS[unit_idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(1024), "1.0KB");
        assert_eq!(format_bytes(1536), "1.5KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0GB");
        assert_eq!(format_bytes(1536 * 1024 * 1024), "1.5GB");
    }
}
