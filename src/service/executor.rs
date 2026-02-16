//! Executor configuration system for spawning agents.
//!
//! Provides configuration loading and template variable substitution for
//! executor configs stored in `.workgraph/executors/<name>.toml`.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agency;
use crate::graph::Task;

/// Template variables that can be used in executor configurations.
#[derive(Debug, Clone)]
pub struct TemplateVars {
    pub task_id: String,
    pub task_title: String,
    pub task_description: String,
    pub task_context: String,
    pub task_identity: String,
    pub working_dir: String,
    pub skills_preamble: String,
}

impl TemplateVars {
    /// Create template variables from a task, optional context, and optional workgraph directory.
    ///
    /// If the task has an agent set and `workgraph_dir` is provided, the Agent is loaded
    /// by hash and its role and motivation are resolved from agency storage and rendered
    /// into an identity prompt. If resolution fails or no agent is set, `task_identity`
    /// is empty (backward compatible).
    pub fn from_task(task: &Task, context: Option<&str>, workgraph_dir: Option<&Path>) -> Self {
        let task_identity = Self::resolve_identity(task, workgraph_dir);

        let working_dir = workgraph_dir
            .and_then(|d| {
                // Canonicalize to resolve relative paths like ".workgraph"
                // whose parent() would be "" instead of the actual directory.
                let abs = d.canonicalize().ok()?;
                abs.parent().map(|p| p.to_string_lossy().to_string())
            })
            .unwrap_or_default();

        let skills_preamble = Self::resolve_skills_preamble(workgraph_dir);

        Self {
            task_id: task.id.clone(),
            task_title: task.title.clone(),
            task_description: task.description.clone().unwrap_or_default(),
            task_context: context.unwrap_or_default().to_string(),
            task_identity,
            working_dir,
            skills_preamble,
        }
    }

