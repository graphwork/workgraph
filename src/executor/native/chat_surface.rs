//! Chat-file I/O surface for a long-running `wg nex` session.
//!
//! When a nex session is bound to a chat-dir (via
//! `AgentLoop::with_chat_ref`), it bypasses stdin/stderr and:
//!
//! - **Reads** user turns from `chat/<ref>/inbox.jsonl` via
//!   `chat::read_inbox_since_ref` — one `ChatMessage` per line,
//!   either inotify-driven (sub-millisecond wake-up on new writes)
//!   or a fallback polling loop. A cursor file (`.nex-cursor`)
//!   persists the last-consumed message id so a restart resumes
//!   where we left off.
//! - **Writes** streaming token chunks to `chat/<ref>/.streaming`
//!   via `chat::write_streaming_ref` — the canonical streaming
//!   dotfile that the TUI already tails.
//! - **Appends** each finalized assistant turn to
//!   `chat/<ref>/outbox.jsonl` via `chat::append_outbox_ref`,
//!   tagged with the originating `request_id` so the TUI can
//!   correlate.
//!
//! Sessions are identified by string reference: a UUID, UUID prefix,
//! or alias (`coordinator-0`, `task-<id>`, user-chosen). The
//! filesystem `chat/<alias>` → `chat/<uuid>` symlinks installed by
//! `chat_sessions` let readers address the same session under any
//! name without this module having to resolve anything.
//!
//! This module is a thin adapter over `crate::chat` — same paths,
//! same formats — so a nex agent bound to a chat-ref is
//! indistinguishable from the legacy in-process `native_coordinator_loop`
//! as far as the TUI is concerned.
//!
//! Journal location is also deterministic from the chat-ref so
//! `--resume` restores from the right place:
//!   `chat/<ref>/conversation.jsonl`
//!   `chat/<ref>/session-summary.md`

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// One user turn, flattened from `crate::chat::ChatMessage` into what
/// the agent loop actually needs: the request_id (for correlating the
/// outbox response) and the message content.
#[derive(Debug, Clone)]
pub struct InboxEntry {
    pub request_id: String,
    pub message: String,
    /// Monotonic `ChatMessage.id` — stored back to the cursor file so
    /// a restart skips what we already consumed.
    pub id: u64,
}

/// Paths for one chat-tethered nex session. The inbox/outbox/streaming
/// trio is owned by `crate::chat` — we only name journal + cursor files
/// that chat.rs doesn't know about.
#[derive(Clone, Debug)]
pub struct ChatPaths {
    pub dir: PathBuf,
    pub journal: PathBuf,
    pub session_summary: PathBuf,
    pub cursor: PathBuf,
}

impl ChatPaths {
    /// Build paths from a session reference (UUID, alias, or numeric
    /// coord id). Resolves through `chat::chat_dir_for_ref`, which
    /// goes through the sessions.json registry — the canonical path
    /// is always `chat/<uuid>/`, aliases exist only in the registry.
    pub fn for_ref(workgraph_dir: &Path, session_ref: &str) -> Self {
        let dir = crate::chat::chat_dir_for_ref(workgraph_dir, session_ref);
        Self {
            journal: dir.join("conversation.jsonl"),
            session_summary: dir.join("session-summary.md"),
            cursor: dir.join(".nex-cursor"),
            dir,
        }
    }

    /// Legacy numeric-id constructor. Equivalent to
    /// `for_ref(dir, &id.to_string())`.
    pub fn for_chat_id(workgraph_dir: &Path, chat_id: u32) -> Self {
        Self::for_ref(workgraph_dir, &chat_id.to_string())
    }

    pub fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("create_dir_all {:?}", self.dir))
    }
}

