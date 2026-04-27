//! Spawn-site isolation: a single source of truth for spawn decisions.
//!
//! Background — the `spawn-single-source` decomposition (commits a7d0ab022,
//! 4f23b1224, 3e96c04c0, ff38bc4a2, 6576ac7c6, dc2f2f65c, 1a7e2b1de) routed
//! every executor / model / endpoint decision through `dispatch::plan_spawn`.
//! Before that work, four independent spawn sites built executor-and-argv
//! decisions on their own — and at least one regressed by silently ignoring
//! a `[dispatcher].executor=claude` pin and spawning `native --endpoint
//! openrouter` based on a model alias.
//!
//! This test pins the invariant *structurally* via a grep pass:
//!
//!   - Every spawn site outside `src/dispatch/` MUST call
//!     `workgraph::dispatch::plan_spawn`.
//!   - The set of files that build executor-aware spawn argv is a closed
//!     allowlist — a new file showing up with a `plan_spawn` call (or with
//!     `spawn_agent_inner`-style argv construction) is a signal that a
//!     reviewer needs to either add it to this list (and document why) or
//!     route the new code through an existing spawn site.
//!
//! If you are landing a legitimate new spawn site, update `EXPECTED_SITES`
//! below — the failure message will tell you exactly what to add.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Files outside `src/dispatch/` that are allowed to call `plan_spawn`.
/// Each entry is the canonical, single spawn entry point for one surface:
///
/// - `src/commands/spawn_task.rs`            — `wg spawn-task` internal helper
///                                              (called by the wrapper after
///                                              `wg spawn` resolves the agent
///                                              context). Calls `plan_spawn`.
/// - `src/commands/spawn/execution.rs`       — `spawn_agent_inner`, the one
///                                              shared argv builder used by
///                                              both CLI `wg spawn` and the
///                                              dispatcher. Calls `plan_spawn`.
/// - `src/commands/service/coordinator.rs`   — the dispatcher loop's spawn
///                                              site. Calls `plan_spawn`.
/// - `src/commands/service/ipc.rs`           — `handle_spawn` IPC entry.
///                                              Calls `plan_spawn`.
/// - `src/commands/service/coordinator_agent.rs` — chat-supervisor spawns +
///                                              per-iteration subprocess
///                                              spawns. Both call
///                                              `plan_spawn`.
const EXPECTED_SITES: &[&str] = &[
    "src/commands/spawn_task.rs",
    "src/commands/spawn/execution.rs",
    "src/commands/service/coordinator.rs",
    "src/commands/service/ipc.rs",
    "src/commands/service/coordinator_agent.rs",
];

fn repo_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn walk_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rs_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Strip line and block comments and string literals from Rust source so
/// pattern matches don't trip on doc references / examples / fixture data.
fn strip_comments_and_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        // Line comment
        if b == b'/' && next == Some(b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment (no nesting handling — our codebase doesn't use it)
        if b == b'/' && next == Some(b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        // String literal
        if b == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

fn relpath(src_root: &Path, file: &Path) -> String {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let rel = file.strip_prefix(&manifest).unwrap_or(file);
    rel.to_string_lossy().replace('\\', "/")
}

fn _src_root() -> PathBuf {
    repo_src_dir()
}

#[test]
fn test_all_spawn_sites_call_plan_spawn() {
    let src = repo_src_dir();
    let mut files = Vec::new();
    walk_rs_files(&src, &mut files);

    let dispatch_dir = src.join("dispatch");

    // Files (outside src/dispatch/) that contain a real `plan_spawn(` call,
    // ignoring strings/comments (so doc-comment references in unrelated files
    // don't get falsely flagged).
    let mut found: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        if f.starts_with(&dispatch_dir) {
            continue;
        }
        let raw = match fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let code = strip_comments_and_strings(&raw);
        if code.contains("plan_spawn(") {
            found.insert(relpath(&src, f));
        }
    }

    let expected: BTreeSet<String> = EXPECTED_SITES.iter().map(|s| s.to_string()).collect();

    let missing: Vec<&String> = expected.difference(&found).collect();
    let extra: Vec<&String> = found.difference(&expected).collect();

    assert!(
        missing.is_empty(),
        "spawn-site isolation regression: file(s) listed in EXPECTED_SITES \
         no longer call `plan_spawn(` — they likely regressed to building \
         spawn argv on their own, which is exactly the bug the \
         spawn-single-source decomposition fixed.\n  missing: {:?}",
        missing,
    );
    assert!(
        extra.is_empty(),
        "spawn-site isolation: a NEW file outside src/dispatch/ contains a \
         `plan_spawn(` call. If this is a legitimate new spawn site, add it \
         to EXPECTED_SITES in tests/spawn_site_isolation.rs (and document \
         what surface it implements). If it's not, the new caller should \
         delegate to one of the existing spawn entry points instead of \
         re-implementing argv-and-executor decisions.\n  extra: {:?}",
        extra,
    );
}

#[test]
fn test_no_independent_argv_executor_construction_outside_spawn_sites() {
    // Sentinel patterns that would indicate a file is independently building
    // executor-aware argv (e.g. `args.push("--executor")`) without going
    // through plan_spawn. We allow:
    //   - anything in src/dispatch/   (the single source)
    //   - the EXPECTED_SITES         (real spawn entry points; verified above)
    //   - src/commands/spawn/mod.rs   (thin pub-fn wrapper around
    //                                  spawn_agent_inner)
    //   - src/commands/service/mod.rs (`wg service start` argv builder for
    //                                  the daemon — argv flows to `wg
    //                                  service`, NOT to a spawned agent)
    //   - src/tui/                    (the TUI builds `wg service start` argv
    //                                  for new chats; same as above —
    //                                  daemon launch, not agent spawn)
    //   - src/commands/init.rs / agent_crud.rs etc. (CLI help text only,
    //                                  filtered by string-stripping)
    let src = repo_src_dir();
    let mut files = Vec::new();
    walk_rs_files(&src, &mut files);

    let dispatch_dir = src.join("dispatch");

    let allow: BTreeSet<&str> = [
        "src/commands/spawn_task.rs",
        "src/commands/spawn/execution.rs",
        "src/commands/spawn/mod.rs",
        "src/commands/service/coordinator.rs",
        "src/commands/service/coordinator_agent.rs",
        "src/commands/service/ipc.rs",
        "src/commands/service/mod.rs",
    ]
    .into_iter()
    .collect();

    let mut offenders: Vec<(String, &'static str)> = Vec::new();
    for f in &files {
        if f.starts_with(&dispatch_dir) {
            continue;
        }
        let rel = relpath(&src, f);
        if allow.contains(rel.as_str()) {
            continue;
        }
        // TUI builds daemon-launch argv (wg service start), not agent argv.
        if rel.starts_with("src/tui/") {
            continue;
        }
        let raw = match fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let code = strip_comments_and_strings(&raw);

        // ExecutorKind::Native / ::Claude / ::Codex / ::Amplifier / ::Shell
        // construction outside src/dispatch/ is a strong signal of an
        // independent spawn-decision site.
        for variant in ["Native", "Claude", "Codex", "Amplifier", "Shell"] {
            let needle = format!("ExecutorKind::{}", variant);
            if code.contains(&needle) {
                offenders.push((rel.clone(), "ExecutorKind::* construction"));
                break;
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "spawn-site isolation: file(s) outside src/dispatch/ and the \
         allowlist construct `ExecutorKind::*` directly. These should \
         delegate to plan_spawn or be added to the allowlist with a \
         comment explaining why.\n  offenders: {:#?}",
        offenders,
    );
}
