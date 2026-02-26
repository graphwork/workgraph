//! Spawn command - spawns an agent to work on a task
//!
//! Usage:
//!   wg spawn <task-id> --executor <name> [--timeout <duration>]
//!
//! The spawn command:
//! 1. Claims the task (fails if already claimed)
//! 2. Loads executor config from .workgraph/executors/<name>.toml
//! 3. Starts the executor process with task context
//! 4. Registers the agent in the registry
//! 5. Prints agent info (ID, PID, output file)
//! 6. Returns immediately (doesn't wait for completion)

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use workgraph::config::Config;
use workgraph::context_scope::ContextScope;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::executor::{
    build_prompt, ExecutorRegistry, PromptTemplate, ScopeContext, TemplateVars,
};
use workgraph::service::registry::AgentRegistry;

use super::graph_path;

/// Escape a string for safe use in shell commands (for simple args)
fn shell_escape(s: &str) -> String {
    // Use single quotes and escape any single quotes within
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Generate a command that reads prompt from a file
/// This is more reliable than heredoc when output redirection is involved
fn prompt_file_command(prompt_file: &str, command: &str) -> String {
    format!("cat {} | {}", shell_escape(prompt_file), command)
}

/// Result of spawning an agent
#[derive(Debug, Serialize)]
pub struct SpawnResult {
    pub agent_id: String,
    pub pid: u32,
    pub task_id: String,
    pub executor: String,
    pub executor_type: String,
    pub output_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Parse a timeout duration string like "30m", "1h", "90s" into seconds.
/// Returns the number of seconds as a u64.
fn parse_timeout_secs(timeout_str: &str) -> Result<u64> {
    let timeout_str = timeout_str.trim();
    if timeout_str.is_empty() {
        anyhow::bail!("Empty timeout string");
    }

    let (num_str, unit) = if let Some(s) = timeout_str.strip_suffix('s') {
        (s, "s")
    } else if let Some(s) = timeout_str.strip_suffix('m') {
        (s, "m")
    } else if let Some(s) = timeout_str.strip_suffix('h') {
        (s, "h")
    } else {
        // Default to seconds if no unit
        (timeout_str, "s")
    };

    let num: u64 = num_str.parse().context("Invalid timeout number")?;

    let secs = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        _ => num,
    };

    Ok(secs)
}

/// Parse a timeout duration string like "30m", "1h", "90s"
#[cfg(test)]
fn parse_timeout(timeout_str: &str) -> Result<std::time::Duration> {
    let timeout_str = timeout_str.trim();
    if timeout_str.is_empty() {
        anyhow::bail!("Empty timeout string");
    }

    let (num_str, unit) = if let Some(s) = timeout_str.strip_suffix('s') {
        (s, "s")
    } else if let Some(s) = timeout_str.strip_suffix('m') {
        (s, "m")
    } else if let Some(s) = timeout_str.strip_suffix('h') {
        (s, "h")
    } else {
        // Default to seconds if no unit
        (timeout_str, "s")
    };

    let num: u64 = num_str.parse().context("Invalid timeout number")?;

    let secs = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        _ => num,
    };

    Ok(std::time::Duration::from_secs(secs))
}

/// Get the output directory for an agent
fn agent_output_dir(workgraph_dir: &Path, agent_id: &str) -> PathBuf {
    workgraph_dir.join("agents").join(agent_id)
}

/// Build context string from dependency artifacts and logs.
///
/// When scope >= Task, includes upstream task titles alongside artifacts (R5).
fn build_task_context(graph: &workgraph::WorkGraph, task: &workgraph::graph::Task) -> String {
    let mut context_parts = Vec::new();

    for dep_id in &task.after {
        if let Some(dep_task) = graph.get_task(dep_id) {
            // R5: Include upstream task title alongside artifacts
            if !dep_task.artifacts.is_empty() {
                context_parts.push(format!(
                    "From {} ({}): artifacts: {}",
                    dep_id,
                    dep_task.title,
                    dep_task.artifacts.join(", ")
                ));
            }

            if dep_task.status == Status::Done && !dep_task.log.is_empty() {
                let logs: Vec<&LogEntry> = dep_task.log.iter().rev().take(5).collect();
                for entry in logs.iter().rev() {
                    context_parts.push(format!(
                        "From {} logs: {} {}",
                        dep_id, entry.timestamp, entry.message
                    ));
                }
            }
        }
    }

    // Inject cycle metadata if this task has cycle_config
    if let Some(ref cc) = task.cycle_config {
        context_parts.push(format!(
            "Cycle status: iteration {} of this task (max {})",
            task.loop_iteration, cc.max_iterations
        ));
        if let Some(ref delay) = cc.delay {
            context_parts.push(format!("  cycle delay: {}", delay));
        }
    }

    if context_parts.is_empty() {
        "No context from dependencies".to_string()
    } else {
        context_parts.join("\n")
    }
}

