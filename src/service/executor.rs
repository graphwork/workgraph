//! Executor plugin system for spawning and managing agents.
//!
//! Executors define how to run agents. Each executor can spawn processes
//! that work on tasks, with configurable commands, arguments, and environment.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

use crate::graph::Task;

/// Template variables that can be used in executor configurations.
#[derive(Debug, Clone)]
pub struct TemplateVars {
    pub task_id: String,
    pub task_title: String,
    pub task_description: String,
    pub task_context: String,
}

impl TemplateVars {
    /// Create template variables from a task and optional context.
    pub fn from_task(task: &Task, context: Option<&str>) -> Self {
        Self {
            task_id: task.id.clone(),
            task_title: task.title.clone(),
            task_description: task.description.clone().unwrap_or_default(),
            task_context: context.unwrap_or_default().to_string(),
        }
    }

    /// Apply template substitution to a string.
    pub fn apply(&self, template: &str) -> String {
        template
            .replace("{{task_id}}", &self.task_id)
            .replace("{{task_title}}", &self.task_title)
            .replace("{{task_description}}", &self.task_description)
            .replace("{{task_context}}", &self.task_context)
    }
}

/// Configuration for an executor, loaded from `.workgraph/executors/<name>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorConfig {
    /// The executor configuration section.
    pub executor: ExecutorSettings,
}

/// Settings within an executor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorSettings {
    /// Type of executor: "claude", "shell", "custom".
    #[serde(rename = "type")]
    pub executor_type: String,

    /// Command to execute.
    pub command: String,

    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Prompt template configuration (optional).
    #[serde(default)]
    pub prompt_template: Option<PromptTemplate>,

    /// Working directory for the executor (optional).
    #[serde(default)]
    pub working_dir: Option<String>,

    /// Timeout in seconds (optional).
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Prompt template for injecting task context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptTemplate {
    /// The template string with placeholders.
    #[serde(default)]
    pub template: String,
}

impl ExecutorConfig {
    /// Load executor configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read executor config: {}", path.display()))?;

        let config: ExecutorConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse executor config: {}", path.display()))?;

        Ok(config)
    }

    /// Load executor configuration from the workgraph executors directory.
    pub fn load_by_name(workgraph_dir: &Path, name: &str) -> Result<Self> {
        let config_path = workgraph_dir.join("executors").join(format!("{}.toml", name));

        if !config_path.exists() {
            return Err(anyhow!(
                "Executor config not found: {}. Create it at {}",
                name,
                config_path.display()
            ));
        }

        Self::load(&config_path)
    }

    /// Apply template variables to all configurable fields.
    pub fn apply_templates(&self, vars: &TemplateVars) -> ExecutorSettings {
        let mut settings = self.executor.clone();

        // Apply to command
        settings.command = vars.apply(&settings.command);

        // Apply to args
        settings.args = settings.args.iter().map(|arg| vars.apply(arg)).collect();

        // Apply to env vars
        settings.env = settings
            .env
            .iter()
            .map(|(k, v)| (k.clone(), vars.apply(v)))
            .collect();

        // Apply to prompt template
        if let Some(ref mut pt) = settings.prompt_template {
            pt.template = vars.apply(&pt.template);
        }

        // Apply to working dir
        if let Some(ref wd) = settings.working_dir {
            settings.working_dir = Some(vars.apply(wd));
        }

        settings
    }
}

/// Handle to a spawned agent process.
pub struct AgentHandle {
    /// Process ID of the agent.
    pub pid: u32,

    /// Handle to the child process.
    child: Child,

    /// Standard input handle (if piped).
    stdin: Option<ChildStdin>,

    /// Standard output handle (if piped).
    stdout: Option<ChildStdout>,
}

impl AgentHandle {
    /// Create a new agent handle from a child process.
    fn new(mut child: Child) -> Self {
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();

        Self {
            pid,
            child,
            stdin,
            stdout,
        }
    }

    /// Create an agent handle from a child process (public alias for new).
    pub fn from_child(child: Child) -> Self {
        Self::new(child)
    }

    /// Get mutable access to stdin for writing input to the agent.
    pub fn stdin(&mut self) -> Option<&mut ChildStdin> {
        self.stdin.as_mut()
    }

