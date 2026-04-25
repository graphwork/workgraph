//! Unified chat-history access across every executor type.
//!
//! Each executor (native, claude, codex) writes its conversation to a
//! canonical location. We don't mirror or translate in the background —
//! we just know where each one lives and read it on demand when
//! something needs to display or export history (`wg chat history`,
//! the TUI Log tab, future adapters).
//!
//! Principle: native isn't special. It's one of three executors, each
//! with its own native transcript file. This module is the one place
//! that knows which file to read per executor.
//!
//! | executor | path                                                         |
//! |----------|--------------------------------------------------------------|
//! | native   | `<workgraph>/chat/<ref>/conversation.jsonl`                  |
//! | claude   | newest `~/.claude/projects/<cwd-slug>/<session-uuid>.jsonl`  |
//! | codex    | newest `~/.codex/sessions/…/rollout-*.jsonl` whose           |
//! |          | `session_meta.payload.cwd` canonicalises to the given cwd    |
//!
//! `locate` picks the right variant; `read_turns` parses it into a
//! common `Turn` stream. The caller renders however they want (plain
//! text, JSON, colored TUI, etc.).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where a vendor stores conversation history for a given coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VendorHistory {
    /// `<workgraph>/chat/<ref>/conversation.jsonl` — nex's own format.
    Native(PathBuf),
    /// Newest `.jsonl` in `~/.claude/projects/<cwd-slug>/`.
    Claude(PathBuf),
    /// Newest rollout in `~/.codex/sessions/` whose `session_meta.payload.cwd`
    /// canonicalises to the coordinator's CWD.
    Codex(PathBuf),
}

impl VendorHistory {
    pub fn path(&self) -> &Path {
        match self {
            Self::Native(p) | Self::Claude(p) | Self::Codex(p) => p,
        }
    }
}

/// A single turn of conversation in a vendor-agnostic shape. `text` is
/// the display text (concatenated text blocks for multi-block
/// responses); richer structure is intentionally omitted at this
/// layer — callers wanting tool-use visibility should parse the raw
/// `path` themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// RFC3339 timestamp. Best-effort; some rows may lack one.
    pub timestamp: Option<String>,
    /// `"user"` or `"assistant"`. Vendor-specific "developer" /
    /// "system" / "tool" rows are filtered out.
    pub role: String,
    /// Concatenated visible text.
    pub text: String,
}

/// Pick the right history file for this executor. Returns `None` when
/// the executor has no file yet (fresh session, never spoken) or isn't
/// one we know about.
pub fn locate(
    executor: &str,
    workgraph_dir: &Path,
    chat_ref: &str,
    cwd: &Path,
) -> Option<VendorHistory> {
    match executor {
        "native" => {
            let p =
                crate::chat::chat_dir_for_ref(workgraph_dir, chat_ref).join("conversation.jsonl");
            if p.exists() {
                Some(VendorHistory::Native(p))
            } else {
                None
            }
        }
        "claude" => claude_newest_for_cwd(cwd).map(VendorHistory::Claude),
        "codex" => codex_newest_for_cwd(cwd).map(VendorHistory::Codex),
        _ => None,
    }
}

/// Parse the history file into a stream of turns, oldest first.
pub fn read_turns(hist: &VendorHistory) -> Result<Vec<Turn>> {
    match hist {
        VendorHistory::Native(p) => read_native(p),
        VendorHistory::Claude(p) => read_claude(p),
        VendorHistory::Codex(p) => read_codex(p),
    }
}

// ---------------------------------------------------------------------
// Native: <workgraph>/chat/<ref>/conversation.jsonl
// ---------------------------------------------------------------------

