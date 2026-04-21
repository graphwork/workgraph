//! Session registry for chat-file nex sessions.
//!
//! Every nex session — interactive, coordinator, or task-agent —
//! lives under `<workgraph>/chat/<uuid>/` with the same file layout
//! (inbox.jsonl, outbox.jsonl, .streaming, conversation.jsonl, ...).
//! A session is identified by its UUID. Humans and legacy code
//! address sessions by **alias**, which resolves to a UUID via
//! this registry.
//!
//! Aliases:
//! - `coordinator-0`, `coordinator-1`, ... for workgraph coordinators
//!   (what used to be numeric `chat/0/`, `chat/1/` directly)
//! - `task-<task-id>` for task-agent sessions
//! - `tty-<slug>` for interactive sessions pinned to a terminal
//! - Arbitrary user-chosen aliases (e.g. `debug-redis`) via
//!   `wg chat new --alias X`
//!
//! The registry is a single JSON file at
//! `<workgraph>/chat/sessions.json` plus one filesystem symlink per
//! alias (`chat/<alias>` → `chat/<uuid>`). Symlinks mean existing
//! code that writes `chat/0/inbox.jsonl` keeps working unchanged —
//! the kernel resolves the alias for us. The JSON registry is the
//! authoritative listing (for `wg chat list`, attach-by-prefix,
//! dangling-alias cleanup).
//!
//! Resolution order for `resolve_ref`:
//! 1. Exact UUID match (string equality on the 36-char form)
//! 2. Exact alias match
//! 3. Unambiguous UUID prefix (≥4 chars, like git short hashes)
//! 4. Error
//!
//! The registry is read on every call (cheap JSON parse) rather than
//! cached in-memory — this sidesteps the "two processes editing
//! sessions.json" coordination problem. Writes take a file lock.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What kind of nex session this is. Surfaces in `wg chat list` and
/// lets the TUI group sessions by role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionKind {
    /// Long-running daemon coordinator (historical `chat/0/`).
    Coordinator,
    /// Autonomous task agent spawned by the coordinator for a graph task.
    TaskAgent,
    /// A human at a terminal running `wg nex`.
    Interactive,
    /// An evaluator run, a /skill session, or anything else
    /// explicitly classified later.
    Other,
}

/// Per-session metadata. UUID is the dir name; this struct is the
/// entry in `chat/sessions.json` keyed by UUID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub kind: SessionKind,
    /// ISO-8601 timestamp of registration.
    pub created: String,
    /// Human handles. Must each be unique across the whole registry —
    /// `register_alias` enforces this. Empty is allowed (UUID-only
    /// session, still addressable by its UUID).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Optional free-form label for `wg chat list` display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// UUID of the parent session if this one was forked. Populated
    /// by `fork_session`. Forked sessions start with a copy of the
    /// parent's journal at fork time and then evolve independently.
    /// `wg session list` shows a `forked-from <short>` annotation
    /// when this is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

/// The on-disk registry file shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub sessions: HashMap<String, SessionMeta>,
}

fn default_version() -> u32 {
    1
}

/// Path to the registry file.
pub fn registry_path(workgraph_dir: &Path) -> PathBuf {
    workgraph_dir.join("chat").join("sessions.json")
}

/// Path to the chat-dir for a given UUID.
pub fn chat_dir_for_uuid(workgraph_dir: &Path, uuid: &str) -> PathBuf {
    workgraph_dir.join("chat").join(uuid)
}

/// Load the registry, returning an empty one if the file doesn't exist.
pub fn load(workgraph_dir: &Path) -> Result<Registry> {
    let path = registry_path(workgraph_dir);
    if !path.exists() {
        return Ok(Registry::default());
    }
    let mut s = String::new();
    File::open(&path)
        .with_context(|| format!("open {:?}", path))?
        .read_to_string(&mut s)?;
    if s.trim().is_empty() {
        return Ok(Registry::default());
    }
    let reg: Registry =
        serde_json::from_str(&s).with_context(|| format!("parse registry {:?}", path))?;
    Ok(reg)
}

