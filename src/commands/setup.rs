//! Interactive configuration wizard for first-time workgraph setup.
//!
//! Creates/updates ~/.workgraph/config.toml via guided prompts using dialoguer.

use anyhow::{bail, Result};
use dialoguer::{Confirm, Input, Select};
use std::io::IsTerminal;
use workgraph::config::Config;

/// Choices gathered from the interactive wizard.
#[derive(Debug, Clone)]
pub struct SetupChoices {
    pub executor: String,
    pub api_key: Option<String>,
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
    lines.push(format!(
        "  model = \"{}\"",
        choices.model
    ));
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

    // 2. API key for amplifier
    let api_key = if executor == "amplifier" {
        let key: String = Input::new()
            .with_prompt("OpenRouter API key? (stored in config)")
            .allow_empty(true)
            .interact_text()?;
        if key.is_empty() { None } else { Some(key) }
    } else {
        None
    };

    // 3. Default model
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
        let eval_options = &["haiku (recommended, lightweight)", "sonnet", "same as default"];
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
        api_key,
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

    println!();
    println!("Setup complete. Run `wg init` in a project directory to get started.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::config::Config;

    #[test]
    fn test_build_config_defaults() {
        let choices = SetupChoices {
            executor: "claude".to_string(),
            api_key: None,
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
            api_key: Some("sk-test-key".to_string()),
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
            api_key: None,
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
            api_key: None,
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
            api_key: None,
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
            api_key: None,
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
            api_key: None,
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
            api_key: None,
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
    fn test_build_config_custom_executor() {
        let choices = SetupChoices {
            executor: "my-custom-executor".to_string(),
            api_key: None,
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
}
