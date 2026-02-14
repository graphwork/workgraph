//! Configuration management commands

use anyhow::Result;
use std::path::Path;
use workgraph::config::{Config, MatrixConfig};

/// Show current configuration
pub fn show(dir: &Path, json: bool) -> Result<()> {
    let config = Config::load(dir)?;

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
        println!();
        if config.project.name.is_some() || config.project.description.is_some() {
            println!("[project]");
            if let Some(ref name) = config.project.name {
                println!("  name = \"{}\"", name);
            }
            if let Some(ref desc) = config.project.description {
                println!("  description = \"{}\"", desc);
            }
        }
    }

    Ok(())
}

/// Initialize default config file
pub fn init(dir: &Path) -> Result<()> {
    if Config::init(dir)? {
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
    retention_heuristics: Option<&str>,
    auto_triage: Option<bool>,
    triage_model: Option<&str>,
    triage_timeout: Option<u64>,
    triage_max_log_bytes: Option<usize>,
) -> Result<()> {
    let mut config = Config::load(dir)?;
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
        changed = true;
    }

    if let Some(m) = evaluator_model {
        config.agency.evaluator_model = Some(m.to_string());
        println!("Set agency.evaluator_model = \"{}\"", m);
        changed = true;
    }

    if let Some(m) = evolver_model {
        config.agency.evolver_model = Some(m.to_string());
        println!("Set agency.evolver_model = \"{}\"", m);
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

    if changed {
        config.save(dir)?;
        println!("Configuration saved.");
    } else {
        println!("No changes specified. Use --show to view current config.");
    }

    Ok(())
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

/// Mask a token for display (show first and last 4 chars)
fn mask_token(token: &str) -> String {
    if token.len() <= 12 {
        "********".to_string()
    } else {
        format!("{}...{}", &token[..4], &token[token.len() - 4..])
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
        let result = init(temp_dir.path());
        assert!(result.is_ok());

        // Show should work
        let result = show(temp_dir.path(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_update() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path()).unwrap();

        let result = update(
            temp_dir.path(),
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
        init(temp_dir.path()).unwrap();

        let result = update(
            temp_dir.path(),
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
        init(temp_dir.path()).unwrap();

        let result = update(
            temp_dir.path(),
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
        );
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.coordinator.poll_interval, 120);
    }

    #[test]
    fn test_update_agency() {
        let temp_dir = TempDir::new().unwrap();
        init(temp_dir.path()).unwrap();

        let result = update(
            temp_dir.path(),
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
            Some("Retire below 0.3 after 10 evals"),
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
            config.agency.retention_heuristics,
            Some("Retire below 0.3 after 10 evals".to_string())
        );
    }
}
