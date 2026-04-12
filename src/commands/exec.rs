use anyhow::{Context, Result};
use chrono::Utc;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use workgraph::config::Config;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, modify_graph};
use workgraph::service::executor::{TemplateVars, build_prompt};

use super::spawn::context::{
    build_scope_context, build_task_context, discover_test_files, format_test_discovery_context,
    resolve_task_scope,
};
use super::spawn::worktree;

#[cfg(test)]
use super::graph_path;

/// Execute a task's shell command
///
/// This implements the "optional exec helper" part of the execution model:
/// - Claims the task if not already in progress
/// - Runs the task's exec command
/// - Marks done on success (exit 0), fail on error
pub fn run(dir: &Path, task_id: &str, actor: Option<&str>, dry_run: bool) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Read task data and validate
    let mut error: Option<anyhow::Error> = None;
    let mut exec_cmd_opt: Option<String> = None;
    let mut task_status = Status::Open;

    modify_graph(&path, |graph| {
        let task = match graph.get_task(task_id) {
            Some(t) => t,
            None => {
                error = Some(anyhow::anyhow!("Task '{}' not found", task_id));
                return false;
            }
        };

        exec_cmd_opt = task.exec.clone();
        task_status = task.status;

        if exec_cmd_opt.is_none() {
            error = Some(anyhow::anyhow!(
                "Task '{}' has no exec command defined",
                task_id
            ));
            return false;
        }
        if task.status == Status::Done {
            error = Some(anyhow::anyhow!("Task '{}' is already done", task_id));
            return false;
        }
        if dry_run {
            return false;
        }

        // Claim the task if open
        if task.status == Status::Open {
            let task = graph.get_task_mut(task_id).expect("task verified above");
            task.status = Status::InProgress;
            task.started_at = Some(Utc::now().to_rfc3339());
            if let Some(actor_id) = actor {
                task.assigned = Some(actor_id.to_string());
            }
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: actor.map(String::from),
                user: Some(workgraph::current_user()),
                message: format!(
                    "Started execution: {}",
                    exec_cmd_opt.as_deref().unwrap_or("")
                ),
            });
            return true;
        }
        false
    })
    .context("Failed to modify graph")?;
    if let Some(e) = error {
        return Err(e);
    }

    let exec_cmd = exec_cmd_opt.unwrap();

    if dry_run {
        println!("Would execute for task '{}':", task_id);
        println!("  Command: {}", exec_cmd);
        println!("  Status: {:?} -> InProgress -> Done/Failed", task_status);
        return Ok(());
    }

    if task_status == Status::Open {
        super::notify_graph_changed(dir);
        println!("Claimed task '{}' for execution", task_id);
    }

    // Run the command
    println!("Executing: {}", exec_cmd);
    let output = Command::new("sh")
        .arg("-c")
        .arg(&exec_cmd)
        .output()
        .context("Failed to execute command")?;

    let success = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stdout.is_empty() {
        println!("{}", stdout);
    }
    if !stderr.is_empty() {
        eprintln!("{}", stderr);
    }

    // Update status atomically (task may have been modified by exec command)
    let actor_clone = actor.map(String::from);
    let exit_code = output.status.code().unwrap_or(-1);
    modify_graph(&path, |graph| {
        if let Some(task) = graph.get_task_mut(task_id) {
            if success {
                task.status = Status::Done;
                task.completed_at = Some(Utc::now().to_rfc3339());
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: actor_clone.clone(),
                    user: Some(workgraph::current_user()),
                    message: "Execution completed successfully".to_string(),
                });
            } else {
                task.status = Status::Failed;
                task.retry_count += 1;
                task.failure_reason = Some(format!("Command exited with code {}", exit_code));
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: actor_clone.clone(),
                    user: Some(workgraph::current_user()),
                    message: format!("Execution failed with exit code {}", exit_code),
                });
            }
            true
        } else {
            false
        }
    })
    .context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    if success {
        println!("Task '{}' completed successfully", task_id);
    } else {
        anyhow::bail!("Task '{}' failed with exit code {}", task_id, exit_code);
    }

    Ok(())
}

