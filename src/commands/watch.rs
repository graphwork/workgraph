use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

use workgraph::provenance;

#[derive(Debug, Serialize)]
pub struct WatchEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub data: serde_json::Value,
}

/// Map provenance operation name to watch event type.
fn op_to_event_type(op: &str) -> Option<&str> {
    match op {
        "add_task" => Some("task.created"),
        "claim" => Some("task.started"),
        "done" => Some("task.completed"),
        "fail" => Some("task.failed"),
        "retry" => Some("task.retried"),
        "evaluate_record" | "evaluate_auto" | "evaluate" => Some("evaluation.recorded"),
        "spawn_agent" => Some("agent.spawned"),
        "agent_complete" => Some("agent.completed"),
        _ => None,
    }
}

/// Map event type to its category for filtering.
fn event_category(event_type: &str) -> &str {
    if event_type.starts_with("task.") {
        "task_state"
    } else if event_type.starts_with("evaluation.") {
        "evaluation"
    } else if event_type.starts_with("agent.") {
        "agent"
    } else {
        "other"
    }
}

fn should_include_event(
    event_type: &str,
    event_filters: &HashSet<String>,
    task_filter: Option<&str>,
    task_id: Option<&str>,
) -> bool {
    // Check event type filter
    if !event_filters.contains("all") {
        let category = event_category(event_type);
        if !event_filters.contains(category) && !event_filters.contains(event_type) {
            return false;
        }
    }

    // Check task filter (prefix match)
    if let Some(filter) = task_filter {
        match task_id {
            Some(tid) => {
                if !tid.starts_with(filter) {
                    return false;
                }
            }
            None => return false,
        }
    }

    true
}

fn op_to_watch_event(op: &provenance::OperationEntry) -> Option<WatchEvent> {
    let event_type = op_to_event_type(&op.op)?;
    Some(WatchEvent {
        event_type: event_type.to_string(),
        timestamp: op.timestamp.clone(),
        task_id: op.task_id.clone(),
        data: op.detail.clone(),
    })
}

