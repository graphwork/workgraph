//! Handlers for `wg session ...` — chat-session management CLI.
//!
//! Every `wg nex` invocation — interactive CLI, coordinator,
//! task-agent — registers itself in `chat/sessions.json`. These
//! commands are the human-facing UX around that registry: list
//! sessions, attach to one (tail its outbox + `.streaming`), mint
//! new aliases, remove stale ones.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use workgraph::chat_sessions::SessionKind;

use crate::cli::{SessionAliasCommands, SessionCommands};

pub fn run(workgraph_dir: &Path, cmd: SessionCommands) -> Result<()> {
    match cmd {
        SessionCommands::List { json, short } => run_list(workgraph_dir, json, short),
        SessionCommands::Attach { session } => run_attach(workgraph_dir, &session),
        SessionCommands::New { alias, label } => run_new(workgraph_dir, &alias, label),
        SessionCommands::Fork { source, alias } => run_fork(workgraph_dir, &source, alias),
        SessionCommands::Alias { command } => match command {
            SessionAliasCommands::Add { session, alias } => {
                workgraph::chat_sessions::add_alias(workgraph_dir, &session, &alias)?;
                eprintln!(
                    "\x1b[32m[wg session]\x1b[0m added alias {:?} → {}",
                    alias, session
                );
                Ok(())
            }
            SessionAliasCommands::Rm { alias } => {
                workgraph::chat_sessions::remove_alias(workgraph_dir, &alias)?;
                eprintln!("\x1b[32m[wg session]\x1b[0m removed alias {:?}", alias);
                Ok(())
            }
        },
        SessionCommands::Rm { session } => {
            let uuid = workgraph::chat_sessions::resolve_ref(workgraph_dir, &session)?;
            workgraph::chat_sessions::delete_session(workgraph_dir, &session)?;
            eprintln!("\x1b[32m[wg session]\x1b[0m removed session {}", uuid);
            Ok(())
        }
        SessionCommands::Release { session, wait } => run_release(workgraph_dir, &session, wait),
        SessionCommands::Status { session } => run_status(workgraph_dir, &session),
        SessionCommands::Check { fix } => run_check(workgraph_dir, fix),
    }
}

