//! Project configuration for workgraph
//!
//! Configuration is stored in `.workgraph/config.toml` and controls
//! agent behavior, executor settings, and project defaults.
//!
//! Sensitive credentials (like Matrix login) are stored separately in
//! `~/.config/workgraph/matrix.toml` to avoid accidentally committing secrets.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Agent configuration
    #[serde(default)]
    pub agent: AgentConfig,

    /// Coordinator configuration
    #[serde(default)]
    pub coordinator: CoordinatorConfig,

    /// Project metadata
    #[serde(default)]
    pub project: ProjectConfig,
}

/// Agent-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Executor system: "claude", "opencode", "codex", "shell"
    #[serde(default = "default_executor")]
    pub executor: String,

    /// Model to use (e.g., "opus-4-5", "sonnet", "haiku")
    #[serde(default = "default_model")]
    pub model: String,

    /// Default sleep interval between agent iterations (seconds)
    #[serde(default = "default_interval")]
    pub interval: u64,

    /// Command template for AI-based execution
    /// Placeholders: {model}, {prompt}, {task_id}, {workdir}
    #[serde(default = "default_command_template")]
    pub command_template: String,

    /// Maximum tasks per agent run (None = unlimited)
    #[serde(default)]
    pub max_tasks: Option<u32>,

    /// Heartbeat timeout in minutes (for detecting dead agents)
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,
}

/// Coordinator-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    /// Maximum number of parallel agents
    #[serde(default = "default_max_agents")]
    pub max_agents: usize,

    /// Poll interval in seconds
    #[serde(default = "default_coordinator_interval")]
    pub interval: u64,

    /// Executor to use for spawned agents
    #[serde(default = "default_executor")]
    pub executor: String,
}

fn default_max_agents() -> usize {
    4
}

fn default_coordinator_interval() -> u64 {
    30
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            max_agents: default_max_agents(),
            interval: default_coordinator_interval(),
            executor: default_executor(),
        }
    }
}

/// Project metadata
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    /// Project name
    #[serde(default)]
    pub name: Option<String>,

    /// Project description
    #[serde(default)]
    pub description: Option<String>,

    /// Default skills for new actors
    #[serde(default)]
    pub default_skills: Vec<String>,
}

fn default_executor() -> String {
    "claude".to_string()
}

fn default_model() -> String {
    "opus-4-5".to_string()
}

fn default_interval() -> u64 {
    10
}

fn default_heartbeat_timeout() -> u64 {
    5
}

fn default_command_template() -> String {
    "claude --model {model} --print \"{prompt}\"".to_string()
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            executor: default_executor(),
            model: default_model(),
            interval: default_interval(),
            command_template: default_command_template(),
            max_tasks: None,
            heartbeat_timeout: default_heartbeat_timeout(),
        }
    }
}

/// Matrix configuration for notifications and collaboration
/// Stored in ~/.config/workgraph/matrix.toml (user's global config, not in repo)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MatrixConfig {
    /// Matrix homeserver URL (e.g., "https://matrix.org")
    #[serde(default)]
    pub homeserver_url: Option<String>,

    /// Matrix username (e.g., "@user:matrix.org")
    #[serde(default)]
    pub username: Option<String>,

    /// Matrix password (prefer access_token for better security)
    #[serde(default)]
    pub password: Option<String>,

    /// Matrix access token (preferred over password)
    #[serde(default)]
    pub access_token: Option<String>,

    /// Default room to send notifications to (e.g., "!roomid:matrix.org")
    #[serde(default)]
    pub default_room: Option<String>,
}

impl MatrixConfig {
    /// Get the path to the global Matrix config file
    pub fn config_path() -> anyhow::Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?;
        Ok(config_dir.join("workgraph").join("matrix.toml"))
    }

    /// Load Matrix configuration from ~/.config/workgraph/matrix.toml
    /// Returns default (empty) config if file doesn't exist
    pub fn load() -> anyhow::Result<Self> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&config_path)
            .map_err(|e| anyhow::anyhow!("Failed to read Matrix config: {}", e))?;

        let config: MatrixConfig = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse Matrix config: {}", e))?;

        Ok(config)
    }

    /// Save Matrix configuration to ~/.config/workgraph/matrix.toml
    pub fn save(&self) -> anyhow::Result<()> {
        let config_path = Self::config_path()?;

        // Create parent directory if needed
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Failed to create config directory: {}", e))?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize Matrix config: {}", e))?;

        fs::write(&config_path, content)
            .map_err(|e| anyhow::anyhow!("Failed to write Matrix config: {}", e))?;

        Ok(())
    }

    /// Check if the configuration has valid credentials
    pub fn has_credentials(&self) -> bool {
        self.homeserver_url.is_some()
            && self.username.is_some()
            && (self.password.is_some() || self.access_token.is_some())
    }

    /// Check if the configuration is complete (has credentials and default room)
    pub fn is_complete(&self) -> bool {
        self.has_credentials() && self.default_room.is_some()
    }
}

