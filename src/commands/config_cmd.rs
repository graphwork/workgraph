//! Configuration management commands

use anyhow::Result;
use std::path::Path;
use workgraph::config::{
    Config, ConfigSource, EndpointConfig, MatrixConfig, ModelRegistryEntry, Tier,
};

/// Send an all-None Reconfigure IPC to the daemon to make it re-read
/// `config.toml` from disk. Returns `Ok(true)` when the daemon
/// acknowledged, `Ok(false)` when no daemon is running (benign — the
/// config change is already on disk for the next startup), or `Err`
/// when the daemon is running but the IPC failed.
#[cfg(unix)]
fn try_reload_daemon(dir: &Path) -> Result<bool> {
    use crate::commands::service;
    use crate::commands::service::{IpcRequest, ServiceState};

    // Short-circuit if no daemon state file exists — nothing to reload.
    match ServiceState::load(dir) {
        Ok(Some(_)) => {}
        _ => return Ok(false),
    }

    let req = IpcRequest::Reconfigure {
        max_agents: None,
        executor: None,
        poll_interval: None,
        model: None,
    };
    match service::send_request(dir, &req) {
        Ok(resp) if resp.ok => Ok(true),
        Ok(resp) => Err(anyhow::anyhow!(
            "daemon rejected reconfigure: {}",
            resp.error.unwrap_or_else(|| "unknown".to_string())
        )),
        Err(e) => {
            // "Service not running" / stale state → treat as no-op.
            let msg = format!("{:#}", e);
            if msg.contains("not running") || msg.contains("stale") {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(not(unix))]
fn try_reload_daemon(_dir: &Path) -> Result<bool> {
    Ok(false)
}

/// Scope for config operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    Local,
    Global,
}

/// Show current configuration
pub fn show(dir: &Path, scope: Option<ConfigScope>, json: bool) -> Result<()> {
    let config = match scope {
        Some(ConfigScope::Global) => Config::load_global()?.unwrap_or_default(),
        Some(ConfigScope::Local) => Config::load(dir)?,
        None => Config::load_merged(dir)?,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&config)?);
    } else {
        println!("Workgraph Configuration");
        println!("========================");
        println!();
        println!("[agent]");
        println!("  executor = \"{}\"", config.agent.executor);
        println!("  model = \"{}\"", config.agent.model);
        println!("  interval = {}", config.agent.interval);
        println!("  heartbeat_timeout = {}", config.agent.heartbeat_timeout);
        if let Some(max) = config.agent.max_tasks {
            println!("  max_tasks = {}", max);
        }
        println!();
        println!("[coordinator]");
        println!("  max_agents = {}", config.coordinator.max_agents);
        println!(
            "  max_coordinators = {}",
            config.coordinator.max_coordinators
        );
        println!("  interval = {}", config.coordinator.interval);
        println!("  poll_interval = {}", config.coordinator.poll_interval);
        println!(
            "  executor = \"{}\"",
            config.coordinator.effective_executor()
        );
        if let Some(ref m) = config.coordinator.model {
            println!("  model = \"{}\"", m);
        }
        println!();
        println!("[agency]");
        println!("  auto_evaluate = {}", config.agency.auto_evaluate);
        println!("  auto_assign = {}", config.agency.auto_assign);
        println!("  auto_create = {}", config.agency.auto_create);
        if let Some(ref agent) = config.agency.assigner_agent {
            println!("  assigner_agent = \"{}\"", agent);
        }
        if let Some(ref agent) = config.agency.evaluator_agent {
            println!("  evaluator_agent = \"{}\"", agent);
        }
        if let Some(ref agent) = config.agency.evolver_agent {
            println!("  evolver_agent = \"{}\"", agent);
        }
        if let Some(ref agent) = config.agency.creator_agent {
            println!("  creator_agent = \"{}\"", agent);
        }
        if let Some(ref heuristics) = config.agency.retention_heuristics {
            println!("  retention_heuristics = \"{}\"", heuristics);
        }
        println!("  auto_triage = {}", config.agency.auto_triage);
        if let Some(timeout) = config.agency.triage_timeout {
            println!("  triage_timeout = {}", timeout);
        }
        if let Some(max_bytes) = config.agency.triage_max_log_bytes {
            println!("  triage_max_log_bytes = {}", max_bytes);
        }
        if let Some(threshold) = config.agency.eval_gate_threshold {
            println!("  eval_gate_threshold = {}", threshold);
        }
        if config.agency.eval_gate_all {
            println!("  eval_gate_all = {}", config.agency.eval_gate_all);
        }
        if config.agency.flip_enabled {
            println!("  flip_enabled = {}", config.agency.flip_enabled);
        }
        if let Some(threshold) = config.agency.flip_verification_threshold {
            println!("  flip_verification_threshold = {}", threshold);
        }
        println!("  auto_place = {}", config.agency.auto_place);
        if config.agency.auto_evolve {
            println!("  auto_evolve = {}", config.agency.auto_evolve);
            println!(
                "  evolution_interval = {}",
                config.agency.evolution_interval
            );
            println!(
                "  evolution_threshold = {}",
                config.agency.evolution_threshold
            );
            println!("  evolution_budget = {}", config.agency.evolution_budget);
            println!(
                "  evolution_reactive_threshold = {}",
                config.agency.evolution_reactive_threshold
            );
        }
        println!();

        // Unified agency agents display
        {
            use workgraph::config::DispatchRole;
            println!("[agency agents]");

            // Helper to get auto-toggle status for applicable roles
            let auto_status = |role: &DispatchRole| -> Option<&str> {
                match role {
                    DispatchRole::Placer => Some(if config.agency.auto_place {
                        "on"
                    } else {
                        "off"
                    }),
                    DispatchRole::Assigner => Some(if config.agency.auto_assign {
                        "on"
                    } else {
                        "off"
                    }),
                    DispatchRole::Evaluator | DispatchRole::CoordinatorEval => {
                        Some(if config.agency.auto_evaluate {
                            "on"
                        } else {
                            "off"
                        })
                    }
                    DispatchRole::Creator => Some(if config.agency.auto_create {
                        "on"
                    } else {
                        "off"
                    }),
                    DispatchRole::Evolver => Some(if config.agency.auto_evolve {
                        "on"
                    } else {
                        "off"
                    }),
                    DispatchRole::Triage => Some(if config.agency.auto_triage {
                        "on"
                    } else {
                        "off"
                    }),
                    _ => None,
                }
            };

            for role in DispatchRole::ALL {
                let resolved = config.resolve_model_for_role(*role);
                let tier = role.default_tier();
                // Display as provider:id (e.g., "claude:opus") for consistency
                let display_model = if let Some(ref entry) = resolved.registry_entry {
                    let prefix = workgraph::config::native_provider_to_prefix(&entry.provider);
                    format!("{}:{}", prefix, entry.id)
                } else if let Some(ref provider) = resolved.provider {
                    let prefix = workgraph::config::native_provider_to_prefix(provider);
                    format!("{}:{}", prefix, resolved.model)
                } else {
                    resolved.model.clone()
                };

                let auto_str = match auto_status(role) {
                    Some(status) => format!(", auto: {}", status),
                    None => String::new(),
                };

                println!(
                    "  {:<14} = {:<10} (tier: {}{})",
                    role, display_model, tier, auto_str
                );
            }
        }
        println!();
        println!("[guardrails]");
        println!(
            "  max_child_tasks_per_agent = {}",
            config.guardrails.max_child_tasks_per_agent
        );
        println!("  max_task_depth = {}", config.guardrails.max_task_depth);
        println!();
        println!("[tui]");
        println!("  chat_history = {}", config.tui.chat_history);
        println!("  chat_history_max = {}", config.tui.chat_history_max);
        println!("  counters = \"{}\"", config.tui.counters);
        println!("  show_system_tasks = {}", config.tui.show_system_tasks);
        println!(
            "  show_running_system_tasks = {}",
            config.tui.show_running_system_tasks
        );
        println!();
        println!("[viz]");
        println!("  edge_color = \"{}\"", config.viz.edge_color);
        println!();
        if config.project.name.is_some() || config.project.description.is_some() {
            println!("[project]");
            if let Some(ref name) = config.project.name {
                println!("  name = \"{}\"", name);
            }
            if let Some(ref desc) = config.project.description {
                println!("  description = \"{}\"", desc);
            }
            println!();
        }
        // Display unified [models] section
        {
            use workgraph::config::DispatchRole;
            let has_any = config.models.default.is_some()
                || DispatchRole::ALL
                    .iter()
                    .any(|r| config.models.get_role(*r).is_some());
            if has_any {
                println!("[models]");
                if let Some(ref default_cfg) = config.models.default {
                    if let Some(ref m) = default_cfg.model {
                        println!("  default.model = \"{}\"", m);
                    }
                    if let Some(ref p) = default_cfg.provider {
                        println!("  default.provider = \"{}\"", p);
                    }
                }
                for role in DispatchRole::ALL {
                    if let Some(role_cfg) = config.models.get_role(*role) {
                        if let Some(ref m) = role_cfg.model {
                            println!("  {}.model = \"{}\"", role, m);
                        }
                        if let Some(ref p) = role_cfg.provider {
                            println!("  {}.provider = \"{}\"", role, p);
                        }
                        if let Some(ref t) = role_cfg.tier {
                            println!("  {}.tier = \"{}\"", role, t);
                        }
                    }
                }
                println!();
            }
        }

        // Health check
        let validation = config.validate_config();
        if validation.is_clean() {
            println!("[health check]");
            println!("  status = ok");
        } else {
            println!("[health check]");
            if validation.is_ok() {
                println!("  status = warnings");
            } else {
                println!("  status = errors");
            }
            print!("{}", validation.display());
        }
    }

    Ok(())
}