/// Atomically save the registry. Writes to a temp file then renames
/// so a concurrent reader never sees a half-written file.
pub fn save(workgraph_dir: &Path, reg: &Registry) -> Result<()> {
    let path = registry_path(workgraph_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        let json = serde_json::to_string_pretty(reg)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Create a new session UUID, directory, and registry entry.
/// Optionally adds aliases (each creates a symlink under `chat/`).
///
/// Returns the new UUID.
pub fn create_session(
    workgraph_dir: &Path,
    kind: SessionKind,
    aliases: &[String],
    label: Option<String>,
) -> Result<String> {
    let uuid = Uuid::new_v4().to_string();
    let dir = chat_dir_for_uuid(workgraph_dir, &uuid);
    fs::create_dir_all(&dir).with_context(|| format!("create_dir_all {:?}", dir))?;

    // Register in the JSON index first so a crashed symlink-creation
    // doesn't leave an unregistered session orphan.
    let mut reg = load(workgraph_dir).unwrap_or_default();
    for a in aliases {
        if let Some(existing) = find_by_alias(&reg, a) {
            bail!("alias {:?} already points to session {}", a, existing.0);
        }
    }
    reg.sessions.insert(
        uuid.clone(),
        SessionMeta {
            kind,
            created: Utc::now().to_rfc3339(),
            aliases: aliases.to_vec(),
            label,
            forked_from: None,
        },
    );
    save(workgraph_dir, &reg)?;

    // Then make the alias symlinks point at the UUID dir.
    for a in aliases {
        create_alias_symlink(workgraph_dir, a, &uuid)?;
    }
    Ok(uuid)
}

/// Fork an existing session: copy its journal (`conversation.jsonl`
/// and `session-summary.md`) into a fresh UUID-named dir and register
/// the new session with `forked_from = <source_uuid>`.
///
/// The fork is an independent session from that point forward — its
/// own inbox, outbox, cursor, streaming file. Writing to it doesn't
/// affect the parent, and vice versa. Future messages evolve
/// independently.
///
/// `source_ref` accepts the same formats as `resolve_ref`: UUID,
/// UUID prefix, or alias. `new_alias` is optional; when omitted, the
/// fork gets a generated `fork-<short>` alias so it's addressable.
///
/// Returns the fork's UUID.
pub fn fork_session(
    workgraph_dir: &Path,
    source_ref: &str,
    new_alias: Option<String>,
) -> Result<String> {
    let source_uuid = resolve_ref(workgraph_dir, source_ref)?;
    let source_dir = chat_dir_for_uuid(workgraph_dir, &source_uuid);
    if !source_dir.exists() {
        bail!("source session {} has no chat dir on disk", source_uuid);
    }

    // Allocate the fork's UUID and set up its directory.
    let fork_uuid = Uuid::new_v4().to_string();
    let fork_dir = chat_dir_for_uuid(workgraph_dir, &fork_uuid);
    fs::create_dir_all(&fork_dir).with_context(|| format!("create_dir_all {:?}", fork_dir))?;

    // Copy journal + session summary. Skip inbox/outbox/streaming —
    // those are per-session live state, not history; the fork starts
    // with an empty inbox ready for fresh input.
    for name in ["conversation.jsonl", "session-summary.md"] {
        let src = source_dir.join(name);
        if src.exists() {
            let dst = fork_dir.join(name);
            fs::copy(&src, &dst).with_context(|| format!("copy {:?} -> {:?}", src, dst))?;
        }
    }

    // Pick or generate the fork's alias.
    let short = &fork_uuid[..8];
    let alias = new_alias.unwrap_or_else(|| format!("fork-{}", short));

    // Carry over the parent's SessionKind when it's interactive-ish;
    // coordinator/task-agent forks are rare and the user can
    // re-classify via the registry if needed.
    let reg = load(workgraph_dir).unwrap_or_default();
    let parent_kind = reg
        .sessions
        .get(&source_uuid)
        .map(|m| m.kind)
        .unwrap_or(SessionKind::Interactive);
    let parent_label = reg
        .sessions
        .get(&source_uuid)
        .and_then(|m| m.label.clone())
        .unwrap_or_else(|| source_uuid.clone());

    // Check alias isn't already in use.
    if let Some((existing, _)) = find_by_alias(&reg, &alias) {
        bail!(
            "alias {:?} already points to session {} — pass a different `new_alias`",
            alias,
            existing
        );
    }

    // Insert the new session meta.
    let mut reg = reg;
    reg.sessions.insert(
        fork_uuid.clone(),
        SessionMeta {
            kind: parent_kind,
            created: Utc::now().to_rfc3339(),
            aliases: vec![alias.clone()],
            label: Some(format!("fork of: {}", parent_label)),
            forked_from: Some(source_uuid),
        },
    );
    save(workgraph_dir, &reg)?;

    // Install the alias symlink.
    create_alias_symlink(workgraph_dir, &alias, &fork_uuid)?;

    Ok(fork_uuid)
}

/// Register a coordinator session the way the daemon needs it.
///
/// Installs BOTH aliases for the coordinator's UUID:
///   * `coordinator-<N>` — the handle the subprocess is spawned
///     with (`wg nex --chat coordinator-<N>`) and that `wg session
///     list` surfaces.
///   * `<N>` (bare numeric) — the path that the legacy
///     `chat::append_inbox_for(dir, N, …)` API writes to. This
///     API is still used by the IPC `UserChat` handler (the TUI's
///     `wg chat` → daemon path). Without this alias, the IPC's
///     writes land in a disconnected `chat/<N>/` real directory
///     and the subprocess (inotify-watching `chat/coordinator-<N>/…`)
///     never sees them — which manifests as "TUI chat never replies."
///
/// Also migrates any pre-existing `chat/<N>/` real directory from
/// a previous non-aliased daemon version. Idempotent across
/// restart cycles; returns the session's UUID.
///
/// All coordinator startup paths should go through this function.
/// The unit test
/// `daemon_style_coordinator_registration_creates_both_paths`
/// locks in the invariant.
pub fn register_coordinator_session(workgraph_dir: &Path, n: u32) -> Result<String> {
    let _ = migrate_numeric_coord_dir(workgraph_dir, n);
    let canonical = format!("coordinator-{}", n);
    let uuid = ensure_session(
        workgraph_dir,
        &canonical,
        SessionKind::Coordinator,
        Some(format!("coordinator {}", n)),
    )?;
    // Install the bare numeric alias. add_alias errors "already
    // points to …" on restart, which is the steady-state case
    // after the first call — swallow that specific error, surface
    // anything else.
    match add_alias(workgraph_dir, &canonical, &n.to_string()) {
        Ok(()) => {}
        Err(e) => {
            let msg = format!("{}", e);
            if !msg.contains("already") {
                return Err(e);
            }
        }
    }
    Ok(uuid)
}

/// Ensure a session with the given alias exists, creating it if not.
/// Idempotent — a second call with the same alias returns the existing
/// UUID without creating a new session. Intended for callers like the
/// coordinator supervisor that want a stable UUID behind a well-known
/// alias (`coordinator-0`) without racing on startup.
pub fn ensure_session(
    workgraph_dir: &Path,
    alias: &str,
    kind: SessionKind,
    label: Option<String>,
) -> Result<String> {
    let reg = load(workgraph_dir).unwrap_or_default();
    if let Some((uuid, _)) = find_by_alias(&reg, alias) {
        // Double-check the symlink points where we think — idempotent
        // repair in case a bare chat dir exists without its alias link.
        let _ = create_alias_symlink(workgraph_dir, alias, &uuid);
        return Ok(uuid);
    }
    create_session(workgraph_dir, kind, &[alias.to_string()], label)
}

/// Resolve a reference (UUID, prefix, or alias) to a UUID.
pub fn resolve_ref(workgraph_dir: &Path, reference: &str) -> Result<String> {
    let reg = load(workgraph_dir).unwrap_or_default();

    // 1. Exact UUID (36-char canonical form).
    if reg.sessions.contains_key(reference) {
        return Ok(reference.to_string());
    }

    // 2. Exact alias.
    if let Some((uuid, _)) = find_by_alias(&reg, reference) {
        return Ok(uuid);
    }

    // 3. UUID prefix (≥4 chars, must be unambiguous).
    if reference.len() >= 4 {
        let matches: Vec<_> = reg
            .sessions
            .keys()
            .filter(|k| k.starts_with(reference))
            .cloned()
            .collect();
        match matches.len() {
            0 => {}
            1 => return Ok(matches.into_iter().next().unwrap()),
            _ => bail!(
                "ambiguous session prefix {:?}: {} matches — be more specific",
                reference,
                matches.len()
            ),
        }
    }

    Err(anyhow!(
        "session reference {:?} did not match any UUID, prefix, or alias",
        reference
    ))
}

/// Find a session by alias. Returns (UUID, metadata) on match.
pub fn find_by_alias<'a>(reg: &'a Registry, alias: &str) -> Option<(String, &'a SessionMeta)> {
    for (uuid, meta) in &reg.sessions {
        if meta.aliases.iter().any(|a| a == alias) {
            return Some((uuid.clone(), meta));
        }
    }
    None
}

/// Create (or refresh) a symlink `chat/<alias>` → `<uuid>`.
/// The target is relative so the whole workgraph dir stays movable.
fn create_alias_symlink(workgraph_dir: &Path, alias: &str, uuid: &str) -> Result<()> {
    let link = workgraph_dir.join("chat").join(alias);
    let target_dir = workgraph_dir.join("chat").join(uuid);
    let metadata = fs::symlink_metadata(&link).ok();
    if let Some(md) = metadata {
        if md.file_type().is_symlink() {
            // Existing symlink — safe to blow away and recreate.
            let _ = fs::remove_file(&link);
        } else if md.file_type().is_dir() {
            // Real directory sitting where our alias should be. Can
            // happen when some code path called `append_inbox_for(_, N, …)`
            // (or similar) before the session was ever registered —
            // `chat::append_message` creates missing parent dirs, so
            // the path becomes a regular chat dir. Leaving it there
            // causes split-brain: the IPC path writes here, the
            // handler reads the UUID dir via the other alias, and
            // messages never meet. Merge-and-replace: move the
            // legacy dir's contents into the UUID dir (which is the
            // real storage), then replace the legacy dir with a
            // symlink.
            merge_legacy_chat_dir(&link, &target_dir).with_context(|| {
                format!(
                    "merging legacy chat dir {:?} into UUID dir {:?}",
                    link, target_dir
                )
            })?;
            fs::remove_dir_all(&link)
                .with_context(|| format!("removing merged-away legacy chat dir {:?}", link))?;
        } else {
            // Regular file — remove and continue.
            let _ = fs::remove_file(&link);
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(uuid, &link)
            .with_context(|| format!("symlink {:?} -> {}", link, uuid))?;
    }
    #[cfg(not(unix))]
    {
        // Windows: not supported in this pass. The JSON registry
        // still works; only path-based back-compat is unavailable.
        let _ = uuid;
        bail!("alias symlinks are not supported on non-Unix targets");
    }
    Ok(())
}

/// Move files from a legacy numeric chat dir into the canonical
/// UUID chat dir, concatenating JSONL logs instead of overwriting.
/// Used by `create_alias_symlink` to resolve split-brain cases
/// where `chat/0/` got created as a regular directory before the
/// alias symlink was installed.
///
/// Behavior:
/// - `inbox.jsonl` / `outbox.jsonl` / `chat.log`: appended to the
///   UUID dir's copy (so no history is lost).
/// - Other files (`.streaming`, `.handler.pid`, cursors, compactor
///   state): copied only if not present in the UUID dir.
/// - Lock sidecars are skipped — they're per-process and get
///   recreated on demand.
fn merge_legacy_chat_dir(legacy: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("create target dir {:?}", target))?;
    let Ok(entries) = fs::read_dir(legacy) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".lock") {
            continue;
        }
        let src = entry.path();
        let dst = target.join(&name);
        let is_append_target = matches!(
            name_str.as_ref(),
            "inbox.jsonl" | "outbox.jsonl" | "chat.log"
        );
        if is_append_target {
            // Append src contents to dst. JSONL concatenation is
            // safe because every row is self-contained.
            let bytes = fs::read(&src).with_context(|| format!("read {:?}", src))?;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&dst)
                .with_context(|| format!("open-for-append {:?}", dst))?;
            use std::io::Write;
            file.write_all(&bytes)
                .with_context(|| format!("append to {:?}", dst))?;
        } else if !dst.exists() {
            fs::copy(&src, &dst).with_context(|| format!("copy {:?} -> {:?}", src, dst))?;
        }
        // Dst already exists and isn't a log → leave target's copy
        // intact. Legacy writer's version is discarded.
    }
    Ok(())
}

