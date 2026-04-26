//! Interactive configuration wizard for first-time workgraph setup.
//!
//! Creates/updates ~/.workgraph/config.toml via guided prompts using dialoguer.

use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, Input, Select};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use workgraph::config::{Config, EndpointConfig, ModelRegistryEntry, Tier};
use workgraph::models::ModelRegistry;
use workgraph::notify::config as notify_config;

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

/// CLI arguments for `wg setup` (non-interactive mode).
#[derive(Debug, Clone, Default)]
pub struct SetupArgs {
    /// Provider name: "anthropic", "openrouter", "openai", "local", "custom"
    pub provider: Option<String>,
    /// Path to API key file
    pub api_key_file: Option<String>,
    /// Environment variable name for API key
    pub api_key_env: Option<String>,
    /// API endpoint URL
    pub url: Option<String>,
    /// Default model ID
    pub model: Option<String>,
    /// Skip API key validation
    pub skip_validation: bool,
}

/// Choices gathered from the interactive wizard.
#[derive(Debug, Clone)]
pub struct SetupChoices {
    pub provider: String,
    pub executor: String,
    pub model: String,
    pub agency_enabled: bool,
    pub max_agents: usize,
    /// Endpoint config for non-Anthropic providers
    pub endpoint: Option<EndpointChoices>,
    /// Model registry entries to add
    pub model_registry_entries: Vec<ModelRegistryEntry>,
}

/// Endpoint configuration gathered from the wizard.
#[derive(Debug, Clone)]
pub struct EndpointChoices {
    pub name: String,
    pub provider: String,
    pub url: String,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
}

/// Build a Config from wizard choices, optionally layered on top of an existing config.
pub fn build_config(choices: &SetupChoices, base: Option<&Config>) -> Config {
    use workgraph::config::{EndpointConfig, EndpointsConfig, RoleModelConfig};

    let mut config = base.cloned().unwrap_or_default();

    config.coordinator.executor = Some(choices.executor.clone());
    config.agent.executor = choices.executor.clone();

    // Build provider:model format if the model doesn't already include a provider prefix
    let model_spec = if choices.model.contains(':') {
        choices.model.clone()
    } else {
        // Map provider name to provider prefix for model spec
        let prefix = match choices.provider.as_str() {
            "anthropic" => "claude",
            other => other,
        };
        format!("{}:{}", prefix, choices.model)
    };

    config.agent.model = model_spec.clone();
    config.coordinator.model = Some(model_spec.clone());

    config.coordinator.max_agents = choices.max_agents;

    // Set models.default with provider:model format
    if choices.provider != "anthropic" {
        config.models.default = Some(RoleModelConfig {
            provider: None,
            model: Some(model_spec.clone()),
            tier: None,
            endpoint: None,
        });
    }

    // Configure endpoint
    if let Some(ref ep) = choices.endpoint {
        let endpoint = EndpointConfig {
            name: ep.name.clone(),
            provider: ep.provider.clone(),
            url: Some(ep.url.clone()),
            model: None,
            api_key: None,
            api_key_file: ep.api_key_file.clone(),
            api_key_env: ep.api_key_env.clone(),
            is_default: true,
            context_window: None,
        };
        config.llm_endpoints = EndpointsConfig {
            inherit_global: false,
            endpoints: vec![endpoint],
        };
    }

    // Add model registry entries
    if !choices.model_registry_entries.is_empty() {
        config.model_registry = choices.model_registry_entries.clone();
    }

    config.agency.auto_assign = choices.agency_enabled;
    config.agency.auto_evaluate = choices.agency_enabled;

    config
}

/// Format a summary of what will be written.
pub fn format_summary(choices: &SetupChoices) -> String {
    let mut lines = Vec::new();
    // Build provider:model format for summary
    let model_spec = if choices.model.contains(':') {
        choices.model.clone()
    } else {
        let prefix = match choices.provider.as_str() {
            "anthropic" => "claude",
            other => other,
        };
        format!("{}:{}", prefix, choices.model)
    };

    lines.push("[dispatcher]".to_string());
    lines.push(format!("  executor = \"{}\"", choices.executor));
    lines.push(format!("  model = \"{}\"", model_spec));
    lines.push(format!("  max_agents = {}", choices.max_agents));
    lines.push(String::new());
    lines.push("[agent]".to_string());
    lines.push(format!("  executor = \"{}\"", choices.executor));
    lines.push(format!("  model = \"{}\"", model_spec));
    if choices.provider != "anthropic" {
        lines.push(String::new());
        lines.push("[models.default]".to_string());
        lines.push(format!("  model = \"{}\"", model_spec));
    }
    if let Some(ref ep) = choices.endpoint {
        lines.push(String::new());
        lines.push("[[llm_endpoints.endpoints]]".to_string());
        lines.push(format!("  name = \"{}\"", ep.name));
        lines.push(format!("  provider = \"{}\"", ep.provider));
        lines.push(format!("  url = \"{}\"", ep.url));
        if let Some(ref env) = ep.api_key_env {
            lines.push(format!("  api_key_env = \"{}\"", env));
        }
        if let Some(ref file) = ep.api_key_file {
            lines.push(format!("  api_key_file = \"{}\"", file));
        }
        lines.push("  is_default = true".to_string());
    }
    if !choices.model_registry_entries.is_empty() {
        for entry in &choices.model_registry_entries {
            lines.push(String::new());
            lines.push("[[model_registry]]".to_string());
            lines.push(format!("  id = \"{}\"", entry.id));
            lines.push(format!("  provider = \"{}\"", entry.provider));
            lines.push(format!("  model = \"{}\"", entry.model));
        }
    }
    lines.push(String::new());
    lines.push("[agency]".to_string());
    lines.push(format!("  auto_assign = {}", choices.agency_enabled));
    lines.push(format!("  auto_evaluate = {}", choices.agency_enabled));
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

/// Result of an API key validation attempt.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether the key authenticated successfully
    pub success: bool,
    /// HTTP status code from the /models endpoint
    pub status_code: u16,
    /// Human-readable status message
    pub message: String,
    /// Raw model IDs returned by the API (empty if validation failed)
    pub model_ids: Vec<String>,
}

/// Validate an API key by hitting the provider's /models endpoint.
///
/// Returns a `ValidationResult` with connectivity and auth status plus the
/// list of model IDs the provider returned on success.
pub fn validate_api_key(
    provider: &str,
    api_key: &str,
    url: Option<&str>,
) -> Result<ValidationResult> {
    let base_url = url.unwrap_or_else(|| EndpointConfig::default_url_for_provider(provider));

    if base_url.is_empty() {
        return Ok(ValidationResult {
            success: false,
            status_code: 0,
            message: "No URL configured for provider".to_string(),
            model_ids: vec![],
        });
    }

    let models_url = match provider {
        "anthropic" => format!("{}/v1/models", base_url.trim_end_matches('/')),
        _ => format!("{}/models", base_url.trim_end_matches('/')),
    };

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let mut headers = HeaderMap::new();
    match provider {
        "anthropic" => {
            headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        _ => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", api_key))?,
            );
        }
    }

    let response = client.get(&models_url).headers(headers).send()?;
    let status_code = response.status().as_u16();

    if status_code == 401 || status_code == 403 {
        return Ok(ValidationResult {
            success: false,
            status_code,
            message: "Authentication failed — check your API key".to_string(),
            model_ids: vec![],
        });
    }

    if !response.status().is_success() {
        return Ok(ValidationResult {
            success: false,
            status_code,
            message: format!("API returned status {}", status_code),
            model_ids: vec![],
        });
    }

    // Parse model IDs from the response
    let body = response.text().unwrap_or_default();
    let model_ids = parse_model_ids_from_response(&body);

    Ok(ValidationResult {
        success: true,
        status_code,
        message: "Authentication successful".to_string(),
        model_ids,
    })
}

