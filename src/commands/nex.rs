//! Interactive multi-turn REPL using the native executor.
//!
//! `wg nex` drops the user into an agentic coding session powered by any
//! OpenAI-compatible model. Supports streaming, tool calling, and multi-turn
//! conversation.

use std::path::Path;

use anyhow::{Context, Result};

use workgraph::config::{Config, DispatchRole};
use workgraph::executor::native::agent::AgentLoop;
use workgraph::executor::native::provider::create_provider_ext;
use workgraph::executor::native::tools::ToolRegistry;
use workgraph::models::ModelRegistry;

#[allow(clippy::too_many_arguments)]
pub fn run(
    workgraph_dir: &Path,
    model: Option<&str>,
    endpoint: Option<&str>,
    system_prompt: Option<&str>,
    message: Option<&str>,
    max_turns: usize,
    chatty: bool,
    verbose: bool,
    read_only: bool,
    resume: Option<&str>,
    role: Option<&str>,
    chat_id: Option<u32>,
    chat_ref: Option<&str>,
    autonomous: bool,
    no_mcp: bool,
    eval_mode: bool,
) -> Result<()> {
    // --eval-mode is a preset for benchmark-harness invocation:
    //   * implies --autonomous  (one-shot, EndTurn exits the loop)
    //   * implies --no-mcp      (deterministic tool surface)
    //   * no chat-file surface  (no inbox/outbox/.streaming pollution
    //                            in the repo being evaluated)
    //   * silent banner         (clean stderr for harness logs)
    //   * stdout JSON summary   (machine-readable harness output)
    // The flags are forced here rather than at CLI-parse time so the
    // CLI surface stays orthogonal — a caller could still pass
    // `--autonomous --eval-mode` redundantly without confusion.
    let autonomous = autonomous || eval_mode;
    let no_mcp = no_mcp || eval_mode;

    let config = Config::load_or_default(workgraph_dir);

    let effective_model = model
        .map(String::from)
        .or_else(|| std::env::var("WG_MODEL").ok())
        .unwrap_or_else(|| config.resolve_model_for_role(DispatchRole::TaskAgent).model);

    let working_dir = std::env::current_dir().unwrap_or_default();

    let is_coordinator = role.is_some_and(|r| r.eq_ignore_ascii_case("coordinator"));

    // The tokio runtime is created here rather than later so MCP
    // server spawn/handshake can run inside it before we hand the
    // registry to `AgentLoop`.
    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;

    let mut registry = {
        let mut reg = ToolRegistry::default_all_with_config(
            workgraph_dir,
            &working_dir,
            &config.native_executor,
        );
        if is_coordinator {
            // Coordinator mode: keep ALL wg tools — the agent manages
            // the workgraph (add tasks, mark done, log, etc.).
        } else {
            // Interactive/skill mode: strip wg mutation tools — there's
            // no task context. wg_show/wg_list kept for browsing.
            reg.remove_tools(&["wg_done", "wg_add", "wg_fail", "wg_rescue", "wg_artifact"]);
        }
        if read_only {
            reg.filter_read_only()
        } else {
            reg
        }
    };

    // MCP: spawn configured servers, discover their tools, register
    // each one into the registry. The returned `_mcp_manager` keeps
    // all server subprocesses alive for the lifetime of this nex
    // session (servers are killed when the manager is dropped).
    let _mcp_manager = if no_mcp || config.mcp.servers.is_empty() {
        None
    } else {
        let server_configs: Vec<workgraph::executor::native::mcp::McpServerConfig> = config
            .mcp
            .servers
            .iter()
            .map(|s| workgraph::executor::native::mcp::McpServerConfig {
                name: s.name.clone(),
                command: s.command.clone(),
                args: s.args.clone(),
                env: s.env.clone(),
                enabled: s.enabled,
            })
            .collect();
        rt.block_on(async {
            match workgraph::executor::native::mcp::manager::start_and_discover(server_configs)
                .await
            {
                Ok((manager, tools)) => {
                    let count = tools.len();
                    for t in tools {
                        registry.register(Box::new(t));
                    }
                    if verbose || count > 0 {
                        eprintln!(
                            "\x1b[2m[wg nex] MCP: {} tools from {} server(s)\x1b[0m",
                            count,
                            manager.server_count()
                        );
                    }
                    Some(manager)
                }
                Err(e) => {
                    eprintln!(
                        "\x1b[33m[wg nex] MCP startup failed: {} — continuing without MCP\x1b[0m",
                        e
                    );
                    None
                }
            }
        })
    };

    // Load role/skill content from the agency primitives directory.
    // "coordinator" is a special-case role handled above (restores
    // wg tools). Other role names are looked up by fuzzy match
    // against component names in .workgraph/agency/primitives/components/.
    let role_prompt_addendum = if let Some(role_name) = role {
        if is_coordinator {
            Some(
                "You are operating as a workgraph coordinator. Your tools include \
                 wg_add, wg_done, wg_log, wg_list, wg_show, and related graph operations. \
                 You dispatch work rather than doing it directly."
                    .to_string(),
            )
        } else {
            match load_agency_role(workgraph_dir, role_name) {
                Some(content) => {
                    eprintln!("\x1b[2m[wg nex] loaded role: {}\x1b[0m", role_name);
                    Some(content)
                }
                None => {
                    eprintln!(
                        "\x1b[33m[wg nex] role '{}' not found in agency primitives\x1b[0m",
                        role_name
                    );
                    None
                }
            }
        }
    } else {
        None
    };

    let now = chrono::Local::now();
    let default_system = format!(
        "You are an AI assistant in an interactive terminal session. You have tools for \
         reading and writing files, running shell commands, searching and fetching from \
         the web, and summarizing or delegating work.\n\
         \n\
         Working directory: {}\n\
         Current date: {} ({})\n\
         \n\
         Note: workgraph mutation tools (wg_done, wg_add, wg_log, wg_fail) are not for \
         this session — they belong to task-agent runs, not interactive conversations.",
        working_dir.display(),
        now.format("%Y-%m-%d %H:%M %Z"),
        now.format("%A"),
    );
    let system_with_role = if let Some(ref addendum) = role_prompt_addendum {
        format!("{}\n\n## Role\n\n{}", default_system, addendum)
    } else {
        default_system.clone()
    };
    let system = system_prompt.unwrap_or(&system_with_role);

    // Every nex session — CLI, coordinator, task-agent — lives under
    // `<workgraph>/chat/<ref>/`. Pick the reference:
    //   1. `--chat <ref>`  — explicit, wins over everything else.
    //   2. `--chat-id N`   — legacy numeric id, same effect.
    //   3. `--resume`      — interactive picker (no arg) or pattern
    //                        match (with arg), resolves to an
    //                        existing session's alias.
    //   4. None of the above — fresh session with a new UUID.
    //
    // Bare `wg nex` (no flags) no longer auto-resumes a tty-
    // derived session. That was confusing (recycled ptys could
    // resurrect stranger conversations) and the failure mode
    // wasn't what users expected. `--resume` is now the explicit
    // opt-in.
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let session_ref: String = if let Some(r) = chat_ref {
        r.to_string()
    } else if let Some(n) = chat_id {
        let _ = workgraph::chat_sessions::migrate_numeric_coord_dir(workgraph_dir, n);
        let _ = workgraph::chat_sessions::ensure_session(
            workgraph_dir,
            &format!("coordinator-{}", n),
            workgraph::chat_sessions::SessionKind::Coordinator,
            Some(format!("coordinator {}", n)),
        );
        n.to_string()
    } else if let Some(pattern) = resume {
        // `--resume` with optional pattern. Empty pattern → picker.
        // Non-empty → substring match on alias/uuid/kind, pick the
        // most-recent matching session.
        match pick_resume_session(workgraph_dir, pattern) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("\x1b[33m[wg nex] --resume: {}\x1b[0m", e);
                eprintln!(
                    "\x1b[2m  Starting a fresh session instead. Use `wg session list` to see what's available.\x1b[0m"
                );
                fresh_session(workgraph_dir, &stamp)?
            }
        }
    } else {
        // Fresh session. Every bare `wg nex` invocation gets a new
        // UUID and a new journal.
        fresh_session(workgraph_dir, &stamp)?
    };

    let chat_dir = workgraph_dir.join("chat").join(&session_ref);
    let _ = std::fs::create_dir_all(&chat_dir);
    let journal_path = chat_dir.join("conversation.jsonl");
    let output_log = chat_dir.join("trace.ndjson");

    // Resume is enabled iff the chosen session has a journal.
    // With the new semantics, this is always true for `--resume` /
    // `--chat <ref>` pointing at a real session, and always false
    // for fresh sessions. No magic auto-resume.
    let journal_exists = journal_path.exists();
    let resume_enabled = journal_exists;
    if resume_enabled {
        eprintln!("\x1b[1;33m[wg nex] resuming session {}\x1b[0m", session_ref);
    }

    if verbose {
        eprintln!(
            "\x1b[2m[wg nex] session log → {}\x1b[0m",
            output_log.display()
        );
        eprintln!(
            "\x1b[2m[wg nex] journal    → {}\x1b[0m",
            journal_path.display()
        );
    }

    let client = create_provider_ext(workgraph_dir, &effective_model, None, endpoint, None)?;

    let model_registry = ModelRegistry::load(workgraph_dir).unwrap_or_default();
    let supports_tools = model_registry.supports_tool_use(&effective_model);

    let mut agent = AgentLoop::with_tool_support(
        client,
        registry,
        system.to_string(),
        max_turns,
        output_log,
        supports_tools,
    )
    .with_nex_verbose(verbose)
    .with_nex_chatty(chatty || verbose)
    .with_nex_repl_mode(true)
    .with_journal(journal_path, format!("nex-{}", stamp))
    .with_working_dir(working_dir.clone())
    .with_workgraph_dir(workgraph_dir.to_path_buf())
    .with_resume(resume_enabled);

    // Chat-file I/O surface. Enabled whenever the caller said "I'm
    // tethered to a chat dir" (via `--chat` or `--chat-id`) OR when
    // running autonomous (task-agent mode) — autonomous runs always
    // want their inbox/outbox on disk so someone can attach to them
    // later via `wg chat attach <ref>`.
    //
    // Plain interactive `wg nex` (no flags) does NOT mount the chat
    // surface — it uses stdin/stderr for the human's low-latency
    // typing path, with the journal still written to
    // `chat/<ref>/conversation.jsonl` for persistence + auto-resume.
    // Eval mode skips the chat surface even though it's autonomous:
    // the benchmarked repo shouldn't get inbox.jsonl/outbox.jsonl/
    // .streaming files written into its `.workgraph/chat/<alias>/`
    // directory (no attacher will ever read them, and some graders
    // diff the working tree). Explicit chat bindings still win.
    let mount_chat_surface = chat_ref.is_some()
        || chat_id.is_some()
        || (autonomous && !eval_mode);
    if mount_chat_surface {
        agent = agent.with_chat_ref(
            workgraph_dir.to_path_buf(),
            session_ref.clone(),
            resume_enabled,
        );
    }
    if autonomous {
        agent = agent.with_autonomous(true);
    }

    if let Some(entry) = config.registry_lookup(&effective_model) {
        agent = agent.with_registry_entry(entry);
    }

    // Always show the minimal banner — it names the model so the user
    // knows what they're talking to. Verbose-only details (warning
    // text, exit hint) are gated. Eval mode is the one exception:
    // the harness captures stderr as logs, we keep it clean.
    if !eval_mode {
        if read_only {
            eprintln!(
                "\x1b[1;32mwg nex\x1b[0m \x1b[33m[read-only]\x1b[0m — interactive session with \x1b[1m{}\x1b[0m",
                effective_model
            );
        } else {
            eprintln!(
                "\x1b[1;32mwg nex\x1b[0m — interactive session with \x1b[1m{}\x1b[0m",
                effective_model
            );
        }
        if !supports_tools {
            eprintln!(
                "\x1b[33mWarning: model '{}' may not support tool use\x1b[0m",
                effective_model
            );
        }
        if verbose {
            eprintln!("Type /quit or Ctrl-D to exit.\n");
        } else {
            eprintln!();
        }
    }

    // Eval mode: suppress the stderr half of `tool_progress!` for
    // the duration of the run. Callback routing (if any scope
    // installs one) still works; only the process-wide stderr
    // broadcast is silenced. Non-eval callers pass `false` and the
    // scope is a no-op — backward-compatible.
    let result = rt.block_on(
        workgraph::executor::native::tools::progress::stderr_scope(
            eval_mode,
            agent.run_interactive(message),
        ),
    )?;

    if verbose {
        eprintln!(
            "\n\x1b[2mSession: {} turns, {} input + {} output tokens\x1b[0m",
            result.turns, result.total_usage.input_tokens, result.total_usage.output_tokens,
        );
    }

    // Eval mode: emit a single-line JSON summary on stdout so the
    // benchmark harness has a parseable completion record. Stdout
    // is reserved for this one line; everything else (banner,
    // progress, errors) lives on stderr. Emitted BEFORE the abnormal-
    // exit bail below so graders see the full outcome even on
    // failures (status becomes "abnormal" + exit_reason names it).
    if eval_mode {
        let status = if result.terminated_cleanly() {
            "ok"
        } else {
            "abnormal"
        };
        println!(
            "{{\"status\":\"{}\",\"turns\":{},\"input_tokens\":{},\"output_tokens\":{},\"exit_reason\":{}}}",
            status,
            result.turns,
            result.total_usage.input_tokens,
            result.total_usage.output_tokens,
            serde_json::to_string(&result.exit_reason).unwrap_or_else(|_| "\"\"".to_string()),
        );
    }

    // When the loop exited abnormally (context_limit, max_turns, etc.),
    // propagate that as a non-zero process exit so any wrapper (e.g., the
    // autonomous agent runner that calls `complete_task` on exit 0) marks
    // the driving task as FAILED rather than DONE. Observed 2026-04-17 on
    // ulivo: a research task hit the context limit on turn 34, the loop
    // returned Ok(result), the wrapper saw exit 0 and marked the graph
    // task done — with no deliverable on disk and FLIP scoring 0.45. The
    // mis-status broke downstream assumptions.
    if !result.terminated_cleanly() {
        anyhow::bail!(
            "agent loop terminated abnormally (reason: {}). \
             {} turns, {} input + {} output tokens. \
             Session journal is preserved; inspect it to recover state.",
            result.exit_reason,
            result.turns,
            result.total_usage.input_tokens,
            result.total_usage.output_tokens,
        );
    }

    Ok(())
}