/// Initialize default config file
pub fn init(dir: &Path, scope: Option<ConfigScope>) -> Result<()> {
    if scope == Some(ConfigScope::Global) {
        if Config::init_global()? {
            let path = Config::global_config_path()?;
            println!("Created default global configuration at {}", path.display());
        } else {
            let path = Config::global_config_path()?;
            println!("Global configuration already exists at {}", path.display());
        }
    } else if Config::init(dir)? {
        println!("Created default configuration at .workgraph/config.toml");
    } else {
        println!("Configuration already exists at .workgraph/config.toml");
    }
    Ok(())
}

/// Update configuration values
#[allow(clippy::too_many_arguments)]
pub fn update(
    dir: &Path,
    scope: ConfigScope,
    executor: Option<&str>,
    model: Option<&str>,
    interval: Option<u64>,
    max_agents: Option<usize>,
    max_coordinators: Option<usize>,
    coordinator_interval: Option<u64>,
    poll_interval: Option<u64>,
    coordinator_executor: Option<&str>,
    coordinator_model: Option<&str>,
    coordinator_provider: Option<&str>,
    auto_evaluate: Option<bool>,
    auto_assign: Option<bool>,
    assigner_agent: Option<&str>,
    evaluator_agent: Option<&str>,
    evolver_agent: Option<&str>,
    creator_agent: Option<&str>,
    retention_heuristics: Option<&str>,
    auto_triage: Option<bool>,
    auto_place: Option<bool>,
    auto_create: Option<bool>,
    triage_timeout: Option<u64>,
    triage_max_log_bytes: Option<usize>,
    max_child_tasks: Option<u32>,
    max_task_depth: Option<u32>,
    viz_edge_color: Option<&str>,
    eval_gate_threshold: Option<f64>,
    eval_gate_all: Option<bool>,
    flip_enabled: Option<bool>,
    flip_verification_threshold: Option<f64>,
    chat_history: Option<bool>,
    chat_history_max: Option<usize>,
    tui_counters: Option<&str>,
    retry_context_tokens: Option<u32>,
    endpoint: Option<&str>,
    no_reload: bool,
) -> Result<()> {
    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(dir)?,
    };
    let mut changed = false;

    // Endpoint-driven update: shares semantics with `wg init -m/-e`.
    // Writes a default oai-compat endpoint entry + applies the `local:`
    // prefix to the model name so the provider:model validator accepts
    // it on reload. Model-only sets flow through the existing validated
    // agent.model / coordinator.model blocks further down (we re-check
    // here so the existing blocks don't double-apply when we already did).
    let endpoint_handled_model = if endpoint.is_some() {
        let summary = config.apply_model_endpoint(model, endpoint)?;
        for line in &summary {
            println!("Set {}", line);
        }
        changed = true;
        true
    } else {
        false
    };

    // Agent settings
    if let Some(exec) = executor {
        config.agent.executor = exec.to_string();
        println!("Set agent.executor = \"{}\"", exec);
        changed = true;
    }

    if let Some(m) = model
        && !endpoint_handled_model
    {
        // Validate provider:model format
        if let Err(e) = workgraph::config::parse_model_spec_strict(m) {
            anyhow::bail!(
                "Invalid model format: {}. Use provider:model format (e.g., 'claude:opus').",
                e
            );
        }
        config.agent.model = m.to_string();
        println!("Set agent.model = \"{}\"", m);
        changed = true;
    }

    if let Some(i) = interval {
        config.agent.interval = i;
        println!("Set agent.interval = {}", i);
        changed = true;
    }

    // Coordinator settings
    if let Some(max) = max_agents {
        config.coordinator.max_agents = max;
        println!("Set coordinator.max_agents = {}", max);
        changed = true;
    }

    if let Some(max) = max_coordinators {
        config.coordinator.max_coordinators = max;
        println!("Set coordinator.max_coordinators = {}", max);
        changed = true;
    }

    if let Some(i) = coordinator_interval {
        config.coordinator.interval = i;
        println!("Set coordinator.interval = {}", i);
        changed = true;
    }

    if let Some(i) = poll_interval {
        config.coordinator.poll_interval = i;
        println!("Set coordinator.poll_interval = {}", i);
        changed = true;
    }

    if let Some(exec) = coordinator_executor {
        config.coordinator.executor = Some(exec.to_string());
        println!("Set coordinator.executor = \"{}\"", exec);
        changed = true;
    }

    if let Some(m) = coordinator_model {
        // Validate provider:model format
        if let Err(e) = workgraph::config::parse_model_spec_strict(m) {
            anyhow::bail!(
                "Invalid model format: {}. Use provider:model format (e.g., 'claude:opus').",
                e
            );
        }
        config.coordinator.model = Some(m.to_string());
        config.coordinator.provider = None; // Clear deprecated field
        println!("Set coordinator.model = \"{}\"", m);
        changed = true;
    }

    if let Some(p) = coordinator_provider {
        let suggested_provider = if p == "anthropic" { "claude" } else { p };
        let current_model_raw = config
            .coordinator
            .model
            .as_deref()
            .unwrap_or(&config.agent.model);
        // Extract just the model ID (strip any existing provider prefix)
        let current_model_id = workgraph::config::parse_model_spec(current_model_raw).model_id;
        eprintln!(
            "Warning: --coordinator-provider is deprecated. Use provider:model format in --coordinator-model instead.\n\
             Example: wg config --coordinator-model {}:{}",
            suggested_provider, current_model_id,
        );
        config.coordinator.provider = Some(p.to_string());
        println!("Set coordinator.provider = \"{}\"", p);
        changed = true;
    }

    // Agency settings
    if let Some(v) = auto_evaluate {
        config.agency.auto_evaluate = v;
        println!("Set agency.auto_evaluate = {}", v);
        changed = true;
    }

    if let Some(v) = auto_assign {
        config.agency.auto_assign = v;
        println!("Set agency.auto_assign = {}", v);
        changed = true;
    }

    if let Some(v) = assigner_agent {
        config.agency.assigner_agent = Some(v.to_string());
        println!("Set agency.assigner_agent = \"{}\"", v);
        changed = true;
    }

    if let Some(v) = evaluator_agent {
        config.agency.evaluator_agent = Some(v.to_string());
        println!("Set agency.evaluator_agent = \"{}\"", v);
        changed = true;
    }

    if let Some(v) = evolver_agent {
        config.agency.evolver_agent = Some(v.to_string());
        println!("Set agency.evolver_agent = \"{}\"", v);
        changed = true;
    }

    if let Some(v) = creator_agent {
        config.agency.creator_agent = Some(v.to_string());
        println!("Set agency.creator_agent = \"{}\"", v);
        changed = true;
    }

    if let Some(v) = retention_heuristics {
        config.agency.retention_heuristics = Some(v.to_string());
        println!("Set agency.retention_heuristics = \"{}\"", v);
        changed = true;
    }

    if let Some(v) = auto_triage {
        config.agency.auto_triage = v;
        println!("Set agency.auto_triage = {}", v);
        changed = true;
    }

    if let Some(v) = auto_place {
        config.agency.auto_place = v;
        println!("Set agency.auto_place = {}", v);
        changed = true;
    }

    if let Some(v) = auto_create {
        config.agency.auto_create = v;
        println!("Set agency.auto_create = {}", v);
        changed = true;
    }

    if let Some(t) = triage_timeout {
        config.agency.triage_timeout = Some(t);
        println!("Set agency.triage_timeout = {}", t);
        changed = true;
    }

    if let Some(b) = triage_max_log_bytes {
        config.agency.triage_max_log_bytes = Some(b);
        println!("Set agency.triage_max_log_bytes = {}", b);
        changed = true;
    }

    // Guardrails settings
    if let Some(v) = max_child_tasks {
        config.guardrails.max_child_tasks_per_agent = v;
        println!("Set guardrails.max_child_tasks_per_agent = {}", v);
        changed = true;
    }

    if let Some(v) = max_task_depth {
        config.guardrails.max_task_depth = v;
        println!("Set guardrails.max_task_depth = {}", v);
        changed = true;
    }

    // Eval gate settings
    if let Some(threshold) = eval_gate_threshold {
        if !(0.0..=1.0).contains(&threshold) {
            anyhow::bail!(
                "eval_gate_threshold must be in [0.0, 1.0] range, got {}",
                threshold
            );
        }
        config.agency.eval_gate_threshold = Some(threshold);
        println!("Set agency.eval_gate_threshold = {}", threshold);
        changed = true;
    }

    if let Some(v) = eval_gate_all {
        config.agency.eval_gate_all = v;
        println!("Set agency.eval_gate_all = {}", v);
        changed = true;
    }

    // FLIP settings
    if let Some(v) = flip_enabled {
        config.agency.flip_enabled = v;
        println!("Set agency.flip_enabled = {}", v);
        changed = true;
    }

    if let Some(v) = flip_verification_threshold {
        config.agency.flip_verification_threshold = Some(v);
        println!("Set agency.flip_verification_threshold = {}", v);
        changed = true;
    }

    // TUI chat history settings
    if let Some(v) = chat_history {
        config.tui.chat_history = v;
        println!("Set tui.chat_history = {}", v);
        changed = true;
    }

    if let Some(v) = chat_history_max {
        config.tui.chat_history_max = v;
        println!("Set tui.chat_history_max = {}", v);
        changed = true;
    }

    if let Some(counters) = tui_counters {
        let valid = ["uptime", "cumulative", "active", "session"];
        for part in counters.split(',') {
            let p = part.trim();
            if !p.is_empty() && !valid.contains(&p) {
                anyhow::bail!(
                    "Invalid counter '{}'. Valid: uptime, cumulative, active, session",
                    p
                );
            }
        }
        config.tui.counters = counters.to_string();
        println!("Set tui.counters = \"{}\"", counters);
        changed = true;
    }

    // Checkpoint settings
    if let Some(tokens) = retry_context_tokens {
        config.checkpoint.retry_context_tokens = tokens;
        println!("Set checkpoint.retry_context_tokens = {}", tokens);
        changed = true;
    }

    // Viz settings
    if let Some(color) = viz_edge_color {
        match color {
            "gray" | "white" | "mixed" => {
                config.viz.edge_color = color.to_string();
                println!("Set viz.edge_color = \"{}\"", color);
                changed = true;
            }
            _ => {
                anyhow::bail!(
                    "Invalid edge color '{}'. Valid options: gray, white, mixed",
                    color
                );
            }
        }
    }

    if changed {
        // Snapshot local config.toml before overwriting — only after all
        // validation has passed, so a failed `wg config` run doesn't leave
        // stray backup files behind.
        if matches!(scope, ConfigScope::Local)
            && let Some(backup) = Config::backup_on_disk(dir)?
        {
            println!("Backed up previous config → {}", backup.display());
        }
        match scope {
            ConfigScope::Global => {
                config.save_global()?;
                let path = Config::global_config_path()?;
                println!("Global configuration saved to {}", path.display());
            }
            ConfigScope::Local => {
                config.save(dir)?;
                println!("Configuration saved.");
            }
        }

        // Auto-reload: when the user changed model/endpoint on the local
        // config and a daemon is running, signal it to re-read config.toml
        // so the change takes effect without a `wg service reload`. Only
        // fires for local scope (global config isn't read by a daemon).
        let wants_reload = !no_reload
            && matches!(scope, ConfigScope::Local)
            && (endpoint.is_some() || model.is_some());
        if wants_reload {
            match try_reload_daemon(dir) {
                Ok(true) => println!("Daemon reconfigured (picked up new config)."),
                Ok(false) => {} // no daemon running; nothing to do
                Err(e) => {
                    println!(
                        "Note: config saved but daemon reload failed ({}). Run `wg service reload` to retry.",
                        e
                    );
                }
            }
        }
    } else {
        println!("No changes specified. Use --show to view current config.");
    }

    Ok(())
}