/// Tail-style reader over the chat inbox. Uses inotify (via the
/// `notify` crate) to wake as soon as the inbox changes; falls back
/// to a 5s poll if the watcher ever dies or misses a write.
pub struct ChatInboxReader {
    workgraph_dir: PathBuf,
    session_ref: String,
    paths: ChatPaths,
    cursor: Arc<Mutex<u64>>,
    /// Filesystem watcher kept alive for the life of this reader.
    /// `None` means inotify init failed — readers fall back to pure
    /// polling.
    _watcher: Option<RecommendedWatcher>,
    /// Tokio async channel that the watcher pushes events into.
    /// Wrapped in a `Mutex` because `next_entry` takes `&self` (the
    /// reader is shared through the agent loop); only one coroutine
    /// ever waits on it at a time in practice.
    events_rx: Option<Arc<tokio::sync::Mutex<UnboundedReceiver<notify::Result<Event>>>>>,
}

impl ChatInboxReader {
    pub fn new(workgraph_dir: PathBuf, session_ref: String, paths: ChatPaths) -> Result<Self> {
        paths.ensure_dir()?;
        let cursor = load_cursor(&paths.cursor).unwrap_or(0);

        // Set up the inotify watcher on the chat dir. Watching the
        // directory (not just `inbox.jsonl`) catches atomic-rename
        // writes where the target path is replaced, which some tools
        // (including `crate::chat::append_message`) do under the hood.
        // If the watcher can't be created, degrade to pure polling.
        //
        // We bridge the `notify` crate's synchronous callback into a
        // tokio unbounded channel, so `wait_for_change` can actually
        // `await` on new events and wake sub-millisecond rather than
        // falling back to a 250ms poll.
        let (tx, rx): (
            UnboundedSender<notify::Result<Event>>,
            UnboundedReceiver<notify::Result<Event>>,
        ) = unbounded_channel();
        let watcher_result = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .and_then(|mut w| w.watch(&paths.dir, RecursiveMode::NonRecursive).map(|_| w));
        let (watcher, events_rx) = match watcher_result {
            Ok(w) => (Some(w), Some(Arc::new(tokio::sync::Mutex::new(rx)))),
            Err(e) => {
                eprintln!(
                    "\x1b[33m[chat-inbox] inotify setup failed for {:?}: {} — falling back to polling\x1b[0m",
                    paths.dir, e
                );
                (None, None)
            }
        };

        Ok(Self {
            workgraph_dir,
            session_ref,
            paths,
            cursor: Arc::new(Mutex::new(cursor)),
            _watcher: watcher,
            events_rx,
        })
    }

    /// Block until the next inbox entry beyond our cursor is available.
    ///
    /// Waits on the inotify channel with a `poll_interval` timeout —
    /// the timeout is the fallback floor (so if the watcher silently
    /// drops an event, we still re-scan within `poll_interval`).
    /// With inotify working, wake-up is sub-millisecond.
    ///
    /// Returns `None` only on persistent read errors; callers should
    /// treat that as shutdown.
    pub async fn next_entry(&self, poll_interval: Duration) -> Option<InboxEntry> {
        let chat_dir = crate::chat::chat_dir_for_ref(&self.workgraph_dir, &self.session_ref);
        loop {
            // Check cooperative release marker. If another process
            // (typically the TUI, after a user-send takeover trigger)
            // asked us to release, return None — the caller treats
            // None as EOF and exits the loop cleanly at the next turn
            // boundary. Without this check, the handler would block
            // forever on the inbox even after the release was
            // requested. See docs/design/sessions-as-identity.md
            // §Handoff policy.
            if crate::session_lock::release_requested(&chat_dir) {
                return None;
            }
            match self.try_next_entry() {
                Ok(Some(entry)) => return Some(entry),
                Ok(None) => {
                    self.wait_for_change(poll_interval).await;
                }
                Err(e) => {
                    eprintln!(
                        "\x1b[33m[chat-inbox] read error on {}: {} — retrying\x1b[0m",
                        self.session_ref, e
                    );
                    tokio::time::sleep(poll_interval * 2).await;
                }
            }
        }
    }

