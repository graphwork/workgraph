//! Native executor CLI entry point.
//!
//! `wg native-exec` runs the Rust-native LLM agent loop for a task.
//! It is called by the spawn wrapper script when the executor type is "native".
//!
//! This command:
//! 1. Reads the prompt from a file
//! 2. Resolves the bundle for the exec_mode (tool filtering)
//! 3. Initializes the appropriate LLM client (Anthropic or OpenAI-compatible)
//! 4. Runs the agent loop to completion
//! 5. Exits with 0 on success, non-zero on failure

use std::path::Path;

use anyhow::{Context, Result};

use workgraph::executor::native::agent::AgentLoop;
use workgraph::executor::native::bundle::resolve_bundle;
use workgraph::executor::native::provider::create_provider_ext;
use workgraph::executor::native::tools::ToolRegistry;
use workgraph::models::ModelRegistry;

const DEFAULT_MODEL: &str = "claude-sonnet-4-5-20250514";

/// Run the native executor agent loop.
#[allow(clippy::too_many_arguments)]
pub fn run(
    workgraph_dir: &Path,
    prompt_file: &str,
    exec_mode: &str,
    task_id: &str,
    model: Option<&str>,
    provider: Option<&str>,
    endpoint_name: Option<&str>,
    endpoint_url: Option<&str>,
    api_key: Option<&str>,
    max_turns: usize,
) -> Result<()> {
    let prompt = std::fs::read_to_string(prompt_file)
        .with_context(|| format!("Failed to read prompt file: {}", prompt_file))?;

    let effective_model = model
        .map(String::from)
        .or_else(|| std::env::var("WG_MODEL").ok())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    // Resolve the working directory (parent of .workgraph/)
    let working_dir = workgraph_dir
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Build the tool registry
    let mut registry = ToolRegistry::default_all(workgraph_dir, &working_dir);

    // Resolve bundle and filter tools
    let system_suffix = if let Some(bundle) = resolve_bundle(exec_mode, workgraph_dir) {
        let suffix = bundle.system_prompt_suffix.clone();
        registry = bundle.filter_registry(registry);
        suffix
    } else {
        String::new()
    };

    // Build full system prompt
    let system_prompt = if system_suffix.is_empty() {
        prompt
    } else {
        format!("{}\n\n{}", prompt, system_suffix)
    };

    // Build output log path
    let output_log = if let Ok(agent_id) = std::env::var("WG_AGENT_ID") {
        workgraph_dir
            .join("agents")
            .join(&agent_id)
            .join("agent.ndjson")
    } else {
        workgraph_dir.join("native-exec.ndjson")
    };

    eprintln!(
        "[native-exec] Starting agent loop for task '{}' with model '{}', exec_mode '{}', max_turns {}",
        task_id, effective_model, exec_mode, max_turns
    );

    // Create the LLM provider (auto-selects by model name).
    // Provider resolution: CLI --provider > WG_LLM_PROVIDER env var > create_provider_ext fallback.
    let effective_provider = provider
        .map(String::from)
        .or_else(|| std::env::var("WG_LLM_PROVIDER").ok());
    let effective_endpoint = endpoint_name
        .map(String::from)
        .or_else(|| std::env::var("WG_ENDPOINT").ok());
    // If endpoint_url was passed explicitly, set WG_ENDPOINT_URL so create_provider_ext picks it up.
    if let Some(url) = endpoint_url {
        // SAFETY: native-exec is single-threaded at this point (before tokio runtime creation).
        unsafe { std::env::set_var("WG_ENDPOINT_URL", url) };
    }
    let client = create_provider_ext(
        workgraph_dir,
        &effective_model,
        effective_provider.as_deref(),
        effective_endpoint.as_deref(),
        api_key,
    )?;

    // Check if the model supports tool use
    let model_registry = ModelRegistry::load(workgraph_dir).unwrap_or_default();
    let supports_tools = model_registry.supports_tool_use(&effective_model);
    if !supports_tools {
        eprintln!(
            "[native-exec] Model '{}' does not support tool use, sending requests without tools",
            effective_model
        );
    }

    // Create and run the agent loop
    let agent = AgentLoop::with_tool_support(
        client,
        registry,
        system_prompt,
        max_turns,
        output_log,
        supports_tools,
    );

    // Run the async agent loop
    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
    let result = rt.block_on(agent.run(&format!(
        "You are working on task '{}'. Complete the task as described in your system prompt. \
         When done, use the wg_done tool with task_id '{}'. \
         If you cannot complete the task, use the wg_fail tool with a reason.",
        task_id, task_id
    )))?;

    eprintln!(
        "[native-exec] Agent completed: {} turns, {}+{} tokens",
        result.turns, result.total_usage.input_tokens, result.total_usage.output_tokens
    );

    Ok(())
}