/// List merged configuration with source annotations
pub fn list(dir: &Path, json: bool) -> Result<()> {
    let (config, sources) = Config::load_with_sources(dir)?;

    if json {
        let merged_val = toml::Value::try_from(&config)?;
        let mut entries = Vec::new();
        collect_leaf_entries(&merged_val, "", &sources, &mut entries);
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        let merged_val = toml::Value::try_from(&config)?;
        let mut entries = Vec::new();
        collect_leaf_entries(&merged_val, "", &sources, &mut entries);

        println!("Workgraph Configuration (merged)");
        println!("=================================");
        println!();
        for entry in &entries {
            let source = entry["source"].as_str().unwrap_or("default");
            let key = entry["key"].as_str().unwrap_or("");
            let value = &entry["value"];
            println!(
                "  {:40} = {:20} [{}]",
                key,
                format_toml_value(value),
                source
            );
        }
    }

    Ok(())
}

/// Format a serde_json::Value for display
fn format_toml_value(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => format!("\"{}\"", s),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Recursively collect leaf entries from a TOML value for list output
fn collect_leaf_entries(
    val: &toml::Value,
    prefix: &str,
    sources: &std::collections::BTreeMap<String, ConfigSource>,
    entries: &mut Vec<serde_json::Value>,
) {
    if let toml::Value::Table(table) = val {
        for (key, v) in table {
            let full_key = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{}.{}", prefix, key)
            };
            match v {
                toml::Value::Table(_) => {
                    collect_leaf_entries(v, &full_key, sources, entries);
                }
                _ => {
                    let source = sources
                        .get(&full_key)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "default".to_string());
                    let json_val = toml_value_to_json(v);
                    entries.push(serde_json::json!({
                        "key": full_key,
                        "value": json_val,
                        "source": source,
                    }));
                }
            }
        }
    }
}

