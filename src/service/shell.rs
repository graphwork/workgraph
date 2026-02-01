//! Shell executor for running shell commands as agents.
//!
//! This executor runs shell commands (bash scripts, etc.) as agents.
//! Task information is provided via environment variables.

use anyhow::{anyhow, Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::graph::Task;

use super::executor::{AgentHandle, Executor, ExecutorConfig, ExecutorSettings, TemplateVars};

/// Environment variable names for task information.
pub mod env_vars {
    /// Task ID
    pub const TASK_ID: &str = "WG_TASK_ID";
    /// Task title
    pub const TASK_TITLE: &str = "WG_TASK_TITLE";
    /// Task description
    pub const TASK_DESCRIPTION: &str = "WG_TASK_DESCRIPTION";
    /// Context from dependencies
    pub const TASK_CONTEXT: &str = "WG_TASK_CONTEXT";
    /// The exec command from the task (if any)
    pub const TASK_EXEC: &str = "WG_TASK_EXEC";
    /// Working directory
    pub const WORKDIR: &str = "WG_WORKDIR";
}

/// Shell executor - runs shell commands as agents.
pub struct ShellExecutor {
    /// Shell to use (default: bash).
    shell: String,

    /// Output directory for agent logs.
    output_dir: PathBuf,
}

impl ShellExecutor {
    /// Create a new shell executor with default settings.
    pub fn new(workgraph_dir: &Path) -> Self {
        Self {
            shell: "bash".to_string(),
            output_dir: workgraph_dir.join("agents"),
        }
    }

    /// Set the shell to use.
    pub fn shell(mut self, shell: &str) -> Self {
        self.shell = shell.to_string();
        self
    }

    /// Set the output directory for agent logs.
    pub fn output_dir(mut self, dir: PathBuf) -> Self {
        self.output_dir = dir;
        self
    }

    /// Get the agent output directory for a specific agent.
    pub fn agent_output_dir(&self, agent_id: &str) -> PathBuf {
        self.output_dir.join(agent_id)
    }

    /// Ensure the agent output directory exists.
    fn ensure_output_dir(&self, agent_id: &str) -> Result<PathBuf> {
        let dir = self.agent_output_dir(agent_id);
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create agent output directory: {}", dir.display()))?;
        }
        Ok(dir)
    }

    /// Create a metadata file for the agent.
    fn write_metadata(&self, agent_dir: &Path, task: &Task, command: &str) -> Result<()> {
        let metadata = serde_json::json!({
            "task_id": task.id,
            "task_title": task.title,
            "executor": "shell",
            "shell": self.shell,
            "command": command,
            "started_at": chrono::Utc::now().to_rfc3339(),
        });

        let metadata_file = agent_dir.join("metadata.json");
        let content = serde_json::to_string_pretty(&metadata)?;
        fs::write(&metadata_file, content)?;

        Ok(())
    }

    /// Get the command to run for a task.
    fn get_command(&self, task: &Task, config: &ExecutorConfig, vars: &TemplateVars) -> Result<String> {
        // Priority:
        // 1. Task's exec field (if set)
        // 2. Config's args (template-substituted)
        // 3. Error if neither

        if let Some(ref exec) = task.exec {
            return Ok(exec.clone());
        }

        let settings = config.apply_templates(vars);

        // Check if args contains a command (typically after -c)
        if settings.args.len() >= 2 && settings.args[0] == "-c" {
            return Ok(settings.args[1].clone());
        }

        // If args is non-empty, join them
        if !settings.args.is_empty() {
            return Ok(settings.args.join(" "));
        }

        // Check if task_context has content (might be the command)
        if !vars.task_context.is_empty() {
            return Ok(vars.task_context.clone());
        }

        Err(anyhow!(
            "No command specified for shell executor. Set the task's 'exec' field or provide command in config args."
        ))
    }
}

impl Executor for ShellExecutor {
    fn name(&self) -> &str {
        "shell"
    }

    fn spawn(&self, task: &Task, config: &ExecutorConfig, vars: &TemplateVars) -> Result<AgentHandle> {
        // Generate a temporary agent ID for directory purposes
        let temp_agent_id = format!("agent-shell-{}", std::process::id());
        let agent_dir = self.ensure_output_dir(&temp_agent_id)?;

        // Get the command to run
        let command = self.get_command(task, config, vars)?;

        // Write metadata
        self.write_metadata(&agent_dir, task, &command)?;

        // Write the command to a script file
        let script_file = agent_dir.join("script.sh");
        {
            let mut file = File::create(&script_file)?;
            writeln!(file, "#!/bin/bash")?;
            writeln!(file, "# Shell agent script for task: {}", task.id)?;
            writeln!(file)?;
            writeln!(file, "{}", command)?;
        }

        // Build the command
        let settings = config.apply_templates(vars);
        let mut cmd = Command::new(&settings.command);

        // Run the command via -c
        cmd.args(["-c", &command]);

        // Set task information as environment variables
        cmd.env(env_vars::TASK_ID, &vars.task_id);
        cmd.env(env_vars::TASK_TITLE, &vars.task_title);
        cmd.env(env_vars::TASK_DESCRIPTION, &vars.task_description);
        cmd.env(env_vars::TASK_CONTEXT, &vars.task_context);

        if let Some(ref exec) = task.exec {
            cmd.env(env_vars::TASK_EXEC, exec);
        }

        // Set additional environment variables from config
        for (key, value) in &settings.env {
            cmd.env(key, value);
        }

        // Set working directory if specified
        if let Some(ref wd) = settings.working_dir {
            cmd.current_dir(wd);
            cmd.env(env_vars::WORKDIR, wd);
        }

        // Create output log file
        let output_file = agent_dir.join("output.log");
        let log_file = File::create(&output_file)
            .with_context(|| format!("Failed to create output log: {}", output_file.display()))?;

        // Configure stdio - capture stdout/stderr to file
        cmd.stdin(Stdio::null());
        cmd.stdout(log_file.try_clone()?);
        cmd.stderr(log_file);

        // Spawn the process
        let child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn shell command: {}", command))?;

        Ok(AgentHandle::from_child(child))
    }
}

