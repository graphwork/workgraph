//! Spawn execution — claims a task, assembles prompt, launches executor process,
//! and registers the agent.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use workgraph::config::Config;
use workgraph::graph::{LogEntry, Node, Status, Task, is_system_task};
use workgraph::parser::{load_graph, mutate_graph};
use workgraph::service::executor::{ExecutorRegistry, PromptTemplate, TemplateVars, build_prompt};
use workgraph::service::registry::AgentRegistry;

use super::context::{
    build_scope_context, build_task_context, resolve_task_exec_mode, resolve_task_scope,
};
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
    }

    // Resolve context scope
    let config = Config::load_or_default(dir);
    let scope = resolve_task_scope(task, &config, dir);

    // Build context from dependencies
    let task_context = build_task_context(&graph, task);

    // Build scope context for prompt assembly
    let scope_ctx = build_scope_context(&graph, task, scope, &config, dir);

    // Create template variables
    let mut vars = TemplateVars::from_task(task, Some(&task_context), Some(dir));

    // Get task exec command for shell executor
    let task_exec = task.exec.clone();
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

    // Model resolution hierarchy:
    //   task.model > executor.model > model param (CLI --model or coordinator.model)
    let effective_model = task_model
        .or_else(|| executor_config.executor.model.clone())
        .or_else(|| model.map(std::string::ToString::to_string));

    // Override model in template vars with effective model
    if let Some(ref m) = effective_model {
        vars.model = m.clone();
    }

    // Load agent registry and prepare agent output directory
    let mut agent_registry = AgentRegistry::load(dir)?;

    // We need to know the agent ID before spawning to set up the output directory
    let temp_agent_id = format!("agent-{}", agent_registry.next_agent_id);
    let output_dir = agent_output_dir(dir, &temp_agent_id);
    fs::create_dir_all(&output_dir).with_context(|| {
        format!(
            "Failed to create agent output directory at {:?}",
            output_dir
        )
    })?;

    let output_file = output_dir.join("output.log");
    let output_file_str = output_file.to_string_lossy().to_string();

    // Apply templates to executor settings (with effective model in vars)
    let mut settings = executor_config.apply_templates(&vars);

    // Scope-based prompt assembly for built-in executors.
    // When no custom prompt_template is defined (built-in defaults),
    // use build_prompt() to assemble the prompt based on context scope.
    if settings.prompt_template.is_none()
        && (settings.executor_type == "claude"
            || settings.executor_type == "amplifier"
            || settings.executor_type == "native")
    {
        let prompt = build_prompt(&vars, scope, &scope_ctx);
        settings.prompt_template = Some(PromptTemplate { template: prompt });
    }

    // Use resolved exec_mode (already accounts for role defaults)
    let exec_mode = resolved_exec_mode.as_str();

    // Build the inner command string first
    let inner_command = build_inner_command(
        &settings,
        exec_mode,
        &output_dir,
        &effective_model,
        &vars,
        &task_exec,
        resume_session_id.as_deref(),
    )?;

    // Resolve effective timeout: CLI param > executor config > coordinator config.
    // Empty string means disabled.
    let effective_timeout_secs: Option<u64> = if let Some(t) = timeout {
        if t.is_empty() {
            None
        } else {
            Some(parse_timeout_secs(t).context("Invalid --timeout value")?)
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
    if let Some(ref m) = effective_model {
        cmd.env("WG_MODEL", m);
    }

    // Set working directory if specified
    if let Some(ref wd) = settings.working_dir {
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

    // Atomically re-check task status and claim it under the file lock.
    // This prevents the TOCTOU race where two concurrent spawns both pass
    // the initial status check (above) and then both try to claim the task.
    // mutate_graph() holds flock across load→check→claim→save.
    {
        let agent_id_for_claim = temp_agent_id.clone();
        let spawned_by_owned = spawned_by.to_string();
        let executor_name_owned = executor_name.to_string();
        let effective_model_owned = effective_model.clone();
        let task_title_for_audit_c = task_title_for_audit.clone();
        let task_agent_for_audit_c = task_agent_for_audit.clone();
        let task_id_owned = task_id.to_string();

        mutate_graph(&graph_path, |g| -> Result<()> {
            let t = g.get_task_mut_or_err(&task_id_owned)?;

            // Re-check status under the lock — another spawn may have
            // claimed the task between our initial check and now.
            match t.status {
                Status::Open | Status::Blocked => {}
                Status::InProgress => {
                    anyhow::bail!(
                        "Task '{}' was claimed by another agent (TOCTOU avoided)",
                        task_id_owned
                    );
                }
                other => {
                    anyhow::bail!(
                        "Task '{}' changed to {:?} before claim",
                        task_id_owned,
                        other
                    );
                }
            }
            if t.assigned.is_some() {
                anyhow::bail!(
                    "Task '{}' already assigned (TOCTOU avoided)",
                    task_id_owned
                );
            }

            // Collect deps before re-borrowing mutably
            let deps: Vec<String> = t.after.clone();

            // Re-check all deps are satisfied under the lock.
            // This catches the race where deps are added (via `wg edit --add-after`)
            // between the coordinator's readiness check and the actual spawn.
            for dep_id in &deps {
                let dep_satisfied = g
                    .get_task(dep_id)
                    .map(|d| d.status.is_terminal())
                    .unwrap_or(false);
                if !dep_satisfied {
                    anyhow::bail!(
                        "Task '{}' has unsatisfied dep '{}' (added after readiness check)",
                        task_id_owned,
                        dep_id
                    );
                }
            }

            // Re-acquire mutable reference after immutable borrows are done
            let t = g.get_task_mut_or_err(&task_id_owned)?;
            t.status = Status::InProgress;
            t.started_at = Some(Utc::now().to_rfc3339());
            t.assigned = Some(agent_id_for_claim.clone());
            t.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some(agent_id_for_claim.clone()),
                message: format!(
                    "Spawned by {} --executor {}{}",
                    spawned_by_owned,
                    executor_name_owned,
                    effective_model_owned
                        .as_ref()
                        .map(|m| format!(" --model {}", m))
                        .unwrap_or_default()
                ),
                ..Default::default()
            });

            // Create .assign-* audit trail if missing (defense-in-depth).
            let assign_task_id = format!(".assign-{}", task_id_owned);
            if !is_system_task(&task_id_owned)
                && g.get_task(&assign_task_id).is_none()
            {
                let now = Utc::now().to_rfc3339();
                let audit_desc = if let Some(ref agent_id) = task_agent_for_audit_c {
                    format!(
                        "Direct dispatch: agent={} → '{}'\nNo lightweight assignment flow (auto_assign disabled or skipped)",
                        agent_id, task_id_owned
                    )
                } else {
                    format!(
                        "Direct dispatch: '{}'\nNo agent pre-assigned (auto_assign disabled or skipped)",
                        task_id_owned
                    )
                };
                g.add_node(Node::Task(Task {
                    id: assign_task_id,
                    title: format!("Assign agent for: {}", task_title_for_audit_c),
                    description: Some(audit_desc),
                    status: Status::Done,
                    before: vec![task_id_owned.to_string()],
                    tags: vec!["assignment".to_string(), "agency".to_string()],
                    created_at: Some(now.clone()),
                    started_at: Some(now.clone()),
                    completed_at: Some(now),
                    exec_mode: Some("bare".to_string()),
                    visibility: "internal".to_string(),
                    log: vec![LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: Some("coordinator".to_string()),
                        message: "Created at spawn time (no prior .assign-* task existed)"
                            .to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }));
            }

            Ok(())
        })
        .context("Failed to atomically claim task")?;
    }

    // Spawn the process (don't wait). If spawn fails, unclaim the task.
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Spawn failed — atomically revert the task claim so it's not stuck
            let rollback_agent = temp_agent_id.clone();
            let rollback_task = task_id.to_string();
            let rollback_err_msg = format!("Spawn failed, reverting claim: {}", e);
            if let Err(rollback_err) =
                mutate_graph(&graph_path, |g| -> Result<()> {
                    if let Some(t) = g.get_task_mut(&rollback_task) {
                        t.status = Status::Open;
                        t.started_at = None;
                        t.assigned = None;
                        t.log.push(LogEntry {
                            timestamp: Utc::now().to_rfc3339(),
                            actor: Some(rollback_agent.clone()),
                            message: rollback_err_msg.clone(),
                            ..Default::default()
                        });
                    }
                    Ok(())
                })
            {
                eprintln!(
                    "Warning: failed to rollback claim for task '{}': {}",
                    task_id, rollback_err
                );
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
    let agent_id = agent_registry.register_agent_with_model(
        pid,
        task_id,
        executor_name,
        &output_file_str,
        effective_model.as_deref(),
    );
    if let Err(save_err) = agent_registry.save(dir) {
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
    let metadata = serde_json::json!({
        "agent_id": agent_id,
        "pid": pid,
        "task_id": task_id,
        "executor": executor_name,
        "model": &effective_model,
        "started_at": Utc::now().to_rfc3339(),
        "timeout_secs": effective_timeout_secs,
    });
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
fn build_inner_command(
    settings: &workgraph::service::executor::ExecutorSettings,
    exec_mode: &str,
    output_dir: &Path,
    effective_model: &Option<String>,
    vars: &TemplateVars,
    task_exec: &Option<String>,
    resume_session_id: Option<&str>,
) -> Result<String> {
    // Always write prompt.txt when a prompt template exists, regardless of executor type.
    // This is useful for debugging and allows any executor to read it.
    let prompt_file = output_dir.join("prompt.txt");
    let has_prompt = if let Some(ref pt) = settings.prompt_template {
        fs::write(&prompt_file, &pt.template)
            .with_context(|| format!("Failed to write prompt file: {:?}", prompt_file))?;
        true
    } else {
        false
    };

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
            cmd_parts.push(shell_escape("Agent"));
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
            // prompt.txt already written at the top of this function.
            let prompt_content = settings
                .prompt_template
                .as_ref()
                .map(|pt| pt.template.clone())
                .unwrap_or_default();

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
            cmd_parts.push(shell_escape("Edit,Write,NotebookEdit,Agent"));

            cmd_parts.push("--disable-slash-commands".to_string());
            // Add model flag if specified
            if let Some(m) = effective_model {
                cmd_parts.push("--model".to_string());
                cmd_parts.push(shell_escape(m));
            }
            let claude_cmd = cmd_parts.join(" ");

            if has_prompt {
                // prompt.txt already written at the top of this function
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
            cmd_parts.push(shell_escape("Agent"));

            cmd_parts.push("--disable-slash-commands".to_string());
            // Add model flag if specified
            if let Some(m) = effective_model {
                cmd_parts.push("--model".to_string());
                cmd_parts.push(shell_escape(m));
            }
            let claude_cmd = cmd_parts.join(" ");

            if has_prompt {
                // prompt.txt already written at the top of this function
                prompt_file_command(&prompt_file.to_string_lossy(), &claude_cmd)
            } else {
                claude_cmd
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

            if has_prompt {
                // prompt.txt already written at the top of this function
                prompt_file_command(&prompt_file.to_string_lossy(), &amplifier_cmd)
            } else {
                amplifier_cmd
            }
        }
        "native" => {
            // Native executor: runs the agent loop in-process via `wg native-exec`.
            // Prompt is written to a file and passed as an argument. The bundle is
            // resolved from exec_mode by the native-exec subcommand.
            // prompt.txt already written at the top of this function.
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
            let cmd = parts.join(" ");

            // Use prompt_mode to decide prompt delivery for custom executors.
            // This decouples stdin piping from executor type.
            let prompt_mode = settings.prompt_mode.as_deref().unwrap_or("none");
            if has_prompt && prompt_mode == "stdin" {
                prompt_file_command(&prompt_file.to_string_lossy(), &cmd)
            } else {
                cmd
            }
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

    let stream_file = output_dir.join("stream.jsonl");
    let stream_file_str = stream_file.to_string_lossy().to_string();

    // For Claude executor: split stdout (JSONL) to raw_stream.jsonl, stderr to output.log.
    // Also tee stdout to output.log for backward compatibility.
    // For native: the agent loop writes stream.jsonl directly; wrapper just adds bookends.
    // For amplifier/shell/other: wrapper emits Init+Result bookend events.
    let (run_command, stream_init, stream_result) = match executor_type {
        "claude" => {
            let raw_stream_file = output_dir.join("raw_stream.jsonl");
            let raw_str = raw_stream_file.to_string_lossy().to_string();
            // Capture Claude's JSONL stdout to raw_stream.jsonl and also copy to output.log.
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

    let env_unset = workgraph::env_sanitize::shell_unset_clause();
    let wrapper_script = format!(
        r#"#!/bin/bash
TASK_ID={escaped_task_id}
OUTPUT_FILE={escaped_output_file}

# Allow nested Claude Code sessions (spawned agents are independent)
{env_unset}{timeout_note}
{stream_init}
# Run the agent command
{run_command}
EXIT_CODE=$?
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

exit $EXIT_CODE
"#,
        escaped_task_id = shell_escape(task_id),
        escaped_output_file = shell_escape(output_file_str),
        run_command = run_command,
        timeout_note = timeout_note,
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