/// Drop into an interactive agent session for a task.
///
/// Replicates the spawned-agent experience: same context, same env vars,
/// but human-driven via an interactive executor session.
pub fn run_interactive(
    dir: &Path,
    task_id: &str,
    actor: Option<&str>,
    dry_run: bool,
    use_worktree: bool,
    model: Option<&str>,
) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let task = graph.get_task_or_err(task_id)?;

    // Validate task status
    match task.status {
        Status::Open | Status::Blocked => {}
        Status::InProgress => {
            // Allow re-entering an in-progress task (the user might be resuming)
            eprintln!(
                "Note: task '{}' is already in-progress (assigned: {})",
                task_id,
                task.assigned.as_deref().unwrap_or("none")
            );
        }
        Status::Done => anyhow::bail!("Task '{}' is already done", task_id),
        Status::Failed => {
            anyhow::bail!("Task '{}' is failed. Use 'wg retry' first.", task_id)
        }
        Status::Abandoned => anyhow::bail!("Task '{}' is abandoned", task_id),
        Status::Waiting => anyhow::bail!("Task '{}' is waiting", task_id),
        Status::PendingValidation => {
            anyhow::bail!("Task '{}' is pending validation", task_id)
        }
    }

    // Resolve config + context scope
    let config = Config::load_or_default(dir);
    let scope = resolve_task_scope(task, &config, dir);

    // Build context from dependencies
    let task_context = build_task_context(&graph, task);

    // Build scope context
    let mut scope_ctx = build_scope_context(&graph, task, scope, &config, dir);

    // Inject test discovery
    let project_root = dir
        .canonicalize()
        .ok()
        .and_then(|abs| abs.parent().map(|p| p.to_path_buf()));
    if let Some(ref root) = project_root {
        let test_files = discover_test_files(root);
        if !test_files.is_empty() {
            scope_ctx.discovered_tests = format_test_discovery_context(&test_files);
        }
    }

    // Build template vars
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

    // Override model if specified
    if let Some(m) = model {
        vars.model = m.to_string();
    }

    // Assemble the prompt
    let prompt = build_prompt(&vars, scope, &scope_ctx);

    // Build env vars map (same as spawned agents)
    let user = workgraph::current_user();
    let agent_label = actor.unwrap_or(&user);
    let mut env_vars: Vec<(String, String)> = vec![
        ("WG_TASK_ID".into(), task_id.to_string()),
        ("WG_AGENT_ID".into(), format!("exec-{}", agent_label)),
        ("WG_EXECUTOR_TYPE".into(), "claude".into()),
        ("WG_USER".into(), user.clone()),
    ];
    if let Some(m) = model {
        env_vars.push(("WG_MODEL".into(), m.to_string()));
    } else if !vars.model.is_empty() {
        env_vars.push(("WG_MODEL".into(), vars.model.clone()));
    }

    // --- Dry run: print context and env vars ---
    if dry_run {
        println!("=== Environment Variables ===\n");
        for (key, val) in &env_vars {
            println!("  {}={}", key, val);
        }
        println!("\n=== Assembled Prompt ({} bytes) ===\n", prompt.len());
        println!("{}", prompt);
        return Ok(());
    }

    // --- Claim the task ---
    let needs_claim = task.status == Status::Open || task.status == Status::Blocked;
    if needs_claim {
        let actor_s = actor.map(String::from);
        modify_graph(&path, |graph| {
            if let Some(t) = graph.get_task_mut(task_id) {
                t.status = Status::InProgress;
                t.started_at = Some(Utc::now().to_rfc3339());
                if let Some(ref a) = actor_s {
                    t.assigned = Some(a.clone());
                } else {
                    t.assigned = Some(format!("exec-{}", workgraph::current_user()));
                }
                t.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: actor_s.clone(),
                    user: Some(workgraph::current_user()),
                    message: "Started interactive exec session".to_string(),
                });
                true
            } else {
                false
            }
        })
        .context("Failed to claim task")?;
        super::notify_graph_changed(dir);
        eprintln!("Claimed task '{}' for interactive session", task_id);
    }

    // --- Optional worktree ---
    let worktree_info = if use_worktree {
        let root = project_root
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine project root for worktree"))?;
        let wt = worktree::create_worktree(root, dir, &format!("exec-{}", agent_label), task_id)?;
        eprintln!("Created worktree at {:?} (branch: {})", wt.path, wt.branch);
        env_vars.push(("WG_WORKTREE_PATH".into(), wt.path.to_string_lossy().into()));
        env_vars.push(("WG_BRANCH".into(), wt.branch.clone()));
        env_vars.push((
            "WG_PROJECT_ROOT".into(),
            wt.project_root.to_string_lossy().into(),
        ));
        Some(wt)
    } else {
        None
    };

    // --- Write prompt to temp file ---
    let prompt_dir = dir.join("agents").join(format!("exec-{}", agent_label));
    std::fs::create_dir_all(&prompt_dir).context("Failed to create exec agent output directory")?;
    let prompt_file = prompt_dir.join("prompt.txt");
    std::fs::write(&prompt_file, &prompt).context("Failed to write prompt file")?;

    // Log the prompt file location
    let _ = modify_graph(&path, |graph| {
        if let Some(t) = graph.get_task_mut(task_id) {
            t.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: actor.map(String::from),
                user: Some(workgraph::current_user()),
                message: format!(
                    "Interactive exec session started. Prompt: {}",
                    prompt_file.display()
                ),
            });
            true
        } else {
            false
        }
    });

    // --- Launch executor interactively ---
    eprintln!(
        "Launching interactive claude session for task '{}'...",
        task_id
    );
    eprintln!("(Prompt written to {})", prompt_file.display());

    let mut cmd = Command::new("claude");
    // Interactive mode: no --print, no --output-format
    // Pass prompt via --system-prompt and give task info on stdin
    cmd.arg("--system-prompt").arg(&prompt);

    // Add model if specified
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    } else if !vars.model.is_empty() {
        cmd.arg("--model").arg(&vars.model);
    }

    // Set env vars
    for (key, val) in &env_vars {
        cmd.env(key, val);
    }

    // Remove CLAUDECODE to allow nested sessions
    cmd.env_remove("CLAUDECODE");
    cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");

    // Set working directory
    if let Some(ref wt) = worktree_info {
        cmd.current_dir(&wt.path);
    }

    // Run interactively (inherit stdio)
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    let status = cmd.status().context(
        "Failed to launch claude CLI. Is it installed? \
         (Try: npm install -g @anthropic-ai/claude-code)",
    )?;

    let exit_code = status.code().unwrap_or(-1);
    eprintln!("\nClaude session ended (exit code: {})", exit_code);

    // --- Log session end ---
    let _ = modify_graph(&path, |graph| {
        if let Some(t) = graph.get_task_mut(task_id) {
            t.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: actor.map(String::from),
                user: Some(workgraph::current_user()),
                message: format!("Interactive exec session ended (exit code: {})", exit_code),
            });
            true
        } else {
            false
        }
    });

    // --- Completion handling: prompt user ---
    if std::io::stdin().is_terminal() {
        eprintln!("\nTask '{}' is still in-progress.", task_id);
        eprint!("Mark as: [d]one, [f]ailed, or [l]eave in-progress? ");
        std::io::stderr().flush().ok();

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .context("Failed to read input")?;
        let choice = input.trim().to_lowercase();

        match choice.as_str() {
            "d" | "done" => {
                modify_graph(&path, |graph| {
                    if let Some(t) = graph.get_task_mut(task_id) {
                        t.status = Status::Done;
                        t.completed_at = Some(Utc::now().to_rfc3339());
                        t.log.push(LogEntry {
                            timestamp: Utc::now().to_rfc3339(),
                            actor: actor.map(String::from),
                            user: Some(workgraph::current_user()),
                            message: "Marked done via interactive exec session".to_string(),
                        });
                        true
                    } else {
                        false
                    }
                })
                .context("Failed to mark task done")?;
                super::notify_graph_changed(dir);
                eprintln!("Task '{}' marked as done.", task_id);
            }
            "f" | "failed" | "fail" => {
                eprint!("Failure reason (optional): ");
                std::io::stderr().flush().ok();
                let mut reason = String::new();
                std::io::stdin().read_line(&mut reason).ok();
                let reason = reason.trim().to_string();
                let reason_opt = if reason.is_empty() {
                    None
                } else {
                    Some(reason)
                };

                modify_graph(&path, |graph| {
                    if let Some(t) = graph.get_task_mut(task_id) {
                        t.status = Status::Failed;
                        t.retry_count += 1;
                        if let Some(ref r) = reason_opt {
                            t.failure_reason = Some(r.clone());
                        }
                        t.log.push(LogEntry {
                            timestamp: Utc::now().to_rfc3339(),
                            actor: actor.map(String::from),
                            user: Some(workgraph::current_user()),
                            message: format!(
                                "Marked failed via interactive exec session{}",
                                reason_opt
                                    .as_ref()
                                    .map(|r| format!(": {}", r))
                                    .unwrap_or_default()
                            ),
                        });
                        true
                    } else {
                        false
                    }
                })
                .context("Failed to mark task failed")?;
                super::notify_graph_changed(dir);
                eprintln!("Task '{}' marked as failed.", task_id);
            }
            _ => {
                eprintln!("Task '{}' left in-progress.", task_id);
            }
        }
    }

    // --- Worktree cleanup ---
    if let Some(ref wt) = worktree_info {
        eprintln!("Cleaning up worktree...");
        if let Err(e) = worktree::remove_worktree(&wt.project_root, &wt.path, &wt.branch) {
            eprintln!("Warning: failed to clean up worktree: {}", e);
        }
    }

    Ok(())
}

