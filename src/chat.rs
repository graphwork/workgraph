//! Chat inbox/outbox storage for user↔coordinator communication.
//!
//! Messages are stored as JSONL files in `.workgraph/chat/{coordinator_id}/`:
//! - `inbox.jsonl`  — user → coordinator messages
//! - `outbox.jsonl` — coordinator → user responses
//! - `.cursor`      — CLI/TUI read cursor (last-read outbox message ID)
//! - `.coordinator-cursor` — coordinator read cursor (last-processed inbox message ID)
//!
//! Each coordinator gets its own subdirectory for isolated chat channels.
//! Coordinator 0 is the default; backward-compatible API functions use coordinator 0.
//!
//! Follows the same concurrency model as `src/messages.rs`:
//! writers use `O_APPEND` + `flock()` for safe ID assignment,
//! readers are lock-free.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A file attachment on a chat message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    /// Path relative to the workgraph root (e.g. ".workgraph/attachments/20260303-143022-a1b2c3.png")
    pub path: String,
    /// MIME type (e.g. "image/png")
    pub mime_type: String,
    /// File size in bytes
    pub size_bytes: u64,
}

/// A single chat message between user and coordinator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    /// Monotonically increasing ID within the file (inbox and outbox have separate sequences)
    pub id: u64,
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// "user" or "coordinator"
    pub role: String,
    /// Message content (free-form text, may contain markdown).
    /// For coordinator messages this is the summary (last text block).
    pub content: String,
    /// Correlates a user request with the coordinator's response.
    pub request_id: String,
    /// Optional file attachments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Full response text including tool calls and their outputs (coordinator messages only).
    /// When present, the UI can show this in an expanded view instead of just `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_response: Option<String>,
}

/// Directory for chat files for a specific coordinator.
fn chat_dir_for(workgraph_dir: &Path, coordinator_id: u32) -> PathBuf {
    workgraph_dir.join("chat").join(coordinator_id.to_string())
}

/// Directory for chat files (backward compat: coordinator 0).
#[cfg(test)]
fn chat_dir(workgraph_dir: &Path) -> PathBuf {
    chat_dir_for(workgraph_dir, 0)
}

/// Path to the inbox JSONL file for a specific coordinator.
fn inbox_path_for(workgraph_dir: &Path, coordinator_id: u32) -> PathBuf {
    chat_dir_for(workgraph_dir, coordinator_id).join("inbox.jsonl")
}

/// Path to the inbox JSONL file (coordinator 0).
#[cfg(test)]
fn inbox_path(workgraph_dir: &Path) -> PathBuf {
    inbox_path_for(workgraph_dir, 0)
}

/// Path to the outbox JSONL file for a specific coordinator.
fn outbox_path_for(workgraph_dir: &Path, coordinator_id: u32) -> PathBuf {
    chat_dir_for(workgraph_dir, coordinator_id).join("outbox.jsonl")
}

/// Path to the outbox JSONL file (coordinator 0).
#[allow(dead_code)]
fn outbox_path(workgraph_dir: &Path) -> PathBuf {
    outbox_path_for(workgraph_dir, 0)
}

/// Path to the CLI/TUI cursor file for a specific coordinator.
fn cursor_path_for(workgraph_dir: &Path, coordinator_id: u32) -> PathBuf {
    chat_dir_for(workgraph_dir, coordinator_id).join(".cursor")
}

/// Path to the CLI/TUI cursor file (coordinator 0).
#[allow(dead_code)]
fn cursor_path(workgraph_dir: &Path) -> PathBuf {
    cursor_path_for(workgraph_dir, 0)
}

/// Path to the coordinator cursor file for a specific coordinator.
fn coordinator_cursor_path_for(workgraph_dir: &Path, coordinator_id: u32) -> PathBuf {
    chat_dir_for(workgraph_dir, coordinator_id).join(".coordinator-cursor")
}

/// Path to the coordinator cursor file (coordinator 0).
#[allow(dead_code)]
fn coordinator_cursor_path(workgraph_dir: &Path) -> PathBuf {
    coordinator_cursor_path_for(workgraph_dir, 0)
}