/// Parse model IDs from a JSON response body.
///
/// Supports both OpenAI-style `{"data": [{"id": "..."}]}` and
/// Anthropic-style `{"data": [{"id": "..."}]}` responses.
pub fn parse_model_ids_from_response(body: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };

    // Try {"data": [{"id": "..."}]} format (OpenAI / OpenRouter / Anthropic)
    if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
        return data
            .iter()
            .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
            .collect();
    }

    // Try {"models": [{"name": "..."}]} format (Ollama)
    if let Some(models) = json.get("models").and_then(|m| m.as_array()) {
        return models
            .iter()
            .filter_map(|m| {
                m.get("name")
                    .or_else(|| m.get("model"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect();
    }

    vec![]
}

/// Build model registry entries from discovered model IDs for a given provider.
pub fn build_registry_from_discovered(
    provider: &str,
    model_ids: &[String],
) -> Vec<ModelRegistryEntry> {
    model_ids
        .iter()
        .map(|id| {
            let tier = infer_tier_from_model_id(id);
            // Use a short alias: last segment of the model ID
            let alias = id.rsplit('/').next().unwrap_or(id).to_string();
            ModelRegistryEntry {
                id: alias,
                provider: provider.to_string(),
                model: id.clone(),
                tier,
                ..Default::default()
            }
        })
        .collect()
}

/// Infer a quality tier from a model ID string using common naming patterns.
pub fn infer_tier_from_model_id(model_id: &str) -> Tier {
    let lower = model_id.to_lowercase();

    // Premium tier: opus, large reasoning models
    if lower.contains("opus") || lower.contains("o1-pro") || lower.contains("o3-pro") {
        return Tier::Premium;
    }

    // Fast tier: haiku, mini, flash, small, nano, tiny
    if lower.contains("haiku")
        || lower.contains("mini")
        || lower.contains("flash")
        || lower.contains("nano")
        || lower.contains("tiny")
        || lower.contains("small")
    {
        return Tier::Fast;
    }

    // Everything else: standard
    Tier::Standard
}

/// Auto-map tier config from a set of model registry entries.
///
/// Picks one model per tier (fast, standard, premium). If multiple models
/// share a tier, the first one wins.
pub fn auto_map_tiers(entries: &[ModelRegistryEntry]) -> workgraph::config::TierConfig {
    let mut fast: Option<String> = None;
    let mut standard: Option<String> = None;
    let mut premium: Option<String> = None;

    for entry in entries {
        match entry.tier {
            Tier::Fast if fast.is_none() => fast = Some(entry.id.clone()),
            Tier::Standard if standard.is_none() => standard = Some(entry.id.clone()),
            Tier::Premium if premium.is_none() => premium = Some(entry.id.clone()),
            _ => {}
        }
    }

    workgraph::config::TierConfig {
        fast,
        standard,
        premium,
    }
}

/// Print a summary of what is already configured.
pub fn check_existing_config(config: &Config) -> String {
    let mut lines = Vec::new();

    // Endpoint status
    if config.llm_endpoints.endpoints.is_empty() {
        lines.push("  Endpoints: (none configured)".to_string());
    } else {
        for ep in &config.llm_endpoints.endpoints {
            let default_marker = if ep.is_default { " (default)" } else { "" };
            let key_status = match ep.resolve_api_key(None) {
                Ok(Some(_)) => "key present",
                _ => "no key",
            };
            lines.push(format!(
                "  Endpoint: {}{} [{}] — {}",
                ep.name, default_marker, ep.provider, key_status
            ));
        }
    }

    // Model config
    let model = config
        .coordinator
        .model
        .as_deref()
        .unwrap_or(&config.agent.model);
    if model.is_empty() || model == "sonnet" {
        lines.push("  Model: (default)".to_string());
    } else {
        lines.push(format!("  Model: {}", model));
    }

    // Tiers
    let has_tiers = config.tiers.fast.is_some()
        || config.tiers.standard.is_some()
        || config.tiers.premium.is_some();
    if has_tiers {
        lines.push(format!(
            "  Tiers: fast={}, standard={}, premium={}",
            config.tiers.fast.as_deref().unwrap_or("(unset)"),
            config.tiers.standard.as_deref().unwrap_or("(unset)"),
            config.tiers.premium.as_deref().unwrap_or("(unset)"),
        ));
    } else {
        lines.push("  Tiers: (not configured)".to_string());
    }

    // Model registry
    if config.model_registry.is_empty() {
        lines.push("  Registry: (empty)".to_string());
    } else {
        lines.push(format!(
            "  Registry: {} model(s)",
            config.model_registry.len()
        ));
    }

    // Agency
    if config.agency.auto_assign || config.agency.auto_evaluate {
        lines.push("  Agency: enabled".to_string());
    } else {
        lines.push("  Agency: disabled".to_string());
    }

    lines.join("\n")
}

/// Run setup in non-interactive mode using CLI flags.
pub fn run_non_interactive(args: &SetupArgs) -> Result<()> {
    let provider = args.provider.as_deref().unwrap_or("anthropic");

    let existing = Config::load_global()?.unwrap_or_default();
    let global_path = Config::global_config_path()?;

    // Determine endpoint URL
    let url = args
        .url
        .as_deref()
        .unwrap_or_else(|| EndpointConfig::default_url_for_provider(provider));

    // Resolve API key for validation
    let api_key = resolve_key_from_args(args)?;

    // Validate if we have a key and validation is not skipped
    let mut discovered_model_ids = Vec::new();
    if let Some(ref key) = api_key
        && !args.skip_validation
    {
        eprintln!("Validating API key for {} ...", provider);
        match validate_api_key(provider, key, Some(url)) {
            Ok(result) => {
                if result.success {
                    eprintln!(
                        "  \u{2713} {} (found {} models)",
                        result.message,
                        result.model_ids.len()
                    );
                    discovered_model_ids = result.model_ids;
                } else {
                    bail!(
                        "API key validation failed: {} (status {})",
                        result.message,
                        result.status_code
                    );
                }
            }
            Err(e) => {
                bail!("Could not connect to {} API: {}", provider, e);
            }
        }
    }

    // Build model registry from discovered or use defaults
    let model_registry_entries = if !discovered_model_ids.is_empty() {
        build_registry_from_discovered(provider, &discovered_model_ids)
    } else {
        vec![]
    };

    // Determine default model
    let model = args.model.as_deref().unwrap_or(match provider {
        "anthropic" => "sonnet",
        "openrouter" => "anthropic/claude-sonnet-4",
        "openai" => "gpt-4o",
        _ => "default",
    });

    // Determine executor
    let executor = match provider {
        "anthropic" => "claude",
        _ => "native",
    };

    // Build endpoint config for non-Anthropic providers
    let endpoint = if provider != "anthropic" {
        Some(EndpointChoices {
            name: provider.to_string(),
            provider: provider.to_string(),
            url: url.to_string(),
            api_key_env: args.api_key_env.clone(),
            api_key_file: args.api_key_file.clone(),
        })
    } else {
        None
    };

    let choices = SetupChoices {
        provider: provider.to_string(),
        executor: executor.to_string(),
        model: model.to_string(),
        agency_enabled: existing.agency.auto_assign,
        max_agents: existing.coordinator.max_agents,
        endpoint,
        model_registry_entries: model_registry_entries.clone(),
    };

    let mut config = build_config(&choices, Some(&existing));

    // Set tier mappings from discovered models
    if !model_registry_entries.is_empty() {
        config.tiers = auto_map_tiers(&model_registry_entries);
    }

    config.save_global()?;

    println!("Configuration written to {}", global_path.display());
    println!();
    println!("Summary:");
    println!("  Provider: {}", provider);
    println!("  Executor: {}", executor);
    println!("  Model:    {}", model);
    if !model_registry_entries.is_empty() {
        println!(
            "  Registry: {} model(s) discovered",
            model_registry_entries.len()
        );
    }
    if config.tiers.fast.is_some()
        || config.tiers.standard.is_some()
        || config.tiers.premium.is_some()
    {
        println!(
            "  Tiers:    fast={}, standard={}, premium={}",
            config.tiers.fast.as_deref().unwrap_or("(unset)"),
            config.tiers.standard.as_deref().unwrap_or("(unset)"),
            config.tiers.premium.as_deref().unwrap_or("(unset)"),
        );
    }

    Ok(())
}

/// Resolve an API key from SetupArgs (key file or env var).
fn resolve_key_from_args(args: &SetupArgs) -> Result<Option<String>> {
    if let Some(ref file_path) = args.api_key_file {
        let expanded = if file_path.starts_with('~') {
            if let Some(home) = dirs::home_dir() {
                home.join(file_path.strip_prefix("~/").unwrap_or(file_path))
            } else {
                PathBuf::from(file_path)
            }
        } else {
            PathBuf::from(file_path)
        };
        if expanded.exists() {
            let key = std::fs::read_to_string(&expanded)
                .with_context(|| format!("Failed to read key file: {}", expanded.display()))?;
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Ok(Some(key));
            }
        }
    }

    if let Some(ref env_var) = args.api_key_env
        && let Ok(key) = std::env::var(env_var)
    {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(Some(key));
        }
    }

    // Try provider-specific env vars
    let provider = args.provider.as_deref().unwrap_or("anthropic");
    for var_name in EndpointConfig::env_var_names_for_provider(provider) {
        if let Ok(key) = std::env::var(var_name) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Ok(Some(key));
            }
        }
    }

    Ok(None)
}