/// Convert a toml::Value to serde_json::Value for serialization
fn toml_value_to_json(val: &toml::Value) -> serde_json::Value {
    match val {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(t) => {
            let mut map = serde_json::Map::new();
            for (k, v) in t {
                map.insert(k.clone(), toml_value_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
    }
}

/// Show Matrix configuration
pub fn show_matrix(json: bool) -> Result<()> {
    let config = MatrixConfig::load()?;
    let config_path = MatrixConfig::config_path()?;

    if json {
        // Mask password in JSON output
        let output = serde_json::json!({
            "config_path": config_path.display().to_string(),
            "homeserver_url": config.homeserver_url,
            "username": config.username,
            "password": config.password.as_ref().map(|_| "********"),
            "access_token": config.access_token.as_ref().map(|t| mask_token(t)),
            "default_room": config.default_room,
            "has_credentials": config.has_credentials(),
            "is_complete": config.is_complete(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Matrix Configuration");
        println!("====================");
        println!();
        println!("Config file: {}", config_path.display());
        if !config_path.exists() {
            println!("  (file does not exist yet)");
        }
        println!();

        if let Some(ref url) = config.homeserver_url {
            println!("  homeserver_url = \"{}\"", url);
        } else {
            println!("  homeserver_url = (not set)");
        }

        if let Some(ref user) = config.username {
            println!("  username = \"{}\"", user);
        } else {
            println!("  username = (not set)");
        }

        if config.password.is_some() {
            println!("  password = ********");
        } else {
            println!("  password = (not set)");
        }

        if let Some(ref token) = config.access_token {
            println!("  access_token = {}", mask_token(token));
        } else {
            println!("  access_token = (not set)");
        }

        if let Some(ref room) = config.default_room {
            println!("  default_room = \"{}\"", room);
        } else {
            println!("  default_room = (not set)");
        }

        println!();
        if config.is_complete() {
            println!("Status: Ready (credentials and room configured)");
        } else if config.has_credentials() {
            println!("Status: Credentials set, but no default room");
        } else {
            println!("Status: Not configured");
            println!();
            println!("To configure, use:");
            println!("  wg config --homeserver https://matrix.org \\");
            println!("            --username @user:matrix.org \\");
            println!("            --access-token <token> \\");
            println!("            --room '!roomid:matrix.org'");
        }
    }

    Ok(())
}

/// Update Matrix configuration
pub fn update_matrix(
    homeserver: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
    access_token: Option<&str>,
    room: Option<&str>,
) -> Result<()> {
    let mut config = MatrixConfig::load()?;
    let mut changed = false;

    if let Some(url) = homeserver {
        config.homeserver_url = Some(url.to_string());
        println!("Set homeserver_url = \"{}\"", url);
        changed = true;
    }

    if let Some(user) = username {
        config.username = Some(user.to_string());
        println!("Set username = \"{}\"", user);
        changed = true;
    }

    if let Some(pass) = password {
        config.password = Some(pass.to_string());
        println!("Set password = ********");
        changed = true;
    }

    if let Some(token) = access_token {
        config.access_token = Some(token.to_string());
        println!("Set access_token = {}", mask_token(token));
        changed = true;
    }

    if let Some(r) = room {
        config.default_room = Some(r.to_string());
        println!("Set default_room = \"{}\"", r);
        changed = true;
    }

    if changed {
        config.save()?;
        let config_path = MatrixConfig::config_path()?;
        println!();
        println!("Matrix configuration saved to {}", config_path.display());

        if config.is_complete() {
            println!("Status: Ready");
        } else if config.has_credentials() {
            println!("Status: Credentials set, but no default room configured");
        } else {
            println!("Status: Partially configured (missing credentials)");
        }
    } else {
        println!("No changes specified. Use --matrix to view Matrix config.");
    }

    Ok(())
}

/// Show model routing configuration: resolved model+provider for each dispatch role.
pub fn show_model_routing(dir: &Path, json: bool) -> Result<()> {
    use workgraph::config::DispatchRole;

    let config = Config::load_merged(dir)?;

    if json {
        let mut entries = serde_json::Map::new();
        // Show default
        let resolved = config.resolve_model_for_role(DispatchRole::Default);
        let source = config.resolve_model_source(DispatchRole::Default);
        entries.insert(
            "default".to_string(),
            serde_json::json!({
                "model": resolved.model,
                "provider": resolved.provider,
                "endpoint": resolved.endpoint,
                "tier": DispatchRole::Default.default_tier().to_string(),
                "source": source,
            }),
        );
        for role in DispatchRole::ALL {
            let resolved = config.resolve_model_for_role(*role);
            let source = config.resolve_model_source(*role);
            entries.insert(
                role.to_string(),
                serde_json::json!({
                    "model": resolved.model,
                    "provider": resolved.provider,
                    "endpoint": resolved.endpoint,
                    "tier": role.default_tier().to_string(),
                    "source": source,
                }),
            );
        }
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        println!("Model Routing Configuration");
        println!("===========================");
        println!();
        println!(
            "  {:<20} {:<10} {:<30} {:<14} {:<16} SOURCE",
            "ROLE", "TIER", "MODEL", "PROVIDER", "ENDPOINT"
        );
        println!("  {}", "-".repeat(106));

        // Default
        let resolved = config.resolve_model_for_role(DispatchRole::Default);
        let source = config.resolve_model_source(DispatchRole::Default);
        let provider_display = resolved
            .provider
            .as_deref()
            .map(workgraph::config::native_provider_to_prefix)
            .unwrap_or("(not set)");
        println!(
            "  {:<20} {:<10} {:<30} {:<14} {:<16} {}",
            "default",
            DispatchRole::Default.default_tier(),
            resolved.model,
            provider_display,
            resolved.endpoint.as_deref().unwrap_or(""),
            source,
        );

        // Per-role
        for role in DispatchRole::ALL {
            let resolved = config.resolve_model_for_role(*role);
            let source = config.resolve_model_source(*role);
            let provider_display = resolved
                .provider
                .as_deref()
                .map(workgraph::config::native_provider_to_prefix)
                .unwrap_or("(not set)");
            println!(
                "  {:<20} {:<10} {:<30} {:<14} {:<16} {}",
                role.to_string(),
                role.default_tier(),
                resolved.model,
                provider_display,
                resolved.endpoint.as_deref().unwrap_or(""),
                source,
            );
        }
        println!();
        println!("Sources: explicit = user-set model, tier-default = from default_tier(),");
        println!("         tier-override = from [models.role].tier, legacy = from agency.*_model,");
        println!("         fallback = from [models.default] or agent.model");
        println!();
        println!("Use --set-model <role> <model> to override a role.");
        println!("Use --set-provider <role> <provider> to set a provider.");
        println!("Use --set-endpoint <role> <endpoint-name> to bind an endpoint.");
    }

    Ok(())
}

/// Update FLIP model configuration (--flip-model / --flip-inference-model / --flip-comparison-model).
pub fn update_flip_models(
    dir: &Path,
    scope: ConfigScope,
    inference_model: Option<&str>,
    comparison_model: Option<&str>,
) -> Result<()> {
    use workgraph::config::DispatchRole;

    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(dir)?,
    };

    let mut changed = false;

    if let Some(model) = inference_model {
        if let Err(e) = workgraph::config::parse_model_spec_strict(model) {
            anyhow::bail!(
                "Invalid model format for --flip-inference-model: {}. Use provider:model format (e.g., 'claude:opus').",
                e
            );
        }
        config.models.set_model(DispatchRole::FlipInference, model);
        println!("Set models.flip_inference.model = \"{}\"", model);
        let spec = workgraph::config::parse_model_spec(model);
        if let Some(ref provider) = spec.provider {
            config
                .models
                .set_provider(DispatchRole::FlipInference, provider);
        }
        changed = true;
    }

    if let Some(model) = comparison_model {
        if let Err(e) = workgraph::config::parse_model_spec_strict(model) {
            anyhow::bail!(
                "Invalid model format for --flip-comparison-model: {}. Use provider:model format (e.g., 'claude:haiku').",
                e
            );
        }
        config.models.set_model(DispatchRole::FlipComparison, model);
        println!("Set models.flip_comparison.model = \"{}\"", model);
        let spec = workgraph::config::parse_model_spec(model);
        if let Some(ref provider) = spec.provider {
            config
                .models
                .set_provider(DispatchRole::FlipComparison, provider);
        }
        changed = true;
    }

    if changed {
        match scope {
            ConfigScope::Global => config.save_global()?,
            ConfigScope::Local => config.save(dir)?,
        }
    }

    Ok(())
}

/// Update model routing configuration (--set-model / --set-provider / --set-endpoint).
pub fn update_model_routing(
    dir: &Path,
    scope: ConfigScope,
    set_model: Option<&[String]>,
    set_provider: Option<&[String]>,
    set_endpoint: Option<&[String]>,
) -> Result<()> {
    use workgraph::config::DispatchRole;

    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(dir)?,
    };

    let mut changed = false;

    if let Some(args) = set_model {
        if args.len() != 2 {
            anyhow::bail!("--set-model requires exactly 2 arguments: <role> <model>");
        }
        let role: DispatchRole = args[0].parse()?;
        let model = &args[1];

        // Validate provider:model format
        if let Err(e) = workgraph::config::parse_model_spec_strict(model) {
            anyhow::bail!(
                "Invalid model format: {}. Use provider:model format (e.g., 'claude:opus').",
                e
            );
        }

        config.models.set_model(role, model);
        println!("Set models.{}.model = \"{}\"", role, model);

        // Auto-populate provider from provider:model spec
        let spec = workgraph::config::parse_model_spec(model);
        if let Some(ref provider) = spec.provider {
            config.models.set_provider(role, provider);
            println!(
                "Set models.{}.provider = \"{}\" (from provider:model)",
                role, provider
            );
        }

        // Validate: warn if model ID is not in the registry
        let spec = workgraph::config::parse_model_spec(model);
        let lookup_id = &spec.model_id;
        if config.registry_lookup(lookup_id).is_none() {
            eprintln!(
                "Warning: model '{}' is not in the registry. It will be used as a raw model ID.",
                lookup_id
            );
            eprintln!(
                "  If this is a short alias, add it with: wg config --registry-add --id {} ...",
                lookup_id
            );
        } else {
            // Informational: check tier compatibility
            if let Some(entry) = config.registry_lookup(lookup_id) {
                let role_tier = role.default_tier();
                if entry.tier != role_tier {
                    eprintln!(
                        "Note: model '{}' is tier '{}' but role '{}' defaults to tier '{}'.",
                        lookup_id, entry.tier, role, role_tier
                    );
                }
            }
        }
        changed = true;
    }

    if let Some(args) = set_provider {
        if args.len() != 2 {
            anyhow::bail!("--set-provider requires exactly 2 arguments: <role> <provider>");
        }
        let role_name = &args[0];
        let provider = &args[1];
        let suggested_provider = if provider == "anthropic" {
            "claude"
        } else {
            provider
        };
        eprintln!(
            "Warning: --set-provider is deprecated. Use provider:model format in --set-model instead.\n\
             Example: wg config --set-model {} {}:MODEL",
            role_name, suggested_provider,
        );
        let role: DispatchRole = role_name.parse()?;
        config.models.set_provider(role, provider);
        println!("Set models.{}.provider = \"{}\"", role, provider);
        changed = true;
    }

    if let Some(args) = set_endpoint {
        if args.len() != 2 {
            anyhow::bail!("--set-endpoint requires exactly 2 arguments: <role> <endpoint-name>");
        }
        let role: DispatchRole = args[0].parse()?;
        let endpoint_name = &args[1];

        // Validate: warn if endpoint name is not configured
        if config.llm_endpoints.find_by_name(endpoint_name).is_none() {
            eprintln!(
                "Warning: endpoint '{}' is not configured. Add it with: wg endpoints add {}",
                endpoint_name, endpoint_name
            );
        }

        config.models.set_endpoint(role, endpoint_name);
        println!("Set models.{}.endpoint = \"{}\"", role, endpoint_name);
        changed = true;
    }

    if changed {
        match scope {
            ConfigScope::Global => {
                config.save_global()?;
                let path = Config::global_config_path()?;
                println!("Global configuration saved to {}", path.display());
            }
            ConfigScope::Local => {
                config.save(dir)?;
                println!("Configuration saved.");
            }
        }
    }

    Ok(())
}

/// Show all model registry entries (built-in + user-defined).
pub fn show_registry(dir: &Path, json: bool) -> Result<()> {
    let config = Config::load_merged(dir)?;
    let entries = config.effective_registry();

    if json {
        let val: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "provider": e.provider,
                    "model": e.model,
                    "tier": e.tier.to_string(),
                    "context_window": e.context_window,
                    "cost_per_input_mtok": e.cost_per_input_mtok,
                    "cost_per_output_mtok": e.cost_per_output_mtok,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("No model registry entries.");
        return Ok(());
    }

    println!(
        "  {:<12} {:<12} {:<30} {:<10} COST (in/out per MTok)",
        "ID", "PROVIDER", "MODEL", "TIER"
    );
    println!("  {}", "-".repeat(85));

    for entry in &entries {
        let cost = if entry.cost_per_input_mtok > 0.0 || entry.cost_per_output_mtok > 0.0 {
            format!(
                "${:.2}/${:.2}",
                entry.cost_per_input_mtok, entry.cost_per_output_mtok
            )
        } else {
            "-".to_string()
        };
        println!(
            "  {:<12} {:<12} {:<30} {:<10} {}",
            entry.id, entry.provider, entry.model, entry.tier, cost,
        );
    }

    Ok(())
}

/// Add a new model entry to the registry.
#[allow(clippy::too_many_arguments)]
pub fn add_registry_entry(
    dir: &Path,
    scope: ConfigScope,
    id: &str,
    provider: &str,
    model: &str,
    tier: &str,
    endpoint: Option<&str>,
    context_window: Option<u64>,
    cost_input: Option<f64>,
    cost_output: Option<f64>,
) -> Result<()> {
    let tier: Tier = tier.parse()?;

    let entry = ModelRegistryEntry {
        id: id.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        tier,
        endpoint: endpoint.map(|s| s.to_string()),
        context_window: context_window.unwrap_or(0),
        cost_per_input_mtok: cost_input.unwrap_or(0.0),
        cost_per_output_mtok: cost_output.unwrap_or(0.0),
        ..Default::default()
    };

    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(dir)?,
    };

    // Check for duplicate ID and update if exists
    let existing_idx = config.model_registry.iter().position(|e| e.id == id);
    if let Some(idx) = existing_idx {
        config.model_registry[idx] = entry;
        println!("Updated registry entry: {}", id);
    } else {
        config.model_registry.push(entry);
        println!("Added registry entry: {}", id);
    }

    save_config(&config, dir, scope)?;

    println!("  {} / {} / {} (tier: {})", id, provider, model, tier);

    Ok(())
}

/// Remove a registry entry by ID. Warns about dependents unless --force is set.
pub fn remove_registry_entry(
    dir: &Path,
    scope: ConfigScope,
    id: &str,
    force: bool,
    json: bool,
) -> Result<()> {
    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(dir)?,
    };

    // Check if entry exists in user config
    let idx = config.model_registry.iter().position(|e| e.id == id);

    if idx.is_none() {
        // Check if it's a built-in
        let merged = Config::load_merged(dir)?;
        if merged.effective_registry().iter().any(|e| e.id == id) {
            anyhow::bail!(
                "'{}' is a built-in registry entry and cannot be removed.\n\
                 To override it, add a custom entry with the same ID using --registry-add.",
                id
            );
        }
        anyhow::bail!("Registry entry '{}' not found.", id);
    }

    // Check for dependents: tier defaults and role overrides
    let mut warnings = Vec::new();

    // Check tier defaults
    let tiers = &config.tiers;
    if tiers.fast.as_deref() == Some(id) {
        warnings.push(format!("tiers.fast = '{}'", id));
    }
    if tiers.standard.as_deref() == Some(id) {
        warnings.push(format!("tiers.standard = '{}'", id));
    }
    if tiers.premium.as_deref() == Some(id) {
        warnings.push(format!("tiers.premium = '{}'", id));
    }

    // Check role overrides (including default, which is excluded from ALL)
    use workgraph::config::DispatchRole;
    if let Some(ref default_cfg) = config.models.default
        && default_cfg.model.as_deref() == Some(id)
    {
        warnings.push(format!("[models.default].model = '{}'", id));
    }
    for role in DispatchRole::ALL {
        if let Some(role_cfg) = config.models.get_role(*role)
            && role_cfg.model.as_deref() == Some(id)
        {
            warnings.push(format!("[models.{}].model = '{}'", role, id));
        }
    }

    if !warnings.is_empty() && !force {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "error": "entry has dependents",
                    "id": id,
                    "dependents": warnings,
                })
            );
        } else {
            eprintln!("Cannot remove '{}': referenced by:", id);
            for w in &warnings {
                eprintln!("  - {}", w);
            }
            eprintln!();
            eprintln!("Use --force to remove anyway, or reassign the dependents first.");
        }
        std::process::exit(1);
    }

    config.model_registry.remove(idx.unwrap());

    save_config(&config, dir, scope)?;

    if !warnings.is_empty() {
        println!(
            "Removed registry entry '{}' (with {} dangling reference(s))",
            id,
            warnings.len()
        );
    } else {
        println!("Removed registry entry '{}'", id);
    }

    Ok(())
}