/// Append a message to a JSONL file with flock-based ID assignment.
///
/// Opens the file with O_APPEND, acquires an exclusive lock,
/// reads the current max ID, assigns the next ID, and appends.
fn append_message(
    path: &Path,
    role: &str,
    content: &str,
    request_id: &str,
    attachments: Vec<Attachment>,
    full_response: Option<String>,
) -> Result<u64> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create chat directory: {}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .with_context(|| format!("Failed to open chat file: {}", path.display()))?;

    // Lock the file exclusively for ID assignment + append
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret != 0 {
            anyhow::bail!(
                "Failed to acquire lock on chat file: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    // Read existing messages to find the max ID
    let max_id = {
        let reader = BufReader::new(&file);
        let mut max = 0u64;
        for line in reader.lines() {
            let line = line.context("Failed to read chat file line")?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<ChatMessage>(&line)
                && msg.id > max
            {
                max = msg.id;
            }
        }
        max
    };

    let next_id = max_id + 1;
    let msg = ChatMessage {
        id: next_id,
        timestamp: Utc::now().to_rfc3339(),
        role: role.to_string(),
        content: content.to_string(),
        request_id: request_id.to_string(),
        attachments,
        full_response,
    };

    let mut json = serde_json::to_string(&msg).context("Failed to serialize chat message")?;
    json.push('\n');

    let mut file_ref = &file;
    file_ref
        .write_all(json.as_bytes())
        .with_context(|| format!("Failed to write to chat file: {}", path.display()))?;

    // Lock is released when file is dropped
    Ok(next_id)
}

/// Read all messages from a JSONL file.
fn read_messages(path: &Path) -> Result<Vec<ChatMessage>> {
    if !path.exists() {
        return Ok(vec![]);
    }

    let file = fs::File::open(path)
        .with_context(|| format!("Failed to open chat file: {}", path.display()))?;

    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = line.context("Failed to read chat file line")?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: ChatMessage = serde_json::from_str(&line)
            .with_context(|| format!("Failed to parse chat message: {}", line))?;
        messages.push(msg);
    }

    messages.sort_by_key(|m| m.id);
    Ok(messages)
}

/// Read a cursor value from a file. Returns 0 if the file doesn't exist.
fn read_cursor_file(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read cursor file: {}", path.display()))?;

    content.trim().parse::<u64>().with_context(|| {
        format!(
            "Invalid cursor value in {}: '{}'",
            path.display(),
            content.trim()
        )
    })
}

/// Write a cursor value atomically (write-to-temp + rename).
fn write_cursor_file(path: &Path, cursor: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cursor directory: {}", parent.display()))?;
    }

    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, format!("{}\n", cursor))
        .with_context(|| format!("Failed to write temp cursor file: {}", tmp_path.display()))?;

    fs::rename(&tmp_path, path)
        .with_context(|| format!("Failed to rename cursor file: {}", path.display()))?;

    Ok(())
}

// --- Public API: coordinator_id-aware versions ---

/// Append a user message to a specific coordinator's inbox.
pub fn append_inbox_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    content: &str,
    request_id: &str,
) -> Result<u64> {
    let path = inbox_path_for(workgraph_dir, coordinator_id);
    append_message(&path, "user", content, request_id, vec![], None)
}

/// Append a user message with attachments to a specific coordinator's inbox.
pub fn append_inbox_with_attachments_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    content: &str,
    request_id: &str,
    attachments: Vec<Attachment>,
) -> Result<u64> {
    let path = inbox_path_for(workgraph_dir, coordinator_id);
    append_message(&path, "user", content, request_id, attachments, None)
}

/// Append a coordinator response to a specific coordinator's outbox.
pub fn append_outbox_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    content: &str,
    request_id: &str,
) -> Result<u64> {
    let path = outbox_path_for(workgraph_dir, coordinator_id);
    append_message(&path, "coordinator", content, request_id, vec![], None)
}

/// Append a coordinator response with full response text to a specific coordinator's outbox.
pub fn append_outbox_full_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    content: &str,
    full_response: Option<String>,
    request_id: &str,
) -> Result<u64> {
    let path = outbox_path_for(workgraph_dir, coordinator_id);
    append_message(
        &path,
        "coordinator",
        content,
        request_id,
        vec![],
        full_response,
    )
}

/// Read all inbox messages for a specific coordinator.
pub fn read_inbox_for(workgraph_dir: &Path, coordinator_id: u32) -> Result<Vec<ChatMessage>> {
    read_messages(&inbox_path_for(workgraph_dir, coordinator_id))
}

/// Read inbox messages with ID > cursor for a specific coordinator.
pub fn read_inbox_since_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    cursor: u64,
) -> Result<Vec<ChatMessage>> {
    let all = read_messages(&inbox_path_for(workgraph_dir, coordinator_id))?;
    Ok(all.into_iter().filter(|m| m.id > cursor).collect())
}

/// Read outbox messages with ID > cursor for a specific coordinator.
pub fn read_outbox_since_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    cursor: u64,
) -> Result<Vec<ChatMessage>> {
    let all = read_messages(&outbox_path_for(workgraph_dir, coordinator_id))?;
    Ok(all.into_iter().filter(|m| m.id > cursor).collect())
}

/// Block until a response appears in a specific coordinator's outbox, or timeout.
pub fn wait_for_response_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    request_id: &str,
    timeout: Duration,
) -> Result<Option<ChatMessage>> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(200);

    loop {
        let messages = read_messages(&outbox_path_for(workgraph_dir, coordinator_id))?;
        if let Some(msg) = messages.into_iter().find(|m| m.request_id == request_id) {
            return Ok(Some(msg));
        }

        if start.elapsed() >= timeout {
            return Ok(None);
        }

        std::thread::sleep(poll_interval);
    }
}

/// Read the CLI/TUI cursor for a specific coordinator.
pub fn read_cursor_for(workgraph_dir: &Path, coordinator_id: u32) -> Result<u64> {
    read_cursor_file(&cursor_path_for(workgraph_dir, coordinator_id))
}

