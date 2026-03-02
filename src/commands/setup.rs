//! Interactive configuration wizard for first-time workgraph setup.
//!
//! Creates/updates ~/.workgraph/config.toml via guided prompts using dialoguer.

use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, Input, Select};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use workgraph::config::Config;

/// Marker used to detect whether workgraph directives are already present in CLAUDE.md.
const CLAUDE_MD_MARKER: &str = "<!-- workgraph-managed -->";

/// The workgraph directives block appended to CLAUDE.md.
const CLAUDE_MD_DIRECTIVES: &str = r#"<!-- workgraph-managed -->
# Workgraph

Use workgraph for task management.

**At the start of each session, run `wg quickstart` in your terminal to orient yourself.**
Use `wg service start` to dispatch work — do not manually claim tasks.

## For All Agents (Including the Orchestrating Agent)

CRITICAL: Do NOT use built-in TaskCreate/TaskUpdate/TaskList/TaskGet tools.
These are a separate system that does NOT interact with workgraph.
Always use `wg` CLI commands for all task management.

CRITICAL: Do NOT use the built-in **Task tool** (subagents). NEVER spawn Explore, Plan,
general-purpose, or any other subagent type. The Task tool creates processes outside
workgraph, which defeats the entire system. If you need research, exploration, or planning
done — create a `wg add` task and let the coordinator dispatch it.

ALL tasks — including research, exploration, and planning — should be workgraph tasks.

### Orchestrating agent role

The orchestrating agent (the one the user interacts with directly) does ONLY:
- **Conversation** with the user
- **Inspection** via `wg show`, `wg viz`, `wg list`, `wg status`, and reading files
- **Task creation** via `wg add` with descriptions, dependencies, and context
- **Monitoring** via `wg agents`, `wg service status`, `wg watch`

It NEVER writes code, implements features, or does research itself.
Everything gets dispatched through `wg add` and `wg service start`.
"#;

/// Choices gathered from the interactive wizard.
#[derive(Debug, Clone)]
pub struct SetupChoices {
    pub executor: String,
    pub model: String,
    pub agency_enabled: bool,
    pub evaluator_model: Option<String>,
    pub assigner_model: Option<String>,
    pub max_agents: usize,
}

/// Build a Config from wizard choices, optionally layered on top of an existing config.
pub fn build_config(choices: &SetupChoices, base: Option<&Config>) -> Config {
    let mut config = base.cloned().unwrap_or_default();

    config.coordinator.executor = choices.executor.clone();
    config.agent.executor = choices.executor.clone();

    config.agent.model = choices.model.clone();
    config.coordinator.model = Some(choices.model.clone());

    config.coordinator.max_agents = choices.max_agents;

    config.agency.auto_assign = choices.agency_enabled;
    config.agency.auto_evaluate = choices.agency_enabled;

    if let Some(ref eval_model) = choices.evaluator_model {
        config.agency.evaluator_model = Some(eval_model.clone());
    }
    if let Some(ref assign_model) = choices.assigner_model {
        config.agency.assigner_model = Some(assign_model.clone());
    }

    config
}

/// Format a summary of what will be written.
pub fn format_summary(choices: &SetupChoices) -> String {
    let mut lines = Vec::new();
    lines.push("[coordinator]".to_string());
    lines.push(format!("  executor = \"{}\"", choices.executor));
    lines.push(format!("  model = \"{}\"", choices.model));
    lines.push(format!("  max_agents = {}", choices.max_agents));
    lines.push(String::new());
    lines.push("[agent]".to_string());
    lines.push(format!("  executor = \"{}\"", choices.executor));
    lines.push(format!("  model = \"{}\"", choices.model));
    lines.push(String::new());
    lines.push("[agency]".to_string());
    lines.push(format!("  auto_assign = {}", choices.agency_enabled));
    lines.push(format!("  auto_evaluate = {}", choices.agency_enabled));
    if let Some(ref m) = choices.evaluator_model {
        lines.push(format!("  evaluator_model = \"{}\"", m));
    }
    if let Some(ref m) = choices.assigner_model {
        lines.push(format!("  assigner_model = \"{}\"", m));
    }
    lines.join("\n")
}