/// Build the ScopeContext for scope-based prompt assembly.
///
/// Gathers R1 (downstream awareness), R4 (tags/skills), project description,
/// graph summaries, and CLAUDE.md content based on the resolved scope.
fn build_scope_context(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
    scope: ContextScope,
    config: &Config,
    workgraph_dir: &Path,
) -> ScopeContext {
    let mut ctx = ScopeContext::default();

    // R1: Downstream awareness (task+ scope)
    if scope >= ContextScope::Task {
        let task_id = &task.id;
        let downstream: Vec<_> = graph
            .tasks()
            .filter(|t| t.after.contains(task_id))
            .collect();
        if !downstream.is_empty() {
            let mut lines =
                vec!["## Downstream Consumers\n\nTasks that depend on your work:".to_string()];
            for dt in &downstream {
                lines.push(format!("- **{}**: \"{}\"", dt.id, dt.title));
            }
            ctx.downstream_info = lines.join("\n");
        }
    }

    // R4: Tags and skills (task+ scope)
    if scope >= ContextScope::Task {
        let mut info_parts = Vec::new();
        if !task.tags.is_empty() {
            info_parts.push(format!("- **Tags:** {}", task.tags.join(", ")));
        }
        if !task.skills.is_empty() {
            info_parts.push(format!("- **Skills:** {}", task.skills.join(", ")));
        }
        if !info_parts.is_empty() {
            ctx.tags_skills_info = info_parts.join("\n");
        }
    }

    // Graph+ scope: project description
    if scope >= ContextScope::Graph {
        if let Some(ref desc) = config.project.description {
            if !desc.is_empty() {
                ctx.project_description = desc.clone();
            }
        }
    }

    // Graph+ scope: 1-hop neighborhood subgraph summary
    if scope >= ContextScope::Graph {
        ctx.graph_summary = build_graph_summary(graph, task, workgraph_dir);
    }

    // Full scope: full graph summary
    if scope >= ContextScope::Full {
        ctx.full_graph_summary = build_full_graph_summary(graph);
    }

    // Full scope: CLAUDE.md content
    if scope >= ContextScope::Full {
        ctx.claude_md_content = read_claude_md(workgraph_dir);
    }

    ctx
}

/// Inline artifact content for graph+ scopes.
///
/// - Files under 500 bytes: inline full content
/// - Larger files: first 3 lines + byte count
/// - Non-existent files: note that file was not found
fn inline_artifact_content(artifacts: &[String], workgraph_dir: &Path) -> String {
    if artifacts.is_empty() {
        return String::new();
    }

    let project_root = workgraph_dir
        .canonicalize()
        .ok()
        .and_then(|abs| abs.parent().map(std::path::Path::to_path_buf));

    let project_root = match project_root {
        Some(r) => r,
        None => return String::new(),
    };

    let mut lines = Vec::new();
    for artifact in artifacts {
        let path = project_root.join(artifact);
        match fs::metadata(&path) {
            Ok(meta) => {
                let size = meta.len();
                if size <= 500 {
                    match fs::read_to_string(&path) {
                        Ok(content) => {
                            lines.push(format!("  {} ({} bytes):\n  ```\n{}\n  ```", artifact, size, content));
                        }
                        Err(_) => {
                            lines.push(format!("  {} ({} bytes, binary)", artifact, size));
                        }
                    }
                } else {
                    // Large file: first 3 lines + byte count
                    match fs::read_to_string(&path) {
                        Ok(content) => {
                            let preview: String = content
                                .lines()
                                .take(3)
                                .collect::<Vec<_>>()
                                .join("\n");
                            lines.push(format!(
                                "  {} ({} bytes):\n  ```\n{}\n  ...\n  ```",
                                artifact, size, preview
                            ));
                        }
                        Err(_) => {
                            lines.push(format!("  {} ({} bytes, binary)", artifact, size));
                        }
                    }
                }
            }
            Err(_) => {
                lines.push(format!("  {} (not found)", artifact));
            }
        }
    }
    lines.join("\n")
}

