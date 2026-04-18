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
    resume: bool,
    role: Option<&str>,
    chat_id: Option<u32>,
    chat_ref: Option<&str>,
    autonomous: bool,
) -> Result<()> {
    let config = Config::load_or_default(workgraph_dir);

    let effective_model = model
        .map(String::from)
        .or_else(|| std::env::var("WG_MODEL").ok())
        .unwrap_or_else(|| config.resolve_model_for_role(DispatchRole::TaskAgent).model);

    let working_dir = std::env::current_dir().unwrap_or_default();

    let is_coordinator = role.is_some_and(|r| r.eq_ignore_ascii_case("coordinator"));

    let registry = {
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

    // Load role/skill content from the agency primitives directory.
    // "coordinator" is a special-case role handled above (restores
    // wg tools). Other role names are looked up by fuzzy match
    // against component names in .workgraph/agency/primitives/components/.
    let role_prompt_addendum = if let Some(role_name) = role {
        if is_coordinator {
            // Compose the full coordinator system prompt from the
            // project's `agency/coordinator-prompt/*.md` files, with
            // a built-in fallback for projects that haven't
            // customized. This is the same prompt the daemon used
            // to hand to the Claude CLI, now shared between every
            // coordinator invocation.
            Some(workgraph::coordinator_prompt::build_system_prompt(
                workgraph_dir,
            ))
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
    //   1. Explicit `--chat <ref>` wins.
    //   2. Else legacy `--chat-id N` (resolves through the numeric
    //      alias symlink, for back-compat with daemons/tests that
    //      still pass the old flag).
    //   3. Else a tty-derived default so running `wg nex` in the
    //      same terminal auto-resumes without needing a flag.
    //
    // Whatever we pick, if the session isn't in the registry yet,
    // we register it here with `ensure_session` so future `wg chat
    // list` sees it and the alias symlink is wired up.
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let session_ref: String = if let Some(r) = chat_ref {
        r.to_string()
    } else if let Some(n) = chat_id {
        // Legacy numeric id — migrate old `chat/N/` real dir to a
        // UUID-named dir under the `coordinator-N` alias if needed.
        let _ = workgraph::chat_sessions::migrate_numeric_coord_dir(workgraph_dir, n);
        let _ = workgraph::chat_sessions::ensure_session(
            workgraph_dir,
            &format!("coordinator-{}", n),
            workgraph::chat_sessions::SessionKind::Coordinator,
            Some(format!("coordinator {}", n)),
        );
        n.to_string()
    } else {
        // Interactive CLI. Sticky alias per tty so `wg nex` + Ctrl-C
        // + `wg nex` reattaches to the same session.
        let alias = default_interactive_alias(&stamp);
        let _ = workgraph::chat_sessions::ensure_session(
            workgraph_dir,
            &alias,
            workgraph::chat_sessions::SessionKind::Interactive,
            Some(format!("interactive {}", alias)),
        );
        alias
    };

    let chat_dir = workgraph_dir.join("chat").join(&session_ref);
    let _ = std::fs::create_dir_all(&chat_dir);
    let journal_path = chat_dir.join("conversation.jsonl");
    let output_log = chat_dir.join("trace.ndjson");

    // Resume semantics: auto-resume if the journal already exists.
    // Explicit `--resume` means "require that resume succeed" — if
    // the journal is missing, warn (but still proceed fresh so the
    // caller isn't wedged).
    let journal_exists = journal_path.exists();
    let resume_enabled = if resume {
        if !journal_exists {
            eprintln!(
                "\x1b[33m[wg nex] --resume: no journal at {} — starting fresh\x1b[0m",
                journal_path.display()
            );
        }
        journal_exists
    } else {
        // Default: auto-resume when possible — the low-friction
        // model the user asked for. No journal = first run of this
        // session_ref, start fresh.
        if journal_exists {
            eprintln!(
                "\x1b[1;33m[wg nex] auto-resuming session {} (journal exists)\x1b[0m",
                session_ref
            );
        }
        journal_exists
    };

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
    let mount_chat_surface = chat_ref.is_some() || chat_id.is_some() || autonomous;
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
    // text, exit hint) are gated.
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

    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;

    let result = rt.block_on(agent.run_interactive(message))?;

    if verbose {
        eprintln!(
            "\n\x1b[2mSession: {} turns, {} input + {} output tokens\x1b[0m",
            result.turns, result.total_usage.input_tokens, result.total_usage.output_tokens,
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

/// Derive a stable per-terminal alias for interactive `wg nex`
/// sessions with no explicit chat-ref. The goal is sticky auto-resume:
/// running `wg nex` in the same terminal twice reattaches to the
/// same journal instead of creating a fresh session each time.
///
/// We key on `$TTY` (or the current controlling terminal via
/// `libc::ttyname`) — one slot per pts. Terminals that don't have a
/// tty (detached invocations, piped stdin) fall back to a timestamp
/// alias, which is effectively a fresh session every time.
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
                    return format!("tty-{}", slug);
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