/// Configuration builder for shell executor.
pub struct ShellExecutorConfig {
    /// Shell to use.
    pub shell: String,

    /// Command to run (or use task's exec field).
    pub command: Option<String>,

    /// Environment variables.
    pub env: std::collections::HashMap<String, String>,

    /// Working directory.
    pub working_dir: Option<String>,

    /// Timeout in seconds.
    pub timeout: Option<u64>,
}

impl Default for ShellExecutorConfig {
    fn default() -> Self {
        Self {
            shell: "bash".to_string(),
            command: None,
            env: std::collections::HashMap::new(),
            working_dir: None,
            timeout: None,
        }
    }
}

impl ShellExecutorConfig {
    /// Create a new shell executor config builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the shell to use.
    pub fn shell(mut self, shell: &str) -> Self {
        self.shell = shell.to_string();
        self
    }

    /// Set the command to run.
    pub fn command(mut self, cmd: &str) -> Self {
        self.command = Some(cmd.to_string());
        self
    }

    /// Add an environment variable.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_string(), value.to_string());
        self
    }

    /// Set the working directory.
    pub fn working_dir(mut self, dir: &str) -> Self {
        self.working_dir = Some(dir.to_string());
        self
    }

    /// Set the timeout.
    pub fn timeout(mut self, seconds: u64) -> Self {
        self.timeout = Some(seconds);
        self
    }

    /// Convert to an ExecutorConfig.
    pub fn build(self) -> ExecutorConfig {
        let args = if let Some(ref cmd) = self.command {
            vec!["-c".to_string(), cmd.clone()]
        } else {
            vec!["-c".to_string(), "{{task_context}}".to_string()]
        };

        // Add standard task info to env
        let mut env = self.env;
        env.insert("TASK_ID".to_string(), "{{task_id}}".to_string());
        env.insert("TASK_TITLE".to_string(), "{{task_title}}".to_string());

        ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "shell".to_string(),
                command: self.shell,
                args,
                env,
                prompt_template: None,
                working_dir: self.working_dir,
                timeout: self.timeout,
            },
        }
    }
}