/// Build a 1-hop neighborhood graph summary for graph+ scopes.
///
/// Includes: status counts, upstream tasks, downstream tasks, and siblings.
/// Neighbor content is wrapped in XML fencing for prompt injection protection.
/// Hard cap at 4000 chars.
fn build_graph_summary(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
    workgraph_dir: &Path,
) -> String {
    let mut parts = Vec::new();

    // Status counts
    let mut open = 0u32;
    let mut in_progress = 0u32;
    let mut done = 0u32;
    let mut failed = 0u32;
    let mut blocked = 0u32;
    let total = graph.tasks().count() as u32;
    for t in graph.tasks() {
        match t.status {
            Status::Open => open += 1,
            Status::InProgress => in_progress += 1,
            Status::Done => done += 1,
            Status::Failed => failed += 1,
            Status::Blocked => blocked += 1,
            Status::Abandoned => {}
        }
    }
    parts.push(format!(
        "## Graph Status\n\n{} tasks \u{2014} {} done, {} in-progress, {} open, {} blocked, {} failed",
        total, done, in_progress, open, blocked, failed
    ));

    // Upstream tasks (direct dependencies) — XML fenced
    if !task.after.is_empty() {
        let mut lines = vec!["### Upstream (dependencies)".to_string()];
        for dep_id in &task.after {
            if let Some(dep) = graph.get_task(dep_id) {
                let desc_preview = dep
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect::<String>();
                let mut entry = format!(
                    "<neighbor-context source=\"{}\">\n- **{}** [{}]: {} \u{2014} {}",
                    dep.id, dep.id, dep.status, dep.title, desc_preview
                );
                // Inline artifact content for neighbors
                let artifact_content = inline_artifact_content(&dep.artifacts, workgraph_dir);
                if !artifact_content.is_empty() {
                    entry.push_str(&format!("\n  Artifacts:\n{}", artifact_content));
                }
                entry.push_str("\n</neighbor-context>");
                lines.push(entry);
            }
        }
        parts.push(lines.join("\n"));
    }

    // Downstream tasks (tasks that depend on this one) — XML fenced
    let task_id = &task.id;
    let downstream: Vec<_> = graph
        .tasks()
        .filter(|t| t.after.contains(task_id))
        .collect();
    if !downstream.is_empty() {
        let mut lines = vec!["### Downstream (dependents)".to_string()];
        for dt in &downstream {
            let desc_preview = dt
                .description
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>();
            let mut entry = format!(
                "<neighbor-context source=\"{}\">\n- **{}** [{}]: {} \u{2014} {}",
                dt.id, dt.id, dt.status, dt.title, desc_preview
            );
            let artifact_content = inline_artifact_content(&dt.artifacts, workgraph_dir);
            if !artifact_content.is_empty() {
                entry.push_str(&format!("\n  Artifacts:\n{}", artifact_content));
            }
            entry.push_str("\n</neighbor-context>");
            lines.push(entry);
        }
        parts.push(lines.join("\n"));
    }

    // Siblings (tasks sharing the same upstream dependencies)
    if !task.after.is_empty() {
        let siblings: Vec<_> = graph
            .tasks()
            .filter(|t| {
                t.id != task.id
                    && !t.after.is_empty()
                    && t.after.iter().any(|dep| task.after.contains(dep))
            })
            .collect();
        if !siblings.is_empty() {
            let mut lines = vec!["### Siblings (share upstream dependencies)".to_string()];
            for sib in siblings.iter().take(10) {
                lines.push(format!(
                    "- **{}** [{}]: {}",
                    sib.id, sib.status, sib.title
                ));
            }
            if siblings.len() > 10 {
                lines.push(format!("- ... and {} more", siblings.len() - 10));
            }
            parts.push(lines.join("\n"));
        }
    }

    let summary = parts.join("\n\n");
    // Hard cap at 4000 chars
    if summary.len() > 4000 {
        let mut truncated = summary[..3950].to_string();
        truncated.push_str("\n\n... (graph summary truncated)");
        truncated
    } else {
        summary
    }
}

/// Build a full graph summary for full scope.
///
/// Lists all tasks with statuses and dependency edges, with 4000-char budget.
fn build_full_graph_summary(graph: &workgraph::WorkGraph) -> String {
    let mut parts = vec!["## Full Graph Summary\n".to_string()];
    let mut budget = 4000i32;
    let mut task_count = 0u32;
    let total = graph.tasks().count();

    for t in graph.tasks() {
        let deps = if t.after.is_empty() {
            String::new()
        } else {
            format!(" (after: {})", t.after.join(", "))
        };
        let line = format!("- **{}** [{}]: {}{}\n", t.id, t.status, t.title, deps);
        budget -= line.len() as i32;
        if budget < 0 {
            let remaining = total - task_count as usize;
            parts.push(format!("... and {} more tasks", remaining));
            break;
        }
        parts.push(line);
        task_count += 1;
    }

    parts.join("")
}

/// Read CLAUDE.md content from the project root (parent of .workgraph/).
fn read_claude_md(workgraph_dir: &Path) -> String {
    let project_root = workgraph_dir
        .canonicalize()
        .ok()
        .and_then(|abs| abs.parent().map(std::path::Path::to_path_buf));

    let project_root = match project_root {
        Some(r) => r,
        None => return String::new(),
    };

    let claude_md_path = project_root.join("CLAUDE.md");
    match std::fs::read_to_string(&claude_md_path) {
        Ok(content) => content,
        Err(_) => String::new(),
    }
}

/// Resolve the context scope for a task using the priority hierarchy:
/// task > role > coordinator config > default ("task").
fn resolve_task_scope(
    task: &workgraph::graph::Task,
    config: &Config,
    workgraph_dir: &Path,
) -> ContextScope {
    // Get role's default_context_scope if task has an agent
    let role_scope = task.agent.as_ref().and_then(|agent_hash| {
        let agency_dir = workgraph_dir.join("agency");
        let agents_dir = agency_dir.join("cache/agents");
        let roles_dir = agency_dir.join("cache/roles");
        let agent = workgraph::agency::find_agent_by_prefix(&agents_dir, agent_hash).ok()?;
        let role = workgraph::agency::find_role_by_prefix(&roles_dir, &agent.role_id).ok()?;
        role.default_context_scope
    });

    workgraph::context_scope::resolve_context_scope(
        task.context_scope.as_deref(),
        role_scope.as_deref(),
        config.coordinator.default_context_scope.as_deref(),
    )
}