/// Check whether a CLAUDE.md file already contains workgraph directives.
pub fn has_workgraph_directives(path: &Path) -> bool {
    if let Ok(content) = std::fs::read_to_string(path) {
        content.contains(CLAUDE_MD_MARKER)
    } else {
        false
    }
}

/// Configure ~/.claude/CLAUDE.md with workgraph directives.
///
/// - If ~/.claude/ doesn't exist, it is created.
/// - If CLAUDE.md doesn't exist, it is created with the directives.
/// - If CLAUDE.md exists but has no workgraph marker, directives are appended.
/// - If CLAUDE.md already contains the marker, it is left unchanged (idempotent).
///
/// Returns a status string for display and whether changes were made.
pub fn configure_claude_md() -> Result<(String, bool)> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let claude_dir = PathBuf::from(&home).join(".claude");
    let claude_md = claude_dir.join("CLAUDE.md");

    configure_claude_md_at(&claude_md)
}

/// Configure a CLAUDE.md at the given project directory.
///
/// Creates or updates `<project_dir>/CLAUDE.md` with workgraph directives.
/// Same idempotency rules as `configure_claude_md`.
pub fn configure_project_claude_md(project_dir: &Path) -> Result<(String, bool)> {
    let claude_md = project_dir.join("CLAUDE.md");
    configure_claude_md_at(&claude_md)
}

/// Shared implementation for configuring a CLAUDE.md at a specific path.
fn configure_claude_md_at(claude_md: &Path) -> Result<(String, bool)> {
    if has_workgraph_directives(claude_md) {
        return Ok((format!("{} already configured", claude_md.display()), false));
    }

    // Ensure parent directory exists
    if let Some(parent) = claude_md.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    if claude_md.exists() {
        // Append to existing file
        let existing = std::fs::read_to_string(claude_md)
            .with_context(|| format!("Failed to read {}", claude_md.display()))?;
        let separator = if existing.ends_with('\n') || existing.is_empty() {
            "\n"
        } else {
            "\n\n"
        };
        let new_content = format!("{}{}{}", existing, separator, CLAUDE_MD_DIRECTIVES);
        std::fs::write(claude_md, new_content)
            .with_context(|| format!("Failed to write {}", claude_md.display()))?;
        Ok((
            format!("Updated {} with workgraph directives", claude_md.display()),
            true,
        ))
    } else {
        // Create new file
        std::fs::write(claude_md, CLAUDE_MD_DIRECTIVES)
            .with_context(|| format!("Failed to create {}", claude_md.display()))?;
        Ok((
            format!("Created {} with workgraph directives", claude_md.display()),
            true,
        ))
    }
}

