//! Metrics and monitoring infrastructure for workgraph operations.
//!
//! This module provides centralized tracking of cleanup operations, timing statistics,
//! and recovery branch management for observability and debugging purposes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

/// Global metrics collector for cleanup operations.
static CLEANUP_METRICS: CleanupMetrics = CleanupMetrics::new();

/// Metrics for cleanup operations including success/failure rates and timing.
#[derive(Debug)]
pub struct CleanupMetrics {
    /// Total number of successful cleanup operations.
    pub cleanup_success_count: AtomicU64,
    /// Total number of failed cleanup operations.
    pub cleanup_failure_count: AtomicU64,
    /// Total number of recovery branches created.
    pub recovery_branch_count: AtomicU64,
    /// Total number of orphaned worktrees cleaned.
    pub orphaned_cleanup_count: AtomicU64,
    /// Total number of dead agent cleanups.
    pub dead_agent_cleanup_count: AtomicU64,

    /// Timing statistics (protected by mutex for updates).
    timing_stats: Mutex<TimingStats>,
}

/// Detailed timing statistics for cleanup operations.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TimingStats {
    /// Average cleanup duration in milliseconds.
    pub avg_cleanup_duration_ms: f64,
    /// Total cleanup time accumulated.
    pub total_cleanup_time_ms: u64,
    /// Number of timed operations.
    pub timed_operations: u64,
    /// Minimum cleanup duration observed.
    pub min_cleanup_duration_ms: u64,
    /// Maximum cleanup duration observed.
    pub max_cleanup_duration_ms: u64,
    /// Resource recovery statistics.
    pub resource_stats: ResourceRecoveryStats,
}

/// Statistics for resource recovery during cleanup operations.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ResourceRecoveryStats {
    /// Number of worktrees successfully removed.
    pub worktrees_removed: u64,
    /// Number of symlinks cleaned up.
    pub symlinks_cleaned: u64,
    /// Number of directories removed.
    pub directories_removed: u64,
    /// Number of git branches pruned.
    pub branches_pruned: u64,
    /// Total disk space recovered in bytes.
    pub disk_space_recovered_bytes: u64,
}

/// Complete metrics snapshot for reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Cleanup success count.
    pub cleanup_success: u64,
    /// Cleanup failure count.
    pub cleanup_failure: u64,
    /// Recovery branch count.
    pub recovery_branches: u64,
    /// Orphaned cleanup count.
    pub orphaned_cleanups: u64,
    /// Dead agent cleanup count.
    pub dead_agent_cleanups: u64,
    /// Success rate as a percentage.
    pub success_rate_percent: f64,
    /// Timing statistics.
    pub timing: TimingStats,
}

impl CleanupMetrics {
    /// Create a new metrics collector.
    const fn new() -> Self {
        Self {
            cleanup_success_count: AtomicU64::new(0),
            cleanup_failure_count: AtomicU64::new(0),
            recovery_branch_count: AtomicU64::new(0),
            orphaned_cleanup_count: AtomicU64::new(0),
            dead_agent_cleanup_count: AtomicU64::new(0),
            timing_stats: Mutex::new(TimingStats {
                avg_cleanup_duration_ms: 0.0,
                total_cleanup_time_ms: 0,
                timed_operations: 0,
                min_cleanup_duration_ms: u64::MAX,
                max_cleanup_duration_ms: 0,
                resource_stats: ResourceRecoveryStats {
                    worktrees_removed: 0,
                    symlinks_cleaned: 0,
                    directories_removed: 0,
                    branches_pruned: 0,
                    disk_space_recovered_bytes: 0,
                },
            }),
        }
    }

