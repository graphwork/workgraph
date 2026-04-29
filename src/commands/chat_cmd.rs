//! `wg chat <subcommand>` — chat as a first-class graph entity.
//!
//! Decouples chat persistence from service runtime: chats are graph tasks
//! (`.chat-N`) that survive daemon restart. The supervisor in the running
//! daemon spawns a handler subprocess for each active chat task.
//!
//! Design constraints:
//! - `wg chat create`, `send`, `list`, `show` MUST work when the service
//!   daemon is down — they operate directly on `.workgraph/graph.jsonl`
//!   and `.workgraph/chat/<uuid>/`.
//! - `wg chat resume` and `wg chat stop` require the daemon (the handler
//!   process is owned by the supervisor); they error clearly when down.
//! - When the daemon IS running, `create` / `delete` / `archive` go
//!   through IPC so the supervisor immediately reflects the change.
//!
//! See task wg-chat-as for the full spec.
//!
//! Backward compat: `wg service create-chat` etc. still parse, but emit
//! a deprecation warning and route here.

use anyhow::{Context, Result};
use std::path::Path;

use workgraph::chat_id;
use workgraph::graph::{Status, WorkGraph};

use crate::commands::graph_path;
use crate::commands::is_process_alive;

/// Liveness category for `wg chat list` / `show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRuntimeStatus {
    /// Chat task exists in graph; service daemon is running and a
    /// supervisor entry exists for this chat (handler may be alive
    /// or about to be respawned by the supervisor).
    Supervised,
    /// Chat task exists in graph; service daemon is NOT running.
    /// Inbox messages will be queued until the daemon is started.
    Dormant,
    /// Chat task is Status::Done with the `archived` tag.
    Archived,
    /// Chat task is Status::Abandoned.
    Deleted,
    /// Chat task exists, daemon is up, but the supervisor has no
    /// active handler entry (e.g. after `wg chat stop`).
    Stopped,
}

impl ChatRuntimeStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Supervised => "supervised",
            Self::Dormant => "dormant",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
            Self::Stopped => "stopped",
        }
    }
}

/// Service-running detection — checks ServiceState file + PID liveness.
/// Returns true when the daemon socket should be reachable.
pub fn service_is_running(dir: &Path) -> bool {
    use crate::commands::service::ServiceState;
    match ServiceState::load(dir) {
        Ok(Some(state)) => is_process_alive(state.pid),
        _ => false,
    }
}

/// Resolve a chat reference (numeric ID, `.chat-N`, `.coordinator-N`,
/// or alias name like "testbot") to the numeric chat agent ID.
pub fn resolve_chat_id(graph: &WorkGraph, reference: &str) -> Option<u32> {
    // Numeric form ("0", "7")
    if let Ok(n) = reference.parse::<u32>() {
        if chat_id::find_chat_task(graph, n).is_some() {
            return Some(n);
        }
        return Some(n); // tolerate ID-without-task (still try downstream ops)
    }
    // Full task ID form
    if let Some(n) = chat_id::parse_chat_task_id(reference) {
        return Some(n);
    }
    // Name-based: scan chat tasks for a matching title suffix.
    // Title format from create_chat_in_graph is "Chat: <name>" or "Chat <id>".
    let want = reference.to_ascii_lowercase();
    for task in graph.tasks() {
        if !task.tags.iter().any(|t| chat_id::is_chat_loop_tag(t)) {
            continue;
        }
        let title_lower = task.title.to_ascii_lowercase();
        // Match "chat: <name>" exactly on the suffix
        let matches_suffix = title_lower
            .strip_prefix("chat: ")
            .map(|rest| rest == want)
            .unwrap_or(false);
        if matches_suffix
            && let Some(id) = chat_id::parse_chat_task_id(&task.id)
        {
            return Some(id);
        }
    }
    None
}