/// Run the interactive setup wizard.
pub fn run() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("wg setup requires an interactive terminal");
    }

    // Load existing global config for defaults
    let existing = Config::load_global()?.unwrap_or_default();
    let global_path = Config::global_config_path()?;

    println!("Welcome to workgraph setup.");
    println!(
        "This will configure your global defaults at {}",
        global_path.display()
    );
    println!();

    // 1. Executor
    let executor_options = &["claude", "amplifier", "custom"];
    let current_executor_idx = executor_options
        .iter()
        .position(|&e| e == existing.coordinator.executor)
        .unwrap_or(0);

    let executor_idx = Select::new()
        .with_prompt("Which executor backend?")
        .items(executor_options)
        .default(current_executor_idx)
        .interact()?;

    let executor = if executor_idx == 2 {
        // Custom executor
        let custom: String = Input::new()
            .with_prompt("Custom executor name")
            .default(existing.coordinator.executor.clone())
            .interact_text()?;
        custom
    } else {
        executor_options[executor_idx].to_string()
    };

    // 2. Default model
    let model_options = &[
        ("opus", "Most capable, best for complex tasks"),
        ("sonnet", "Balanced capability and speed"),
        ("haiku", "Fastest, best for simple tasks"),
    ];
    let model_labels: Vec<String> = model_options
        .iter()
        .map(|(name, desc)| format!("{} — {}", name, desc))
        .collect();

    let current_model = existing
        .coordinator
        .model
        .as_deref()
        .unwrap_or(&existing.agent.model);
    let current_model_idx = model_options
        .iter()
        .position(|(name, _)| *name == current_model)
        .unwrap_or(0);

    let model_idx = Select::new()
        .with_prompt("Default model for agents?")
        .items(&model_labels)
        .default(current_model_idx)
        .interact()?;

    let model = if model_idx < model_options.len() {
        model_options[model_idx].0.to_string()
    } else {
        // Shouldn't happen with Select, but handle gracefully
        let custom: String = Input::new()
            .with_prompt("Custom model ID")
            .interact_text()?;
        custom
    };

    // 4. Agency
    let agency_enabled = Confirm::new()
        .with_prompt("Enable agency (auto-assign agents to tasks, auto-evaluate completed work)?")
        .default(existing.agency.auto_assign || existing.agency.auto_evaluate)
        .interact()?;

    let (evaluator_model, assigner_model) = if agency_enabled {
        // Evaluator model
        let eval_options = &[
            "haiku (recommended, lightweight)",
            "sonnet",
            "same as default",
        ];
        let current_eval_idx = match existing.agency.evaluator_model.as_deref() {
            Some("sonnet") => 1,
            Some(m) if m == model => 2,
            _ => 0,
        };
        let eval_idx = Select::new()
            .with_prompt("Evaluator model?")
            .items(eval_options)
            .default(current_eval_idx)
            .interact()?;
        let eval_model = match eval_idx {
            0 => Some("haiku".to_string()),
            1 => Some("sonnet".to_string()),
            _ => None, // same as default = don't set, falls through to agent.model
        };

        // Assigner model
        let assign_options = &["haiku (recommended, cheap)", "sonnet", "same as default"];
        let current_assign_idx = match existing.agency.assigner_model.as_deref() {
            Some("sonnet") => 1,
            Some(m) if m == model => 2,
            _ => 0,
        };
        let assign_idx = Select::new()
            .with_prompt("Assigner model?")
            .items(assign_options)
            .default(current_assign_idx)
            .interact()?;
        let assign_model = match assign_idx {
            0 => Some("haiku".to_string()),
            1 => Some("sonnet".to_string()),
            _ => None,
        };

        (eval_model, assign_model)
    } else {
        (None, None)
    };

    // 5. Max agents
    let max_agents: usize = Input::new()
        .with_prompt("Max parallel agents?")
        .default(existing.coordinator.max_agents)
        .interact_text()?;

    let choices = SetupChoices {
        executor,
        model,
        agency_enabled,
        evaluator_model,
        assigner_model,
        max_agents,
    };

    // 6. Summary and confirmation
    println!();
    println!("Configuration to write:");
    println!("───────────────────────");
    println!("{}", format_summary(&choices));
    println!("───────────────────────");
    println!();

    let confirm = Confirm::new()
        .with_prompt(format!("Write to {}?", global_path.display()))
        .default(true)
        .interact()?;

    if !confirm {
        println!("Setup cancelled.");
        return Ok(());
    }

    // Build and save
    let config = build_config(&choices, Some(&existing));
    config.save_global()?;

    // Post-save: guide skill/bundle installation based on executor
    println!();
    let skill_status = guide_skill_bundle_install(&choices.executor)?;

    // Configure ~/.claude/CLAUDE.md for Claude Code executor
    let claude_md_status = if choices.executor == "claude" {
        println!();
        guide_claude_md_install()?
    } else {
        "N/A (non-Claude executor)".to_string()
    };

    println!();
    println!("Setup complete.");
    println!();
    println!("Summary:");
    println!("  Executor:  {}", choices.executor);
    println!("  Model:     {}", choices.model);
    println!("  Agents:    {} max parallel", choices.max_agents);
    println!("  Skill:     {}", skill_status);
    println!("  CLAUDE.md: {}", claude_md_status);
    println!();
    println!("Run `wg init` in a project directory to get started.");

    Ok(())
}

