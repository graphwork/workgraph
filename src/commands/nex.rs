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
) -> Result<()> {
    let config = Config::load_or_default(workgraph_dir);

    let effective_model = model
        .map(String::from)
        .or_else(|| std::env::var("WG_MODEL").ok())
        .unwrap_or_else(|| config.resolve_model_for_role(DispatchRole::TaskAgent).model);

    let working_dir = std::env::current_dir().unwrap_or_default();

    let registry = {
        let full = ToolRegistry::default_all_with_config(
            workgraph_dir,
            &working_dir,
            &config.native_executor,
        );
        if read_only {
            full.filter_read_only()
        } else {
            full
        }
    };

    let default_system = format!(
        "You are an expert software engineer working in an interactive coding session.\n\
         Working directory: {}\n\n\
         You have tools available: read files, write/edit files, run bash commands, \
         grep/search, and more. Use them freely to help the user.\n\n\
         Be concise. Show code when relevant. Execute commands to verify your work.\n\n\
         IMPORTANT: You are a coordinator agent - your role is to facilitate development tasks \n\
         but you should NOT attempt to mark tasks as 'done' or participate in the workgraph system.\n\
         Your job is to assist developers, not to manage the workgraph lifecycle.",
        working_dir.display()
    );
    let system = system_prompt.unwrap_or(&default_system);

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
    let journal_path = sessions_dir.join(format!("{}.journal.jsonl", &stamp));
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
    .with_working_dir(working_dir.clone());

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
