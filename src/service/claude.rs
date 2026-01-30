//! Claude executor for spawning Claude Code agents.
//!
//! This executor spawns Claude Code (the `claude` CLI) to work on tasks.
//! It builds a prompt from the task information and context, then runs
//! Claude in non-interactive mode.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::graph::Task;

use super::executor::{AgentHandle, Executor, ExecutorConfig, ExecutorSettings, PromptTemplate, TemplateVars};

/// Default prompt template for Claude executor.
pub const DEFAULT_CLAUDE_PROMPT: &str = r#"You are agent-executor working on the workgraph agent service. Your trajectory is:

1. {{task_id}} (CLAIMED - start here)

## Current Task: {{task_id}}

{{task_title}}

{{task_description}}

## Context from Dependencies

{{task_context}}

## Workflow
After completing the task:
1. `wg artifact {{task_id}} <file>` for any files created
2. `wg done {{task_id}}`
3. Check `wg ready` for your next task

Run tests with `cargo test` after implementation if applicable. Commit after task completion.
"#;

/// Claude executor - spawns Claude Code to work on tasks.
pub struct ClaudeExecutor {
    /// Model to use (e.g., "opus-4", "sonnet", "haiku").
    model: Option<String>,

    /// Whether to skip permission prompts.
    skip_permissions: bool,

    /// Output directory for agent logs.
    output_dir: PathBuf,
}

impl ClaudeExecutor {
    /// Create a new Claude executor with default settings.
    pub fn new(workgraph_dir: &Path) -> Self {
        Self {
            model: None,
            skip_permissions: true,
            output_dir: workgraph_dir.join("agents"),
        }
    }

    /// Create a Claude executor with a specific model.
    pub fn with_model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    /// Set whether to skip permission prompts.
    pub fn skip_permissions(mut self, skip: bool) -> Self {
        self.skip_permissions = skip;
        self
    }

    /// Set the output directory for agent logs.
    pub fn output_dir(mut self, dir: PathBuf) -> Self {
        self.output_dir = dir;
        self
    }

    /// Build the prompt for a task.
    pub fn build_prompt(&self, _task: &Task, config: &ExecutorConfig, vars: &TemplateVars) -> String {
        // Use custom prompt template if provided, otherwise use default
        if let Some(ref pt) = config.executor.prompt_template {
            if !pt.template.is_empty() {
                return vars.apply(&pt.template);
            }
        }

        // Use default prompt template
        vars.apply(DEFAULT_CLAUDE_PROMPT)
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

    /// Write the prompt to a file for the agent.
    fn write_prompt_file(&self, agent_dir: &Path, prompt: &str) -> Result<PathBuf> {
        let prompt_file = agent_dir.join("prompt.txt");
        let mut file = File::create(&prompt_file)
            .with_context(|| format!("Failed to create prompt file: {}", prompt_file.display()))?;
        file.write_all(prompt.as_bytes())?;
        Ok(prompt_file)
    }

    /// Create a metadata file for the agent.
    fn write_metadata(&self, agent_dir: &Path, task: &Task, config: &ExecutorConfig) -> Result<()> {
        let metadata = serde_json::json!({
            "task_id": task.id,
            "task_title": task.title,
            "executor": "claude",
            "model": self.model,
            "started_at": chrono::Utc::now().to_rfc3339(),
            "config": {
                "command": config.executor.command,
                "args": config.executor.args,
            }
        });

        let metadata_file = agent_dir.join("metadata.json");
        let content = serde_json::to_string_pretty(&metadata)?;
        fs::write(&metadata_file, content)?;

        Ok(())
    }
}

impl Executor for ClaudeExecutor {
    fn name(&self) -> &str {
        "claude"
    }

