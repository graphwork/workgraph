//! Send task notifications to Matrix room
//!
//! This command allows agents to summon humans when blocked or need review
//! by sending nicely formatted task details to a Matrix room.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use workgraph::graph::{Status, Task};
use workgraph::parser::load_graph;
use workgraph::{MatrixClient, MatrixConfig};

use super::graph_path;

/// JSON output for notify command
#[derive(Debug, Serialize)]
struct NotifyResult {
    task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    room: Option<String>,
    sent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Helper to output errors consistently (JSON or plain text)
fn output_error(json: bool, task_id: &str, room: Option<&str>, error: &str) -> Result<()> {
    if json {
        let output = NotifyResult {
            task_id: task_id.to_string(),
            room: room.map(|r| r.to_string()),
            sent: false,
            error: Some(error.to_string()),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        Ok(())
    } else {
        anyhow::bail!("{}", error)
    }
}

pub fn run(
    dir: &Path,
    task_id: &str,
    room: Option<&str>,
    message: Option<&str>,
    json: bool,
) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        return output_error(json, task_id, None, "Workgraph not initialized. Run 'wg init' first.");
    }

    // Load task
    let graph = match load_graph(&path) {
        Ok(g) => g,
        Err(e) => return output_error(json, task_id, None, &format!("Failed to load graph: {}", e)),
    };
    let task = match graph.get_task(task_id) {
        Some(t) => t,
        None => return output_error(json, task_id, None, &format!("Task '{}' not found", task_id)),
    };

    // Load Matrix config
    let matrix_config = match MatrixConfig::load() {
        Ok(c) => c,
        Err(e) => return output_error(json, task_id, None, &format!("Failed to load Matrix config: {}", e)),
    };

    if !matrix_config.has_credentials() {
        return output_error(
            json,
            task_id,
            None,
            "Matrix not configured. Run 'wg config --matrix' to set up credentials. \
             Required: homeserver_url, username, and either password or access_token",
        );
    }

    // Determine room to send to
    let target_room = room.map(|r| r.to_string()).or(matrix_config.default_room.clone());
    let target_room = match target_room {
        Some(r) => r,
        None => {
            return output_error(
                json,
                task_id,
                None,
                "No room specified. Use --room or configure a default room with 'wg config --room <room>'",
            )
        }
    };

    // Build the notification message
    let (plain_text, html) = format_notification(task, message);

    // Send via Matrix
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create runtime")?;

    let result = rt.block_on(async {
        send_notification(dir, &matrix_config, &target_room, &plain_text, &html).await
    });