/// Add an alias to an existing session (UUID or existing alias).
pub fn add_alias(workgraph_dir: &Path, reference: &str, alias: &str) -> Result<()> {
    let uuid = resolve_ref(workgraph_dir, reference)?;
    let mut reg = load(workgraph_dir).unwrap_or_default();
    if let Some((existing, _)) = find_by_alias(&reg, alias)
        && existing != uuid
    {
        bail!(
            "alias {:?} already points to a different session ({})",
            alias,
            existing
        );
    }
    if let Some(meta) = reg.sessions.get_mut(&uuid)
        && !meta.aliases.iter().any(|a| a == alias)
    {
        meta.aliases.push(alias.to_string());
    }
    save(workgraph_dir, &reg)?;
    create_alias_symlink(workgraph_dir, alias, &uuid)?;
    Ok(())
}

/// Remove an alias (and its symlink). The session itself stays.
pub fn remove_alias(workgraph_dir: &Path, alias: &str) -> Result<()> {
    let mut reg = load(workgraph_dir).unwrap_or_default();
    let Some((uuid, _)) = find_by_alias(&reg, alias) else {
        bail!("no such alias {:?}", alias);
    };
    if let Some(meta) = reg.sessions.get_mut(&uuid) {
        meta.aliases.retain(|a| a != alias);
    }
    save(workgraph_dir, &reg)?;
    let link = workgraph_dir.join("chat").join(alias);
    let _ = fs::remove_file(link);
    Ok(())
}