/// Create a fresh interactive session and return its alias. The
/// alias combines the controlling tty (if any) with the timestamp,
/// so running `wg nex` twice in the same terminal produces two
/// DISTINCT sessions instead of one that silently accumulates. To
/// resume either, use `wg nex --resume`.
fn fresh_session(workgraph_dir: &Path, stamp: &str) -> Result<String> {
    let alias = default_interactive_alias(stamp);
    workgraph::chat_sessions::ensure_session(
        workgraph_dir,
        &alias,
        workgraph::chat_sessions::SessionKind::Interactive,
        Some(format!("interactive {}", alias)),
    )
    .map_err(|e| anyhow::anyhow!("failed to register fresh session: {}", e))?;
    Ok(alias)
}

/// Resolve `--resume [PATTERN]` to a concrete session alias.
///
/// - empty pattern: show an interactive picker over all sessions,
///   most-recent-journal first. Returns the picked session's
///   alias (or first UUID if no aliases).
/// - non-empty pattern: substring-match against session aliases,
///   UUID prefixes, and kinds (interactive / coordinator /
///   task-agent / other). Pick the most-recent matching session.
///
/// Errors if nothing matches, or if stdin isn't a tty for the
/// picker path.
fn pick_resume_session(workgraph_dir: &Path, pattern: &str) -> Result<String> {
    let sessions =
        workgraph::chat_sessions::list(workgraph_dir).context("failed to list sessions")?;
    if sessions.is_empty() {
        anyhow::bail!("no sessions to resume — `wg session list` is empty");
    }

    // Sort most-recent-first by journal mtime, falling back to the
    // `created` string on meta when the journal is missing.
    let mut ranked: Vec<_> = sessions
        .into_iter()
        .map(|(uuid, meta)| {
            let journal = workgraph_dir
                .join("chat")
                .join(&uuid)
                .join("conversation.jsonl");
            let mtime = std::fs::metadata(&journal).and_then(|m| m.modified()).ok();
            (uuid, meta, mtime)
        })
        .collect();
    ranked.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| b.1.created.cmp(&a.1.created)));

    let pat = pattern.trim();
    if !pat.is_empty() {
        // Pattern match: return the first (most-recent) session
        // whose alias, UUID prefix, or kind contains the pattern
        // (case-insensitive).
        let needle = pat.to_lowercase();
        for (uuid, meta, _) in &ranked {
            let kind_str = format!("{:?}", meta.kind).to_lowercase();
            if uuid.to_lowercase().starts_with(&needle)
                || meta
                    .aliases
                    .iter()
                    .any(|a| a.to_lowercase().contains(&needle))
                || kind_str.contains(&needle)
            {
                return Ok(pick_best_ref(uuid, meta));
            }
        }
        anyhow::bail!("no session matches pattern {:?}", pattern);
    }

    // Empty pattern: interactive picker. Require a tty so
    // non-interactive callers (scripts, the daemon) get a clear
    // error instead of a hang.
    use dialoguer::{Select, theme::ColorfulTheme};
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        anyhow::bail!(
            "--resume requires a terminal for the picker; pass a pattern or an explicit `--chat <ref>`"
        );
    }
    let options: Vec<String> = ranked
        .iter()
        .take(30)
        .map(|(uuid, meta, _)| {
            let short = &uuid[..std::cmp::min(uuid.len(), 8)];
            let aliases = if meta.aliases.is_empty() {
                String::new()
            } else {
                format!(" [{}]", meta.aliases.join(", "))
            };
            let kind = format!("{:?}", meta.kind).to_lowercase();
            let label = meta.label.as_deref().unwrap_or("");
            format!("{} {} {}{}", short, kind, aliases, label)
        })
        .collect();
    if options.is_empty() {
        anyhow::bail!("no sessions to resume");
    }
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Resume which session?")
        .default(0)
        .items(&options)
        .interact()
        .context("picker cancelled")?;
    let (uuid, meta, _) = &ranked[selection];
    Ok(pick_best_ref(uuid, meta))
}