/// Show current tier→model assignments.
pub fn show_tiers(dir: &Path, json: bool) -> Result<()> {
    let config = Config::load_merged(dir)?;
    let tiers = config.effective_tiers_public();
    let registry = config.effective_registry();

    let resolve = |model_id: Option<&str>| -> String {
        match model_id {
            Some(id) => registry
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.model.clone())
                .unwrap_or_else(|| format!("{} (not in registry)", id)),
            None => "(unset)".to_string(),
        }
    };

    if json {
        let val = serde_json::json!({
            "fast": {
                "model_id": tiers.fast,
                "resolved_model": resolve(tiers.fast.as_deref()),
            },
            "standard": {
                "model_id": tiers.standard,
                "resolved_model": resolve(tiers.standard.as_deref()),
            },
            "premium": {
                "model_id": tiers.premium,
                "resolved_model": resolve(tiers.premium.as_deref()),
            },
        });
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    println!("  {:<12} {:<12} RESOLVED MODEL", "TIER", "MODEL ID");
    println!("  {}", "-".repeat(60));

    println!(
        "  {:<12} {:<12} {}",
        "fast",
        tiers.fast.as_deref().unwrap_or("(unset)"),
        resolve(tiers.fast.as_deref()),
    );
    println!(
        "  {:<12} {:<12} {}",
        "standard",
        tiers.standard.as_deref().unwrap_or("(unset)"),
        resolve(tiers.standard.as_deref()),
    );
    println!(
        "  {:<12} {:<12} {}",
        "premium",
        tiers.premium.as_deref().unwrap_or("(unset)"),
        resolve(tiers.premium.as_deref()),
    );

    Ok(())
}

