use crate::graph::Task;
use chrono::{DateTime, Utc};
use cron::Schedule;
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
            return Err(CronError::InvalidExpression(
                format!("Expected 5 or 6 fields, got {}", parts.len())
            ));
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
        let result = parse_cron_expression("0 0 2 * * *");  // Daily at 2 AM
        if result.is_err() {
            println!("Debug: Error parsing '0 0 2 * * *': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 */5 * * * *");  // Every 5 minutes
        if result.is_err() {
            println!("Debug: Error parsing '0 */5 * * * *': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 0 12 * * 1-5");  // Weekdays at noon
        if result.is_err() {
            println!("Debug: Error parsing '0 0 12 * * 1-5': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 30 14 1 * *");  // 2:30 PM on 1st day of month
        if result.is_err() {
            println!("Debug: Error parsing '0 30 14 1 * *': {:?}", result);
        }
        assert!(result.is_ok());

        // Test invalid expressions
        assert!(parse_cron_expression("invalid").is_err());
        assert!(parse_cron_expression("").is_err());
        assert!(parse_cron_expression("60 25 32 13 8 8").is_err()); // Invalid values
    }
}