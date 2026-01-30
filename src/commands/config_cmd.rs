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
        println!("  executor = \"{}\"", config.coordinator.executor);
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
pub fn update(
    dir: &Path,
    executor: Option<&str>,
    model: Option<&str>,
    interval: Option<u64>,
    max_agents: Option<usize>,
    coordinator_interval: Option<u64>,
    coordinator_executor: Option<&str>,
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

    if let Some(exec) = coordinator_executor {
        config.coordinator.executor = exec.to_string();
        println!("Set coordinator.executor = \"{}\"", exec);
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

        let result = update(temp_dir.path(), Some("opencode"), Some("gpt-4"), Some(30), None, None, None);
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

        let result = update(temp_dir.path(), None, None, None, Some(8), Some(60), Some("shell"));
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.coordinator.max_agents, 8);
        assert_eq!(config.coordinator.interval, 60);
        assert_eq!(config.coordinator.executor, "shell");
    }
}
