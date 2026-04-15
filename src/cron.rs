use crate::graph::Task;
use chrono::{DateTime, Duration, Utc};
use cron::Schedule;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CronError {
    #[error("Invalid cron expression: {0}")]
    InvalidExpression(String),
    #[error("Cron parsing failed: {0}")]
    ParseError(#[from] cron::error::Error),
}

/// Parse a cron expression string into a Schedule
///
/// Supports both 5-field ("min hour day month dow") and 6-field ("sec min hour day month dow") formats.
/// 5-field expressions are automatically converted to 6-field by prepending "0" for seconds.
///
/// # Arguments
/// * `expr` - A cron expression string (5 or 6 field format)
///
/// # Returns
/// * `Result<Schedule, CronError>` - The parsed schedule or an error
///
/// # Examples
/// ```
/// use workgraph::cron::parse_cron_expression;
///
/// let schedule1 = parse_cron_expression("0 2 * * *").unwrap();    // 5-field: daily at 2 AM
/// let schedule2 = parse_cron_expression("0 0 2 * * *").unwrap();  // 6-field: daily at 2 AM
/// ```
pub fn parse_cron_expression(expr: &str) -> Result<Schedule, CronError> {
    let parts: Vec<&str> = expr.split_whitespace().collect();

    let expr_to_parse = match parts.len() {
        5 => {
            // 5-field format: prepend "0" for seconds
            format!("0 {}", expr)
        }
        6 => {
            // 6-field format: use as-is
            expr.to_string()
        }
        _ => {
            return Err(CronError::InvalidExpression(format!(
                "Expected 5 or 6 fields, got {}",
                parts.len()
            )));
        }
    };

    Schedule::from_str(&expr_to_parse).map_err(CronError::ParseError)
}

/// Calculate the next fire time for a cron schedule from a given datetime
///
/// # Arguments
/// * `schedule` - The cron schedule
/// * `from` - The datetime to calculate from
///
/// # Returns
/// * `Option<DateTime<Utc>>` - The next fire time, or None if no next time exists
///
/// # Examples
/// ```
/// use workgraph::cron::{parse_cron_expression, calculate_next_fire};
/// use chrono::Utc;
///
/// let schedule = parse_cron_expression("0 0 2 * * *").unwrap(); // Daily at 2 AM
/// let next_fire = calculate_next_fire(&schedule, Utc::now());
/// ```
pub fn calculate_next_fire(schedule: &Schedule, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
    schedule.after(&from).next()
}

/// Maximum jitter in seconds (15 minutes).
const MAX_JITTER_SECS: i64 = 15 * 60;

/// Calculate deterministic jitter for a cron task.
///
/// Jitter is ±10% of the period between consecutive fire times, capped at 15 minutes.
/// The sign and magnitude are determined by hashing the task ID, so the same task
/// always gets the same jitter offset.
///
/// # Arguments
/// * `task_id` - The task ID used as hash seed for deterministic jitter
/// * `schedule` - The parsed cron schedule
/// * `from` - A reference time to compute the period from
///
/// # Returns
/// * `Duration` - The jitter offset (may be negative)
pub fn calculate_jitter(task_id: &str, schedule: &Schedule, from: DateTime<Utc>) -> Duration {
    // Compute the period as the interval between two consecutive fire times
    let mut upcoming = schedule.after(&from);
    let first = match upcoming.next() {
        Some(t) => t,
        None => return Duration::zero(),
    };
    let second = match upcoming.next() {
        Some(t) => t,
        None => return Duration::zero(),
    };
    let period_secs = (second - first).num_seconds();
    if period_secs <= 0 {
        return Duration::zero();
    }

    // 10% of the period, capped at MAX_JITTER_SECS
    let max_offset_secs = (period_secs / 10).min(MAX_JITTER_SECS);
    if max_offset_secs == 0 {
        return Duration::zero();
    }

    // Hash the task ID to get a deterministic value in [-max_offset, +max_offset]
    let mut hasher = DefaultHasher::new();
    task_id.hash(&mut hasher);
    let hash_val = hasher.finish();

    // Map hash to range [-max_offset_secs, +max_offset_secs]
    let range = 2 * max_offset_secs + 1; // inclusive range size
    let offset = (hash_val % range as u64) as i64 - max_offset_secs;

    Duration::seconds(offset)
}

/// Calculate the next fire time for a cron task, including deterministic jitter.
///
/// # Arguments
/// * `task_id` - The task ID (used for jitter hash)
/// * `schedule` - The parsed cron schedule
/// * `from` - The time to calculate from (typically last fire or now)
///
/// # Returns
/// * `Option<DateTime<Utc>>` - The next jittered fire time
pub fn calculate_next_fire_with_jitter(
    task_id: &str,
    schedule: &Schedule,
    from: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let next = schedule.after(&from).next()?;
    let jitter = calculate_jitter(task_id, schedule, from);
    Some(next + jitter)
}

/// Reset a cron task after completion: set status to Open, update fire times.
///
/// # Arguments
/// * `task` - The task to reset (must be cron-enabled and Done)
///
/// # Returns
/// * `bool` - true if the task was reset, false if not applicable
pub fn reset_cron_task(task: &mut Task) -> bool {
    if !task.cron_enabled || task.cron_schedule.is_none() {
        return false;
    }
    if task.status != crate::graph::Status::Done {
        return false;
    }

    let cron_expr = task.cron_schedule.as_ref().unwrap();
    let schedule = match parse_cron_expression(cron_expr) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let now = Utc::now();

    // Record last fire time
    task.last_cron_fire = Some(now.to_rfc3339());

    // Compute next fire time with jitter
    task.next_cron_fire =
        calculate_next_fire_with_jitter(&task.id, &schedule, now).map(|dt| dt.to_rfc3339());

    // Reset task to Open for next cron cycle
    task.status = crate::graph::Status::Open;
    task.assigned = None;
    task.completed_at = None;

    true
}

/// Check if a task with cron scheduling is due to run based on current time
///
/// # Arguments
/// * `task` - The task to check
/// * `now` - Current datetime
///
/// # Returns
/// * `bool` - true if the task is due to run, false otherwise
///
/// # Examples
/// ```
/// use workgraph::cron::is_cron_due;
/// use workgraph::graph::Task;
/// use chrono::Utc;
///
/// let task = Task {
///     cron_enabled: true,
///     cron_schedule: Some("0 0 2 * * *".to_string()), // Daily at 2 AM
///     ..Default::default()
/// };
/// let due = is_cron_due(&task, Utc::now());
/// ```
pub fn is_cron_due(task: &Task, now: DateTime<Utc>) -> bool {
    // Check if cron is enabled for this task
    if !task.cron_enabled {
        return false;
    }

    // Must have a cron schedule
    let cron_schedule = match &task.cron_schedule {
        Some(schedule) => schedule,
        None => return false,
    };

    // Parse the cron expression
    let schedule = match parse_cron_expression(cron_schedule) {
        Ok(schedule) => schedule,
        Err(_) => return false, // Invalid cron expression means not due
    };

    // If we have a pre-computed next_cron_fire (includes jitter), use that
    if let Some(ref next_fire_str) = task.next_cron_fire {
        if let Ok(next_fire) = DateTime::parse_from_rfc3339(next_fire_str) {
            return next_fire.with_timezone(&Utc) <= now;
        }
        // Invalid timestamp, fall through to schedule-based check
    }

    // If no last fire time, check if we should fire now based on schedule
    let last_fire = match &task.last_cron_fire {
        Some(last_fire_str) => {
            match DateTime::parse_from_rfc3339(last_fire_str) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(_) => return true, // Invalid timestamp, assume we should fire
            }
        }
        None => {
            // No last fire time recorded, check if current time matches schedule
            return schedule.includes(now);
        }
    };

    // Check if there's a next fire time between last fire and now
    match calculate_next_fire(&schedule, last_fire) {
        Some(next_fire) => next_fire <= now,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn test_parse_cron_expression_valid() {
        // Daily at 2 AM
        let result = parse_cron_expression("0 0 2 * * *");
        assert!(result.is_ok());

        // Every 5 minutes
        let result = parse_cron_expression("0 */5 * * * *");
        assert!(result.is_ok());

        // Weekdays at noon
        let result = parse_cron_expression("0 0 12 * * 1-5");
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_cron_expression_invalid() {
        // Invalid format
        let result = parse_cron_expression("invalid cron");
        assert!(result.is_err());

        // Too many fields
        let result = parse_cron_expression("0 0 0 0 0 0");
        assert!(result.is_err());
    }

    #[test]
    fn test_calculate_next_fire() {
        let schedule = parse_cron_expression("0 0 2 * * *").unwrap(); // Daily at 2 AM

        // Test from 1 AM, next should be 2 AM today
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();
        let next = calculate_next_fire(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap());

        // Test from 3 AM, next should be 2 AM tomorrow
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 3, 0, 0).unwrap();
        let next = calculate_next_fire(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 2, 2, 0, 0).unwrap());
    }

    #[test]
    fn test_is_cron_due_disabled() {
        let task = Task {
            id: "test".to_string(),
            cron_enabled: false,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            ..Default::default()
        };

        let now = Utc::now();
        assert_eq!(is_cron_due(&task, now), false);
    }

    #[test]
    fn test_is_cron_due_no_schedule() {
        let task = Task {
            id: "test".to_string(),
            cron_enabled: true,
            cron_schedule: None,
            ..Default::default()
        };

        let now = Utc::now();
        assert_eq!(is_cron_due(&task, now), false);
    }

    #[test]
    fn test_is_cron_due_invalid_schedule() {
        let task = Task {
            id: "test".to_string(),
            cron_enabled: true,
            cron_schedule: Some("invalid".to_string()),
            ..Default::default()
        };

        let now = Utc::now();
        assert_eq!(is_cron_due(&task, now), false);
    }

    #[test]
    fn test_is_cron_due_no_last_fire() {
        let schedule_str = "0 0 2 * * *"; // Daily at 2 AM
        let task = Task {
            id: "test".to_string(),
            cron_enabled: true,
            cron_schedule: Some(schedule_str.to_string()),
            last_cron_fire: None,
            ..Default::default()
        };

        // Test at 2 AM - should be due
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), true);

        // Test at 3 AM - should not be due
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 3, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), false);
    }

    #[test]
    fn test_is_cron_due_with_last_fire() {
        let schedule_str = "0 0 2 * * *"; // Daily at 2 AM
        let last_fire = "2024-01-01T02:00:00Z"; // Fired at 2 AM on Jan 1

        let task = Task {
            id: "test".to_string(),
            cron_enabled: true,
            cron_schedule: Some(schedule_str.to_string()),
            last_cron_fire: Some(last_fire.to_string()),
            ..Default::default()
        };

        // Test at 1 AM next day - should not be due yet
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 1, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), false);

        // Test at 2 AM next day - should be due
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 2, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), true);

        // Test at 3 AM next day - should be due (missed the 2 AM window)
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 3, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), true);
    }

    #[test]
    fn cron_parsing() {
        // Test various cron expressions (6-field format with seconds)
        let result = parse_cron_expression("0 0 2 * * *"); // Daily at 2 AM
        if result.is_err() {
            println!("Debug: Error parsing '0 0 2 * * *': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 */5 * * * *"); // Every 5 minutes
        if result.is_err() {
            println!("Debug: Error parsing '0 */5 * * * *': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 0 12 * * 1-5"); // Weekdays at noon
        if result.is_err() {
            println!("Debug: Error parsing '0 0 12 * * 1-5': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 30 14 1 * *"); // 2:30 PM on 1st day of month
        if result.is_err() {
            println!("Debug: Error parsing '0 30 14 1 * *': {:?}", result);
        }
        assert!(result.is_ok());

        // Test invalid expressions
        assert!(parse_cron_expression("invalid").is_err());
        assert!(parse_cron_expression("").is_err());
        assert!(parse_cron_expression("60 25 32 13 8 8").is_err()); // Invalid values
    }

    #[test]
    fn test_cron_task_becomes_ready_at_fire_time() {
        // A cron task with next_cron_fire in the past should be due
        let past_fire = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();
        let task = Task {
            id: "cron-ready-test".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            next_cron_fire: Some(past_fire.to_rfc3339()),
            ..Default::default()
        };

        // Time after fire time → should be due
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 1).unwrap();
        assert!(is_cron_due(&task, now));

        // Time exactly at fire time → should be due
        assert!(is_cron_due(&task, past_fire));

        // Time before fire time → should NOT be due
        let before = Utc.with_ymd_and_hms(2024, 1, 1, 1, 59, 59).unwrap();
        assert!(!is_cron_due(&task, before));
    }

    #[test]
    fn test_cron_task_resets_to_open_after_completion() {
        let mut task = Task {
            id: "cron-reset-test".to_string(),
            status: crate::graph::Status::Done,
            cron_enabled: true,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            assigned: Some("agent-123".to_string()),
            completed_at: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let result = reset_cron_task(&mut task);
        assert!(
            result,
            "reset_cron_task should return true for Done cron task"
        );
        assert_eq!(task.status, crate::graph::Status::Open);
        assert!(task.assigned.is_none(), "assigned should be cleared");
        assert!(
            task.completed_at.is_none(),
            "completed_at should be cleared"
        );
        assert!(
            task.last_cron_fire.is_some(),
            "last_cron_fire should be set"
        );
        assert!(
            task.next_cron_fire.is_some(),
            "next_cron_fire should be set"
        );
    }

    #[test]
    fn test_cron_reset_does_not_apply_to_non_cron_task() {
        let mut task = Task {
            id: "non-cron".to_string(),
            status: crate::graph::Status::Done,
            cron_enabled: false,
            ..Default::default()
        };
        assert!(!reset_cron_task(&mut task));
    }

    #[test]
    fn test_cron_reset_does_not_apply_to_non_done_task() {
        let mut task = Task {
            id: "cron-not-done".to_string(),
            status: crate::graph::Status::InProgress,
            cron_enabled: true,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            ..Default::default()
        };
        assert!(!reset_cron_task(&mut task));
    }

    #[test]
    fn test_jitter_is_deterministic_per_task_id() {
        let schedule = parse_cron_expression("0 0 2 * * *").unwrap(); // Daily at 2 AM
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        // Same task ID produces same jitter
        let jitter1 = calculate_jitter("my-cron-task", &schedule, from);
        let jitter2 = calculate_jitter("my-cron-task", &schedule, from);
        assert_eq!(jitter1, jitter2, "jitter should be deterministic");

        // Different task IDs produce (likely) different jitter
        let jitter_other = calculate_jitter("other-cron-task", &schedule, from);
        // Can't guarantee different, but with a daily schedule (86400s period, ±8640s jitter range)
        // two random hashes should differ. Let's at least check they're both within bounds.
        let period_secs = 86400i64; // daily
        let max_offset = (period_secs / 10).min(MAX_JITTER_SECS);
        assert!(jitter1.num_seconds().abs() <= max_offset);
        assert!(jitter_other.num_seconds().abs() <= max_offset);
    }

    #[test]
    fn test_jitter_bounded_by_max() {
        // Every-minute schedule: period = 60s, 10% = 6s, max = min(6, 900) = 6
        let schedule = parse_cron_expression("0 * * * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        let jitter = calculate_jitter("task-a", &schedule, from);
        assert!(
            jitter.num_seconds().abs() <= 6,
            "jitter should be ≤6s for minute schedule"
        );
    }

    #[test]
    fn test_calculate_next_fire_with_jitter_returns_value() {
        let schedule = parse_cron_expression("0 0 2 * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        let next = calculate_next_fire_with_jitter("test-task", &schedule, from);
        assert!(next.is_some());

        // The raw next fire is 2024-01-01 02:00:00. With jitter (±8640s for daily),
        // the result should be within that range.
        let next = next.unwrap();
        let raw_next = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();
        let diff = (next - raw_next).num_seconds().abs();
        assert!(
            diff <= MAX_JITTER_SECS,
            "jitter should be within MAX_JITTER_SECS"
        );
    }

    #[test]
    fn test_is_cron_due_with_next_cron_fire() {
        // When next_cron_fire is set, it takes priority over schedule-based checks
        let future_fire = Utc.with_ymd_and_hms(2024, 6, 15, 10, 0, 0).unwrap();
        let task = Task {
            id: "fire-test".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            next_cron_fire: Some(future_fire.to_rfc3339()),
            ..Default::default()
        };

        // Before the fire time → not due
        let before = Utc.with_ymd_and_hms(2024, 6, 15, 9, 59, 59).unwrap();
        assert!(!is_cron_due(&task, before));

        // At the fire time → due
        assert!(is_cron_due(&task, future_fire));

        // After the fire time → due
        let after = Utc.with_ymd_and_hms(2024, 6, 15, 10, 0, 1).unwrap();
        assert!(is_cron_due(&task, after));
    }

    #[test]
    fn test_5_field_cron_expression() {
        // 5-field format should be auto-converted to 6-field
        let result = parse_cron_expression("0 2 * * *"); // Daily at 2:00 AM
        assert!(result.is_ok());

        let schedule = result.unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();
        let next = calculate_next_fire(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap());
    }
}
