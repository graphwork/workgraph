//! Telegram commands for workgraph CLI
//!
//! Provides commands for interacting with Telegram:
//! - `wg telegram listen` - Start the Telegram bot listener
//! - `wg telegram send` - Send a message to the configured chat
//! - `wg telegram status` - Show Telegram configuration status

use anyhow::{Context, Result};
use std::path::Path;

use workgraph::notify::NotificationChannel;
use workgraph::notify::config::NotifyConfig;
use workgraph::notify::telegram::{TelegramChannel, TelegramConfig};

/// Run the Telegram listener.
///
/// Starts a long-running process that polls for incoming messages via the
/// Telegram Bot API and dispatches workgraph commands.
pub fn run_listen(dir: &Path, chat_id: Option<&str>) -> Result<()> {
    let config = load_telegram_config()?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    println!("Starting Telegram listener...");
    println!(
        "Bot token: {}...{}",
        &config.bot_token[..6],
        &config.bot_token[config.bot_token.len().saturating_sub(4)..]
    );
    println!("Chat ID: {}", effective_chat_id);
    println!("Press Ctrl+C to stop\n");

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let channel = TelegramChannel::new(config);
        let mut rx = channel
            .listen()
            .await
            .context("Failed to start Telegram listener")?;

        let workgraph_dir = dir.to_path_buf();
        while let Some(msg) = rx.recv().await {
            // Try to parse as a command
            if let Some(cmd) = workgraph::telegram_commands::parse(&msg.body) {
                println!(
                    "[{}] Command from {}: {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    msg.sender,
                    cmd.description()
                );

                let response =
                    workgraph::telegram_commands::execute(&workgraph_dir, &cmd, &msg.sender);

                // Send response back
                if let Err(e) = channel.send_text(&effective_chat_id, &response).await {
                    eprintln!("Failed to send response: {e}");
                }
            } else if let Some(ref action_id) = msg.action_id {
                // Handle callback button presses
                println!(
                    "[{}] Button press from {}: {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    msg.sender,
                    action_id
                );

                // Action IDs follow the pattern "action:task_id" (e.g. "approve:my-task")
                let response = handle_action(&workgraph_dir, action_id, &msg.sender);

                if let Err(e) = channel.send_text(&effective_chat_id, &response).await {
                    eprintln!("Failed to send response: {e}");
                }
            } else {
                println!(
                    "[{}] Message from {}: {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    msg.sender,
                    msg.body
                );
            }
        }

        Ok(())
    })
}

/// Send a message to the configured Telegram chat.
pub fn run_send(chat_id: Option<&str>, message: &str) -> Result<()> {
    let config = load_telegram_config()?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let channel = TelegramChannel::new(config);
        channel
            .send_text(&effective_chat_id, message)
            .await
            .context("Failed to send message")?;
        println!("Message sent to chat {}", effective_chat_id);
        Ok(())
    })
}

/// Show Telegram configuration status.
pub fn run_status(json: bool) -> Result<()> {
    match load_telegram_config() {
        Ok(config) => {
            if json {
                let status = serde_json::json!({
                    "configured": true,
                    "chat_id": config.chat_id,
                    "bot_token_prefix": &config.bot_token[..config.bot_token.len().min(6)],
                });
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Telegram: configured");
                println!(
                    "  Bot token: {}...",
                    &config.bot_token[..config.bot_token.len().min(6)]
                );
                println!("  Chat ID: {}", config.chat_id);
            }
        }
        Err(_) => {
            if json {
                let status = serde_json::json!({ "configured": false });
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Telegram: not configured");
                println!("\nAdd a [telegram] section to your notify.toml:");
                println!("  ~/.config/workgraph/notify.toml");
                println!("  or .workgraph/notify.toml");
                println!();
                println!("  [telegram]");
                println!("  bot_token = \"123456:ABC-DEF...\"");
                println!("  chat_id = \"12345678\"");
            }
        }
    }
    Ok(())
}