fn read_native(path: &Path) -> Result<Vec<Turn>> {
    let mut out = Vec::new();
    for line in read_lines(path)? {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let ty = val.get("entry_type").and_then(|v| v.as_str()).unwrap_or("");
        if ty != "message" {
            continue;
        }
        let role = val
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if role != "user" && role != "assistant" {
            continue;
        }
        let text = val
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| concat_text_blocks(blocks, "text", "text"))
            .unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let timestamp = val
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);
        out.push(Turn {
            timestamp,
            role,
            text,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Claude: ~/.claude/projects/<cwd-slug>/<session-uuid>.jsonl
// ---------------------------------------------------------------------

fn claude_project_dir(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let cwd_str = cwd.to_str()?;
    // Claude Code's project-slug scheme: absolute path, `/` → `-`.
    let slug = cwd_str.replace('/', "-");
    Some(home.join(".claude").join("projects").join(slug))
}

fn claude_newest_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let dir = claude_project_dir(cwd)?;
    newest_jsonl_in(&dir)
}

fn read_claude(path: &Path) -> Result<Vec<Turn>> {
    let mut out = Vec::new();
    for line in read_lines(path)? {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let ty = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "user" => {
                // Claude's session log stores user content as a plain
                // string (not block array), unlike the API shape.
                let text = val
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                if text.trim().is_empty() {
                    continue;
                }
                let timestamp = val
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                out.push(Turn {
                    timestamp,
                    role: "user".to_string(),
                    text: text.to_string(),
                });
            }
            "assistant" => {
                let text = val
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .map(|blocks| concat_text_blocks(blocks, "text", "text"))
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                let timestamp = val
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                out.push(Turn {
                    timestamp,
                    role: "assistant".to_string(),
                    text,
                });
            }
            _ => { /* queue-operation, ai-title, tool_result, attachment — skip */ }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Codex: ~/.codex/sessions/**/rollout-*.jsonl
// ---------------------------------------------------------------------

fn codex_sessions_root() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".codex").join("sessions"))
}

fn codex_newest_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let root = codex_sessions_root()?;
    let target = std::fs::canonicalize(cwd).ok()?;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    walk_jsonl(&root, &mut |path| {
        if !codex_rollout_cwd_matches(path, &target) {
            return;
        }
        if let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified())
            && best.as_ref().is_none_or(|(_, b)| mtime > *b)
        {
            best = Some((path.to_path_buf(), mtime));
        }
    });
    best.map(|(p, _)| p)
}

fn codex_rollout_cwd_matches(path: &Path, target_canonical: &Path) -> bool {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut first = String::new();
    if std::io::BufReader::new(file).read_line(&mut first).is_err() {
        return false;
    }
    let Ok(val) = serde_json::from_str::<serde_json::Value>(first.trim()) else {
        return false;
    };
    let session_cwd = val
        .get("payload")
        .and_then(|p| p.get("cwd"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if session_cwd.is_empty() {
        return false;
    }
    match std::fs::canonicalize(Path::new(session_cwd)) {
        Ok(c) => c == target_canonical,
        Err(_) => Path::new(session_cwd) == target_canonical,
    }
}

fn read_codex(path: &Path) -> Result<Vec<Turn>> {
    let mut out = Vec::new();
    for line in read_lines(path)? {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = val.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue; // developer / system / tool → skip
        }
        let text = payload
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| concat_text_blocks(blocks, "text", "text"))
            .unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let timestamp = val
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);
        out.push(Turn {
            timestamp,
            role: role.to_string(),
            text,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

fn read_lines(path: &Path) -> Result<Vec<String>> {
    use std::io::BufRead;
    let f = std::fs::File::open(path)?;
    Ok(std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .collect())
}

fn newest_jsonl_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

fn walk_jsonl(root: &Path, sink: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, sink);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            sink(&path);
        }
    }
}

