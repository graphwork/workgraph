//! `wg claude-handler` — standalone bridge between Claude CLI's
//! stream-json stdio and `chat/<ref>/*.jsonl`.
//!
//! Peer of `wg nex --chat <ref>`: where nex IS a native handler that
//! speaks chat/*.jsonl directly, this handler spawns the `claude` CLI
//! and translates between the two protocols. From the daemon's and
//! TUI's perspective, spawning a claude coordinator is now identical
//! to spawning a native one — both go through `wg spawn-task` which
//! execs into the right handler binary.
//!
//! Minimum viable responsibilities for Phase 7:
//!   - Acquire the session lock (same contract as nex)
//!   - Spawn `claude --print --input-format stream-json --output-format
//!     stream-json` with system prompt + allowed tools
//!   - Poll the inbox.jsonl cursor for new user messages; format each
//!     as a stream-json user message on Claude stdin
//!   - Parse Claude stdout events; stream partial text to `.streaming`
//!     and append finalized replies to outbox.jsonl
//!   - Exit on SIGTERM with the lock released; exit non-zero on Claude
//!     CLI death so the parent supervisor can restart us
//!
//! Out of scope for MVP (tracked for follow-ups):
//!   - Per-turn context injection (will wire through `build_coordinator_context`)
//!   - Chat compaction gating
//!   - Crash-recovery context on restart
//!   - Inline evaluation

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use workgraph::chat;
use workgraph::session_lock::{HandlerKind, SessionLock};

/// Poll interval for new inbox messages when the inbox appears empty.
/// Short enough that chat feels snappy; long enough not to hammer the
/// filesystem.
const INBOX_POLL: Duration = Duration::from_millis(200);

/// Timeout for collecting a single assistant response before we give up
/// and write a timeout message. Should cover tool-heavy turns.
const TURN_TIMEOUT: Duration = Duration::from_secs(300);