    /// Wait for either:
    /// - an inotify event to arrive on the chat dir (sub-ms), or
    /// - `poll_interval` to elapse as a safety-net poll.
    ///
    /// With the tokio-channel bridge, `recv().await` genuinely
    /// awaits the next event — we don't busy-poll, and an inbox
    /// write wakes us within the kernel's scheduling quantum.
    /// The timeout is the floor for the rare case where inotify
    /// silently drops an event.
    async fn wait_for_change(&self, poll_interval: Duration) {
        let Some(rx) = self.events_rx.as_ref() else {
            // No watcher — pure polling fallback.
            tokio::time::sleep(poll_interval).await;
            return;
        };
        let mut guard = rx.lock().await;
        let _ = tokio::time::timeout(poll_interval, guard.recv()).await;
        // Drain any extra burst events so the next try_next_entry
        // runs without a backlog of stale signals.
        while guard.try_recv().is_ok() {}
    }

    /// Non-blocking read. Returns Ok(None) if no new entries, Ok(Some)
    /// if one was read (advancing the cursor), Err on I/O failure.
    pub fn try_next_entry(&self) -> Result<Option<InboxEntry>> {
        let cursor = *self.cursor.lock().unwrap();
        let new_msgs =
            crate::chat::read_inbox_since_ref(&self.workgraph_dir, &self.session_ref, cursor)
                .with_context(|| {
                    format!(
                        "read_inbox_since_ref(session={}, cursor={})",
                        self.session_ref, cursor
                    )
                })?;
        for msg in new_msgs {
            *self.cursor.lock().unwrap() = msg.id;
            save_cursor(&self.paths.cursor, msg.id);
            if msg.role != "user" {
                continue;
            }
            return Ok(Some(InboxEntry {
                request_id: msg.request_id,
                message: msg.content,
                id: msg.id,
            }));
        }
        Ok(None)
    }

    pub fn cursor(&self) -> u64 {
        *self.cursor.lock().unwrap()
    }
}

fn load_cursor(path: &Path) -> Option<u64> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse::<u64>().ok()
}

fn save_cursor(path: &Path, cursor: u64) {
    let _ = std::fs::write(path, cursor.to_string());
}

/// Advance the cursor past all current inbox messages. Used at
/// fresh-session start (no `--resume`) so we don't re-process queued
/// messages meant for a previous session.
pub fn seek_inbox_to_end(
    workgraph_dir: &Path,
    session_ref: &str,
    paths: &ChatPaths,
) -> Result<u64> {
    let msgs = crate::chat::read_inbox_ref(workgraph_dir, session_ref).unwrap_or_default();
    let last_id = msgs.iter().map(|m| m.id).max().unwrap_or(0);
    save_cursor(&paths.cursor, last_id);
    Ok(last_id)
}

/// Overwrite the streaming dotfile with the full accumulated text.
/// Thin pass-through to `chat::write_streaming_ref`.
pub fn write_streaming(workgraph_dir: &Path, session_ref: &str, text: &str) -> Result<()> {
    crate::chat::write_streaming_ref(workgraph_dir, session_ref, text)
}

/// Append a finalized assistant response to the outbox.
/// Thin pass-through to `chat::append_outbox_ref`.
pub fn append_outbox(
    workgraph_dir: &Path,
    session_ref: &str,
    text: &str,
    request_id: &str,
) -> Result<u64> {
    crate::chat::append_outbox_ref(workgraph_dir, session_ref, text, request_id)
}

