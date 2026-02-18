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
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::executor::{ExecutorRegistry, TemplateVars};
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

/// Build context string from dependency artifacts and logs
fn build_task_context(graph: &workgraph::WorkGraph, task: &workgraph::graph::Task) -> String {
    let mut context_parts = Vec::new();

    for dep_id in &task.blocked_by {
        if let Some(dep_task) = graph.get_task(dep_id) {
            if !dep_task.artifacts.is_empty() {
                context_parts.push(format!(
                    "From {}: artifacts: {}",
                    dep_id,
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

    if context_parts.is_empty() {
        "No context from dependencies".to_string()
    } else {
        context_parts.join("\n")
    }
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

    // Build context from dependencies
    let task_context = build_task_context(&graph, task);

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
    let settings = executor_config.apply_templates(&vars);

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

    // Create a wrapper script that runs the command and handles completion
    // This ensures tasks get marked done/failed even if the agent doesn't do it
    let complete_cmd = "wg done \"$TASK_ID\" 2>> \"$OUTPUT_FILE\" || echo \"[wrapper] WARNING: 'wg done' failed with exit code $?\" >> \"$OUTPUT_FILE\"".to_string();
    let complete_msg = "[wrapper] Agent exited successfully, marking task done";

    let wrapper_script = format!(
        r#"#!/bin/bash
TASK_ID={escaped_task_id}
OUTPUT_FILE={escaped_output_file}

# Allow nested Claude Code sessions (spawned agents are independent)
unset CLAUDECODE
unset CLAUDE_CODE_ENTRYPOINT

# Run the agent command
{inner_command} >> "$OUTPUT_FILE" 2>&1
EXIT_CODE=$?

# Check if task is still in progress (agent didn't mark it done/failed)
TASK_STATUS=$(wg show "$TASK_ID" --json 2>/dev/null | grep -o '"status": *"[^"]*"' | head -1 | sed 's/.*"status": *"//;s/"//' || echo "unknown")

if [ "$TASK_STATUS" = "in-progress" ]; then
    if [ $EXIT_CODE -eq 0 ]; then
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
        inner_command = inner_command,
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
        "timeout": timeout,
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
        main_task.blocked_by = vec!["dep-1".to_string()];
        graph.add_node(Node::Task(main_task.clone()));

        let context = build_task_context(&graph, &main_task);
        assert!(context.contains("dep-1"));
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
}