/// Write the CLI/TUI cursor for a specific coordinator.
pub fn write_cursor_for(workgraph_dir: &Path, coordinator_id: u32, cursor: u64) -> Result<()> {
    write_cursor_file(&cursor_path_for(workgraph_dir, coordinator_id), cursor)
}

/// Read and advance the CLI/TUI cursor for a specific coordinator.
pub fn read_and_advance_cursor_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
) -> Result<(u64, Vec<ChatMessage>)> {
    let old_cursor = read_cursor_for(workgraph_dir, coordinator_id)?;
    let new_messages = read_outbox_since_for(workgraph_dir, coordinator_id, old_cursor)?;

    let new_cursor = new_messages.last().map(|m| m.id).unwrap_or(old_cursor);
    if new_cursor > old_cursor {
        write_cursor_for(workgraph_dir, coordinator_id, new_cursor)?;
    }

    Ok((new_cursor, new_messages))
}

/// Read the coordinator cursor for a specific coordinator.
pub fn read_coordinator_cursor_for(workgraph_dir: &Path, coordinator_id: u32) -> Result<u64> {
    read_cursor_file(&coordinator_cursor_path_for(workgraph_dir, coordinator_id))
}

/// Write the coordinator cursor for a specific coordinator.
pub fn write_coordinator_cursor_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    cursor: u64,
) -> Result<()> {
    write_cursor_file(
        &coordinator_cursor_path_for(workgraph_dir, coordinator_id),
        cursor,
    )
}

/// Read all inbox and outbox messages interleaved by timestamp for a specific coordinator.
pub fn read_history_for(workgraph_dir: &Path, coordinator_id: u32) -> Result<Vec<ChatMessage>> {
    let mut all = read_messages(&inbox_path_for(workgraph_dir, coordinator_id))?;
    all.extend(read_messages(&outbox_path_for(workgraph_dir, coordinator_id))?);
    all.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    Ok(all)
}

/// Rotate old chat history for a specific coordinator.
pub fn rotate_history_for(
    workgraph_dir: &Path,
    coordinator_id: u32,
    keep_count: usize,
) -> Result<()> {
    rotate_file(&inbox_path_for(workgraph_dir, coordinator_id), keep_count)?;
    rotate_file(&outbox_path_for(workgraph_dir, coordinator_id), keep_count)?;
    Ok(())
}

// --- Public API: backward-compatible wrappers (coordinator 0) ---

/// Append a user message to the inbox (coordinator 0).
pub fn append_inbox(workgraph_dir: &Path, content: &str, request_id: &str) -> Result<u64> {
    append_inbox_for(workgraph_dir, 0, content, request_id)
}

/// Append a user message with attachments to the inbox (coordinator 0).
pub fn append_inbox_with_attachments(
    workgraph_dir: &Path,
    content: &str,
    request_id: &str,
    attachments: Vec<Attachment>,
) -> Result<u64> {
    append_inbox_with_attachments_for(workgraph_dir, 0, content, request_id, attachments)
}

/// Append a coordinator response to the outbox (coordinator 0).
pub fn append_outbox(workgraph_dir: &Path, content: &str, request_id: &str) -> Result<u64> {
    append_outbox_for(workgraph_dir, 0, content, request_id)
}

/// Append a coordinator response with full response text to the outbox (coordinator 0).
pub fn append_outbox_full(
    workgraph_dir: &Path,
    content: &str,
    full_response: Option<String>,
    request_id: &str,
) -> Result<u64> {
    append_outbox_full_for(workgraph_dir, 0, content, full_response, request_id)
}

/// Read all inbox messages (coordinator 0).
pub fn read_inbox(workgraph_dir: &Path) -> Result<Vec<ChatMessage>> {
    read_inbox_for(workgraph_dir, 0)
}

/// Read inbox messages with ID > cursor (coordinator 0).
pub fn read_inbox_since(workgraph_dir: &Path, cursor: u64) -> Result<Vec<ChatMessage>> {
    read_inbox_since_for(workgraph_dir, 0, cursor)
}

/// Read outbox messages with ID > cursor (coordinator 0).
pub fn read_outbox_since(workgraph_dir: &Path, cursor: u64) -> Result<Vec<ChatMessage>> {
    read_outbox_since_for(workgraph_dir, 0, cursor)
}

/// Block until a response with the given request_id appears in the outbox (coordinator 0).
pub fn wait_for_response(
    workgraph_dir: &Path,
    request_id: &str,
    timeout: Duration,
) -> Result<Option<ChatMessage>> {
    wait_for_response_for(workgraph_dir, 0, request_id, timeout)
}

/// Read the CLI/TUI cursor (coordinator 0).
pub fn read_cursor(workgraph_dir: &Path) -> Result<u64> {
    read_cursor_for(workgraph_dir, 0)
}

