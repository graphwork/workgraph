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