/// Check if the wg Claude Code skill is installed.
pub fn is_claude_skill_installed() -> bool {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".claude/skills/wg/SKILL.md")
            .exists()
    } else {
        false
    }
}

/// Check if the amplifier-bundle-workgraph setup script exists in common locations.
fn find_amplifier_bundle_setup() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        let candidate = PathBuf::from(&home).join("amplifier-bundle-workgraph/setup.sh");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// After executor selection, guide the user to install the appropriate skill or bundle.
/// Returns a status string for the summary.
fn guide_skill_bundle_install(executor: &str) -> Result<String> {
    match executor {
        "claude" => {
            if is_claude_skill_installed() {
                Ok("wg skill installed ✓".to_string())
            } else {
                println!(
                    "Spawned Claude Code agents need the wg skill to understand workgraph commands."
                );
                let install = Confirm::new()
                    .with_prompt("Install wg skill for Claude Code? (recommended)")
                    .default(true)
                    .interact()?;
                if install {
                    super::skills::run_install()?;
                    Ok("wg skill installed ✓".to_string())
                } else {
                    println!("  You can install it later with: wg skill install");
                    Ok("wg skill NOT installed — run `wg skill install`".to_string())
                }
            }
        }
        "amplifier" => {
            if let Some(setup_path) = find_amplifier_bundle_setup() {
                println!(
                    "Found amplifier-bundle-workgraph at: {}",
                    setup_path.parent().unwrap().display()
                );
                println!("  Run the setup script to install the executor and bundle:");
                println!("    {}", setup_path.display());
                println!();
                println!("  Then start sessions with: amplifier run -B workgraph");
            } else {
                println!(
                    "Spawned Amplifier agents need the workgraph bundle to understand wg commands."
                );
                println!();
                println!("  Install the bundle:");
                println!(
                    "    git clone https://github.com/graphwork/amplifier-bundle-workgraph ~/amplifier-bundle-workgraph"
                );
                println!("    cd ~/amplifier-bundle-workgraph && ./setup.sh");
                println!();
                println!("  Or add it directly:");
                println!(
                    "    amplifier bundle add git+https://github.com/graphwork/amplifier-bundle-workgraph"
                );
                println!();
                println!("  Then start sessions with: amplifier run -B workgraph");
            }
            Ok("amplifier bundle — see instructions above".to_string())
        }
        _ => {
            println!("Custom executor selected. Make sure your agents know about wg commands.");
            println!("  For reference, see: wg quickstart");
            Ok(format!(
                "custom executor '{}' — manual setup needed",
                executor
            ))
        }
    }
}