    /// Record a successful cleanup operation.
    pub fn record_cleanup_success(&self) {
        self.cleanup_success_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failed cleanup operation.
    pub fn record_cleanup_failure(&self) {
        self.cleanup_failure_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record recovery branch creation.
    pub fn record_recovery_branch(&self) {
        self.recovery_branch_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record orphaned worktree cleanup.
    pub fn record_orphaned_cleanup(&self) {
        self.orphaned_cleanup_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record dead agent cleanup.
    pub fn record_dead_agent_cleanup(&self) {
        self.dead_agent_cleanup_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record cleanup timing and resource recovery statistics.
    pub fn record_cleanup_timing(&self, duration: Duration, resources: ResourceRecoveryStats) {
        let duration_ms = duration.as_millis() as u64;

        if let Ok(mut timing) = self.timing_stats.lock() {
            timing.timed_operations += 1;
            timing.total_cleanup_time_ms += duration_ms;
            timing.avg_cleanup_duration_ms = timing.total_cleanup_time_ms as f64 / timing.timed_operations as f64;

            if timing.min_cleanup_duration_ms == 0 || duration_ms < timing.min_cleanup_duration_ms {
                timing.min_cleanup_duration_ms = duration_ms;
            }
            if duration_ms > timing.max_cleanup_duration_ms {
                timing.max_cleanup_duration_ms = duration_ms;
            }

            // Aggregate resource statistics
            timing.resource_stats.worktrees_removed += resources.worktrees_removed;
            timing.resource_stats.symlinks_cleaned += resources.symlinks_cleaned;
            timing.resource_stats.directories_removed += resources.directories_removed;
            timing.resource_stats.branches_pruned += resources.branches_pruned;
            timing.resource_stats.disk_space_recovered_bytes += resources.disk_space_recovered_bytes;
        }
    }

    /// Get a snapshot of current metrics.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let success = self.cleanup_success_count.load(Ordering::Relaxed);
        let failure = self.cleanup_failure_count.load(Ordering::Relaxed);
        let total = success + failure;
        let success_rate = if total > 0 {
            (success as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        let timing = self.timing_stats.lock()
            .map(|stats| stats.clone())
            .unwrap_or_default();

        MetricsSnapshot {
            cleanup_success: success,
            cleanup_failure: failure,
            recovery_branches: self.recovery_branch_count.load(Ordering::Relaxed),
            orphaned_cleanups: self.orphaned_cleanup_count.load(Ordering::Relaxed),
            dead_agent_cleanups: self.dead_agent_cleanup_count.load(Ordering::Relaxed),
            success_rate_percent: success_rate,
            timing,
        }
    }

    /// Reset all metrics (useful for testing).
    #[cfg(test)]
    pub fn reset(&self) {
        self.cleanup_success_count.store(0, Ordering::Relaxed);
        self.cleanup_failure_count.store(0, Ordering::Relaxed);
        self.recovery_branch_count.store(0, Ordering::Relaxed);
        self.orphaned_cleanup_count.store(0, Ordering::Relaxed);
        self.dead_agent_cleanup_count.store(0, Ordering::Relaxed);

        if let Ok(mut timing) = self.timing_stats.lock() {
            *timing = TimingStats::default();
        }
    }
}

/// Timer for measuring cleanup operation duration.
pub struct CleanupTimer {
    start: Instant,
    operation_type: String,
}

impl CleanupTimer {
    /// Start timing a cleanup operation.
    pub fn start(operation_type: impl Into<String>) -> Self {
        let op_name = operation_type.into();
        eprintln!("[metrics] Starting cleanup timer for: {}", op_name);
        Self {
            start: Instant::now(),
            operation_type: op_name,
        }
    }

    /// Complete the timing and record results.
    pub fn complete(self, success: bool, resources: ResourceRecoveryStats) {
        let duration = self.start.elapsed();

        eprintln!("[metrics] Cleanup '{}' completed in {}ms (success: {}, resources: {} worktrees, {} bytes recovered)",
            self.operation_type,
            duration.as_millis(),
            success,
            resources.worktrees_removed,
            resources.disk_space_recovered_bytes
        );

        // Record the metrics
        if success {
            CLEANUP_METRICS.record_cleanup_success();
        } else {
            CLEANUP_METRICS.record_cleanup_failure();
        }

        CLEANUP_METRICS.record_cleanup_timing(duration, resources);
    }
}

/// Global functions for accessing the metrics collector.

/// Record a successful cleanup operation.
pub fn record_cleanup_success() {
    CLEANUP_METRICS.record_cleanup_success();
}

/// Record a failed cleanup operation.
pub fn record_cleanup_failure() {
    CLEANUP_METRICS.record_cleanup_failure();
}

/// Record recovery branch creation.
pub fn record_recovery_branch() {
    CLEANUP_METRICS.record_recovery_branch();
    eprintln!("[metrics] Recovery branch created (total: {})",
        CLEANUP_METRICS.recovery_branch_count.load(Ordering::Relaxed));
}

/// Record orphaned worktree cleanup.
pub fn record_orphaned_cleanup() {
    CLEANUP_METRICS.record_orphaned_cleanup();
}

/// Record dead agent cleanup.
pub fn record_dead_agent_cleanup() {
    CLEANUP_METRICS.record_dead_agent_cleanup();
}

/// Get current metrics snapshot.
pub fn get_metrics_snapshot() -> MetricsSnapshot {
    CLEANUP_METRICS.snapshot()
}

/// Log current metrics to stderr for debugging.
pub fn log_metrics_summary() {
    let metrics = get_metrics_snapshot();
    eprintln!("[metrics] === Cleanup Metrics Summary ===");
    eprintln!("[metrics] Success: {} | Failure: {} | Success Rate: {:.1}%",
        metrics.cleanup_success, metrics.cleanup_failure, metrics.success_rate_percent);
    eprintln!("[metrics] Recovery Branches: {} | Orphaned: {} | Dead Agents: {}",
        metrics.recovery_branches, metrics.orphaned_cleanups, metrics.dead_agent_cleanups);
    eprintln!("[metrics] Avg Cleanup Time: {:.1}ms | Total Operations: {}",
        metrics.timing.avg_cleanup_duration_ms, metrics.timing.timed_operations);
    eprintln!("[metrics] Resources Recovered: {} worktrees, {} bytes",
        metrics.timing.resource_stats.worktrees_removed,
        metrics.timing.resource_stats.disk_space_recovered_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_cleanup_metrics_basic() {
        let metrics = CleanupMetrics::new();

        metrics.record_cleanup_success();
        metrics.record_cleanup_success();
        metrics.record_cleanup_failure();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.cleanup_success, 2);
        assert_eq!(snapshot.cleanup_failure, 1);
        assert_eq!(snapshot.success_rate_percent, 66.66666666666667);
    }

    #[test]
    fn test_cleanup_timer() {
        CLEANUP_METRICS.reset();

        let timer = CleanupTimer::start("test-operation");
        thread::sleep(Duration::from_millis(10));

        let resources = ResourceRecoveryStats {
            worktrees_removed: 1,
            disk_space_recovered_bytes: 1024,
            ..Default::default()
        };

        timer.complete(true, resources);

        let snapshot = CLEANUP_METRICS.snapshot();
        assert_eq!(snapshot.cleanup_success, 1);
        assert!(snapshot.timing.avg_cleanup_duration_ms >= 10.0);
        assert_eq!(snapshot.timing.resource_stats.worktrees_removed, 1);
    }

    #[test]
    fn test_recovery_branch_tracking() {
        CLEANUP_METRICS.reset();

        CLEANUP_METRICS.record_recovery_branch();
        CLEANUP_METRICS.record_recovery_branch();

        let snapshot = CLEANUP_METRICS.snapshot();
        assert_eq!(snapshot.recovery_branches, 2);
    }

    #[test]
    fn test_resource_recovery_stats() {
        CLEANUP_METRICS.reset();

        let resources1 = ResourceRecoveryStats {
            worktrees_removed: 2,
            symlinks_cleaned: 3,
            directories_removed: 1,
            branches_pruned: 1,
            disk_space_recovered_bytes: 2048,
        };

        let resources2 = ResourceRecoveryStats {
            worktrees_removed: 1,
            symlinks_cleaned: 2,
            directories_removed: 1,
            branches_pruned: 0,
            disk_space_recovered_bytes: 1024,
        };

        CLEANUP_METRICS.record_cleanup_timing(Duration::from_millis(100), resources1);
        CLEANUP_METRICS.record_cleanup_timing(Duration::from_millis(200), resources2);

        let snapshot = CLEANUP_METRICS.snapshot();
        assert_eq!(snapshot.timing.resource_stats.worktrees_removed, 3);
        assert_eq!(snapshot.timing.resource_stats.disk_space_recovered_bytes, 3072);
        assert_eq!(snapshot.timing.avg_cleanup_duration_ms, 150.0);
    }
}