/// Write the CLI/TUI cursor (coordinator 0).
pub fn write_cursor(workgraph_dir: &Path, cursor: u64) -> Result<()> {
    write_cursor_for(workgraph_dir, 0, cursor)
}

/// Read and advance the CLI/TUI cursor (coordinator 0).
pub fn read_and_advance_cursor(workgraph_dir: &Path) -> Result<(u64, Vec<ChatMessage>)> {
    read_and_advance_cursor_for(workgraph_dir, 0)
}

/// Read the coordinator cursor (coordinator 0).
pub fn read_coordinator_cursor(workgraph_dir: &Path) -> Result<u64> {
    read_coordinator_cursor_for(workgraph_dir, 0)
}

/// Write the coordinator cursor (coordinator 0).
pub fn write_coordinator_cursor(workgraph_dir: &Path, cursor: u64) -> Result<()> {
    write_coordinator_cursor_for(workgraph_dir, 0, cursor)
}

/// Read all inbox and outbox messages interleaved by timestamp (coordinator 0).
pub fn read_history(workgraph_dir: &Path) -> Result<Vec<ChatMessage>> {
    read_history_for(workgraph_dir, 0)
}

/// Rotate old chat history (coordinator 0).
pub fn rotate_history(workgraph_dir: &Path, keep_count: usize) -> Result<()> {
    rotate_history_for(workgraph_dir, 0, keep_count)
}

/// Rotate a single JSONL file, keeping only the last `keep_count` messages.
fn rotate_file(path: &Path, keep_count: usize) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let messages = read_messages(path)?;
    if messages.len() <= keep_count {
        return Ok(());
    }

    // Keep only the last N messages
    let to_keep = &messages[messages.len() - keep_count..];

    // Write to temp file and atomically rename
    let tmp = path.with_extension("rotate-tmp");
    {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("Failed to create rotation temp file: {}", tmp.display()))?;
        for msg in to_keep {
            let mut json = serde_json::to_string(msg)
                .context("Failed to serialize message during rotation")?;
            json.push('\n');
            file.write_all(json.as_bytes()).with_context(|| {
                format!("Failed to write to rotation temp file: {}", tmp.display())
            })?;
        }
    }

    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename rotated file: {}", path.display()))?;

    Ok(())
}

// --- Attachment support ---

/// Directory for storing attached files.
fn attachments_dir(workgraph_dir: &Path) -> PathBuf {
    workgraph_dir.join("attachments")
}

/// Known image extensions and their MIME types.
fn mime_for_extension(ext: &str) -> Option<&'static str> {
    match ext.to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        "tiff" | "tif" => Some("image/tiff"),
        "ico" => Some("image/x-icon"),
        "pdf" => Some("application/pdf"),
        "txt" => Some("text/plain"),
        "json" => Some("application/json"),
        "yaml" | "yml" => Some("text/yaml"),
        "toml" => Some("text/toml"),
        "md" => Some("text/markdown"),
        "log" => Some("text/plain"),
        _ => None,
    }
}

/// Detect MIME type from file magic bytes (first few bytes).
fn mime_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some("image/png");
    }
    if bytes.len() >= 3 && &bytes[..3] == b"\xff\xd8\xff" {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 4 && &bytes[..4] == b"RIFF" && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return Some("image/bmp");
    }
    if bytes.len() >= 5 && &bytes[..5] == b"%PDF-" {
        return Some("application/pdf");
    }
    None
}

/// Validate that a file exists and determine its MIME type.
/// Returns `(mime_type, file_size)`.
pub fn validate_attachment(path: &Path) -> Result<(String, u64)> {
    if !path.exists() {
        anyhow::bail!("File not found: {}", path.display());
    }
    if !path.is_file() {
        anyhow::bail!("Not a regular file: {}", path.display());
    }

    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to read metadata: {}", path.display()))?;
    let size = metadata.len();

    // Try extension first, then magic bytes.
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    if let Some(mime) = mime_for_extension(ext) {
        return Ok((mime.to_string(), size));
    }

    // Read first 12 bytes for magic detection.
    let mut file =
        fs::File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
    let mut header = [0u8; 12];
    let n = file.read(&mut header).unwrap_or(0);
    if let Some(mime) = mime_from_magic(&header[..n]) {
        return Ok((mime.to_string(), size));
    }

    // Default to octet-stream for unknown types.
    Ok(("application/octet-stream".to_string(), size))
}

