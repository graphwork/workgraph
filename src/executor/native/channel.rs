//! Tool output channeling.
//!
//! Large tool outputs (bash dumps, file reads, grep results) are written to
//! disk and replaced in the message vec with a small handle string that
//! tells the agent where to look. The goal is a **hard invariant**: no
//! single tool call can inject more than `threshold_bytes` of content into
//! the message vec, so context explosion from a single chatty tool call is
//! structurally impossible.
//!
//! The handle string includes a short preview plus explicit bash hints
//! (`cat`, `head`, `tail`, `sed`, `grep`) so the agent can retrieve any
//! slice of the full output on demand. This makes channeled content
//! **retrievable**, which is the property that distinguishes L1 from L0:
//! compacted content is lost, channeled content is paged.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default threshold: tool outputs up to 2 KiB pass through unchanged.
/// Anything larger gets channeled.
pub const DEFAULT_CHANNEL_THRESHOLD_BYTES: usize = 2048;

/// Tools whose output should NEVER be channeled — their whole job is
/// to bring structured data INTO the model's context, so replacing
/// that data with a handle defeats the point. `web_search` enforces
/// its own size cap (MAX_RESULTS results) so it fits comfortably in
/// a turn and channeling is redundant. `web_fetch` self-manages its
/// output differently — it writes fetched pages to a file artifact
/// and returns metadata + preview, so it never passes a huge body
/// to the channeler in the first place.
///
/// We learned this the hard way: qwen3-coder-30b was hallucinating
/// restaurant names from real-looking URLs because it had never
/// actually seen the web_search output — the 8 KB of results had
/// been channeled to disk and the model only had a 400-char preview
/// to work with. The model grounded on what it could see (restaurant
/// name fragments) and confabulated plausible variants for the rest.
const NEVER_CHANNEL_TOOLS: &[&str] = &["web_search"];

/// Number of chars of preview included in the handle string.
pub const DEFAULT_PREVIEW_CHARS: usize = 400;

/// Routes oversized tool outputs to disk and returns a compact handle.
pub struct ToolOutputChanneler {
    /// Directory where channeled outputs are written (typically
    /// `<agent_dir>/tool-outputs/`).
    dir: PathBuf,
    /// Monotonic counter for output filenames.
    counter: AtomicUsize,
    /// Outputs ≤ this size pass through unchanged.
    threshold_bytes: usize,
    /// Chars of preview to include in the handle string.
    preview_chars: usize,
}

impl ToolOutputChanneler {
    pub fn new(dir: PathBuf) -> Self {
        Self::with_threshold(dir, DEFAULT_CHANNEL_THRESHOLD_BYTES)
    }

    pub fn with_threshold(dir: PathBuf, threshold_bytes: usize) -> Self {
        Self {
            dir,
            counter: AtomicUsize::new(0),
            threshold_bytes,
            preview_chars: DEFAULT_PREVIEW_CHARS,
        }
    }

    /// If `content` exceeds the threshold, write it to disk and return a
    /// handle string pointing to the file. Otherwise return `content`
    /// unchanged.
    ///
    /// Tools in `NEVER_CHANNEL_TOOLS` always pass through regardless
    /// of size — their outputs are the whole point of the call and
    /// truncating them to a 400-char preview destroys the value.
    ///
    /// On any I/O failure, returns the original content rather than
    /// silently losing it — channeling is best-effort, never a blocker.
    pub fn maybe_channel(&self, tool_name: &str, content: &str) -> String {
        if NEVER_CHANNEL_TOOLS.contains(&tool_name) {
            return content.to_string();
        }
        if content.len() <= self.threshold_bytes {
            return content.to_string();
        }

        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let filename = format!("{:05}.log", n);
        let path = self.dir.join(&filename);

        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            eprintln!(
                "[channel] Failed to create {}: {} — passing output through unchanneled",
                self.dir.display(),
                e
            );
            return content.to_string();
        }
        if let Err(e) = std::fs::write(&path, content) {
            eprintln!(
                "[channel] Failed to write {}: {} — passing output through unchanneled",
                path.display(),
                e
            );
            return content.to_string();
        }