    /// Get mutable access to stdout for reading output from the agent.
    pub fn stdout(&mut self) -> Option<&mut ChildStdout> {
        self.stdout.as_mut()
    }

    /// Take ownership of stdin.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// Take ownership of stdout.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// Read a line from stdout (blocking).
    pub fn read_line(&mut self) -> Result<Option<String>> {
        if let Some(ref mut stdout) = self.stdout {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => Ok(None), // EOF
                Ok(_) => Ok(Some(line)),
                Err(e) => Err(anyhow!("Failed to read from agent: {}", e)),
            }
        } else {
            Err(anyhow!("Stdout not available"))
        }
    }

    /// Write to stdin.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref mut stdin) = self.stdin {
            stdin.write_all(data)?;
            stdin.flush()?;
            Ok(())
        } else {
            Err(anyhow!("Stdin not available"))
        }
    }

    /// Check if the process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait for the agent process to complete.
    pub fn wait(&mut self) -> Result<ExitStatus> {
        self.child
            .wait()
            .context("Failed to wait for agent process")
    }

    /// Try to wait for the process without blocking.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .try_wait()
            .context("Failed to check agent process status")
    }

    /// Send SIGTERM to the process (graceful shutdown).
    /// On Unix, this sends SIGTERM. On Windows, this falls back to kill.
    #[cfg(unix)]
    pub fn terminate(&mut self) -> Result<()> {
        use std::process::Command;

        // Use kill command to send SIGTERM
        let status = Command::new("kill")
            .args(["-TERM", &self.pid.to_string()])
            .status()
            .with_context(|| format!("Failed to send SIGTERM to process {}", self.pid))?;

        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("Failed to send SIGTERM to process {}", self.pid))
        }
    }

    /// Terminate the process (Windows version - just kills it).
    #[cfg(not(unix))]
    pub fn terminate(&mut self) -> Result<()> {
        self.kill()
    }

    /// Forcefully kill the process.
    pub fn kill(&mut self) -> Result<()> {
        self.child
            .kill()
            .with_context(|| format!("Failed to kill agent process {}", self.pid))
    }
}

/// Trait for executor plugins.
pub trait Executor: Send + Sync {
    /// Get the name of this executor.
    fn name(&self) -> &str;

    /// Spawn an agent to work on a task.
    fn spawn(&self, task: &Task, config: &ExecutorConfig, vars: &TemplateVars) -> Result<AgentHandle>;
}

/// Default executor implementation that runs shell commands.
pub struct DefaultExecutor;

impl DefaultExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DefaultExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for DefaultExecutor {
    fn name(&self) -> &str {
        "default"
    }

    fn spawn(&self, _task: &Task, config: &ExecutorConfig, vars: &TemplateVars) -> Result<AgentHandle> {
        let settings = config.apply_templates(vars);

        let mut cmd = Command::new(&settings.command);

        // Add arguments
        cmd.args(&settings.args);

        // Set environment variables
        for (key, value) in &settings.env {
            cmd.env(key, value);
        }

        // Set working directory if specified
        if let Some(ref wd) = settings.working_dir {
            cmd.current_dir(wd);
        }

        // Configure stdio
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn executor command: {}", settings.command))?;

        Ok(AgentHandle::new(child))
    }
}

/// Registry of available executors.
pub struct ExecutorRegistry {
    executors: HashMap<String, Box<dyn Executor>>,
    config_dir: PathBuf,
}

impl ExecutorRegistry {
    /// Create a new executor registry.
    pub fn new(workgraph_dir: &Path) -> Self {
        let mut registry = Self {
            executors: HashMap::new(),
            config_dir: workgraph_dir.join("executors"),
        };

        // Register the default executor
        registry.register(Box::new(DefaultExecutor::new()));

        registry
    }

    /// Register an executor.
    pub fn register(&mut self, executor: Box<dyn Executor>) {
        self.executors.insert(executor.name().to_string(), executor);
    }

    /// Get an executor by name.
    pub fn get(&self, name: &str) -> Option<&dyn Executor> {
        self.executors.get(name).map(|e| e.as_ref())
    }