/// Resolve an API key from EndpointChoices (reading env var or key file).
fn resolve_endpoint_key(ep: &EndpointChoices) -> Option<String> {
    if let Some(ref env_var) = ep.api_key_env
        && let Ok(key) = std::env::var(env_var)
    {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }
    if let Some(ref file_path) = ep.api_key_file {
        let expanded = if file_path.starts_with('~') {
            if let Some(home) = dirs::home_dir() {
                home.join(file_path.strip_prefix("~/").unwrap_or(file_path))
            } else {
                PathBuf::from(file_path)
            }
        } else {
            PathBuf::from(file_path)
        };
        if let Ok(content) = std::fs::read_to_string(&expanded) {
            let key = content.trim().to_string();
            if !key.is_empty() {
                return Some(key);
            }
        }
    }
    None
}

/// Run the setup wizard, dispatching to interactive or non-interactive mode.
pub fn run_with_args(args: &SetupArgs) -> Result<()> {
    if !std::io::stdin().is_terminal() || args.provider.is_some() {
        return run_non_interactive(args);
    }
    run()
}

/// Run the interactive setup wizard.
pub fn run() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "wg setup requires an interactive terminal. Use --provider for non-interactive mode."
        );
    }

    // Load existing global config for defaults
    let existing = Config::load_global()?.unwrap_or_default();
    let global_path = Config::global_config_path()?;

    println!("Hey! Welcome to workgraph setup.");
    println!("We'll get you configured at {}", global_path.display());
    println!();

    // Auto-detection phase — show what's already in place
    let detection = detect_environment();
    println!("{}", format_detection_summary(&detection));
    println!();

    // If existing config, show what's there
    if detection.global_config {
        println!("Current configuration:");
        println!("{}", check_existing_config(&existing));
        println!();
    }

    // 1. Provider selection (primary decision point)
    let provider_options = &[
        "Anthropic (direct)",
        "OpenRouter",
        "OpenAI",
        "Local (Ollama/vLLM)",
        "Custom",
    ];
    let provider_keys = &["anthropic", "openrouter", "openai", "local", "custom"];

    // Smart default: use existing config, or infer from detected API keys
    let current_provider = existing.coordinator.provider.as_deref().unwrap_or({
        if detection.anthropic_key {
            "anthropic"
        } else if detection.openrouter_key {
            "openrouter"
        } else if detection.openai_key {
            "openai"
        } else {
            "anthropic"
        }
    });
    let current_provider_idx = provider_keys
        .iter()
        .position(|&p| p == current_provider)
        .unwrap_or(0);

    let provider_idx = Select::new()
        .with_prompt("Which LLM provider?")
        .items(provider_options)
        .default(current_provider_idx)
        .interact()?;

    let provider = provider_keys[provider_idx].to_string();

    // 2. Auto-set executor based on provider, with override option
    let default_executor = match provider.as_str() {
        "anthropic" => "claude",
        "openrouter" | "oai-compat" | "openai" | "local" => "native",
        _ => "native",
    };

    println!();
    let executor_ok = match (default_executor, detection.claude_cli, detection.amplifier) {
        ("claude", true, _) => {
            println!(
                "  Using '{}' executor — you've got the claude CLI, perfect.",
                default_executor
            );
            true
        }
        ("claude", false, true) => {
            println!("  Heads up: claude CLI isn't installed, but amplifier is.");
            println!("  You might want to switch the executor.");
            false
        }
        ("claude", false, false) => {
            println!("  Note: claude CLI isn't installed yet.");
            println!(
                "  You'll need it before running agents. Install from: https://docs.anthropic.com/claude-code"
            );
            true
        }
        _ => {
            println!(
                "  Using '{}' executor for {} provider.",
                default_executor, provider
            );
            true
        }
    };

    let override_executor = if executor_ok {
        Confirm::new()
            .with_prompt("Want to change the executor?")
            .default(false)
            .interact()?
    } else {
        Confirm::new()
            .with_prompt("Override executor?")
            .default(true)
            .interact()?
    };

    let executor = if override_executor {
        let executor_options = &["claude", "native", "amplifier", "custom"];
        let current_idx = executor_options
            .iter()
            .position(|&e| e == default_executor)
            .unwrap_or(0);
        let idx = Select::new()
            .with_prompt("Which executor backend?")
            .items(executor_options)
            .default(current_idx)
            .interact()?;
        if idx == 3 {
            let custom: String = Input::new()
                .with_prompt("Custom executor name")
                .interact_text()?;
            custom
        } else {
            executor_options[idx].to_string()
        }
    } else {
        default_executor.to_string()
    };

    // 3. Provider-specific configuration
    let (endpoint, mut model_registry_entries, model) = match provider.as_str() {
        "openrouter" => configure_openrouter(&existing)?,
        "openai" => configure_openai(&existing)?,
        "local" => configure_local(&existing)?,
        "custom" => configure_custom_provider(&existing)?,
        _ => configure_anthropic(&existing)?,
    };

    // 3b. Validate API key if an endpoint is configured
    if let Some(ref ep) = endpoint {
        let api_key = resolve_endpoint_key(ep);
        if let Some(ref key) = api_key {
            println!();
            println!("Validating API key...");
            match validate_api_key(&ep.provider, key, Some(&ep.url)) {
                Ok(result) if result.success => {
                    println!(
                        "  \u{2713} {} (found {} models)",
                        result.message,
                        result.model_ids.len()
                    );

                    // Offer to auto-discover models if we got a response
                    if !result.model_ids.is_empty() && model_registry_entries.is_empty() {
                        let discover = Confirm::new()
                            .with_prompt(format!(
                                "Register {} discovered models in the model registry?",
                                result.model_ids.len()
                            ))
                            .default(true)
                            .interact()?;
                        if discover {
                            model_registry_entries =
                                build_registry_from_discovered(&ep.provider, &result.model_ids);
                            println!(
                                "  Registered {} models with auto-detected tiers.",
                                model_registry_entries.len()
                            );
                        }
                    }
                }
                Ok(result) => {
                    println!(
                        "  \u{2717} {} (status {})",
                        result.message, result.status_code
                    );
                    let cont = Confirm::new()
                        .with_prompt("Continue anyway?")
                        .default(false)
                        .interact()?;
                    if !cont {
                        println!("Setup cancelled.");
                        return Ok(());
                    }
                }
                Err(e) => {
                    println!("  \u{2717} Connection failed: {}", e);
                    let cont = Confirm::new()
                        .with_prompt("Continue anyway?")
                        .default(false)
                        .interact()?;
                    if !cont {
                        println!("Setup cancelled.");
                        return Ok(());
                    }
                }
            }
        }
    }

    // 4. Agency
    println!();
    println!("Agency lets workgraph automatically match the best agent identity to each task");
    println!("and evaluate their work when done. It's the evolutionary identity system.");
    let agency_enabled = Confirm::new()
        .with_prompt("Enable agency?")
        .default(existing.agency.auto_assign || existing.agency.auto_evaluate)
        .interact()?;

    // 5. Max agents
    println!();
    println!("How many agents can work in parallel? More = faster, but uses more resources.");
    let max_agents: usize = Input::new()
        .with_prompt("Max parallel agents")
        .default(existing.coordinator.max_agents)
        .interact_text()?;

    let choices = SetupChoices {
        provider: provider.clone(),
        executor,
        model,
        agency_enabled,
        max_agents,
        endpoint,
        model_registry_entries,
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
    let mut config = build_config(&choices, Some(&existing));

    // Auto-map tiers from registry entries
    if !choices.model_registry_entries.is_empty() {
        config.tiers = auto_map_tiers(&choices.model_registry_entries);
    }

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

    // 7. Notification setup (optional)
    println!();
    println!("Notifications let workgraph ping you when tasks need attention,");
    println!("agents get stuck, or work is done.");
    let notify_status = guide_notification_setup()?;

    println!();
    println!("You're all set! Here's what we configured:");
    println!();
    println!("  Provider:       {}", choices.provider);
    println!("  Executor:       {}", choices.executor);
    println!("  Model:          {}", choices.model);
    println!("  Max agents:     {}", choices.max_agents);
    if choices.endpoint.is_some() {
        println!("  Endpoint:       configured");
    }
    println!(
        "  Agency:         {}",
        if choices.agency_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  Skill:          {}", skill_status);
    println!("  CLAUDE.md:      {}", claude_md_status);
    println!("  Notifications:  {}", notify_status);
    println!();
    println!("Run `wg init` in a project directory to get started, or `wg setup` again to update.");

    Ok(())
}

/// Configure OpenRouter provider: API key, model selection, endpoint.
fn configure_openrouter(
    existing: &Config,
) -> Result<(Option<EndpointChoices>, Vec<ModelRegistryEntry>, String)> {
    println!();
    println!("OpenRouter configuration");
    println!("────────────────────────");

    // API key setup
    let api_key_options = &[
        "Environment variable (OPENROUTER_API_KEY)",
        "Key file (e.g., ~/.config/openrouter/key)",
    ];
    let key_idx = Select::new()
        .with_prompt("How should the API key be provided?")
        .items(api_key_options)
        .default(0)
        .interact()?;

    let (api_key_env, api_key_file) = if key_idx == 0 {
        // Check if the env var is already set
        if std::env::var("OPENROUTER_API_KEY").is_ok() {
            println!("  OPENROUTER_API_KEY is set in your environment.");
        } else {
            println!("  Set OPENROUTER_API_KEY in your shell profile before running agents.");
            println!("  Example: export OPENROUTER_API_KEY=sk-or-...");
        }
        (Some("OPENROUTER_API_KEY".to_string()), None)
    } else {
        let default_path = "~/.config/openrouter/key".to_string();
        let key_path: String = Input::new()
            .with_prompt("Path to API key file")
            .default(default_path)
            .interact_text()?;
        println!("  Make sure the key file exists and contains your OpenRouter API key.");
        (None, Some(key_path))
    };

    // Model selection
    println!();
    let model_method_options = &[
        "Enter model ID manually",
        "Use popular defaults (Claude via OpenRouter)",
    ];
    let method_idx = Select::new()
        .with_prompt("How would you like to select models?")
        .items(model_method_options)
        .default(1)
        .interact()?;

    let (model, registry_entries) = if method_idx == 0 {
        // Manual model entry
        let current_model = existing
            .coordinator
            .model
            .as_deref()
            .unwrap_or("anthropic/claude-sonnet-4");
        let model_id: String = Input::new()
            .with_prompt("Default model ID (OpenRouter format, e.g., anthropic/claude-sonnet-4)")
            .default(current_model.to_string())
            .interact_text()?;

        let entry = ModelRegistryEntry {
            id: model_id.clone(),
            provider: "openrouter".to_string(),
            model: model_id.clone(),
            tier: workgraph::config::Tier::Standard,
            ..Default::default()
        };

        (model_id, vec![entry])
    } else {
        // Popular defaults
        let entries = default_openrouter_registry();
        let model_labels: Vec<String> = entries
            .iter()
            .map(|e| format!("{} — {}", e.id, e.model))
            .collect();

        let default_idx = entries.iter().position(|e| e.id == "sonnet").unwrap_or(0);

        let idx = Select::new()
            .with_prompt("Default model?")
            .items(&model_labels)
            .default(default_idx)
            .interact()?;

        let model = entries[idx].id.clone();
        (model, entries)
    };

    let endpoint = EndpointChoices {
        name: "openrouter".to_string(),
        provider: "openrouter".to_string(),
        url: "https://openrouter.ai/api/v1".to_string(),
        api_key_env,
        api_key_file,
    };

    Ok((Some(endpoint), registry_entries, model))
}

/// Configure OpenAI provider.
fn configure_openai(
    existing: &Config,
) -> Result<(Option<EndpointChoices>, Vec<ModelRegistryEntry>, String)> {
    println!();
    println!("OpenAI configuration");
    println!("────────────────────");

    let api_key_options = &["Environment variable (OPENAI_API_KEY)", "Key file"];
    let key_idx = Select::new()
        .with_prompt("How should the API key be provided?")
        .items(api_key_options)
        .default(0)
        .interact()?;

    let (api_key_env, api_key_file) = if key_idx == 0 {
        if std::env::var("OPENAI_API_KEY").is_ok() {
            println!("  OPENAI_API_KEY is set in your environment.");
        } else {
            println!("  Set OPENAI_API_KEY in your shell profile before running agents.");
        }
        (Some("OPENAI_API_KEY".to_string()), None)
    } else {
        let key_path: String = Input::new()
            .with_prompt("Path to API key file")
            .default("~/.config/openai/key".to_string())
            .interact_text()?;
        (None, Some(key_path))
    };

    let current_model = existing.coordinator.model.as_deref().unwrap_or("gpt-4o");
    let model_id: String = Input::new()
        .with_prompt("Default model ID")
        .default(current_model.to_string())
        .interact_text()?;

    let entry = ModelRegistryEntry {
        id: model_id.clone(),
        provider: "openai".to_string(),
        model: model_id.clone(),
        tier: workgraph::config::Tier::Standard,
        ..Default::default()
    };

    let endpoint = EndpointChoices {
        name: "openai".to_string(),
        provider: "openai".to_string(),
        url: "https://api.openai.com/v1".to_string(),
        api_key_env,
        api_key_file,
    };

    Ok((Some(endpoint), vec![entry], model_id))
}

/// Configure local provider (Ollama/vLLM).
fn configure_local(
    existing: &Config,
) -> Result<(Option<EndpointChoices>, Vec<ModelRegistryEntry>, String)> {
    println!();
    println!("Local LLM configuration (Ollama/vLLM)");
    println!("──────────────────────────────────────");

    let url: String = Input::new()
        .with_prompt("API endpoint URL")
        .default("http://localhost:11434/v1".to_string())
        .interact_text()?;

    let current_model = existing.coordinator.model.as_deref().unwrap_or("llama3");
    let model_id: String = Input::new()
        .with_prompt("Default model ID")
        .default(current_model.to_string())
        .interact_text()?;

    let entry = ModelRegistryEntry {
        id: model_id.clone(),
        provider: "local".to_string(),
        model: model_id.clone(),
        tier: workgraph::config::Tier::Standard,
        ..Default::default()
    };

    let endpoint = EndpointChoices {
        name: "local".to_string(),
        provider: "local".to_string(),
        url,
        api_key_env: None,
        api_key_file: None,
    };

    Ok((Some(endpoint), vec![entry], model_id))
}

/// Configure custom provider.
fn configure_custom_provider(
    existing: &Config,
) -> Result<(Option<EndpointChoices>, Vec<ModelRegistryEntry>, String)> {
    println!();
    println!("Custom provider configuration");
    println!("─────────────────────────────");

    let provider_name: String = Input::new()
        .with_prompt("Provider name")
        .default("custom".to_string())
        .interact_text()?;

    let url: String = Input::new()
        .with_prompt("API endpoint URL")
        .interact_text()?;

    let api_key_env: String = Input::new()
        .with_prompt("Environment variable for API key (leave empty for none)")
        .default(String::new())
        .interact_text()?;

    let current_model = existing.coordinator.model.as_deref().unwrap_or("default");
    let model_id: String = Input::new()
        .with_prompt("Default model ID")
        .default(current_model.to_string())
        .interact_text()?;

    let entry = ModelRegistryEntry {
        id: model_id.clone(),
        provider: provider_name.clone(),
        model: model_id.clone(),
        tier: workgraph::config::Tier::Standard,
        ..Default::default()
    };

    let endpoint = EndpointChoices {
        name: provider_name.clone(),
        provider: provider_name,
        url,
        api_key_env: if api_key_env.is_empty() {
            None
        } else {
            Some(api_key_env)
        },
        api_key_file: None,
    };

    Ok((Some(endpoint), vec![entry], model_id))
}

/// Configure Anthropic (direct) provider — uses existing model registry flow.
fn configure_anthropic(
    existing: &Config,
) -> Result<(Option<EndpointChoices>, Vec<ModelRegistryEntry>, String)> {
    println!();
    let registry = ModelRegistry::with_defaults();
    let model_options = registry.model_choices_with_descriptions();
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
        .position(|(name, _)| name == current_model)
        .unwrap_or(0);

    let model_idx = Select::new()
        .with_prompt("Default model for agents?")
        .items(&model_labels)
        .default(current_model_idx)
        .interact()?;

    let model = model_options[model_idx].0.clone();

    Ok((None, vec![], model))
}

/// Default OpenRouter model registry entries for Claude models.
fn default_openrouter_registry() -> Vec<ModelRegistryEntry> {
    vec![
        ModelRegistryEntry {
            id: "opus".to_string(),
            provider: "openrouter".to_string(),
            model: "anthropic/claude-opus-4".to_string(),
            tier: workgraph::config::Tier::Premium,
            context_window: 200_000,
            max_output_tokens: 32_000,
            ..Default::default()
        },
        ModelRegistryEntry {
            id: "sonnet".to_string(),
            provider: "openrouter".to_string(),
            model: "anthropic/claude-sonnet-4".to_string(),
            tier: workgraph::config::Tier::Standard,
            context_window: 200_000,
            max_output_tokens: 64_000,
            ..Default::default()
        },
        ModelRegistryEntry {
            id: "haiku".to_string(),
            provider: "openrouter".to_string(),
            model: "anthropic/claude-haiku-4".to_string(),
            tier: workgraph::config::Tier::Fast,
            context_window: 200_000,
            max_output_tokens: 8_192,
            ..Default::default()
        },
    ]
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

/// Result of auto-detecting what tools and configuration are already in place.
#[derive(Debug, Clone, Default)]
pub struct DetectionResult {
    /// Whether the `claude` CLI was found in PATH.
    pub claude_cli: bool,
    /// Version string of the `claude` CLI, if detected.
    pub claude_cli_version: Option<String>,
    /// Whether `amplifier` was found in PATH.
    pub amplifier: bool,
    /// Whether `git` was found in PATH.
    pub git: bool,
    /// Whether `tmux` was found in PATH.
    pub tmux: bool,
    /// Whether ANTHROPIC_API_KEY is set.
    pub anthropic_key: bool,
    /// Whether OPENROUTER_API_KEY is set.
    pub openrouter_key: bool,
    /// Whether OPENAI_API_KEY is set.
    pub openai_key: bool,
    /// Whether `.workgraph/config.toml` exists in current directory.
    pub local_config: bool,
    /// Whether `~/.workgraph/config.toml` exists.
    pub global_config: bool,
}

/// Check if a command is available in PATH by running `which <cmd>`.
pub fn is_command_available(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get the version string from `claude --version`.
pub fn get_claude_version() -> Option<String> {
    std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
        })
}

/// Run the auto-detection phase, checking for tools, API keys, and config files.
pub fn detect_environment() -> DetectionResult {
    let claude_cli = is_command_available("claude");
    let claude_cli_version = if claude_cli {
        get_claude_version()
    } else {
        None
    };

    let global_config = Config::global_config_path()
        .map(|p| p.exists())
        .unwrap_or(false);

    DetectionResult {
        claude_cli,
        claude_cli_version,
        amplifier: is_command_available("amplifier"),
        git: is_command_available("git"),
        tmux: is_command_available("tmux"),
        anthropic_key: std::env::var("ANTHROPIC_API_KEY")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false),
        openrouter_key: std::env::var("OPENROUTER_API_KEY")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false),
        openai_key: std::env::var("OPENAI_API_KEY")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false),
        local_config: std::path::Path::new(".workgraph/config.toml").exists(),
        global_config,
    }
}