/// Set which model a tier uses. Format: <tier>=<model-id>
pub fn set_tier(dir: &Path, scope: ConfigScope, tier_spec: &str) -> Result<()> {
    let parts: Vec<&str> = tier_spec.splitn(2, '=').collect();
    if parts.len() != 2 {
        anyhow::bail!(
            "--tier requires format <tier>=<model-id>, got \"{}\"",
            tier_spec
        );
    }

    let tier_name = parts[0].trim();
    let model_id = parts[1].trim();

    // Validate tier name
    let _tier: Tier = tier_name.parse()?;

    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(dir)?,
    };

    // Warn if model_id is not in registry
    let merged = Config::load_merged(dir)?;
    if merged.registry_lookup(model_id).is_none() {
        eprintln!(
            "Warning: '{}' is not in the model registry. \
             Tier will resolve to it as a bare model name.",
            model_id
        );
    }

    match tier_name {
        "fast" => config.tiers.fast = Some(model_id.to_string()),
        "standard" => config.tiers.standard = Some(model_id.to_string()),
        "premium" => config.tiers.premium = Some(model_id.to_string()),
        _ => unreachable!(), // already validated by Tier::from_str
    }

    save_config(&config, dir, scope)?;

    println!("Set tiers.{} = \"{}\"", tier_name, model_id);

    Ok(())
}