    /// List available executor names.
    pub fn available(&self) -> Vec<&str> {
        self.executors.keys().map(|s| s.as_str()).collect()
    }

    /// Load executor config by name.
    pub fn load_config(&self, name: &str) -> Result<ExecutorConfig> {
        let config_path = self.config_dir.join(format!("{}.toml", name));

        if config_path.exists() {
            ExecutorConfig::load(&config_path)
        } else {
            // Return a default config for built-in executors
            self.default_config(name)
        }
    }

    /// Get default config for built-in executors.
    fn default_config(&self, name: &str) -> Result<ExecutorConfig> {
        match name {
            "claude" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "claude".to_string(),
                    command: "claude".to_string(),
                    args: vec![
                        "--permission-mode".to_string(),
                        "bypassPermissions".to_string(),
                    ],
                    env: HashMap::new(),
                    prompt_template: Some(PromptTemplate {
                        template: r#"You are working on task: {{task_id}}
Title: {{task_title}}
Description: {{task_description}}

Context from dependencies:
{{task_context}}

When done, run: wg done {{task_id}}
If blocked, run: wg fail {{task_id}} --reason "...""#.to_string(),
                    }),
                    working_dir: None,
                    timeout: None,
                },
            }),
            "shell" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "shell".to_string(),
                    command: "bash".to_string(),
                    args: vec!["-c".to_string(), "{{task_context}}".to_string()],
                    env: {
                        let mut env = HashMap::new();
                        env.insert("TASK_ID".to_string(), "{{task_id}}".to_string());
                        env.insert("TASK_TITLE".to_string(), "{{task_title}}".to_string());
                        env
                    },
                    prompt_template: None,
                    working_dir: None,
                    timeout: None,
                },
            }),
            "default" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "default".to_string(),
                    command: "echo".to_string(),
                    args: vec!["Task: {{task_id}}".to_string()],
                    env: HashMap::new(),
                    prompt_template: None,
                    working_dir: None,
                    timeout: None,
                },
            }),
            _ => Err(anyhow!(
                "Unknown executor '{}'. Available: {:?}",
                name,
                self.available()
            )),
        }
    }

    /// Spawn an agent for a task using the specified executor.
    pub fn spawn(
        &self,
        executor_name: &str,
        task: &Task,
        context: Option<&str>,
    ) -> Result<AgentHandle> {
        // Use default executor if the named one isn't registered
        let executor = self
            .get(executor_name)
            .or_else(|| self.get("default"))
            .ok_or_else(|| anyhow!("No executor available"))?;

        let config = self.load_config(executor_name)?;
        let vars = TemplateVars::from_task(task, context);

        executor.spawn(task, &config, &vars)
    }

    /// Ensure the executors directory exists and has default configs.
    pub fn init(&self) -> Result<()> {
        if !self.config_dir.exists() {
            fs::create_dir_all(&self.config_dir)
                .with_context(|| format!("Failed to create executors directory: {}", self.config_dir.display()))?;
        }

        // Create default executor configs if they don't exist
        for name in ["claude", "shell"] {
            let config_path = self.config_dir.join(format!("{}.toml", name));
            if !config_path.exists() {
                let config = self.default_config(name)?;
                let content = toml::to_string_pretty(&config)
                    .with_context(|| format!("Failed to serialize {} config", name))?;
                fs::write(&config_path, content)
                    .with_context(|| format!("Failed to write {} config", name))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_test_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: Some("Test description".to_string()),
            status: crate::graph::Status::Open,
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
    fn test_template_vars_apply() {
        let task = make_test_task("task-123", "Implement feature");
        let vars = TemplateVars::from_task(&task, Some("Context from deps"));

        let template = "Working on {{task_id}}: {{task_title}}. Context: {{task_context}}";
        let result = vars.apply(template);

        assert_eq!(
            result,
            "Working on task-123: Implement feature. Context: Context from deps"
        );
    }

    #[test]
    fn test_template_vars_from_task() {
        let task = make_test_task("my-task", "My Title");
        let vars = TemplateVars::from_task(&task, None);

        assert_eq!(vars.task_id, "my-task");
        assert_eq!(vars.task_title, "My Title");
        assert_eq!(vars.task_description, "Test description");
        assert_eq!(vars.task_context, "");
    }

    #[test]
    fn test_executor_config_load() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("test.toml");

        let config_content = r#"
[executor]
type = "custom"
command = "my-agent"
args = ["--task", "{{task_id}}"]

[executor.env]
TASK_TITLE = "{{task_title}}"

[executor.prompt_template]
template = "Work on {{task_id}}"
"#;
        fs::write(&config_path, config_content).unwrap();

        let config = ExecutorConfig::load(&config_path).unwrap();
        assert_eq!(config.executor.executor_type, "custom");
        assert_eq!(config.executor.command, "my-agent");
        assert_eq!(config.executor.args, vec!["--task", "{{task_id}}"]);
    }

    #[test]
    fn test_executor_config_apply_templates() {
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "test".to_string(),
                command: "run-{{task_id}}".to_string(),
                args: vec!["--title".to_string(), "{{task_title}}".to_string()],
                env: {
                    let mut env = HashMap::new();
                    env.insert("TASK".to_string(), "{{task_id}}".to_string());
                    env
                },
                prompt_template: Some(PromptTemplate {
                    template: "Context: {{task_context}}".to_string(),
                }),
                working_dir: Some("/work/{{task_id}}".to_string()),
                timeout: None,
            },
        };

        let task = make_test_task("t-1", "Test Task");
        let vars = TemplateVars::from_task(&task, Some("dep context"));
        let settings = config.apply_templates(&vars);

        assert_eq!(settings.command, "run-t-1");
        assert_eq!(settings.args, vec!["--title", "Test Task"]);
        assert_eq!(settings.env.get("TASK"), Some(&"t-1".to_string()));
        assert_eq!(
            settings.prompt_template.unwrap().template,
            "Context: dep context"
        );
        assert_eq!(settings.working_dir, Some("/work/t-1".to_string()));
    }

    #[test]
    fn test_executor_registry_new() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());

        // Default executor should be registered
        assert!(registry.get("default").is_some());
        assert!(registry.available().contains(&"default"));
    }

    #[test]
    fn test_executor_registry_default_configs() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());

        // Should return default configs for built-in executors
        let claude_config = registry.load_config("claude").unwrap();
        assert_eq!(claude_config.executor.executor_type, "claude");
        assert_eq!(claude_config.executor.command, "claude");

        let shell_config = registry.load_config("shell").unwrap();
        assert_eq!(shell_config.executor.executor_type, "shell");
        assert_eq!(shell_config.executor.command, "bash");
    }

    #[test]
    fn test_executor_registry_init() {
        let temp_dir = TempDir::new().unwrap();
        let workgraph_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&workgraph_dir).unwrap();

        let registry = ExecutorRegistry::new(&workgraph_dir);
        registry.init().unwrap();

        // Should create executor configs
        assert!(workgraph_dir.join("executors/claude.toml").exists());
        assert!(workgraph_dir.join("executors/shell.toml").exists());
    }

    #[test]
    fn test_default_executor_spawn_echo() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());

        let task = make_test_task("test-task", "Test");
        let config = registry.load_config("default").unwrap();
        let vars = TemplateVars::from_task(&task, None);

        let executor = DefaultExecutor::new();
        let mut handle = executor.spawn(&task, &config, &vars).unwrap();

        // The echo command should complete quickly
        let status = handle.wait().unwrap();
        assert!(status.success());
    }

    #[test]
    fn test_agent_handle_is_running() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());

        let task = make_test_task("test-task", "Test");

        // Use a command that runs briefly
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "test".to_string(),
                command: "sleep".to_string(),
                args: vec!["0.1".to_string()],
                env: HashMap::new(),
                prompt_template: None,
                working_dir: None,
                timeout: None,
            },
        };

        let vars = TemplateVars::from_task(&task, None);
        let executor = DefaultExecutor::new();
        let mut handle = executor.spawn(&task, &config, &vars).unwrap();

        // Should be running initially
        assert!(handle.is_running());

        // Wait for completion
        handle.wait().unwrap();

        // Should no longer be running
        assert!(!handle.is_running());
    }
}
