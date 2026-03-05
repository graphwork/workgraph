//! Configuration management commands

use anyhow::Result;
use std::path::Path;
use workgraph::config::{Config, ConfigSource, MatrixConfig};

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
        println!("  command_template = \"{}\"", config.agent.command_template);
        println!();
        println!("[coordinator]");
        println!("  max_agents = {}", config.coordinator.max_agents);
        println!("  interval = {}", config.coordinator.interval);
        println!("  poll_interval = {}", config.coordinator.poll_interval);
        println!("  executor = \"{}\"", config.coordinator.executor);
        println!();
        println!("[agency]");
        println!("  auto_evaluate = {}", config.agency.auto_evaluate);
        println!("  auto_assign = {}", config.agency.auto_assign);
        if let Some(ref agent) = config.agency.assigner_agent {
            println!("  assigner_agent = \"{}\"", agent);
        }
        if let Some(ref agent) = config.agency.evaluator_agent {
            println!("  evaluator_agent = \"{}\"", agent);
        }
        if let Some(ref model) = config.agency.assigner_model {
            println!("  assigner_model = \"{}\"", model);
        }
        if let Some(ref model) = config.agency.evaluator_model {
            println!("  evaluator_model = \"{}\"", model);
        }
        if let Some(ref model) = config.agency.evolver_model {
            println!("  evolver_model = \"{}\"", model);
        }
        if let Some(ref agent) = config.agency.evolver_agent {
            println!("  evolver_agent = \"{}\"", agent);
        }
        if let Some(ref agent) = config.agency.creator_agent {
            println!("  creator_agent = \"{}\"", agent);
        }
        if let Some(ref model) = config.agency.creator_model {
            println!("  creator_model = \"{}\"", model);
        }
        if let Some(ref heuristics) = config.agency.retention_heuristics {
            println!("  retention_heuristics = \"{}\"", heuristics);
        }
        println!("  auto_triage = {}", config.agency.auto_triage);
        if let Some(ref model) = config.agency.triage_model {
            println!("  triage_model = \"{}\"", model);
        }
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
        if let Some(ref model) = config.agency.flip_inference_model {
            println!("  flip_inference_model = \"{}\"", model);
        }
        if let Some(ref model) = config.agency.flip_comparison_model {
            println!("  flip_comparison_model = \"{}\"", model);
        }
        if let Some(threshold) = config.agency.flip_verification_threshold {
            println!("  flip_verification_threshold = {}", threshold);
        }
        println!("  flip_verification_model = \"{}\"", config.agency.flip_verification_model);
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
                    }
                }
                println!();
            }
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
    coordinator_interval: Option<u64>,
    poll_interval: Option<u64>,
    coordinator_executor: Option<&str>,
    auto_evaluate: Option<bool>,
    auto_assign: Option<bool>,
    assigner_model: Option<&str>,
    evaluator_model: Option<&str>,
    evolver_model: Option<&str>,
    assigner_agent: Option<&str>,
    evaluator_agent: Option<&str>,
    evolver_agent: Option<&str>,
    creator_agent: Option<&str>,
    creator_model: Option<&str>,
    retention_heuristics: Option<&str>,
    auto_triage: Option<bool>,
    triage_model: Option<&str>,
    triage_timeout: Option<u64>,
    triage_max_log_bytes: Option<usize>,
    max_child_tasks: Option<u32>,
    max_task_depth: Option<u32>,
    viz_edge_color: Option<&str>,
    eval_gate_threshold: Option<f64>,
    eval_gate_all: Option<bool>,
    flip_enabled: Option<bool>,
    flip_inference_model: Option<&str>,
    flip_comparison_model: Option<&str>,
    flip_verification_threshold: Option<f64>,
    flip_verification_model: Option<&str>,
    chat_history: Option<bool>,
    chat_history_max: Option<usize>,
) -> Result<()> {
    let mut config = match scope {
        ConfigScope::Global => Config::load_global()?.unwrap_or_default(),
        ConfigScope::Local => Config::load(dir)?,
    };
    let mut changed = false;

    // Agent settings
    if let Some(exec) = executor {
        config.agent.executor = exec.to_string();
        println!("Set agent.executor = \"{}\"", exec);
        changed = true;
    }

    if let Some(m) = model {
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
        config.coordinator.executor = exec.to_string();
        println!("Set coordinator.executor = \"{}\"", exec);
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

    if let Some(m) = assigner_model {
        config.agency.assigner_model = Some(m.to_string());
        println!("Set agency.assigner_model = \"{}\"", m);
        eprintln!("Warning: --assigner-model is deprecated. Use --role-model assigner={} or --set-model assigner {} instead.", m, m);
        changed = true;
    }

    if let Some(m) = evaluator_model {
        config.agency.evaluator_model = Some(m.to_string());
        println!("Set agency.evaluator_model = \"{}\"", m);
        eprintln!("Warning: --evaluator-model is deprecated. Use --role-model evaluator={} or --set-model evaluator {} instead.", m, m);
        changed = true;
    }

    if let Some(m) = evolver_model {
        config.agency.evolver_model = Some(m.to_string());
        println!("Set agency.evolver_model = \"{}\"", m);
        eprintln!("Warning: --evolver-model is deprecated. Use --role-model evolver={} or --set-model evolver {} instead.", m, m);
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

    if let Some(v) = creator_model {
        config.agency.creator_model = Some(v.to_string());
        println!("Set agency.creator_model = \"{}\"", v);
        eprintln!("Warning: --creator-model is deprecated. Use --role-model creator={} or --set-model creator {} instead.", v, v);
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

    if let Some(m) = triage_model {
        config.agency.triage_model = Some(m.to_string());
        println!("Set agency.triage_model = \"{}\"", m);
        eprintln!("Warning: --triage-model is deprecated. Use --role-model triage={} or --set-model triage {} instead.", m, m);
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

    if let Some(m) = flip_inference_model {
        config.agency.flip_inference_model = Some(m.to_string());
        println!("Set agency.flip_inference_model = \"{}\"", m);
        eprintln!("Warning: --flip-inference-model is deprecated. Use --role-model flip_inference={} or --set-model flip_inference {} instead.", m, m);
        changed = true;
    }

    if let Some(m) = flip_comparison_model {
        config.agency.flip_comparison_model = Some(m.to_string());
        println!("Set agency.flip_comparison_model = \"{}\"", m);
        eprintln!("Warning: --flip-comparison-model is deprecated. Use --role-model flip_comparison={} or --set-model flip_comparison {} instead.", m, m);
        changed = true;
    }

    if let Some(v) = flip_verification_threshold {
        config.agency.flip_verification_threshold = Some(v);
        println!("Set agency.flip_verification_threshold = {}", v);
        changed = true;
    }

    if let Some(m) = flip_verification_model {
        config.agency.flip_verification_model = m.to_string();
        println!("Set agency.flip_verification_model = \"{}\"", m);
        eprintln!("Warning: --flip-verification-model is deprecated. Use --role-model verification={} or --set-model verification {} instead.", m, m);
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
        entries.insert(
            "default".to_string(),
            serde_json::json!({
                "model": resolved.model,
                "provider": resolved.provider,
            }),
        );
        for role in DispatchRole::ALL {
            let resolved = config.resolve_model_for_role(*role);
            entries.insert(
                role.to_string(),
                serde_json::json!({
                    "model": resolved.model,
                    "provider": resolved.provider,
                }),
            );
        }
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        println!("Model Routing Configuration");
        println!("===========================");
        println!();
        println!(
            "  {:<20} {:<20} {}",
            "ROLE", "MODEL", "PROVIDER"
        );
        println!("  {}", "-".repeat(55));

        // Default
        let resolved = config.resolve_model_for_role(DispatchRole::Default);
        println!(
            "  {:<20} {:<20} {}",
            "default",
            resolved.model,
            resolved.provider.as_deref().unwrap_or("(not set)")
        );

        // Per-role
        for role in DispatchRole::ALL {
            let resolved = config.resolve_model_for_role(*role);
            let role_cfg = config.models.get_role(*role);
            let has_explicit = role_cfg.and_then(|c| c.model.as_ref()).is_some();
            let marker = if has_explicit { "" } else { " (inherited)" };
            println!(
                "  {:<20} {:<20} {}{}",
                role.to_string(),
                resolved.model,
                resolved.provider.as_deref().unwrap_or("(not set)"),
                marker,
            );
        }
        println!();
        println!("Use --set-model <role> <model> to override a role.");
        println!("Use --set-provider <role> <provider> to set a provider.");
    }

    Ok(())
}

/// Update model routing configuration (--set-model / --set-provider).
pub fn update_model_routing(
    dir: &Path,
    scope: ConfigScope,
    set_model: Option<&[String]>,
    set_provider: Option<&[String]>,
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
        config.models.set_model(role, model);
        println!("Set models.{}.model = \"{}\"", role, model);
        changed = true;
    }

    if let Some(args) = set_provider {
        if args.len() != 2 {
            anyhow::bail!("--set-provider requires exactly 2 arguments: <role> <provider>");
        }
        let role: DispatchRole = args[0].parse()?;
        let provider = &args[1];
        config.models.set_provider(role, provider);
        println!("Set models.{}.provider = \"{}\"", role, provider);
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
            Some("gpt-4"),
            Some(30),
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
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.agent.executor, "opencode");
        assert_eq!(config.agent.model, "gpt-4");
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
            Some(8),
            Some(60),
            None,
            Some("shell"),
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
            None,
        );
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.coordinator.max_agents, 8);
        assert_eq!(config.coordinator.interval, 60);
        assert_eq!(config.coordinator.executor, "shell");
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
            None,
            None,
            Some(120),
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
            None,
            None,
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
            None,
            None,
            None,
            None,
            Some(true),
            Some(true),
            Some("sonnet"),
            Some("haiku"),
            Some("opus-4-5"),
            Some("assigner-hash"),
            Some("evaluator-hash"),
            Some("evolver-hash"),
            Some("creator-hash"),
            Some("haiku"),
            Some("Retire below 0.3 after 10 evals"),
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
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert!(config.agency.auto_evaluate);
        assert!(config.agency.auto_assign);
        assert_eq!(config.agency.assigner_model, Some("sonnet".to_string()));
        assert_eq!(config.agency.evaluator_model, Some("haiku".to_string()));
        assert_eq!(config.agency.evolver_model, Some("opus-4-5".to_string()));
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
        assert_eq!(config.agency.creator_model, Some("haiku".to_string()));
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
}