/// Doctor: scan `chat/` + `sessions.json` for inconsistencies and
/// report them. With `--fix`, perform safe cleanup.
///
/// Checks performed:
///   1. Sessions in the registry whose `chat/<uuid>/` dir is missing.
///   2. `chat/<uuid>/` dirs with no registry entry (orphan storage).
///   3. Non-UUID filesystem entries under `chat/` that aren't
///      `sessions.json`: leftover legacy regular dirs or symlinks
///      from pre-full-UUID installs. These should be gone; if they
///      exist, `chat::chat_dir_for_ref` might still hit them
///      through the naive-join fallback — split-brain risk.
///   4. Stale session locks: `.handler.pid` files where the listed
///      PID is no longer alive.
///   5. Duplicate aliases in the registry (two sessions claiming
///      the same alias — shouldn't happen but we check).
fn run_check(workgraph_dir: &Path, fix: bool) -> Result<()> {
    use std::collections::HashSet;

    let chat_root = workgraph_dir.join("chat");
    if !chat_root.is_dir() {
        println!("\x1b[2mno chat/ dir yet — nothing to check\x1b[0m");
        return Ok(());
    }

    let reg = workgraph::chat_sessions::load(workgraph_dir).unwrap_or_default();
    let registered: HashSet<String> = reg.sessions.keys().cloned().collect();

    // 36-char canonical UUID; we don't want to require uuid crate parse
    // here — cheap shape check is enough to distinguish UUID-named dirs
    // from legacy alias paths like `0` or `coordinator-0`.
    let looks_like_uuid = |s: &str| s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4;

    let mut issues = 0usize;
    let mut fixed = 0usize;

    // (1) Registry → disk
    for (uuid, meta) in &reg.sessions {
        let dir = chat_root.join(uuid);
        if !dir.is_dir() {
            println!(
                "\x1b[33m⚠\x1b[0m missing dir for registered session {} ({:?})",
                &uuid[..8],
                meta.kind
            );
            issues += 1;
        }
    }

    // (2) + (3) Disk → registry + legacy paths
    let mut orphan_uuid_dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut legacy_paths: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&chat_root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy().to_string();
            if name_s == "sessions.json" {
                continue;
            }
            let path = entry.path();
            let md = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if looks_like_uuid(&name_s) {
                if !registered.contains(&name_s) && md.file_type().is_dir() {
                    orphan_uuid_dirs.push(path.clone());
                }
            } else {
                // Non-UUID entry — legacy alias path that shouldn't
                // exist anymore in full-UUID mode.
                legacy_paths.push(path.clone());
            }
        }
    }
    for p in &orphan_uuid_dirs {
        println!(
            "\x1b[33m⚠\x1b[0m orphan chat dir (no registry entry): {}",
            p.display()
        );
        issues += 1;
    }
    for p in &legacy_paths {
        let md = std::fs::symlink_metadata(p).ok();
        let kind = match md.as_ref().map(|m| m.file_type()) {
            Some(ft) if ft.is_symlink() => "symlink",
            Some(ft) if ft.is_dir() => "directory",
            Some(_) => "file",
            None => "unknown",
        };
        println!(
            "\x1b[33m⚠\x1b[0m legacy alias path at {} ({}): full-UUID expects registry-only aliases",
            p.display(),
            kind
        );
        issues += 1;
    }

    // (4) Stale locks
    for uuid in reg.sessions.keys() {
        let dir = chat_root.join(uuid);
        if !dir.is_dir() {
            continue;
        }
        if let Ok(Some(info)) = workgraph::session_lock::read_holder(&dir)
            && !info.alive
        {
            println!(
                "\x1b[33m⚠\x1b[0m stale lock on session {} (PID {} is dead, kind={:?})",
                &uuid[..8],
                info.pid,
                info.kind
            );
            issues += 1;
            if fix {
                let lock_path = workgraph::session_lock::SessionLock::lock_path(&dir);
                if std::fs::remove_file(&lock_path).is_ok() {
                    fixed += 1;
                    println!(
                        "  \x1b[32m✓\x1b[0m removed stale lock {}",
                        lock_path.display()
                    );
                }
            }
        }
    }

    // (5) Duplicate aliases
    let mut seen_aliases: std::collections::HashMap<String, Vec<String>> = Default::default();
    for (uuid, meta) in &reg.sessions {
        for a in &meta.aliases {
            seen_aliases
                .entry(a.clone())
                .or_default()
                .push(uuid.clone());
        }
    }
    for (alias, uuids) in &seen_aliases {
        if uuids.len() > 1 {
            println!(
                "\x1b[31m✗\x1b[0m alias {:?} is claimed by {} sessions: {}",
                alias,
                uuids.len(),
                uuids
                    .iter()
                    .map(|u| u[..8].to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            issues += 1;
        }
    }

    // Apply --fix for the cleanups we're confident about.
    if fix {
        // Merge-and-remove legacy alias paths (symlinks + regular dirs).
        // We only touch paths that have a corresponding registered
        // alias — other paths we leave alone (could be user data).
        let all_aliases: HashSet<String> = reg
            .sessions
            .values()
            .flat_map(|m| m.aliases.iter().cloned())
            .collect();
        for p in &legacy_paths {
            let name = match p.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !all_aliases.contains(&name) {
                continue; // unregistered, leave it alone
            }
            // Trigger re-registration by resolving the alias; the
            // alias_symlink path does the merge+remove dance.
            if let Ok(uuid) = workgraph::chat_sessions::resolve_ref(workgraph_dir, &name) {
                if workgraph::chat_sessions::add_alias(workgraph_dir, &uuid, &name).is_ok() {
                    fixed += 1;
                    println!(
                        "  \x1b[32m✓\x1b[0m cleaned legacy alias path {}",
                        p.display()
                    );
                }
            }
        }
    }

    if issues == 0 {
        println!("\x1b[32m✓\x1b[0m no issues found — registry and chat/ are consistent");
    } else if fix {
        println!(
            "\n{} issues found, {} auto-fixed. Remaining need manual triage.",
            issues, fixed
        );
    } else {
        println!(
            "\n{} issues found. Rerun with --fix to auto-repair.",
            issues
        );
    }
    Ok(())
}

/// Resolve `ref` to a chat dir path. Tries the session registry
/// first; falls back to `<wg>/chat/<ref>` if that directory exists
/// on disk. Makes release/status work on chat-dirs that weren't
/// registered via `ensure_session` (e.g., raw `wg nex --chat foo`
/// invocations).
fn resolve_chat_dir(workgraph_dir: &Path, session: &str) -> Result<std::path::PathBuf> {
    if let Ok(uuid) = workgraph::chat_sessions::resolve_ref(workgraph_dir, session) {
        return Ok(workgraph_dir.join("chat").join(uuid));
    }
    let direct = workgraph_dir.join("chat").join(session);
    if direct.exists() {
        return Ok(direct);
    }
    anyhow::bail!(
        "session reference {:?} did not match any UUID, prefix, alias, or chat dir",
        session
    );
}

fn run_release(workgraph_dir: &Path, session: &str, wait_secs: u64) -> Result<()> {
    let chat_dir = resolve_chat_dir(workgraph_dir, session)?;
    match workgraph::session_lock::read_holder(&chat_dir)? {
        None => {
            eprintln!(
                "\x1b[2m[wg session]\x1b[0m {} has no live handler — nothing to release",
                session
            );
            return Ok(());
        }
        Some(info) if !info.alive => {
            eprintln!(
                "\x1b[33m[wg session]\x1b[0m {} lock is stale (pid {} not running) — clearing",
                session, info.pid
            );
            // Stale lock — just remove it.
            let _ =
                std::fs::remove_file(workgraph::session_lock::SessionLock::lock_path(&chat_dir));
            return Ok(());
        }
        Some(info) => {
            eprintln!(
                "\x1b[1;33m[wg session]\x1b[0m asking handler (pid={} kind={}) on {} to release...",
                info.pid,
                info.kind.map(|k| k.label()).unwrap_or("unknown"),
                session
            );
            workgraph::session_lock::request_release(&chat_dir)?;
        }
    }
    if wait_secs == 0 {
        eprintln!("\x1b[2m[wg session]\x1b[0m release requested (not waiting for completion)");
        return Ok(());
    }
    match workgraph::session_lock::wait_for_release(
        &chat_dir,
        std::time::Duration::from_secs(wait_secs),
    ) {
        Ok(()) => {
            eprintln!("\x1b[32m[wg session]\x1b[0m {} released", session);
            Ok(())
        }
        Err(_) => {
            eprintln!(
                "\x1b[33m[wg session]\x1b[0m {} handler did not release within {}s — may be mid-tool-call",
                session, wait_secs
            );
            eprintln!(
                "\x1b[2m  The release marker is still set; handler will exit at its next turn boundary\x1b[0m"
            );
            Ok(())
        }
    }
}

fn run_status(workgraph_dir: &Path, session: &str) -> Result<()> {
    let chat_dir = resolve_chat_dir(workgraph_dir, session)?;
    match workgraph::session_lock::read_holder(&chat_dir)? {
        None => {
            println!("{}: no handler", session);
        }
        Some(info) => {
            let alive_label = if info.alive { "live" } else { "STALE" };
            println!(
                "{}: {} pid={} kind={} started={}",
                session,
                alive_label,
                info.pid,
                info.kind.map(|k| k.label()).unwrap_or("unknown"),
                info.started_at
            );
        }
    }
    Ok(())
}

fn run_list(workgraph_dir: &Path, json: bool, short: bool) -> Result<()> {
    let sessions = workgraph::chat_sessions::list(workgraph_dir)?;
    if json {
        let value: Vec<_> = sessions
            .iter()
            .map(|(uuid, meta)| {
                serde_json::json!({
                    "uuid": uuid,
                    "kind": format!("{:?}", meta.kind).to_lowercase(),
                    "created": meta.created,
                    "aliases": meta.aliases,
                    "label": meta.label,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    if sessions.is_empty() {
        eprintln!("\x1b[2m[wg session]\x1b[0m no sessions registered");
        return Ok(());
    }
    // Default: full 36-char UUID. `--short` truncates to 8-char
    // prefixes (git-log-oneline style). The full form is the honest
    // display; users asked for it after the previous version hid it.
    let uuid_col_width = if short { 8 } else { 36 };
    println!(
        "{:<width$} {:<12} {:<40} LABEL",
        "UUID",
        "KIND",
        "ALIASES",
        width = uuid_col_width
    );
    for (uuid, meta) in sessions {
        let shown: &str = if short {
            &uuid[..std::cmp::min(uuid.len(), 8)]
        } else {
            &uuid
        };
        let kind = format!("{:?}", meta.kind).to_lowercase();
        let aliases = if meta.aliases.is_empty() {
            "-".to_string()
        } else {
            meta.aliases.join(",")
        };
        let label = meta.label.clone().unwrap_or_default();
        println!(
            "{:<width$} {:<12} {:<40} {}",
            shown,
            kind,
            aliases,
            label,
            width = uuid_col_width
        );
    }
    Ok(())
}

fn run_fork(workgraph_dir: &Path, source: &str, alias: Option<String>) -> Result<()> {
    let fork_uuid = workgraph::chat_sessions::fork_session(workgraph_dir, source, alias.clone())?;
    let reg = workgraph::chat_sessions::load(workgraph_dir)?;
    let meta = reg
        .sessions
        .get(&fork_uuid)
        .ok_or_else(|| anyhow::anyhow!("fork not in registry"))?;
    let handle = meta
        .aliases
        .first()
        .cloned()
        .unwrap_or_else(|| fork_uuid.clone());
    eprintln!(
        "\x1b[32m[wg session]\x1b[0m forked {} → {} (alias: {})",
        source, fork_uuid, handle
    );
    eprintln!("\x1b[2m  Resume it with: \x1b[0mwg nex --chat {}", handle);
    println!("{}", fork_uuid);
    Ok(())
}

fn run_new(workgraph_dir: &Path, alias: &str, label: Option<String>) -> Result<()> {
    let uuid = workgraph::chat_sessions::create_session(
        workgraph_dir,
        SessionKind::Other,
        &[alias.to_string()],
        label,
    )?;
    eprintln!(
        "\x1b[32m[wg session]\x1b[0m created session {} alias={:?}",
        uuid, alias
    );
    println!("{}", uuid);
    Ok(())
}

/// Tail a session's `.streaming` + `outbox.jsonl` to stderr so the
/// human can watch the session's output as it's produced.
///
/// This is read-only. Sending input to the session is a different
/// operation (`wg chat send`, or direct `wg nex --chat <ref>`).
/// Eventually a flag like `--bidir` would make this the full
/// interactive attach.
fn run_attach(workgraph_dir: &Path, session_ref: &str) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::{RecvTimeoutError, channel};

    // Tolerate bare chat-dir refs (same pattern as release/status)
    // so `wg session attach .coordinator-0` works even for sessions
    // that weren't registered through `ensure_session`. The TUI's
    // observer pane spawns this for the active coordinator's task.
    let chat_dir = resolve_chat_dir(workgraph_dir, session_ref)?;
    let display_ref = chat_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(session_ref);
    eprintln!("\x1b[1;32m[wg session attach]\x1b[0m {}", display_ref);
    let streaming = chat_dir.join(".streaming");
    let outbox = chat_dir.join("outbox.jsonl");

    // Print whatever's already in .streaming so the user sees the
    // current in-flight turn on attach.
    if let Ok(txt) = std::fs::read_to_string(&streaming)
        && !txt.is_empty()
    {
        eprintln!("\x1b[2m[in-flight turn]\x1b[0m");
        eprint!("{}", txt);
    }

    // Tail outbox.jsonl line-by-line. We start from EOF (new turns
    // only) rather than replaying the whole history.
    let mut outbox_pos: u64 = if let Ok(meta) = std::fs::metadata(&outbox) {
        meta.len()
    } else {
        0
    };

    // Set up an inotify (or FSEvents on macOS) watcher on the chat
    // dir so we wake sub-millisecond when anything changes, instead
    // of polling at human-eyeblink granularity. A 2s timeout on the
    // recv is the safety-net floor — if an event gets dropped we
    // still re-scan within that window.
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("create filesystem watcher for attach")?;
    watcher
        .watch(&chat_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch {:?}", chat_dir))?;

    eprintln!("\x1b[2m[attached — Ctrl-C to detach]\x1b[0m");
    let idle_timeout = Duration::from_secs(2);
    let mut last_streaming = String::new();
    loop {
        // Wait for a filesystem event OR the idle timeout, whichever
        // comes first. Drain any burst so we don't rerun the scan N
        // times for N coalesced events.
        match rx.recv_timeout(idle_timeout) {
            Ok(_) => while rx.try_recv().is_ok() {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Streaming: print the diff since last seen.
        if let Ok(current) = std::fs::read_to_string(&streaming)
            && current != last_streaming
        {
            if current.starts_with(&last_streaming) {
                eprint!("{}", &current[last_streaming.len()..]);
            } else {
                // Streaming got cleared (turn finished) or overwritten.
                eprintln!();
                eprint!("{}", current);
            }
            last_streaming = current;
        }
        // Outbox: read any new bytes and print each new turn.
        if let Ok(mut f) = std::fs::File::open(&outbox) {
            let len = f.metadata().ok().map(|m| m.len()).unwrap_or(0);
            if len > outbox_pos {
                let _ = f.seek(SeekFrom::Start(outbox_pos));
                let reader = BufReader::new(f);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(msg) = serde_json::from_str::<workgraph::chat::ChatMessage>(&line) {
                        eprintln!("\x1b[1;36m↳ {}\x1b[0m {}", msg.request_id, msg.content);
                        last_streaming.clear();
                    }
                }
                outbox_pos = len;
            }
        }
    }
    Ok(())
}