    match result {
        Ok(()) => {
            if json {
                let output = NotifyResult {
                    task_id: task_id.to_string(),
                    room: Some(target_room.clone()),
                    sent: true,
                    error: None,
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Notification sent to {}", target_room);
            }
            Ok(())
        }
        Err(e) => {
            if json {
                let output = NotifyResult {
                    task_id: task_id.to_string(),
                    room: Some(target_room),
                    sent: false,
                    error: Some(e.to_string()),
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

async fn send_notification(
    dir: &Path,
    config: &MatrixConfig,
    room: &str,
    plain_text: &str,
    html: &str,
) -> Result<()> {
    let client = MatrixClient::new(dir, config)
        .await
        .context("Failed to connect to Matrix")?;

    // Try to join the room first (in case we're not in it)
    let _ = client.join_room(room).await;

    // Send the formatted message
    client
        .send_html_message(room, plain_text, html)
        .await
        .context("Failed to send notification")?;

    Ok(())
}

/// Format the notification message for a task
/// Returns (plain_text, html) tuple
fn format_notification(task: &Task, custom_message: Option<&str>) -> (String, String) {
    let status_emoji = match task.status {
        Status::Open => "📋",
        Status::InProgress => "🔄",
        Status::Done => "✅",
        Status::Blocked => "🚫",
        Status::Failed => "❌",
        Status::Abandoned => "🗑️",
    };

    let status_str = format_status(&task.status);

    // Build plain text version
    let mut plain = String::new();

    // Custom message first if provided
    if let Some(msg) = custom_message {
        plain.push_str(msg);
        plain.push_str("\n\n");
    }

    plain.push_str(&format!("{} Task: {} ({})\n", status_emoji, task.title, task.id));
    plain.push_str(&format!("Status: {}\n", status_str));

    if let Some(ref desc) = task.description {
        plain.push_str(&format!("\nDescription:\n{}\n", desc));
    }

    if let Some(ref assigned) = task.assigned {
        plain.push_str(&format!("\nAssigned to: {}\n", assigned));
    }

    // Show blockers for blocked/failed tasks
    if !task.blocked_by.is_empty() {
        plain.push_str(&format!("\nBlocked by: {}\n", task.blocked_by.join(", ")));
    }

    if let Some(ref reason) = task.failure_reason {
        plain.push_str(&format!("\nFailure reason: {}\n", reason));
    }

    // Action hints
    plain.push_str("\n---\n");
    plain.push_str("Reply with: claim | done | input <info> | help\n");

    // Build HTML version
    let mut html = String::new();

    // Custom message first if provided
    if let Some(msg) = custom_message {
        html.push_str(&format!("<p><strong>{}</strong></p>", escape_html(msg)));
    }

    html.push_str(&format!(
        "<h4>{} {} <code>{}</code></h4>",
        status_emoji,
        escape_html(&task.title),
        escape_html(&task.id)
    ));

    html.push_str(&format!(
        "<p><strong>Status:</strong> {}</p>",
        status_str
    ));

    if let Some(ref desc) = task.description {
        html.push_str(&format!(
            "<p><strong>Description:</strong></p><blockquote>{}</blockquote>",
            escape_html(desc).replace('\n', "<br>")
        ));
    }

    if let Some(ref assigned) = task.assigned {
        html.push_str(&format!(
            "<p><strong>Assigned to:</strong> {}</p>",
            escape_html(assigned)
        ));
    }

    // Show blockers
    if !task.blocked_by.is_empty() {
        let blockers: Vec<String> = task
            .blocked_by
            .iter()
            .map(|b| format!("<code>{}</code>", escape_html(b)))
            .collect();
        html.push_str(&format!(
            "<p><strong>Blocked by:</strong> {}</p>",
            blockers.join(", ")
        ));
    }

    if let Some(ref reason) = task.failure_reason {
        html.push_str(&format!(
            "<p><strong>Failure reason:</strong> <em>{}</em></p>",
            escape_html(reason)
        ));
    }

    // Action hints
    html.push_str("<hr>");
    html.push_str("<p><em>Reply with:</em> <code>claim</code> | <code>done</code> | <code>input &lt;info&gt;</code> | <code>help</code></p>");

    (plain, html)
}

fn format_status(status: &Status) -> &'static str {
    match status {
        Status::Open => "open",
        Status::InProgress => "in-progress",
        Status::Done => "done",
        Status::Blocked => "blocked",
        Status::Failed => "failed",
        Status::Abandoned => "abandoned",
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::Task;

    fn make_test_task() -> Task {
        Task {
            id: "test-task".to_string(),
            title: "Test Task Title".to_string(),
            description: Some("This is a test description".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            estimate: None,
            blocks: vec![],
            blocked_by: vec!["blocker-1".to_string()],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
        }
    }

    #[test]
    fn test_format_notification_basic() {
        let task = make_test_task();
        let (plain, html) = format_notification(&task, None);

        assert!(plain.contains("Test Task Title"));
        assert!(plain.contains("test-task"));
        assert!(plain.contains("in-progress"));
        assert!(plain.contains("agent-1"));
        assert!(plain.contains("blocker-1"));

        assert!(html.contains("Test Task Title"));
        assert!(html.contains("<code>test-task</code>"));
    }

    #[test]
    fn test_format_notification_with_custom_message() {
        let task = make_test_task();
        let (plain, html) = format_notification(&task, Some("Need help with this!"));

        assert!(plain.starts_with("Need help with this!"));
        assert!(html.contains("Need help with this!"));
    }

    #[test]
    fn test_format_notification_failed_task() {
        let mut task = make_test_task();
        task.status = Status::Failed;
        task.failure_reason = Some("Build failed".to_string());

        let (plain, html) = format_notification(&task, None);

        assert!(plain.contains("❌"));
        assert!(plain.contains("Build failed"));
        assert!(html.contains("Build failed"));
    }

    #[test]
    fn test_escape_html() {
        assert_eq!(escape_html("<script>"), "&lt;script&gt;");
        assert_eq!(escape_html("a & b"), "a &amp; b");
        assert_eq!(escape_html("\"quoted\""), "&quot;quoted&quot;");
    }

    #[test]
    fn test_action_hints_included() {
        let task = make_test_task();
        let (plain, html) = format_notification(&task, None);

        assert!(plain.contains("claim"));
        assert!(plain.contains("done"));
        assert!(plain.contains("input"));
        assert!(html.contains("<code>claim</code>"));
    }
}