impl Config {
    /// Load configuration from .workgraph/config.toml
    /// Returns default config if file doesn't exist
    pub fn load(workgraph_dir: &Path) -> anyhow::Result<Self> {
        let config_path = workgraph_dir.join("config.toml");

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&config_path)
            .map_err(|e| anyhow::anyhow!("Failed to read config: {}", e))?;

        let config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

        Ok(config)
    }

    /// Save configuration to .workgraph/config.toml
    pub fn save(&self, workgraph_dir: &Path) -> anyhow::Result<()> {
        let config_path = workgraph_dir.join("config.toml");

        let content = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;

        fs::write(&config_path, content)
            .map_err(|e| anyhow::anyhow!("Failed to write config: {}", e))?;

        Ok(())
    }

    /// Initialize default config file if it doesn't exist
    pub fn init(workgraph_dir: &Path) -> anyhow::Result<bool> {
        let config_path = workgraph_dir.join("config.toml");

        if config_path.exists() {
            return Ok(false); // Already exists
        }

        let config = Self::default();
        config.save(workgraph_dir)?;
        Ok(true) // Created new
    }

    /// Build the executor command from template
    pub fn build_command(&self, prompt: &str, task_id: &str, workdir: &str) -> String {
        self.agent
            .command_template
            .replace("{model}", &self.agent.model)
            .replace("{prompt}", prompt)
            .replace("{task_id}", task_id)
            .replace("{workdir}", workdir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.agent.executor, "claude");
        assert_eq!(config.agent.model, "opus-4-5");
        assert_eq!(config.agent.interval, 10);
    }

    #[test]
    fn test_load_missing_config() {
        let temp_dir = TempDir::new().unwrap();
        let config = Config::load(temp_dir.path()).unwrap();
        assert_eq!(config.agent.executor, "claude");
    }

    #[test]
    fn test_save_and_load() {
        let temp_dir = TempDir::new().unwrap();

        let mut config = Config::default();
        config.agent.model = "haiku".to_string();
        config.agent.interval = 30;
        config.save(temp_dir.path()).unwrap();

        let loaded = Config::load(temp_dir.path()).unwrap();
        assert_eq!(loaded.agent.model, "haiku");
        assert_eq!(loaded.agent.interval, 30);
    }

    #[test]
    fn test_init_config() {
        let temp_dir = TempDir::new().unwrap();

        // First init should create file
        let created = Config::init(temp_dir.path()).unwrap();
        assert!(created);

        // Second init should not overwrite
        let created = Config::init(temp_dir.path()).unwrap();
        assert!(!created);
    }

    #[test]
    fn test_build_command() {
        let config = Config::default();
        let cmd = config.build_command("do something", "task-1", "/home/user/project");
        assert!(cmd.contains("opus-4-5"));
        assert!(cmd.contains("do something"));
    }

    #[test]
    fn test_parse_custom_config() {
        let toml_str = r#"
[agent]
executor = "opencode"
model = "gpt-4"
interval = 60
command_template = "opencode run --model {model} '{prompt}'"

[project]
name = "My Project"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.executor, "opencode");
        assert_eq!(config.agent.model, "gpt-4");
        assert_eq!(config.project.name, Some("My Project".to_string()));
    }

    #[test]
    fn test_matrix_config_default() {
        let config = MatrixConfig::default();
        assert!(config.homeserver_url.is_none());
        assert!(config.username.is_none());
        assert!(config.password.is_none());
        assert!(config.access_token.is_none());
        assert!(config.default_room.is_none());
        assert!(!config.has_credentials());
        assert!(!config.is_complete());
    }

    #[test]
    fn test_matrix_config_has_credentials() {
        let mut config = MatrixConfig::default();
        assert!(!config.has_credentials());

        config.homeserver_url = Some("https://matrix.org".to_string());
        assert!(!config.has_credentials());

        config.username = Some("@user:matrix.org".to_string());
        assert!(!config.has_credentials());

        config.password = Some("secret".to_string());
        assert!(config.has_credentials());
        assert!(!config.is_complete());

        config.default_room = Some("!room:matrix.org".to_string());
        assert!(config.is_complete());
    }

    #[test]
    fn test_matrix_config_access_token() {
        let mut config = MatrixConfig::default();
        config.homeserver_url = Some("https://matrix.org".to_string());
        config.username = Some("@user:matrix.org".to_string());
        config.access_token = Some("syt_abc123".to_string());
        assert!(config.has_credentials());
    }

    #[test]
    fn test_parse_matrix_config() {
        let toml_str = r#"
homeserver_url = "https://matrix.example.com"
username = "@bot:example.com"
access_token = "syt_token_here"
default_room = "!notifications:example.com"
"#;
        let config: MatrixConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.homeserver_url,
            Some("https://matrix.example.com".to_string())
        );
        assert_eq!(config.username, Some("@bot:example.com".to_string()));
        assert_eq!(config.access_token, Some("syt_token_here".to_string()));
        assert_eq!(
            config.default_room,
            Some("!notifications:example.com".to_string())
        );
        assert!(config.is_complete());
    }
}
