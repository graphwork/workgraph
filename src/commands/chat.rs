//! `wg chat` command: send messages to the coordinator and receive responses.
//!
//! Supports single-message mode and interactive REPL mode.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use workgraph::chat::{self, Attachment};

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

/// Process --attachment flags: validate each file, copy to .workgraph/attachments/,
/// and return the list of Attachment structs.
fn process_attachments(dir: &Path, paths: &[String]) -> Result<Vec<Attachment>> {
    let mut attachments = Vec::new();
    for path_str in paths {
        let source = std::path::Path::new(path_str);
        let att = chat::store_attachment(dir, source)
            .with_context(|| format!("Failed to attach file: {}", path_str))?;
        eprintln!(
            "Attached: {} ({}, {} bytes)",
            att.path, att.mime_type, att.size_bytes
        );
        attachments.push(att);
    }
    Ok(attachments)
}

/// Send a single chat message and wait for a response.
pub fn run_send(
    dir: &Path,
    message: &str,
    timeout_secs: Option<u64>,
    attachment_paths: &[String],
    coordinator_id: u32,
) -> Result<()> {
    // Validate message size
    if message.len() > MAX_MESSAGE_SIZE {
        eprintln!(
            "Warning: Message truncated to {}KB (was {}KB)",
            MAX_MESSAGE_SIZE / 1024,
            message.len() / 1024
        );
    }
    let msg = if message.len() > MAX_MESSAGE_SIZE {
        &message[..message.floor_char_boundary(MAX_MESSAGE_SIZE)]
    } else {
        message
    };

    if msg.trim().is_empty() && attachment_paths.is_empty() {
        anyhow::bail!("Message cannot be empty");
    }

    // Process attachments
    let attachments = process_attachments(dir, attachment_paths)?;

    let request_id = generate_request_id();
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    // Build the message content, appending attachment references.
    let full_message = if attachments.is_empty() {
        msg.to_string()
    } else {
        let att_lines: Vec<String> = attachments
            .iter()
            .map(|a| {
                let filename = std::path::Path::new(&a.path)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(&a.path);
                format!("[Attached: {}]", filename)
            })
            .collect();
        if msg.trim().is_empty() {
            att_lines.join("\n")
        } else {
            format!("{}\n{}", msg, att_lines.join("\n"))
        }
    };

    // Send UserChat IPC request to the daemon
    let ipc_response = service::send_request(
        dir,
        &service::IpcRequest::UserChat {
            message: full_message,
            request_id: request_id.clone(),
            attachments: attachments.clone(),
            coordinator_id: Some(coordinator_id),
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
    match chat::wait_for_response_for(dir, coordinator_id, &request_id, timeout)? {
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
pub fn run_interactive(dir: &Path, timeout_secs: Option<u64>, coordinator_id: u32) -> Result<()> {
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
            &trimmed[..trimmed.floor_char_boundary(MAX_MESSAGE_SIZE)]
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
                attachments: vec![],
                coordinator_id: Some(coordinator_id),
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
        match chat::wait_for_response_for(dir, coordinator_id, &request_id, timeout)? {
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

/// Display chat history for a coordinator.
///
/// Resolution order:
/// 1. Check which executor backs this coordinator (per `coordinator.effective_executor`).
/// 2. Ask `vendor_history::locate` for the canonical transcript file.
///    Native → `chat/<ref>/conversation.jsonl`, claude → newest session
///    in `~/.claude/projects/<slug>/`, codex → newest rollout in
///    `~/.codex/sessions/…` whose `session_meta.payload.cwd` matches.
/// 3. Parse + print / emit JSON.
/// 4. If the vendor file doesn't exist yet (fresh session) fall back
///    to the legacy daemon-coordinator inbox/outbox so history written
///    before PTY mode existed still shows up.
pub fn run_history(
    dir: &Path,
    json: bool,
    coordinator_id: u32,
    history_depth: Option<usize>,
) -> Result<()> {
    let chat_ref = coordinator_id.to_string();
    let chat_dir = workgraph::chat::chat_dir_for_ref(dir, &chat_ref);
    let config = workgraph::config::Config::load_or_default(dir);
    let executor = config.coordinator.effective_executor();

    if let Some(hist) = workgraph::vendor_history::locate(&executor, dir, &chat_ref, &chat_dir) {
        let turns = workgraph::vendor_history::read_turns(&hist)?;
        print_vendor_turns(&turns, &executor, hist.path(), json, history_depth)?;
        return Ok(());
    }

    // Fallback: legacy daemon-coordinator inbox+outbox history. Kept
    // so pre-PTY workgraphs still render chat history without
    // surprises.
    let history = chat::read_history_for(dir, coordinator_id)?;

    if json {
        let to_serialize = match history_depth {
            Some(n) => history
                .into_iter()
                .rev()
                .take(n)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>(),
            None => history,
        };
        println!("{}", serde_json::to_string_pretty(&to_serialize)?);
        return Ok(());
    }

    if history.is_empty() {
        println!("No chat history.");
        return Ok(());
    }

    let display_msgs: &[_] = match history_depth {
        Some(n) => {
            let skip = history.len().saturating_sub(n);
            &history[skip..]
        }
        None => &history,
    };

    for msg in display_msgs {
        let time = if let Some(t_pos) = msg.timestamp.find('T') {
            let time_part = &msg.timestamp[t_pos + 1..];
            if time_part.len() >= 8 {
                &time_part[..8]
            } else {
                time_part
            }
        } else {
            &msg.timestamp
        };

        println!("[{}] {}: {}", time, msg.role, msg.content);
        for att in &msg.attachments {
            let filename = std::path::Path::new(&att.path)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(&att.path);
            println!("        [Attached: {} ({})]", filename, att.mime_type);
        }
    }

    Ok(())
}

fn print_vendor_turns(
    turns: &[workgraph::vendor_history::Turn],
    executor: &str,
    path: &std::path::Path,
    json: bool,
    history_depth: Option<usize>,
) -> Result<()> {
    let slice: &[_] = match history_depth {
        Some(n) => {
            let skip = turns.len().saturating_sub(n);
            &turns[skip..]
        }
        None => turns,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(slice)?);
        return Ok(());
    }

    if slice.is_empty() {
        println!("No chat history ({}).", executor);
        println!("  source: {}", path.display());
        return Ok(());
    }

    eprintln!("# executor: {}  source: {}", executor, path.display());
    for turn in slice {
        let time = turn
            .timestamp
            .as_deref()
            .and_then(|ts| ts.find('T').map(|i| &ts[i + 1..]))
            .map(|rest| if rest.len() >= 8 { &rest[..8] } else { rest })
            .unwrap_or("--:--:--");
        println!("[{}] {}: {}", time, turn.role, turn.text);
    }
    Ok(())
}

/// Clear chat history for a specific coordinator.
pub fn run_clear(dir: &Path, coordinator_id: u32) -> Result<()> {
    chat::clear_for(dir, coordinator_id)?;
    println!("Chat history cleared for coordinator {}.", coordinator_id);
    Ok(())
}

/// Force-rotate chat files to archive for a specific coordinator.
pub fn run_rotate(dir: &Path, coordinator_id: u32) -> Result<()> {
    let rotated_ipc = chat::force_rotate_for(dir, coordinator_id)?;
    let rotated_tui = chat::force_rotate_tui_history_for(dir, coordinator_id)?;

    if rotated_ipc || rotated_tui {
        println!(
            "Chat files rotated to archive for coordinator {}.",
            coordinator_id
        );
        let archives = chat::list_archives_for(dir, coordinator_id)?;
        println!("{} archived file(s) total.", archives.len());
    } else {
        println!(
            "No chat files to rotate for coordinator {}.",
            coordinator_id
        );
    }

    // Also run retention cleanup
    let cleaned = chat::cleanup_archives_for(dir, coordinator_id)?;
    if cleaned > 0 {
        println!("Cleaned up {} expired archive(s).", cleaned);
    }

    Ok(())
}

/// Clean up expired archived chat files for a specific coordinator.
pub fn run_cleanup(dir: &Path, coordinator_id: u32) -> Result<()> {
    let cleaned = chat::cleanup_archives_for(dir, coordinator_id)?;
    if cleaned > 0 {
        println!(
            "Cleaned up {} expired archive(s) for coordinator {}.",
            cleaned, coordinator_id
        );
    } else {
        println!(
            "No expired archives to clean up for coordinator {}.",
            coordinator_id
        );
    }
    Ok(())
}

/// Compact chat history into a context summary for a specific coordinator.
/// Share context from one coordinator to another.
///
/// Reads the source coordinator's compacted context summary and writes it
/// as clearly-labeled imported context for the target coordinator.
pub fn run_share(dir: &Path, from_id: u32, to_id: u32) -> Result<()> {
    if from_id == to_id {
        anyhow::bail!(
            "Source and target coordinator must be different (both are {})",
            from_id
        );
    }

    // Look up coordinator label from graph
    let graph_path = dir.join("graph.jsonl");
    let from_label = if graph_path.exists() {
        let graph = workgraph::parser::load_graph(&graph_path)?;
        coordinator_label_from_graph(&graph, from_id)
    } else {
        None
    };
    let label_str = from_label.as_deref().unwrap_or("Unknown");

    let content = chat::share_context(dir, from_id, to_id, Some(label_str))?;

    eprintln!(
        "Shared context from coordinator {} ({}) → coordinator {}",
        from_id, label_str, to_id
    );
    eprintln!("({} bytes of imported context)", content.len());
    eprintln!("The target coordinator will consume this on its next turn.");

    Ok(())
}

/// Resolve coordinator label from the graph.
fn coordinator_label_from_graph(graph: &workgraph::graph::WorkGraph, cid: u32) -> Option<String> {
    let task_id = if cid == 0 {
        ".coordinator".to_string()
    } else {
        format!(".coordinator-{}", cid)
    };
    graph.get_task(&task_id).map(|t| t.title.clone())
}

pub fn run_compact(dir: &Path, coordinator_id: u32, json: bool) -> Result<()> {
    use workgraph::service::chat_compactor;

    let output_path = chat_compactor::run_chat_compaction(dir, coordinator_id)?;

    if json {
        let result = serde_json::json!({
            "path": output_path.display().to_string(),
            "coordinator_id": coordinator_id,
            "status": "ok",
        });
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Chat compacted → {}", output_path.display());
    }

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
        run_history(&dir, false, 0, None).unwrap();
        run_history(&dir, true, 0, None).unwrap();
    }

    #[test]
    fn test_run_history_with_messages() {
        let (_tmp, dir) = setup();

        chat::append_inbox(&dir, "hello", "req-1").unwrap();
        chat::append_outbox(&dir, "hi there", "req-1").unwrap();

        // Should not error
        run_history(&dir, false, 0, None).unwrap();
    }

    #[test]
    fn test_run_history_json() {
        let (_tmp, dir) = setup();

        chat::append_inbox(&dir, "hello", "req-1").unwrap();
        chat::append_outbox(&dir, "hi there", "req-1").unwrap();

        // Should not error
        run_history(&dir, true, 0, None).unwrap();
    }

    #[test]
    fn test_run_clear() {
        let (_tmp, dir) = setup();

        chat::append_inbox(&dir, "msg", "r1").unwrap();
        chat::append_outbox(&dir, "resp", "r1").unwrap();

        run_clear(&dir, 0).unwrap();

        let history = chat::read_history(&dir).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn test_send_empty_message_fails() {
        let (_tmp, dir) = setup();
        let result = run_send(&dir, "  ", None, &[], 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }
}
