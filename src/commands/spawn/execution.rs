//! Spawn execution — claims a task, assembles prompt, launches executor process,
//! and registers the agent.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use workgraph::agency;
use workgraph::config::{CapBehavior, Config, EndpointConfig};
use workgraph::graph::{LogEntry, Node, Status, Task, is_system_task};
use workgraph::parser::{load_graph, modify_graph};
use workgraph::service::executor::{ExecutorRegistry, PromptTemplate, TemplateVars, build_prompt};
use workgraph::service::registry::AgentRegistry;

use super::context::{
    build_auto_verify_command, build_previous_attempt_context, build_scope_context,
    build_task_context, discover_test_files, format_test_discovery_context, resolve_task_exec_mode,
    resolve_task_scope,
};
use super::worktree;
use super::{
    SpawnResult, agent_output_dir, graph_path, parse_timeout_secs, prompt_file_command,
    shell_escape,
};

/// Internal shared implementation for spawning an agent.
/// Both `run()` (CLI) and `spawn_agent()` (coordinator) delegate here.
pub(crate) fn spawn_agent_inner(
    dir: &Path,
    task_id: &str,
    executor_name: &str,
    timeout: Option<&str>,
    model: Option<&str>,
    spawned_by: &str,
) -> Result<SpawnResult> {
    let graph_path = graph_path(dir);

    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Load the graph and get task info
    let graph = load_graph(&graph_path).context("Failed to load graph")?;

    let task = graph.get_task_or_err(task_id)?;

    // Capture audit info before mutable borrows
    let task_title_for_audit = task.title.clone();
    let task_agent_for_audit = task.agent.clone();

    // Look up agency agent preferences if task has an assigned agent identity.
    // These are used later in model/provider resolution.
    let (agent_preferred_model, agent_preferred_provider) =
        if let Some(ref agent_hash) = task_agent_for_audit {
            let agents_dir = dir.join("agency/cache/agents");
            match agency::find_agent_by_prefix(&agents_dir, agent_hash) {
                Ok(agent) => (
                    agent.preferred_model.clone(),
                    agent.preferred_provider.clone(),
                ),
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };

    // Only allow spawning on tasks that are Open or Blocked
    match task.status {
        Status::Open | Status::Blocked => {}
        Status::InProgress => {
            let since = task
                .started_at
                .as_ref()
                .map(|t| format!(" (since {})", t))
                .unwrap_or_default();
            match &task.assigned {
                Some(assigned) => {
                    anyhow::bail!(
                        "Task '{}' is already claimed by @{}{}",
                        task_id,
                        assigned,
                        since
                    );
                }
                None => {
                    anyhow::bail!("Task '{}' is already in progress{}", task_id, since);
                }
            }
        }
        Status::Done => {
            anyhow::bail!("Task '{}' is already done", task_id);
        }
        Status::Failed => {
            anyhow::bail!(
                "Cannot spawn on task '{}': task is Failed. Use 'wg retry' first.",
                task_id
            );
        }
        Status::Abandoned => {
            anyhow::bail!("Cannot spawn on task '{}': task is Abandoned", task_id);
        }
        Status::Waiting => {
            anyhow::bail!("Cannot spawn on task '{}': task is Waiting", task_id);
        }
        Status::PendingValidation => {
            anyhow::bail!(
                "Cannot spawn on task '{}': task is pending validation",
                task_id
            );
        }
    }

    // Resolve context scope
    let config = Config::load_or_default(dir);

    // Check OpenRouter cost caps before proceeding with expensive operations
    if let Some(provider) = model.and_then(|m| workgraph::config::parse_model_spec(m).provider)
        && provider == "openrouter"
    {
        check_openrouter_cost_caps(&config, dir, task_id, model)?;
    }

    let scope = resolve_task_scope(task, &config, dir);

    // Build context from dependencies
    let task_context = build_task_context(&graph, task);

    // Build scope context for prompt assembly
    let mut scope_ctx = build_scope_context(&graph, task, scope, &config, dir);

    // Inject previous attempt context on retry
    if task.retry_count > 0 {
        let max_tokens = config.checkpoint.retry_context_tokens;
        scope_ctx.previous_attempt_context = build_previous_attempt_context(task, dir, max_tokens);
    }

    // Create template variables
    let mut vars = TemplateVars::from_task(task, Some(&task_context), Some(dir));

    // Detect failed dependencies for triage mode
    let mut failed_deps_lines = Vec::new();
    for dep_id in &task.after {
        if let Some(dep_task) = graph.get_task(dep_id)
            && dep_task.status == Status::Failed
        {
            let reason = dep_task.failure_reason.as_deref().unwrap_or("unknown");
            failed_deps_lines.push(format!(
                "- {}: \"{}\" — Reason: {}",
                dep_id, dep_task.title, reason
            ));
        }
    }
    if !failed_deps_lines.is_empty() {
        vars.has_failed_deps = true;
        vars.failed_deps_info = failed_deps_lines.join("\n");
    }

    // Pre-task test discovery: scan for test files and inject into agent context.
    // Also auto-populate --verify gate when no explicit verify is set.
    let auto_verify_command: Option<String> = if config.coordinator.auto_test_discovery {
        let project_root = dir
            .canonicalize()
            .ok()
            .and_then(|abs| abs.parent().map(|p| p.to_path_buf()));
        if let Some(ref root) = project_root {
            let test_files = discover_test_files(root);
            if !test_files.is_empty() {
                eprintln!(
                    "[spawn] Test discovery: found {} test file(s) for task '{}'",
                    test_files.len(),
                    task_id
                );
                // Inject discovered tests into scope context for prompt
                scope_ctx.discovered_tests = format_test_discovery_context(&test_files);
                // Auto-set verify if task has no explicit --verify gate
                if vars.task_verify.is_none() {
                    if let Some(cmd) = build_auto_verify_command(&test_files) {
                        eprintln!(
                            "[spawn] Auto-verify: setting verify gate for '{}': {}",
                            task_id, cmd
                        );
                        vars.task_verify = Some(cmd.clone());
                        Some(cmd)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Get task exec command for shell executor
    let task_exec = task.exec.clone();
    // Get per-task timeout override
    let task_timeout = task.timeout.clone();
    // Get task model preference
    let task_model = task.model.clone();
    // Get session_id for resume (from previous wg wait)
    let resume_session_id = task.session_id.clone();
    // Resolve exec_mode: task.exec_mode > role.default_exec_mode > "full"
    let resolved_exec_mode = resolve_task_exec_mode(task, dir);
    // Load executor config using the registry
    let executor_registry = ExecutorRegistry::new(dir);
    let executor_config = executor_registry.load_config(executor_name)?;

    // For shell executor, we need an exec command
    if executor_config.executor.executor_type == "shell" && task_exec.is_none() {
        anyhow::bail!("Task '{}' has no exec command for shell executor", task_id);
    }

    // --- Unified model + provider resolution ---
    // Resolves model and provider in a single pass through the precedence hierarchy.
    // At each tier, if the model uses `provider:model` format, the provider is
    // extracted automatically via parse_model_spec().
    let task_provider = graph.get_task(task_id).and_then(|t| t.provider.clone());
    let resolved_task_agent =
        config.resolve_model_for_role(workgraph::config::DispatchRole::TaskAgent);
    let resolved = resolve_model_and_provider(
        task_model.clone(),
        task_provider.clone(),
        agent_preferred_model,
        agent_preferred_provider.clone(),
        executor_config.executor.model.clone(),
        Some(resolved_task_agent.model.clone()),
        resolved_task_agent.provider.clone(),
        model,
        config.coordinator.provider.clone(),
    );

    // --- Model registry alias resolution ---
    // If the effective model string matches a registry entry, resolve it to the
    // actual API model ID, provider, and endpoint. Built-in tier aliases
    // (haiku/sonnet/opus) are kept as-is for backward compatibility with the
    // Claude CLI, which understands them natively.
    let (effective_model, registry_provider, registry_endpoint) =
        resolve_model_via_registry(resolved.model, task_model.as_ref(), &config, dir)?;

    // --- Pre-flight model validation ---
    // Validate OpenRouter-style models against the cached model list before spawning.
    // This is a warning/suggestion system, not a hard gate.
    let (effective_model, model_validation_warning) = {
        let mut model = effective_model;
        let mut warning: Option<String> = None;
        if let Some(ref m) = model
            && m.contains('/')
            && !BUILTIN_TIER_ALIASES.contains(&m.as_str())
        {
            let validation =
                workgraph::executor::native::openai_client::validate_openrouter_model(m, dir);
            if !validation.was_valid {
                if let Some(ref w) = validation.warning {
                    eprintln!("[spawn] WARNING: {}", w);
                }
                warning = validation.warning;
                model = Some(validation.model);
            } else {
                eprintln!("[spawn] Model '{}' validated against model cache", m);
            }
        }
        (model, warning)
    };

    // Override model in template vars with effective model
    if let Some(ref m) = effective_model {
        vars.model = m.clone();
    }

    // Load agent registry with lock for concurrent safety.
    // The lock is held until save() to prevent two concurrent spawns from
    // reading the same next_agent_id and overwriting each other's registration.
    // Lock hierarchy: graph lock (per-call in load/save_graph) < registry lock (held here).
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // We need to know the agent ID before spawning to set up the output directory
    let temp_agent_id = format!("agent-{}", locked_registry.next_agent_id);
    let output_dir = agent_output_dir(dir, &temp_agent_id);
    fs::create_dir_all(&output_dir).with_context(|| {
        format!(
            "Failed to create agent output directory at {:?}",
            output_dir
        )
    })?;

    let output_file = output_dir.join("output.log");
    let output_file_str = output_file.to_string_lossy().to_string();

    // --- Worktree isolation ---
    // When enabled, each agent gets its own git worktree for file-level isolation.
    // Falls back gracefully if worktree creation fails (e.g., not a git repo).
    let worktree_info = if config.coordinator.worktree_isolation {
        let project_root = dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine project root from {:?}", dir))?;
        match worktree::create_worktree(project_root, dir, &temp_agent_id, task_id) {
            Ok(info) => {
                eprintln!(
                    "[spawn] Created worktree for {} at {:?} (branch: {})",
                    temp_agent_id, info.path, info.branch
                );
                Some(info)
            }
            Err(e) => {
                eprintln!(
                    "[spawn] Worktree creation failed for {}, falling back to shared working directory: {}",
                    temp_agent_id, e
                );
                None
            }
        }
    } else {
        None
    };

    vars.in_worktree = worktree_info.is_some();

    // Apply templates to executor settings (with effective model in vars)
    let mut settings = executor_config.apply_templates(&vars);

    // Universal wg context injection for all executor types.
    // Ensures all executors receive consistent workgraph context in their prompts,
    // with model-appropriate knowledge tier based on context window and capabilities.
    let model_str = settings.model.as_deref().unwrap_or("");
    let model_tier = super::context::classify_model_tier(model_str);
    scope_ctx.wg_guide_content = super::context::build_tiered_guide(dir, model_tier, model_str);

    // Scope-based prompt assembly for built-in executors.
    // When no custom prompt_template is defined (built-in defaults),
    // use build_prompt() to assemble the prompt based on context scope.
    if settings.prompt_template.is_none()
        && (settings.executor_type == "claude"
            || settings.executor_type == "amplifier"
            || settings.executor_type == "native")
    {
        let prompt = build_prompt(&vars, scope, &scope_ctx);

        // Debug logging: capture spawn metadata if WG_DEBUG_PROMPTS is set
        if std::env::var("WG_DEBUG_PROMPTS").is_ok()
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/wg_debug_prompts.log")
        {
            use std::io::Write;
            let debug_info = format!(
                "=== WG DEBUG: Spawning Agent ===\n\
                    Task ID: {}\n\
                    Executor: {}\n\
                    Model: {}\n\
                    Context Scope: {:?}\n\
                    Execution Mode: {}\n\
                    Agent Identity: {}\n\
                    === End of Spawn Metadata ===\n\n",
                task_id,
                executor_name,
                vars.model,
                scope,
                resolved_exec_mode.as_str(),
                task.agent
                    .as_deref()
                    .unwrap_or("Default (no specific agent assigned)")
            );
            let _ = file.write_all(debug_info.as_bytes());
        }

        settings.prompt_template = Some(PromptTemplate { template: prompt });
    }

    // Use resolved exec_mode (already accounts for role defaults)
    let exec_mode = resolved_exec_mode.as_str();

    // Provider is already resolved by resolve_model_and_provider() above.
    // The registry may contribute a provider if the model matched a registry entry;
    // use it only when the tier cascade didn't already produce one.
    let task_endpoint = graph.get_task(task_id).and_then(|t| t.endpoint.clone());
    let effective_provider: Option<String> = resolved.provider.or(registry_provider.clone());

    // Endpoint resolution cascade:
    //   1. task.endpoint — explicit endpoint name on the task
    //   2. registry entry endpoint — from model registry alias
    //   3. task.provider — find matching endpoint by provider
    //   4. registry provider — find matching endpoint by registry provider
    //   5. agent.preferred_provider — find matching endpoint by agent's provider
    //   6. role config endpoint — from [models.task_agent].endpoint
    let effective_endpoint: Option<String> = task_endpoint
        .or(registry_endpoint.clone())
        .or_else(|| {
            task_provider
                .as_ref()
                .and_then(|prov| config.llm_endpoints.find_for_provider(prov))
                .map(|ep| ep.name.clone())
        })
        .or_else(|| {
            registry_provider
                .as_ref()
                .and_then(|prov| config.llm_endpoints.find_for_provider(prov))
                .map(|ep| ep.name.clone())
        })
        .or_else(|| {
            agent_preferred_provider
                .as_ref()
                .and_then(|prov| config.llm_endpoints.find_for_provider(prov))
                .map(|ep| ep.name.clone())
        })
        .or_else(|| resolved_task_agent.endpoint.clone());

    // Resolve endpoint config, URL, and API key from the named endpoint.
    // Falls back to the default endpoint if no specific endpoint was resolved via the cascade.
    let endpoint_config = effective_endpoint
        .as_ref()
        .and_then(|name| config.llm_endpoints.find_by_name(name))
        .or_else(|| config.llm_endpoints.find_default());
    let effective_endpoint_url: Option<String> = endpoint_config.and_then(|ep| ep.url.clone());
    let effective_api_key: Option<String> =
        endpoint_config.and_then(|ep| ep.resolve_api_key(Some(dir)).ok().flatten());

    // Validate endpoint resolution for registry-resolved models.
    // If the model came from the registry with an explicit endpoint that doesn't exist
    // in config, or the endpoint has no valid key, fail early with a clear message.
    if let Some(ref reg_ep) = registry_endpoint {
        if endpoint_config.is_none() {
            anyhow::bail!(
                "Model references endpoint '{}' which is not configured.\n\
                 Add it with: wg endpoint add {} --provider <provider> --url <url>",
                reg_ep,
                reg_ep,
            );
        }
        if effective_api_key.is_none() {
            let ep = endpoint_config.unwrap(); // safe: checked above
            anyhow::bail!(
                "Endpoint '{}' (provider: {}) has no valid API key.\n\
                 Set one with: wg key set {} --value <key>",
                reg_ep,
                ep.provider,
                ep.provider,
            );
        }
    }

    // Build the inner command string first
    let inner_command = build_inner_command(
        &settings,
        exec_mode,
        &output_dir,
        &effective_model,
        &effective_provider,
        &effective_endpoint,
        &effective_endpoint_url,
        &effective_api_key,
        &vars,
        &task_exec,
        resume_session_id.as_deref(),
    )?;

    // Resolve effective timeout: CLI param > task.timeout > executor config > coordinator config.
    // Empty string means disabled.
    let effective_timeout_secs: Option<u64> = if let Some(t) = timeout {
        if t.is_empty() {
            None
        } else {
            Some(parse_timeout_secs(t).context("Invalid --timeout value")?)
        }
    } else if let Some(ref t) = task_timeout {
        if t.is_empty() {
            None
        } else {
            Some(parse_timeout_secs(t).context("Invalid task timeout value")?)
        }
    } else if let Some(t) = settings.timeout {
        if t == 0 { None } else { Some(t) }
    } else {
        let agent_timeout = &config.coordinator.agent_timeout;
        if agent_timeout.is_empty() {
            None
        } else {
            Some(
                parse_timeout_secs(agent_timeout)
                    .context("Invalid coordinator.agent_timeout config")?,
            )
        }
    };

    // Build the actual command line, optionally wrapped with `timeout`
    let timed_command = if let Some(secs) = effective_timeout_secs {
        format!(
            "timeout --signal=TERM --kill-after=30 {} {}",
            secs, inner_command
        )
    } else {
        inner_command.clone()
    };

    // Create and write wrapper script
    let wrapper_path = write_wrapper_script(
        &output_dir,
        task_id,
        &output_file_str,
        &timed_command,
        effective_timeout_secs,
        &settings.executor_type,
    )?;

    // Run the wrapper script
    let mut cmd = Command::new("bash");
    cmd.arg(&wrapper_path);

    // Set environment variables from executor config
    for (key, value) in &settings.env {
        cmd.env(key, value);
    }

    // Add task ID and agent ID to environment
    cmd.env("WG_TASK_ID", task_id);
    cmd.env("WG_AGENT_ID", &temp_agent_id);
    cmd.env("WG_EXECUTOR_TYPE", &settings.executor_type);
    // Time budget: inject timeout and spawn epoch for graceful completion
    if let Some(secs) = effective_timeout_secs {
        cmd.env("WG_TASK_TIMEOUT_SECS", secs.to_string());
    }
    cmd.env(
        "WG_SPAWN_EPOCH",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string(),
    );
    // Propagate user identity to spawned agents
    cmd.env("WG_USER", workgraph::current_user());
    if let Some(ref m) = effective_model {
        cmd.env("WG_MODEL", m);
    }
    if let Some(ref ep) = effective_endpoint {
        cmd.env("WG_ENDPOINT", ep);
        cmd.env("WG_ENDPOINT_NAME", ep);
    }
    if let Some(ref provider) = effective_provider {
        cmd.env("WG_LLM_PROVIDER", provider);
    }
    if let Some(ref url) = effective_endpoint_url {
        cmd.env("WG_ENDPOINT_URL", url);
    }
    if let Some(ref key) = effective_api_key {
        cmd.env("WG_API_KEY", key);
        // Also set the provider-specific env var (e.g. OPENROUTER_API_KEY) so the
        // native executor and any child processes can find the key via standard env vars.
        if let Some(ep) = endpoint_config {
            for var_name in EndpointConfig::env_var_names_for_provider(&ep.provider) {
                cmd.env(var_name, key);
            }
        }
    }

    // Set working directory: worktree overrides settings.working_dir
    if let Some(ref wt) = worktree_info {
        cmd.current_dir(&wt.path);
        cmd.env("WG_WORKTREE_PATH", &wt.path);
        cmd.env("WG_BRANCH", &wt.branch);
        cmd.env("WG_PROJECT_ROOT", &wt.project_root);
        // Signal to Claude Code (and other tools) that this session is already
        // inside a managed worktree — do not create a competing one.
        cmd.env("WG_WORKTREE_ACTIVE", "1");
        // Isolate cargo target directory to prevent file lock contention between agents
        cmd.env("CARGO_TARGET_DIR", wt.path.join("target"));
    } else if let Some(ref wd) = settings.working_dir {
        cmd.current_dir(wd);
    }

    // Wrapper script handles output redirect internally
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // Detach the agent into its own session so it survives daemon restart/crash.
    // setsid() creates a new session and process group, making the agent
    // independent of the daemon's process group.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    // Claim the task BEFORE spawning the process to prevent race conditions
    // where two concurrent spawns both pass the status check.
    // Use modify_graph for atomic claim under flock.
    let spawned_by_clone = spawned_by.to_string();
    let executor_name_clone = executor_name;
    let effective_model_clone = effective_model.clone();
    let task_title_for_audit_clone = task_title_for_audit.clone();
    let task_agent_for_audit_clone = task_agent_for_audit.clone();
    let temp_agent_id_clone = temp_agent_id.clone();
    let task_id_str = task_id.to_string();
    let model_validation_warning_clone = model_validation_warning.clone();
    let auto_verify_clone = auto_verify_command.clone();

    let mut claim_error: Option<anyhow::Error> = None;
    modify_graph(&graph_path, |graph| {
        let task = match graph.get_task_mut(&task_id_str) {
            Some(t) => t,
            None => {
                claim_error = Some(anyhow::anyhow!("Task '{}' not found", task_id_str));
                return false;
            }
        };
        task.status = Status::InProgress;
        task.started_at = Some(Utc::now().to_rfc3339());
        task.assigned = Some(temp_agent_id_clone.clone());
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: Some(temp_agent_id_clone.clone()),
            user: Some(workgraph::current_user()),
            message: format!(
                "Spawned by {} --executor {}{}",
                spawned_by_clone,
                executor_name_clone,
                effective_model_clone
                    .as_ref()
                    .map(|m| format!(" --model {}", m))
                    .unwrap_or_default()
            ),
        });

        // Log pre-flight model validation result
        if let Some(ref warning) = model_validation_warning_clone {
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("spawn".to_string()),
                user: None,
                message: format!("Pre-flight model validation: {}", warning),
            });
        }

        // Persist auto-discovered verify gate to the graph so `wg done` enforces it
        if let Some(ref verify_cmd) = auto_verify_clone
            && task.verify.is_none() {
                task.verify = Some(verify_cmd.clone());
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("spawn".to_string()),
                    user: None,
                    message: format!("Auto-verify: set --verify gate from test discovery: {}", verify_cmd),
                });
            }

        // Create .assign-* audit trail if missing (defense-in-depth).
        let assign_task_id = format!(".assign-{}", task_id_str);
        if !is_system_task(&task_id_str) && graph.get_task(&assign_task_id).is_none() {
            let now = Utc::now().to_rfc3339();
            let audit_desc = if let Some(ref agent_id) = task_agent_for_audit_clone {
                format!(
                    "Direct dispatch: agent={} → '{}'\nNo lightweight assignment flow (auto_assign disabled or skipped)",
                    agent_id, task_id_str
                )
            } else {
                format!(
                    "Direct dispatch: '{}'\nNo agent pre-assigned (auto_assign disabled or skipped)",
                    task_id_str
                )
            };
            graph.add_node(Node::Task(Task {
                id: assign_task_id,
                title: format!("Assign agent for: {}", task_title_for_audit_clone),
                description: Some(audit_desc),
                status: Status::Done,
                before: vec![task_id_str.clone()],
                tags: vec!["assignment".to_string(), "agency".to_string()],
                created_at: Some(now.clone()),
                started_at: Some(now.clone()),
                completed_at: Some(now),
                exec_mode: Some("bare".to_string()),
                visibility: "internal".to_string(),
                log: vec![LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("coordinator".to_string()),
                    user: Some(workgraph::current_user()),
                    message: "Created at spawn time (no prior .assign-* task existed)".to_string(),
                }],
                ..Default::default()
            }));
        }
        true
    })
    .context("Failed to save graph")?;
    if let Some(e) = claim_error {
        return Err(e);
    }

    // Spawn the process (don't wait). If spawn fails, unclaim the task.
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Spawn failed — revert the task claim so it's not stuck
            let task_id_rollback = task_id.to_string();
            let agent_id_rollback = temp_agent_id.clone();
            let err_msg = format!("Spawn failed, reverting claim: {}", e);
            if let Err(rollback_err) = modify_graph(&graph_path, |graph| {
                if let Some(t) = graph.get_task_mut(&task_id_rollback) {
                    t.status = Status::Open;
                    t.started_at = None;
                    t.assigned = None;
                    t.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: Some(agent_id_rollback.clone()),
                        user: Some(workgraph::current_user()),
                        message: err_msg.clone(),
                    });
                    true
                } else {
                    false
                }
            }) {
                eprintln!(
                    "Warning: failed to rollback graph for task '{}': {}",
                    task_id, rollback_err
                );
            }
            // Clean up worktree on spawn failure
            if let Some(ref wt) = worktree_info {
                let _ = worktree::remove_worktree(&wt.project_root, &wt.path, &wt.branch);
            }
            return Err(anyhow::anyhow!(
                "Failed to spawn executor '{}' (command: {}): {}",
                executor_name,
                settings.command,
                e
            ));
        }
    };

    let pid = child.id();

    // Register the agent (with model tracking)
    let agent_id = locked_registry.register_agent_with_model(
        pid,
        task_id,
        executor_name,
        &output_file_str,
        effective_model.as_deref(),
    );
    // save() consumes the LockedRegistry, releasing the lock after write.
    if let Err(save_err) = locked_registry.save() {
        // Registry save failed — kill the orphaned process to prevent invisible agents
        eprintln!(
            "Warning: failed to save agent registry for {} (PID {}), killing process: {}",
            agent_id, pid, save_err
        );
        #[cfg(unix)]
        {
            // SAFETY: sending SIGKILL to a known PID we just spawned
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        return Err(save_err.context("Failed to persist agent registry after spawn"));
    }

    // Advance message cursor for this agent so queued messages aren't re-read.
    // The queued messages were already included in the prompt via ScopeContext.
    if let Ok(all_msgs) = workgraph::messages::list_messages(dir, task_id)
        && let Some(last) = all_msgs.last()
    {
        let _ = workgraph::messages::write_cursor(dir, &agent_id, task_id, last.id);
    }

    // Write metadata
    let metadata_path = output_dir.join("metadata.json");
    let mut metadata = serde_json::json!({
        "agent_id": agent_id,
        "pid": pid,
        "task_id": task_id,
        "executor": executor_name,
        "model": &effective_model,
        "started_at": Utc::now().to_rfc3339(),
        "timeout_secs": effective_timeout_secs,
    });
    if let Some(ref wt) = worktree_info {
        metadata["worktree_path"] = serde_json::json!(wt.path.to_string_lossy());
        metadata["worktree_branch"] = serde_json::json!(&wt.branch);
    }
    fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

    Ok(SpawnResult {
        agent_id,
        pid,
        task_id: task_id.to_string(),
        executor: executor_name.to_string(),
        executor_type: settings.executor_type.clone(),
        output_file: output_file_str,
        model: effective_model,
    })
}