/// Categorize a chat task's runtime status given current daemon state.
fn classify_chat_task(
    task: &workgraph::graph::Task,
    daemon_running: bool,
    supervised_ids: &[u32],
) -> ChatRuntimeStatus {
    if matches!(task.status, Status::Abandoned) {
        return ChatRuntimeStatus::Deleted;
    }
    if task.tags.iter().any(|t| t == "archived") {
        return ChatRuntimeStatus::Archived;
    }
    let id = match chat_id::parse_chat_task_id(&task.id) {
        Some(n) => n,
        None => return ChatRuntimeStatus::Dormant,
    };
    if !daemon_running {
        return ChatRuntimeStatus::Dormant;
    }
    if supervised_ids.contains(&id) {
        ChatRuntimeStatus::Supervised
    } else {
        ChatRuntimeStatus::Stopped
    }
}

/// Query the running daemon for its supervised chat IDs (if reachable).
/// Returns empty Vec on failure or when daemon is down.
fn supervised_chat_ids(dir: &Path) -> Vec<u32> {
    if !service_is_running(dir) {
        return Vec::new();
    }
    use crate::commands::service::ipc::IpcRequest;
    use crate::commands::service::send_request;
    let resp = match send_request(dir, &IpcRequest::ListChats) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let data = match &resp.data {
        Some(d) => d,
        None => return Vec::new(),
    };
    let arr = match data.get("coordinators").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|v| v.get("coordinator_id").and_then(|x| x.as_u64()))
        .map(|n| n as u32)
        .collect()
}

// ============================================================================
// Subcommand: create
// ============================================================================

/// `wg chat create` — create a new chat agent entity in the graph.
///
/// When the service is running, talks to it via IPC (so the supervisor
/// can immediately spawn the handler). When it's down, writes the graph
/// task directly — the supervisor picks it up on next service start.
/// Both paths produce identical on-disk state.
pub fn run_create(
    dir: &Path,
    name: Option<&str>,
    model: Option<&str>,
    executor: Option<&str>,
    json: bool,
) -> Result<()> {
    if service_is_running(dir) {
        run_create_via_ipc(dir, name, model, executor, json)
    } else {
        run_create_direct(dir, name, model, executor, json)
    }
}

#[cfg(unix)]
fn run_create_via_ipc(
    dir: &Path,
    name: Option<&str>,
    model: Option<&str>,
    executor: Option<&str>,
    json: bool,
) -> Result<()> {
    crate::commands::service::run_create_coordinator(
        dir,
        name,
        model,
        executor,
        json,
    )
}

#[cfg(not(unix))]
fn run_create_via_ipc(
    _dir: &Path,
    _name: Option<&str>,
    _model: Option<&str>,
    _executor: Option<&str>,
    _json: bool,
) -> Result<()> {
    anyhow::bail!("Service IPC is only supported on Unix systems")
}

fn run_create_direct(
    dir: &Path,
    name: Option<&str>,
    model: Option<&str>,
    executor: Option<&str>,
    json: bool,
) -> Result<()> {
    let next_id = crate::commands::service::ipc::create_chat_in_graph(
        dir, name, model, executor,
    )?;
    let task_id = chat_id::format_chat_task_id(next_id);
    if json {
        let v = serde_json::json!({
            "chat_id": next_id,
            "coordinator_id": next_id,
            "task_id": task_id,
            "name": name,
            "service": "down",
            "status": "dormant",
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "Created chat {} (task {}). Service is not running — chat is dormant.",
            next_id, task_id
        );
        println!(
            "Start the service ('wg service start') and the supervisor will spawn the handler."
        );
    }
    Ok(())
}

// ============================================================================
// Subcommand: list / ls
// ============================================================================

/// `wg chat list` — show all chat entities with truthful status.
pub fn run_list(dir: &Path, json: bool) -> Result<()> {
    let graph = workgraph::parser::load_graph(&graph_path(dir))
        .with_context(|| "Failed to load graph")?;

    let daemon_running = service_is_running(dir);
    let supervised = supervised_chat_ids(dir);

    let mut rows = Vec::new();
    for task in graph.tasks() {
        if !task.tags.iter().any(|t| chat_id::is_chat_loop_tag(t)) {
            continue;
        }
        let cid = match chat_id::parse_chat_task_id(&task.id) {
            Some(n) => n,
            None => continue,
        };
        let status = classify_chat_task(task, daemon_running, &supervised);
        rows.push((cid, task, status));
    }
    rows.sort_by_key(|(cid, _, _)| *cid);

    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|(cid, t, s)| {
                serde_json::json!({
                    "chat_id": cid,
                    "task_id": t.id,
                    "title": t.title,
                    "status": s.label(),
                    "task_status": format!("{:?}", t.status),
                    "service_running": daemon_running,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"chats": arr}))?
        );
        return Ok(());
    }

    if rows.is_empty() {
        println!("No chats. Create one with 'wg chat create --name <NAME>'.");
        return Ok(());
    }

    println!(
        "{:<6}  {:<14}  {:<24}  {}",
        "ID", "STATUS", "TASK", "TITLE"
    );
    for (cid, t, s) in rows {
        let suffix = if matches!(s, ChatRuntimeStatus::Dormant) && !daemon_running {
            " — service stopped"
        } else {
            ""
        };
        println!(
            "{:<6}  {:<14}  {:<24}  {}{}",
            cid,
            s.label(),
            t.id,
            t.title,
            suffix
        );
    }
    Ok(())
}