/// Helper: save config to the appropriate location based on scope.
fn save_config(config: &Config, dir: &Path, scope: ConfigScope) -> Result<()> {
    match scope {
        ConfigScope::Global => config.save_global()?,
        ConfigScope::Local => config.save(dir)?,
    }
    Ok(())
}

/// Check OpenRouter API key validity and credit status
pub fn check_key(dir: &Path, json: bool) -> Result<()> {
    use workgraph::executor::native::openai_client::resolve_openai_api_key_from_dir;

    let key = match resolve_openai_api_key_from_dir(dir) {
        Ok(k) => k,
        Err(_) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"error": "No API key found. Run `wg endpoints add`, set OPENROUTER_API_KEY, or add [native_executor] api_key to config."})
                );
            } else {
                eprintln!("Error: No API key found.");
                eprintln!("Configure a key via:");
                eprintln!("  - wg endpoints add (recommended)");
                eprintln!("  - Set OPENROUTER_API_KEY or OPENAI_API_KEY environment variable");
                eprintln!("  - Add [native_executor] api_key to .workgraph/config.toml");
            }
            std::process::exit(1);
        }
    };

    let client = reqwest::blocking::Client::new();
    let resp = client
        .get("https://openrouter.ai/api/v1/key")
        .header("Authorization", format!("Bearer {}", key))
        .send();

    match resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json()?;
            let data = body.get("data").unwrap_or(&body);

            if json {
                println!("{}", serde_json::to_string_pretty(data)?);
            } else {
                println!("OpenRouter API Key Status");
                println!("========================");
                println!();
                println!("  Key: {}", mask_token(&key));

                if let Some(limit) = data.get("limit") {
                    if limit.is_null() {
                        println!("  Credit limit: unlimited");
                    } else {
                        println!("  Credit limit: ${}", limit);
                    }
                }

                if let Some(remaining) = data.get("limit_remaining") {
                    if remaining.is_null() {
                        println!("  Remaining: unlimited");
                    } else {
                        println!("  Remaining: ${}", remaining);
                    }
                }

                if let Some(usage) = data.get("usage") {
                    println!("  Usage (all-time): ${}", usage);
                }

                if let Some(is_free) = data.get("is_free_tier") {
                    println!(
                        "  Tier: {}",
                        if is_free.as_bool().unwrap_or(false) {
                            "free"
                        } else {
                            "paid"
                        }
                    );
                }

                if let Some(daily) = data.get("usage_daily") {
                    println!("  Usage (today): ${}", daily);
                }

                println!();
                println!("Status: Valid");
            }
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().unwrap_or_default();
            if json {
                println!(
                    "{}",
                    serde_json::json!({"error": format!("HTTP {}", status), "body": body})
                );
            } else {
                eprintln!("Error: API key check failed (HTTP {})", status);
                if !body.is_empty() {
                    eprintln!("  {}", body);
                }
            }
            std::process::exit(1);
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"error": format!("Request failed: {}", e)})
                );
            } else {
                eprintln!("Error: Could not reach OpenRouter API: {}", e);
            }
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Mask a token for display (show first and last 4 chars)
fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 12 {
        "********".to_string()
    } else {
        let prefix: String = chars[..4].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{}...{}", prefix, suffix)
    }
}

/// Install the current project's config as the global default.
///
/// Copies `.workgraph/config.toml` → `~/.workgraph/config.toml`.
/// If the global config already exists and `--force` is not set, shows a diff
/// summary and asks for confirmation on stdin.
pub fn install_global(workgraph_dir: &Path, force: bool) -> Result<()> {
    let global_path = Config::global_config_path()?;
    let global_dir = Config::global_dir()?;
    install_global_to(workgraph_dir, &global_path, &global_dir, force)
}