/// Build the inner command string for the executor.
#[allow(clippy::too_many_arguments)]
fn build_inner_command(
    settings: &workgraph::service::executor::ExecutorSettings,
    exec_mode: &str,
    output_dir: &Path,
    effective_model: &Option<String>,
    effective_provider: &Option<String>,
    effective_endpoint: &Option<String>,
    effective_endpoint_url: &Option<String>,
    effective_api_key: &Option<String>,
    vars: &TemplateVars,
    task_exec: &Option<String>,
    resume_session_id: Option<&str>,
) -> Result<String> {
    let inner_command = match settings.executor_type.as_str() {
        "claude" if resume_session_id.is_some() && exec_mode != "bare" => {
            // Resume mode: use --resume <session_id> with checkpoint as follow-up message
            let session_id = resume_session_id.unwrap();
            let mut cmd_parts = vec![shell_escape(&settings.command)];
            cmd_parts.push("--resume".to_string());
            cmd_parts.push(shell_escape(session_id));
            cmd_parts.push("--print".to_string());
            cmd_parts.push("--verbose".to_string());
            cmd_parts.push("--output-format".to_string());
            cmd_parts.push("stream-json".to_string());
            cmd_parts.push("--dangerously-skip-permissions".to_string());
            cmd_parts.push("--disallowedTools".to_string());
            cmd_parts.push(shell_escape("Agent,EnterWorktree,ExitWorktree"));
            cmd_parts.push("--disable-slash-commands".to_string());
            if let Some(m) = effective_model {
                cmd_parts.push("--model".to_string());
                cmd_parts.push(shell_escape(m));
            }
            let claude_cmd = cmd_parts.join(" ");

            // Write the resume context (checkpoint) as the follow-up message
            let resume_msg = vars.task_context.clone();
            let resume_file = output_dir.join("resume_message.txt");
            fs::write(&resume_file, &resume_msg)
                .with_context(|| format!("Failed to write resume message: {:?}", resume_file))?;
            prompt_file_command(&resume_file.to_string_lossy(), &claude_cmd)
        }
        "claude" if exec_mode == "bare" => {
            // Bare mode: lightweight execution with --system-prompt and no tools.
            // Used for pure-reasoning tasks (synthesis, triage, summarization).
            // The prompt is passed via --system-prompt and stdin provides the task input.
            let prompt_file = output_dir.join("prompt.txt");
            let prompt_content = settings
                .prompt_template
                .as_ref()
                .map(|pt| pt.template.clone())
                .unwrap_or_default();
            fs::write(&prompt_file, &prompt_content)
                .with_context(|| format!("Failed to write prompt file: {:?}", prompt_file))?;

            let mut cmd_parts = vec![shell_escape(&settings.command)];
            cmd_parts.push("--print".to_string());
            cmd_parts.push("--verbose".to_string());
            cmd_parts.push("--output-format".to_string());
            cmd_parts.push("stream-json".to_string());
            cmd_parts.push("--dangerously-skip-permissions".to_string());
            cmd_parts.push("--tools".to_string());
            cmd_parts.push(shell_escape("Bash(wg:*)"));
            cmd_parts.push("--allowedTools".to_string());
            cmd_parts.push(shell_escape("Bash(wg:*)"));
            cmd_parts.push("--disable-slash-commands".to_string());
            cmd_parts.push("--system-prompt".to_string());
            cmd_parts.push(shell_escape(&prompt_content));
            // Add model flag if specified
            if let Some(m) = effective_model {
                cmd_parts.push("--model".to_string());
                cmd_parts.push(shell_escape(m));
            }
            let claude_cmd = cmd_parts.join(" ");

            // In bare mode, pipe the task title+description as the user message
            let user_message = format!(
                "Complete this task:\n\nTitle: {}\n\n{}",
                vars.task_id, vars.task_description
            );
            let user_msg_file = output_dir.join("user_message.txt");
            fs::write(&user_msg_file, &user_message).with_context(|| {
                format!("Failed to write user message file: {:?}", user_msg_file)
            })?;
            prompt_file_command(&user_msg_file.to_string_lossy(), &claude_cmd)
        }
        "claude" if exec_mode == "light" => {
            // Light mode: read-only file access + wg CLI tools.
            // Used for research, code review, exploration, analysis tasks.
            // Standard prompt-via-stdin flow with --allowedTools restriction.
            let mut cmd_parts = vec![shell_escape(&settings.command)];
            cmd_parts.push("--print".to_string());
            cmd_parts.push("--verbose".to_string());
            cmd_parts.push("--output-format".to_string());
            cmd_parts.push("stream-json".to_string());
            cmd_parts.push("--dangerously-skip-permissions".to_string());
            cmd_parts.push("--allowedTools".to_string());
            cmd_parts.push(shell_escape("Bash(wg:*),Read,Glob,Grep,WebFetch,WebSearch"));
            cmd_parts.push("--disallowedTools".to_string());
            cmd_parts.push(shell_escape("Edit,Write,NotebookEdit,Agent,EnterWorktree,ExitWorktree"));

            cmd_parts.push("--disable-slash-commands".to_string());
            // Add model flag if specified
            if let Some(m) = effective_model {
                cmd_parts.push("--model".to_string());
                cmd_parts.push(shell_escape(m));
            }
            let claude_cmd = cmd_parts.join(" ");

            if let Some(ref prompt_template) = settings.prompt_template {
                // Write prompt to file for safe passing
                let prompt_file = output_dir.join("prompt.txt");
                fs::write(&prompt_file, &prompt_template.template)
                    .with_context(|| format!("Failed to write prompt file: {:?}", prompt_file))?;
                prompt_file_command(&prompt_file.to_string_lossy(), &claude_cmd)
            } else {
                claude_cmd
            }
        }
        "claude" => {
            // Full mode: standard Claude Code session with all tools
            // Write prompt to file and pipe to claude - avoids all quoting issues
            let mut cmd_parts = vec![shell_escape(&settings.command)];
            for arg in &settings.args {
                cmd_parts.push(shell_escape(arg));
            }
            // Prevent agents from spawning sub-agents outside workgraph
            cmd_parts.push("--disallowedTools".to_string());
            cmd_parts.push(shell_escape("Agent,EnterWorktree,ExitWorktree"));

            cmd_parts.push("--disable-slash-commands".to_string());
            // Add model flag if specified
            if let Some(m) = effective_model {
                cmd_parts.push("--model".to_string());
                cmd_parts.push(shell_escape(m));
            }
            let claude_cmd = cmd_parts.join(" ");

            if let Some(ref prompt_template) = settings.prompt_template {
                // Write prompt to file for safe passing
                let prompt_file = output_dir.join("prompt.txt");
                fs::write(&prompt_file, &prompt_template.template)
                    .with_context(|| format!("Failed to write prompt file: {:?}", prompt_file))?;
                prompt_file_command(&prompt_file.to_string_lossy(), &claude_cmd)
            } else {
                claude_cmd
            }
        }
        "codex" => {
            // Codex runs non-interactively via `codex exec`, reading the prompt from stdin.
            // We keep this aligned with the Claude single-shot flow: write the assembled
            // prompt to disk, pipe it in, and let the wrapper capture JSONL output.
            let mut cmd_parts = vec![shell_escape(&settings.command)];
            for arg in &settings.args {
                cmd_parts.push(shell_escape(arg));
            }
            if let Some(m) = effective_model {
                cmd_parts.push("--model".to_string());
                cmd_parts.push(shell_escape(m));
            }
            let codex_cmd = cmd_parts.join(" ");

            if let Some(ref prompt_template) = settings.prompt_template {
                let prompt_file = output_dir.join("prompt.txt");
                fs::write(&prompt_file, &prompt_template.template)
                    .with_context(|| format!("Failed to write prompt file: {:?}", prompt_file))?;
                prompt_file_command(&prompt_file.to_string_lossy(), &codex_cmd)
            } else {
                codex_cmd
            }
        }
        "amplifier" => {
            // Write prompt to file and pipe to amplifier - same pattern as claude
            let mut cmd_parts = vec![shell_escape(&settings.command)];
            for arg in &settings.args {
                cmd_parts.push(shell_escape(arg));
            }
            // Add model flag if specified.
            // Model can be "provider:model" (e.g., "provider-openai:minimax/minimax-m2.5")
            // which splits into -p provider -m model, or just "model" which passes -m only.
            // If no model is set, amplifier uses its settings.yaml default.
            if let Some(m) = effective_model {
                if let Some((provider, model)) = m.split_once(':') {
                    cmd_parts.push("-p".to_string());
                    cmd_parts.push(shell_escape(provider));
                    cmd_parts.push("-m".to_string());
                    cmd_parts.push(shell_escape(model));
                } else {
                    cmd_parts.push("-m".to_string());
                    cmd_parts.push(shell_escape(m));
                }
            }
            let amplifier_cmd = cmd_parts.join(" ");

            if let Some(ref prompt_template) = settings.prompt_template {
                let prompt_file = output_dir.join("prompt.txt");
                fs::write(&prompt_file, &prompt_template.template)
                    .with_context(|| format!("Failed to write prompt file: {:?}", prompt_file))?;
                prompt_file_command(&prompt_file.to_string_lossy(), &amplifier_cmd)
            } else {
                amplifier_cmd
            }
        }
        "native" => {
            // Native executor: runs the agent loop in-process via `wg native-exec`.
            // Prompt is written to a file and passed as an argument. The bundle is
            // resolved from exec_mode by the native-exec subcommand.
            let prompt_content = settings
                .prompt_template
                .as_ref()
                .map(|pt| pt.template.clone())
                .unwrap_or_default();
            let prompt_file = output_dir.join("prompt.txt");
            fs::write(&prompt_file, &prompt_content)
                .with_context(|| format!("Failed to write prompt file: {:?}", prompt_file))?;

            let mut cmd_parts = vec![shell_escape(&settings.command)];
            cmd_parts.push("native-exec".to_string());
            cmd_parts.push("--prompt-file".to_string());
            cmd_parts.push(shell_escape(&prompt_file.to_string_lossy()));
            cmd_parts.push("--exec-mode".to_string());
            cmd_parts.push(shell_escape(exec_mode));
            cmd_parts.push("--task-id".to_string());
            cmd_parts.push(shell_escape(&vars.task_id));
            if let Some(m) = effective_model {
                cmd_parts.push("--model".to_string());
                cmd_parts.push(shell_escape(m));
            }
            if let Some(p) = effective_provider {
                cmd_parts.push("--provider".to_string());
                cmd_parts.push(shell_escape(p));
            }
            if let Some(ep) = effective_endpoint {
                cmd_parts.push("--endpoint-name".to_string());
                cmd_parts.push(shell_escape(ep));
            }
            if let Some(url) = effective_endpoint_url {
                cmd_parts.push("--endpoint-url".to_string());
                cmd_parts.push(shell_escape(url));
            }
            if let Some(key) = effective_api_key {
                cmd_parts.push("--api-key".to_string());
                cmd_parts.push(shell_escape(key));
            }
            cmd_parts.join(" ")
        }
        "shell" => {
            format!(
                "{} -c {}",
                shell_escape(&settings.command),
                shell_escape(task_exec.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("shell executor requires task exec command")
                })?)
            )
        }
        _ => {
            let mut parts = vec![shell_escape(&settings.command)];
            for arg in &settings.args {
                parts.push(shell_escape(arg));
            }
            parts.join(" ")
        }
    };
    Ok(inner_command)
}