    /// Resolve the identity prompt for a task by looking up its Agent, then the
    /// Agent's role and motivation.
    fn resolve_identity(task: &Task, workgraph_dir: Option<&Path>) -> String {
        let agent_hash = match &task.agent {
            Some(h) => h,
            None => return String::new(),
        };

        let wg_dir = match workgraph_dir {
            Some(dir) => dir,
            None => return String::new(),
        };

        let agency_dir = wg_dir.join("agency");
        let agents_dir = agency_dir.join("agents");
        let roles_dir = agency_dir.join("roles");
        let motivations_dir = agency_dir.join("motivations");

        // Look up the Agent entity by hash
        let agent = match agency::find_agent_by_prefix(&agents_dir, agent_hash) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("Warning: could not resolve agent '{}': {}", agent_hash, e);
                return String::new();
            }
        };

        let role = match agency::find_role_by_prefix(&roles_dir, &agent.role_id) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "Warning: could not resolve role '{}' for agent '{}': {}",
                    agent.role_id, agent_hash, e
                );
                return String::new();
            }
        };

        let motivation =
            match agency::find_motivation_by_prefix(&motivations_dir, &agent.motivation_id) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "Warning: could not resolve motivation '{}' for agent '{}': {}",
                        agent.motivation_id, agent_hash, e
                    );
                    return String::new();
                }
            };

        // Resolve skills from the role, using the project root (parent of .workgraph/)
        let workgraph_root = wg_dir.parent().unwrap_or(wg_dir);
        let resolved_skills = agency::resolve_all_skills(&role, workgraph_root);

        agency::render_identity_prompt(&role, &motivation, &resolved_skills)
    }

    /// Read skills preamble from project-level `.claude/skills/` directory.
    ///
    /// If `using-superpowers/SKILL.md` exists, its content is included so that
    /// agents spawned via `--print` (which don't trigger SessionStart hooks)
    /// still get the skill-invocation discipline.
    fn resolve_skills_preamble(workgraph_dir: Option<&Path>) -> String {
        let project_root = match workgraph_dir.and_then(|d| {
            // Canonicalize to handle relative paths like ".workgraph"
            d.canonicalize()
                .ok()
                .and_then(|abs| abs.parent().map(|p| p.to_path_buf()))
        }) {
            Some(r) => r,
            None => return String::new(),
        };

        let skill_path = project_root
            .join(".claude")
            .join("skills")
            .join("using-superpowers")
            .join("SKILL.md");

        match std::fs::read_to_string(&skill_path) {
            Ok(content) => {
                // Strip YAML frontmatter if present
                let body = if content.starts_with("---") {
                    // splitn(3, "---") on "---\nfoo: bar\n---\nbody" gives ["", "\nfoo: bar\n", "\nbody"]
                    // If there's no closing ---, nth(2) is None; skip past the first line instead.
                    content
                        .splitn(3, "---")
                        .nth(2)
                        .unwrap_or_else(|| {
                            // Malformed frontmatter (no closing ---): skip the opening --- line
                            content
                                .strip_prefix("---")
                                .and_then(|s| s.split_once('\n').map(|(_, rest)| rest))
                                .unwrap_or("")
                        })
                        .trim()
                } else {
                    content.trim()
                };
                format!(
                    "<EXTREMELY_IMPORTANT>\nYou have superpowers.\n\n\
                     Below is your introduction to using skills. \
                     For all other skills, use the Skill tool:\n\n\
                     {}\n</EXTREMELY_IMPORTANT>\n",
                    body
                )
            }
            Err(_) => String::new(),
        }
    }

    /// Apply template substitution to a string.
    pub fn apply(&self, template: &str) -> String {
        template
            .replace("{{task_id}}", &self.task_id)
            .replace("{{task_title}}", &self.task_title)
            .replace("{{task_description}}", &self.task_description)
            .replace("{{task_context}}", &self.task_context)
            .replace("{{task_identity}}", &self.task_identity)
            .replace("{{working_dir}}", &self.working_dir)
            .replace("{{skills_preamble}}", &self.skills_preamble)
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
        let config_path = workgraph_dir
            .join("executors")
            .join(format!("{}.toml", name));

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

/// Registry for loading executor configurations.
pub struct ExecutorRegistry {
    config_dir: PathBuf,
}

impl ExecutorRegistry {
    /// Create a new executor registry.
    pub fn new(workgraph_dir: &Path) -> Self {
        Self {
            config_dir: workgraph_dir.join("executors"),
        }
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
                        "--print".to_string(),
                        "--verbose".to_string(),
                        "--permission-mode".to_string(),
                        "bypassPermissions".to_string(),
                        "--output-format".to_string(),
                        "stream-json".to_string(),
                    ],
                    env: HashMap::new(),
                    prompt_template: Some(PromptTemplate {
                        template: r#"{{skills_preamble}}# Task Assignment

You are an AI agent working on a task in a workgraph project.

{{task_identity}}
## Your Task
- **ID:** {{task_id}}
- **Title:** {{task_title}}
- **Description:** {{task_description}}

## Context from Dependencies
{{task_context}}

## Required Workflow

You MUST use these commands to track your work:

1. **Log progress** as you work (helps recovery if interrupted):
   ```bash
   wg log {{task_id}} "Starting implementation..."
   wg log {{task_id}} "Completed X, now working on Y"
   ```

2. **Record artifacts** if you create/modify files:
   ```bash
   wg artifact {{task_id}} path/to/file
   ```

3. **Complete the task** when done:
   ```bash
   wg done {{task_id}}
   ```

4. **Mark as failed** if you cannot complete:
   ```bash
   wg fail {{task_id}} --reason "Specific reason why"
   ```

## Important
- Run `wg log` commands BEFORE doing work to track progress
- Run `wg done` BEFORE you finish responding
- If the task description is unclear, do your best interpretation
- Focus only on this specific task

## CRITICAL: Use wg CLI, NOT built-in tools
- You MUST use `wg` CLI commands for ALL task management
- NEVER use built-in TaskCreate, TaskUpdate, TaskList, or TaskGet tools — they are a completely separate system that does NOT interact with workgraph
- If you need to create subtasks: `wg add "title" --blocked-by {{task_id}}`
- To check task status: `wg show <task-id>`
- To list tasks: `wg list`

Begin working on the task now."#.to_string(),
                    }),
                    working_dir: Some("{{working_dir}}".to_string()),
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
                "Unknown executor '{}'. Available: claude, shell, default",
                name,
            )),
        }
    }

    /// Ensure the executors directory exists and has default configs.
    #[cfg(test)]
    pub fn init(&self) -> Result<()> {
        if !self.config_dir.exists() {
            fs::create_dir_all(&self.config_dir).with_context(|| {
                format!(
                    "Failed to create executors directory: {}",
                    self.config_dir.display()
                )
            })?;
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
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
            paused: false,
        }
    }

    #[test]
    fn test_template_vars_apply() {
        let task = make_test_task("task-123", "Implement feature");
        let vars = TemplateVars::from_task(&task, Some("Context from deps"), None);

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
        let vars = TemplateVars::from_task(&task, None, None);

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
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
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
    fn test_template_vars_no_identity_when_none() {
        let task = make_test_task("task-1", "Test Task");
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_identity, "");
    }

    #[test]
    fn test_template_vars_no_identity_when_no_workgraph_dir() {
        let mut task = make_test_task("task-1", "Test Task");
        task.agent = Some("some-agent-hash".to_string());
        // No workgraph_dir provided, so identity should be empty
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_identity, "");
    }

    #[test]
    fn test_template_vars_identity_resolved_from_agency() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        let roles_dir = wg_dir.join("agency").join("roles");
        let motivations_dir = wg_dir.join("agency").join("motivations");
        let agents_dir = wg_dir.join("agency").join("agents");
        fs::create_dir_all(&roles_dir).unwrap();
        fs::create_dir_all(&motivations_dir).unwrap();
        fs::create_dir_all(&agents_dir).unwrap();

        // Create a role using content-hash ID builder
        let role = agency::build_role("Implementer", "Implements features", vec![], "Working code");
        let role_id = role.id.clone();
        agency::save_role(&role, &roles_dir).unwrap();

        // Create a motivation using content-hash ID builder
        let motivation = agency::build_motivation(
            "Quality First",
            "Prioritize quality",
            vec!["Spend more time".to_string()],
            vec!["Skip tests".to_string()],
        );
        let motivation_id = motivation.id.clone();
        agency::save_motivation(&motivation, &motivations_dir).unwrap();

        // Create an Agent entity pairing the role and motivation
        let agent_id = agency::content_hash_agent(&role_id, &motivation_id);
        let agent = agency::Agent {
            id: agent_id.clone(),
            role_id: role_id.clone(),
            motivation_id: motivation_id.clone(),
            name: "Test Agent".to_string(),
            performance: agency::PerformanceRecord {
                task_count: 0,
                avg_score: None,
                evaluations: vec![],
            },
            lineage: agency::Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "claude".to_string(),
        };
        agency::save_agent(&agent, &agents_dir).unwrap();

        // Create a task with agent reference
        let mut task = make_test_task("task-1", "Test Task");
        task.agent = Some(agent_id);

        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert!(!vars.task_identity.is_empty());
        assert!(vars.task_identity.contains("Implementer"));
        assert!(vars.task_identity.contains("Spend more time")); // acceptable tradeoff
        assert!(vars.task_identity.contains("Skip tests")); // unacceptable tradeoff
        assert!(vars.task_identity.contains("Agent Identity"));
    }

    #[test]
    fn test_template_vars_identity_missing_agent_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        let agents_dir = wg_dir.join("agency").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();

        let mut task = make_test_task("task-1", "Test Task");
        task.agent = Some("nonexistent-agent-hash".to_string());

        // Should gracefully fallback to empty string when agent can't be found
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert_eq!(vars.task_identity, "");
    }

    #[test]
    fn test_template_apply_with_identity() {
        let mut task = make_test_task("task-1", "Test Task");
        task.agent = None;
        let vars = TemplateVars::from_task(&task, None, None);

        let template = "Preamble\n{{task_identity}}\nTask: {{task_id}}";
        let result = vars.apply(template);
        assert_eq!(result, "Preamble\n\nTask: task-1");
    }

    // --- Error path tests for ExecutorConfig ---

    #[test]
    fn test_load_by_name_missing_config_file() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(wg_dir.join("executors")).unwrap();

        let result = ExecutorConfig::load_by_name(&wg_dir, "nonexistent");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Executor config not found: nonexistent"),
            "Expected 'not found' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_by_name_missing_executors_directory() {
        let temp_dir = TempDir::new().unwrap();
        // .workgraph exists but executors/ subdirectory does not
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let result = ExecutorConfig::load_by_name(&wg_dir, "claude");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Executor config not found"),
            "Expected 'not found' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_malformed_toml() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("bad.toml");
        fs::write(&config_path, "this is [not valid {{ toml").unwrap();

        let result = ExecutorConfig::load(&config_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse executor config"),
            "Expected parse error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_missing_required_fields_no_executor_section() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("incomplete.toml");
        // Valid TOML but missing the [executor] section entirely
        fs::write(&config_path, "[something_else]\nkey = \"value\"\n").unwrap();

        let result = ExecutorConfig::load(&config_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse executor config"),
            "Expected parse error for missing section, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_missing_required_fields_no_command() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("no_command.toml");
        // Has [executor] and type, but missing required 'command' field
        fs::write(&config_path, "[executor]\ntype = \"custom\"\n").unwrap();

        let result = ExecutorConfig::load(&config_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse executor config"),
            "Expected parse error for missing command, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_missing_required_fields_no_type() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("no_type.toml");
        // Has [executor] and command, but missing required 'type' field
        fs::write(&config_path, "[executor]\ncommand = \"echo\"\n").unwrap();

        let result = ExecutorConfig::load(&config_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse executor config"),
            "Expected parse error for missing type, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_nonexistent_file() {
        let result = ExecutorConfig::load(Path::new("/tmp/does_not_exist_ever_12345.toml"));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to read executor config"),
            "Expected read error, got: {}",
            err_msg
        );
    }

    // --- TemplateVars edge case tests ---

    #[test]
    fn test_template_vars_nonexistent_workgraph_dir() {
        let task = make_test_task("task-1", "Test");
        // Pass a path that doesn't exist on disk — canonicalize will fail,
        // so working_dir should fall back to empty string.
        let fake_path = Path::new("/tmp/nonexistent_workgraph_dir_xyz_12345");
        let vars = TemplateVars::from_task(&task, None, Some(fake_path));
        assert_eq!(vars.working_dir, "");
    }

    #[test]
    fn test_template_vars_empty_task_description() {
        let mut task = make_test_task("task-1", "Test");
        task.description = None;
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_description, "");
    }

    #[test]
    fn test_template_vars_special_characters() {
        let mut task = make_test_task("task-with-special", "Title with \"quotes\" & <tags>");
        task.description = Some("Desc with {{braces}} and $dollars and `backticks`".to_string());
        let vars = TemplateVars::from_task(&task, Some("Context with\nnewlines\tand\ttabs"), None);

        // Template application should be a literal substitution
        let result = vars.apply(
            "id={{task_id}} title={{task_title}} desc={{task_description}} ctx={{task_context}}",
        );
        assert_eq!(
            result,
            "id=task-with-special title=Title with \"quotes\" & <tags> desc=Desc with {{braces}} and $dollars and `backticks` ctx=Context with\nnewlines\tand\ttabs"
        );
    }

    #[test]
    fn test_template_apply_missing_variables_passthrough() {
        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);

        // Unrecognized placeholders should pass through unchanged
        let template = "{{task_id}} {{unknown_var}} {{another_unknown}}";
        let result = vars.apply(template);
        assert_eq!(result, "task-1 {{unknown_var}} {{another_unknown}}");
    }

    #[test]
    fn test_template_apply_no_placeholders() {
        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);

        let template = "Just a plain string with no placeholders";
        let result = vars.apply(template);
        assert_eq!(result, "Just a plain string with no placeholders");
    }

    #[test]
    fn test_template_apply_empty_string() {
        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);

        let result = vars.apply("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_template_vars_working_dir_with_real_path() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));

        // working_dir should be the canonical parent of .workgraph
        let expected = temp_dir.path().canonicalize().unwrap();
        assert_eq!(vars.working_dir, expected.to_string_lossy().to_string());
    }

    // --- ExecutorRegistry error path tests ---

    #[test]
    fn test_registry_unknown_executor() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());

        let result = registry.load_config("totally_unknown_executor");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Unknown executor"),
            "Expected unknown executor error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_registry_load_from_file_overrides_default() {
        let temp_dir = TempDir::new().unwrap();
        let executors_dir = temp_dir.path().join("executors");
        fs::create_dir_all(&executors_dir).unwrap();

        // Write a custom claude config that overrides the default
        let custom_config = r#"