pub fn run(
    dir: &Path,
    event_types: &[String],
    task_filter: Option<&str>,
    replay: usize,
) -> Result<()> {
    let ops_path = provenance::operations_path(dir);

    let event_filters: HashSet<String> = event_types.iter().cloned().collect();

    // Historical replay
    if replay > 0 {
        let all_ops = provenance::read_all_operations(dir).unwrap_or_default();
        let start = all_ops.len().saturating_sub(replay);
        for op in &all_ops[start..] {
            if let Some(event) = op_to_watch_event(op)
                && should_include_event(
                    &event.event_type,
                    &event_filters,
                    task_filter,
                    event.task_id.as_deref(),
                ) {
                    let line = serde_json::to_string(&event)?;
                    println!("{}", line);
                }
        }
    }

    // Live streaming via polling
    let poll_ms: u64 = std::env::var("WG_WATCH_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    // Seek to end of current file
    let mut last_pos = if ops_path.exists() {
        std::fs::metadata(&ops_path)
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    };

    let stdout = std::io::stdout();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(poll_ms));

        if !ops_path.exists() {
            continue;
        }

        let current_size = match std::fs::metadata(&ops_path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };

        // Detect file rotation (size reset)
        if current_size < last_pos {
            last_pos = 0;
        }

        if current_size == last_pos {
            continue;
        }

        // Read new lines
        let file = match std::fs::File::open(&ops_path) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(last_pos)).is_err() {
            continue;
        }

        let mut new_pos = last_pos;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    new_pos += n as u64;
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(op) = serde_json::from_str::<provenance::OperationEntry>(trimmed)
                        && let Some(event) = op_to_watch_event(&op)
                            && should_include_event(
                                &event.event_type,
                                &event_filters,
                                task_filter,
                                event.task_id.as_deref(),
                            ) {
                                let json_line = serde_json::to_string(&event)?;
                                let mut out = stdout.lock();
                                if writeln!(out, "{}", json_line).is_err() {
                                    // Broken pipe - exit cleanly
                                    return Ok(());
                                }
                                let _ = out.flush();
                            }
                }
                Err(_) => break,
            }
        }
        last_pos = new_pos;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── op_to_event_type ──

    #[test]
    fn test_op_to_event_type_known_ops() {
        assert_eq!(op_to_event_type("add_task"), Some("task.created"));
        assert_eq!(op_to_event_type("claim"), Some("task.started"));
        assert_eq!(op_to_event_type("done"), Some("task.completed"));
        assert_eq!(op_to_event_type("fail"), Some("task.failed"));
        assert_eq!(op_to_event_type("retry"), Some("task.retried"));
        assert_eq!(op_to_event_type("evaluate_record"), Some("evaluation.recorded"));
        assert_eq!(op_to_event_type("evaluate_auto"), Some("evaluation.recorded"));
        assert_eq!(op_to_event_type("evaluate"), Some("evaluation.recorded"));
        assert_eq!(op_to_event_type("spawn_agent"), Some("agent.spawned"));
        assert_eq!(op_to_event_type("agent_complete"), Some("agent.completed"));
    }

    #[test]
    fn test_op_to_event_type_unknown_op() {
        assert_eq!(op_to_event_type("unknown_op"), None);
        assert_eq!(op_to_event_type(""), None);
    }

    // ── event_category ──

    #[test]
    fn test_event_category_task() {
        assert_eq!(event_category("task.created"), "task_state");
        assert_eq!(event_category("task.completed"), "task_state");
        assert_eq!(event_category("task.failed"), "task_state");
    }

    #[test]
    fn test_event_category_evaluation() {
        assert_eq!(event_category("evaluation.recorded"), "evaluation");
    }

    #[test]
    fn test_event_category_agent() {
        assert_eq!(event_category("agent.spawned"), "agent");
        assert_eq!(event_category("agent.completed"), "agent");
    }

    #[test]
    fn test_event_category_other() {
        assert_eq!(event_category("something.else"), "other");
        assert_eq!(event_category(""), "other");
    }

    // ── should_include_event ──

    #[test]
    fn test_should_include_event_all_filter() {
        let filters: HashSet<String> = ["all"].iter().map(|s| s.to_string()).collect();
        assert!(should_include_event("task.created", &filters, None, Some("t1")));
        assert!(should_include_event("agent.spawned", &filters, None, None));
    }

    #[test]
    fn test_should_include_event_category_filter() {
        let filters: HashSet<String> = ["task_state"].iter().map(|s| s.to_string()).collect();
        assert!(should_include_event("task.created", &filters, None, Some("t1")));
        assert!(!should_include_event("agent.spawned", &filters, None, None));
    }

    #[test]
    fn test_should_include_event_exact_type_filter() {
        let filters: HashSet<String> = ["task.created"].iter().map(|s| s.to_string()).collect();
        assert!(should_include_event("task.created", &filters, None, Some("t1")));
        assert!(!should_include_event("task.completed", &filters, None, Some("t1")));
    }

    #[test]
    fn test_should_include_event_task_prefix_filter() {
        let filters: HashSet<String> = ["all"].iter().map(|s| s.to_string()).collect();
        // Prefix match: "feat-" matches "feat-login"
        assert!(should_include_event("task.created", &filters, Some("feat-"), Some("feat-login")));
        // Prefix mismatch
        assert!(!should_include_event("task.created", &filters, Some("feat-"), Some("bug-fix")));
        // Task filter set but event has no task_id
        assert!(!should_include_event("task.created", &filters, Some("feat-"), None));
    }

    // ── op_to_watch_event ──

    #[test]
    fn test_op_to_watch_event_conversion() {
        let op = provenance::OperationEntry {
            timestamp: "2026-02-28T12:00:00Z".to_string(),
            op: "done".to_string(),
            task_id: Some("my-task".to_string()),
            actor: Some("agent-1".to_string()),
            detail: serde_json::json!({"reason": "completed"}),
        };
        let event = op_to_watch_event(&op).unwrap();
        assert_eq!(event.event_type, "task.completed");
        assert_eq!(event.timestamp, "2026-02-28T12:00:00Z");
        assert_eq!(event.task_id, Some("my-task".to_string()));
        assert_eq!(event.data, serde_json::json!({"reason": "completed"}));
    }

    #[test]
    fn test_op_to_watch_event_unknown_returns_none() {
        let op = provenance::OperationEntry {
            timestamp: "2026-02-28T12:00:00Z".to_string(),
            op: "unknown".to_string(),
            task_id: None,
            actor: None,
            detail: serde_json::Value::Null,
        };
        assert!(op_to_watch_event(&op).is_none());
    }

    // ── WatchEvent serialization ──

    #[test]
    fn test_watch_event_serialization_format() {
        let event = WatchEvent {
            event_type: "task.completed".to_string(),
            timestamp: "2026-02-28T12:00:00Z".to_string(),
            task_id: Some("my-task".to_string()),
            data: serde_json::json!({"key": "value"}),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // "type" field should be used instead of "event_type" (serde rename)
        assert_eq!(parsed["type"], "task.completed");
        assert_eq!(parsed["timestamp"], "2026-02-28T12:00:00Z");
        assert_eq!(parsed["task_id"], "my-task");
        assert_eq!(parsed["data"]["key"], "value");
    }

    #[test]
    fn test_watch_event_serialization_skips_none_task_id() {
        let event = WatchEvent {
            event_type: "agent.spawned".to_string(),
            timestamp: "2026-02-28T12:00:00Z".to_string(),
            task_id: None,
            data: serde_json::json!({}),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // skip_serializing_if means the key is absent entirely
        assert!(!parsed.as_object().unwrap().contains_key("task_id"));
    }
}