/// Create and write the wrapper shell script that runs the agent command
/// and handles completion/failure.
fn write_wrapper_script(
    output_dir: &Path,
    task_id: &str,
    output_file_str: &str,
    timed_command: &str,
    effective_timeout_secs: Option<u64>,
    executor_type: &str,
) -> Result<std::path::PathBuf> {
    let complete_cmd = "wg done \"$TASK_ID\" 2>> \"$OUTPUT_FILE\" || echo \"[wrapper] WARNING: 'wg done' failed with exit code $?\" >> \"$OUTPUT_FILE\"".to_string();
    let complete_msg = "[wrapper] Agent exited successfully, marking task done";

    let timeout_note = if let Some(secs) = effective_timeout_secs {
        format!(
            "\n# Hard timeout: {}s (SIGTERM, then SIGKILL after 30s)\n",
            secs
        )
    } else {
        String::new()
    };

    // Pass debug environment variables to spawned subprocesses
    let debug_env_vars = if std::env::var("WG_DEBUG_PROMPTS").is_ok() {
        "export WG_DEBUG_PROMPTS=1\n".to_string()
    } else {
        String::new()
    };

    let stream_file = output_dir.join("stream.jsonl");
    let stream_file_str = stream_file.to_string_lossy().to_string();

    // For Claude executor: split stdout (JSONL) to raw_stream.jsonl, stderr to output.log.
    // Also tee stdout to output.log for backward compatibility.
    // For native: the agent loop writes stream.jsonl directly; wrapper just adds bookends.
    // For amplifier/shell/other: wrapper emits Init+Result bookend events.
    let (run_command, stream_init, stream_result) = match executor_type {
        "claude" | "codex" => {
            let raw_stream_file = output_dir.join("raw_stream.jsonl");
            let raw_str = raw_stream_file.to_string_lossy().to_string();
            // Capture JSONL stdout to raw_stream.jsonl and also copy to output.log.
            // stderr goes to output.log only.
            let cmd = format!(
                "{timed_command} > >(tee -a {raw} >> \"$OUTPUT_FILE\") 2>> \"$OUTPUT_FILE\"",
                timed_command = timed_command,
                raw = shell_escape(&raw_str),
            );
            (cmd, String::new(), String::new())
        }
        "native" => {
            // Native executor writes stream.jsonl itself; wrapper just runs the command.
            let cmd = format!(
                "{timed_command} >> \"$OUTPUT_FILE\" 2>&1",
                timed_command = timed_command,
            );
            (cmd, String::new(), String::new())
        }
        _ => {
            // Amplifier, shell, and custom executors: wrapper writes bookend events.
            let cmd = format!(
                "{timed_command} >> \"$OUTPUT_FILE\" 2>&1",
                timed_command = timed_command,
            );
            let ts_cmd = "date +%s%3N"; // milliseconds since epoch
            let init = format!(
                "echo '{{\"type\":\"init\",\"executor_type\":\"{etype}\",\"timestamp_ms\":'$({ts})'}}' >> {sf}",
                etype = executor_type,
                ts = ts_cmd,
                sf = shell_escape(&stream_file_str),
            );
            let result_ok = format!(
                "echo '{{\"type\":\"result\",\"success\":true,\"usage\":{{\"input_tokens\":0,\"output_tokens\":0}},\"timestamp_ms\":'$({ts})'}}' >> {sf}",
                ts = ts_cmd,
                sf = shell_escape(&stream_file_str),
            );
            let result_fail = format!(
                "echo '{{\"type\":\"result\",\"success\":false,\"usage\":{{\"input_tokens\":0,\"output_tokens\":0}},\"timestamp_ms\":'$({ts})'}}' >> {sf}",
                ts = ts_cmd,
                sf = shell_escape(&stream_file_str),
            );
            let result_block = format!(
                "if [ $EXIT_CODE -eq 0 ]; then\n    {result_ok}\nelse\n    {result_fail}\nfi",
                result_ok = result_ok,
                result_fail = result_fail,
            );
            (cmd, init, result_block)
        }
    };

    let wrapper_script = format!(
        r#"#!/bin/bash
TASK_ID={escaped_task_id}
OUTPUT_FILE={escaped_output_file}

# Allow nested Claude Code sessions (spawned agents are independent)
unset CLAUDECODE
unset CLAUDE_CODE_ENTRYPOINT
{timeout_note}
{debug_env_vars}
{stream_init}
# Background heartbeat loop — keeps registry heartbeat fresh while agent runs.
# Without this, agents running longer than heartbeat_timeout get reaped as dead.
(while kill -0 $$ 2>/dev/null; do
    sleep 120
    wg heartbeat "$WG_AGENT_ID" 2>/dev/null || true
done) &
HEARTBEAT_PID=$!

# Run the agent command
{run_command}
EXIT_CODE=$?

# Stop the heartbeat loop
kill $HEARTBEAT_PID 2>/dev/null; wait $HEARTBEAT_PID 2>/dev/null
{stream_result}

# Check if task is still in progress (agent didn't mark it done/failed)
TASK_STATUS=$(wg show "$TASK_ID" --json 2>/dev/null | grep -o '"status": *"[^"]*"' | head -1 | sed 's/.*"status": *"//;s/"//' || echo "unknown")

if [ "$TASK_STATUS" = "in-progress" ]; then
    if [ $EXIT_CODE -eq 124 ]; then
        echo "" >> "$OUTPUT_FILE"
        echo "[wrapper] Agent killed by hard timeout, marking task failed" >> "$OUTPUT_FILE"
        wg fail "$TASK_ID" --reason "Agent exceeded hard timeout" 2>> "$OUTPUT_FILE" || echo "[wrapper] WARNING: 'wg fail' failed with exit code $?" >> "$OUTPUT_FILE"
    elif [ $EXIT_CODE -eq 0 ]; then
        echo "" >> "$OUTPUT_FILE"
        # Safety net: check for unread messages the agent may have missed
        UNREAD=$(wg msg read "$TASK_ID" --agent "$WG_AGENT_ID" 2>/dev/null)
        if [ -n "$UNREAD" ] && ! echo "$UNREAD" | grep -q "No unread messages"; then
            echo "[wrapper] WARNING: Agent finished with unread messages:" >> "$OUTPUT_FILE"
            echo "$UNREAD" >> "$OUTPUT_FILE"
        fi
        echo "{complete_msg}" >> "$OUTPUT_FILE"
        {complete_cmd}
    else
        echo "" >> "$OUTPUT_FILE"
        echo "[wrapper] Agent exited with code $EXIT_CODE, marking task failed" >> "$OUTPUT_FILE"
        wg fail "$TASK_ID" --reason "Agent exited with code $EXIT_CODE" 2>> "$OUTPUT_FILE" || echo "[wrapper] WARNING: 'wg fail' failed with exit code $?" >> "$OUTPUT_FILE"
    fi
fi

# --- Merge Back (worktree isolation) ---
# When the agent ran in an isolated worktree, merge its changes back to the main
# branch and clean up the worktree. Env vars are set by spawn when worktree
# isolation is enabled; this section is a no-op otherwise.
if [ -n "$WG_WORKTREE_PATH" ] && [ -n "$WG_BRANCH" ] && [ -n "$WG_PROJECT_ROOT" ]; then
    # Worktree integrity check: verify the .git pointer still exists.
    # If the agent escaped to a Claude Code worktree, the wg worktree's .git
    # may have been severed by a competing git worktree add with the same basename.
    if [ ! -e "$WG_WORKTREE_PATH/.git" ]; then
        echo "[wrapper] WARNING: Worktree .git pointer missing at $WG_WORKTREE_PATH — possible worktree escape detected" >> "$OUTPUT_FILE"
    fi

    TASK_STATUS_FINAL=$(wg show "$TASK_ID" --json 2>/dev/null | grep -o '"status": *"[^"]*"' | head -1 | sed 's/.*"status": *"//;s/"//' || echo "unknown")

    if [ "$TASK_STATUS_FINAL" = "done" ] || [ "$TASK_STATUS_FINAL" = "pending-validation" ]; then
        # Check if agent made any commits on its worktree branch
        COMMITS=$(git -C "$WG_PROJECT_ROOT" log --oneline "HEAD..$WG_BRANCH" 2>/dev/null | wc -l | tr -d ' ')
        if [ "$COMMITS" -gt 0 ]; then
            cd "$WG_PROJECT_ROOT"

            # Acquire merge lock (serialize concurrent merges)
            MERGE_LOCK="$WG_PROJECT_ROOT/.wg-worktrees/.merge-lock"
            mkdir -p "$(dirname "$MERGE_LOCK")"
            exec 9>"$MERGE_LOCK"
            flock 9

            git merge --squash "$WG_BRANCH" 2>> "$OUTPUT_FILE"
            MERGE_EXIT=$?

            if [ $MERGE_EXIT -ne 0 ]; then
                git merge --abort 2>/dev/null
                echo "[wrapper] Merge conflict on $WG_BRANCH — marking task failed for retry" >> "$OUTPUT_FILE"
                wg fail "$TASK_ID" --reason "Merge conflict integrating worktree branch $WG_BRANCH" 2>> "$OUTPUT_FILE"
            else
                git commit -m "feat: $TASK_ID ($WG_AGENT_ID)

Squash-merged from worktree branch $WG_BRANCH" 2>> "$OUTPUT_FILE"
                echo "[wrapper] Merged $WG_BRANCH to $(git rev-parse --abbrev-ref HEAD)" >> "$OUTPUT_FILE"
            fi

            # Release merge lock
            flock -u 9
        else
            echo "[wrapper] No commits on $WG_BRANCH, nothing to merge" >> "$OUTPUT_FILE"
        fi
    fi

    # Only clean up the worktree on success; preserve on failure for debugging/retry
    if [ "$TASK_STATUS_FINAL" = "done" ] || [ "$TASK_STATUS_FINAL" = "pending-validation" ]; then
        rm -f "$WG_WORKTREE_PATH/.workgraph" 2>/dev/null
        git -C "$WG_PROJECT_ROOT" worktree remove --force "$WG_WORKTREE_PATH" 2>/dev/null
        git -C "$WG_PROJECT_ROOT" branch -D "$WG_BRANCH" 2>/dev/null
        echo "[wrapper] Cleaned up worktree at $WG_WORKTREE_PATH" >> "$OUTPUT_FILE"
    else
        echo "[wrapper] TASK_STATUS_FINAL is '$TASK_STATUS_FINAL' (failed or non-success) — skip cleanup, preserving worktree at $WG_WORKTREE_PATH for debugging" >> "$OUTPUT_FILE"
    fi
fi

exit $EXIT_CODE
"#,
        escaped_task_id = shell_escape(task_id),
        escaped_output_file = shell_escape(output_file_str),
        run_command = run_command,
        timeout_note = timeout_note,
        debug_env_vars = debug_env_vars,
        stream_init = stream_init,
        stream_result = stream_result,
        complete_cmd = complete_cmd,
        complete_msg = complete_msg,
    );

    // Write wrapper script
    let wrapper_path = output_dir.join("run.sh");
    fs::write(&wrapper_path, &wrapper_script)
        .with_context(|| format!("Failed to write wrapper script: {:?}", wrapper_path))?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755))?;
    }

    Ok(wrapper_path)
}