        // Prefer canonical/absolute path in the handle for agent clarity.
        let display_path = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .display()
            .to_string();

        let preview_end = content.floor_char_boundary(self.preview_chars);
        let preview = &content[..preview_end];

        format!(
            "[CHANNELED OUTPUT — {bytes} bytes from tool '{tool}' saved to {path}]\n\
             First {plen} chars preview:\n\
             ---\n\
             {preview}\n\
             ---\n\
             [Full output is on disk. To retrieve specific parts, use bash:]\n\
             - `cat {path}`                  (entire file)\n\
             - `head -n 50 {path}`           (first 50 lines)\n\
             - `tail -n 50 {path}`           (last 50 lines)\n\
             - `sed -n '100,200p' {path}`    (lines 100–200)\n\
             - `grep -n 'PATTERN' {path}`    (find pattern with line numbers)\n\
             - `wc -l {path}`                (total line count)",
            bytes = content.len(),
            tool = tool_name,
            path = display_path,
            plen = preview.len(),
            preview = preview,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_small_output_passes_through() {
        let tmp = TempDir::new().unwrap();
        let channeler = ToolOutputChanneler::with_threshold(tmp.path().to_path_buf(), 2048);
        let out = channeler.maybe_channel("bash", "hello world");
        assert_eq!(out, "hello world");
        // No file should have been written
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[test]
    fn test_large_output_is_channeled() {
        let tmp = TempDir::new().unwrap();
        let channeler = ToolOutputChanneler::with_threshold(tmp.path().to_path_buf(), 100);
        let content: String = "a".repeat(5000);
        let handle = channeler.maybe_channel("read_file", &content);

        // Handle is much smaller than original
        assert!(handle.len() < content.len() / 2);
        // Handle mentions the size
        assert!(handle.contains("5000 bytes"));
        // Handle mentions the tool name
        assert!(handle.contains("read_file"));
        // Handle includes bash hints
        assert!(handle.contains("cat "));
        assert!(handle.contains("head -n"));
        assert!(handle.contains("grep"));

        // The file should exist and contain the full original content
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let file_content = std::fs::read_to_string(entries[0].path()).unwrap();
        assert_eq!(file_content, content);
    }

    #[test]
    fn test_counter_increments_across_calls() {
        let tmp = TempDir::new().unwrap();
        let channeler = ToolOutputChanneler::with_threshold(tmp.path().to_path_buf(), 10);
        let _ = channeler.maybe_channel("bash", &"x".repeat(100));
        let _ = channeler.maybe_channel("bash", &"y".repeat(100));
        let _ = channeler.maybe_channel("bash", &"z".repeat(100));

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries.len(), 3);
        // Filenames are monotonically numbered
        let mut sorted = entries.clone();
        sorted.sort();
        assert_eq!(sorted[0], "00000.log");
        assert_eq!(sorted[1], "00001.log");
        assert_eq!(sorted[2], "00002.log");
    }

    #[test]
    fn test_preview_included_in_handle() {
        let tmp = TempDir::new().unwrap();
        let channeler = ToolOutputChanneler::with_threshold(tmp.path().to_path_buf(), 10);
        let content = format!("PREFIX_{}", "x".repeat(2000));
        let handle = channeler.maybe_channel("grep", &content);
        // The preview should include the prefix
        assert!(handle.contains("PREFIX_"));
    }

    #[test]
    fn test_default_threshold_is_reasonable() {
        let tmp = TempDir::new().unwrap();
        let channeler = ToolOutputChanneler::new(tmp.path().to_path_buf());
        // 1KB passes through at default threshold
        let small = "a".repeat(1024);
        assert_eq!(channeler.maybe_channel("bash", &small), small);
        // 4KB gets channeled
        let large = "a".repeat(4096);
        let handle = channeler.maybe_channel("bash", &large);
        assert!(handle.contains("CHANNELED OUTPUT"));
    }
}