/// Entry point wired into `main.rs`.
pub fn run(
    workgraph_dir: &Path,
    chat_ref: &str,
    resume: bool,
    role: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    // Resume is accepted for argv symmetry with `wg nex --resume` but
    // for Claude CLI we don't have a journal-replay concept yet — the
    // on-disk conversation.jsonl is written server-side. Keep the flag
    // so the same spawn-task caller works for both executors.
    let _ = resume;

    let chat_dir = workgraph_dir.join("chat").join(chat_ref);
    std::fs::create_dir_all(&chat_dir)
        .with_context(|| format!("create chat dir {:?}", chat_dir))?;

    let mut _lock = SessionLock::acquire(&chat_dir, HandlerKind::Adapter).with_context(|| {
        format!(
            "acquire session lock for chat session {:?} — another handler is running",
            chat_ref
        )
    })?;

    let handler_log = chat_dir.join("handler.log");
    let logger = HandlerLogger::open(&handler_log)?;
    logger.info(&format!(
        "claude-handler starting: chat_ref={}, role={:?}, model={:?}",
        chat_ref, role, model
    ));

    // SIGTERM → kernel kills us; lock lingers as stale, next handler
    // picks it up. SIGINT → forwarded to the Claude CLI child so the
    // user's "stop generating" gesture (e.g. Ctrl+C in the TUI
    // pathway through `CoordinatorAgent::interrupt()`) preserves the
    // session instead of killing the whole handler. See
    // `install_sigint_forwarder` below.
    let shutdown = Arc::new(Mutex::new(false));

    // Resolve system prompt. For the coordinator-N convention we build
    // the full coordinator prompt; other sessions get a minimal role
    // line (caller can override via --role, which gets appended).
    let system_prompt = build_handler_system_prompt(workgraph_dir, chat_ref, role);

    // Spawn Claude CLI.
    let (mut child, mut stdin, stdout) =
        spawn_claude_process(workgraph_dir, &system_prompt, model, &logger)
            .context("spawn claude CLI")?;

    // Record the child PID in a process-global atomic so the SIGINT
    // handler (installed below, async-signal-safe) can forward to
    // it. Set BEFORE installing the handler to close the race where
    // SIGINT arrives between spawn and handler install.
    CLAUDE_CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    install_sigint_forwarder();

    // Reader thread: Claude stdout → ResponseEvent channel.
    let (resp_tx, resp_rx) = mpsc::channel::<ResponseEvent>();
    let reader_logger = logger.clone();
    let _reader = thread::Builder::new()
        .name("claude-handler-stdout".into())
        .spawn(move || stdout_reader(stdout, resp_tx, reader_logger))
        .context("spawn stdout reader thread")?;

    // Main loop: poll inbox, format → stdin, collect → outbox.
    // Cursor starts at the highest inbox id that already has a
    // matching outbox response — i.e., everything up to the last
    // ANSWERED turn is skipped, and any pending (un-answered) turns
    // are picked up by this handler. This handles both first-run
    // (no outbox → cursor=0 → process all inbox) and restart
    // (outbox has replies up to id N → cursor=N → process id N+1..).
    let mut inbox_cursor: u64 = last_answered_inbox_id(workgraph_dir, chat_ref);
    let coordinator_id = parse_coordinator_id(chat_ref);
    let mut last_interaction = chrono::Utc::now().to_rfc3339();
    logger.info(&format!(
        "claude-handler ready: inbox_cursor={}, coordinator_id={:?}, handler_log={}",
        inbox_cursor,
        coordinator_id,
        handler_log.display()
    ));

    loop {
        if *shutdown.lock().unwrap_or_else(|e| e.into_inner()) {
            logger.info("claude-handler: shutdown signal received");
            break;
        }

        // Child-alive check: if Claude CLI died, exit non-zero so the
        // spawn-task supervisor (daemon) restarts us.
        if let Some(status) = child.try_wait().unwrap_or(None) {
            logger.warn(&format!(
                "claude-handler: Claude CLI exited with status {:?} — handler exiting for restart",
                status
            ));
            // Draining is handled by Drop on SessionLock.
            return Err(anyhow::anyhow!(
                "Claude CLI exited with status {:?}",
                status
            ));
        }

        // Pull any new inbox messages since our cursor.
        let new_msgs = match chat::read_inbox_since_ref(workgraph_dir, chat_ref, inbox_cursor) {
            Ok(msgs) => msgs,
            Err(e) => {
                logger.warn(&format!("claude-handler: inbox read error: {}", e));
                thread::sleep(INBOX_POLL);
                continue;
            }
        };

        if new_msgs.is_empty() {
            thread::sleep(INBOX_POLL);
            continue;
        }

        for msg in new_msgs {
            inbox_cursor = msg.id.max(inbox_cursor);
            let request_id = if msg.request_id.is_empty() {
                format!("req-{}", msg.id)
            } else {
                msg.request_id.clone()
            };

            logger.info(&format!(
                "claude-handler: processing inbox id={} request_id={} ({} chars)",
                msg.id,
                request_id,
                msg.content.len()
            ));

            // For coordinator sessions, prepend the same graph-state
            // context the daemon's legacy inline path injected — so
            // the coordinator sees recent task events, active agents,
            // and failed-task attention markers every turn.
            let full_content = if let Some(cid) = coordinator_id {
                match crate::commands::service::coordinator_agent::build_coordinator_context(
                    workgraph_dir,
                    &last_interaction,
                    None,
                    cid,
                ) {
                    Ok(ctx) if !ctx.is_empty() => {
                        format!("{}\n\n---\n\nUser message:\n{}", ctx, msg.content)
                    }
                    _ => format!("User message:\n{}", msg.content),
                }
            } else {
                msg.content.clone()
            };

            // Format + write user turn.
            let user_msg = format_stream_json_user_message(&full_content);
            if let Err(e) = stdin
                .write_all(user_msg.as_bytes())
                .and_then(|_| stdin.flush())
            {
                logger.error(&format!("claude-handler: stdin write failed: {}", e));
                let _ = chat::append_outbox_ref(
                    workgraph_dir,
                    chat_ref,
                    "The coordinator encountered an error sending to Claude. Restarting.",
                    &request_id,
                );
                return Err(anyhow::anyhow!("stdin write failed: {}", e));
            }

            // Collect response; stream partial text to `.streaming`.
            let streaming_path = chat::streaming_path_ref(workgraph_dir, chat_ref);
            let collected = collect_response(
                &resp_rx,
                &logger,
                TURN_TIMEOUT,
                Some((&streaming_path, workgraph_dir, chat_ref)),
            );

            match collected {
                Some(resp) if !resp.summary.is_empty() => {
                    logger.info(&format!(
                        "claude-handler: response ready for {} ({} chars)",
                        request_id,
                        resp.summary.len()
                    ));
                    if let Err(e) =
                        chat::append_outbox_ref(workgraph_dir, chat_ref, &resp.summary, &request_id)
                    {
                        logger.error(&format!("claude-handler: outbox write failed: {}", e));
                    }
                }
                Some(_) => {
                    logger.warn("claude-handler: empty response");
                    let _ = chat::append_outbox_ref(
                        workgraph_dir,
                        chat_ref,
                        "The coordinator processed your message but produced no response text.",
                        &request_id,
                    );
                }
                None => {
                    logger.warn("claude-handler: response timeout");
                    let _ = chat::append_outbox_ref(
                        workgraph_dir,
                        chat_ref,
                        "The coordinator timed out processing your message.",
                        &request_id,
                    );
                }
            }

            chat::clear_streaming_ref(workgraph_dir, chat_ref);
            last_interaction = chrono::Utc::now().to_rfc3339();
        }
    }

    // Graceful shutdown: ask Claude to exit, then reap. SessionLock
    // drop will remove the lock file.
    let _ = child.kill();
    let _ = child.wait();
    logger.info("claude-handler: exited cleanly");
    Ok(())
}