/// Format a friendly, conversational summary of auto-detection results.
pub fn format_detection_summary(det: &DetectionResult) -> String {
    let mut lines = Vec::new();

    lines.push("Let's see what you've got...\n".to_string());

    // Tools
    if det.claude_cli {
        if let Some(ref ver) = det.claude_cli_version {
            lines.push(format!("  ✓ claude CLI — {} — nice!", ver));
        } else {
            lines.push("  ✓ claude CLI — installed, good to go!".to_string());
        }
    } else {
        lines.push("  ✗ claude CLI — not found (needed for executor=claude)".to_string());
    }

    if det.amplifier {
        lines.push("  ✓ amplifier — installed!".to_string());
    } else {
        lines.push("  · amplifier — not installed (optional)".to_string());
    }

    if det.git {
        lines.push("  ✓ git — yep!".to_string());
    } else {
        lines.push("  ✗ git — not found (required for worktree isolation)".to_string());
    }

    if det.tmux {
        lines.push("  ✓ tmux — ready for `wg server`!".to_string());
    } else {
        lines.push("  · tmux — not installed (needed for `wg server`)".to_string());
    }

    // API keys
    lines.push(String::new());
    let has_any_key = det.anthropic_key || det.openrouter_key || det.openai_key;
    if has_any_key {
        lines.push("  API keys detected:".to_string());
        if det.anthropic_key {
            lines.push("    ✓ ANTHROPIC_API_KEY — set!".to_string());
        }
        if det.openrouter_key {
            lines.push("    ✓ OPENROUTER_API_KEY — set!".to_string());
        }
        if det.openai_key {
            lines.push("    ✓ OPENAI_API_KEY — set!".to_string());
        }
    } else {
        lines.push("  No API keys detected in environment.".to_string());
        lines.push("  (That's fine — we can configure key files or env vars next.)".to_string());
    }

    // Config
    lines.push(String::new());
    if det.global_config {
        lines.push("  ✓ Global config exists — we'll update it with your choices.".to_string());
    } else {
        lines.push("  · No global config yet — we'll create one for you.".to_string());
    }
    if det.local_config {
        lines.push("  ✓ Project config found (.workgraph/config.toml).".to_string());
    }

    lines.join("\n")
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

/// Build a default notify.toml content string for a given channel.
pub fn build_notify_config(channel: &str) -> String {
    match channel {
        "telegram" => r#"[routing]
default = ["telegram"]
urgent = ["telegram"]
approval = ["telegram"]

[telegram]
# Get a bot token from @BotFather on Telegram
bot_token = ""
# Your chat or group ID
chat_id = ""
"#
        .to_string(),
        "slack" => r#"[routing]
default = ["slack"]
urgent = ["slack"]
approval = ["slack"]

[slack]
# Slack incoming webhook URL
webhook_url = ""
# Channel to post to (optional, uses webhook default)
channel = ""
"#
        .to_string(),
        "email" => r#"[routing]
default = ["email"]
digest = ["email"]

[email]
smtp_host = "smtp.gmail.com"
smtp_port = 587
from = ""
to = [""]
"#
        .to_string(),
        "webhook" => r#"[routing]
default = ["webhook"]

[webhook]
url = ""
"#
        .to_string(),
        _ => format!(
            "[routing]\ndefault = [\"{channel}\"]\n\n[{channel}]\n# Configure your {channel} integration here\n"
        ),
    }
}

/// Run the notification setup interactively.
/// Returns a status string for the summary.
fn guide_notification_setup() -> Result<String> {
    let config_path = notify_config::default_config_path();

    // Check if already configured
    if let Some(ref path) = config_path
        && path.exists()
        && let Ok(Some(existing)) = notify_config::NotifyConfig::load_default()
    {
        let summary = existing.status_summary();
        println!("  Notifications already configured:");
        for line in summary.lines() {
            println!("    {}", line);
        }

        let update = Confirm::new()
            .with_prompt("Reconfigure notifications?")
            .default(false)
            .interact()?;
        if !update {
            return Ok("already configured ✓".to_string());
        }
    }

    let channel_options = &[
        "Telegram",
        "Slack",
        "Email (SMTP)",
        "Webhook (generic)",
        "Skip — I'll set this up later",
    ];
    let channel_keys = &["telegram", "slack", "email", "webhook", "skip"];

    let idx = Select::new()
        .with_prompt("How should workgraph notify you?")
        .items(channel_options)
        .default(4)
        .interact()?;

    let channel = channel_keys[idx];
    if channel == "skip" {
        return Ok("skipped".to_string());
    }

    let config_content = build_notify_config(channel);

    let Some(path) = config_path else {
        println!("  Could not determine config directory. Skipping.");
        return Ok("skipped (no config dir)".to_string());
    };

    // Ensure parent dir exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    std::fs::write(&path, &config_content)
        .with_context(|| format!("Failed to write {}", path.display()))?;

    println!("  Wrote template to {}", path.display());
    println!(
        "  Edit the file to fill in your {} credentials, then notifications will work automatically.",
        channel
    );

    Ok(format!("{} template written ✓", channel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::config::{CLAUDE_SONNET_MODEL_ID, Config};

    #[test]
    fn test_build_config_defaults() {
        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "claude".to_string(),
            model: "opus".to_string(),
            agency_enabled: true,
            max_agents: 4,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let config = build_config(&choices, None);
        assert_eq!(config.coordinator.executor, Some("claude".to_string()));
        assert_eq!(config.agent.executor, "claude");
        assert_eq!(config.agent.model, "claude:opus");
        assert_eq!(config.coordinator.model, Some("claude:opus".to_string()));
        assert_eq!(config.coordinator.max_agents, 4);
        assert!(config.agency.auto_assign);
        assert!(config.agency.auto_evaluate);
    }

    #[test]
    fn test_build_config_amplifier() {
        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "amplifier".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: false,
            max_agents: 8,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let config = build_config(&choices, None);
        assert_eq!(config.coordinator.executor, Some("amplifier".to_string()));
        assert_eq!(config.agent.executor, "amplifier");
        assert_eq!(config.agent.model, "claude:sonnet");
        assert_eq!(config.coordinator.max_agents, 8);
        assert!(!config.agency.auto_assign);
        assert!(!config.agency.auto_evaluate);
    }

    #[test]
    fn test_build_config_preserves_base() {
        let mut base = Config::default();
        base.project.name = Some("my-project".to_string());
        base.agency.retention_heuristics = Some("keep good ones".to_string());
        base.log.rotation_threshold = 5_000_000;

        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "claude".to_string(),
            model: "haiku".to_string(),
            agency_enabled: true,
            max_agents: 2,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let config = build_config(&choices, Some(&base));
        // Wizard-set values
        assert_eq!(config.agent.model, "claude:haiku");
        assert_eq!(config.coordinator.max_agents, 2);
        assert!(config.agency.auto_assign);

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
            provider: "anthropic".to_string(),
            executor: "claude".to_string(),
            model: "opus".to_string(),
            agency_enabled: false,
            max_agents: 4,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let config = build_config(&choices, None);
        assert!(!config.agency.auto_assign);
        assert!(!config.agency.auto_evaluate);
    }

    #[test]
    fn test_build_config_same_as_default_models() {
        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "claude".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: true,
            max_agents: 4,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let config = build_config(&choices, None);
        assert!(config.agency.auto_assign);
        assert!(config.agency.auto_evaluate);
    }

    #[test]
    fn test_format_summary_basic() {
        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "claude".to_string(),
            model: "opus".to_string(),
            agency_enabled: true,
            max_agents: 4,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let summary = format_summary(&choices);
        assert!(summary.contains("executor = \"claude\""));
        assert!(summary.contains("model = \"claude:opus\""));
        assert!(summary.contains("max_agents = 4"));
        assert!(summary.contains("auto_assign = true"));
        assert!(summary.contains("auto_evaluate = true"));
    }

    #[test]
    fn test_format_summary_agency_disabled() {
        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "amplifier".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: false,
            max_agents: 8,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let summary = format_summary(&choices);
        assert!(summary.contains("executor = \"amplifier\""));
        assert!(summary.contains("auto_assign = false"));
        assert!(summary.contains("auto_evaluate = false"));
    }

    #[test]
    fn test_build_config_roundtrip_through_toml() {
        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "claude".to_string(),
            model: "opus".to_string(),
            agency_enabled: true,
            max_agents: 6,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let config = build_config(&choices, None);
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let reloaded: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(reloaded.coordinator.executor, Some("claude".to_string()));
        assert_eq!(reloaded.agent.model, "claude:opus");
        assert_eq!(reloaded.coordinator.max_agents, 6);
        assert!(reloaded.agency.auto_assign);
        assert!(reloaded.agency.auto_evaluate);
    }

    #[test]
    fn test_format_summary_includes_executor_and_model() {
        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "claude".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: false,
            max_agents: 3,
            endpoint: None,
            model_registry_entries: vec![],
        };
        let summary = format_summary(&choices);
        assert!(summary.contains("executor = \"claude\""));
        assert!(summary.contains("model = \"claude:sonnet\""));
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
            provider: "anthropic".to_string(),
            executor: "my-custom-executor".to_string(),
            model: "haiku".to_string(),
            agency_enabled: false,
            max_agents: 1,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let config = build_config(&choices, None);
        assert_eq!(
            config.coordinator.executor,
            Some("my-custom-executor".to_string())
        );
        assert_eq!(config.agent.executor, "my-custom-executor");
    }

    #[test]
    fn test_build_config_openrouter_provider() {
        let choices = SetupChoices {
            provider: "openrouter".to_string(),
            executor: "native".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: false,
            max_agents: 4,
            endpoint: Some(EndpointChoices {
                name: "openrouter".to_string(),
                provider: "openrouter".to_string(),
                url: "https://openrouter.ai/api/v1".to_string(),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                api_key_file: None,
            }),
            model_registry_entries: default_openrouter_registry(),
        };

        let config = build_config(&choices, None);

        // Executor — provider is now embedded in model spec, not a separate field
        assert_eq!(config.coordinator.executor, Some("native".to_string()));
        assert_eq!(config.agent.executor, "native");
        // coordinator.provider is no longer set (deprecated)

        // models.default — provider embedded in model spec
        let models_default = config.models.default.as_ref().unwrap();
        assert_eq!(models_default.provider, None);
        assert_eq!(models_default.model, Some("openrouter:sonnet".to_string()));

        // Endpoint
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.name, "openrouter");
        assert_eq!(ep.provider, "openrouter");
        assert_eq!(ep.url, Some("https://openrouter.ai/api/v1".to_string()));
        assert_eq!(ep.api_key_env, Some("OPENROUTER_API_KEY".to_string()));
        assert!(ep.is_default);

        // Model registry
        assert!(!config.model_registry.is_empty());
        let sonnet = config
            .model_registry
            .iter()
            .find(|e| e.id == "sonnet")
            .unwrap();
        assert_eq!(sonnet.provider, "openrouter");
        assert_eq!(sonnet.model, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn test_build_config_openrouter_roundtrip_toml() {
        let choices = SetupChoices {
            provider: "openrouter".to_string(),
            executor: "native".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: true,
            max_agents: 2,
            endpoint: Some(EndpointChoices {
                name: "openrouter".to_string(),
                provider: "openrouter".to_string(),
                url: "https://openrouter.ai/api/v1".to_string(),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                api_key_file: None,
            }),
            model_registry_entries: default_openrouter_registry(),
        };

        let config = build_config(&choices, None);
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let reloaded: Config = toml::from_str(&toml_str).unwrap();

        // Verify everything survives round-trip
        // coordinator.provider is deprecated and skip_serializing, so it won't round-trip
        assert_eq!(reloaded.coordinator.provider, None);
        assert_eq!(reloaded.coordinator.effective_executor(), "native");
        assert_eq!(reloaded.llm_endpoints.endpoints.len(), 1);
        assert!(!reloaded.model_registry.is_empty());
        let models_default = reloaded.models.default.as_ref().unwrap();
        // Provider is now embedded in model spec, not separate field
        assert_eq!(models_default.model, Some("openrouter:sonnet".to_string()));
        assert_eq!(models_default.provider, None);
    }

    #[test]
    fn test_format_summary_openrouter() {
        let choices = SetupChoices {
            provider: "openrouter".to_string(),
            executor: "native".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: false,
            max_agents: 4,
            endpoint: Some(EndpointChoices {
                name: "openrouter".to_string(),
                provider: "openrouter".to_string(),
                url: "https://openrouter.ai/api/v1".to_string(),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                api_key_file: None,
            }),
            model_registry_entries: default_openrouter_registry(),
        };

        let summary = format_summary(&choices);
        assert!(summary.contains("executor = \"native\""));
        assert!(summary.contains("provider = \"openrouter\""));
        assert!(summary.contains("[models.default]"));
        assert!(summary.contains("[[llm_endpoints.endpoints]]"));
        assert!(summary.contains("api_key_env = \"OPENROUTER_API_KEY\""));
        assert!(summary.contains("[[model_registry]]"));
    }

    #[test]
    fn test_format_summary_anthropic_no_extra_sections() {
        let choices = SetupChoices {
            provider: "anthropic".to_string(),
            executor: "claude".to_string(),
            model: "opus".to_string(),
            agency_enabled: false,
            max_agents: 4,
            endpoint: None,
            model_registry_entries: vec![],
        };

        let summary = format_summary(&choices);
        // Anthropic provider should NOT include extra sections
        assert!(!summary.contains("[models.default]"));
        assert!(!summary.contains("[[llm_endpoints.endpoints]]"));
        assert!(!summary.contains("[[model_registry]]"));
        assert!(!summary.contains("provider = "));
    }

    #[test]
    fn test_default_openrouter_registry() {
        let entries = default_openrouter_registry();
        assert_eq!(entries.len(), 3);

        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"opus"));
        assert!(ids.contains(&"sonnet"));
        assert!(ids.contains(&"haiku"));

        for entry in &entries {
            assert_eq!(entry.provider, "openrouter");
            assert!(entry.model.starts_with("anthropic/"));
        }
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

    // ── parse_model_ids_from_response ─────────────────────────────────

    #[test]
    fn test_parse_model_ids_openai_format() {
        let body = r#"{"data": [{"id": "gpt-4o"}, {"id": "gpt-3.5-turbo"}]}"#;
        let ids = parse_model_ids_from_response(body);
        assert_eq!(ids, vec!["gpt-4o", "gpt-3.5-turbo"]);
    }

    #[test]
    fn test_parse_model_ids_ollama_format() {
        let body = r#"{"models": [{"name": "llama3"}, {"name": "codellama"}]}"#;
        let ids = parse_model_ids_from_response(body);
        assert_eq!(ids, vec!["llama3", "codellama"]);
    }

    #[test]
    fn test_parse_model_ids_empty_data() {
        let body = r#"{"data": []}"#;
        let ids = parse_model_ids_from_response(body);
        assert!(ids.is_empty());
    }

    #[test]
    fn test_parse_model_ids_invalid_json() {
        let ids = parse_model_ids_from_response("not json");
        assert!(ids.is_empty());
    }

    #[test]
    fn test_parse_model_ids_missing_id_field() {
        let body = r#"{"data": [{"name": "test"}]}"#;
        let ids = parse_model_ids_from_response(body);
        assert!(ids.is_empty());
    }

    // ── infer_tier_from_model_id ──────────────────────────────────────

    #[test]
    fn test_infer_tier_opus_is_premium() {
        assert_eq!(infer_tier_from_model_id("claude-opus-4"), Tier::Premium);
        assert_eq!(
            infer_tier_from_model_id("anthropic/claude-opus-4"),
            Tier::Premium
        );
    }

    #[test]
    fn test_infer_tier_haiku_is_fast() {
        assert_eq!(infer_tier_from_model_id("claude-haiku-4"), Tier::Fast);
        assert_eq!(infer_tier_from_model_id("gpt-4o-mini"), Tier::Fast);
        assert_eq!(infer_tier_from_model_id("gemini-2.0-flash"), Tier::Fast);
    }

    #[test]
    fn test_infer_tier_sonnet_is_standard() {
        assert_eq!(infer_tier_from_model_id("claude-sonnet-4"), Tier::Standard);
        assert_eq!(infer_tier_from_model_id("gpt-4o"), Tier::Standard);
    }

    // ── auto_map_tiers ────────────────────────────────────────────────

    #[test]
    fn test_auto_map_tiers_all_tiers() {
        let entries = vec![
            ModelRegistryEntry {
                id: "fast-model".to_string(),
                tier: Tier::Fast,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "standard-model".to_string(),
                tier: Tier::Standard,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "premium-model".to_string(),
                tier: Tier::Premium,
                ..Default::default()
            },
        ];
        let tiers = auto_map_tiers(&entries);
        assert_eq!(tiers.fast, Some("fast-model".to_string()));
        assert_eq!(tiers.standard, Some("standard-model".to_string()));
        assert_eq!(tiers.premium, Some("premium-model".to_string()));
    }

    #[test]
    fn test_auto_map_tiers_first_wins() {
        let entries = vec![
            ModelRegistryEntry {
                id: "fast-1".to_string(),
                tier: Tier::Fast,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "fast-2".to_string(),
                tier: Tier::Fast,
                ..Default::default()
            },
        ];
        let tiers = auto_map_tiers(&entries);
        assert_eq!(tiers.fast, Some("fast-1".to_string()));
    }

    #[test]
    fn test_auto_map_tiers_empty() {
        let tiers = auto_map_tiers(&[]);
        assert!(tiers.fast.is_none());
        assert!(tiers.standard.is_none());
        assert!(tiers.premium.is_none());
    }

    #[test]
    fn test_auto_map_tiers_partial() {
        let entries = vec![ModelRegistryEntry {
            id: "only-standard".to_string(),
            tier: Tier::Standard,
            ..Default::default()
        }];
        let tiers = auto_map_tiers(&entries);
        assert!(tiers.fast.is_none());
        assert_eq!(tiers.standard, Some("only-standard".to_string()));
        assert!(tiers.premium.is_none());
    }

    // ── build_registry_from_discovered ────────────────────────────────

    #[test]
    fn test_build_registry_from_discovered() {
        let ids = vec![
            "anthropic/claude-opus-4".to_string(),
            "anthropic/claude-sonnet-4".to_string(),
            "anthropic/claude-haiku-4".to_string(),
        ];
        let entries = build_registry_from_discovered("openrouter", &ids);
        assert_eq!(entries.len(), 3);

        // Check aliases are short names
        assert_eq!(entries[0].id, "claude-opus-4");
        assert_eq!(entries[1].id, "claude-sonnet-4");
        assert_eq!(entries[2].id, "claude-haiku-4");

        // Check tiers are inferred
        assert_eq!(entries[0].tier, Tier::Premium);
        assert_eq!(entries[1].tier, Tier::Standard);
        assert_eq!(entries[2].tier, Tier::Fast);

        // Check provider is set
        for e in &entries {
            assert_eq!(e.provider, "openrouter");
        }
    }

    // ── check_existing_config ─────────────────────────────────────────

    #[test]
    fn test_check_existing_config_empty() {
        let config = Config::default();
        let status = check_existing_config(&config);
        assert!(status.contains("(none configured)"));
        assert!(status.contains("(not configured)"));
        assert!(status.contains("(empty)"));
    }

    #[test]
    fn test_check_existing_config_with_endpoint() {
        let mut config = Config::default();
        config
            .llm_endpoints
            .endpoints
            .push(workgraph::config::EndpointConfig {
                name: "my-ep".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: None,
                api_key: Some("sk-test".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            });
        let status = check_existing_config(&config);
        assert!(status.contains("my-ep"));
        assert!(status.contains("(default)"));
        assert!(status.contains("key present"));
    }

    #[test]
    fn test_check_existing_config_with_tiers() {
        let mut config = Config::default();
        config.tiers.fast = Some("haiku".to_string());
        config.tiers.standard = Some("sonnet".to_string());
        config.tiers.premium = Some("opus".to_string());
        let status = check_existing_config(&config);
        assert!(status.contains("fast=haiku"));
        assert!(status.contains("standard=sonnet"));
        assert!(status.contains("premium=opus"));
    }

    // ── validate_api_key (with mock server) ───────────────────────────

    fn mock_server(status: u16, body: &str) -> String {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}", addr.port());
        let body = body.to_string();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body,
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        url
    }

    #[test]
    fn test_validate_api_key_success() {
        let body = r#"{"data": [{"id": "gpt-4o"}, {"id": "gpt-3.5-turbo"}]}"#;
        let mock_url = mock_server(200, body);

        let result = validate_api_key("openai", "sk-test", Some(&mock_url)).unwrap();
        assert!(result.success);
        assert_eq!(result.status_code, 200);
        assert_eq!(result.model_ids.len(), 2);
        assert!(result.model_ids.contains(&"gpt-4o".to_string()));
    }

    #[test]
    fn test_validate_api_key_auth_failure() {
        let mock_url = mock_server(401, r#"{"error":"unauthorized"}"#);

        let result = validate_api_key("openai", "sk-bad", Some(&mock_url)).unwrap();
        assert!(!result.success);
        assert_eq!(result.status_code, 401);
        assert!(result.model_ids.is_empty());
        assert!(result.message.contains("Authentication failed"));
    }

    #[test]
    fn test_validate_api_key_forbidden() {
        let mock_url = mock_server(403, r#"{"error":"forbidden"}"#);

        let result = validate_api_key("openai", "sk-bad", Some(&mock_url)).unwrap();
        assert!(!result.success);
        assert_eq!(result.status_code, 403);
    }

    #[test]
    fn test_validate_api_key_server_error() {
        let mock_url = mock_server(500, r#"{"error":"internal"}"#);

        let result = validate_api_key("openai", "sk-test", Some(&mock_url)).unwrap();
        assert!(!result.success);
        assert_eq!(result.status_code, 500);
    }

    #[test]
    fn test_validate_api_key_connection_refused() {
        let result = validate_api_key("openai", "sk-test", Some("http://127.0.0.1:1"));
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_api_key_anthropic_uses_x_api_key() {
        // Just verify Anthropic path doesn't panic — actual header verification
        // would need a more sophisticated mock
        let body = format!(r#"{{"data": [{{"id": "{CLAUDE_SONNET_MODEL_ID}"}}]}}"#);
        let mock_url = mock_server(200, &body);

        let result = validate_api_key("anthropic", "sk-ant-test", Some(&mock_url)).unwrap();
        assert!(result.success);
        assert_eq!(result.model_ids.len(), 1);
    }

    // ── resolve_key_from_args ──────────────────────────────────────────

    #[test]
    fn test_resolve_key_from_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let key_file = tmp.path().join("api_key.txt");
        std::fs::write(&key_file, "sk-test-key\n").unwrap();

        let args = SetupArgs {
            api_key_file: Some(key_file.to_str().unwrap().to_string()),
            ..Default::default()
        };
        let key = resolve_key_from_args(&args).unwrap();
        assert_eq!(key, Some("sk-test-key".to_string()));
    }

    #[test]
    fn test_resolve_key_from_file_not_found() {
        let args = SetupArgs {
            api_key_file: Some("/nonexistent/path/key.txt".to_string()),
            ..Default::default()
        };
        let key = resolve_key_from_args(&args).unwrap();
        assert!(key.is_none());
    }

    #[test]
    fn test_resolve_key_from_file_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let key_file = tmp.path().join("empty_key.txt");
        std::fs::write(&key_file, "  \n").unwrap();

        let args = SetupArgs {
            api_key_file: Some(key_file.to_str().unwrap().to_string()),
            ..Default::default()
        };
        let key = resolve_key_from_args(&args).unwrap();
        assert!(key.is_none());
    }

    // ── build_config with tiers ───────────────────────────────────────

    #[test]
    fn test_build_config_sets_tiers_from_registry() {
        let entries = vec![
            ModelRegistryEntry {
                id: "haiku".to_string(),
                provider: "openrouter".to_string(),
                model: "anthropic/claude-haiku-4".to_string(),
                tier: Tier::Fast,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "sonnet".to_string(),
                provider: "openrouter".to_string(),
                model: "anthropic/claude-sonnet-4".to_string(),
                tier: Tier::Standard,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "opus".to_string(),
                provider: "openrouter".to_string(),
                model: "anthropic/claude-opus-4".to_string(),
                tier: Tier::Premium,
                ..Default::default()
            },
        ];

        let choices = SetupChoices {
            provider: "openrouter".to_string(),
            executor: "native".to_string(),
            model: "sonnet".to_string(),
            agency_enabled: false,
            max_agents: 4,
            endpoint: None,
            model_registry_entries: entries.clone(),
        };

        let mut config = build_config(&choices, None);
        config.tiers = auto_map_tiers(&entries);

        assert_eq!(config.tiers.fast, Some("haiku".to_string()));
        assert_eq!(config.tiers.standard, Some("sonnet".to_string()));
        assert_eq!(config.tiers.premium, Some("opus".to_string()));
    }

    // ── DetectionResult + format_detection_summary ───────────────────

    #[test]
    fn test_detection_result_default_all_false() {
        let det = DetectionResult::default();
        assert!(!det.claude_cli);
        assert!(!det.amplifier);
        assert!(!det.git);
        assert!(!det.tmux);
        assert!(!det.anthropic_key);
        assert!(!det.openrouter_key);
        assert!(!det.openai_key);
        assert!(!det.local_config);
        assert!(!det.global_config);
        assert!(det.claude_cli_version.is_none());
    }

    #[test]
    fn test_format_detection_summary_nothing_detected() {
        let det = DetectionResult::default();
        let summary = format_detection_summary(&det);
        assert!(summary.contains("Let's see what you've got"));
        assert!(summary.contains("✗ claude CLI"));
        assert!(summary.contains("not found"));
        assert!(summary.contains("· amplifier"));
        assert!(summary.contains("No API keys detected"));
        assert!(summary.contains("No global config yet"));
    }

    #[test]
    fn test_format_detection_summary_everything_detected() {
        let det = DetectionResult {
            claude_cli: true,
            claude_cli_version: Some("1.2.3".to_string()),
            amplifier: true,
            git: true,
            tmux: true,
            anthropic_key: true,
            openrouter_key: true,
            openai_key: true,
            local_config: true,
            global_config: true,
        };
        let summary = format_detection_summary(&det);
        assert!(summary.contains("claude CLI — 1.2.3 — nice!"));
        assert!(summary.contains("✓ amplifier"));
        assert!(summary.contains("✓ git"));
        assert!(summary.contains("✓ tmux"));
        assert!(summary.contains("ANTHROPIC_API_KEY — set!"));
        assert!(summary.contains("OPENROUTER_API_KEY — set!"));
        assert!(summary.contains("OPENAI_API_KEY — set!"));
        assert!(summary.contains("Global config exists"));
        assert!(summary.contains("Project config found"));
    }

    #[test]
    fn test_format_detection_summary_claude_without_version() {
        let det = DetectionResult {
            claude_cli: true,
            claude_cli_version: None,
            ..Default::default()
        };
        let summary = format_detection_summary(&det);
        assert!(summary.contains("claude CLI — installed, good to go!"));
    }

    #[test]
    fn test_format_detection_summary_partial_keys() {
        let det = DetectionResult {
            anthropic_key: true,
            ..Default::default()
        };
        let summary = format_detection_summary(&det);
        assert!(summary.contains("API keys detected:"));
        assert!(summary.contains("ANTHROPIC_API_KEY — set!"));
        // Should not mention keys that aren't set
        assert!(!summary.contains("OPENROUTER_API_KEY"));
        assert!(!summary.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn test_format_detection_git_missing_warning() {
        let det = DetectionResult::default();
        let summary = format_detection_summary(&det);
        assert!(summary.contains("✗ git — not found"));
    }

    #[test]
    fn test_is_command_available_returns_bool() {
        // `ls` should always be available on Unix
        assert!(is_command_available("ls"));
        // A garbage command should not
        assert!(!is_command_available(
            "this_command_definitely_does_not_exist_xyz_123"
        ));
    }

    // ── detect_environment (smoke test) ──────────────────────────────

    #[test]
    fn test_detect_environment_returns_something() {
        // Just verify it doesn't panic; actual values depend on environment
        let det = detect_environment();
        // git should be available in this repo's CI/dev environment
        assert!(det.git);
    }

    // ── build_notify_config ──────────────────────────────────────────

    #[test]
    fn test_build_notify_config_telegram() {
        let config = build_notify_config("telegram");
        assert!(config.contains("[routing]"));
        assert!(config.contains("[telegram]"));
        assert!(config.contains("bot_token"));
        assert!(config.contains("chat_id"));
        // Should be parseable
        let parsed: toml::Value = toml::from_str(&config).unwrap();
        let routing = parsed.get("routing").unwrap();
        assert!(routing.get("default").is_some());
    }

    #[test]
    fn test_build_notify_config_slack() {
        let config = build_notify_config("slack");
        assert!(config.contains("[slack]"));
        assert!(config.contains("webhook_url"));
        let _parsed: toml::Value = toml::from_str(&config).unwrap();
    }

    #[test]
    fn test_build_notify_config_email() {
        let config = build_notify_config("email");
        assert!(config.contains("[email]"));
        assert!(config.contains("smtp_host"));
        let _parsed: toml::Value = toml::from_str(&config).unwrap();
    }

    #[test]
    fn test_build_notify_config_webhook() {
        let config = build_notify_config("webhook");
        assert!(config.contains("[webhook]"));
        assert!(config.contains("url"));
        let _parsed: toml::Value = toml::from_str(&config).unwrap();
    }

    #[test]
    fn test_build_notify_config_unknown_channel() {
        let config = build_notify_config("mychannel");
        assert!(config.contains("[mychannel]"));
        assert!(config.contains("mychannel"));
        let _parsed: toml::Value = toml::from_str(&config).unwrap();
    }
}
