pub mod agency;
pub mod check;
pub mod config;
pub mod cycle;
pub mod federation;
pub mod graph;
#[cfg(feature = "matrix")]
pub mod matrix;
pub mod matrix_commands;
#[cfg(feature = "matrix-lite")]
pub mod matrix_lite;
pub mod parser;
pub mod plan_validator;
pub mod provenance;
pub mod query;
pub mod runs;
pub mod service;
pub mod trace_function;
pub mod trace_memory;
pub mod usage;

pub use config::MatrixConfig;
pub use graph::WorkGraph;
#[cfg(feature = "matrix")]
pub use matrix::commands::{MatrixCommand, help_text as matrix_help_text};
#[cfg(feature = "matrix")]
pub use matrix::listener::{ListenerConfig, MatrixListener, run_listener};
#[cfg(feature = "matrix")]
pub use matrix::{IncomingMessage, MatrixClient, VerificationEvent};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::commands::{
    MatrixCommand as MatrixCommandLite, help_text as matrix_lite_help_text,
};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::listener::{
    ListenerConfig as ListenerConfigLite, MatrixListener as MatrixListenerLite,
    run_listener as run_listener_lite,
};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::{
    IncomingMessage as IncomingMessageLite, MatrixClient as MatrixClientLite, send_notification,
    send_notification_to_room,
};
pub use parser::{load_graph, save_graph};
pub use service::{AgentEntry, AgentRegistry, AgentStatus};

#[cfg(any(test, feature = "test-support"))]
pub mod test_helpers;

/// Format a duration in seconds to a human-readable string.
///
/// When `compact` is false, includes the next smaller unit if non-zero
/// (e.g., "1h 5m", "1d 2h", "30s").
/// When `compact` is true, shows only the largest unit
/// (e.g., "1h", "1d", "5m").
pub fn format_duration(secs: i64, compact: bool) -> String {
    if secs < 60 {
        return format!("{}s", secs);
    }
    if secs < 3600 {
        let mins = secs / 60;
        if compact {
            return format!("{}m", mins);
        }
        let s = secs % 60;
        if s > 0 {
            return format!("{}m {}s", mins, s);
        }
        return format!("{}m", mins);
    }
    if secs < 86400 {
        let hours = secs / 3600;
        if compact {
            return format!("{}h", hours);
        }
        let mins = (secs % 3600) / 60;
        if mins > 0 {
            return format!("{}h {}m", hours, mins);
        }
        return format!("{}h", hours);
    }
    let days = secs / 86400;
    if compact {
        return format!("{}d", days);
    }
    let hours = (secs % 86400) / 3600;
    if hours > 0 {
        format!("{}d {}h", days, hours)
    } else {
        format!("{}d", days)
    }
}

/// Format hours nicely (no decimals if whole number)
pub fn format_hours(hours: f64) -> String {
    if !hours.is_finite() {
        return "?".to_string();
    }
    if hours.fract() == 0.0 && hours >= i64::MIN as f64 && hours <= i64::MAX as f64 {
        format!("{}", hours as i64)
    } else {
        format!("{:.1}", hours)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_verbose() {
        assert_eq!(format_duration(30, false), "30s");
        assert_eq!(format_duration(90, false), "1m 30s");
        assert_eq!(format_duration(60, false), "1m");
        assert_eq!(format_duration(3600, false), "1h");
        assert_eq!(format_duration(3661, false), "1h 1m");
        assert_eq!(format_duration(86400, false), "1d");
        assert_eq!(format_duration(90000, false), "1d 1h");
    }

    #[test]
    fn test_format_duration_compact() {
        assert_eq!(format_duration(30, true), "30s");
        assert_eq!(format_duration(90, true), "1m");
        assert_eq!(format_duration(3600, true), "1h");
        assert_eq!(format_duration(3661, true), "1h");
        assert_eq!(format_duration(86400, true), "1d");
        assert_eq!(format_duration(90000, true), "1d");
    }

    #[test]
    fn test_format_duration_edge_cases() {
        assert_eq!(format_duration(0, false), "0s");
        assert_eq!(format_duration(59, false), "59s");
        assert_eq!(format_duration(60, false), "1m");
        assert_eq!(format_duration(119, false), "1m 59s");
        assert_eq!(format_duration(120, false), "2m");
        assert_eq!(format_duration(0, true), "0s");
    }
}