/// Compute the starting inbox cursor: the highest inbox id for which
/// an outbox reply already exists (matched by `request_id`). Messages
/// with larger ids OR without a matching outbox reply are unprocessed
/// work, so we pick them up.
///
/// First run with a fresh inbox: no outbox → cursor = 0 → we process
/// everything.
///
/// Restart scenario: outbox contains replies for the earlier inbox
/// messages → cursor = id of the last answered one → we skip those
/// and resume from the first un-answered message.
fn last_answered_inbox_id(workgraph_dir: &Path, chat_ref: &str) -> u64 {
    let inbox = match chat::read_inbox_ref(workgraph_dir, chat_ref) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let outbox = match chat::read_outbox_since_ref(workgraph_dir, chat_ref, 0) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let answered_request_ids: std::collections::HashSet<String> =
        outbox.iter().map(|m| m.request_id.clone()).collect();
    inbox
        .iter()
        .filter(|m| answered_request_ids.contains(&m.request_id))
        .map(|m| m.id)
        .max()
        .unwrap_or(0)
}

/// If the chat ref is a coordinator alias (`coordinator-N`), return
/// the numeric id so we can call coordinator-specific helpers like
/// `build_coordinator_context`. Otherwise `None` — the handler runs
/// as a plain chat session with no graph-state injection.
fn parse_coordinator_id(chat_ref: &str) -> Option<u32> {
    chat_ref
        .strip_prefix("coordinator-")
        .and_then(|s| s.parse::<u32>().ok())
}

/// Build the system prompt. For `coordinator-N` sessions we load the
/// full coordinator prompt (same as the old inline path). Otherwise a
/// minimal role-specific prompt.
fn build_handler_system_prompt(workgraph_dir: &Path, chat_ref: &str, role: Option<&str>) -> String {
    if chat_ref.starts_with("coordinator-") || role == Some("coordinator") {
        crate::commands::service::coordinator_agent::build_system_prompt(workgraph_dir)
    } else if let Some(r) = role {
        format!("You are acting in the role of: {}.", r)
    } else {
        String::from("You are a workgraph task agent.")
    }
}

// --- Claude stdio bridging ---------------------------------------------------

