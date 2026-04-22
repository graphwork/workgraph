// Pre-existing clippy lints surfaced by rust 1.95 that weren't in
// 1.93. Allowed crate-wide while we decide whether to refactor each
// site individually. Not caused by the sessions-as-identity rollout
// work; CI was red before Phase 1 started.
#![allow(clippy::while_let_loop)]
#![allow(clippy::manual_div_ceil)]
#![allow(clippy::manual_checked_ops)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::unnecessary_sort_by)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]

pub mod agency;
pub mod chat;
pub mod chat_sessions;
pub mod check;
pub mod config;
pub mod context_scope;
pub mod cron;
pub mod cycle;
pub mod executor;
pub mod executor_discovery;
pub mod federation;
pub mod function;
pub mod function_memory;
pub mod graph;
pub mod json_extract;
pub mod markdown;
#[cfg(feature = "matrix")]
pub mod matrix;
pub mod matrix_commands;
#[cfg(feature = "matrix-lite")]
pub mod matrix_lite;
pub mod messages;
pub mod metrics;
pub mod model_benchmarks;
pub mod models;
pub mod notify;
pub mod parser;
pub mod plan_validator;
pub mod profile;
pub mod provenance;
pub mod query;
pub mod registry {
    pub use crate::service::registry::AgentRegistry as Registry;
    pub use crate::service::registry::AgentEntry as Agent;
    pub use crate::service::registry::AgentStatus;
}
pub mod runs;
pub mod service;
pub mod session_lock;
pub mod stream_event;
pub mod syntect_convert;
pub mod telegram_commands;
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
pub use parser::{load_graph, modify_graph, save_graph};
pub use service::{AgentEntry, AgentRegistry, AgentStatus};

#[cfg(any(test, feature = "test-support"))]
pub mod test_helpers;

/// Return the current user identity.
///
/// Fallback chain: `WG_USER` env var → `USER` env var → `"unknown"`.
pub fn current_user() -> String {
    std::env::var("WG_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string())
}

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
        if days >= 365 {
            return format!("{}y", days / 365);
        }
        if days >= 30 {
            return format!("{}mo", days / 30);
        }
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
        // months and years
        assert_eq!(format_duration(86400 * 29, true), "29d");
        assert_eq!(format_duration(86400 * 30, true), "1mo");
        assert_eq!(format_duration(86400 * 60, true), "2mo");
        assert_eq!(format_duration(86400 * 364, true), "12mo");
        assert_eq!(format_duration(86400 * 365, true), "1y");
        assert_eq!(format_duration(86400 * 730, true), "2y");
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

    // Mutex to serialize tests that mutate env vars (process-global state).
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_current_user_returns_wg_user_when_set() {
        let _lock = ENV_MUTEX.lock().unwrap();
        unsafe {
            let orig_wg = std::env::var("WG_USER").ok();
            let orig_user = std::env::var("USER").ok();

            std::env::set_var("WG_USER", "alice");
            assert_eq!(current_user(), "alice");

            // Restore
            match orig_wg {
                Some(v) => std::env::set_var("WG_USER", v),
                None => std::env::remove_var("WG_USER"),
            }
            match orig_user {
                Some(v) => std::env::set_var("USER", v),
                None => std::env::remove_var("USER"),
            }
        }
    }

    #[test]
    fn test_current_user_falls_back_to_user_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        unsafe {
            let orig_wg = std::env::var("WG_USER").ok();
            let orig_user = std::env::var("USER").ok();

            std::env::remove_var("WG_USER");
            std::env::set_var("USER", "bob");
            assert_eq!(current_user(), "bob");

            // Restore
            match orig_wg {
                Some(v) => std::env::set_var("WG_USER", v),
                None => std::env::remove_var("WG_USER"),
            }
            match orig_user {
                Some(v) => std::env::set_var("USER", v),
                None => std::env::remove_var("USER"),
            }
        }
    }

    #[test]
    fn test_current_user_returns_unknown_when_neither_set() {
        let _lock = ENV_MUTEX.lock().unwrap();
        unsafe {
            let orig_wg = std::env::var("WG_USER").ok();
            let orig_user = std::env::var("USER").ok();

            std::env::remove_var("WG_USER");
            std::env::remove_var("USER");
            assert_eq!(current_user(), "unknown");

            // Restore
            match orig_wg {
                Some(v) => std::env::set_var("WG_USER", v),
                None => {}
            }
            match orig_user {
                Some(v) => std::env::set_var("USER", v),
                None => {}
            }
        }
    }
}