/// Guide the user through configuring ~/.claude/CLAUDE.md.
/// Returns a status string for the summary.
fn guide_claude_md_install() -> Result<String> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let claude_md = PathBuf::from(&home).join(".claude/CLAUDE.md");

    if has_workgraph_directives(&claude_md) {
        return Ok("already configured ✓".to_string());
    }

    println!("Claude Code's built-in task and agent tools conflict with workgraph.");
    println!(
        "Configuring ~/.claude/CLAUDE.md suppresses them so Claude uses `wg` commands instead."
    );

    let action = if claude_md.exists() {
        "Append workgraph directives to"
    } else {
        "Create"
    };

    let install = Confirm::new()
        .with_prompt(format!("{} ~/.claude/CLAUDE.md? (recommended)", action))
        .default(true)
        .interact()?;

    if install {
        let (status, _changed) = configure_claude_md()?;
        println!("  {}", status);
        Ok("configured ✓".to_string())
    } else {
        println!("  You can configure it later with: wg setup");
        Ok("NOT configured — Claude may use its own task tools".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::config::Config;

    #[test]
    fn test_build_config_defaults() {
        let choices = SetupChoices {
            executor: "claude".to_string(),

            model: "opus".to_string(),
            agency_enabled: true,
            evaluator_model: Some("sonnet".to_string()),
            assigner_model: Some("haiku".to_string()),
            max_agents: 4,
        };

        let config = build_config(&choices, None);
        assert_eq!(config.coordinator.executor, "claude");
        assert_eq!(config.agent.executor, "claude");
        assert_eq!(config.agent.model, "opus");
        assert_eq!(config.coordinator.model, Some("opus".to_string()));
        assert_eq!(config.coordinator.max_agents, 4);
        assert!(config.agency.auto_assign);
        assert!(config.agency.auto_evaluate);
        assert_eq!(config.agency.evaluator_model, Some("sonnet".to_string()));
        assert_eq!(config.agency.assigner_model, Some("haiku".to_string()));
    }

    #[test]
    fn test_build_config_amplifier() {
        let choices = SetupChoices {
            executor: "amplifier".to_string(),

            model: "sonnet".to_string(),
            agency_enabled: false,
            evaluator_model: None,
            assigner_model: None,
            max_agents: 8,
        };

        let config = build_config(&choices, None);
        assert_eq!(config.coordinator.executor, "amplifier");
        assert_eq!(config.agent.executor, "amplifier");
        assert_eq!(config.agent.model, "sonnet");
        assert_eq!(config.coordinator.max_agents, 8);
        assert!(!config.agency.auto_assign);
        assert!(!config.agency.auto_evaluate);
        assert!(config.agency.evaluator_model.is_none());
        assert!(config.agency.assigner_model.is_none());
    }

    #[test]
    fn test_build_config_preserves_base() {
        let mut base = Config::default();
        base.project.name = Some("my-project".to_string());
        base.agency.retention_heuristics = Some("keep good ones".to_string());
        base.log.rotation_threshold = 5_000_000;

        let choices = SetupChoices {
            executor: "claude".to_string(),

            model: "haiku".to_string(),
            agency_enabled: true,
            evaluator_model: Some("sonnet".to_string()),
            assigner_model: None,
            max_agents: 2,
        };

        let config = build_config(&choices, Some(&base));
        // Wizard-set values
        assert_eq!(config.agent.model, "haiku");
        assert_eq!(config.coordinator.max_agents, 2);
        assert!(config.agency.auto_assign);
        assert_eq!(config.agency.evaluator_model, Some("sonnet".to_string()));

        // Preserved from base
        assert_eq!(config.project.name, Some("my-project".to_string()));
        assert_eq!(
            config.agency.retention_heuristics,
            Some("keep good ones".to_string())
        );
        assert_eq!(config.log.rotation_threshold, 5_000_000);
    }

    #[test]
    fn test_build_config_agency_disabled() {
        let choices = SetupChoices {
            executor: "claude".to_string(),

            model: "opus".to_string(),
            agency_enabled: false,
            evaluator_model: None,
            assigner_model: None,
            max_agents: 4,
        };

        let config = build_config(&choices, None);
        assert!(!config.agency.auto_assign);
        assert!(!config.agency.auto_evaluate);
        assert!(config.agency.evaluator_model.is_none());
        assert!(config.agency.assigner_model.is_none());
    }

    #[test]
    fn test_build_config_same_as_default_models() {
        // When user picks "same as default", evaluator/assigner models are None
        let choices = SetupChoices {
            executor: "claude".to_string(),

            model: "sonnet".to_string(),
            agency_enabled: true,
            evaluator_model: None,
            assigner_model: None,
            max_agents: 4,
        };

        let config = build_config(&choices, None);
        assert!(config.agency.auto_assign);
        assert!(config.agency.auto_evaluate);
        assert!(config.agency.evaluator_model.is_none());
        assert!(config.agency.assigner_model.is_none());
    }

    #[test]
    fn test_format_summary_basic() {
        let choices = SetupChoices {
            executor: "claude".to_string(),

            model: "opus".to_string(),
            agency_enabled: true,
            evaluator_model: Some("sonnet".to_string()),
            assigner_model: Some("haiku".to_string()),
            max_agents: 4,
        };

        let summary = format_summary(&choices);
        assert!(summary.contains("executor = \"claude\""));
        assert!(summary.contains("model = \"opus\""));
        assert!(summary.contains("max_agents = 4"));
        assert!(summary.contains("auto_assign = true"));
        assert!(summary.contains("auto_evaluate = true"));
        assert!(summary.contains("evaluator_model = \"sonnet\""));
        assert!(summary.contains("assigner_model = \"haiku\""));
    }

    #[test]
    fn test_format_summary_agency_disabled() {
        let choices = SetupChoices {
            executor: "amplifier".to_string(),

            model: "sonnet".to_string(),
            agency_enabled: false,
            evaluator_model: None,
            assigner_model: None,
            max_agents: 8,
        };

        let summary = format_summary(&choices);
        assert!(summary.contains("executor = \"amplifier\""));
        assert!(summary.contains("auto_assign = false"));
        assert!(summary.contains("auto_evaluate = false"));
        assert!(!summary.contains("evaluator_model"));
        assert!(!summary.contains("assigner_model"));
    }

    #[test]
    fn test_build_config_roundtrip_through_toml() {
        let choices = SetupChoices {
            executor: "claude".to_string(),

            model: "opus".to_string(),
            agency_enabled: true,
            evaluator_model: Some("sonnet".to_string()),
            assigner_model: Some("haiku".to_string()),
            max_agents: 6,
        };

        let config = build_config(&choices, None);
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let reloaded: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(reloaded.coordinator.executor, "claude");
        assert_eq!(reloaded.agent.model, "opus");
        assert_eq!(reloaded.coordinator.max_agents, 6);
        assert!(reloaded.agency.auto_assign);
        assert!(reloaded.agency.auto_evaluate);
        assert_eq!(reloaded.agency.evaluator_model, Some("sonnet".to_string()));
        assert_eq!(reloaded.agency.assigner_model, Some("haiku".to_string()));
    }

    #[test]
    fn test_format_summary_includes_executor_and_model() {
        let choices = SetupChoices {
            executor: "claude".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: false,
            evaluator_model: None,
            assigner_model: None,
            max_agents: 3,
        };
        let summary = format_summary(&choices);
        assert!(summary.contains("executor = \"claude\""));
        assert!(summary.contains("model = \"sonnet\""));
        assert!(summary.contains("max_agents = 3"));
    }

    #[test]
    fn test_is_claude_skill_installed_returns_bool() {
        // Just verify the function runs without panicking.
        // Actual result depends on the test environment.
        let _installed = super::is_claude_skill_installed();
    }

    #[test]
    fn test_build_config_custom_executor() {
        let choices = SetupChoices {
            executor: "my-custom-executor".to_string(),

            model: "haiku".to_string(),
            agency_enabled: false,
            evaluator_model: None,
            assigner_model: None,
            max_agents: 1,
        };

        let config = build_config(&choices, None);
        assert_eq!(config.coordinator.executor, "my-custom-executor");
        assert_eq!(config.agent.executor, "my-custom-executor");
    }

    #[test]
    fn test_configure_claude_md_creates_new_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");

        let (status, changed) = configure_claude_md_at(&claude_md).unwrap();
        assert!(changed);
        assert!(status.contains("Created"));

        let content = std::fs::read_to_string(&claude_md).unwrap();
        assert!(content.contains(CLAUDE_MD_MARKER));
        assert!(content.contains("Do NOT use built-in TaskCreate"));
        assert!(content.contains("Do NOT use the built-in **Task tool**"));
        assert!(content.contains("wg quickstart"));
    }

    #[test]
    fn test_configure_claude_md_appends_to_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");

        let existing_content = "# My Existing Config\n\nSome custom rules here.\n";
        std::fs::write(&claude_md, existing_content).unwrap();

        let (status, changed) = configure_claude_md_at(&claude_md).unwrap();
        assert!(changed);
        assert!(status.contains("Updated"));

        let content = std::fs::read_to_string(&claude_md).unwrap();
        // Original content preserved
        assert!(content.contains("# My Existing Config"));
        assert!(content.contains("Some custom rules here."));
        // Workgraph directives appended
        assert!(content.contains(CLAUDE_MD_MARKER));
        assert!(content.contains("Do NOT use built-in TaskCreate"));
    }

    #[test]
    fn test_configure_claude_md_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");

        // First call creates
        let (_status, changed1) = configure_claude_md_at(&claude_md).unwrap();
        assert!(changed1);

        let content_after_first = std::fs::read_to_string(&claude_md).unwrap();

        // Second call is a no-op
        let (status, changed2) = configure_claude_md_at(&claude_md).unwrap();
        assert!(!changed2);
        assert!(status.contains("already configured"));

        let content_after_second = std::fs::read_to_string(&claude_md).unwrap();
        assert_eq!(content_after_first, content_after_second);
    }

    #[test]
    fn test_configure_claude_md_idempotent_with_existing_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");

        std::fs::write(&claude_md, "# Pre-existing\n").unwrap();

        let (_status, changed1) = configure_claude_md_at(&claude_md).unwrap();
        assert!(changed1);

        let content_after_first = std::fs::read_to_string(&claude_md).unwrap();

        // Second call doesn't duplicate
        let (_status, changed2) = configure_claude_md_at(&claude_md).unwrap();
        assert!(!changed2);

        let content_after_second = std::fs::read_to_string(&claude_md).unwrap();
        assert_eq!(content_after_first, content_after_second);
        assert_eq!(
            content_after_second.matches(CLAUDE_MD_MARKER).count(),
            1,
            "marker should appear exactly once"
        );
    }

    #[test]
    fn test_configure_claude_md_creates_parent_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_md = tmp.path().join("nested").join("dir").join("CLAUDE.md");

        let (_, changed) = configure_claude_md_at(&claude_md).unwrap();
        assert!(changed);
        assert!(claude_md.exists());
    }

    #[test]
    fn test_has_workgraph_directives_false_for_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");
        assert!(!has_workgraph_directives(&claude_md));
    }

    #[test]
    fn test_has_workgraph_directives_false_for_plain_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");
        std::fs::write(&claude_md, "# Just some markdown\n").unwrap();
        assert!(!has_workgraph_directives(&claude_md));
    }

    #[test]
    fn test_has_workgraph_directives_true_after_configure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");
        configure_claude_md_at(&claude_md).unwrap();
        assert!(has_workgraph_directives(&claude_md));
    }

    #[test]
    fn test_configure_project_claude_md() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project_dir = tmp.path();

        let (status, changed) = configure_project_claude_md(project_dir).unwrap();
        assert!(changed);
        assert!(status.contains("Created"));

        let claude_md = project_dir.join("CLAUDE.md");
        let content = std::fs::read_to_string(&claude_md).unwrap();
        assert!(content.contains(CLAUDE_MD_MARKER));
        assert!(content.contains("wg quickstart"));
    }

    #[test]
    fn test_claude_md_directives_contain_critical_rules() {
        // Verify the template contains all the critical rules from the task description
        assert!(CLAUDE_MD_DIRECTIVES.contains("TaskCreate"));
        assert!(CLAUDE_MD_DIRECTIVES.contains("TaskUpdate"));
        assert!(CLAUDE_MD_DIRECTIVES.contains("TaskList"));
        assert!(CLAUDE_MD_DIRECTIVES.contains("TaskGet"));
        assert!(CLAUDE_MD_DIRECTIVES.contains("Task tool"));
        assert!(CLAUDE_MD_DIRECTIVES.contains("subagent"));
        assert!(CLAUDE_MD_DIRECTIVES.contains("wg quickstart"));
        assert!(CLAUDE_MD_DIRECTIVES.contains("Orchestrating agent"));
        assert!(CLAUDE_MD_DIRECTIVES.contains("wg service start"));
    }
}