/// Spawn `claude` with stream-json stdio. Mirrors the flags the daemon
/// previously used inline.
fn spawn_claude_process(
    workgraph_dir: &Path,
    system_prompt: &str,
    model: Option<&str>,
    logger: &HandlerLogger,
) -> Result<(Child, ChildStdin, ChildStdout)> {
    let registry = workgraph::service::executor::ExecutorRegistry::new(workgraph_dir);
    let executor_config = registry
        .load_config("claude")
        .context("load claude executor config")?;
    let command = &executor_config.executor.command;

    let mut cmd = Command::new(command);
    cmd.env_remove("CLAUDECODE");
    cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");
    cmd.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--dangerously-skip-permissions",
    ]);
    cmd.args(["--system-prompt", system_prompt]);
    cmd.args(["--allowedTools", "Bash(wg:*)"]);

    if let Some(m) = model {
        // Strip provider prefix (e.g., "claude:opus" → "opus") for the CLI
        let spec = workgraph::config::parse_model_spec(m);
        cmd.args(["--model", &spec.model_id]);
    }

    cmd.current_dir(workgraph_dir.parent().unwrap_or(workgraph_dir));
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());

    let stderr_path = workgraph_dir
        .join("service")
        .join("claude-handler-stderr.log");
    if let Some(parent) = stderr_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    cmd.stderr(stderr_file);

    logger.info(&format!(
        "claude-handler: spawning {} (model={}, cwd={:?}, stderr={:?})",
        command,
        model.unwrap_or("default"),
        workgraph_dir.parent().unwrap_or(workgraph_dir),
        stderr_path
    ));

    let mut child = cmd.spawn().context("spawn claude CLI process")?;
    let stdin = child.stdin.take().context("claude stdin take")?;
    let stdout = child.stdout.take().context("claude stdout take")?;
    Ok((child, stdin, stdout))
}

/// Format a user message as a stream-json user message.
fn format_stream_json_user_message(content: &str) -> String {
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": content,
        }
    });
    let mut s = serde_json::to_string(&msg).unwrap_or_default();
    s.push('\n');
    s
}

/// Events emitted by the stdout reader.
enum ResponseEvent {
    Text(String),
    #[allow(dead_code)]
    ToolUse {
        name: String,
        input: String,
    },
    #[allow(dead_code)]
    ToolResult(String),
    TurnComplete,
    StreamEnd,
}

struct CollectedResponse {
    summary: String,
}

/// Read Claude stdout line-by-line, parse stream-json, forward to
/// `tx`. Mirrors the daemon's previous inline parser.
fn stdout_reader(stdout: ChildStdout, tx: mpsc::Sender<ResponseEvent>, logger: HandlerLogger) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                logger.warn(&format!("stdout read error: {}", e));
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msg_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "assistant" => {
                if let Some(message) = val.get("message") {
                    if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            let block_type =
                                block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match block_type {
                                "text" => {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                        let _ = tx.send(ResponseEvent::Text(text.to_string()));
                                    }
                                }
                                "tool_use" => {
                                    let name = block
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("unknown")
                                        .to_string();
                                    let input = block
                                        .get("input")
                                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                                        .unwrap_or_default();
                                    let _ = tx.send(ResponseEvent::ToolUse { name, input });
                                }
                                _ => {}
                            }
                        }
                    }
                    let stop_reason = message
                        .get("stop_reason")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if stop_reason == "end_turn" || stop_reason == "stop_sequence" {
                        let _ = tx.send(ResponseEvent::TurnComplete);
                    }
                }
            }
            "tool_use" => {
                let name = val
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let input = val
                    .get("input")
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                let _ = tx.send(ResponseEvent::ToolUse { name, input });
            }
            "tool_result" => {
                let content_text = if let Some(content) = val.get("content") {
                    if let Some(s) = content.as_str() {
                        s.to_string()
                    } else if let Some(arr) = content.as_array() {
                        arr.iter()
                            .filter_map(|b| {
                                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    b.get("text").and_then(|t| t.as_str()).map(String::from)
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    }
                } else if let Some(output) = val.get("output").and_then(|o| o.as_str()) {
                    output.to_string()
                } else {
                    String::new()
                };
                if !content_text.is_empty() {
                    let _ = tx.send(ResponseEvent::ToolResult(content_text));
                }
            }
            "result" => {
                let _ = tx.send(ResponseEvent::TurnComplete);
            }
            _ => {}
        }
    }
    let _ = tx.send(ResponseEvent::StreamEnd);
}

