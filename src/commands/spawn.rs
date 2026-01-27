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
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::executor::{ExecutorRegistry, TemplateVars};
use workgraph::service::registry::AgentRegistry;

use super::graph_path;

/// Result of spawning an agent
#[derive(Debug, Serialize)]
pub struct SpawnResult {
    pub agent_id: String,
    pub pid: u32,
    pub task_id: String,
    pub executor: String,
    pub output_file: String,
}

/// Parse a timeout duration string like "30m", "1h", "90s"
fn parse_timeout(timeout_str: &str) -> Result<std::time::Duration> {
    let timeout_str = timeout_str.trim();
    if timeout_str.is_empty() {
        anyhow::bail!("Empty timeout string");
    }

    let (num_str, unit) = if timeout_str.ends_with('s') {
        (&timeout_str[..timeout_str.len() - 1], "s")
    } else if timeout_str.ends_with('m') {
        (&timeout_str[..timeout_str.len() - 1], "m")
    } else if timeout_str.ends_with('h') {
        (&timeout_str[..timeout_str.len() - 1], "h")
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

/// Build context string from dependency artifacts
fn build_task_context(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
) -> String {
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
        }
    }

    if context_parts.is_empty() {
        "No context from dependencies".to_string()
    } else {
        context_parts.join("\n")
    }
}