/// Copy a file into `.workgraph/attachments/` with a content-addressed name.
/// Returns the `Attachment` with the relative path.
pub fn store_attachment(workgraph_dir: &Path, source: &Path) -> Result<Attachment> {
    let (mime_type, size_bytes) = validate_attachment(source)?;

    let att_dir = attachments_dir(workgraph_dir);
    fs::create_dir_all(&att_dir)
        .with_context(|| format!("Failed to create attachments dir: {}", att_dir.display()))?;

    // Generate content hash for deduplication.
    let file_bytes =
        fs::read(source).with_context(|| format!("Failed to read file: {}", source.display()))?;
    let hash = Sha256::digest(&file_bytes);
    let hash_hex = format!("{:x}", hash);
    let short_hash = &hash_hex[..8];

    // Timestamp prefix for sortability.
    let now = Utc::now().format("%Y%m%d-%H%M%S");

    let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let filename = format!("{}-{}.{}", now, short_hash, ext);
    let dest = att_dir.join(&filename);

    // If the exact file already exists (same hash), skip copy.
    if !dest.exists() {
        fs::write(&dest, &file_bytes)
            .with_context(|| format!("Failed to write attachment: {}", dest.display()))?;
    }

    // Return path relative to workgraph dir's parent (project root).
    let relative = format!(".workgraph/attachments/{}", filename);

    Ok(Attachment {
        path: relative,
        mime_type,
        size_bytes,
    })
}

/// Clear chat data for a specific coordinator.
pub fn clear_for(workgraph_dir: &Path, coordinator_id: u32) -> Result<()> {
    let dir = chat_dir_for(workgraph_dir, coordinator_id);
    if dir.exists() {
        fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to clear chat directory: {}", dir.display()))?;
    }
    Ok(())
}