/// Collect the full assistant response until `TurnComplete`.
/// Streams partial text to the `.streaming` file if given.
fn collect_response(
    rx: &mpsc::Receiver<ResponseEvent>,
    logger: &HandlerLogger,
    timeout: Duration,
    streaming: Option<(&Path, &Path, &str)>,
) -> Option<CollectedResponse> {
    let deadline = Instant::now() + timeout;
    let mut text_parts: Vec<String> = Vec::new();
    let mut streaming_text = String::new();

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            logger.warn("response collection timed out");
            return build_collected(&text_parts);
        }

        match rx.recv_timeout(remaining) {
            Ok(ResponseEvent::Text(t)) => {
                if let Some((_, wg_dir, chat_ref)) = streaming {
                    streaming_text.push_str(&t);
                    if !t.ends_with('\n') {
                        streaming_text.push('\n');
                    }
                    let _ = chat::write_streaming_ref(wg_dir, chat_ref, &streaming_text);
                }
                text_parts.push(t);
            }
            Ok(ResponseEvent::ToolUse { .. }) | Ok(ResponseEvent::ToolResult(_)) => {
                // Claude may emit tool_use / tool_result events for its
                // own tool invocations. We don't surface them in the
                // outbox summary — the final text block is the user-
                // visible reply.
            }
            Ok(ResponseEvent::TurnComplete) => {
                if text_parts.is_empty() {
                    // The turn completed with only tool calls; keep
                    // waiting for the next turn's text.
                    continue;
                }
                return build_collected(&text_parts);
            }
            Ok(ResponseEvent::StreamEnd) => {
                logger.warn("stdout stream ended during response collection");
                return build_collected(&text_parts);
            }
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return build_collected(&text_parts);
            }
        }
    }
}

fn build_collected(parts: &[String]) -> Option<CollectedResponse> {
    let summary = parts.last().cloned().unwrap_or_default();
    if summary.is_empty() {
        return None;
    }
    Some(CollectedResponse { summary })
}

// --- SIGINT forwarding -------------------------------------------------------

/// PID of the Claude CLI child process. Set by the handler's main
/// thread before installing the signal handler. The `SIGINT` handler
/// (below) uses it to forward the signal; `libc::kill` is
/// async-signal-safe so this is legal from inside a signal handler.
/// 0 means "no child spawned yet" — the handler ignores SIGINT in
/// that case rather than crashing.
static CLAUDE_CHILD_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn sigint_forwarder(_sig: libc::c_int) {
    // Async-signal-safe: just read the atomic + issue kill.
    let pid = CLAUDE_CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
    }
    // Do NOT exit the handler process — Claude CLI treats SIGINT as
    // "stop generating" and the handler continues processing future
    // inbox messages after the interrupted turn flushes.
}

fn install_sigint_forwarder() {
    unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_forwarder as *const () as libc::sighandler_t,
        );
    }
}

// --- Handler-local logger ----------------------------------------------------

#[derive(Clone)]
struct HandlerLogger {
    inner: Arc<Mutex<HandlerLoggerInner>>,
}

struct HandlerLoggerInner {
    file: std::fs::File,
}

impl HandlerLogger {
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open handler log {:?}", path))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(HandlerLoggerInner { file })),
        })
    }

    fn log(&self, level: &str, msg: &str) {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let line = format!("{} [{}] {}\n", ts, level, msg);
        // Also mirror to stderr so the daemon captures it via its
        // child-stderr pipe (gives operators a single log to tail).
        eprint!("{}", line);
        if let Ok(mut inner) = self.inner.lock() {
            let _ = inner.file.write_all(line.as_bytes());
            let _ = inner.file.flush();
        }
    }

    fn info(&self, msg: &str) {
        self.log("INFO", msg);
    }
    fn warn(&self, msg: &str) {
        self.log("WARN", msg);
    }
    fn error(&self, msg: &str) {
        self.log("ERROR", msg);
    }
}