/// Handle an action button callback.
fn handle_action(workgraph_dir: &Path, action_id: &str, sender: &str) -> String {
    let parts: Vec<&str> = action_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return format!("Unknown action: {action_id}");
    }

    let (action, task_id) = (parts[0], parts[1]);
    match action {
        "approve" | "claim" => {
            workgraph::matrix_commands::execute_claim(workgraph_dir, task_id, Some(sender))
        }
        "reject" | "fail" => workgraph::matrix_commands::execute_fail(
            workgraph_dir,
            task_id,
            Some("rejected via Telegram"),
        ),
        "done" => workgraph::matrix_commands::execute_done(workgraph_dir, task_id),
        _ => format!("Unknown action: {action}"),
    }
}

/// Poll for replies from the configured Telegram chat.
///
/// Calls the Telegram Bot API getUpdates endpoint and waits for replies
/// from the configured chat_id within the timeout period.
pub fn run_poll(chat_id: Option<&str>, timeout_seconds: u64) -> Result<()> {
    let config = load_telegram_config()?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    println!("Polling for messages from chat {}...", effective_chat_id);
    println!("Timeout: {} seconds", timeout_seconds);

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let channel = TelegramChannel::new(config);

        // Load last seen update_id
        let offset = load_last_update_id().unwrap_or(0);

        let start_time = std::time::Instant::now();
        let timeout_duration = std::time::Duration::from_secs(timeout_seconds);

        loop {
            if start_time.elapsed() >= timeout_duration {
                println!("Timeout reached - no new messages");
                return Ok(());
            }

            match poll_once(&channel, offset, &effective_chat_id, 10).await {
                Ok(Some((message, new_offset))) => {
                    // Save the new offset
                    if let Err(e) = save_last_update_id(new_offset) {
                        eprintln!("Warning: failed to save update_id: {}", e);
                    }

                    println!("Message from {}: {}", message.sender, message.body);
                    return Ok(());
                }
                Ok(None) => {
                    // No new messages, wait a bit before trying again
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(e) => {
                    eprintln!("Poll error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// Send a message and wait for reply.
///
/// Sends the message and polls for reply at intervals. Times out after
/// configurable max wait. Includes task ID context if provided.
pub fn run_ask(
    message: &str,
    chat_id: Option<&str>,
    timeout_seconds: u64,
    interval_seconds: u64,
    task_id: Option<&str>,
) -> Result<()> {
    let config = load_telegram_config()?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    // Format message with task context if provided
    let formatted_message = if let Some(task) = task_id {
        format!("[{}] Agent question: {}", task, message)
    } else {
        format!("Agent question: {}", message)
    };

    println!("Sending message and waiting for reply...");
    println!("Message: {}", formatted_message);
    println!(
        "Timeout: {} seconds, polling every {} seconds",
        timeout_seconds, interval_seconds
    );

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let channel = TelegramChannel::new(config);

        // Send the message first
        match channel
            .send_text(&effective_chat_id, &formatted_message)
            .await
        {
            Ok(msg_id) => {
                println!("Message sent (ID: {})", msg_id.0);
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to send message: {}", e));
            }
        }

        // Load last seen update_id
        let offset = load_last_update_id().unwrap_or(0);

        let start_time = std::time::Instant::now();
        let timeout_duration = std::time::Duration::from_secs(timeout_seconds);
        let interval_duration = std::time::Duration::from_secs(interval_seconds);

        loop {
            if start_time.elapsed() >= timeout_duration {
                println!("Timeout reached - no reply received");
                return Ok(());
            }

            match poll_once(&channel, offset, &effective_chat_id, 10).await {
                Ok(Some((message, new_offset))) => {
                    // Save the new offset
                    if let Err(e) = save_last_update_id(new_offset) {
                        eprintln!("Warning: failed to save update_id: {}", e);
                    }

                    println!("Reply from {}: {}", message.sender, message.body);
                    return Ok(());
                }
                Ok(None) => {
                    // No new messages, wait for the next polling interval
                    tokio::time::sleep(interval_duration).await;
                }
                Err(e) => {
                    eprintln!("Poll error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// Poll Telegram once for new messages from a specific chat.
/// Returns the first message and the new offset, or None if no messages.
async fn poll_once(
    channel: &TelegramChannel,
    offset: i64,
    target_chat_id: &str,
    timeout: u32,
) -> Result<Option<(workgraph::notify::IncomingMessage, i64)>> {
    let body = serde_json::json!({
        "offset": offset,
        "timeout": timeout,
        "allowed_updates": ["message", "callback_query"],
    });

    let resp = channel.api_call("getUpdates", &body).await?;

    let updates = resp
        .get("result")
        .and_then(|r| r.as_array())
        .context("Invalid response format")?;

    let mut new_offset = offset;

    for update in updates {
        if let Some(uid) = update.get("update_id").and_then(|u| u.as_i64()) {
            new_offset = uid + 1;
        }

        // Handle callback queries (button presses)
        if let Some(cb) = update.get("callback_query") {
            let chat_id = cb
                .get("message")
                .and_then(|m| m.get("chat"))
                .and_then(|c| c.get("id"))
                .and_then(|id| id.as_i64())
                .map(|id| id.to_string());

            if chat_id.as_deref() == Some(target_chat_id) {
                let sender = cb
                    .get("from")
                    .and_then(|f| f.get("username"))
                    .and_then(|u| u.as_str())
                    .or_else(|| {
                        cb.get("from")
                            .and_then(|f| f.get("id"))
                            .and_then(|i| i.as_i64())
                            .map(|_| "unknown")
                    })
                    .unwrap_or("unknown");

                let action_id = cb
                    .get("data")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();

                let reply_to = cb
                    .get("message")
                    .and_then(|m| m.get("message_id"))
                    .and_then(|m| m.as_i64())
                    .map(|mid| workgraph::notify::MessageId(mid.to_string()));

                let msg = workgraph::notify::IncomingMessage {
                    channel: "telegram".to_string(),
                    sender: sender.to_string(),
                    body: action_id.clone(),
                    action_id: Some(action_id),
                    reply_to,
                };

                return Ok(Some((msg, new_offset)));
            }
        }

        // Handle regular messages
        if let Some(message) = update.get("message") {
            let chat_id = message
                .get("chat")
                .and_then(|c| c.get("id"))
                .and_then(|id| id.as_i64())
                .map(|id| id.to_string());

            if chat_id.as_deref() == Some(target_chat_id) {
                let sender = message
                    .get("from")
                    .and_then(|f| f.get("username"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("unknown");

                let body = message
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();

                let reply_to = message
                    .get("reply_to_message")
                    .and_then(|r| r.get("message_id"))
                    .and_then(|m| m.as_i64())
                    .map(|mid| workgraph::notify::MessageId(mid.to_string()));

                let msg = workgraph::notify::IncomingMessage {
                    channel: "telegram".to_string(),
                    sender: sender.to_string(),
                    body,
                    action_id: None,
                    reply_to,
                };

                return Ok(Some((msg, new_offset)));
            }
        }
    }

    Ok(None)
}

/// Load the last seen update_id from state file.
fn load_last_update_id() -> Result<i64> {
    let state_file = get_state_file_path()?;
    let content = std::fs::read_to_string(state_file)?;
    let id: i64 = content
        .trim()
        .parse()
        .context("Invalid update_id format in state file")?;
    Ok(id)
}

/// Save the last seen update_id to state file.
fn save_last_update_id(update_id: i64) -> Result<()> {
    let state_file = get_state_file_path()?;

    // Ensure parent directory exists
    if let Some(parent) = state_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(state_file, update_id.to_string())?;
    Ok(())
}

/// Get the path to the update_id state file.
fn get_state_file_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(std::path::Path::new(&home)
        .join(".config")
        .join("workgraph")
        .join("telegram_update_id"))
}

/// Load Telegram config from notify.toml.
fn load_telegram_config() -> Result<TelegramConfig> {
    let notify_config = NotifyConfig::load(Some(Path::new(".")))
        .context("Failed to load notification config")?
        .context("No notify.toml found. Create one at ~/.config/workgraph/notify.toml")?;
    TelegramConfig::from_notify_config(&notify_config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_action_approve() {
        // We can't easily test without a graph, but we can verify parsing.
        let result = handle_action(Path::new("/nonexistent"), "approve:my-task", "testuser");
        assert!(result.contains("Error") || result.contains("Claimed"));
    }

    #[test]
    fn handle_action_unknown() {
        let result = handle_action(Path::new("/nonexistent"), "foobar:task", "testuser");
        assert!(result.contains("Unknown action"));
    }

    #[test]
    fn handle_action_malformed() {
        let result = handle_action(Path::new("/nonexistent"), "no-colon", "testuser");
        assert!(result.contains("Unknown action"));
    }
}