/// A resolved model+provider pair from the unified resolution cascade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedModelProvider {
    /// The winning model string, potentially still containing a `provider:` prefix.
    /// Downstream `resolve_model_via_registry()` handles prefix stripping.
    pub model: Option<String>,
    /// The resolved provider, extracted from the highest-priority tier that
    /// supplies one — either an explicit provider field or a `provider:model` spec.
    pub provider: Option<String>,
}

/// A single tier in the model/provider resolution cascade.
/// Each tier may supply a model, a provider, or both. If the model string
/// contains a `provider:model` spec, the provider is extracted automatically.
struct ResolutionTier {
    model: Option<String>,
    provider: Option<String>,
}

impl ResolutionTier {
    fn new(model: Option<String>, provider: Option<String>) -> Self {
        // If there's an explicit provider, use it directly.
        // Otherwise, try to extract provider from the model spec.
        if provider.is_some() {
            return Self { model, provider };
        }
        if let Some(ref m) = model {
            let spec = workgraph::config::parse_model_spec(m);
            if let Some(ref p) = spec.provider {
                return Self {
                    model,
                    provider: Some(workgraph::config::provider_to_native_provider(p).to_string()),
                };
            }
        }
        Self { model, provider }
    }
}

/// Resolve model and provider from the unified precedence hierarchy.
///
/// Resolution tiers (highest to lowest priority):
///   1. Task-level (task.model, task.provider)
///   2. Agent preferences (agent.preferred_model, agent.preferred_provider)
///   3. Executor defaults (executor.model)
///   4. Role-based config (role_config.model, role_config.provider)
///   5. Coordinator defaults (coordinator.model, coordinator.provider)
///
/// At each tier, if the model string uses `provider:model` format, the provider
/// is extracted from it via `parse_model_spec()`. An explicit provider at the
/// same tier takes precedence over a provider embedded in the model spec.
///
/// Model and provider are resolved independently: the highest-priority tier
/// that supplies a model wins for model, and likewise for provider. This means
/// a task can set `model = "openrouter:deepseek-v3"` (setting both) or just
/// `provider = "openrouter"` (overriding only the provider).
///
/// The returned model string retains any `provider:` prefix so that downstream
/// `resolve_model_via_registry()` can handle prefix stripping per executor type.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_model_and_provider(
    task_model: Option<String>,
    task_provider: Option<String>,
    agent_preferred_model: Option<String>,
    agent_preferred_provider: Option<String>,
    executor_model: Option<String>,
    role_model: Option<String>,
    role_provider: Option<String>,
    coordinator_model: Option<&str>,
    coordinator_provider: Option<String>,
) -> ResolvedModelProvider {
    let tiers = [
        ResolutionTier::new(task_model, task_provider),
        ResolutionTier::new(agent_preferred_model, agent_preferred_provider),
        ResolutionTier::new(executor_model, None),
        ResolutionTier::new(role_model, role_provider),
        ResolutionTier::new(
            coordinator_model.map(|s| s.to_string()),
            coordinator_provider,
        ),
    ];

    let model = tiers.iter().find_map(|t| t.model.clone());
    let provider = tiers.iter().find_map(|t| t.provider.clone());

    ResolvedModelProvider { model, provider }
}

