//! Configuration management commands

use anyhow::Result;
use std::path::Path;
use workgraph::config::Config;

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
) -> Result<()> {
    let mut config = Config::load(dir)?;
    let mut changed = false;

    if let Some(exec) = executor {
        config.agent.executor = exec.to_string();
        println!("Set executor = \"{}\"", exec);
        changed = true;
    }

    if let Some(m) = model {
        config.agent.model = m.to_string();
        println!("Set model = \"{}\"", m);
        changed = true;
    }

    if let Some(i) = interval {
        config.agent.interval = i;
        println!("Set interval = {}", i);
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

        let result = update(temp_dir.path(), Some("opencode"), Some("gpt-4"), Some(30));
        assert!(result.is_ok());

        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.agent.executor, "opencode");
        assert_eq!(config.agent.model, "gpt-4");
        assert_eq!(config.agent.interval, 30);
    }
}