    fn spawn(&self, task: &Task, config: &ExecutorConfig, vars: &TemplateVars) -> Result<AgentHandle> {
        // Generate a temporary agent ID for directory purposes
        let temp_agent_id = format!("agent-tmp-{}", std::process::id());
        let agent_dir = self.ensure_output_dir(&temp_agent_id)?;

        // Build the prompt
        let prompt = self.build_prompt(task, config, vars);

        // Write prompt to file
        let _prompt_file = self.write_prompt_file(&agent_dir, &prompt)?;

        // Write metadata
        self.write_metadata(&agent_dir, task, config)?;

        // Build the command - wrap with stdbuf to force line buffering
        // (otherwise output is block-buffered when stdout is not a TTY)
        let settings = config.apply_templates(vars);
        let mut cmd = Command::new("stdbuf");
        cmd.args(["-oL", "-eL", &settings.command]);

        // Add model if specified
        if let Some(ref model) = self.model {
            cmd.args(["--model", model]);
        }

        // Add standard arguments
        cmd.arg("--print");

        if self.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }

        // Add the prompt as the final argument
        cmd.arg(&prompt);

        // Set environment variables from config
        for (key, value) in &settings.env {
            cmd.env(key, value);
        }

        // Set working directory if specified
        if let Some(ref wd) = settings.working_dir {
            cmd.current_dir(wd);
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
            .with_context(|| format!("Failed to spawn claude command: {}", settings.command))?;

        Ok(AgentHandle::from_child(child))
    }
}

/// Configuration builder for Claude executor.
pub struct ClaudeExecutorConfig {
    /// Model to use.
    pub model: Option<String>,

    /// Whether to skip permission prompts.
    pub skip_permissions: bool,

    /// Custom prompt template.
    pub prompt_template: Option<String>,

    /// Environment variables.
    pub env: std::collections::HashMap<String, String>,

    /// Working directory.
    pub working_dir: Option<String>,

    /// Timeout in seconds.
    pub timeout: Option<u64>,
}

impl Default for ClaudeExecutorConfig {
    fn default() -> Self {
        Self {
            model: None,
            skip_permissions: true,
            prompt_template: None,
            env: std::collections::HashMap::new(),
            working_dir: None,
            timeout: None,
        }
    }
}

impl ClaudeExecutorConfig {
    /// Create a new Claude executor config builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the model.
    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    /// Set whether to skip permissions.
    pub fn skip_permissions(mut self, skip: bool) -> Self {
        self.skip_permissions = skip;
        self
    }

    /// Set a custom prompt template.
    pub fn prompt_template(mut self, template: &str) -> Self {
        self.prompt_template = Some(template.to_string());
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
        let mut args = vec!["--print".to_string()];

        if self.skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        }

        if let Some(ref model) = self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }

        ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "claude".to_string(),
                command: "claude".to_string(),
                args,
                env: self.env,
                prompt_template: self.prompt_template.map(|t| PromptTemplate { template: t }),
                working_dir: self.working_dir,
                timeout: self.timeout,
            },
        }
    }
}