/// Built-in tier alias IDs that the Claude CLI understands natively.
const BUILTIN_TIER_ALIASES: &[&str] = &["haiku", "sonnet", "opus"];

/// Resolve a model string through the model registry.
///
/// If the model matches a registry entry:
/// - Built-in tier aliases (haiku/sonnet/opus) are kept as-is (Claude CLI understands them)
/// - Custom aliases are resolved to their full API model ID
/// - The entry's provider and endpoint are returned for downstream resolution
///
/// If the model is not in the registry:
/// - If the task explicitly specified it → error (user should register it first)
/// - Otherwise (from executor/coordinator defaults) → pass through unchanged
///
/// Returns `(effective_model, registry_provider, registry_endpoint)`.
fn resolve_model_via_registry(
    effective_model: Option<String>,
    task_model: Option<&String>,
    config: &Config,
    dir: &Path,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let model_str = match effective_model {
        Some(ref s) => s.clone(),
        None => return Ok((None, None, None)),
    };

    // Parse unified provider:model spec. If the model has an explicit provider
    // prefix (e.g. "openrouter:deepseek/deepseek-v3.2"), extract it and use
    // the model ID for registry lookup.
    let spec = workgraph::config::parse_model_spec(&model_str);
    if let Some(ref provider_prefix) = spec.provider {
        let native_provider =
            Some(workgraph::config::provider_to_native_provider(provider_prefix).to_string());
        // Try registry lookup on the bare model part for endpoint resolution
        let merged = Config::load_merged(dir).unwrap_or_else(|_| config.clone());
        let endpoint = merged
            .registry_lookup(&spec.model_id)
            .or_else(|| {
                merged
                    .effective_registry()
                    .into_iter()
                    .find(|e| e.model == spec.model_id)
            })
            .and_then(|e| e.endpoint.clone());
        // CLI-backed executors do not understand provider:model format; pass only
        // the bare model ID. Native/API-backed executors preserve the full spec
        // so downstream provider resolution can re-parse the prefix.
        let effective = if matches!(
            workgraph::config::provider_to_executor(provider_prefix),
            "claude" | "codex"
        ) {
            spec.model_id.clone()
        } else {
            model_str.clone()
        };
        return Ok((Some(effective), native_provider, endpoint));
    }

    // No provider prefix — fall back to existing resolution logic.
    // Load merged config for registry lookup (includes global + local + builtins)
    let merged = Config::load_merged(dir).unwrap_or_else(|_| config.clone());

    // Look up by short ID first, then by full model field (e.g., "deepseek/deepseek-chat"
    // matching a registry entry with model = "deepseek/deepseek-chat").
    let registry_entry = merged.registry_lookup(&model_str).or_else(|| {
        merged
            .effective_registry()
            .into_iter()
            .find(|e| e.model == model_str)
    });

    if let Some(entry) = registry_entry {
        // Found in registry
        let is_builtin = BUILTIN_TIER_ALIASES.contains(&model_str.as_str());
        let resolved_model = if is_builtin {
            // Keep tier alias as-is for backward compat with Claude CLI
            model_str
        } else {
            // Custom alias → use actual API model ID
            entry.model.clone()
        };
        Ok((
            Some(resolved_model),
            Some(entry.provider.clone()),
            entry.endpoint.clone(),
        ))
    } else if task_model.is_some() && task_model.map(|s| s.as_str()) == effective_model.as_deref() {
        // Task explicitly specified a model that's not in the registry.
        if model_str.contains('/') {
            // Full provider/model ID (e.g., "deepseek/deepseek-chat") — pass through.
            // The native executor's create_provider_ext() auto-detects the provider
            // from the slash in the model name.
            Ok((effective_model, None, None))
        } else {
            // Short alias that's not registered — try resolving against model cache.
            let resolution = workgraph::executor::native::openai_client::resolve_short_model_name(
                &model_str, dir,
            );
            if let Some(resolved_id) = resolution.resolved {
                eprintln!(
                    "[spawn] Resolved short model name '{}' → 'openrouter:{}'",
                    model_str, resolved_id
                );
                // Re-resolve with the full provider:model format
                let full_spec = format!("openrouter:{}", resolved_id);
                let spec = workgraph::config::parse_model_spec(&full_spec);
                let native_provider = Some(
                    workgraph::config::provider_to_native_provider(
                        spec.provider.as_deref().unwrap_or("openrouter"),
                    )
                    .to_string(),
                );
                let merged = Config::load_merged(dir).unwrap_or_else(|_| config.clone());
                let endpoint = merged
                    .registry_lookup(&spec.model_id)
                    .or_else(|| {
                        merged
                            .effective_registry()
                            .into_iter()
                            .find(|e| e.model == spec.model_id)
                    })
                    .and_then(|e| e.endpoint.clone());
                Ok((Some(full_spec), native_provider, endpoint))
            } else {
                // No resolution possible — error with suggestions.
                let suggestions = if resolution.suggestions.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n  Did you mean one of:\n{}",
                        resolution
                            .suggestions
                            .iter()
                            .map(|s| format!("    - openrouter:{}", s))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                anyhow::bail!(
                    "Model '{}' not found in config or model cache.{}\n  \
                     Try: `wg models search {}` to find valid alternatives\n  \
                     Or:  `wg models list` to see the local registry\n  \
                     Add: `wg model add {} --provider <provider> --model-id <model-id>` to register it\n  \
                     Tip: `openrouter/auto` is a safe default that auto-routes to the best model.",
                    model_str,
                    suggestions,
                    model_str,
                    model_str,
                );
            }
        }
    } else {
        // Model came from executor/coordinator defaults — pass through unchanged.
        // It may be a direct model ID the executor understands.
        Ok((effective_model, None, None))
    }
}