// ============================================================================
// Subcommand: show
// ============================================================================

/// `wg chat show` — detailed view of a single chat entity.
pub fn run_show(dir: &Path, reference: &str, json: bool) -> Result<()> {
    let graph = workgraph::parser::load_graph(&graph_path(dir))
        .with_context(|| "Failed to load graph")?;

    let cid = resolve_chat_id(&graph, reference)
        .with_context(|| format!("No chat matching '{}'", reference))?;
    let task = chat_id::find_chat_task(&graph, cid)
        .with_context(|| format!("Chat task for id {} not found in graph", cid))?;

    let daemon_running = service_is_running(dir);
    let supervised = supervised_chat_ids(dir);
    let status = classify_chat_task(task, daemon_running, &supervised);

    // Per-chat overrides from CoordinatorState.
    let coord_state = crate::commands::service::CoordinatorState::load_for(dir, cid);
    let exec_override = coord_state
        .as_ref()
        .and_then(|s| s.executor_override.clone());
    let model_override = coord_state.as_ref().and_then(|s| s.model_override.clone());

    if json {
        let v = serde_json::json!({
            "chat_id": cid,
            "task_id": task.id,
            "title": task.title,
            "task_status": format!("{:?}", task.status),
            "runtime_status": status.label(),
            "service_running": daemon_running,
            "executor": exec_override,
            "model": model_override,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    println!("Chat {}", cid);
    println!("  task     : {}", task.id);
    println!("  title    : {}", task.title);
    println!("  status   : {}", status.label());
    println!("  task     : {:?}", task.status);
    if let Some(e) = exec_override {
        println!("  executor : {}", e);
    }
    if let Some(m) = model_override {
        println!("  model    : {}", m);
    }
    println!("  service  : {}", if daemon_running { "running" } else { "stopped" });
    Ok(())
}

// ============================================================================
// Subcommand: send
// ============================================================================

/// `wg chat send <ref> <msg>` — append a message to the chat's inbox.
///
/// Works with the daemon up OR down: `inbox.jsonl` is the source of
/// truth. When the daemon is up, the supervisor's handler will pick
/// the message up via the standard chat loop. When down, the message
/// queues until the daemon (re)starts.
pub fn run_send(dir: &Path, reference: &str, message: &str, json: bool) -> Result<()> {
    let graph = workgraph::parser::load_graph(&graph_path(dir))
        .with_context(|| "Failed to load graph")?;
    let cid = resolve_chat_id(&graph, reference)
        .with_context(|| format!("No chat matching '{}'", reference))?;

    // Make sure the chat dir exists (chat::append_inbox_for creates parent
    // dirs, but we want a stable filesystem location for non-running chats).
    let request_id = format!(
        "wg-chat-send-{}",
        chrono::Utc::now().timestamp_millis()
    );
    let inbox_id = workgraph::chat::append_inbox_for(dir, cid, message, &request_id)
        .with_context(|| format!("Failed to append to chat {} inbox", cid))?;

    let running = service_is_running(dir);
    if json {
        let v = serde_json::json!({
            "chat_id": cid,
            "inbox_id": inbox_id,
            "request_id": request_id,
            "service_running": running,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "Appended message #{} to chat {} inbox.{}",
            inbox_id,
            cid,
            if running {
                ""
            } else {
                " Service is not running — message will be processed when daemon starts."
            }
        );
    }
    Ok(())
}

// ============================================================================
// Subcommand: stop / resume / archive / delete
// ============================================================================

/// `wg chat stop` — SIGTERM the live handler (chat entity stays in graph).
/// Requires the daemon (the supervisor owns the handler).
pub fn run_stop(dir: &Path, reference: &str, json: bool) -> Result<()> {
    let graph = workgraph::parser::load_graph(&graph_path(dir))
        .with_context(|| "Failed to load graph")?;
    let cid = resolve_chat_id(&graph, reference)
        .with_context(|| format!("No chat matching '{}'", reference))?;
    if !service_is_running(dir) {
        anyhow::bail!(
            "Cannot stop chat {}: service daemon is not running. \
             The handler is supervised by the daemon — without it there is no \
             handler to stop. Start the daemon ('wg service start') first.",
            cid
        );
    }
    crate::commands::service::run_stop_coordinator(dir, cid, json)
}

/// `wg chat resume` — ask the supervisor to (re)spawn the handler.
/// Requires the daemon. Errors clearly when down.
pub fn run_resume(dir: &Path, reference: &str, json: bool) -> Result<()> {
    let graph = workgraph::parser::load_graph(&graph_path(dir))
        .with_context(|| "Failed to load graph")?;
    let cid = resolve_chat_id(&graph, reference)
        .with_context(|| format!("No chat matching '{}'", reference))?;
    if !service_is_running(dir) {
        anyhow::bail!(
            "Cannot resume chat {}: service daemon is not running. \
             Resume requires the supervisor (which lives in the daemon) to spawn \
             the handler. Start the daemon ('wg service start') and the supervisor \
             will pick up this chat automatically.",
            cid
        );
    }
    // The supervisor lazy-spawns the handler on the next message — sending an
    // empty marker via SetChatExecutor with no overrides is a no-op that
    // reaches the daemon; for now, rely on the lazy-spawn path triggered by
    // any subsequent UserChat IPC. To force a respawn now, we use the
    // existing executor swap path with current settings.
    use crate::commands::service::ipc::IpcRequest;
    use crate::commands::service::send_request;
    let resp = send_request(
        dir,
        &IpcRequest::SetChatExecutor {
            chat_id: cid,
            executor: None,
            model: None,
        },
    )?;
    if !resp.ok {
        let msg = resp.error.unwrap_or_else(|| "Unknown error".to_string());
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({"error": msg}))?
            );
        } else {
            eprintln!("Error: {}", msg);
        }
        anyhow::bail!("{}", msg);
    }
    if json {
        if let Some(d) = resp.data {
            println!("{}", serde_json::to_string_pretty(&d)?);
        }
    } else {
        println!(
            "Asked supervisor to resume chat {}. Send a message to wake the handler.",
            cid
        );
    }
    Ok(())
}

/// `wg chat archive` — mark Done + tag 'archived'. Reversible-ish (archived
/// chats can still be inspected; their dirs are moved to .archive/).
pub fn run_archive(dir: &Path, reference: &str, json: bool) -> Result<()> {
    let graph = workgraph::parser::load_graph(&graph_path(dir))
        .with_context(|| "Failed to load graph")?;
    let cid = resolve_chat_id(&graph, reference)
        .with_context(|| format!("No chat matching '{}'", reference))?;
    let result = if service_is_running(dir) {
        crate::commands::service::run_archive_coordinator(dir, cid, json)
    } else {
        archive_chat_direct(dir, cid, json)
    };
    // Tear down the tmux chat session so we don't accumulate orphan
    // wg-chat-* sessions. Best-effort — the archive itself succeeded
    // (or failed) before this runs.
    chat_id::kill_chat_tmux_session_for_id(dir, cid);
    result
}

fn archive_chat_direct(dir: &Path, cid: u32, json: bool) -> Result<()> {
    let graph_p = graph_path(dir);
    let task_id = chat_id::format_chat_task_id(cid);
    let legacy_id = format!(".coordinator-{}", cid);
    workgraph::parser::modify_graph(&graph_p, |g| {
        let resolved = if g.get_task(&task_id).is_some() {
            task_id.clone()
        } else if g.get_task(&legacy_id).is_some() {
            legacy_id.clone()
        } else {
            return false;
        };
        if let Some(t) = g.get_task_mut(&resolved) {
            t.status = Status::Done;
            if !t.tags.iter().any(|x| x == "archived") {
                t.tags.push("archived".to_string());
            }
            t.log.push(workgraph::graph::LogEntry {
                timestamp: chrono::Utc::now().to_rfc3339(),
                actor: Some("wg-chat-archive".to_string()),
                user: Some(workgraph::current_user()),
                message: format!("Chat {} archived (service down)", cid),
            });
        }
        true
    })?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "chat_id": cid,
                "archived": true,
                "service": "down",
            }))?
        );
    } else {
        println!("Archived chat {} (service was not running).", cid);
    }
    Ok(())
}