/// Set the exec command for a task
pub fn set_exec(dir: &Path, task_id: &str, command: &str) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut error: Option<anyhow::Error> = None;
    modify_graph(&path, |graph| match graph.get_task_mut(task_id) {
        Some(task) => {
            task.exec = Some(command.to_string());
            true
        }
        None => {
            error = Some(anyhow::anyhow!("Task '{}' not found", task_id));
            false
        }
    })
    .context("Failed to modify graph")?;
    if let Some(e) = error {
        return Err(e);
    }

    super::notify_graph_changed(dir);
    println!("Set exec command for '{}': {}", task_id, command);
    Ok(())
}

/// Clear the exec command for a task
pub fn clear_exec(dir: &Path, task_id: &str) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut error: Option<anyhow::Error> = None;
    let mut was_none = false;
    modify_graph(&path, |graph| match graph.get_task_mut(task_id) {
        Some(task) => {
            if task.exec.is_none() {
                was_none = true;
                return false;
            }
            task.exec = None;
            true
        }
        None => {
            error = Some(anyhow::anyhow!("Task '{}' not found", task_id));
            false
        }
    })
    .context("Failed to modify graph")?;
    if let Some(e) = error {
        return Err(e);
    }

    if was_none {
        println!("Task '{}' has no exec command to clear", task_id);
    } else {
        super::notify_graph_changed(dir);
        println!("Cleared exec command for '{}'", task_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_graph_with_exec() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        temp_dir
    }

    #[test]
    fn test_exec_success() {
        let temp_dir = setup_graph_with_exec();

        let result = run(temp_dir.path(), "t1", None, false);
        assert!(result.is_ok());

        // Verify task is done
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_exec_failure() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Failing Task");
        task.exec = Some("exit 1".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", None, false);
        assert!(result.is_err());

        // Verify task is failed
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
    }

    #[test]
    fn test_exec_no_command() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "No Exec Task");
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no exec command"));
    }

    #[test]
    fn test_exec_dry_run() {
        let temp_dir = setup_graph_with_exec();

        let result = run(temp_dir.path(), "t1", None, true);
        assert!(result.is_ok());

        // Verify task is still open (dry run doesn't execute)
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_set_exec() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Test Task");
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let result = set_exec(temp_dir.path(), "t1", "echo test");
        assert!(result.is_ok());

        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.exec, Some("echo test".to_string()));
    }

    #[test]
    fn test_clear_exec() {
        let temp_dir = setup_graph_with_exec();

        let result = clear_exec(temp_dir.path(), "t1");
        assert!(result.is_ok());

        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.exec.is_none());
    }
}