/// Delete a session entirely (registry entry + symlinks + chat dir).
/// Destructive — no undo.
pub fn delete_session(workgraph_dir: &Path, reference: &str) -> Result<()> {
    let uuid = resolve_ref(workgraph_dir, reference)?;
    let mut reg = load(workgraph_dir).unwrap_or_default();
    if let Some(meta) = reg.sessions.remove(&uuid) {
        for a in &meta.aliases {
            let link = workgraph_dir.join("chat").join(a);
            let _ = fs::remove_file(link);
        }
    }
    save(workgraph_dir, &reg)?;
    let dir = chat_dir_for_uuid(workgraph_dir, &uuid);
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("rm -rf {:?}", dir))?;
    }
    Ok(())
}

/// Return a sorted list of (UUID, meta) for display.
pub fn list(workgraph_dir: &Path) -> Result<Vec<(String, SessionMeta)>> {
    let reg = load(workgraph_dir)?;
    let mut out: Vec<_> = reg.sessions.into_iter().collect();
    out.sort_by(|a, b| a.1.created.cmp(&b.1.created));
    Ok(out)
}

/// Migrate an existing numeric coord dir (`chat/0`, `chat/1`, …) to a
/// UUID-named dir with the corresponding `coordinator-N` alias.
/// Idempotent — if `chat/N` is already a symlink into a UUID dir, it's
/// left alone. If `chat/N` is a real directory with content, its
/// contents are moved to `chat/<new-uuid>` and the original path is
/// re-created as a symlink. This lets older daemons that wrote to
/// `chat/0/` coexist with new UUID-aware ones without losing history.
pub fn migrate_numeric_coord_dir(workgraph_dir: &Path, n: u32) -> Result<Option<String>> {
    let old = workgraph_dir.join("chat").join(n.to_string());
    if !old.exists() {
        return Ok(None);
    }
    // Already a symlink — assume prior migration succeeded.
    if old.is_symlink() {
        return Ok(None);
    }

    let alias = format!("coordinator-{}", n);
    let numeric_alias = n.to_string();
    let reg = load(workgraph_dir).unwrap_or_default();

    // If `coordinator-N` is already registered, don't create a
    // duplicate session. This happens when an older subprocess left
    // behind a bare `chat/N/` dir while the new registry-aware
    // daemon had already registered the session under a UUID. Merge
    // instead: move any files from the legacy dir into the existing
    // session's dir (skipping files that would overwrite — those
    // are newer and belong to the registered session), then install
    // the `chat/N` → `<uuid>` symlink.
    if let Some((existing_uuid, _)) = find_by_alias(&reg, &alias) {
        let target_dir = chat_dir_for_uuid(workgraph_dir, &existing_uuid);
        fs::create_dir_all(&target_dir).ok();
        // Merge files from old dir into target_dir. Files that would
        // collide are kept at the target (the registered session's
        // data is the authoritative one).
        if let Ok(entries) = fs::read_dir(&old) {
            for entry in entries.flatten() {
                let src = entry.path();
                let dest = target_dir.join(entry.file_name());
                if dest.exists() {
                    // Keep the registered version; drop the orphan.
                    if src.is_dir() {
                        let _ = fs::remove_dir_all(&src);
                    } else {
                        let _ = fs::remove_file(&src);
                    }
                } else {
                    let _ = fs::rename(&src, &dest);
                }
            }
        }
        // Remove the now-empty old dir and install the alias
        // symlink + numeric alias.
        let _ = fs::remove_dir_all(&old);
        create_alias_symlink(workgraph_dir, &numeric_alias, &existing_uuid)?;
        // Also ensure the numeric alias is in the registry entry.
        let mut reg2 = load(workgraph_dir).unwrap_or_default();
        if let Some(meta) = reg2.sessions.get_mut(&existing_uuid)
            && !meta.aliases.iter().any(|a| a == &numeric_alias)
        {
            meta.aliases.push(numeric_alias.clone());
            save(workgraph_dir, &reg2)?;
        }
        return Ok(Some(existing_uuid));
    }

    // No existing alias — standard migration path. Create a fresh
    // UUID dir, move the legacy contents in, register with both
    // aliases.
    let uuid = Uuid::new_v4().to_string();
    let new_dir = chat_dir_for_uuid(workgraph_dir, &uuid);
    fs::rename(&old, &new_dir).with_context(|| format!("migrate {:?} -> {:?}", old, new_dir))?;

    let mut reg = load(workgraph_dir).unwrap_or_default();
    reg.sessions.insert(
        uuid.clone(),
        SessionMeta {
            kind: SessionKind::Coordinator,
            created: Utc::now().to_rfc3339(),
            aliases: vec![alias.clone(), numeric_alias.clone()],
            label: Some(format!("coordinator {} (migrated)", n)),
            forked_from: None,
        },
    );
    save(workgraph_dir, &reg)?;

    create_alias_symlink(workgraph_dir, &alias, &uuid)?;
    create_alias_symlink(workgraph_dir, &numeric_alias, &uuid)?;
    Ok(Some(uuid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Regression: if `chat/0` was created as a regular directory
    /// BEFORE `register_coordinator_session` ran (legacy code path
    /// or early daemon crash), the symlink install used to fail
    /// silently. `chat/0` and `chat/coordinator-0` then pointed to
    /// two different filesystem entities, TUI writes went to one,
    /// handler reads came from the other, and user messages were
    /// never seen. This test locks in the merge-and-replace fix.
    #[test]
    fn register_merges_legacy_regular_chat_dir_into_uuid_dir() {
        use std::fs;
        let dir = tempdir().unwrap();
        let wg = dir.path();

        // Simulate a legacy `chat/0` regular dir left by an older
        // daemon run — a bare inbox with some rows.
        let legacy = wg.join("chat").join("0");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(
            legacy.join("inbox.jsonl"),
            "{\"id\":1,\"timestamp\":\"2026-01-01T00:00:00Z\",\"role\":\"user\",\
             \"content\":\"legacy-row\",\"request_id\":\"legacy-1\"}\n",
        )
        .unwrap();

        // Now register the session — this is what the daemon does
        // at startup. The `0` alias should end up as a symlink into
        // the UUID dir, and the legacy row should be preserved in
        // the UUID dir's inbox.
        let uuid = register_coordinator_session(wg, 0).unwrap();

        // `chat/0` is now a symlink, not a regular dir.
        let link_meta = fs::symlink_metadata(wg.join("chat").join("0")).unwrap();
        assert!(
            link_meta.file_type().is_symlink(),
            "chat/0 should be a symlink after register, not a dir"
        );

        // The legacy row survived the merge.
        let uuid_inbox = wg.join("chat").join(&uuid).join("inbox.jsonl");
        let contents = fs::read_to_string(&uuid_inbox).unwrap();
        assert!(
            contents.contains("legacy-row"),
            "merge_legacy_chat_dir should have concatenated the legacy inbox into the UUID dir"
        );
    }

    #[test]
    fn create_and_resolve_session() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let uuid =
            create_session(wg, SessionKind::Interactive, &["my-work".to_string()], None).unwrap();

        // Resolve by UUID
        assert_eq!(resolve_ref(wg, &uuid).unwrap(), uuid);
        // Resolve by alias
        assert_eq!(resolve_ref(wg, "my-work").unwrap(), uuid);
        // Resolve by prefix
        assert_eq!(resolve_ref(wg, &uuid[..8]).unwrap(), uuid);
    }

    #[test]
    fn alias_symlink_exists_after_create() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let uuid =
            create_session(wg, SessionKind::TaskAgent, &["task-foo".to_string()], None).unwrap();
        let link = wg.join("chat").join("task-foo");
        assert!(link.is_symlink(), "alias should be a symlink");
        let target = fs::read_link(&link).unwrap();
        assert_eq!(target.to_string_lossy(), uuid);
    }

    #[test]
    fn ambiguous_prefix_errors() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        // Two UUIDs will share the empty prefix ""; we want to test
        // that a SHORT prefix that's genuinely ambiguous errors out.
        // Since UUID randomness makes this flaky, we manually seed
        // the registry with two UUIDs that share a prefix.
        fs::create_dir_all(wg.join("chat")).unwrap();
        let mut reg = Registry::default();
        let u1 = "aaaa1111-e29b-41d4-a716-446655440000".to_string();
        let u2 = "aaaa2222-e29b-41d4-a716-446655440000".to_string();
        reg.sessions.insert(
            u1.clone(),
            SessionMeta {
                kind: SessionKind::Interactive,
                created: "2026-01-01".into(),
                aliases: vec![],
                label: None,
                forked_from: None,
            },
        );
        reg.sessions.insert(
            u2.clone(),
            SessionMeta {
                kind: SessionKind::Interactive,
                created: "2026-01-02".into(),
                aliases: vec![],
                label: None,
                forked_from: None,
            },
        );
        save(wg, &reg).unwrap();
        let err = resolve_ref(wg, "aaaa").unwrap_err();
        assert!(err.to_string().contains("ambiguous"));
        // But a more specific prefix resolves.
        assert_eq!(resolve_ref(wg, "aaaa1").unwrap(), u1);
    }

    #[test]
    fn ensure_session_is_idempotent() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let uuid1 = ensure_session(wg, "coordinator-0", SessionKind::Coordinator, None).unwrap();
        let uuid2 = ensure_session(wg, "coordinator-0", SessionKind::Coordinator, None).unwrap();
        assert_eq!(uuid1, uuid2);
    }

    #[test]
    fn add_and_remove_alias() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let uuid = create_session(wg, SessionKind::Interactive, &["primary".into()], None).unwrap();
        add_alias(wg, "primary", "secondary").unwrap();
        assert_eq!(resolve_ref(wg, "secondary").unwrap(), uuid);
        remove_alias(wg, "secondary").unwrap();
        assert!(resolve_ref(wg, "secondary").is_err());
        // Primary still works.
        assert_eq!(resolve_ref(wg, "primary").unwrap(), uuid);
    }

    #[test]
    fn fork_copies_journal_and_records_parent() {
        let dir = tempdir().unwrap();
        let wg = dir.path();

        // Parent: create a session and seed its journal + summary so
        // we can verify both get copied into the fork.
        let parent_uuid = create_session(
            wg,
            SessionKind::Interactive,
            &["parent".into()],
            Some("the original".into()),
        )
        .unwrap();
        let parent_dir = chat_dir_for_uuid(wg, &parent_uuid);
        std::fs::write(parent_dir.join("conversation.jsonl"), "turn-1\nturn-2\n").unwrap();
        std::fs::write(
            parent_dir.join("session-summary.md"),
            "## Summary\nsome text",
        )
        .unwrap();
        // Seed an inbox too so we can verify it does NOT get forked.
        std::fs::write(parent_dir.join("inbox.jsonl"), "{\"id\":1}\n").unwrap();

        // Fork from the parent's alias with an explicit new alias.
        let fork_uuid = fork_session(wg, "parent", Some("alt-take".into())).unwrap();
        assert_ne!(fork_uuid, parent_uuid);

        // Journal + summary got copied verbatim.
        let fork_dir = chat_dir_for_uuid(wg, &fork_uuid);
        assert_eq!(
            std::fs::read_to_string(fork_dir.join("conversation.jsonl")).unwrap(),
            "turn-1\nturn-2\n",
        );
        assert_eq!(
            std::fs::read_to_string(fork_dir.join("session-summary.md")).unwrap(),
            "## Summary\nsome text",
        );
        // Inbox did NOT get copied — the fork starts clean.
        assert!(
            !fork_dir.join("inbox.jsonl").exists(),
            "fork must start with an empty inbox",
        );

        // Registry entry records the parent UUID.
        let reg = load(wg).unwrap();
        let meta = reg.sessions.get(&fork_uuid).expect("fork registered");
        assert_eq!(meta.forked_from.as_deref(), Some(parent_uuid.as_str()));
        assert_eq!(meta.kind, SessionKind::Interactive);
        assert!(meta.aliases.iter().any(|a| a == "alt-take"));

        // The new alias is resolvable.
        assert_eq!(resolve_ref(wg, "alt-take").unwrap(), fork_uuid);

        // Writing to the fork doesn't mutate the parent (and vice
        // versa) — independence invariant.
        std::fs::write(fork_dir.join("conversation.jsonl"), "new-turn\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(parent_dir.join("conversation.jsonl")).unwrap(),
            "turn-1\nturn-2\n",
            "parent must be untouched by fork writes",
        );
    }

    #[test]
    fn fork_with_default_alias() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let parent_uuid =
            create_session(wg, SessionKind::Interactive, &["orig".into()], None).unwrap();
        std::fs::write(
            chat_dir_for_uuid(wg, &parent_uuid).join("conversation.jsonl"),
            "seed\n",
        )
        .unwrap();
        let fork_uuid = fork_session(wg, "orig", None).unwrap();
        let reg = load(wg).unwrap();
        let meta = reg.sessions.get(&fork_uuid).unwrap();
        // Generated alias has the fork-<short> shape.
        assert!(
            meta.aliases.iter().any(|a| a.starts_with("fork-")),
            "expected fork-<short> alias, got {:?}",
            meta.aliases
        );
    }

    #[test]
    fn fork_rejects_taken_alias() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let _parent =
            create_session(wg, SessionKind::Interactive, &["parent".into()], None).unwrap();
        let _other = create_session(
            wg,
            SessionKind::Interactive,
            &["already-taken".into()],
            None,
        )
        .unwrap();
        let err = fork_session(wg, "parent", Some("already-taken".into())).unwrap_err();
        assert!(
            format!("{}", err).contains("already-taken"),
            "error should mention the taken alias: {}",
            err
        );
    }

    #[test]
    fn daemon_style_coordinator_registration_creates_both_paths() {
        // Regression test for the "TUI chat never replies" bug.
        //
        // When the daemon starts a coordinator, it registers TWO
        // aliases — `coordinator-N` (subprocess arg) AND bare `N`
        // (legacy numeric path used by `chat::append_inbox_for` via
        // the IPC `UserChat` handler). Both must resolve to the
        // same underlying UUID dir, otherwise:
        //   * the TUI's `wg chat` → IPC → `append_inbox_for(dir, 0, …)`
        //     writes to `chat/0/inbox.jsonl`, and
        //   * the subprocess watches `chat/coordinator-0/inbox.jsonl`
        //     — a different path — and never sees the write.
        //
        // This test mirrors the daemon's startup sequence in
        // `coordinator_agent.rs` (`migrate_numeric_coord_dir` then
        // `ensure_session("coordinator-0")` then
        // `add_alias("coordinator-0", "0")`) and verifies that a
        // file written through one alias path is readable through
        // the other.
        let dir = tempdir().unwrap();
        let wg = dir.path();

        // Daemon startup sequence — single entry point, so this
        // test covers the exact code the daemon runs.
        let uuid = register_coordinator_session(wg, 0).unwrap();

        // Idempotency: calling it again on a running coordinator
        // (simulating subprocess restart) must NOT fail and must
        // return the same UUID.
        let uuid_again = register_coordinator_session(wg, 0).unwrap();
        assert_eq!(uuid, uuid_again, "register must be idempotent");

        // Both aliases resolve to the same UUID.
        assert_eq!(resolve_ref(wg, "coordinator-0").unwrap(), uuid);
        assert_eq!(resolve_ref(wg, "0").unwrap(), uuid);

        // Filesystem: both aliases are symlinks into the same UUID dir.
        let link_named = wg.join("chat").join("coordinator-0");
        let link_numeric = wg.join("chat").join("0");
        assert!(link_named.is_symlink(), "coordinator-0 missing symlink");
        assert!(
            link_numeric.is_symlink(),
            "bare `0` missing symlink (the exact regression this test locks in)"
        );

        let target_named = fs::canonicalize(&link_named).unwrap();
        let target_numeric = fs::canonicalize(&link_numeric).unwrap();
        assert_eq!(
            target_named, target_numeric,
            "coordinator-0 and 0 must resolve to the same directory"
        );

        // Round-trip through the legacy numeric path: what the IPC
        // `UserChat` handler writes via
        // `chat::append_inbox_for(dir, 0, …)` — which internally
        // joins `chat/0/inbox.jsonl` — must be readable via the
        // subprocess-facing `coordinator-0` alias.
        let write_path = wg.join("chat").join("0").join("inbox.jsonl");
        std::fs::create_dir_all(write_path.parent().unwrap()).unwrap();
        std::fs::write(&write_path, "sentinel-message").unwrap();

        let read_path = wg.join("chat").join("coordinator-0").join("inbox.jsonl");
        let read_content = std::fs::read_to_string(&read_path).unwrap();
        assert_eq!(
            read_content, "sentinel-message",
            "write via `chat/0/` must be readable via `chat/coordinator-0/` — otherwise the IPC path and the subprocess path are disconnected and TUI chat hangs forever"
        );
    }

    #[test]
    fn migrate_merges_into_existing_alias() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        // First, a fresh coordinator-0 session is created via
        // ensure_session (no pre-existing chat/0 dir).
        let existing_uuid =
            ensure_session(wg, "coordinator-0", SessionKind::Coordinator, None).unwrap();
        let existing_dir = chat_dir_for_uuid(wg, &existing_uuid);
        fs::write(existing_dir.join("existing.txt"), "registered").unwrap();

        // Now simulate a legacy subprocess creating chat/0/ as a
        // real directory with its own content.
        let legacy = wg.join("chat").join("0");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("orphan.txt"), "from legacy subprocess").unwrap();
        fs::write(legacy.join("existing.txt"), "would clobber").unwrap();

        // Migration should MERGE, not create a new session.
        let uuid = migrate_numeric_coord_dir(wg, 0).unwrap().unwrap();
        assert_eq!(uuid, existing_uuid, "should reuse existing UUID");

        // Registry still has exactly one coordinator-0 session.
        let sessions: Vec<_> = list(wg)
            .unwrap()
            .into_iter()
            .filter(|(_, m)| m.aliases.iter().any(|a| a == "coordinator-0"))
            .collect();
        assert_eq!(sessions.len(), 1, "no duplicate coordinator-0 entries");

        // The orphan file got merged into the existing session's dir.
        assert!(existing_dir.join("orphan.txt").exists());
        // The registered session's version of the clobbering file wins.
        assert_eq!(
            fs::read_to_string(existing_dir.join("existing.txt")).unwrap(),
            "registered",
        );
        // Legacy dir is gone, replaced by a symlink to the UUID.
        assert!(legacy.is_symlink());
    }

    #[test]
    fn migrate_numeric_coord_dir_moves_contents() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let old = wg.join("chat").join("0");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("marker.txt"), "legacy data").unwrap();

        let uuid = migrate_numeric_coord_dir(wg, 0).unwrap().unwrap();
        let new_marker = chat_dir_for_uuid(wg, &uuid).join("marker.txt");
        assert!(new_marker.exists(), "legacy file should be under UUID dir");
        assert_eq!(fs::read_to_string(&new_marker).unwrap(), "legacy data");

        // Old path is now a symlink that still works for readers.
        assert!(old.is_symlink());
        assert_eq!(
            fs::read_to_string(old.join("marker.txt")).unwrap(),
            "legacy data"
        );

        // And the `coordinator-0` alias also resolves.
        assert_eq!(resolve_ref(wg, "coordinator-0").unwrap(), uuid);
    }
}