/// Choose the most user-friendly reference for a session: the first
/// alias if present, otherwise the full UUID. Aliases are preferred
/// because they're shorter, readable, and stable across re-registrations.
fn pick_best_ref(uuid: &str, meta: &workgraph::chat_sessions::SessionMeta) -> String {
    meta.aliases
        .first()
        .cloned()
        .unwrap_or_else(|| uuid.to_string())
}

/// Derive a tty-stamp alias for a fresh interactive session. Used
/// only at session CREATION — resume goes through the picker.
///
/// Format: `tty-<pts-slug>-<stamp>`. The stamp keeps separate
/// invocations distinct even in the same terminal; `wg nex`
/// followed by `wg nex` produces two different sessions rather
/// than silently merging.
fn default_interactive_alias(stamp: &str) -> String {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        unsafe {
            // STDIN fd = 0
            let name = libc::ttyname(0);
            if !name.is_null() {
                let s = CStr::from_ptr(name).to_string_lossy();
                let slug = s
                    .trim_start_matches("/dev/")
                    .replace('/', "-")
                    .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-");
                if !slug.is_empty() {
                    return format!("tty-{}-{}", slug, stamp);
                }
            }
        }
    }
    format!("session-{}", stamp)
}

/// Load an agency role/skill component by name. Scans all YAML files
/// in `.workgraph/agency/primitives/components/` for one whose `name`
/// field matches (case-insensitive substring match). Returns the
/// `content` field as a string, or None if no match found.
fn load_agency_role(workgraph_dir: &Path, role_name: &str) -> Option<String> {
    let components_dir = workgraph_dir.join("agency/primitives/components");
    let entries = std::fs::read_dir(&components_dir).ok()?;
    let needle = role_name.to_lowercase();

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        // Quick check before full YAML parse — skip files whose text
        // doesn't contain the needle at all.
        if !text.to_lowercase().contains(&needle) {
            continue;
        }
        // Parse the YAML and check the name field.
        let doc: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
        let name = doc.get("name")?.as_str()?;
        if name.to_lowercase().contains(&needle) {
            // Found it — return the content field.
            let content = doc.get("content")?;
            return match content {
                serde_yaml::Value::Tagged(tagged) => Some(tagged.value.as_str()?.to_string()),
                serde_yaml::Value::String(s) => Some(s.clone()),
                _ => content.as_str().map(String::from),
            };
        }
    }
    None
}