/// Check OpenRouter cost caps before spawning an agent
fn check_openrouter_cost_caps(
    config: &Config,
    workgraph_dir: &Path,
    task_id: &str,
    model: Option<&str>,
) -> Result<()> {
    use crate::commands::service::CoordinatorState;
    use workgraph::executor::native::openai_client::{
        fetch_openrouter_key_status_blocking, resolve_openai_api_key_from_dir,
    };

    let openrouter_config = &config.openrouter;

    // Early exit if no cost caps are configured
    if openrouter_config.cost_cap_global_usd.is_none()
        && openrouter_config.cost_cap_session_usd.is_none()
        && openrouter_config.cost_cap_task_usd.is_none()
    {
        return Ok(());
    }

    // Get OpenRouter API key for status checking
    let api_key = match resolve_openai_api_key_from_dir(workgraph_dir) {
        Ok(key) => key,
        Err(_) => {
            // If no API key available, we can't check costs, so allow the operation
            return Ok(());
        }
    };

    let service_dir = workgraph_dir.join(".workgraph/service");

    // Load current coordinator state for session cost tracking
    let mut coordinator_state = CoordinatorState::load_for(&service_dir, 0).unwrap_or_default();

    // Check if we should refresh key status
    if coordinator_state
        .cost_tracking
        .should_check_key_status(openrouter_config.key_status_check_interval_minutes)
    {
        match fetch_openrouter_key_status_blocking(&api_key, None) {
            Ok(key_status) => {
                coordinator_state
                    .cost_tracking
                    .update_key_status(key_status);
                // Save updated state
                coordinator_state.save_for(&service_dir, 0);
            }
            Err(e) => {
                // Log warning but don't block operation
                eprintln!("Warning: Failed to check OpenRouter key status: {}", e);
            }
        }
    }

    // Check session cost cap
    if let Some(session_cap) = openrouter_config.cost_cap_session_usd
        && coordinator_state.cost_tracking.session_cost_usd >= session_cap
    {
        return handle_cost_cap_violation(
            &openrouter_config.cap_behavior,
            &format!(
                "Session cost cap of ${:.2} exceeded (current: ${:.2})",
                session_cap, coordinator_state.cost_tracking.session_cost_usd
            ),
            openrouter_config.fallback_model.as_deref(),
            task_id,
            model,
        );
    }

    // Check global cost cap using key status if available
    if let (Some(global_cap), Some(key_status)) = (
        openrouter_config.cost_cap_global_usd,
        &coordinator_state.cost_tracking.key_status,
    ) && key_status.usage >= global_cap
    {
        return handle_cost_cap_violation(
            &openrouter_config.cap_behavior,
            &format!(
                "Global cost cap of ${:.2} exceeded (current: ${:.2})",
                global_cap, key_status.usage
            ),
            openrouter_config.fallback_model.as_deref(),
            task_id,
            model,
        );
    }

    // Check warning thresholds
    if let Some(key_status) = &coordinator_state.cost_tracking.key_status
        && key_status.is_above_threshold(openrouter_config.warn_at_usage_percent as f64)
    {
        eprintln!(
            "Warning: OpenRouter usage at {:.1}% of limit (${:.2}/${:.2})",
            key_status.usage_percentage(),
            key_status.usage,
            key_status.limit
        );
    }

    Ok(())
}