/// `wg chat delete` — abandon the graph task and remove the chat dir.
pub fn run_delete(dir: &Path, reference: &str, yes: bool, json: bool) -> Result<()> {
    let graph = workgraph::parser::load_graph(&graph_path(dir))
        .with_context(|| "Failed to load graph")?;
    let cid = resolve_chat_id(&graph, reference)
        .with_context(|| format!("No chat matching '{}'", reference))?;

    if !yes && !json {
        eprint!(
            "Delete chat {} (graph task abandoned, chat dir preserved)? [y/N] ",
            cid
        );
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        if !matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    let result = if service_is_running(dir) {
        crate::commands::service::run_delete_coordinator(dir, cid, json)
    } else {
        delete_chat_direct(dir, cid, json)
    };
    // Tear down the tmux chat session if any — see run_archive.
    chat_id::kill_chat_tmux_session_for_id(dir, cid);
    result
}

fn delete_chat_direct(dir: &Path, cid: u32, json: bool) -> Result<()> {
    let graph_p = graph_path(dir);
    let task_id = chat_id::format_chat_task_id(cid);
    let legacy_id = format!(".coordinator-{}", cid);
    workgraph::parser::modify_graph(&graph_p, |g| {
        let resolved = if g.get_task(&task_id).is_some() {
            task_id.clone()
        } else if g.get_task(&legacy_id).is_some() {
            legacy_id.clone()
        } else {
            return false;
        };
        if let Some(t) = g.get_task_mut(&resolved) {
            t.status = Status::Abandoned;
            t.log.push(workgraph::graph::LogEntry {
                timestamp: chrono::Utc::now().to_rfc3339(),
                actor: Some("wg-chat-delete".to_string()),
                user: Some(workgraph::current_user()),
                message: format!("Chat {} deleted (service down)", cid),
            });
        }
        true
    })?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "chat_id": cid,
                "deleted": true,
                "service": "down",
            }))?
        );
    } else {
        println!("Deleted chat {} (service was not running).", cid);
    }
    Ok(())
}

