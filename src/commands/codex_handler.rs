//! `wg codex-handler` — Codex CLI bridge for multi-turn chat.
//!
//! Peer of `wg claude-handler` and `wg nex --chat`. Dispatched by
//! `wg spawn-task` when the session's executor is `codex`.
//!
//! Architecture: Codex is single-shot (`codex exec` reads a prompt,
//! runs the turn, exits). To keep chat session state across turns we
//! re-spawn Codex for every inbox message, prepending the full
//! accumulated conversation history (from chat/*.jsonl) as a
//! "previous turns" block.
//!
//! Advantages of this model: no long-lived subprocess to supervise,
//! no stream-json parser to maintain, crashes are a non-event (next
//! turn restarts fresh). The cost is replayed context on every turn
//! — fine for coordinator workloads which are low-frequency anyway.
//!
//! ## Stdout-is-protocol contract
//!
//! Stdout for this handler binary is the protocol stream parent
//! supervisors parse line-by-line. **Never write diagnostic text to
//! stdout from this file or anything it transitively calls** — config
//! warnings, deprecation notices, and debug logs go to stderr or
//! `handler.log`. See `tests/integration_handler_stdout_pristine.rs`
//! for the regression lock.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use workgraph::chat;
use workgraph::session_lock::{HandlerKind, SessionLock};

const INBOX_POLL: Duration = Duration::from_millis(200);

pub fn run(
    workgraph_dir: &Path,
    chat_ref: &str,
    resume: bool,
    role: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let _ = resume; // accepted for argv symmetry; codex single-shot has no journal

    // Route through the session registry so aliases resolve to the
    // UUID-backed storage dir — see `chat::chat_dir_for_ref`.
    let chat_dir = workgraph::chat::chat_dir_for_ref(workgraph_dir, chat_ref);
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
        "codex-handler starting: chat_ref={}, role={:?}, model={:?}",
        chat_ref, role, model
    ));

    let system_prompt = build_handler_system_prompt(workgraph_dir, chat_ref, role);
    let coordinator_id = parse_coordinator_id(chat_ref);

    // Cursor: skip inbox messages already answered (matched by
    // request_id in outbox). Same logic as claude-handler.
    let mut inbox_cursor: u64 = last_answered_inbox_id(workgraph_dir, chat_ref);
    logger.info(&format!(
        "codex-handler ready: inbox_cursor={}, coordinator_id={:?}",
        inbox_cursor, coordinator_id
    ));

    loop {
        let new_msgs = match chat::read_inbox_since_ref(workgraph_dir, chat_ref, inbox_cursor) {
            Ok(msgs) => msgs,
            Err(e) => {
                logger.warn(&format!("inbox read error: {}", e));
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
                "codex-handler: processing inbox id={} request_id={} ({} chars)",
                msg.id,
                request_id,
                msg.content.len()
            ));

            // Session resume: reuse the Codex thread_id across
            // turns so the model keeps server-side context (cheaper
            // + faster than replaying history every turn). Session
            // id is persisted to .codex-session-id; first turn
            // reads empty (None), subsequent turns reuse.
            let session_id_path = workgraph_dir
                .join("chat")
                .join(chat_ref)
                .join(".codex-session-id");
            let prior_session_id = std::fs::read_to_string(&session_id_path)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            // With session resume, we don't need to replay history —
            // only send the new user turn. On first turn (no session
            // id yet), include system prompt + graph context so
            // codex knows its role.
            let prompt = if prior_session_id.is_some() {
                assemble_followup_prompt(workgraph_dir, coordinator_id, &msg.content)
            } else {
                assemble_first_turn_prompt(
                    workgraph_dir,
                    coordinator_id,
                    &system_prompt,
                    &msg.content,
                )
            };

            let streaming_path = chat::streaming_path_ref(workgraph_dir, chat_ref);
            let reply = match run_codex_turn(
                &prompt,
                prior_session_id.as_deref(),
                model,
                workgraph_dir,
                &streaming_path,
                &session_id_path,
                &logger,
            ) {
                Ok(t) => t,
                Err(e) => {
                    let err_str = format!("{}", e);
                    if prior_session_id.is_some() && is_stale_session_error(&err_str) {
                        logger.warn(&format!(
                            "session not resumable, clearing stale session ID and retrying fresh: {}",
                            e
                        ));
                        let _ = std::fs::remove_file(&session_id_path);
                        let fresh_prompt = assemble_first_turn_prompt(
                            workgraph_dir,
                            coordinator_id,
                            &system_prompt,
                            &msg.content,
                        );
                        match run_codex_turn(
                            &fresh_prompt,
                            None,
                            model,
                            workgraph_dir,
                            &streaming_path,
                            &session_id_path,
                            &logger,
                        ) {
                            Ok(t) => t,
                            Err(e2) => {
                                logger.error(&format!("codex fresh turn also failed: {}", e2));
                                format!(
                                    "The coordinator encountered an error running codex: {}. Please retry.",
                                    e2
                                )
                            }
                        }
                    } else {
                        logger.error(&format!("codex turn failed: {}", e));
                        format!(
                            "The coordinator encountered an error running codex: {}. Please retry.",
                            e
                        )
                    }
                }
            };

            if let Err(e) = chat::append_outbox_ref(workgraph_dir, chat_ref, &reply, &request_id) {
                logger.error(&format!("outbox write failed: {}", e));
            } else {
                logger.info(&format!(
                    "codex-handler: response written ({} chars) for {}",
                    reply.len(),
                    request_id
                ));
            }

            chat::clear_streaming_ref(workgraph_dir, chat_ref);
        }
    }
}