/// Clear all chat data for all coordinators.
pub fn clear(workgraph_dir: &Path) -> Result<()> {
    let dir = workgraph_dir.join("chat");
    if dir.exists() {
        fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to clear chat directory: {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();
        (tmp, wg_dir)
    }

    #[test]
    fn test_append_and_read_inbox() {
        let (_tmp, wg_dir) = setup();

        let id1 = append_inbox(&wg_dir, "hello coordinator", "req-1").unwrap();
        assert_eq!(id1, 1);

        let id2 = append_inbox(&wg_dir, "another message", "req-2").unwrap();
        assert_eq!(id2, 2);

        let msgs = read_inbox(&wg_dir).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id, 1);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "hello coordinator");
        assert_eq!(msgs[0].request_id, "req-1");
        assert_eq!(msgs[1].id, 2);
        assert_eq!(msgs[1].content, "another message");
        assert_eq!(msgs[1].request_id, "req-2");
    }

    #[test]
    fn test_append_and_read_outbox() {
        let (_tmp, wg_dir) = setup();

        let id1 = append_outbox(&wg_dir, "I'll help with that", "req-1").unwrap();
        assert_eq!(id1, 1);

        let id2 = append_outbox(&wg_dir, "Task created", "req-2").unwrap();
        assert_eq!(id2, 2);

        let msgs = read_outbox_since(&wg_dir, 0).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "coordinator");
        assert_eq!(msgs[0].content, "I'll help with that");
        assert_eq!(msgs[0].request_id, "req-1");
        assert_eq!(msgs[1].content, "Task created");
    }

    #[test]
    fn test_read_outbox_since_filters_by_cursor() {
        let (_tmp, wg_dir) = setup();

        append_outbox(&wg_dir, "msg 1", "req-1").unwrap();
        append_outbox(&wg_dir, "msg 2", "req-2").unwrap();
        append_outbox(&wg_dir, "msg 3", "req-3").unwrap();

        let msgs = read_outbox_since(&wg_dir, 1).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id, 2);
        assert_eq!(msgs[1].id, 3);

        let msgs = read_outbox_since(&wg_dir, 3).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_read_inbox_since_filters_by_cursor() {
        let (_tmp, wg_dir) = setup();

        append_inbox(&wg_dir, "msg 1", "req-1").unwrap();
        append_inbox(&wg_dir, "msg 2", "req-2").unwrap();

        let msgs = read_inbox_since(&wg_dir, 1).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, 2);
    }

    #[test]
    fn test_read_empty_inbox() {
        let (_tmp, wg_dir) = setup();
        let msgs = read_inbox(&wg_dir).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_read_empty_outbox() {
        let (_tmp, wg_dir) = setup();
        let msgs = read_outbox_since(&wg_dir, 0).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_separate_id_sequences() {
        let (_tmp, wg_dir) = setup();

        // Inbox and outbox have independent ID sequences
        let inbox_id = append_inbox(&wg_dir, "user msg", "req-1").unwrap();
        let outbox_id = append_outbox(&wg_dir, "coordinator msg", "req-1").unwrap();

        assert_eq!(inbox_id, 1);
        assert_eq!(outbox_id, 1); // Separate sequence, also starts at 1
    }

    #[test]
    fn test_cli_cursor_roundtrip() {
        let (_tmp, wg_dir) = setup();

        assert_eq!(read_cursor(&wg_dir).unwrap(), 0);

        write_cursor(&wg_dir, 5).unwrap();
        assert_eq!(read_cursor(&wg_dir).unwrap(), 5);

        write_cursor(&wg_dir, 10).unwrap();
        assert_eq!(read_cursor(&wg_dir).unwrap(), 10);
    }

    #[test]
    fn test_coordinator_cursor_roundtrip() {
        let (_tmp, wg_dir) = setup();

        assert_eq!(read_coordinator_cursor(&wg_dir).unwrap(), 0);

        write_coordinator_cursor(&wg_dir, 3).unwrap();
        assert_eq!(read_coordinator_cursor(&wg_dir).unwrap(), 3);

        write_coordinator_cursor(&wg_dir, 7).unwrap();
        assert_eq!(read_coordinator_cursor(&wg_dir).unwrap(), 7);
    }

    #[test]
    fn test_read_and_advance_cursor() {
        let (_tmp, wg_dir) = setup();

        append_outbox(&wg_dir, "msg 1", "req-1").unwrap();
        append_outbox(&wg_dir, "msg 2", "req-2").unwrap();

        // First read: gets both messages, advances cursor to 2
        let (cursor, msgs) = read_and_advance_cursor(&wg_dir).unwrap();
        assert_eq!(cursor, 2);
        assert_eq!(msgs.len(), 2);

        // Second read: no new messages, cursor stays at 2
        let (cursor, msgs) = read_and_advance_cursor(&wg_dir).unwrap();
        assert_eq!(cursor, 2);
        assert!(msgs.is_empty());

        // Add another message
        append_outbox(&wg_dir, "msg 3", "req-3").unwrap();

        // Third read: gets only the new message
        let (cursor, msgs) = read_and_advance_cursor(&wg_dir).unwrap();
        assert_eq!(cursor, 3);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "msg 3");
    }

    #[test]
    fn test_wait_for_response_found() {
        let (_tmp, wg_dir) = setup();

        // Pre-populate outbox with the response
        append_outbox(&wg_dir, "here is my response", "target-req").unwrap();
        append_outbox(&wg_dir, "other response", "other-req").unwrap();

        let result = wait_for_response(&wg_dir, "target-req", Duration::from_secs(1)).unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert_eq!(msg.content, "here is my response");
        assert_eq!(msg.request_id, "target-req");
    }

    #[test]
    fn test_wait_for_response_timeout() {
        let (_tmp, wg_dir) = setup();

        // No matching response exists
        append_outbox(&wg_dir, "wrong response", "wrong-req").unwrap();

        let result = wait_for_response(&wg_dir, "target-req", Duration::from_millis(300)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_wait_for_response_arrives_late() {
        let (_tmp, wg_dir) = setup();
        let wg_dir_clone = wg_dir.clone();

        // Spawn a thread that writes the response after a short delay
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            append_outbox(&wg_dir_clone, "delayed response", "late-req").unwrap();
        });

        let result = wait_for_response(&wg_dir, "late-req", Duration::from_secs(5)).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().content, "delayed response");

        handle.join().unwrap();
    }

    #[test]
    fn test_request_id_correlation() {
        let (_tmp, wg_dir) = setup();

        // Multiple requests and responses, interleaved
        append_inbox(&wg_dir, "first question", "req-aaa").unwrap();
        append_inbox(&wg_dir, "second question", "req-bbb").unwrap();
        append_outbox(&wg_dir, "answer to second", "req-bbb").unwrap();
        append_outbox(&wg_dir, "answer to first", "req-aaa").unwrap();

        // wait_for_response finds the correct one regardless of order
        let r1 = wait_for_response(&wg_dir, "req-aaa", Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(r1.content, "answer to first");

        let r2 = wait_for_response(&wg_dir, "req-bbb", Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(r2.content, "answer to second");
    }

    #[test]
    fn test_concurrent_writes() {
        let (_tmp, wg_dir) = setup();

        let mut handles = vec![];

        // Spawn 10 threads, each writing 10 messages to inbox
        for t in 0..10 {
            let dir = wg_dir.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..10 {
                    append_inbox(
                        &dir,
                        &format!("thread {} msg {}", t, i),
                        &format!("req-{}-{}", t, i),
                    )
                    .unwrap();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // All 100 messages should be present with unique IDs
        let msgs = read_inbox(&wg_dir).unwrap();
        assert_eq!(msgs.len(), 100);

        // IDs should be unique and form the set 1..=100
        let mut ids: Vec<u64> = msgs.iter().map(|m| m.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 100);
        assert_eq!(*ids.first().unwrap(), 1);
        assert_eq!(*ids.last().unwrap(), 100);
    }

    #[test]
    fn test_concurrent_inbox_and_outbox() {
        let (_tmp, wg_dir) = setup();

        let dir1 = wg_dir.clone();
        let dir2 = wg_dir.clone();

        // Write to inbox and outbox concurrently
        let h1 = std::thread::spawn(move || {
            for i in 0..20 {
                append_inbox(&dir1, &format!("inbox {}", i), &format!("req-{}", i)).unwrap();
            }
        });

        let h2 = std::thread::spawn(move || {
            for i in 0..20 {
                append_outbox(&dir2, &format!("outbox {}", i), &format!("req-{}", i)).unwrap();
            }
        });

        h1.join().unwrap();
        h2.join().unwrap();

        let inbox = read_inbox(&wg_dir).unwrap();
        let outbox = read_outbox_since(&wg_dir, 0).unwrap();

        assert_eq!(inbox.len(), 20);
        assert_eq!(outbox.len(), 20);

        // Each file has its own ID sequence
        assert_eq!(inbox.last().unwrap().id, 20);
        assert_eq!(outbox.last().unwrap().id, 20);
    }

    #[test]
    fn test_timestamps_are_valid() {
        let (_tmp, wg_dir) = setup();

        append_inbox(&wg_dir, "test", "req-1").unwrap();
        append_outbox(&wg_dir, "test", "req-1").unwrap();

        let inbox = read_inbox(&wg_dir).unwrap();
        let outbox = read_outbox_since(&wg_dir, 0).unwrap();

        chrono::DateTime::parse_from_rfc3339(&inbox[0].timestamp)
            .expect("inbox timestamp should be valid RFC 3339");
        chrono::DateTime::parse_from_rfc3339(&outbox[0].timestamp)
            .expect("outbox timestamp should be valid RFC 3339");
    }

    #[test]
    fn test_message_with_special_characters() {
        let (_tmp, wg_dir) = setup();

        let content = "Hello! Here's some \"JSON\" with {braces} and\nnewlines\tand unicode: 🎉";
        append_inbox(&wg_dir, content, "req-special").unwrap();

        let msgs = read_inbox(&wg_dir).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, content);
    }

    #[test]
    fn test_directory_created_on_first_write() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        // Don't pre-create .workgraph — append should handle it
        fs::create_dir_all(&wg_dir).unwrap();

        // chat/ directory doesn't exist yet
        assert!(!chat_dir(&wg_dir).exists());

        append_inbox(&wg_dir, "first message", "req-1").unwrap();

        assert!(chat_dir(&wg_dir).exists());
        assert!(inbox_path(&wg_dir).exists());
    }

    #[test]
    fn test_rotate_history_no_op_when_under_limit() {
        let (_tmp, wg_dir) = setup();

        for i in 0..5 {
            append_inbox(&wg_dir, &format!("msg {}", i), &format!("req-{}", i)).unwrap();
        }

        rotate_history(&wg_dir, 10).unwrap();

        let msgs = read_inbox(&wg_dir).unwrap();
        assert_eq!(msgs.len(), 5);
    }

    #[test]
    fn test_rotate_history_truncates_inbox() {
        let (_tmp, wg_dir) = setup();

        for i in 0..20 {
            append_inbox(&wg_dir, &format!("msg {}", i), &format!("req-{}", i)).unwrap();
        }

        rotate_history(&wg_dir, 5).unwrap();

        let msgs = read_inbox(&wg_dir).unwrap();
        assert_eq!(msgs.len(), 5);
        // Should keep the LAST 5 messages
        assert_eq!(msgs[0].content, "msg 15");
        assert_eq!(msgs[4].content, "msg 19");
    }

    #[test]
    fn test_rotate_history_truncates_outbox() {
        let (_tmp, wg_dir) = setup();

        for i in 0..15 {
            append_outbox(&wg_dir, &format!("resp {}", i), &format!("req-{}", i)).unwrap();
        }

        rotate_history(&wg_dir, 3).unwrap();

        let msgs = read_outbox_since(&wg_dir, 0).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].content, "resp 12");
        assert_eq!(msgs[2].content, "resp 14");
    }

    #[test]
    fn test_rotate_history_no_files() {
        let (_tmp, wg_dir) = setup();

        // Should not error when files don't exist
        rotate_history(&wg_dir, 5).unwrap();
    }

    #[test]
    fn test_rotate_history_preserves_message_fields() {
        let (_tmp, wg_dir) = setup();

        for i in 0..10 {
            append_inbox(&wg_dir, &format!("msg {}", i), &format!("req-{}", i)).unwrap();
        }

        rotate_history(&wg_dir, 3).unwrap();

        let msgs = read_inbox(&wg_dir).unwrap();
        assert_eq!(msgs.len(), 3);
        // Check all fields are preserved
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].request_id, "req-7");
        assert!(!msgs[0].timestamp.is_empty());
    }

    // --- Multi-coordinator isolation tests ---

    #[test]
    fn test_multi_coordinator_inbox_isolation() {
        let (_tmp, wg_dir) = setup();

        // Write to coordinator 0 and coordinator 1
        append_inbox_for(&wg_dir, 0, "msg for coord 0", "req-0").unwrap();
        append_inbox_for(&wg_dir, 1, "msg for coord 1", "req-1").unwrap();
        append_inbox_for(&wg_dir, 0, "second msg for coord 0", "req-2").unwrap();

        // Each coordinator sees only its own messages
        let msgs0 = read_inbox_for(&wg_dir, 0).unwrap();
        assert_eq!(msgs0.len(), 2);
        assert_eq!(msgs0[0].content, "msg for coord 0");
        assert_eq!(msgs0[1].content, "second msg for coord 0");

        let msgs1 = read_inbox_for(&wg_dir, 1).unwrap();
        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs1[0].content, "msg for coord 1");

        // Coordinator 2 has no messages
        let msgs2 = read_inbox_for(&wg_dir, 2).unwrap();
        assert!(msgs2.is_empty());
    }

    #[test]
    fn test_multi_coordinator_outbox_isolation() {
        let (_tmp, wg_dir) = setup();

        append_outbox_for(&wg_dir, 0, "resp from coord 0", "req-0").unwrap();
        append_outbox_for(&wg_dir, 1, "resp from coord 1", "req-1").unwrap();

        let msgs0 = read_outbox_since_for(&wg_dir, 0, 0).unwrap();
        assert_eq!(msgs0.len(), 1);
        assert_eq!(msgs0[0].content, "resp from coord 0");

        let msgs1 = read_outbox_since_for(&wg_dir, 1, 0).unwrap();
        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs1[0].content, "resp from coord 1");
    }

    #[test]
    fn test_multi_coordinator_cursor_isolation() {
        let (_tmp, wg_dir) = setup();

        write_cursor_for(&wg_dir, 0, 5).unwrap();
        write_cursor_for(&wg_dir, 1, 10).unwrap();

        assert_eq!(read_cursor_for(&wg_dir, 0).unwrap(), 5);
        assert_eq!(read_cursor_for(&wg_dir, 1).unwrap(), 10);
        assert_eq!(read_cursor_for(&wg_dir, 2).unwrap(), 0); // non-existent
    }

    #[test]
    fn test_multi_coordinator_coordinator_cursor_isolation() {
        let (_tmp, wg_dir) = setup();

        write_coordinator_cursor_for(&wg_dir, 0, 3).unwrap();
        write_coordinator_cursor_for(&wg_dir, 1, 7).unwrap();

        assert_eq!(read_coordinator_cursor_for(&wg_dir, 0).unwrap(), 3);
        assert_eq!(read_coordinator_cursor_for(&wg_dir, 1).unwrap(), 7);
    }

    #[test]
    fn test_multi_coordinator_history_isolation() {
        let (_tmp, wg_dir) = setup();

        append_inbox_for(&wg_dir, 0, "user to coord 0", "req-0").unwrap();
        append_outbox_for(&wg_dir, 0, "coord 0 reply", "req-0").unwrap();
        append_inbox_for(&wg_dir, 1, "user to coord 1", "req-1").unwrap();
        append_outbox_for(&wg_dir, 1, "coord 1 reply", "req-1").unwrap();

        let hist0 = read_history_for(&wg_dir, 0).unwrap();
        assert_eq!(hist0.len(), 2);
        assert_eq!(hist0[0].content, "user to coord 0");
        assert_eq!(hist0[1].content, "coord 0 reply");

        let hist1 = read_history_for(&wg_dir, 1).unwrap();
        assert_eq!(hist1.len(), 2);
        assert_eq!(hist1[0].content, "user to coord 1");
        assert_eq!(hist1[1].content, "coord 1 reply");
    }

    #[test]
    fn test_multi_coordinator_clear_isolation() {
        let (_tmp, wg_dir) = setup();

        append_inbox_for(&wg_dir, 0, "msg 0", "req-0").unwrap();
        append_inbox_for(&wg_dir, 1, "msg 1", "req-1").unwrap();

        // Clear only coordinator 0
        clear_for(&wg_dir, 0).unwrap();

        // Coordinator 0 is empty, coordinator 1 still has data
        let msgs0 = read_inbox_for(&wg_dir, 0).unwrap();
        assert!(msgs0.is_empty());

        let msgs1 = read_inbox_for(&wg_dir, 1).unwrap();
        assert_eq!(msgs1.len(), 1);
    }

    #[test]
    fn test_multi_coordinator_backward_compat() {
        let (_tmp, wg_dir) = setup();

        // The default API should work with coordinator 0
        append_inbox(&wg_dir, "default msg", "req-default").unwrap();

        // Should be visible via coordinator 0 API
        let msgs = read_inbox_for(&wg_dir, 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "default msg");

        // And via the backward-compat API
        let msgs = read_inbox(&wg_dir).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "default msg");
    }

    #[test]
    fn test_multi_coordinator_independent_id_sequences() {
        let (_tmp, wg_dir) = setup();

        let id0 = append_inbox_for(&wg_dir, 0, "coord 0 msg", "req-0").unwrap();
        let id1 = append_inbox_for(&wg_dir, 1, "coord 1 msg", "req-1").unwrap();

        // Each coordinator starts its own ID sequence at 1
        assert_eq!(id0, 1);
        assert_eq!(id1, 1);
    }

    #[test]
    fn test_multi_coordinator_wait_for_response() {
        let (_tmp, wg_dir) = setup();

        // Put response in coordinator 1's outbox
        append_outbox_for(&wg_dir, 1, "response from coord 1", "target-req").unwrap();

        // Searching coordinator 0 should not find it
        let result = wait_for_response_for(&wg_dir, 0, "target-req", Duration::from_millis(100))
            .unwrap();
        assert!(result.is_none());

        // Searching coordinator 1 should find it
        let result = wait_for_response_for(&wg_dir, 1, "target-req", Duration::from_secs(1))
            .unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().content, "response from coord 1");
    }
}
