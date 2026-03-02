//! Chat inbox/outbox storage for user↔coordinator communication.
//!
//! Messages are stored as JSONL files in `.workgraph/chat/`:
//! - `inbox.jsonl`  — user → coordinator messages
//! - `outbox.jsonl` — coordinator → user responses
//! - `.cursor`      — CLI/TUI read cursor (last-read outbox message ID)
//! - `.coordinator-cursor` — coordinator read cursor (last-processed inbox message ID)
//!
//! Follows the same concurrency model as `src/messages.rs`:
//! writers use `O_APPEND` + `flock()` for safe ID assignment,
//! readers are lock-free.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A single chat message between user and coordinator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    /// Monotonically increasing ID within the file (inbox and outbox have separate sequences)
    pub id: u64,
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// "user" or "coordinator"
    pub role: String,
    /// Message content (free-form text, may contain markdown)
    pub content: String,
    /// Correlates a user request with the coordinator's response.
    pub request_id: String,
}

/// Directory for chat files.
fn chat_dir(workgraph_dir: &Path) -> PathBuf {
    workgraph_dir.join("chat")
}

/// Path to the inbox JSONL file (user → coordinator).
fn inbox_path(workgraph_dir: &Path) -> PathBuf {
    chat_dir(workgraph_dir).join("inbox.jsonl")
}

/// Path to the outbox JSONL file (coordinator → user).
fn outbox_path(workgraph_dir: &Path) -> PathBuf {
    chat_dir(workgraph_dir).join("outbox.jsonl")
}

/// Path to the CLI/TUI cursor file (last-read outbox message ID).
fn cursor_path(workgraph_dir: &Path) -> PathBuf {
    chat_dir(workgraph_dir).join(".cursor")
}

/// Path to the coordinator cursor file (last-processed inbox message ID).
fn coordinator_cursor_path(workgraph_dir: &Path) -> PathBuf {
    chat_dir(workgraph_dir).join(".coordinator-cursor")
}

/// Append a message to a JSONL file with flock-based ID assignment.
///
/// Opens the file with O_APPEND, acquires an exclusive lock,
/// reads the current max ID, assigns the next ID, and appends.
fn append_message(path: &Path, role: &str, content: &str, request_id: &str) -> Result<u64> {
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

// --- Public API ---

/// Append a user message to the inbox.
///
/// Returns the assigned inbox message ID.
pub fn append_inbox(workgraph_dir: &Path, content: &str, request_id: &str) -> Result<u64> {
    let path = inbox_path(workgraph_dir);
    append_message(&path, "user", content, request_id)
}

/// Append a coordinator response to the outbox.
///
/// Returns the assigned outbox message ID.
pub fn append_outbox(workgraph_dir: &Path, content: &str, request_id: &str) -> Result<u64> {
    let path = outbox_path(workgraph_dir);
    append_message(&path, "coordinator", content, request_id)
}

/// Read all inbox messages (user → coordinator).
pub fn read_inbox(workgraph_dir: &Path) -> Result<Vec<ChatMessage>> {
    read_messages(&inbox_path(workgraph_dir))
}

/// Read inbox messages with ID > cursor.
pub fn read_inbox_since(workgraph_dir: &Path, cursor: u64) -> Result<Vec<ChatMessage>> {
    let all = read_messages(&inbox_path(workgraph_dir))?;
    Ok(all.into_iter().filter(|m| m.id > cursor).collect())
}

/// Read outbox messages with ID > cursor.
///
/// Does not advance the cursor (caller decides when to advance).
pub fn read_outbox_since(workgraph_dir: &Path, cursor: u64) -> Result<Vec<ChatMessage>> {
    let all = read_messages(&outbox_path(workgraph_dir))?;
    Ok(all.into_iter().filter(|m| m.id > cursor).collect())
}

/// Block until a response with the given request_id appears in the outbox,
/// or timeout expires.
///
/// Polls outbox.jsonl every 200ms, checking for a message whose request_id
/// matches. Returns the first matching response, or None on timeout.
pub fn wait_for_response(
    workgraph_dir: &Path,
    request_id: &str,
    timeout: Duration,
) -> Result<Option<ChatMessage>> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(200);

    loop {
        let messages = read_messages(&outbox_path(workgraph_dir))?;
        if let Some(msg) = messages.into_iter().find(|m| m.request_id == request_id) {
            return Ok(Some(msg));
        }

        if start.elapsed() >= timeout {
            return Ok(None);
        }

        std::thread::sleep(poll_interval);
    }
}

/// Read the CLI/TUI cursor (last-read outbox message ID).
pub fn read_cursor(workgraph_dir: &Path) -> Result<u64> {
    read_cursor_file(&cursor_path(workgraph_dir))
}

/// Write the CLI/TUI cursor.
pub fn write_cursor(workgraph_dir: &Path, cursor: u64) -> Result<()> {
    write_cursor_file(&cursor_path(workgraph_dir), cursor)
}

/// Read and advance the CLI/TUI cursor.
///
/// Returns (new_cursor_value, messages_since_old_cursor).
pub fn read_and_advance_cursor(workgraph_dir: &Path) -> Result<(u64, Vec<ChatMessage>)> {
    let old_cursor = read_cursor(workgraph_dir)?;
    let new_messages = read_outbox_since(workgraph_dir, old_cursor)?;

    let new_cursor = new_messages.last().map(|m| m.id).unwrap_or(old_cursor);
    if new_cursor > old_cursor {
        write_cursor(workgraph_dir, new_cursor)?;
    }

    Ok((new_cursor, new_messages))
}

/// Read the coordinator cursor (last-processed inbox message ID).
pub fn read_coordinator_cursor(workgraph_dir: &Path) -> Result<u64> {
    read_cursor_file(&coordinator_cursor_path(workgraph_dir))
}

/// Write the coordinator cursor.
pub fn write_coordinator_cursor(workgraph_dir: &Path, cursor: u64) -> Result<()> {
    write_cursor_file(&coordinator_cursor_path(workgraph_dir), cursor)
}

/// Read all inbox and outbox messages interleaved by timestamp for history display.
pub fn read_history(workgraph_dir: &Path) -> Result<Vec<ChatMessage>> {
    let mut all = read_messages(&inbox_path(workgraph_dir))?;
    all.extend(read_messages(&outbox_path(workgraph_dir))?);
    all.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    Ok(all)
}

/// Rotate old chat history, keeping only the last `keep_count` messages per file.
///
/// This prevents inbox.jsonl and outbox.jsonl from growing unboundedly.
/// Old messages are discarded. The cursor files are NOT adjusted — callers
/// should only rotate at natural boundaries (e.g., on coordinator restart).
pub fn rotate_history(workgraph_dir: &Path, keep_count: usize) -> Result<()> {
    rotate_file(&inbox_path(workgraph_dir), keep_count)?;
    rotate_file(&outbox_path(workgraph_dir), keep_count)?;
    Ok(())
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

/// Clear all chat data (inbox, outbox, cursors).
pub fn clear(workgraph_dir: &Path) -> Result<()> {
    let dir = chat_dir(workgraph_dir);
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
}