/// Internal shared implementation for spawning an agent.
/// Both `run()` (CLI) and `spawn_agent()` (coordinator) delegate here.
fn spawn_agent_inner(
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
    let mut graph = load_graph(&graph_path).context("Failed to load graph")?;

    let task = graph.get_task_or_err(task_id)?;

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
        && (settings.executor_type == "claude" || settings.executor_type == "amplifier")
    {
        let prompt = build_prompt(&vars, scope, &scope_ctx);
        settings.prompt_template = Some(PromptTemplate {
            template: prompt,
        });
    }

    // Build the inner command string first
    let inner_command = match settings.executor_type.as_str() {
        "claude" => {
            // Write prompt to file and pipe to claude - avoids all quoting issues
            let mut cmd_parts = vec![shell_escape(&settings.command)];
            for arg in &settings.args {
                cmd_parts.push(shell_escape(arg));
            }
            // Add model flag if specified
            if let Some(ref m) = effective_model {
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
            if let Some(ref m) = effective_model {
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
            Some(parse_timeout_secs(agent_timeout).context("Invalid coordinator.agent_timeout config")?)
        }
    };

    // Build the actual command line, optionally wrapped with `timeout`
    let timed_command = if let Some(secs) = effective_timeout_secs {
        format!("timeout --signal=TERM --kill-after=30 {} {}", secs, inner_command)
    } else {
        inner_command.clone()
    };

    // Create a wrapper script that runs the command and handles completion
    // This ensures tasks get marked done/failed even if the agent doesn't do it
    let complete_cmd = "wg done \"$TASK_ID\" 2>> \"$OUTPUT_FILE\" || echo \"[wrapper] WARNING: 'wg done' failed with exit code $?\" >> \"$OUTPUT_FILE\"".to_string();
    let complete_msg = "[wrapper] Agent exited successfully, marking task done";

    let timeout_note = if let Some(secs) = effective_timeout_secs {
        format!("\n# Hard timeout: {}s (SIGTERM, then SIGKILL after 30s)\n", secs)
    } else {
        String::new()
    };

    let wrapper_script = format!(
        r#"#!/bin/bash
TASK_ID={escaped_task_id}
OUTPUT_FILE={escaped_output_file}

# Allow nested Claude Code sessions (spawned agents are independent)
unset CLAUDECODE
unset CLAUDE_CODE_ENTRYPOINT
{timeout_note}
# Run the agent command
{timed_command} >> "$OUTPUT_FILE" 2>&1
EXIT_CODE=$?

# Check if task is still in progress (agent didn't mark it done/failed)
TASK_STATUS=$(wg show "$TASK_ID" --json 2>/dev/null | grep -o '"status": *"[^"]*"' | head -1 | sed 's/.*"status": *"//;s/"//' || echo "unknown")

if [ "$TASK_STATUS" = "in-progress" ]; then
    if [ $EXIT_CODE -eq 124 ]; then
        echo "" >> "$OUTPUT_FILE"
        echo "[wrapper] Agent killed by hard timeout, marking task failed" >> "$OUTPUT_FILE"
        wg fail "$TASK_ID" --reason "Agent exceeded hard timeout" 2>> "$OUTPUT_FILE" || echo "[wrapper] WARNING: 'wg fail' failed with exit code $?" >> "$OUTPUT_FILE"
    elif [ $EXIT_CODE -eq 0 ]; then
        echo "" >> "$OUTPUT_FILE"
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
        escaped_output_file = shell_escape(&output_file_str),
        timed_command = timed_command,
        timeout_note = timeout_note,
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

    // Claim the task BEFORE spawning the process to prevent race conditions
    // where two concurrent spawns both pass the status check.
    let task = graph.get_task_mut_or_err(task_id)?;
    task.status = Status::InProgress;
    task.started_at = Some(Utc::now().to_rfc3339());
    task.assigned = Some(temp_agent_id.clone());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(temp_agent_id.clone()),
        message: format!(
            "Spawned by {} --executor {}{}",
            spawned_by,
            executor_name,
            effective_model
                .as_ref()
                .map(|m| format!(" --model {}", m))
                .unwrap_or_default()
        ),
    });

    save_graph(&graph, &graph_path).context("Failed to save graph")?;

    // Spawn the process (don't wait). If spawn fails, unclaim the task.
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Spawn failed — revert the task claim so it's not stuck
            match load_graph(&graph_path) {
                Ok(mut rollback_graph) => {
                    if let Some(t) = rollback_graph.get_task_mut(task_id) {
                        t.status = Status::Open;
                        t.started_at = None;
                        t.assigned = None;
                        t.log.push(LogEntry {
                            timestamp: Utc::now().to_rfc3339(),
                            actor: Some(temp_agent_id.clone()),
                            message: format!("Spawn failed, reverting claim: {}", e),
                        });
                        if let Err(save_err) = save_graph(&rollback_graph, &graph_path) {
                            eprintln!(
                                "Warning: failed to save rollback graph for task '{}': {}",
                                task_id, save_err
                            );
                        }
                    }
                }
                Err(load_err) => {
                    eprintln!(
                        "Warning: failed to load graph for rollback of task '{}': {}",
                        task_id, load_err
                    );
                }
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

/// Run the spawn command (CLI entry point)
pub fn run(
    dir: &Path,
    task_id: &str,
    executor_name: &str,
    timeout: Option<&str>,
    model: Option<&str>,
    json: bool,
) -> Result<()> {
    let result = spawn_agent_inner(dir, task_id, executor_name, timeout, model, "wg spawn")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Spawned {} for task '{}'", result.agent_id, task_id);
        println!("  Executor: {} ({})", executor_name, result.executor_type);
        if let Some(ref m) = result.model {
            println!("  Model: {}", m);
        }
        println!("  PID: {}", result.pid);
        println!("  Output: {}", result.output_file);
    }

    Ok(())
}

/// Spawn an agent and return (agent_id, pid)
/// This is a helper for the service daemon
pub fn spawn_agent(
    dir: &Path,
    task_id: &str,
    executor_name: &str,
    timeout: Option<&str>,
    model: Option<&str>,
) -> Result<(String, u32)> {
    let result = spawn_agent_inner(dir, task_id, executor_name, timeout, model, "coordinator")?;
    Ok((result.agent_id, result.pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_graph(dir: &Path, tasks: Vec<Task>) {
        let path = graph_path(dir);
        fs::create_dir_all(dir).unwrap();
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
    }

    #[test]
    fn test_prompt_file_command() {
        let result = prompt_file_command("/tmp/prompt.txt", "claude --print");
        assert!(result.contains("cat"));
        assert!(result.contains("/tmp/prompt.txt"));
        assert!(result.contains("claude --print"));
    }

    #[test]
    fn test_parse_timeout_seconds() {
        let dur = parse_timeout("30s").unwrap();
        assert_eq!(dur, std::time::Duration::from_secs(30));
    }

    #[test]
    fn test_parse_timeout_minutes() {
        let dur = parse_timeout("5m").unwrap();
        assert_eq!(dur, std::time::Duration::from_secs(300));
    }

    #[test]
    fn test_parse_timeout_hours() {
        let dur = parse_timeout("2h").unwrap();
        assert_eq!(dur, std::time::Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_timeout_no_unit() {
        let dur = parse_timeout("60").unwrap();
        assert_eq!(dur, std::time::Duration::from_secs(60));
    }

    #[test]
    fn test_spawn_task_not_found() {
        let temp_dir = TempDir::new().unwrap();
        setup_graph(temp_dir.path(), vec![]);

        let result = run(temp_dir.path(), "nonexistent", "shell", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_spawn_already_claimed_task() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.status = Status::InProgress;
        task.assigned = Some("other-agent".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        let result = run(temp_dir.path(), "t1", "shell", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already claimed"));
    }

    #[test]
    fn test_spawn_done_task() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.status = Status::Done;
        setup_graph(temp_dir.path(), vec![task]);

        let result = run(temp_dir.path(), "t1", "shell", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already done"));
    }

    #[test]
    fn test_spawn_shell_without_exec_fails() {
        let temp_dir = TempDir::new().unwrap();
        let task = make_task("t1", "Test Task");
        // Task has no exec command
        setup_graph(temp_dir.path(), vec![task]);

        let result = run(temp_dir.path(), "t1", "shell", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no exec command"));
    }

    #[test]
    fn test_spawn_shell_with_exec() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        // This will actually spawn a process
        let result = run(temp_dir.path(), "t1", "shell", None, None, false);
        assert!(result.is_ok());

        // Verify task was claimed
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);

        // Verify agent was registered
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        assert_eq!(registry.agents.len(), 1);
    }

    #[test]
    fn test_spawn_creates_output_directory() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Small wait for the spawned process to create output file
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Check output directory was created
        let agents_dir = temp_dir.path().join("agents");
        assert!(agents_dir.exists());

        // Should have agent-1 directory
        let agent_dir = agents_dir.join("agent-1");
        assert!(agent_dir.exists());

        // Should have output.log and metadata.json
        assert!(agent_dir.join("output.log").exists());
        assert!(agent_dir.join("metadata.json").exists());
    }

    #[test]
    fn test_build_task_context() {
        let mut graph = WorkGraph::new();

        // Create a dependency task with artifacts and logs
        let mut dep_task = make_task("dep-1", "Dependency");
        dep_task.status = Status::Done;
        dep_task.artifacts = vec!["output.txt".to_string(), "data.json".to_string()];
        dep_task.log = vec![
            LogEntry {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                message: "Started work".to_string(),
            },
            LogEntry {
                timestamp: "2026-01-01T00:01:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                message: "Found important result".to_string(),
            },
            LogEntry {
                timestamp: "2026-01-01T00:02:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                message: "Completed successfully".to_string(),
            },
        ];
        graph.add_node(Node::Task(dep_task));

        // Create main task blocked by dependency
        let mut main_task = make_task("main", "Main Task");
        main_task.after = vec!["dep-1".to_string()];
        graph.add_node(Node::Task(main_task.clone()));

        let context = build_task_context(&graph, &main_task);
        assert!(context.contains("dep-1"));
        // R5: Upstream title included
        assert!(context.contains("(Dependency)"));
        assert!(context.contains("output.txt"));
        assert!(context.contains("data.json"));
        // Verify log entries are included
        assert!(context.contains("From dep-1 logs:"));
        assert!(context.contains("Started work"));
        assert!(context.contains("Found important result"));
        assert!(context.contains("Completed successfully"));
    }

    #[test]
    fn test_build_task_context_no_deps() {
        let graph = WorkGraph::new();
        let task = make_task("t1", "Test Task");

        let context = build_task_context(&graph, &task);
        assert_eq!(context, "No context from dependencies");
        assert!(!context.contains("logs:"));
    }

    #[test]
    fn test_wrapper_script_generation_success() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        task.verify = None; // Not verified, should use wg done
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script was created in agents directory
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        assert!(
            wrapper_path.exists(),
            "Wrapper script not found at {:?}",
            wrapper_path
        );

        // Read wrapper script and verify it contains the expected auto-complete logic
        let script = fs::read_to_string(&wrapper_path).unwrap();
        assert!(
            script.contains("TASK_ID='t1'"),
            "Task ID should be shell-escaped with single quotes"
        );
        assert!(script.contains("wg done \"$TASK_ID\""));
        assert!(script.contains("[wrapper] Agent exited successfully, marking task done"));
        assert!(script.contains("wg show \"$TASK_ID\" --json"));
        assert!(script.contains("if [ \"$TASK_STATUS\" = \"in-progress\" ]"));
    }

    #[test]
    fn test_wrapper_script_for_verified_task() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        task.verify = Some("manual".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script was created in agents directory
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        assert!(
            wrapper_path.exists(),
            "Wrapper script not found at {:?}",
            wrapper_path
        );

        // Verified tasks now also use wg done (submit is deprecated)
        let script = fs::read_to_string(&wrapper_path).unwrap();
        assert!(script.contains("wg done \"$TASK_ID\""));
        assert!(script.contains("[wrapper] Agent exited successfully, marking task done"));
    }

    #[test]
    fn test_wrapper_handles_agent_failure() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("exit 1".to_string()); // Will fail
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script was created in agents directory
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        assert!(
            wrapper_path.exists(),
            "Wrapper script not found at {:?}",
            wrapper_path
        );

        // Read wrapper script and verify it handles failure
        let script = fs::read_to_string(&wrapper_path).unwrap();
        assert!(script.contains("wg fail \"$TASK_ID\""));
        assert!(script.contains("[wrapper] Agent exited with code"));
    }

    #[test]
    fn test_wrapper_detects_task_status() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("wg done t1".to_string()); // Agent marks it done
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script detects if task already done by agent
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Should check task status with wg show
        assert!(script.contains("TASK_STATUS=$(wg show \"$TASK_ID\" --json"));

        // Should only auto-complete if still in_progress
        assert!(script.contains("if [ \"$TASK_STATUS\" = \"in-progress\" ]"));
    }

    #[test]
    fn test_wrapper_script_preserves_exit_code() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("exit 42".to_string()); // Specific exit code
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script preserves exit code
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Should capture and preserve EXIT_CODE
        assert!(script.contains("EXIT_CODE=$?"));
        assert!(script.contains("exit $EXIT_CODE"));
    }

    #[test]
    fn test_wrapper_appends_output_to_log() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo 'Agent output'".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script appends to output file
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Should redirect agent output to output file
        assert!(script.contains(">> \"$OUTPUT_FILE\" 2>&1"));

        // Should append status messages
        assert!(script.contains("echo \"\" >> \"$OUTPUT_FILE\""));
        assert!(script.contains("[wrapper]"));
    }

    #[test]
    fn test_wrapper_suppresses_wg_command_errors() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("true".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script suppresses wg command errors
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Should redirect errors and log failures instead of silencing
        assert!(script.contains("2>> \"$OUTPUT_FILE\" || echo \"[wrapper] WARNING:"));
    }

    #[test]
    fn test_build_task_context_no_loop_metadata_for_normal_tasks() {
        let graph = WorkGraph::new();
        let task = make_task("t1", "Normal Task");
        let context = build_task_context(&graph, &task);
        assert!(!context.contains("Loop status"));
    }

    #[test]
    fn test_build_graph_summary_includes_status_counts() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Done task");
        t1.status = Status::Done;
        graph.add_node(Node::Task(t1));

        let mut t2 = make_task("t2", "Open task");
        t2.status = Status::Open;
        graph.add_node(Node::Task(t2));

        let mut t3 = make_task("t3", "In progress");
        t3.status = Status::InProgress;
        graph.add_node(Node::Task(t3));

        let main = make_task("main", "Main task");
        graph.add_node(Node::Task(main.clone()));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(summary.contains("## Graph Status"), "Should have status header");
        assert!(summary.contains("4 tasks"), "Should count all tasks");
        assert!(summary.contains("1 done"), "Should count done tasks");
        assert!(summary.contains("1 in-progress"), "Should count in-progress tasks");
        assert!(summary.contains("2 open"), "Should count open tasks (main + t2)");
    }

    #[test]
    fn test_build_graph_summary_includes_upstream_and_downstream() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let mut upstream = make_task("upstream", "Upstream task");
        upstream.status = Status::Done;
        upstream.description = Some("Does upstream work".to_string());
        graph.add_node(Node::Task(upstream));

        let mut main = make_task("main", "Main task");
        main.after = vec!["upstream".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let mut downstream = make_task("downstream", "Downstream task");
        downstream.after = vec!["main".to_string()];
        downstream.description = Some("Consumes main output".to_string());
        graph.add_node(Node::Task(downstream));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(summary.contains("### Upstream"), "Should have upstream section");
        assert!(summary.contains("upstream"), "Should list upstream task");
        assert!(summary.contains("### Downstream"), "Should have downstream section");
        assert!(summary.contains("downstream"), "Should list downstream task");
        assert!(summary.contains("Consumes main output"), "Should include description preview");
    }

    #[test]
    fn test_build_graph_summary_includes_siblings() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let parent = make_task("parent", "Parent task");
        graph.add_node(Node::Task(parent));

        let mut main = make_task("main", "Main task");
        main.after = vec!["parent".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let mut sibling = make_task("sibling", "Sibling task");
        sibling.after = vec!["parent".to_string()];
        graph.add_node(Node::Task(sibling));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(summary.contains("### Siblings"), "Should have siblings section");
        assert!(summary.contains("sibling"), "Should list sibling task");
    }

    #[test]
    fn test_build_graph_summary_xml_fencing() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let upstream = make_task("dep", "Dependency");
        graph.add_node(Node::Task(upstream));

        let mut main = make_task("main", "Main task");
        main.after = vec!["dep".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(summary.contains("<neighbor-context source=\"dep\">"), "Upstream should be XML fenced");
        assert!(summary.contains("</neighbor-context>"), "Should close XML fence");
    }

    #[test]
    fn test_build_graph_summary_truncates_at_4000_chars() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        // Create many tasks to exceed 4000 chars
        for i in 0..200 {
            let mut t = make_task(
                &format!("task-{:03}", i),
                &format!("A task with a long title to inflate the summary for task number {}", i),
            );
            t.description = Some(format!("Description for task {} with extra words to pad length", i));
            if i > 0 {
                t.after = vec!["task-000".to_string()];
            }
            graph.add_node(Node::Task(t));
        }

        let main_task = graph.get_task("task-000").unwrap().clone();
        let summary = build_graph_summary(&graph, &main_task, wg_dir);
        assert!(summary.len() <= 4100, "Summary should be capped near 4000 chars, got {}", summary.len());
        if summary.len() > 3950 {
            assert!(summary.contains("truncated"), "Should indicate truncation");
        }
    }

    #[test]
    fn test_build_full_graph_summary_lists_tasks() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "First task");
        t1.status = Status::Done;
        graph.add_node(Node::Task(t1));

        let mut t2 = make_task("t2", "Second task");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t2));

        let summary = build_full_graph_summary(&graph);
        assert!(summary.contains("## Full Graph Summary"), "Should have header");
        assert!(summary.contains("t1"), "Should list first task");
        assert!(summary.contains("[done]"), "Should show status");
        assert!(summary.contains("t2"), "Should list second task");
        assert!(summary.contains("(after: t1)"), "Should show dependencies");
    }

    #[test]
    fn test_build_full_graph_summary_truncates_at_budget() {
        let mut graph = WorkGraph::new();
        // Create enough tasks to exceed the 4000-char budget
        for i in 0..200 {
            let t = make_task(
                &format!("task-with-long-id-{:04}", i),
                &format!("A task with a somewhat long title for padding number {}", i),
            );
            graph.add_node(Node::Task(t));
        }

        let summary = build_full_graph_summary(&graph);
        assert!(summary.len() <= 4200, "Should be bounded by budget, got {}", summary.len());
        assert!(summary.contains("more tasks"), "Should indicate truncation");
    }

    #[test]
    fn test_build_scope_context_clean_scope_empty() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let graph = WorkGraph::new();
        let task = make_task("t1", "Test task");
        let config = Config::default();

        let ctx = build_scope_context(&graph, &task, ContextScope::Clean, &config, wg_dir);
        assert!(ctx.downstream_info.is_empty(), "Clean scope should have no downstream info");
        assert!(ctx.tags_skills_info.is_empty(), "Clean scope should have no tags info");
        assert!(ctx.project_description.is_empty(), "Clean scope should have no project description");
        assert!(ctx.graph_summary.is_empty(), "Clean scope should have no graph summary");
        assert!(ctx.full_graph_summary.is_empty(), "Clean scope should have no full graph summary");
        assert!(ctx.claude_md_content.is_empty(), "Clean scope should have no CLAUDE.md content");
    }

    #[test]
    fn test_build_scope_context_task_scope_includes_downstream() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Main task");
        graph.add_node(Node::Task(task.clone()));

        let mut downstream = make_task("d1", "Dependent task");
        downstream.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(downstream));

        let config = Config::default();
        let ctx = build_scope_context(&graph, &task, ContextScope::Task, &config, wg_dir);
        assert!(ctx.downstream_info.contains("d1"), "Task scope should include downstream");
        assert!(ctx.downstream_info.contains("Dependent task"), "Should include downstream title");
        // Should NOT include graph-level stuff
        assert!(ctx.graph_summary.is_empty(), "Task scope should not have graph summary");
    }

    #[test]
    fn test_build_scope_context_task_scope_includes_tags_skills() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let graph = WorkGraph::new();
        let mut task = make_task("t1", "Tagged task");
        task.tags = vec!["rust".to_string(), "backend".to_string()];
        task.skills = vec!["implementation".to_string()];

        let config = Config::default();
        let ctx = build_scope_context(&graph, &task, ContextScope::Task, &config, wg_dir);
        assert!(ctx.tags_skills_info.contains("rust"), "Should include tags");
        assert!(ctx.tags_skills_info.contains("implementation"), "Should include skills");
    }

    #[test]
    fn test_build_scope_context_graph_scope_includes_summary() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Graph task");
        graph.add_node(Node::Task(task.clone()));

        let mut config = Config::default();
        config.project.description = Some("A test project".to_string());

        let ctx = build_scope_context(&graph, &task, ContextScope::Graph, &config, wg_dir);
        assert!(ctx.project_description.contains("A test project"), "Graph scope should include project description");
        assert!(!ctx.graph_summary.is_empty(), "Graph scope should have graph summary");
        // Should NOT include full-scope stuff
        assert!(ctx.full_graph_summary.is_empty(), "Graph scope should not have full graph summary");
        assert!(ctx.claude_md_content.is_empty(), "Graph scope should not have CLAUDE.md");
    }

    #[test]
    fn test_build_scope_context_full_scope_includes_everything() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Full task");
        graph.add_node(Node::Task(task.clone()));

        let mut config = Config::default();
        config.project.description = Some("Test project".to_string());

        let ctx = build_scope_context(&graph, &task, ContextScope::Full, &config, wg_dir);
        assert!(!ctx.graph_summary.is_empty(), "Full scope should have graph summary");
        assert!(!ctx.full_graph_summary.is_empty(), "Full scope should have full graph summary");
        assert!(ctx.full_graph_summary.contains("Full Graph Summary"), "Should include full graph summary header");
    }

    #[test]
    fn test_resolve_task_scope_defaults_to_task() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let task = make_task("t1", "Test");
        let config = Config::default();
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(scope, ContextScope::Task, "Default scope should be Task");
    }

    #[test]
    fn test_resolve_task_scope_task_overrides() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let mut task = make_task("t1", "Test");
        task.context_scope = Some("clean".to_string());
        let mut config = Config::default();
        config.coordinator.default_context_scope = Some("full".to_string());
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(scope, ContextScope::Clean, "Task scope should override config");
    }

    #[test]
    fn test_resolve_task_scope_config_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir).unwrap();

        let task = make_task("t1", "Test");
        let mut config = Config::default();
        config.coordinator.default_context_scope = Some("graph".to_string());
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(scope, ContextScope::Graph, "Config scope should be used as fallback");
    }

    #[test]
    fn test_parse_timeout_secs_minutes() {
        assert_eq!(parse_timeout_secs("30m").unwrap(), 1800);
    }

    #[test]
    fn test_parse_timeout_secs_hours() {
        assert_eq!(parse_timeout_secs("2h").unwrap(), 7200);
    }

    #[test]
    fn test_parse_timeout_secs_seconds() {
        assert_eq!(parse_timeout_secs("90s").unwrap(), 90);
    }

    #[test]
    fn test_parse_timeout_secs_no_unit() {
        assert_eq!(parse_timeout_secs("120").unwrap(), 120);
    }

    #[test]
    fn test_parse_timeout_secs_empty_fails() {
        assert!(parse_timeout_secs("").is_err());
    }

    #[test]
    fn test_wrapper_script_includes_timeout() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        // Spawn with explicit timeout
        run(temp_dir.path(), "t1", "shell", Some("5m"), None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Verify the timeout command wraps the inner command
        assert!(
            script.contains("timeout --signal=TERM --kill-after=30 300"),
            "Wrapper should contain timeout command with 300s (5m). Script:\n{}",
            script
        );
        // Verify timeout exit code handling
        assert!(
            script.contains("EXIT_CODE -eq 124"),
            "Wrapper should handle timeout exit code 124"
        );
        assert!(
            script.contains("Agent exceeded hard timeout"),
            "Wrapper should report timeout in failure reason"
        );
    }

    #[test]
    fn test_wrapper_script_default_timeout_from_config() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        // Default config has agent_timeout = "30m", no explicit timeout
        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Default timeout is 30m = 1800s
        assert!(
            script.contains("timeout --signal=TERM --kill-after=30 1800"),
            "Wrapper should contain default timeout of 1800s (30m). Script:\n{}",
            script
        );
    }

    #[test]
    fn test_metadata_records_effective_timeout() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", Some("10m"), None, false).unwrap();

        let metadata_path = agent_output_dir(temp_dir.path(), "agent-1").join("metadata.json");
        let metadata: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&metadata_path).unwrap()).unwrap();
        assert_eq!(metadata["timeout_secs"], 600, "Metadata should record 600s (10m)");
    }
}