/// Run the spawn command
pub fn run(
    dir: &Path,
    task_id: &str,
    executor_name: &str,
    timeout: Option<&str>,
    json: bool,
) -> Result<()> {
    let graph_path = graph_path(dir);

    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Load the graph and get task info
    let mut graph = load_graph(&graph_path).context("Failed to load graph")?;

    let task = graph
        .get_task(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Check if task is already claimed
    match task.status {
        Status::InProgress => {
            let since = task
                .started_at
                .as_ref()
                .map(|t| format!(" (since {})", t))
                .unwrap_or_default();
            match &task.assigned {
                Some(assigned) => {
                    anyhow::bail!("Task '{}' is already claimed by @{}{}", task_id, assigned, since);
                }
                None => {
                    anyhow::bail!("Task '{}' is already in progress{}", task_id, since);
                }
            }
        }
        Status::Done => {
            anyhow::bail!("Task '{}' is already done", task_id);
        }
        _ => {}
    }

    // Build context from dependencies
    let task_context = build_task_context(&graph, task);

    // Create template variables
    let vars = TemplateVars {
        task_id: task.id.clone(),
        task_title: task.title.clone(),
        task_description: task.description.clone().unwrap_or_default(),
        task_context: task_context.clone(),
    };

    // Get task exec command for shell executor
    let task_exec = task.exec.clone();

    // Load executor config using the registry
    let executor_registry = ExecutorRegistry::new(dir);
    let executor_config = executor_registry.load_config(executor_name)?;

    // For shell executor, we need an exec command
    if executor_config.executor.executor_type == "shell" && task_exec.is_none() {
        anyhow::bail!("Task '{}' has no exec command for shell executor", task_id);
    }

    // Load agent registry and prepare agent output directory
    let mut agent_registry = AgentRegistry::load(dir)?;

    // We need to know the agent ID before spawning to set up the output directory
    let temp_agent_id = format!("agent-{}", agent_registry.next_agent_id);
    let output_dir = agent_output_dir(dir, &temp_agent_id);
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Failed to create agent output directory at {:?}", output_dir))?;

    let output_file = output_dir.join("output.log");
    let output_file_str = output_file.to_string_lossy().to_string();

    // Apply templates to executor settings
    let settings = executor_config.apply_templates(&vars);

    // Build the command
    let mut cmd = Command::new(&settings.command);

    // Set environment variables from executor config
    for (key, value) in &settings.env {
        cmd.env(key, value);
    }

    // Add task ID and agent ID to environment
    cmd.env("WG_TASK_ID", task_id);
    cmd.env("WG_AGENT_ID", &temp_agent_id);

    // Build arguments based on executor type
    match settings.executor_type.as_str() {
        "claude" => {
            // Add base args
            for arg in &settings.args {
                cmd.arg(arg);
            }

            // Add prompt from template
            if let Some(ref prompt_template) = settings.prompt_template {
                cmd.arg(&prompt_template.template);
            }
        }
        "shell" => {
            // For shell, use the task's exec command as the script
            cmd.arg("-c");
            cmd.arg(task_exec.as_ref().unwrap());
        }
        _ => {
            // Custom executor - just pass args as-is (already templated)
            for arg in &settings.args {
                cmd.arg(arg);
            }
        }
    }

    // Set working directory if specified
    if let Some(ref wd) = settings.working_dir {
        cmd.current_dir(wd);
    }

    // Set up output redirection to log file
    let log_file = File::create(&output_file)
        .with_context(|| format!("Failed to create output log at {:?}", output_file))?;
    let log_file_err = log_file.try_clone()?;

    cmd.stdout(Stdio::from(log_file));
    cmd.stderr(Stdio::from(log_file_err));

    // Spawn the process (don't wait)
    let child = cmd.spawn().with_context(|| {
        format!(
            "Failed to spawn executor '{}' (command: {})",
            executor_name, settings.command
        )
    })?;

    let pid = child.id();

    // Now claim the task
    let task = graph.get_task_mut(task_id).unwrap();
    task.status = Status::InProgress;
    task.started_at = Some(Utc::now().to_rfc3339());
    task.assigned = Some(temp_agent_id.clone());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(temp_agent_id.clone()),
        message: format!("Spawned by wg spawn --executor {}", executor_name),
    });

    save_graph(&graph, &graph_path).context("Failed to save graph")?;

    // Register the agent
    let agent_id = agent_registry.register(pid, task_id, executor_name, &output_file_str);
    agent_registry.save(dir)?;

    // Write metadata
    let metadata_path = output_dir.join("metadata.json");
    let metadata = serde_json::json!({
        "agent_id": agent_id,
        "pid": pid,
        "task_id": task_id,
        "executor": executor_name,
        "started_at": Utc::now().to_rfc3339(),
        "timeout": timeout,
    });
    fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

    // Output result
    if json {
        let result = SpawnResult {
            agent_id: agent_id.clone(),
            pid,
            task_id: task_id.to_string(),
            executor: executor_name.to_string(),
            output_file: output_file_str.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Spawned {} for task '{}'", agent_id, task_id);
        println!("  Executor: {} ({})", executor_name, settings.executor_type);
        println!("  PID: {}", pid);
        println!("  Output: {}", output_file_str);
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
) -> Result<(String, u32)> {
    let graph_path = graph_path(dir);

    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Load the graph and get task info
    let mut graph = load_graph(&graph_path).context("Failed to load graph")?;

    let task = graph
        .get_task(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Check if task is already claimed
    match task.status {
        Status::InProgress => {
            let since = task
                .started_at
                .as_ref()
                .map(|t| format!(" (since {})", t))
                .unwrap_or_default();
            match &task.assigned {
                Some(assigned) => {
                    anyhow::bail!("Task '{}' is already claimed by @{}{}", task_id, assigned, since);
                }
                None => {
                    anyhow::bail!("Task '{}' is already in progress{}", task_id, since);
                }
            }
        }
        Status::Done => {
            anyhow::bail!("Task '{}' is already done", task_id);
        }
        _ => {}
    }

    // Build context from dependencies
    let task_context = build_task_context(&graph, task);

    // Create template variables
    let vars = TemplateVars {
        task_id: task.id.clone(),
        task_title: task.title.clone(),
        task_description: task.description.clone().unwrap_or_default(),
        task_context: task_context.clone(),
    };

    // Get task exec command for shell executor
    let task_exec = task.exec.clone();

    // Load executor config using the registry
    let executor_registry = ExecutorRegistry::new(dir);
    let executor_config = executor_registry.load_config(executor_name)?;

    // For shell executor, we need an exec command
    if executor_config.executor.executor_type == "shell" && task_exec.is_none() {
        anyhow::bail!("Task '{}' has no exec command for shell executor", task_id);
    }

    // Load agent registry and prepare agent output directory
    let mut agent_registry = AgentRegistry::load(dir)?;

    // We need to know the agent ID before spawning to set up the output directory
    let temp_agent_id = format!("agent-{}", agent_registry.next_agent_id);
    let output_dir = agent_output_dir(dir, &temp_agent_id);
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Failed to create agent output directory at {:?}", output_dir))?;

    let output_file = output_dir.join("output.log");
    let output_file_str = output_file.to_string_lossy().to_string();

    // Apply templates to executor settings
    let settings = executor_config.apply_templates(&vars);

    // Build the command
    let mut cmd = Command::new(&settings.command);

    // Set environment variables from executor config
    for (key, value) in &settings.env {
        cmd.env(key, value);
    }

    // Add task ID and agent ID to environment
    cmd.env("WG_TASK_ID", task_id);
    cmd.env("WG_AGENT_ID", &temp_agent_id);

    // Build arguments based on executor type
    match settings.executor_type.as_str() {
        "claude" => {
            // Add base args
            for arg in &settings.args {
                cmd.arg(arg);
            }

            // Add prompt from template
            if let Some(ref prompt_template) = settings.prompt_template {
                cmd.arg(&prompt_template.template);
            }
        }
        "shell" => {
            // For shell, use the task's exec command as the script
            cmd.arg("-c");
            cmd.arg(task_exec.as_ref().unwrap());
        }
        _ => {
            // Custom executor - just pass args as-is (already templated)
            for arg in &settings.args {
                cmd.arg(arg);
            }
        }
    }

    // Set working directory if specified
    if let Some(ref wd) = settings.working_dir {
        cmd.current_dir(wd);
    }

    // Set up output redirection to log file
    let log_file = File::create(&output_file)
        .with_context(|| format!("Failed to create output log at {:?}", output_file))?;
    let log_file_err = log_file.try_clone()?;

    cmd.stdout(Stdio::from(log_file));
    cmd.stderr(Stdio::from(log_file_err));

    // Spawn the process (don't wait)
    let child = cmd.spawn().with_context(|| {
        format!(
            "Failed to spawn executor '{}' (command: {})",
            executor_name, settings.command
        )
    })?;

    let pid = child.id();

    // Now claim the task
    let task = graph.get_task_mut(task_id).unwrap();
    task.status = Status::InProgress;
    task.started_at = Some(Utc::now().to_rfc3339());
    task.assigned = Some(temp_agent_id.clone());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(temp_agent_id.clone()),
        message: format!("Spawned by wg spawn --executor {}", executor_name),
    });

    save_graph(&graph, &graph_path).context("Failed to save graph")?;

    // Register the agent
    let agent_id = agent_registry.register(pid, task_id, executor_name, &output_file_str);
    agent_registry.save(dir)?;

    // Write metadata
    let metadata_path = output_dir.join("metadata.json");
    let metadata = serde_json::json!({
        "agent_id": agent_id,
        "pid": pid,
        "task_id": task_id,
        "executor": executor_name,
        "started_at": Utc::now().to_rfc3339(),
        "timeout": timeout,
    });
    fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

    Ok((agent_id, pid))
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
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
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

        let result = run(temp_dir.path(), "nonexistent", "shell", None, false);
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

        let result = run(temp_dir.path(), "t1", "shell", None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already claimed"));
    }

    #[test]
    fn test_spawn_done_task() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.status = Status::Done;
        setup_graph(temp_dir.path(), vec![task]);

        let result = run(temp_dir.path(), "t1", "shell", None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already done"));
    }

    #[test]
    fn test_spawn_shell_without_exec_fails() {
        let temp_dir = TempDir::new().unwrap();
        let task = make_task("t1", "Test Task");
        // Task has no exec command
        setup_graph(temp_dir.path(), vec![task]);

        let result = run(temp_dir.path(), "t1", "shell", None, false);
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
        let result = run(temp_dir.path(), "t1", "shell", None, false);
        assert!(result.is_ok());

        // Verify task was claimed
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
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

        run(temp_dir.path(), "t1", "shell", None, false).unwrap();

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

        // Create a dependency task with artifacts
        let mut dep_task = make_task("dep-1", "Dependency");
        dep_task.status = Status::Done;
        dep_task.artifacts = vec!["output.txt".to_string(), "data.json".to_string()];
        graph.add_node(Node::Task(dep_task));

        // Create main task blocked by dependency
        let mut main_task = make_task("main", "Main Task");
        main_task.blocked_by = vec!["dep-1".to_string()];
        graph.add_node(Node::Task(main_task.clone()));

        let context = build_task_context(&graph, &main_task);
        assert!(context.contains("dep-1"));
        assert!(context.contains("output.txt"));
        assert!(context.contains("data.json"));
    }

    #[test]
    fn test_build_task_context_no_deps() {
        let graph = WorkGraph::new();
        let task = make_task("t1", "Test Task");

        let context = build_task_context(&graph, &task);
        assert_eq!(context, "No context from dependencies");
    }
}
