//! `wg chat` command: send messages to the coordinator and receive responses.
//!
//! Supports single-message mode and interactive REPL mode.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use workgraph::chat;

use super::service;

/// Maximum message size (100KB) to prevent accidental pipe-of-entire-file.
const MAX_MESSAGE_SIZE: usize = 100 * 1024;

/// Default timeout waiting for coordinator response.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Generate a unique request ID for correlating requests with responses.
///
/// Format: `chat-{unix_millis}-{pid}{nanos_suffix}`
/// The timestamp prefix makes IDs naturally sortable and debuggable.
fn generate_request_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.as_millis();
    let nanos_suffix = now.subsec_nanos() % 100_000;
    let pid = std::process::id();
    format!("chat-{}-{}{:05}", millis, pid, nanos_suffix)
}

/// Send a single chat message and wait for a response.
pub fn run_send(dir: &Path, message: &str, timeout_secs: Option<u64>) -> Result<()> {
    // Validate message size
    if message.len() > MAX_MESSAGE_SIZE {
        eprintln!(
            "Warning: Message truncated to {}KB (was {}KB)",
            MAX_MESSAGE_SIZE / 1024,
            message.len() / 1024
        );
    }
    let msg = if message.len() > MAX_MESSAGE_SIZE {
        &message[..MAX_MESSAGE_SIZE]
    } else {
        message
    };

    if msg.trim().is_empty() {
        anyhow::bail!("Message cannot be empty");
    }

    let request_id = generate_request_id();
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    // Send UserChat IPC request to the daemon
    let ipc_response = service::send_request(
        dir,
        &service::IpcRequest::UserChat {
            message: msg.to_string(),
            request_id: request_id.clone(),
        },
    )
    .context("Failed to connect to service. Is it running? Start with: wg service start")?;

    if !ipc_response.ok {
        let err_msg = ipc_response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        anyhow::bail!("Chat failed: {}", err_msg);
    }

    // Wait for the coordinator's response (poll outbox)
    match chat::wait_for_response(dir, &request_id, timeout)? {
        Some(response) => {
            println!("{}", response.content);
        }
        None => {
            eprintln!(
                "Timeout: coordinator did not respond within {}s.",
                timeout.as_secs()
            );
            eprintln!(
                "Your message was stored in the inbox. The response will appear when the coordinator processes it."
            );
            eprintln!("Use 'wg chat --history' to view past messages.");
        }
    }

    Ok(())
}

/// Run interactive chat REPL.
pub fn run_interactive(dir: &Path, timeout_secs: Option<u64>) -> Result<()> {
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    // Verify service is running before entering the REPL
    let status_response = service::send_request(dir, &service::IpcRequest::Status);
    if status_response.is_err() {
        anyhow::bail!("Service not running. Start it with: wg service start");
    }

    eprintln!("Interactive chat with coordinator (Ctrl-C to exit)");
    eprintln!();

    let stdin = std::io::stdin();
    let mut input = String::new();

    loop {
        eprint!("you> ");
        // Flush stderr so prompt appears before input
        use std::io::Write;
        std::io::stderr().flush().ok();

        input.clear();
        match stdin.read_line(&mut input) {
            Ok(0) => {
                // EOF (Ctrl-D)
                eprintln!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("Input error: {}", e);
                break;
            }
        }

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Truncate if needed
        let msg = if trimmed.len() > MAX_MESSAGE_SIZE {
            eprintln!(
                "Warning: Message truncated to {}KB",
                MAX_MESSAGE_SIZE / 1024
            );
            &trimmed[..MAX_MESSAGE_SIZE]
        } else {
            trimmed
        };

        let request_id = generate_request_id();

        // Send IPC request
        let ipc_response = match service::send_request(
            dir,
            &service::IpcRequest::UserChat {
                message: msg.to_string(),
                request_id: request_id.clone(),
            },
        ) {
            Ok(resp) => resp,
            Err(e) => {
                eprintln!("Error: {}", e);
                eprintln!("Service may have stopped. Restart with: wg service start");
                break;
            }
        };

        if !ipc_response.ok {
            eprintln!(
                "Error: {}",
                ipc_response
                    .error
                    .unwrap_or_else(|| "Unknown error".to_string())
            );
            continue;
        }

        // Wait for response
        match chat::wait_for_response(dir, &request_id, timeout)? {
            Some(response) => {
                eprintln!();
                println!("coordinator> {}", response.content);
                eprintln!();
            }
            None => {
                eprintln!(
                    "Timeout: no response within {}s. Message was stored.",
                    timeout.as_secs()
                );
                eprintln!();
            }
        }
    }

    Ok(())
}

/// Display chat history (interleaved inbox + outbox by timestamp).
pub fn run_history(dir: &Path, json: bool) -> Result<()> {
    let history = chat::read_history(dir)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&history)?);
        return Ok(());
    }

    if history.is_empty() {
        println!("No chat history.");
        return Ok(());
    }

    for msg in &history {
        // Extract time portion from ISO timestamp for compact display
        let time = if let Some(t_pos) = msg.timestamp.find('T') {
            let time_part = &msg.timestamp[t_pos + 1..];
            // Take HH:MM:SS
            if time_part.len() >= 8 {
                &time_part[..8]
            } else {
                time_part
            }
        } else {
            &msg.timestamp
        };

        println!("[{}] {}: {}", time, msg.role, msg.content);
    }

    Ok(())
}

/// Clear all chat history.
pub fn run_clear(dir: &Path) -> Result<()> {
    chat::clear(dir)?;
    println!("Chat history cleared.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();
        (tmp, wg_dir)
    }

    #[test]
    fn test_generate_request_id_format() {
        let id = generate_request_id();
        assert!(
            id.starts_with("chat-"),
            "ID should start with 'chat-': {}",
            id
        );
        // Should contain the timestamp portion
        assert!(id.len() > 10, "ID should be non-trivial length: {}", id);
    }

    #[test]
    fn test_generate_request_id_unique() {
        let ids: Vec<String> = (0..100).map(|_| generate_request_id()).collect();
        let mut deduped = ids.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(ids.len(), deduped.len(), "Request IDs should be unique");
    }

    #[test]
    fn test_run_history_empty() {
        let (_tmp, dir) = setup();
        // Should not error on empty history
        run_history(&dir, false).unwrap();
        run_history(&dir, true).unwrap();
    }

    #[test]
    fn test_run_history_with_messages() {
        let (_tmp, dir) = setup();

        chat::append_inbox(&dir, "hello", "req-1").unwrap();
        chat::append_outbox(&dir, "hi there", "req-1").unwrap();

        // Should not error
        run_history(&dir, false).unwrap();
    }

    #[test]
    fn test_run_history_json() {
        let (_tmp, dir) = setup();

        chat::append_inbox(&dir, "hello", "req-1").unwrap();
        chat::append_outbox(&dir, "hi there", "req-1").unwrap();

        // Should not error
        run_history(&dir, true).unwrap();
    }

    #[test]
    fn test_run_clear() {
        let (_tmp, dir) = setup();

        chat::append_inbox(&dir, "msg", "r1").unwrap();
        chat::append_outbox(&dir, "resp", "r1").unwrap();

        run_clear(&dir).unwrap();

        let history = chat::read_history(&dir).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn test_send_empty_message_fails() {
        let (_tmp, dir) = setup();
        let result = run_send(&dir, "  ", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }
}