// ============================================================================
// Subcommand: attach
// ============================================================================

/// `wg chat attach` — open an interactive view of the chat session.
///
/// Preferred path: when a tmux session exists for this chat (TUI was
/// run with chat-persistence wrappers), `exec tmux attach -t <session>`
/// hands the user the live vendor CLI directly — including history and
/// in-flight tool calls. This is the strongest reattach UX and works
/// from any terminal (no TUI required).
///
/// Fallbacks (in order):
///   1. TUI mode via `chat::run_interactive` when on a TTY + service is
///      up. Talks to daemon over IPC.
///   2. Read-only outbox stream (CLI mode). Use `wg chat send` to
///      enqueue messages.
pub fn run_attach(dir: &Path, reference: &str, force_cli: bool) -> Result<()> {
    let graph = workgraph::parser::load_graph(&graph_path(dir))
        .with_context(|| "Failed to load graph")?;
    let cid = resolve_chat_id(&graph, reference)
        .with_context(|| format!("No chat matching '{}'", reference))?;

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout());

    // Try the tmux fast-path first when on a TTY: if the wg-chat-* tmux
    // session for this chat is alive, attach to it. This is what the
    // user actually wants for "drop me back into my chat" — no
    // outbox-tail, no IPC roundtrip. Skip when --cli forced or when not
    // on a TTY (tmux attach into a pipe would hang).
    if !force_cli
        && is_tty
        && let Some(session) = chat_tmux_session_for_dir(dir, cid)
        && tmux_session_alive(&session)
    {
        eprintln!("Attaching to tmux session: {}", session);
        let status = std::process::Command::new("tmux")
            .args(["attach", "-d", "-t", &session])
            .status()
            .with_context(|| "Failed to invoke tmux attach")?;
        if status.success() {
            return Ok(());
        }
        eprintln!(
            "tmux attach exited with status {:?}; falling back to other modes.",
            status.code()
        );
    }

    if !force_cli && is_tty {
        // Interactive REPL via existing chat::run_interactive (talks to
        // daemon over IPC for live responses).
        if !service_is_running(dir) {
            eprintln!(
                "Note: service daemon is not running. Falling back to read-only \
                 stream view; use 'wg chat send' to enqueue messages."
            );
            return read_only_attach(dir, cid);
        }
        crate::commands::chat::run_interactive(dir, None, cid)
    } else {
        read_only_attach(dir, cid)
    }
}