[executor]
type = "claude"
command = "my-custom-claude"
args = ["--custom-flag"]
"#;
        fs::write(executors_dir.join("claude.toml"), custom_config).unwrap();

        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("claude").unwrap();
        assert_eq!(config.executor.command, "my-custom-claude");
        assert_eq!(config.executor.args, vec!["--custom-flag"]);
    }

    #[test]
    fn test_registry_load_malformed_file_returns_error() {
        let temp_dir = TempDir::new().unwrap();
        let executors_dir = temp_dir.path().join("executors");
        fs::create_dir_all(&executors_dir).unwrap();
        fs::write(executors_dir.join("broken.toml"), "invalid toml {{{").unwrap();

        let registry = ExecutorRegistry::new(temp_dir.path());
        let result = registry.load_config("broken");
        assert!(result.is_err());
    }

    #[test]
    fn test_registry_init_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let registry = ExecutorRegistry::new(&wg_dir);

        // First init
        registry.init().unwrap();
        let claude_content_1 = fs::read_to_string(wg_dir.join("executors/claude.toml")).unwrap();

        // Second init should not fail and should not overwrite existing files
        registry.init().unwrap();
        let claude_content_2 = fs::read_to_string(wg_dir.join("executors/claude.toml")).unwrap();

        assert_eq!(claude_content_1, claude_content_2);
    }

    #[test]
    fn test_registry_default_config_claude_has_prompt_template() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("claude").unwrap();

        assert!(config.executor.prompt_template.is_some());
        let template = config.executor.prompt_template.unwrap().template;
        assert!(template.contains("{{task_id}}"));
        assert!(template.contains("{{task_title}}"));
        assert!(template.contains("{{task_description}}"));
        assert!(template.contains("{{task_context}}"));
        assert!(template.contains("{{task_identity}}"));
    }

    #[test]
    fn test_registry_default_config_shell_has_env() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("shell").unwrap();

        assert_eq!(
            config.executor.env.get("TASK_ID"),
            Some(&"{{task_id}}".to_string())
        );
        assert_eq!(
            config.executor.env.get("TASK_TITLE"),
            Some(&"{{task_title}}".to_string())
        );
    }

    #[test]
    fn test_registry_default_config_default_executor() {
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("default").unwrap();

        assert_eq!(config.executor.executor_type, "default");
        assert_eq!(config.executor.command, "echo");
        assert_eq!(config.executor.args, vec!["Task: {{task_id}}"]);
    }

    // --- apply_templates edge cases ---

    #[test]
    fn test_apply_templates_no_prompt_template() {
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "shell".to_string(),
                command: "bash".to_string(),
                args: vec!["-c".to_string(), "echo {{task_id}}".to_string()],
                env: HashMap::new(),
                prompt_template: None,
                working_dir: None,
                timeout: None,
            },
        };

        let task = make_test_task("t-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);
        let settings = config.apply_templates(&vars);

        assert!(settings.prompt_template.is_none());
        assert_eq!(settings.args, vec!["-c", "echo t-1"]);
    }

    #[test]
    fn test_apply_templates_no_working_dir() {
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "test".to_string(),
                command: "cmd".to_string(),
                args: vec![],
                env: HashMap::new(),
                prompt_template: None,
                working_dir: None,
                timeout: None,
            },
        };

        let task = make_test_task("t-1", "Test");
        let vars = TemplateVars::from_task(&task, None, None);
        let settings = config.apply_templates(&vars);

        assert!(settings.working_dir.is_none());
    }

    #[test]
    fn test_apply_templates_multiple_env_vars() {
        let config = ExecutorConfig {
            executor: ExecutorSettings {
                executor_type: "test".to_string(),
                command: "cmd".to_string(),
                args: vec![],
                env: {
                    let mut env = HashMap::new();
                    env.insert("ID".to_string(), "{{task_id}}".to_string());
                    env.insert("TITLE".to_string(), "{{task_title}}".to_string());
                    env.insert("DESC".to_string(), "{{task_description}}".to_string());
                    env.insert("STATIC".to_string(), "no-template-here".to_string());
                    env
                },
                prompt_template: None,
                working_dir: None,
                timeout: None,
            },
        };

        let task = make_test_task("t-1", "My Task");
        let vars = TemplateVars::from_task(&task, None, None);
        let settings = config.apply_templates(&vars);

        assert_eq!(settings.env.get("ID"), Some(&"t-1".to_string()));
        assert_eq!(settings.env.get("TITLE"), Some(&"My Task".to_string()));
        assert_eq!(
            settings.env.get("DESC"),
            Some(&"Test description".to_string())
        );
        assert_eq!(
            settings.env.get("STATIC"),
            Some(&"no-template-here".to_string())
        );
    }

    // --- Identity resolution edge cases ---

    #[test]
    fn test_identity_agent_exists_but_role_missing() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        let roles_dir = wg_dir.join("agency").join("roles");
        let motivations_dir = wg_dir.join("agency").join("motivations");
        let agents_dir = wg_dir.join("agency").join("agents");
        fs::create_dir_all(&roles_dir).unwrap();
        fs::create_dir_all(&motivations_dir).unwrap();
        fs::create_dir_all(&agents_dir).unwrap();

        // Create an agent that references a non-existent role
        let agent = agency::Agent {
            id: "test-agent-id".to_string(),
            role_id: "nonexistent-role".to_string(),
            motivation_id: "nonexistent-motivation".to_string(),
            name: "Broken Agent".to_string(),
            performance: agency::PerformanceRecord {
                task_count: 0,
                avg_score: None,
                evaluations: vec![],
            },
            lineage: agency::Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "claude".to_string(),
        };
        agency::save_agent(&agent, &agents_dir).unwrap();

        let mut task = make_test_task("task-1", "Test");
        task.agent = Some("test-agent-id".to_string());

        // Should gracefully fall back to empty identity
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert_eq!(vars.task_identity, "");
    }

    #[test]
    fn test_skills_preamble_empty_when_no_skill_file() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert_eq!(vars.skills_preamble, "");
    }

    #[test]
    fn test_skills_preamble_loaded_when_skill_file_exists() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        // Create the skill file at project_root/.claude/skills/using-superpowers/SKILL.md
        let skill_dir = temp_dir
            .path()
            .join(".claude")
            .join("skills")
            .join("using-superpowers");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "Use the Skill tool to invoke skills.",
        )
        .unwrap();

        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert!(vars.skills_preamble.contains("EXTREMELY_IMPORTANT"));
        assert!(
            vars.skills_preamble
                .contains("Use the Skill tool to invoke skills.")
        );
    }

    #[test]
    fn test_skills_preamble_strips_yaml_frontmatter() {
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let skill_dir = temp_dir
            .path()
            .join(".claude")
            .join("skills")
            .join("using-superpowers");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\ntitle: Skill\n---\nActual content here.",
        )
        .unwrap();

        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, None, Some(&wg_dir));
        assert!(vars.skills_preamble.contains("Actual content here."));
        // The frontmatter itself should not appear in the preamble body
        assert!(!vars.skills_preamble.contains("title: Skill"));
    }
}