/// Handle cost cap violation according to the configured behavior
fn handle_cost_cap_violation(
    behavior: &CapBehavior,
    message: &str,
    fallback_model: Option<&str>,
    task_id: &str,
    _current_model: Option<&str>,
) -> Result<()> {
    match behavior {
        CapBehavior::Fail => {
            anyhow::bail!("Cost cap exceeded: {}", message);
        }
        CapBehavior::Fallback => {
            if let Some(fallback) = fallback_model {
                eprintln!(
                    "Warning: {} - would fallback to model '{}' for task '{}' (fallback not implemented yet)",
                    message, fallback, task_id
                );
                Ok(())
            } else {
                anyhow::bail!(
                    "Cost cap exceeded: {} (no fallback model configured)",
                    message
                );
            }
        }
        CapBehavior::Escalate => {
            eprintln!("Warning: {} - continuing due to escalate behavior", message);
            Ok(())
        }
        CapBehavior::Readonly => {
            eprintln!("Warning: {} - entering read-only mode", message);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::config::CLAUDE_OPUS_MODEL_ID;

    // --- resolve_model_and_provider tests ---

    /// Helper to call resolve_model_and_provider with all None defaults except specified args.
    fn resolve(
        task_model: Option<&str>,
        task_provider: Option<&str>,
        agent_model: Option<&str>,
        agent_provider: Option<&str>,
        executor_model: Option<&str>,
        role_model: Option<&str>,
        role_provider: Option<&str>,
        coordinator_model: Option<&str>,
        coordinator_provider: Option<&str>,
    ) -> ResolvedModelProvider {
        resolve_model_and_provider(
            task_model.map(|s| s.to_string()),
            task_provider.map(|s| s.to_string()),
            agent_model.map(|s| s.to_string()),
            agent_provider.map(|s| s.to_string()),
            executor_model.map(|s| s.to_string()),
            role_model.map(|s| s.to_string()),
            role_provider.map(|s| s.to_string()),
            coordinator_model,
            coordinator_provider.map(|s| s.to_string()),
        )
    }

    #[test]
    fn test_unified_model_task_overrides_agent() {
        let r = resolve(
            Some("task-model"),
            None,
            Some("agent-model"),
            None,
            Some("executor-model"),
            None,
            None,
            Some("coordinator-model"),
            None,
        );
        assert_eq!(r.model, Some("task-model".to_string()));
    }

    #[test]
    fn test_unified_model_agent_when_no_task() {
        let r = resolve(
            None,
            None,
            Some("agent-model"),
            None,
            Some("executor-model"),
            None,
            None,
            Some("coordinator-model"),
            None,
        );
        assert_eq!(r.model, Some("agent-model".to_string()));
    }

    #[test]
    fn test_unified_model_executor_when_no_agent() {
        let r = resolve(
            None,
            None,
            None,
            None,
            Some("executor-model"),
            None,
            None,
            Some("coordinator-model"),
            None,
        );
        assert_eq!(r.model, Some("executor-model".to_string()));
    }

    #[test]
    fn test_unified_model_role_when_no_executor() {
        let r = resolve(
            None,
            None,
            None,
            None,
            None,
            Some("role-model"),
            None,
            Some("coordinator-model"),
            None,
        );
        assert_eq!(r.model, Some("role-model".to_string()));
    }

    #[test]
    fn test_unified_model_coordinator_fallback() {
        let r = resolve(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("coordinator-model"),
            None,
        );
        assert_eq!(r.model, Some("coordinator-model".to_string()));
    }

    #[test]
    fn test_unified_none_when_all_empty() {
        let r = resolve(None, None, None, None, None, None, None, None, None);
        assert_eq!(r.model, None);
        assert_eq!(r.provider, None);
    }

    #[test]
    fn test_unified_provider_task_overrides_agent() {
        let r = resolve(
            None,
            Some("task-provider"),
            None,
            Some("agent-provider"),
            None,
            None,
            Some("config-provider"),
            None,
            None,
        );
        assert_eq!(r.provider, Some("task-provider".to_string()));
    }

    #[test]
    fn test_unified_provider_agent_when_no_task() {
        let r = resolve(
            None,
            None,
            None,
            Some("agent-provider"),
            None,
            None,
            Some("config-provider"),
            None,
            None,
        );
        assert_eq!(r.provider, Some("agent-provider".to_string()));
    }

    #[test]
    fn test_unified_provider_config_fallback() {
        let r = resolve(
            None,
            None,
            None,
            None,
            None,
            None,
            Some("config-provider"),
            None,
            None,
        );
        assert_eq!(r.provider, Some("config-provider".to_string()));
    }

    #[test]
    fn test_unified_provider_none_when_all_empty() {
        let r = resolve(None, None, None, None, None, None, None, None, None);
        assert_eq!(r.provider, None);
    }

    #[test]
    fn test_unified_provider_from_model_spec() {
        // provider:model in task.model extracts provider automatically
        let r = resolve(
            Some("openrouter:deepseek/deepseek-v3"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(r.model, Some("openrouter:deepseek/deepseek-v3".to_string()));
        assert_eq!(r.provider, Some("openrouter".to_string()));
    }

    #[test]
    fn test_unified_explicit_provider_beats_model_spec() {
        // An explicit task_provider takes precedence over provider in model spec
        let r = resolve(
            Some("openrouter:deepseek/deepseek-v3"),
            Some("anthropic"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(r.provider, Some("anthropic".to_string()));
    }

    #[test]
    fn test_unified_model_spec_at_agent_tier() {
        // provider:model at agent tier extracts provider when no task-level provider
        let r = resolve(
            None,
            None,
            Some("openai:gpt-5"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(r.model, Some("openai:gpt-5".to_string()));
        assert_eq!(r.provider, Some("openai".to_string()));
    }

    #[test]
    fn test_unified_model_spec_at_coordinator_tier() {
        // provider:model at coordinator tier as last resort
        let r = resolve(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("claude:opus"),
            None,
        );
        assert_eq!(r.model, Some("claude:opus".to_string()));
        assert_eq!(r.provider, Some("anthropic".to_string()));
    }

    #[test]
    fn test_unified_unassigned_task_uses_executor_model() {
        let r = resolve(
            None,
            None,
            None,
            None,
            Some("executor-default"),
            None,
            None,
            Some("coordinator-fallback"),
            None,
        );
        assert_eq!(r.model, Some("executor-default".to_string()));
    }

    #[test]
    fn test_unified_task_provider_overrides_lower_model_spec() {
        // Task has explicit provider, coordinator has provider:model
        // Task provider should win even though coordinator model has a spec
        let r = resolve(
            None,
            Some("anthropic"),
            None,
            None,
            None,
            None,
            None,
            Some("openrouter:deepseek/deepseek-v3"),
            None,
        );
        assert_eq!(r.provider, Some("anthropic".to_string()));
        assert_eq!(r.model, Some("openrouter:deepseek/deepseek-v3".to_string()));
    }

    /// Helper to build an EndpointsConfig for endpoint resolution tests.
    fn test_endpoints_config() -> workgraph::config::EndpointsConfig {
        workgraph::config::EndpointsConfig {
            endpoints: vec![
                workgraph::config::EndpointConfig {
                    name: "my-openrouter".to_string(),
                    provider: "openrouter".to_string(),
                    url: Some("https://openrouter.ai/api/v1".to_string()),
                    api_key: Some("sk-or-test".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    model: None,
                    is_default: true,
                    context_window: None,
                },
                workgraph::config::EndpointConfig {
                    name: "my-anthropic".to_string(),
                    provider: "anthropic".to_string(),
                    url: Some("https://api.anthropic.com".to_string()),
                    api_key: Some("sk-ant-test".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    model: None,
                    is_default: false,
                    context_window: None,
                },
            ],
        }
    }

    #[test]
    fn test_endpoint_resolution_task_endpoint_takes_priority() {
        let endpoints = test_endpoints_config();

        // task.endpoint is set — should win over everything
        let task_endpoint = Some("my-openrouter".to_string());
        let task_provider: Option<String> = Some("anthropic".to_string());
        let agent_provider: Option<String> = Some("anthropic".to_string());
        let role_endpoint: Option<String> = Some("my-anthropic".to_string());

        let effective = task_endpoint
            .or_else(|| {
                task_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or_else(|| {
                agent_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or(role_endpoint);

        assert_eq!(effective, Some("my-openrouter".to_string()));
    }

    #[test]
    fn test_endpoint_resolution_task_provider_lookup() {
        let endpoints = test_endpoints_config();

        // No task.endpoint, but task.provider → find matching endpoint
        let task_endpoint: Option<String> = None;
        let task_provider = Some("openrouter".to_string());
        let agent_provider: Option<String> = None;
        let role_endpoint: Option<String> = None;

        let effective = task_endpoint
            .or_else(|| {
                task_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or_else(|| {
                agent_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or(role_endpoint);

        assert_eq!(effective, Some("my-openrouter".to_string()));
    }

    #[test]
    fn test_endpoint_resolution_agent_provider_fallback() {
        let endpoints = test_endpoints_config();

        // No task.endpoint or task.provider, agent.preferred_provider finds endpoint
        let task_endpoint: Option<String> = None;
        let task_provider: Option<String> = None;
        let agent_provider = Some("anthropic".to_string());
        let role_endpoint: Option<String> = None;

        let effective = task_endpoint
            .or_else(|| {
                task_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or_else(|| {
                agent_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or(role_endpoint);

        assert_eq!(effective, Some("my-anthropic".to_string()));
    }

    #[test]
    fn test_endpoint_resolution_role_config_fallback() {
        let endpoints = test_endpoints_config();

        // Nothing else set, role config endpoint is used
        let task_endpoint: Option<String> = None;
        let task_provider: Option<String> = None;
        let agent_provider: Option<String> = None;
        let role_endpoint = Some("my-anthropic".to_string());

        let effective = task_endpoint
            .or_else(|| {
                task_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or_else(|| {
                agent_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or(role_endpoint);

        assert_eq!(effective, Some("my-anthropic".to_string()));
    }

    #[test]
    fn test_endpoint_resolution_none_when_all_empty() {
        let endpoints = test_endpoints_config();

        let task_endpoint: Option<String> = None;
        let task_provider: Option<String> = None;
        let agent_provider: Option<String> = None;
        let role_endpoint: Option<String> = None;

        let effective = task_endpoint
            .or_else(|| {
                task_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or_else(|| {
                agent_provider
                    .as_ref()
                    .and_then(|prov| endpoints.find_for_provider(prov))
                    .map(|ep| ep.name.clone())
            })
            .or(role_endpoint);

        assert_eq!(effective, None);
    }

    #[test]
    fn test_endpoint_api_key_resolved_from_config() {
        let endpoints = test_endpoints_config();
        let ep = endpoints.find_by_name("my-openrouter").unwrap();
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key, Some("sk-or-test".to_string()));
    }

    // --- resolve_model_via_registry tests ---

    fn setup_registry_dir() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();

        // Create a config with a custom model registry entry
        let mut config = Config::default();
        config.model_registry = vec![workgraph::config::ModelRegistryEntry {
            id: "my-custom".to_string(),
            provider: "openrouter".to_string(),
            model: "anthropic/claude-3.5-sonnet".to_string(),
            tier: workgraph::config::Tier::Standard,
            endpoint: Some("my-openrouter".to_string()),
            ..Default::default()
        }];
        config.save(dir).unwrap();
        tmp
    }

    #[test]
    fn test_registry_resolves_custom_alias_to_model_id() {
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        let (model, provider, endpoint) = resolve_model_via_registry(
            Some("my-custom".to_string()),
            Some(&"my-custom".to_string()),
            &config,
            dir,
        )
        .unwrap();

        assert_eq!(
            model,
            Some("anthropic/claude-3.5-sonnet".to_string()),
            "Custom alias should resolve to actual model ID"
        );
        assert_eq!(
            provider,
            Some("openrouter".to_string()),
            "Provider should come from registry entry"
        );
        assert_eq!(
            endpoint,
            Some("my-openrouter".to_string()),
            "Endpoint should come from registry entry"
        );
    }

    #[test]
    fn test_registry_keeps_builtin_alias_unchanged() {
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        for alias in &["haiku", "sonnet", "opus"] {
            let (model, provider, _endpoint) = resolve_model_via_registry(
                Some(alias.to_string()),
                Some(&alias.to_string()),
                &config,
                dir,
            )
            .unwrap();

            assert_eq!(
                model.as_deref(),
                Some(*alias),
                "Built-in alias '{}' should be kept as-is",
                alias
            );
            assert_eq!(
                provider,
                Some("anthropic".to_string()),
                "Built-in alias '{}' should resolve to anthropic provider",
                alias
            );
        }
    }

    #[test]
    fn test_registry_errors_on_unknown_task_model() {
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        let result = resolve_model_via_registry(
            Some("nonexistent-model".to_string()),
            Some(&"nonexistent-model".to_string()),
            &config,
            dir,
        );

        assert!(
            result.is_err(),
            "Should error when task model is not in registry"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not found in config"),
            "Error should mention 'not found in config': {}",
            err
        );
        assert!(
            err.contains("wg model add"),
            "Error should suggest how to register: {}",
            err
        );
    }

    #[test]
    fn test_registry_passes_through_non_task_model() {
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        // Model came from executor/coordinator, not from task — should pass through.
        // CLAUDE_OPUS_MODEL_ID matches the builtin "opus" entry's model field, so
        // it resolves with provider info from that entry.
        let (model, provider, _endpoint) = resolve_model_via_registry(
            Some(CLAUDE_OPUS_MODEL_ID.to_string()),
            None, // no task model
            &config,
            dir,
        )
        .unwrap();

        assert_eq!(
            model,
            Some(CLAUDE_OPUS_MODEL_ID.to_string()),
            "Non-task model should resolve to the same model ID"
        );
        assert_eq!(
            provider,
            Some("anthropic".to_string()),
            "Should find provider from builtin registry entry"
        );
    }

    #[test]
    fn test_registry_truly_unknown_non_task_model_passes_through() {
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        // A model not in the registry at all, from executor/coordinator
        let (model, provider, endpoint) = resolve_model_via_registry(
            Some("totally-unknown-model".to_string()),
            None, // no task model
            &config,
            dir,
        )
        .unwrap();

        assert_eq!(
            model,
            Some("totally-unknown-model".to_string()),
            "Unknown non-task model should pass through unchanged"
        );
        assert_eq!(
            provider, None,
            "No registry provider for truly unknown model"
        );
        assert_eq!(
            endpoint, None,
            "No registry endpoint for truly unknown model"
        );
    }

    #[test]
    fn test_registry_none_model_returns_none() {
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        let (model, provider, endpoint) =
            resolve_model_via_registry(None, None, &config, dir).unwrap();

        assert_eq!(model, None);
        assert_eq!(provider, None);
        assert_eq!(endpoint, None);
    }

    #[test]
    fn test_registry_non_task_model_matching_alias_still_resolves() {
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        // Model came from executor config but happens to match a registry entry
        let (model, provider, endpoint) = resolve_model_via_registry(
            Some("my-custom".to_string()),
            None, // not from task
            &config,
            dir,
        )
        .unwrap();

        assert_eq!(
            model,
            Some("anthropic/claude-3.5-sonnet".to_string()),
            "Should still resolve even if not from task"
        );
        assert_eq!(provider, Some("openrouter".to_string()));
        assert_eq!(endpoint, Some("my-openrouter".to_string()));
    }

    #[test]
    fn test_registry_strips_codex_prefix_for_codex_executor_models() {
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        let (model, provider, endpoint) =
            resolve_model_via_registry(Some("codex:gpt-5-codex".to_string()), None, &config, dir)
                .unwrap();

        assert_eq!(model, Some("gpt-5-codex".to_string()));
        assert_eq!(provider, Some("openai".to_string()));
        assert_eq!(endpoint, None);
    }

    #[test]
    fn test_registry_full_model_id_passthrough_for_task() {
        // Full model IDs with "/" should pass through even when task-specified,
        // allowing OpenRouter-style "provider/model" to work without registration.
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        let full_model = "deepseek/deepseek-chat".to_string();
        let (model, provider, endpoint) =
            resolve_model_via_registry(Some(full_model.clone()), Some(&full_model), &config, dir)
                .unwrap();

        assert_eq!(
            model,
            Some("deepseek/deepseek-chat".to_string()),
            "Full model ID with / should pass through unchanged"
        );
        assert_eq!(
            provider, None,
            "No provider from registry — auto-detection will handle it"
        );
        assert_eq!(endpoint, None, "No endpoint from registry");
    }

    #[test]
    fn test_registry_lookup_by_model_field() {
        // If a registry entry has model = "anthropic/claude-3.5-sonnet",
        // using --model "anthropic/claude-3.5-sonnet" should find it.
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        let full_model = "anthropic/claude-3.5-sonnet".to_string();
        let (model, provider, endpoint) =
            resolve_model_via_registry(Some(full_model.clone()), Some(&full_model), &config, dir)
                .unwrap();

        assert_eq!(
            model,
            Some("anthropic/claude-3.5-sonnet".to_string()),
            "Should match registry entry by model field"
        );
        assert_eq!(
            provider,
            Some("openrouter".to_string()),
            "Should get provider from matched entry"
        );
        assert_eq!(
            endpoint,
            Some("my-openrouter".to_string()),
            "Should get endpoint from matched entry"
        );
    }

    #[test]
    fn test_registry_short_alias_still_errors_when_unknown() {
        // Short aliases (no "/") that aren't registered should still error
        let tmp = setup_registry_dir();
        let dir = tmp.path();
        let config = Config::load_or_default(dir);

        let unknown = "some-unknown-alias".to_string();
        let result =
            resolve_model_via_registry(Some(unknown.clone()), Some(&unknown), &config, dir);

        assert!(result.is_err(), "Short unknown aliases should still error");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found in config"),
            "Error should mention registration"
        );
    }

    #[test]
    fn test_build_inner_command_codex_uses_prompt_file_and_model() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let output_dir = temp_dir.path();
        let settings = workgraph::service::executor::ExecutorSettings {
            executor_type: "codex".to_string(),
            command: "codex".to_string(),
            args: vec![
                "exec".to_string(),
                "--json".to_string(),
                "--skip-git-repo-check".to_string(),
            ],
            env: std::collections::HashMap::new(),
            prompt_template: Some(PromptTemplate {
                template: "Investigate task".to_string(),
            }),
            working_dir: Some("/tmp".to_string()),
            timeout: None,
            model: None,
        };
        let vars = TemplateVars {
            task_id: "task-1".to_string(),
            task_title: "Task".to_string(),
            task_description: "Desc".to_string(),
            task_context: "Context".to_string(),
            task_identity: String::new(),
            working_dir: "/tmp".to_string(),
            skills_preamble: String::new(),
            model: String::new(),
            task_loop_info: String::new(),
            task_verify: None,
            max_child_tasks: 0,
            max_task_depth: 0,
            has_failed_deps: false,
            failed_deps_info: String::new(),
            in_worktree: false,
        };

        let command = build_inner_command(
            &settings,
            "full",
            output_dir,
            &Some("gpt-5-codex".to_string()),
            &None,
            &None,
            &None,
            &None,
            &vars,
            &None,
            None,
        )
        .unwrap();

        assert!(
            command.contains("cat "),
            "Expected prompt to be piped from a file: {}",
            command
        );
        assert!(
            command.contains("'codex' 'exec'"),
            "Expected codex exec invocation: {}",
            command
        );
        assert!(
            command.contains("'--json'"),
            "Expected codex JSON mode: {}",
            command
        );
        assert!(
            command.contains("'--skip-git-repo-check'"),
            "Expected codex git check bypass flag: {}",
            command
        );
        assert!(
            command.contains("--model 'gpt-5-codex'"),
            "Expected codex model flag: {}",
            command
        );
        let prompt_file = output_dir.join("prompt.txt");
        assert_eq!(
            std::fs::read_to_string(prompt_file).unwrap(),
            "Investigate task"
        );
    }
}
