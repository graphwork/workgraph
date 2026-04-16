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
            reg.remove_tools(&["wg_done", "wg_add", "wg_fail", "wg_artifact"]);
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

    // Per-session timestamped paths. Every `wg nex` invocation gets:
    //
    // - `.ndjson` — compact event log (tool calls, user inputs) for
    //   the session-trace display and post-hoc analysis.
    //
    // - `.journal.jsonl` — full replayable conversation journal
    //   (Init, every Message with role/content, ToolExecution,
    //   Compaction, End). This is what enables resume, fork, replay,
    //   and forensic analysis. Same format the background task agents
    //   use, so tools that work on agent journals work on nex
    //   journals too.
    let sessions_dir = workgraph_dir.join("nex-sessions");
    let _ = std::fs::create_dir_all(&sessions_dir);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let output_log = sessions_dir.join(format!("{}.ndjson", &stamp));

    // If --resume, find the most recent journal and continue from it.
    // Otherwise, create a fresh journal for this session.
    let (journal_path, resume_enabled) = if resume {
        match find_most_recent_journal(&sessions_dir) {
            Some(path) => {
                eprintln!("\x1b[1;33m[wg nex] resuming from {}\x1b[0m", path.display());
                (path, true)
            }
            None => {
                eprintln!(
                    "\x1b[33m[wg nex] --resume: no previous journal found, starting fresh\x1b[0m"
                );
                (
                    sessions_dir.join(format!("{}.journal.jsonl", &stamp)),
                    false,
                )
            }
        }
    } else {
        (
            sessions_dir.join(format!("{}.journal.jsonl", &stamp)),
            false,
        )
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
    .with_resume(resume_enabled);

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

    Ok(())
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
        if !path.extension().is_some_and(|ext| ext == "yaml") {
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

/// Find the most recent `.journal.jsonl` file in the sessions
/// directory. Used by `--resume` to pick up where the last session
/// left off. Returns None if no journal files exist.
fn find_most_recent_journal(sessions_dir: &Path) -> Option<std::path::PathBuf> {
    let mut journals: Vec<_> = std::fs::read_dir(sessions_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(".journal.jsonl"))
        })
        .collect();

    // Sort by modification time (most recent last), take the last.
    journals.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    journals.last().map(|e| e.path())
}
