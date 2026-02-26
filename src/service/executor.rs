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
use crate::context_scope::ContextScope;
use crate::graph::Task;

// --- Prompt section constants for scope-based assembly ---

/// Required Workflow section: wg log/artifact/done/fail instructions + Important bullets.
/// Contains {{task_id}} placeholders for variable substitution.
pub const REQUIRED_WORKFLOW_SECTION: &str = "\
## Required Workflow

You MUST use these commands to track your work:

1. **Log progress** as you work (helps recovery if interrupted):
   ```bash
   wg log {{task_id}} \"Starting implementation...\"
   wg log {{task_id}} \"Completed X, now working on Y\"
   ```

2. **Record artifacts** if you create/modify files:
   ```bash
   wg artifact {{task_id}} path/to/file
   ```

3. **Validate your work** before marking done:
   - **Code tasks:** Run `cargo build` and `cargo test` (or the project's equivalent). Fix any failures.
   - **Research/docs tasks:** Re-read the task description and verify your output addresses every requirement. Check that referenced files and links exist.
   - **All tasks:** Log your validation results:
     ```bash
     wg log {{task_id}} \"Validated: cargo build + cargo test pass\"
     wg log {{task_id}} \"Validated: re-read description, all requirements addressed\"
     ```

4. **Complete the task** when done:
   ```bash
   wg done {{task_id}}
   wg done {{task_id}} --converged  # Use this if task has loop edges and work is complete
   ```

5. **Mark as failed** if you cannot complete:
   ```bash
   wg fail {{task_id}} --reason \"Specific reason why\"
   ```

## Important
- Run `wg log` commands BEFORE doing work to track progress
- Validate BEFORE running `wg done`
- Run `wg done` BEFORE you finish responding
- If the task description is unclear, do your best interpretation\n";

/// Graph Patterns section: vocabulary, golden rule, subtask guidance.
pub const GRAPH_PATTERNS_SECTION: &str = "\
## Graph Patterns (see docs/AGENT-GUIDE.md for details)

**Vocabulary:** pipeline (A\u{2192}B\u{2192}C), diamond (A\u{2192}[B,C,D]\u{2192}E), scatter-gather (heterogeneous reviewers of same artifact), loop (A\u{2192}B\u{2192}C\u{2192}A with `--max-iterations`).

**Golden rule: same files = sequential edges.** NEVER parallelize tasks that modify the same files \u{2014} one will overwrite the other. When unsure, default to pipeline.

**When creating subtasks:**
- Always include an integrator task at join points: `wg add \"Integrate\" --after worker-a,worker-b`
- List each worker's file scope in the task description
- Run `wg quickstart` for full command reference

**After code changes:** Run `cargo install --path .` to update the global binary.\n";

/// Reusable Workflow Functions section: wg func list/apply/show.
pub const REUSABLE_FUNCTIONS_SECTION: &str = "\
## Reusable Workflow Functions
- `wg func list` \u{2014} discover reusable workflow patterns extracted from past tasks
- `wg func apply <id> --input key=value` \u{2014} instantiate a function to create pre-wired tasks
- `wg func show <id>` \u{2014} view function details and required inputs\n";

/// Critical warning about using wg CLI instead of built-in tools.
/// Contains {{task_id}} placeholders for variable substitution.
pub const CRITICAL_WG_CLI_SECTION: &str = "\
## CRITICAL: Use wg CLI, NOT built-in tools
- You MUST use `wg` CLI commands for ALL task management
- NEVER use built-in TaskCreate, TaskUpdate, TaskList, or TaskGet tools \u{2014} they are a completely separate system that does NOT interact with workgraph
- If you need to create subtasks: `wg add \"title\" --after {{task_id}}`
- To check task status: `wg show <task-id>`
- To list tasks: `wg list`\n";

/// System awareness preamble for full scope (~300 tokens).
const SYSTEM_AWARENESS_PREAMBLE: &str = "\
## System Awareness

You are working within **workgraph**, a task orchestration system. Key concepts:

- **Coordinator**: A daemon (`wg service start`) that polls for ready tasks and spawns agents.
- **Agency**: An evolutionary identity system with roles, motivations, and agents. Agents are assigned to tasks based on skills, performance, and fit.
- **Cycles/Loops**: Tasks can form cycles with `--max-iterations`. Use `wg done --converged` when a cycle's work is complete.
- **Trace Functions**: Reusable workflow patterns (`wg func list/apply/show`) that can instantiate pre-wired task subgraphs.
- **Context Scopes**: Agents receive different amounts of context (clean < task < graph < full) based on task requirements.\n";

/// Ethos section: the philosophical dimension of working within a living graph.
/// Injected at task+ scope to encourage autopoietic behavior.
/// Contains {{task_id}} placeholders for variable substitution.
pub const ETHOS_SECTION: &str = "\
## The Graph is Alive

You are not isolated. The graph is a shared medium — artifacts you write are read by other agents, \
tasks you create get dispatched to other agents. You are one node in a living system.

**Your job is not just to complete your task.** It is to leave the system better than you found it:
- Found a bug? `wg add \"Fix: ...\" --after {{task_id}} -d \"Found while working on {{task_id}}\"`
- Documentation wrong? Fix it, or flag it with `wg add`
- Task too large? Decompose it into subtasks
- Follow-up needed? `wg add \"Verify: ...\" --after {{task_id}}`

The coordinator dispatches anything you add. You don't need permission.

**The loop:** spec \u{2192} implement \u{2192} verify \u{2192} improve \u{2192} spec. \
You may be any node. Use `wg context` to see what came before. \
Use `wg add` to create what comes next.\n";

/// Hint for task+ scopes about using wg context/show to get more info (R2).
const WG_CONTEXT_HINT: &str = "\
## Additional Context
- Use `wg show <task-id>` to inspect any task's details, status, artifacts, and logs
- Use `wg context` to view the current task's full context
- Use `wg list` to see all tasks and their statuses\n";

/// Additional context for scope-based prompt assembly beyond TemplateVars.
#[derive(Debug, Default, Clone)]
pub struct ScopeContext {
    /// Downstream task IDs + titles (R1, task+ scope)
    pub downstream_info: String,
    /// Task tags and skills (R4, task+ scope)
    pub tags_skills_info: String,
    /// Project description from config.toml (graph+ scope)
    pub project_description: String,
    /// 1-hop subgraph summary (graph+ scope)
    pub graph_summary: String,
    /// Full graph summary (full scope)
    pub full_graph_summary: String,
    /// CLAUDE.md content (full scope)
    pub claude_md_content: String,
}

/// Build a scope-aware prompt for built-in executors.
///
/// Assembles prompt sections based on the context scope:
/// - `clean`: skills_preamble + identity + task info + upstream context + loop info only
/// - `task`: + workflow commands + graph patterns + reusable functions + wg cli warning + R1/R2/R4
/// - `graph`: + project description + subgraph summary (1-hop neighborhood)
/// - `full`: + system awareness preamble + full graph summary + CLAUDE.md content
pub fn build_prompt(vars: &TemplateVars, scope: ContextScope, ctx: &ScopeContext) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Full scope: system awareness preamble (at the top for orientation)
    if scope >= ContextScope::Full {
        parts.push(SYSTEM_AWARENESS_PREAMBLE.to_string());
    }

    // All scopes: skills preamble
    if !vars.skills_preamble.is_empty() {
        parts.push(vars.skills_preamble.clone());
    }

    // All scopes: task assignment header
    parts.push(
        "# Task Assignment\n\nYou are an AI agent working on a task in a workgraph project.\n"
            .to_string(),
    );

    // All scopes: agent identity
    if !vars.task_identity.is_empty() {
        parts.push(vars.task_identity.clone());
    }

    // All scopes: task details
    parts.push(format!(
        "## Your Task\n- **ID:** {}\n- **Title:** {}\n- **Description:** {}",
        vars.task_id, vars.task_title, vars.task_description
    ));

    // All scopes: verification criteria (R4 from validation synthesis)
    if let Some(ref verify) = vars.task_verify {
        parts.push(format!(
            "## Verification Required\n\nBefore marking done, you MUST verify:\n{}",
            verify
        ));
    }

    // Task+ scope: tags and skills (R4)
    if scope >= ContextScope::Task && !ctx.tags_skills_info.is_empty() {
        parts.push(ctx.tags_skills_info.clone());
    }

    // All scopes: context from dependencies
    parts.push(format!(
        "## Context from Dependencies\n{}",
        vars.task_context
    ));

    // Task+ scope: downstream awareness (R1)
    if scope >= ContextScope::Task && !ctx.downstream_info.is_empty() {
        parts.push(ctx.downstream_info.clone());
    }

    // All scopes: loop info
    if !vars.task_loop_info.is_empty() {
        parts.push(vars.task_loop_info.clone());
    }

    // Task+ scope: workflow sections (with {{task_id}} substitution)
    if scope >= ContextScope::Task {
        parts.push(vars.apply(REQUIRED_WORKFLOW_SECTION));
        parts.push(vars.apply(ETHOS_SECTION));
        parts.push(GRAPH_PATTERNS_SECTION.to_string());
        parts.push(REUSABLE_FUNCTIONS_SECTION.to_string());
        parts.push(vars.apply(CRITICAL_WG_CLI_SECTION));
        parts.push(WG_CONTEXT_HINT.to_string());
    }

    // Graph+ scope: project description
    if scope >= ContextScope::Graph && !ctx.project_description.is_empty() {
        parts.push(format!("## Project\n\n{}", ctx.project_description));
    }

    // Graph+ scope: subgraph summary (1-hop neighborhood)
    if scope >= ContextScope::Graph && !ctx.graph_summary.is_empty() {
        parts.push(ctx.graph_summary.clone());
    }

    // Full scope: full graph summary
    if scope >= ContextScope::Full && !ctx.full_graph_summary.is_empty() {
        parts.push(ctx.full_graph_summary.clone());
    }

    // Full scope: CLAUDE.md content
    if scope >= ContextScope::Full && !ctx.claude_md_content.is_empty() {
        parts.push(format!(
            "## Project Instructions (CLAUDE.md)\n\n{}",
            ctx.claude_md_content
        ));
    }

    parts.push("Begin working on the task now.".to_string());

    parts.join("\n\n")
}

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
    pub model: String,
    pub task_loop_info: String,
    pub task_verify: Option<String>,
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

        let task_loop_info = if let Some(config) = &task.cycle_config {
            format!(
                "## Cycle Information\n\n\
                 This task is a cycle header (iteration {}, max {}).\n\n\
                 **IMPORTANT: When this cycle's work is complete (converged), you MUST use:**\n\
                 ```\n\
                 wg done {} --converged\n\
                 ```\n\
                 Using plain `wg done` will cause the cycle to iterate again and re-open tasks.\n\
                 Only use plain `wg done` if you want the next iteration to proceed.",
                task.loop_iteration,
                config.max_iterations,
                task.id
            )
        } else if task.loop_iteration > 0 {
            format!(
                "## Cycle Information\n\n\
                 This task is in cycle iteration {}.\n\n\
                 **IMPORTANT: When this cycle's work is complete (converged), you MUST use:**\n\
                 ```\n\
                 wg done {} --converged\n\
                 ```",
                task.loop_iteration,
                task.id
            )
        } else {
            String::new()
        };

        Self {
            task_id: task.id.clone(),
            task_title: task.title.clone(),
            task_description: task.description.clone().unwrap_or_default(),
            task_context: context.unwrap_or_default().to_string(),
            task_identity,
            working_dir,
            skills_preamble,
            model: task.model.clone().unwrap_or_default(),
            task_loop_info,
            task_verify: task.verify.clone(),
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
        let agents_dir = agency_dir.join("cache/agents");
        let roles_dir = agency_dir.join("cache/roles");
        let motivations_dir = agency_dir.join("primitives/tradeoffs");

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
            match agency::find_tradeoff_by_prefix(&motivations_dir, &agent.tradeoff_id) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "Warning: could not resolve motivation '{}' for agent '{}': {}",
                        agent.tradeoff_id, agent_hash, e
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
                .and_then(|abs| abs.parent().map(std::path::Path::to_path_buf))
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
            .replace("{{model}}", &self.model)
            .replace("{{task_loop_info}}", &self.task_loop_info)
            .replace("{{task_verify}}", self.task_verify.as_deref().unwrap_or(""))
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

    /// Default model for this executor.
    /// Overrides coordinator.model but is overridden by task.model.
    /// Hierarchy: task.model > executor.model > coordinator.model > 'default'.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
                    // No default template — built-in executors use scope-based
                    // build_prompt() assembly. Custom configs in
                    // .workgraph/executors/*.toml can still define a template
                    // to override this behavior.
                    prompt_template: None,
                    working_dir: Some("{{working_dir}}".to_string()),
                    timeout: None,
                    model: None,
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
                    model: None,
                },
            }),
            "amplifier" => Ok(ExecutorConfig {
                executor: ExecutorSettings {
                    executor_type: "amplifier".to_string(),
                    command: "amplifier".to_string(),
                    args: vec![
                        "run".to_string(),
                        "--mode".to_string(),
                        "single".to_string(),
                        "--output-format".to_string(),
                        "text".to_string(),
                    ],
                    env: {
                        let mut env = HashMap::new();
                        env.insert("WG_TASK_ID".to_string(), "{{task_id}}".to_string());
                        env
                    },
                    // No default template — uses scope-based build_prompt() assembly.
                    prompt_template: None,
                    working_dir: Some("{{working_dir}}".to_string()),
                    timeout: Some(600),
                    model: None,
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
                    model: None,
                },
            }),
            _ => Err(anyhow!(
                "Unknown executor '{}'. Available: claude, amplifier, shell, default",
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
            before: vec![],
            after: vec![],
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
            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            cycle_config: None,
            token_usage: None,
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
                model: None,
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
        let roles_dir = wg_dir.join("agency").join("cache/roles");
        let motivations_dir = wg_dir.join("agency").join("primitives/tradeoffs");
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        fs::create_dir_all(&roles_dir).unwrap();
        fs::create_dir_all(&motivations_dir).unwrap();
        fs::create_dir_all(&agents_dir).unwrap();

        // Create a role using content-hash ID builder
        let role = agency::build_role("Implementer", "Implements features", vec![], "Working code");
        let role_id = role.id.clone();
        agency::save_role(&role, &roles_dir).unwrap();

        // Create a motivation using content-hash ID builder
        let motivation = agency::build_tradeoff(
            "Quality First",
            "Prioritize quality",
            vec!["Spend more time".to_string()],
            vec!["Skip tests".to_string()],
        );
        let motivation_id = motivation.id.clone();
        agency::save_tradeoff(&motivation, &motivations_dir).unwrap();

        // Create an Agent entity pairing the role and motivation
        let agent_id = agency::content_hash_agent(&role_id, &motivation_id);
        let agent = agency::Agent {
            id: agent_id.clone(),
            role_id: role_id.clone(),
            tradeoff_id: motivation_id.clone(),
            name: "Test Agent".to_string(),
            performance: agency::PerformanceRecord::default(),
            lineage: agency::Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "claude".to_string(),
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
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
        let agents_dir = wg_dir.join("agency").join("cache/agents");
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
    fn test_registry_default_config_claude_no_prompt_template() {
        // Built-in claude executor uses scope-based build_prompt() instead of a template
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("claude").unwrap();

        assert!(
            config.executor.prompt_template.is_none(),
            "Built-in claude config should have no prompt_template (uses build_prompt)"
        );
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
                model: None,
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
                model: None,
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
                model: None,
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
        let roles_dir = wg_dir.join("agency").join("cache/roles");
        let motivations_dir = wg_dir.join("agency").join("primitives/tradeoffs");
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        fs::create_dir_all(&roles_dir).unwrap();
        fs::create_dir_all(&motivations_dir).unwrap();
        fs::create_dir_all(&agents_dir).unwrap();

        // Create an agent that references a non-existent role
        let agent = agency::Agent {
            id: "test-agent-id".to_string(),
            role_id: "nonexistent-role".to_string(),
            tradeoff_id: "nonexistent-motivation".to_string(),
            name: "Broken Agent".to_string(),
            performance: agency::PerformanceRecord::default(),
            lineage: agency::Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "claude".to_string(),
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
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

    #[test]
    fn test_template_vars_include_loop_info() {
        let mut task = make_test_task("task-1", "Looping Task");
        task.loop_iteration = 2;
        task.cycle_config = Some(crate::graph::CycleConfig {
            max_iterations: 3,
            guard: None,
            delay: None,
        });

        let vars = TemplateVars::from_task(&task, None, None);

        assert!(vars.task_loop_info.contains("iteration 2"));
    }

    #[test]
    fn test_template_vars_empty_loop_info_for_non_loop_tasks() {
        let task = make_test_task("task-1", "Normal Task");
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_loop_info, "");
    }

    #[test]
    fn test_build_prompt_contains_converged_for_task_scope() {
        // build_prompt with task scope should include --converged in workflow section
        let task = make_test_task("task-1", "Looping Task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);
        assert!(
            prompt.contains("--converged"),
            "Task-scope prompt should mention --converged"
        );
    }

    #[test]
    fn test_build_prompt_no_workflow_for_clean_scope() {
        // build_prompt with clean scope should NOT include workflow sections
        let task = make_test_task("task-1", "Clean Task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);
        assert!(
            !prompt.contains("## Required Workflow"),
            "Clean-scope prompt should not include Required Workflow"
        );
        assert!(
            !prompt.contains("## Graph Patterns"),
            "Clean-scope prompt should not include Graph Patterns"
        );
        assert!(
            !prompt.contains("## CRITICAL"),
            "Clean-scope prompt should not include CRITICAL CLI section"
        );
        // But should still have task info
        assert!(prompt.contains("task-1"));
        assert!(prompt.contains("Clean Task"));
    }

    #[test]
    fn test_default_amplifier_no_prompt_template() {
        // Built-in amplifier executor uses scope-based build_prompt() instead of a template
        let temp_dir = TempDir::new().unwrap();
        let registry = ExecutorRegistry::new(temp_dir.path());
        let config = registry.load_config("amplifier").unwrap();
        assert!(
            config.executor.prompt_template.is_none(),
            "Built-in amplifier config should have no prompt_template (uses build_prompt)"
        );
    }

    #[test]
    fn test_template_apply_loop_info_substitution() {
        let mut task = make_test_task("task-1", "Loop Task");
        task.loop_iteration = 1;
        task.cycle_config = Some(crate::graph::CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
        });

        let vars = TemplateVars::from_task(&task, None, None);
        let template = "Before\n{{task_loop_info}}\nAfter";
        let result = vars.apply(template);

        assert!(result.contains("Before"));
        assert!(result.contains("After"));
        assert!(!result.contains("{{task_loop_info}}"));
    }

    // --- build_prompt scope tests ---

    #[test]
    fn test_build_prompt_clean_scope_minimal() {
        let task = make_test_task("task-1", "Clean Task");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        // Should include task info
        assert!(prompt.contains("# Task Assignment"));
        assert!(prompt.contains("task-1"));
        assert!(prompt.contains("Clean Task"));
        assert!(prompt.contains("Test description"));
        assert!(prompt.contains("dep context"));
        assert!(prompt.contains("Begin working on the task now."));

        // Should NOT include workflow/graph sections
        assert!(!prompt.contains("## Required Workflow"));
        assert!(!prompt.contains("## Graph Patterns"));
        assert!(!prompt.contains("## Reusable Workflow Functions"));
        assert!(!prompt.contains("## CRITICAL"));
        assert!(!prompt.contains("## Additional Context"));
        assert!(!prompt.contains("## System Awareness"));
    }

    #[test]
    fn test_build_prompt_task_scope_includes_workflow() {
        let task = make_test_task("task-1", "Task Scope");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext {
            downstream_info: "## Downstream\n- dt-1: \"Consumer\"".to_string(),
            tags_skills_info: "- **Tags:** rust, impl".to_string(),
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);

        // Should include workflow sections
        assert!(prompt.contains("## Required Workflow"));
        assert!(prompt.contains("wg log task-1")); // {{task_id}} substituted
        assert!(prompt.contains("wg done task-1"));
        assert!(prompt.contains("## Graph Patterns"));
        assert!(prompt.contains("## Reusable Workflow Functions"));
        assert!(prompt.contains("## CRITICAL: Use wg CLI"));
        assert!(prompt.contains("wg add \"title\" --after task-1")); // {{task_id}} substituted
        assert!(prompt.contains("## Additional Context")); // R2

        // Should include R1 and R4
        assert!(prompt.contains("## Downstream"));
        assert!(prompt.contains("Consumer"));
        assert!(prompt.contains("rust, impl"));

        // Should NOT include graph/full sections
        assert!(!prompt.contains("## System Awareness"));
        assert!(!prompt.contains("## Project\n"));
        assert!(!prompt.contains("## Full Graph Summary"));
        assert!(!prompt.contains("CLAUDE.md"));
    }

    #[test]
    fn test_build_prompt_graph_scope_includes_project_and_summary() {
        let task = make_test_task("task-1", "Graph Scope");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext {
            project_description: "A project for testing.".to_string(),
            graph_summary: "## Graph Status\n\n5 tasks".to_string(),
            ..Default::default()
        };
        let prompt = build_prompt(&vars, ContextScope::Graph, &ctx);

        // Should include task+ sections
        assert!(prompt.contains("## Required Workflow"));
        assert!(prompt.contains("## Graph Patterns"));

        // Should include graph sections
        assert!(prompt.contains("## Project\n\nA project for testing."));
        assert!(prompt.contains("## Graph Status\n\n5 tasks"));

        // Should NOT include full sections
        assert!(!prompt.contains("## System Awareness"));
        assert!(!prompt.contains("## Full Graph Summary"));
        assert!(!prompt.contains("CLAUDE.md"));
    }

    #[test]
    fn test_build_prompt_full_scope_includes_everything() {
        let task = make_test_task("task-1", "Full Scope");
        let vars = TemplateVars::from_task(&task, Some("dep context"), None);
        let ctx = ScopeContext {
            downstream_info: "## Downstream\n- dt-1".to_string(),
            tags_skills_info: "- **Tags:** meta".to_string(),
            project_description: "My project".to_string(),
            graph_summary: "## Graph Status\n\n10 tasks".to_string(),
            full_graph_summary: "## Full Graph Summary\n\n- task-a [done]".to_string(),
            claude_md_content: "Always use bun.".to_string(),
        };
        let prompt = build_prompt(&vars, ContextScope::Full, &ctx);

        // Should include all sections
        assert!(prompt.contains("## System Awareness"));
        assert!(prompt.contains("## Required Workflow"));
        assert!(prompt.contains("## Graph Patterns"));
        assert!(prompt.contains("## Project\n\nMy project"));
        assert!(prompt.contains("## Graph Status"));
        assert!(prompt.contains("## Full Graph Summary"));
        assert!(prompt.contains("## Project Instructions (CLAUDE.md)\n\nAlways use bun."));
        assert!(prompt.contains("## Downstream"));
        assert!(prompt.contains("meta"));
    }

    #[test]
    fn test_build_prompt_includes_loop_info() {
        let mut task = make_test_task("task-1", "Loop Task");
        task.loop_iteration = 2;
        task.cycle_config = Some(crate::graph::CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
        });

        let vars = TemplateVars::from_task(&task, None, None);
        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        // Loop info should appear even at clean scope
        assert!(prompt.contains("iteration 2"));
        assert!(prompt.contains("--converged"));
    }

    #[test]
    fn test_build_prompt_includes_identity() {
        let task = make_test_task("task-1", "Identity Task");
        let mut vars = TemplateVars::from_task(&task, None, None);
        vars.task_identity = "## Agent Identity\n\nRole: implementer\n".to_string();

        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        assert!(prompt.contains("## Agent Identity"));
        assert!(prompt.contains("Role: implementer"));
    }

    #[test]
    fn test_build_prompt_includes_skills_preamble() {
        let task = make_test_task("task-1", "Skills Task");
        let mut vars = TemplateVars::from_task(&task, None, None);
        vars.skills_preamble = "<EXTREMELY_IMPORTANT>\nUse skills.\n</EXTREMELY_IMPORTANT>\n".to_string();

        let ctx = ScopeContext::default();
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

        assert!(prompt.contains("EXTREMELY_IMPORTANT"));
        assert!(prompt.contains("Use skills."));
    }

    #[test]
    fn test_build_prompt_empty_scope_context_omits_sections() {
        let task = make_test_task("task-1", "Test");
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext::default(); // all empty

        let prompt = build_prompt(&vars, ContextScope::Full, &ctx);

        // Should still have system preamble and workflow even with empty ctx
        assert!(prompt.contains("## System Awareness"));
        assert!(prompt.contains("## Required Workflow"));

        // But should NOT have empty project/graph sections
        assert!(!prompt.contains("## Project\n"));
        assert!(!prompt.contains("## Graph Status"));
        assert!(!prompt.contains("## Full Graph Summary"));
        assert!(!prompt.contains("CLAUDE.md"));
    }

    #[test]
    fn test_section_constants_contain_expected_content() {
        assert!(REQUIRED_WORKFLOW_SECTION.contains("wg log {{task_id}}"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("wg done {{task_id}}"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("wg fail {{task_id}}"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("wg artifact {{task_id}}"));
        assert!(REQUIRED_WORKFLOW_SECTION.contains("--converged"));

        assert!(GRAPH_PATTERNS_SECTION.contains("Golden rule"));
        assert!(GRAPH_PATTERNS_SECTION.contains("pipeline"));
        assert!(GRAPH_PATTERNS_SECTION.contains("cargo install --path"));

        assert!(REUSABLE_FUNCTIONS_SECTION.contains("wg func list"));
        assert!(REUSABLE_FUNCTIONS_SECTION.contains("wg func apply"));

        assert!(CRITICAL_WG_CLI_SECTION.contains("NEVER use built-in TaskCreate"));
        assert!(CRITICAL_WG_CLI_SECTION.contains("wg add \"title\" --after {{task_id}}"));
    }

    #[test]
    fn test_build_prompt_includes_verify_when_present() {
        let mut task = make_test_task("task-1", "Test");
        task.verify = Some("run cargo test and confirm all pass".to_string());
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext::default();

        // Verify section should appear at all scopes (including clean)
        let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);
        assert!(prompt.contains("## Verification Required"));
        assert!(prompt.contains("run cargo test and confirm all pass"));
        assert!(prompt.contains("Before marking done, you MUST verify:"));
    }

    #[test]
    fn test_build_prompt_omits_verify_when_absent() {
        let task = make_test_task("task-1", "Test");
        assert!(task.verify.is_none());
        let vars = TemplateVars::from_task(&task, Some("dep ctx"), None);
        let ctx = ScopeContext::default();

        let prompt = build_prompt(&vars, ContextScope::Task, &ctx);
        assert!(!prompt.contains("## Verification Required"));
    }

    #[test]
    fn test_template_vars_verify_from_task() {
        let mut task = make_test_task("task-1", "Test");
        task.verify = Some("check output format".to_string());
        let vars = TemplateVars::from_task(&task, None, None);
        assert_eq!(vars.task_verify, Some("check output format".to_string()));

        let task2 = make_test_task("task-2", "Test2");
        let vars2 = TemplateVars::from_task(&task2, None, None);
        assert_eq!(vars2.task_verify, None);
    }
}
