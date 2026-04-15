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
) -> Result<()> {
    let config = Config::load_or_default(workgraph_dir);

    let effective_model = model
        .map(String::from)
        .or_else(|| std::env::var("WG_MODEL").ok())
        .unwrap_or_else(|| config.resolve_model_for_role(DispatchRole::TaskAgent).model);

    let working_dir = std::env::current_dir().unwrap_or_default();

    let registry =
        ToolRegistry::default_all_with_config(workgraph_dir, &working_dir, &config.native_executor);

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

    let output_log = workgraph_dir.join("nex-session.ndjson");

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
    );

    if let Some(entry) = config.registry_lookup(&effective_model) {
        agent = agent.with_registry_entry(entry);
    }

    eprintln!(
        "\x1b[1;32mwg nex\x1b[0m — interactive session with \x1b[1m{}\x1b[0m",
        effective_model
    );
    if !supports_tools {
        eprintln!(
            "\x1b[33mWarning: model '{}' may not support tool use\x1b[0m",
            effective_model
        );
    }
    eprintln!("Type /quit or Ctrl-D to exit.\n");

    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;

    let result = rt.block_on(agent.run_interactive(message))?;

    eprintln!(
        "\n\x1b[2mSession: {} turns, {} input + {} output tokens\x1b[0m",
        result.turns, result.total_usage.input_tokens, result.total_usage.output_tokens,
    );

    Ok(())
}