/// Spawn Claude Code to work on a task.
///
/// This is a convenience function that creates a ClaudeExecutor and spawns an agent.
///
/// # Arguments
/// * `workgraph_dir` - Path to the .workgraph directory
/// * `task` - The task to work on
/// * `context` - Optional context from dependencies
/// * `model` - Optional model to use
///
/// # Returns
/// An AgentHandle for the spawned Claude process.
pub fn spawn_claude_agent(
    workgraph_dir: &Path,
    task: &Task,
    context: Option<&str>,
    model: Option<&str>,
) -> Result<AgentHandle> {
    let mut executor = ClaudeExecutor::new(workgraph_dir);

    if let Some(m) = model {
        executor = executor.with_model(m);
    }

    let config = ClaudeExecutorConfig::new()
        .skip_permissions(true)
        .build();

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
        }
    }

    #[test]
    fn test_claude_executor_new() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ClaudeExecutor::new(temp_dir.path());

        assert_eq!(executor.name(), "claude");
        assert!(executor.model.is_none());
        assert!(executor.skip_permissions);
    }

    #[test]
    fn test_claude_executor_with_model() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ClaudeExecutor::new(temp_dir.path()).with_model("opus-4");

        assert_eq!(executor.model, Some("opus-4".to_string()));
    }

    #[test]
    fn test_claude_executor_build_prompt_default() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ClaudeExecutor::new(temp_dir.path());

        let task = make_test_task("implement-feature", "Implement feature X");
        let vars = TemplateVars::from_task(&task, Some("Context from deps"));

        let config = ClaudeExecutorConfig::new().build();
        let prompt = executor.build_prompt(&task, &config, &vars);

        assert!(prompt.contains("implement-feature"));
        assert!(prompt.contains("Implement feature X"));
        assert!(prompt.contains("Context from deps"));
        assert!(prompt.contains("wg done"));
    }

    #[test]
    fn test_claude_executor_build_prompt_custom() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ClaudeExecutor::new(temp_dir.path());

        let task = make_test_task("my-task", "My Task");
        let vars = TemplateVars::from_task(&task, None);

        let config = ClaudeExecutorConfig::new()
            .prompt_template("Custom prompt for {{task_id}}: {{task_title}}")
            .build();

        let prompt = executor.build_prompt(&task, &config, &vars);

        assert_eq!(prompt, "Custom prompt for my-task: My Task");
    }

    #[test]
    fn test_claude_executor_config_builder() {
        let config = ClaudeExecutorConfig::new()
            .model("sonnet")
            .skip_permissions(true)
            .env("MY_VAR", "my_value")
            .working_dir("/home/user/project")
            .timeout(3600)
            .build();

        assert_eq!(config.executor.executor_type, "claude");
        assert_eq!(config.executor.command, "claude");
        assert!(config.executor.args.contains(&"--model".to_string()));
        assert!(config.executor.args.contains(&"sonnet".to_string()));
        assert!(config.executor.args.contains(&"--print".to_string()));
        assert!(config.executor.args.contains(&"--dangerously-skip-permissions".to_string()));
        assert_eq!(config.executor.env.get("MY_VAR"), Some(&"my_value".to_string()));
        assert_eq!(config.executor.working_dir, Some("/home/user/project".to_string()));
        assert_eq!(config.executor.timeout, Some(3600));
    }

    #[test]
    fn test_claude_executor_config_no_skip_permissions() {
        let config = ClaudeExecutorConfig::new()
            .skip_permissions(false)
            .build();

        assert!(!config.executor.args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn test_ensure_output_dir() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ClaudeExecutor::new(temp_dir.path());

        let agent_dir = executor.ensure_output_dir("agent-1").unwrap();

        assert!(agent_dir.exists());
        assert!(agent_dir.ends_with("agent-1"));
    }

    #[test]
    fn test_write_prompt_file() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ClaudeExecutor::new(temp_dir.path());

        let agent_dir = executor.ensure_output_dir("agent-1").unwrap();
        let prompt = "Test prompt content";

        let prompt_file = executor.write_prompt_file(&agent_dir, prompt).unwrap();

        assert!(prompt_file.exists());
        let content = fs::read_to_string(&prompt_file).unwrap();
        assert_eq!(content, prompt);
    }

    #[test]
    fn test_write_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let executor = ClaudeExecutor::new(temp_dir.path());

        let agent_dir = executor.ensure_output_dir("agent-1").unwrap();
        let task = make_test_task("test-task", "Test Task");
        let config = ClaudeExecutorConfig::new().build();

        executor.write_metadata(&agent_dir, &task, &config).unwrap();

        let metadata_file = agent_dir.join("metadata.json");
        assert!(metadata_file.exists());

        let content = fs::read_to_string(&metadata_file).unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(metadata["task_id"], "test-task");
        assert_eq!(metadata["task_title"], "Test Task");
        assert_eq!(metadata["executor"], "claude");
    }

    #[test]
    fn test_default_claude_prompt_contains_workflow() {
        assert!(DEFAULT_CLAUDE_PROMPT.contains("wg artifact"));
        assert!(DEFAULT_CLAUDE_PROMPT.contains("wg done"));
        assert!(DEFAULT_CLAUDE_PROMPT.contains("wg ready"));
        assert!(DEFAULT_CLAUDE_PROMPT.contains("{{task_id}}"));
        assert!(DEFAULT_CLAUDE_PROMPT.contains("{{task_title}}"));
        assert!(DEFAULT_CLAUDE_PROMPT.contains("{{task_context}}"));
    }
}