fn chat_tmux_session_for_dir(dir: &Path, cid: u32) -> Option<String> {
    let project_root = dir.parent().unwrap_or(dir).to_path_buf();
    let project_tag = project_root.file_name().and_then(|n| n.to_str())?;
    let chat_ref = format!("chat-{}", cid);
    Some(workgraph::chat_id::chat_tmux_session_name(
        project_tag,
        &chat_ref,
    ))
}

fn tmux_session_alive(name: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_only_attach(dir: &Path, cid: u32) -> Result<()> {
    // Reuse the existing session-attach implementation, addressing the
    // chat by its `.chat-N` task id (chat_dir_for_ref handles both
    // legacy and new naming + alias resolution).
    let session_ref = chat_id::format_chat_task_id(cid);
    crate::commands::chat_session::run(
        dir,
        crate::cli::SessionCommands::Attach {
            session: session_ref,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mk_workgraph_dir() -> TempDir {
        let td = TempDir::new().unwrap();
        let dir = td.path();
        std::fs::create_dir_all(dir.join("service")).unwrap();
        std::fs::write(dir.join("graph.jsonl"), "").unwrap();
        td
    }

    #[test]
    fn create_chat_works_when_service_down() {
        let td = mk_workgraph_dir();
        let dir = td.path();
        // service_is_running is false (no service/state.json) — exercise
        // the direct path:
        assert!(!service_is_running(dir));
        run_create_direct(dir, Some("alpha"), None, None, true).unwrap();

        // Graph contains a .chat-N task
        let g = workgraph::parser::load_graph(&graph_path(dir)).unwrap();
        let chat_tasks: Vec<_> = g
            .tasks()
            .filter(|t| t.tags.iter().any(|x| chat_id::is_chat_loop_tag(x)))
            .collect();
        assert_eq!(chat_tasks.len(), 1, "Should have created one chat task");
        assert!(chat_tasks[0].id.starts_with(".chat-"));
    }

    #[test]
    fn send_to_dormant_chat_appends_inbox() {
        let td = mk_workgraph_dir();
        let dir = td.path();
        run_create_direct(dir, Some("bot"), None, None, true).unwrap();

        // Find the chat id we just created
        let g = workgraph::parser::load_graph(&graph_path(dir)).unwrap();
        let chat = g
            .tasks()
            .find(|t| t.tags.iter().any(|x| chat_id::is_chat_loop_tag(x)))
            .expect("chat task exists");
        let cid = chat_id::parse_chat_task_id(&chat.id).unwrap();

        // Send
        run_send(dir, &cid.to_string(), "hi from test", true).unwrap();

        // Inbox file exists and has one message
        let inbox = workgraph::chat::chat_dir_for_ref(dir, &cid.to_string())
            .join("inbox.jsonl");
        let contents = std::fs::read_to_string(&inbox).expect("inbox file written");
        assert!(
            contents.contains("hi from test"),
            "inbox.jsonl should contain the message: {}",
            contents
        );
    }

    #[test]
    fn resume_errors_clearly_when_service_down() {
        let td = mk_workgraph_dir();
        let dir = td.path();
        run_create_direct(dir, Some("c"), None, None, true).unwrap();
        let g = workgraph::parser::load_graph(&graph_path(dir)).unwrap();
        let chat = g
            .tasks()
            .find(|t| t.tags.iter().any(|x| chat_id::is_chat_loop_tag(x)))
            .unwrap();
        let cid = chat_id::parse_chat_task_id(&chat.id).unwrap();

        let err = run_resume(dir, &cid.to_string(), true).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("daemon is not running") || msg.contains("not running"),
            "Resume error should explain service is down: {}",
            msg
        );
    }

    #[test]
    fn list_truthful_status_when_service_down() {
        let td = mk_workgraph_dir();
        let dir = td.path();
        run_create_direct(dir, Some("alpha"), None, None, true).unwrap();
        run_create_direct(dir, Some("beta"), None, None, true).unwrap();

        // Build the in-memory representation list_truthfully would emit.
        let g = workgraph::parser::load_graph(&graph_path(dir)).unwrap();
        for task in g.tasks().filter(|t| t.tags.iter().any(|x| chat_id::is_chat_loop_tag(x))) {
            let status = classify_chat_task(task, false, &[]);
            assert_eq!(
                status,
                ChatRuntimeStatus::Dormant,
                "Daemon down — every chat should be Dormant"
            );
        }
    }

    #[test]
    fn classify_archived_and_deleted() {
        let mut t = workgraph::graph::Task::default();
        t.id = ".chat-1".to_string();
        t.tags = vec![chat_id::CHAT_LOOP_TAG.to_string(), "archived".to_string()];
        t.status = Status::Done;
        assert_eq!(
            classify_chat_task(&t, true, &[1]),
            ChatRuntimeStatus::Archived
        );

        let mut t2 = workgraph::graph::Task::default();
        t2.id = ".chat-2".to_string();
        t2.tags = vec![chat_id::CHAT_LOOP_TAG.to_string()];
        t2.status = Status::Abandoned;
        assert_eq!(
            classify_chat_task(&t2, true, &[2]),
            ChatRuntimeStatus::Deleted
        );

        let mut t3 = workgraph::graph::Task::default();
        t3.id = ".chat-3".to_string();
        t3.tags = vec![chat_id::CHAT_LOOP_TAG.to_string()];
        t3.status = Status::InProgress;
        // Daemon up, supervised
        assert_eq!(
            classify_chat_task(&t3, true, &[3]),
            ChatRuntimeStatus::Supervised
        );
        // Daemon up but not supervised → stopped
        assert_eq!(
            classify_chat_task(&t3, true, &[]),
            ChatRuntimeStatus::Stopped
        );
        // Daemon down → dormant
        assert_eq!(
            classify_chat_task(&t3, false, &[]),
            ChatRuntimeStatus::Dormant
        );
    }
}