/// Spawn a shell command to work on a task.
///
/// This is a convenience function that creates a ShellExecutor and spawns an agent.
///
/// # Arguments
/// * `workgraph_dir` - Path to the .workgraph directory
/// * `task` - The task to work on
/// * `context` - Optional context from dependencies
///
/// # Returns
/// An AgentHandle for the spawned shell process.
pub fn spawn_shell_agent(
    workgraph_dir: &Path,
    task: &Task,
    context: Option<&str>,
) -> Result<AgentHandle> {
    let executor = ShellExecutor::new(workgraph_dir);
    let config = ShellExecutorConfig::new().build();
    let vars = TemplateVars::from_task(task, context);

    executor.spawn(task, &config, &vars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Status;
    use tempfile::TempDir;

    fn make_test_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: Some("Test description".to_string()),
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
            model: None,
            verify: None,
        }
    }

    #[test]
    fn test_shell_executor_new() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        assert_eq!(executor.name(), "shell");
        assert_eq!(executor.shell, "bash");
    }

    #[test]
    fn test_shell_executor_with_shell() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path()).shell("zsh");

        assert_eq!(executor.shell, "zsh");
    }

    #[test]
    fn test_shell_executor_config_builder() {
        let config = ShellExecutorConfig::new()
            .shell("sh")
            .command("echo hello")
            .env("MY_VAR", "my_value")
            .working_dir("/home/user")
            .timeout(60)
            .build();

        assert_eq!(config.executor.executor_type, "shell");
        assert_eq!(config.executor.command, "sh");
        assert!(config.executor.args.contains(&"-c".to_string()));
        assert!(config.executor.args.contains(&"echo hello".to_string()));
        assert_eq!(config.executor.env.get("MY_VAR"), Some(&"my_value".to_string()));
        assert_eq!(config.executor.working_dir, Some("/home/user".to_string()));
        assert_eq!(config.executor.timeout, Some(60));
    }

    #[test]
    fn test_shell_executor_config_default_uses_task_context() {
        let config = ShellExecutorConfig::new().build();

        assert!(config.executor.args.contains(&"{{task_context}}".to_string()));
    }

    #[test]
    fn test_get_command_from_task_exec() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let mut task = make_test_task("test-task", "Test");
        task.exec = Some("echo 'from exec'".to_string());

        let config = ShellExecutorConfig::new().build();
        let vars = TemplateVars::from_task(&task, None);

        let command = executor.get_command(&task, &config, &vars).unwrap();
        assert_eq!(command, "echo 'from exec'");
    }

    #[test]
    fn test_get_command_from_config_args() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let task = make_test_task("test-task", "Test");
        let config = ShellExecutorConfig::new()
            .command("echo 'from config'")
            .build();
        let vars = TemplateVars::from_task(&task, None);

        let command = executor.get_command(&task, &config, &vars).unwrap();
        assert_eq!(command, "echo 'from config'");
    }

    #[test]
    fn test_get_command_from_context() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let task = make_test_task("test-task", "Test");
        let config = ShellExecutorConfig::new().build();
        let vars = TemplateVars::from_task(&task, Some("echo 'from context'"));

        let command = executor.get_command(&task, &config, &vars).unwrap();
        assert_eq!(command, "echo 'from context'");
    }

    #[test]
    fn test_get_command_priority_exec_over_config() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let mut task = make_test_task("test-task", "Test");
        task.exec = Some("echo 'from exec'".to_string());

        let config = ShellExecutorConfig::new()
            .command("echo 'from config'")
            .build();
        let vars = TemplateVars::from_task(&task, Some("echo 'from context'"));

        // exec should take priority
        let command = executor.get_command(&task, &config, &vars).unwrap();
        assert_eq!(command, "echo 'from exec'");
    }

    #[test]
    fn test_ensure_output_dir() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let agent_dir = executor.ensure_output_dir("agent-1").unwrap();

        assert!(agent_dir.exists());
        assert!(agent_dir.ends_with("agent-1"));
    }

    #[test]
    fn test_write_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let agent_dir = executor.ensure_output_dir("agent-1").unwrap();
        let task = make_test_task("test-task", "Test Task");

        executor.write_metadata(&agent_dir, &task, "echo hello").unwrap();

        let metadata_file = agent_dir.join("metadata.json");
        assert!(metadata_file.exists());

        let content = fs::read_to_string(&metadata_file).unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(metadata["task_id"], "test-task");
        assert_eq!(metadata["executor"], "shell");
        assert_eq!(metadata["command"], "echo hello");
    }

    #[test]
    fn test_spawn_simple_command() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let mut task = make_test_task("test-task", "Test");
        task.exec = Some("echo 'hello world'".to_string());

        let config = ShellExecutorConfig::new().build();
        let vars = TemplateVars::from_task(&task, None);

        let mut handle = executor.spawn(&task, &config, &vars).unwrap();

        // Wait for completion
        let status = handle.wait().unwrap();
        assert!(status.success());
    }

    #[test]
    fn test_spawn_with_env_vars() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let mut task = make_test_task("my-task", "My Task Title");
        task.exec = Some("echo $WG_TASK_ID $WG_TASK_TITLE".to_string());

        let config = ShellExecutorConfig::new().build();
        let vars = TemplateVars::from_task(&task, None);

        let mut handle = executor.spawn(&task, &config, &vars).unwrap();
        let status = handle.wait().unwrap();

        assert!(status.success());
    }

    #[test]
    fn test_spawn_creates_script_file() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ShellExecutor::new(temp_dir.path());

        let mut task = make_test_task("test-task", "Test");
        task.exec = Some("echo 'test'".to_string());

        let config = ShellExecutorConfig::new().build();
        let vars = TemplateVars::from_task(&task, None);

        let mut handle = executor.spawn(&task, &config, &vars).unwrap();
        handle.wait().unwrap();

        // Check that script file was created
        let agent_dir = executor.agent_output_dir(&format!("agent-shell-{}", std::process::id()));
        let script_file = agent_dir.join("script.sh");

        // Note: directory name includes PID, so we need to find it
        let agents_dir = temp_dir.path().join("agents");
        let entries: Vec<_> = fs::read_dir(&agents_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();

        assert!(!entries.is_empty());
        let agent_dir = &entries[0].path();
        assert!(agent_dir.join("script.sh").exists());
        assert!(agent_dir.join("output.log").exists());
        assert!(agent_dir.join("metadata.json").exists());
    }

    #[test]
    fn test_env_var_names() {
        assert_eq!(env_vars::TASK_ID, "WG_TASK_ID");
        assert_eq!(env_vars::TASK_TITLE, "WG_TASK_TITLE");
        assert_eq!(env_vars::TASK_DESCRIPTION, "WG_TASK_DESCRIPTION");
        assert_eq!(env_vars::TASK_CONTEXT, "WG_TASK_CONTEXT");
        assert_eq!(env_vars::TASK_EXEC, "WG_TASK_EXEC");
        assert_eq!(env_vars::WORKDIR, "WG_WORKDIR");
    }
}