/// Concat all blocks whose `type` field matches `expected_type` (or a
/// short alias like `input_text`/`output_text` for codex), joining with
/// newlines when both have content. Both claude and codex use blocks
/// with a `text` field, just under different type strings.
fn concat_text_blocks(blocks: &[serde_json::Value], _type_key: &str, text_key: &str) -> String {
    let mut out = String::new();
    for b in blocks {
        // Accept `text` (native, claude), `input_text` / `output_text` (codex).
        let ty = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let is_text = matches!(ty, "text" | "input_text" | "output_text");
        if !is_text {
            continue;
        }
        if let Some(t) = b.get(text_key).and_then(|v| v.as_str()) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn claude_project_dir_slugifies_path() {
        let path = Path::new("/home/erik/x");
        let result = claude_project_dir(path).unwrap();
        assert_eq!(
            result.file_name().unwrap().to_string_lossy(),
            "-home-erik-x"
        );
    }

    #[test]
    fn concat_text_blocks_skips_non_text() {
        let blocks = vec![
            serde_json::json!({"type": "thinking", "thinking": "..."}),
            serde_json::json!({"type": "text", "text": "Hello"}),
            serde_json::json!({"type": "tool_use", "name": "Bash"}),
            serde_json::json!({"type": "text", "text": "World"}),
        ];
        assert_eq!(concat_text_blocks(&blocks, "type", "text"), "Hello\nWorld");
    }

    #[test]
    fn concat_text_blocks_handles_codex_types() {
        let blocks = vec![
            serde_json::json!({"type": "input_text", "text": "Hello"}),
            serde_json::json!({"type": "output_text", "text": "World"}),
        ];
        assert_eq!(concat_text_blocks(&blocks, "type", "text"), "Hello\nWorld");
    }

    #[test]
    fn read_native_extracts_user_and_assistant_text() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("conversation.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"entry_type":"init","seq":1,"model":"x"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"entry_type":"message","seq":2,"role":"user","content":[{{"type":"text","text":"say hi"}}],"timestamp":"2026-04-21T03:44:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"entry_type":"message","seq":3,"role":"assistant","content":[{{"type":"text","text":"hi there"}}]}}"#
        )
        .unwrap();
        let turns = read_native(&path).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "say hi");
        assert_eq!(turns[0].timestamp.as_deref(), Some("2026-04-21T03:44:00Z"));
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].text, "hi there");
    }

    #[test]
    fn read_claude_extracts_string_user_and_block_assistant() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("abc.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"queue-operation","operation":"enqueue"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-04-22T18:32:58Z","message":{{"role":"user","content":"what's up?"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"..."}},{{"type":"text","text":"Hello!"}}]}}}}"#
        )
        .unwrap();
        let turns = read_claude(&path).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "what's up?");
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].text, "Hello!");
    }

    #[test]
    fn read_codex_extracts_response_items_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"cwd":"/tmp/x"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","timestamp":"2026-04-20T14:59:43Z","payload":{{"type":"message","role":"developer","content":[{{"type":"input_text","text":"skip me"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"hi"}}]}}}}"#
        )
        .unwrap();
        writeln!(f, r#"{{"type":"event_msg","payload":{{}}}}"#).unwrap();
        let turns = read_codex(&path).unwrap();
        assert_eq!(turns.len(), 2, "developer/event_msg rows filtered out");
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "hello");
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].text, "hi");
    }

    #[test]
    fn locate_native_returns_some_when_file_exists() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path();
        let chat_ref = "test-coord";
        let chat_dir = wg_dir.join("chat").join(chat_ref);
        std::fs::create_dir_all(&chat_dir).unwrap();
        std::fs::write(chat_dir.join("conversation.jsonl"), "").unwrap();
        let got = locate("native", wg_dir, chat_ref, &chat_dir);
        assert!(matches!(got, Some(VendorHistory::Native(_))));
    }

    #[test]
    fn locate_native_returns_none_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let got = locate("native", tmp.path(), "nope", tmp.path());
        assert!(got.is_none());
    }

    #[test]
    fn locate_unknown_executor_returns_none() {
        let tmp = TempDir::new().unwrap();
        let got = locate("llamacpp-direct", tmp.path(), "x", tmp.path());
        assert!(got.is_none());
    }
}