/// Core logic for install-global, parameterized for testing.
pub fn install_global_to(
    workgraph_dir: &Path,
    global_path: &Path,
    global_dir: &Path,
    force: bool,
) -> Result<()> {
    let local_path = workgraph_dir.join("config.toml");
    if !local_path.exists() {
        anyhow::bail!(
            "No project config found at {}.\nRun `wg config --init` to create one first.",
            local_path.display()
        );
    }

    let local_content = std::fs::read_to_string(&local_path)?;

    if global_path.exists() && !force {
        let global_content = std::fs::read_to_string(global_path)?;
        if local_content == global_content {
            println!("Global config is already identical to project config — nothing to do.");
            return Ok(());
        }
        println!("Global config already exists at {}", global_path.display());
        println!();
        print_diff_summary(&global_content, &local_content);
        println!();
        eprint!("Overwrite global config? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Ensure parent directory exists
    std::fs::create_dir_all(global_dir)?;

    std::fs::copy(&local_path, global_path)?;
    println!("Installed project config as global default");
    println!("  {} → {}", local_path.display(), global_path.display());
    Ok(())
}

/// Print a brief summary of differences between two TOML config strings.
fn print_diff_summary(old: &str, new: &str) {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut added = 0usize;
    let mut removed = 0usize;
    let mut changed_keys: Vec<String> = Vec::new();

    // Simple line-by-line diff: collect changed lines
    let max_len = old_lines.len().max(new_lines.len());
    for i in 0..max_len {
        let ol = old_lines.get(i).copied().unwrap_or("");
        let nl = new_lines.get(i).copied().unwrap_or("");
        if ol != nl {
            if ol.is_empty() {
                added += 1;
            } else if nl.is_empty() {
                removed += 1;
            } else {
                // Try to extract key name from TOML line
                if let Some(key) = nl.split('=').next() {
                    let k = key.trim().to_string();
                    if !k.is_empty() && !k.starts_with('[') && !changed_keys.contains(&k) {
                        changed_keys.push(k);
                    }
                }
            }
        }
    }

    println!("Diff summary:");
    if !changed_keys.is_empty() {
        let display: Vec<&str> = changed_keys.iter().take(10).map(|s| s.as_str()).collect();
        println!("  Changed keys: {}", display.join(", "));
        if changed_keys.len() > 10 {
            println!("  ... and {} more", changed_keys.len() - 10);
        }
    }
    if added > 0 {
        println!("  +{} new lines", added);
    }
    if removed > 0 {
        println!("  -{} removed lines", removed);
    }
    if changed_keys.is_empty() && added == 0 && removed == 0 {
        println!("  (content differs but no key-level changes detected)");
    }
}

/// Set an API key file reference for a provider's endpoint.
///
/// If an endpoint for the provider already exists, updates its `api_key_file`.
/// Otherwise, creates a new endpoint entry with the file reference.
pub fn set_key(
    workgraph_dir: &Path,
    scope: ConfigScope,
    provider: &str,
    file_path: &str,
) -> Result<()> {
    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(workgraph_dir)?,
    };

    // Find existing endpoint for provider, or create new one
    let mut found = false;
    for ep in &mut config.llm_endpoints.endpoints {
        if ep.provider == provider {
            ep.api_key_file = Some(file_path.to_string());
            ep.api_key = None; // Clear inline key when switching to file
            found = true;
            break;
        }
    }

    if !found {
        let is_first = config.llm_endpoints.endpoints.is_empty();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: provider.to_string(),
            provider: provider.to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(file_path.to_string()),
            api_key_env: None,
            is_default: is_first,
            context_window: None,
        });
    }

    match scope {
        ConfigScope::Global => config.save_global()?,
        ConfigScope::Local => config.save(workgraph_dir)?,
    }

    println!("Set API key file for '{}': {}", provider, file_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_init_and_show() {
        let temp_dir = TempDir::new().unwrap();

        // Init should create config
        let result = init(temp_dir.path(), None);
        assert!(result.is_ok());

        // Show should work (local scope)
        let result = show(temp_dir.path(), Some(ConfigScope::Local), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_update() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path(), None).unwrap();

        let result = update(
            temp_dir.path(),
            ConfigScope::Local,
            Some("opencode"),
            Some("openai:gpt-4"),
            Some(30),
            None, // max_agents
            None, // max_coordinators
            None,
            None,
            None,
            None, // coordinator_model
            None, // coordinator_provider
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // auto_triage
            None, // auto_place
            None, // auto_create
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // endpoint
            false, // no_reload
        );
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.agent.executor, "opencode");
        assert_eq!(config.agent.model, "openai:gpt-4");
        assert_eq!(config.agent.interval, 30);
    }

    #[test]
    fn test_update_coordinator() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path(), None).unwrap();

        let result = update(
            temp_dir.path(),
            ConfigScope::Local,
            None,
            None,
            None,
            Some(8), // max_agents
            None,    // max_coordinators
            Some(60),
            None,
            Some("shell"),
            None, // coordinator_model
            None, // coordinator_provider
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // auto_triage
            None, // auto_place
            None, // auto_create
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // endpoint
            false, // no_reload
        );
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.coordinator.max_agents, 8);
        assert_eq!(config.coordinator.interval, 60);
        assert_eq!(config.coordinator.executor, Some("shell".to_string()));
    }

    #[test]
    fn test_update_poll_interval() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path(), None).unwrap();

        let result = update(
            temp_dir.path(),
            ConfigScope::Local,
            None,
            None,
            None,
            None, // max_agents
            None, // max_coordinators
            None,
            Some(120),
            None,
            None, // coordinator_model
            None, // coordinator_provider
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // auto_triage
            None, // auto_place
            None, // auto_create
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // endpoint
            false, // no_reload
        );
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.coordinator.poll_interval, 120);
    }

    #[test]
    fn test_update_agency() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path(), None).unwrap();

        let result = update(
            temp_dir.path(),
            ConfigScope::Local,
            None,
            None,
            None,
            None, // max_agents
            None, // max_coordinators
            None,
            None,
            None,
            None, // coordinator_model
            None, // coordinator_provider
            Some(true),
            Some(true),
            Some("assigner-hash"),
            Some("evaluator-hash"),
            Some("evolver-hash"),
            Some("creator-hash"),
            Some("Retire below 0.3 after 10 evals"),
            None, // auto_triage
            None, // auto_place
            None, // auto_create
            None, // triage_timeout
            None, // triage_max_log_bytes
            None, // max_child_tasks
            None, // max_task_depth
            None, // viz_edge_color
            None, // eval_gate_threshold
            None, // eval_gate_all
            None, // flip_enabled
            None, // flip_verification_threshold
            None, // chat_history
            None, // chat_history_max
            None, // tui_counters
            None, // retry_context_tokens
            None, // endpoint
            false, // no_reload
        );
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert!(config.agency.auto_evaluate);
        assert!(config.agency.auto_assign);
        assert_eq!(
            config.agency.assigner_agent,
            Some("assigner-hash".to_string())
        );
        assert_eq!(
            config.agency.evaluator_agent,
            Some("evaluator-hash".to_string())
        );
        assert_eq!(
            config.agency.evolver_agent,
            Some("evolver-hash".to_string())
        );
        assert_eq!(
            config.agency.creator_agent,
            Some("creator-hash".to_string())
        );
        assert_eq!(
            config.agency.retention_heuristics,
            Some("Retire below 0.3 after 10 evals".to_string())
        );
    }

    #[test]
    fn test_mask_token_short() {
        assert_eq!(mask_token("abc"), "********");
        assert_eq!(mask_token("123456789012"), "********");
    }

    #[test]
    fn test_mask_token_long() {
        assert_eq!(mask_token("abcdefghijklm"), "abcd...jklm");
    }

    #[test]
    fn test_mask_token_unicode_no_panic() {
        // Multi-byte chars should not panic
        assert_eq!(
            mask_token("🎯🎯🎯🎯🎯🎯🎯🎯🎯🎯🎯🎯🎯"),
            "🎯🎯🎯🎯...🎯🎯🎯🎯"
        );
    }

    #[test]
    fn test_show_merged() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path(), None).unwrap();

        // Show with no scope = merged
        let result = show(temp_dir.path(), None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path(), None).unwrap();

        // List should work and show source annotations
        let result = list(temp_dir.path(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_json() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path(), None).unwrap();

        let result = list(temp_dir.path(), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_config_install_global() {
        // Set up a project dir with a config
        let project_dir = TempDir::new().unwrap();
        init(project_dir.path(), None).unwrap();

        // Set up a separate "global" dir
        let global_dir = TempDir::new().unwrap();
        let global_path = global_dir.path().join("config.toml");

        // Install with --force (no global exists yet)
        let result = install_global_to(project_dir.path(), &global_path, global_dir.path(), true);
        assert!(result.is_ok());
        assert!(global_path.exists(), "Global config should be created");

        // Verify contents match
        let local_content =
            std::fs::read_to_string(project_dir.path().join("config.toml")).unwrap();
        let global_content = std::fs::read_to_string(&global_path).unwrap();
        assert_eq!(local_content, global_content);

        // Install again with --force should overwrite
        let result = install_global_to(project_dir.path(), &global_path, global_dir.path(), true);
        assert!(result.is_ok());

        // Without project config should fail
        let empty_dir = TempDir::new().unwrap();
        let result = install_global_to(empty_dir.path(), &global_path, global_dir.path(), true);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No project config"),
            "Should mention missing project config"
        );
    }

    #[test]
    fn test_config_install_global_creates_parent_dir() {
        let project_dir = TempDir::new().unwrap();
        init(project_dir.path(), None).unwrap();

        // Point to a nested global path that doesn't exist yet
        let global_base = TempDir::new().unwrap();
        let global_dir = global_base.path().join("nested").join(".workgraph");
        let global_path = global_dir.join("config.toml");

        let result = install_global_to(project_dir.path(), &global_path, &global_dir, true);
        assert!(result.is_ok());
        assert!(global_path.exists());
    }

    #[test]
    fn test_diff_summary() {
        // Just verify it doesn't panic
        print_diff_summary(
            "key1 = \"old\"\nkey2 = \"same\"\n",
            "key1 = \"new\"\nkey2 = \"same\"\nkey3 = \"added\"\n",
        );
    }
}