/// Clear the streaming dotfile (called between turns).
pub fn clear_streaming(workgraph_dir: &Path, session_ref: &str) {
    crate::chat::clear_streaming_ref(workgraph_dir, session_ref);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn reader_sees_new_inbox_messages() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let paths = ChatPaths::for_chat_id(wg_dir, 7);
        paths.ensure_dir().unwrap();

        crate::chat::append_inbox_for(wg_dir, 7, "hello", "r1").unwrap();
        crate::chat::append_inbox_for(wg_dir, 7, "world", "r2").unwrap();

        let reader = ChatInboxReader::new(wg_dir.to_path_buf(), "7".to_string(), paths).unwrap();
        let e1 = reader.try_next_entry().unwrap().unwrap();
        assert_eq!(e1.request_id, "r1");
        assert_eq!(e1.message, "hello");
        let e2 = reader.try_next_entry().unwrap().unwrap();
        assert_eq!(e2.request_id, "r2");
        assert_eq!(e2.message, "world");
        assert!(reader.try_next_entry().unwrap().is_none());
    }

    #[test]
    fn cursor_persists_across_reader_instances() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let paths = ChatPaths::for_chat_id(wg_dir, 7);
        paths.ensure_dir().unwrap();

        crate::chat::append_inbox_for(wg_dir, 7, "a", "r1").unwrap();
        crate::chat::append_inbox_for(wg_dir, 7, "b", "r2").unwrap();

        let r1 =
            ChatInboxReader::new(wg_dir.to_path_buf(), "7".to_string(), paths.clone()).unwrap();
        let e = r1.try_next_entry().unwrap().unwrap();
        assert_eq!(e.message, "a");
        drop(r1);

        let r2 = ChatInboxReader::new(wg_dir.to_path_buf(), "7".to_string(), paths).unwrap();
        let e = r2.try_next_entry().unwrap().unwrap();
        assert_eq!(e.message, "b", "cursor should have advanced past 'a'");
    }

    #[test]
    fn seek_to_end_skips_existing_messages() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let paths = ChatPaths::for_chat_id(wg_dir, 7);
        paths.ensure_dir().unwrap();

        crate::chat::append_inbox_for(wg_dir, 7, "old", "r1").unwrap();
        crate::chat::append_inbox_for(wg_dir, 7, "older", "r2").unwrap();

        seek_inbox_to_end(wg_dir, "7", &paths).unwrap();

        let reader = ChatInboxReader::new(wg_dir.to_path_buf(), "7".to_string(), paths).unwrap();
        assert!(
            reader.try_next_entry().unwrap().is_none(),
            "new reader should see no existing messages after seek_to_end"
        );

        crate::chat::append_inbox_for(wg_dir, 7, "new", "r3").unwrap();
        let e = reader.try_next_entry().unwrap().unwrap();
        assert_eq!(e.request_id, "r3");
    }

    #[test]
    fn outbox_roundtrip_via_chat_rs() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let paths = ChatPaths::for_chat_id(wg_dir, 7);
        paths.ensure_dir().unwrap();

        append_outbox(wg_dir, "7", "response text", "r1").unwrap();
        let msgs = crate::chat::read_outbox_since_for(wg_dir, 7, 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "response text");
        assert_eq!(msgs[0].request_id, "r1");
    }

    #[test]
    fn reader_works_with_uuid_ref() {
        use crate::chat_sessions::{SessionKind, create_session};
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let uuid = create_session(wg_dir, SessionKind::Interactive, &[], None).unwrap();
        let paths = ChatPaths::for_ref(wg_dir, &uuid);

        crate::chat::append_inbox_ref(wg_dir, &uuid, "hi via uuid", "r1").unwrap();
        let reader = ChatInboxReader::new(wg_dir.to_path_buf(), uuid.clone(), paths).unwrap();
        let e = reader.try_next_entry().unwrap().unwrap();
        assert_eq!(e.message, "hi via uuid");

        // Reading via alias works too if we add one.
        crate::chat_sessions::add_alias(wg_dir, &uuid, "handy").unwrap();
        crate::chat::append_inbox_ref(wg_dir, "handy", "via alias", "r2").unwrap();
        let e = reader.try_next_entry().unwrap().unwrap();
        assert_eq!(e.message, "via alias");
    }
}