fn parse_coordinator_id(chat_ref: &str) -> Option<u32> {
    chat_ref
        .strip_prefix("coordinator-")
        .and_then(|s| s.parse::<u32>().ok())
}

fn last_answered_inbox_id(workgraph_dir: &Path, chat_ref: &str) -> u64 {
    let inbox = match chat::read_inbox_since_ref(workgraph_dir, chat_ref, 0) {
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

fn build_handler_system_prompt(workgraph_dir: &Path, chat_ref: &str, role: Option<&str>) -> String {
    if chat_ref.starts_with("coordinator-") || role == Some("coordinator") {
        crate::commands::service::coordinator_agent::build_system_prompt(workgraph_dir)
    } else if let Some(r) = role {
        format!("You are acting in the role of: {}.", r)
    } else {
        String::from("You are a workgraph task agent.")
    }
}

/// First turn: include the full system prompt + graph context so
/// codex understands its role. Subsequent turns use
/// `assemble_followup_prompt` which is much shorter because session
/// resume preserves the prior turns server-side.
fn assemble_first_turn_prompt(
    workgraph_dir: &Path,
    coordinator_id: Option<u32>,
    system_prompt: &str,
    latest_user_msg: &str,
) -> String {
    let mut out = String::new();
    out.push_str("# System\n");
    out.push_str(system_prompt);
    out.push_str("\n\n");

    if let Some(cid) = coordinator_id
        && let Ok(ctx) = crate::commands::service::coordinator_agent::build_coordinator_context(
            workgraph_dir,
            "1970-01-01T00:00:00Z",
            None,
            cid,
        )
        && !ctx.is_empty()
    {
        out.push_str(&ctx);
        out.push_str("\n\n");
    }

    out.push_str("# User\n");
    out.push_str(latest_user_msg);
    out.push_str(
        "\n\nRespond to the user. Use `wg` shell tools to inspect the graph when the answer \
         requires live state. Keep your reply concise.",
    );
    out
}

/// Follow-up turn: just the user message + a refreshed graph state
/// snapshot (so coordinator sees current task status each turn). No
/// history replay — the resumed codex thread already has it.
fn assemble_followup_prompt(
    workgraph_dir: &Path,
    coordinator_id: Option<u32>,
    latest_user_msg: &str,
) -> String {
    let mut out = String::new();
    if let Some(cid) = coordinator_id
        && let Ok(ctx) = crate::commands::service::coordinator_agent::build_coordinator_context(
            workgraph_dir,
            "1970-01-01T00:00:00Z",
            None,
            cid,
        )
        && !ctx.is_empty()
    {
        out.push_str(&ctx);
        out.push_str("\n\n");
    }
    out.push_str("# User\n");
    out.push_str(latest_user_msg);
    out
}

/// Spawn `codex exec` (first turn) or `codex exec resume <id>`
/// (follow-up turn). Parse JSONL line-by-line as codex emits it,
/// streaming partial text to `.streaming` and capturing the new
/// thread_id into `session_id_path` for subsequent turns.
///
/// Returns the full text of the final `agent_message`.
#[allow(clippy::too_many_arguments)]
fn run_codex_turn(
    prompt: &str,
    resume_session_id: Option<&str>,
    model: Option<&str>,
    workgraph_dir: &Path,
    streaming_path: &Path,
    session_id_path: &Path,
    logger: &HandlerLogger,
) -> Result<String> {
    let mut cmd = Command::new("codex");
    if let Some(sid) = resume_session_id {
        // `codex exec resume <SESSION_ID>` — reuses server-side
        // conversation state. Prompt (read from stdin via `-`) is
        // just the new user turn.
        cmd.arg("exec")
            .arg("resume")
            .arg(sid)
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("--dangerously-bypass-approvals-and-sandbox");
    } else {
        cmd.arg("exec")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("--dangerously-bypass-approvals-and-sandbox");
    }

    // OAI-compat plumbing: when the wg session has a custom endpoint,
    // pass `--config` overrides so codex routes to it instead of
    // `api.openai.com`. The api_key reaches the codex subprocess via
    // the OPENAI_API_KEY env var (codex looks it up via env_key).
    let endpoint_url = crate::commands::codex_oai_compat::endpoint_url_from_env();
    if let Some(ref url) = endpoint_url {
        for ovr in crate::commands::codex_oai_compat::config_overrides(url) {
            cmd.arg("--config").arg(&ovr);
        }
        logger.info(&format!(
            "codex-handler: routing to custom endpoint {} via --config overrides",
            url
        ));
    }
    if let Some(key) = crate::commands::codex_oai_compat::api_key_from_env() {
        cmd.env(crate::commands::codex_oai_compat::ENV_KEY_NAME, key);
    }

    if let Some(m) = model {
        let spec = workgraph::config::parse_model_spec(m);
        cmd.arg("--model").arg(&spec.model_id);
    }
    cmd.current_dir(workgraph_dir.parent().unwrap_or(workgraph_dir));
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    logger.info(&format!(
        "codex-handler: spawning `codex exec{}` (model={}, endpoint={}, cwd={:?})",
        if resume_session_id.is_some() {
            " resume"
        } else {
            ""
        },
        model.unwrap_or("default"),
        endpoint_url.as_deref().unwrap_or("default"),
        workgraph_dir.parent().unwrap_or(workgraph_dir)
    ));

    let mut child = cmd.spawn().context("spawn codex")?;
    {
        let mut stdin = child.stdin.take().context("codex stdin")?;
        stdin
            .write_all(prompt.as_bytes())
            .context("write prompt to codex stdin")?;
        stdin.flush().ok();
    }

    // Stream-parse stdout. We need to both (a) display text as it
    // arrives for the TUI `.streaming` file and (b) capture the
    // thread_id so subsequent turns can resume.
    let stdout = child.stdout.take().context("codex stdout take")?;
    let reader = BufReader::new(stdout);
    let mut last_agent_text: Option<String> = None;

    for line in reader.lines().map_while(|l| l.ok()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "thread.started" => {
                // Persist the new thread id for future turns to resume.
                if resume_session_id.is_none()
                    && let Some(tid) = val.get("thread_id").and_then(|t| t.as_str())
                {
                    if let Some(parent) = session_id_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(e) = std::fs::write(session_id_path, tid) {
                        logger.warn(&format!("failed to persist codex session id: {}", e));
                    } else {
                        logger.info(&format!("codex-handler: new session id={}", tid));
                    }
                }
            }
            "item.completed" | "item.updated" => {
                if let Some(item) = val.get("item")
                    && item.get("type").and_then(|t| t.as_str()) == Some("agent_message")
                    && let Some(text) = item.get("text").and_then(|t| t.as_str())
                {
                    last_agent_text = Some(text.to_string());
                    // Stream the text as it grows.
                    let mut streaming_text = text.to_string();
                    if !streaming_text.ends_with('\n') {
                        streaming_text.push('\n');
                    }
                    if let Some(parent) = streaming_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(streaming_path, &streaming_text);
                }
            }
            "turn.completed" => {
                if let Some(usage) = val.get("usage") {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cached = usage
                        .get("cached_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    logger.info(&format!(
                        "codex-handler: turn done (input={}, cached={}, output={})",
                        input, cached, output
                    ));
                }
            }
            _ => {}
        }
    }

    // Read any remaining stderr before waiting (prevents pipe deadlock on
    // large error output, though session-not-found errors are small).
    let stderr_output = child.stderr.take().map(|stderr| {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut buf);
        buf
    }).unwrap_or_default();

    let status = child.wait().context("codex wait")?;
    if !status.success() {
        let stderr_trimmed = stderr_output.trim();
        if stderr_trimmed.is_empty() {
            anyhow::bail!("codex exec exited {}", status);
        } else {
            anyhow::bail!("codex exec exited {}: {}", status, stderr_trimmed);
        }
    }

    last_agent_text.ok_or_else(|| anyhow::anyhow!("no agent_message in codex JSONL output"))
}

fn is_stale_session_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("no conversation found")
        || lower.contains("session not found")
        || lower.contains("invalid session")
        || lower.contains("could not resume")
}

// --- Handler-local logger ----------------------------------------------------

#[derive(Clone)]
struct HandlerLogger {
    inner: std::sync::Arc<std::sync::Mutex<HandlerLoggerInner>>,
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
            inner: std::sync::Arc::new(std::sync::Mutex::new(HandlerLoggerInner { file })),
        })
    }

    fn log(&self, level: &str, msg: &str) {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let line = format!("{} [{}] {}\n", ts, level, msg);
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
