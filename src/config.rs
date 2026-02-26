//! Project configuration for workgraph
//!
//! Configuration is stored in `.workgraph/config.toml` and controls
//! agent behavior, executor settings, and project defaults.
//!
//! Sensitive credentials (like Matrix login) are stored separately in
//! `~/.config/workgraph/matrix.toml` to avoid accidentally committing secrets.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

    /// Help display configuration
    #[serde(default)]
    pub help: HelpConfig,

    /// Agency (evolutionary identity) configuration
    #[serde(default)]
    pub agency: AgencyConfig,

    /// Log configuration
    #[serde(default)]
    pub log: LogConfig,

    /// Replay configuration
    #[serde(default)]
    pub replay: ReplayConfig,
}

/// Help display configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelpConfig {
    /// Command ordering: "usage" (default), "alphabetical", or "curated"
    #[serde(default = "default_help_ordering")]
    pub ordering: String,
}

fn default_help_ordering() -> String {
    "usage".to_string()
}

impl Default for HelpConfig {
    fn default() -> Self {
        Self {
            ordering: default_help_ordering(),
        }
    }
}

/// Log configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    /// Rotation threshold in bytes (default: 10 MB)
    #[serde(default = "default_rotation_threshold")]
    pub rotation_threshold: u64,
}

fn default_rotation_threshold() -> u64 {
    10 * 1024 * 1024 // 10 MB
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            rotation_threshold: default_rotation_threshold(),
        }
    }
}

/// Replay configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayConfig {
    /// Default threshold for --keep-done: preserve Done tasks scoring above this (0.0-1.0)
    #[serde(default = "default_keep_done_threshold")]
    pub keep_done_threshold: f64,

    /// Whether to snapshot agent output logs alongside graph.jsonl
    #[serde(default)]
    pub snapshot_agent_output: bool,
}

fn default_keep_done_threshold() -> f64 {
    0.9
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            keep_done_threshold: default_keep_done_threshold(),
            snapshot_agent_output: false,
        }
    }
}

/// Dimension weights for the organisational reward signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgRewardWeights {
    /// Weight on downstream usability dimension (default: 0.50)
    #[serde(default = "default_w_downstream_usability")]
    pub downstream_usability: f64,
    /// Weight on coordination overhead dimension (default: 0.30)
    #[serde(default = "default_w_coordination_overhead")]
    pub coordination_overhead: f64,
    /// Weight on blocking behaviour dimension (default: 0.20)
    #[serde(default = "default_w_blocking_behaviour")]
    pub blocking_behaviour: f64,
}

fn default_w_downstream_usability() -> f64 { 0.50 }
fn default_w_coordination_overhead() -> f64 { 0.30 }
fn default_w_blocking_behaviour() -> f64 { 0.20 }

impl Default for OrgRewardWeights {
    fn default() -> Self {
        Self {
            downstream_usability: default_w_downstream_usability(),
            coordination_overhead: default_w_coordination_overhead(),
            blocking_behaviour: default_w_blocking_behaviour(),
        }
    }
}

/// Configuration for the two-level organisational reward signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgRewardConfig {
    /// How much weight to give org-level score vs task-level score in evolution (0.0–1.0).
    /// 0.0 = ignore org level entirely, 1.0 = weight equally with task level.
    #[serde(default = "default_org_weight")]
    pub org_weight: f64,

    /// Default observation window in seconds (0 = wait for all direct dependents to complete).
    #[serde(default)]
    pub observation_window_secs: u64,

    /// Downstream hop horizon for downstream_usability calculation (default: 3).
    #[serde(default = "default_hop_horizon")]
    pub downstream_hop_horizon: u32,

    /// Dimension weights for the composite org score.
    #[serde(default)]
    pub weights: OrgRewardWeights,
}

fn default_run_mode() -> f64 { 0.2 }
fn default_min_exploration_rate() -> f64 { 0.05 }
fn default_exploration_interval() -> u32 { 20 }
fn default_cache_population_threshold() -> f64 { 0.8 }
fn default_ucb_exploration_constant() -> f64 { std::f64::consts::SQRT_2 }
fn default_novelty_bonus_multiplier() -> f64 { 1.5 }
fn default_bizarre_ideation_interval() -> u32 { 10 }
fn default_performance_threshold() -> f64 { 0.7 }

fn default_org_weight() -> f64 { 0.4 }
fn default_hop_horizon() -> u32 { 3 }

impl Default for OrgRewardConfig {
    fn default() -> Self {
        Self {
            org_weight: default_org_weight(),
            observation_window_secs: 0,
            downstream_hop_horizon: default_hop_horizon(),
            weights: OrgRewardWeights::default(),
        }
    }
}

/// Agency (evolutionary identity system) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgencyConfig {
    /// Automatically trigger evaluation when a task completes
    #[serde(default)]
    pub auto_evaluate: bool,

    /// Automatically assign an identity when spawning agents
    #[serde(default)]
    pub auto_assign: bool,

    /// Content-hash of agent to use as assigner (None = use default pipeline)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigner_agent: Option<String>,

    /// Content-hash of agent to use as evaluator (None = use evaluator_model fallback)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_agent: Option<String>,

    /// Model to use for assigner agents (None = use default agent model).
    /// Fallback when assigner_agent is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigner_model: Option<String>,

    /// Model to use for evaluator agents (None = use default agent model).
    /// Fallback when evaluator_agent is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_model: Option<String>,

    /// Model to use for evolver agents (None = use default agent model).
    /// Fallback when evolver_agent is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evolver_model: Option<String>,

    /// Content-hash of agent to use as evolver
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evolver_agent: Option<String>,

    /// Content-hash of agent to use as agent creator (None = not configured)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_agent: Option<String>,

    /// Model to use for agent creator (None = use default agent model).
    /// Fallback when creator_agent is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_model: Option<String>,

    /// Prose policy for the evolver describing retention heuristics
    /// (e.g. when to retire underperforming roles/motivations)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_heuristics: Option<String>,

    /// Automatically triage dead agents to assess work progress before respawning
    #[serde(default)]
    pub auto_triage: bool,

    /// Model to use for triage (default: "haiku")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_model: Option<String>,

    /// Timeout in seconds for triage calls (default: 30)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_timeout: Option<u64>,

    /// Maximum bytes to read from agent output log for triage (default: 50000)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_max_log_bytes: Option<usize>,

    /// Two-level organisational reward signal configuration.
    #[serde(default)]
    pub org_reward: OrgRewardConfig,

    /// Run mode on the performance/learning continuum.
    /// 0.0 = pure performance, 1.0 = pure learning.
    /// Default: 0.2
    #[serde(default = "default_run_mode")]
    pub run_mode: f64,

    /// Minimum fraction of assignments routed through learning path
    /// even when run_mode is low. Guards against exploitation drift
    /// (March, 1991). Default: 0.05
    #[serde(default = "default_min_exploration_rate")]
    pub min_exploration_rate: f64,

    /// Force a learning assignment every N tasks in performance mode.
    /// 0 = disabled. Default: 20
    #[serde(default = "default_exploration_interval")]
    pub exploration_interval: u32,

    /// Cache score threshold for populating composition cache from
    /// learning experiments. Default: 0.8
    #[serde(default = "default_cache_population_threshold")]
    pub cache_population_threshold: f64,

    /// UCB exploration constant C for primitive selection in learning mode.
    /// Higher values favour uncertainty; lower values favour known performance.
    /// Default: sqrt(2) ≈ 1.414
    #[serde(default = "default_ucb_exploration_constant")]
    pub ucb_exploration_constant: f64,

    /// Multiplier applied to UCB score for low-attractor-weight primitives.
    /// Counteracts attractor-area drift. Default: 1.5
    #[serde(default = "default_novelty_bonus_multiplier")]
    pub novelty_bonus_multiplier: f64,

    /// Force a bizarre ideation composition every N learning assignments.
    /// 0 = disabled. Default: 10
    #[serde(default = "default_bizarre_ideation_interval")]
    pub bizarre_ideation_interval: u32,

    /// Performance threshold for cache-hit deployment in performance mode.
    /// Default: 0.7
    #[serde(default = "default_performance_threshold")]
    pub performance_threshold: f64,
}

impl Default for AgencyConfig {
    fn default() -> Self {
        Self {
            auto_evaluate: false,
            auto_assign: false,
            assigner_agent: None,
            evaluator_agent: None,
            assigner_model: None,
            evaluator_model: None,
            evolver_model: None,
            evolver_agent: None,
            creator_agent: None,
            creator_model: None,
            retention_heuristics: None,
            auto_triage: false,
            triage_model: None,
            triage_timeout: None,
            triage_max_log_bytes: None,
            org_reward: OrgRewardConfig::default(),
            run_mode: default_run_mode(),
            min_exploration_rate: default_min_exploration_rate(),
            exploration_interval: default_exploration_interval(),
            cache_population_threshold: default_cache_population_threshold(),
            ucb_exploration_constant: default_ucb_exploration_constant(),
            novelty_bonus_multiplier: default_novelty_bonus_multiplier(),
            bizarre_ideation_interval: default_bizarre_ideation_interval(),
            performance_threshold: default_performance_threshold(),
        }
    }
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

    /// Poll interval in seconds (used by standalone coordinator command)
    #[serde(default = "default_coordinator_interval")]
    pub interval: u64,

    /// Background poll interval in seconds for the service daemon safety net.
    /// The daemon runs a coordinator tick on this slow interval even without
    /// receiving any GraphChanged IPC events. Catches manual edits, lost events,
    /// or external tools modifying the graph. Default: 60s.
    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,

    /// Executor to use for spawned agents
    #[serde(default = "default_executor")]
    pub executor: String,

    /// Model to use for spawned agents (e.g., "opus-4-5", "sonnet", "haiku")
    /// Overrides agent.model when set. Can be further overridden by CLI --model.
    #[serde(default)]
    pub model: Option<String>,

    /// Default context scope for spawned agents (clean, task, graph, full).
    /// Overridden by role.default_context_scope and task.context_scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context_scope: Option<String>,

    /// Hard timeout for spawned agents (e.g., "30m", "1h", "90s").
    /// Wraps the agent invocation with the `timeout` command.
    /// Default: "30m". Set to empty string to disable.
    #[serde(default = "default_agent_timeout")]
    pub agent_timeout: String,

    /// Settling delay in milliseconds after a GraphChanged event before the
    /// coordinator tick fires. During burst graph construction (rapid task
    /// additions), this prevents premature dispatch by waiting for the burst
    /// to settle. Default: 2000ms (2 seconds).
    #[serde(default = "default_settling_delay_ms")]
    pub settling_delay_ms: u64,
}

fn default_max_agents() -> usize {
    4
}

fn default_coordinator_interval() -> u64 {
    30
}

fn default_settling_delay_ms() -> u64 {
    2000
}

fn default_poll_interval() -> u64 {
    60
}

fn default_agent_timeout() -> String {
    "30m".to_string()
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            max_agents: default_max_agents(),
            interval: default_coordinator_interval(),
            poll_interval: default_poll_interval(),
            executor: default_executor(),
            model: None,
            default_context_scope: None,
            agent_timeout: default_agent_timeout(),
            settling_delay_ms: default_settling_delay_ms(),
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
    "opus".to_string()
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
            .ok_or_else(|| anyhow::anyhow!("Could not determine config directory. Expected ~/.config on Linux, ~/Library/Application Support on macOS, or %APPDATA% on Windows."))?;
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

/// Indicates where a configuration value came from
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSource {
    Global,
    Local,
    Default,
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::Global => write!(f, "global"),
            ConfigSource::Local => write!(f, "local"),
            ConfigSource::Default => write!(f, "default"),
        }
    }
}

/// Deep-merge two TOML values. For (Table, Table) pairs, recursively merge
/// with `local` keys overriding `global`. For all other cases, `local` wins.
pub fn merge_toml(global: toml::Value, local: toml::Value) -> toml::Value {
    match (global, local) {
        (toml::Value::Table(mut g), toml::Value::Table(l)) => {
            for (key, local_val) in l {
                let merged = if let Some(global_val) = g.remove(&key) {
                    merge_toml(global_val, local_val)
                } else {
                    local_val
                };
                g.insert(key, merged);
            }
            toml::Value::Table(g)
        }
        (_global, local) => local,
    }
}

/// Walk a TOML Value table and record source per leaf key (dot-separated path).
fn record_sources(
    val: &toml::Value,
    prefix: &str,
    source: &ConfigSource,
    map: &mut BTreeMap<String, ConfigSource>,
) {
    if let toml::Value::Table(table) = val {
        for (key, v) in table {
            let full_key = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{}.{}", prefix, key)
            };
            match v {
                toml::Value::Table(_) => record_sources(v, &full_key, source, map),
                _ => {
                    map.insert(full_key, source.clone());
                }
            }
        }
    }
}

impl Config {
    /// Return the global workgraph directory (~/.workgraph/)
    pub fn global_dir() -> anyhow::Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
        Ok(home.join(".workgraph"))
    }

    /// Return the global config file path (~/.workgraph/config.toml)
    pub fn global_config_path() -> anyhow::Result<PathBuf> {
        Ok(Self::global_dir()?.join("config.toml"))
    }

    /// Load global configuration from ~/.workgraph/config.toml.
    /// Returns None if the file doesn't exist, Err on parse failure.
    pub fn load_global() -> anyhow::Result<Option<Self>> {
        let global_path = Self::global_config_path()?;
        if !global_path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&global_path).map_err(|e| {
            anyhow::anyhow!(
                "Failed to read global config at {}: {}",
                global_path.display(),
                e
            )
        })?;
        let config: Config = toml::from_str(&content).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse global config at {}: {}",
                global_path.display(),
                e
            )
        })?;
        Ok(Some(config))
    }

    /// Load raw TOML value from a config file path.
    /// Returns empty table if file doesn't exist.
    fn load_toml_value(path: &Path) -> anyhow::Result<toml::Value> {
        if !path.exists() {
            return Ok(toml::Value::Table(toml::map::Map::new()));
        }
        let content = fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("Failed to read config at {}: {}", path.display(), e)
        })?;
        let val: toml::Value = content.parse().map_err(|e| {
            anyhow::anyhow!("Failed to parse config at {}: {}", path.display(), e)
        })?;
        Ok(val)
    }

    /// Load merged configuration: global config deep-merged with local config.
    /// Local keys override global keys. Missing files are treated as empty.
    pub fn load_merged(workgraph_dir: &Path) -> anyhow::Result<Self> {
        let global_path = Self::global_config_path()?;
        let local_path = workgraph_dir.join("config.toml");

        let global_val = Self::load_toml_value(&global_path)?;
        let local_val = Self::load_toml_value(&local_path)?;

        let merged = merge_toml(global_val, local_val);
        let config: Config = merged.try_into().map_err(|e| {
            anyhow::anyhow!("Failed to deserialize merged config: {}", e)
        })?;

        Ok(config)
    }

    /// Load configuration from .workgraph/config.toml (local only).
    /// Returns default config if file doesn't exist.
    pub fn load(workgraph_dir: &Path) -> anyhow::Result<Self> {
        let config_path = workgraph_dir.join("config.toml");

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&config_path)
            .map_err(|e| anyhow::anyhow!("Failed to read config: {}", e))?;

        let config: Config = toml::from_str(&content).map_err(|e| {
            anyhow::anyhow!("Failed to parse config at {}: {}", config_path.display(), e)
        })?;

        Ok(config)
    }

    /// Load configuration with global+local merge, falling back to defaults on error.
    ///
    /// Unlike `.load().unwrap_or_default()`, this emits a stderr warning
    /// when a config file exists but is corrupt, so the user knows
    /// their configuration is being ignored.
    pub fn load_or_default(workgraph_dir: &Path) -> Self {
        match Self::load_merged(workgraph_dir) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("Warning: {}, using defaults", e);
                Self::default()
            }
        }
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

    /// Save configuration to the global path (~/.workgraph/config.toml).
    /// Creates the ~/.workgraph/ directory if needed.
    pub fn save_global(&self) -> anyhow::Result<()> {
        let global_dir = Self::global_dir()?;
        fs::create_dir_all(&global_dir).map_err(|e| {
            anyhow::anyhow!(
                "Failed to create global config directory {}: {}",
                global_dir.display(),
                e
            )
        })?;

        let global_path = global_dir.join("config.toml");
        let content = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;

        fs::write(&global_path, content).map_err(|e| {
            anyhow::anyhow!(
                "Failed to write global config at {}: {}",
                global_path.display(),
                e
            )
        })?;

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

    /// Initialize default global config file if it doesn't exist
    pub fn init_global() -> anyhow::Result<bool> {
        let global_path = Self::global_config_path()?;

        if global_path.exists() {
            return Ok(false);
        }

        let config = Self::default();
        config.save_global()?;
        Ok(true)
    }

    /// Load merged config and record where each leaf key came from.
    pub fn load_with_sources(
        workgraph_dir: &Path,
    ) -> anyhow::Result<(Self, BTreeMap<String, ConfigSource>)> {
        let global_path = Self::global_config_path()?;
        let local_path = workgraph_dir.join("config.toml");

        let global_val = Self::load_toml_value(&global_path)?;
        let local_val = Self::load_toml_value(&local_path)?;

        // Record sources: global first, then local overwrites
        let mut sources = BTreeMap::new();
        record_sources(&global_val, "", &ConfigSource::Global, &mut sources);
        record_sources(&local_val, "", &ConfigSource::Local, &mut sources);

        // Merge and deserialize
        let merged = merge_toml(global_val, local_val);
        let config: Config = merged.try_into().map_err(|e| {
            anyhow::anyhow!("Failed to deserialize merged config: {}", e)
        })?;

        // Fill in defaults for keys not present in either file
        let default_config = Config::default();
        let default_val: toml::Value = toml::Value::try_from(&default_config)
            .unwrap_or(toml::Value::Table(toml::map::Map::new()));
        let mut default_sources = BTreeMap::new();
        record_sources(&default_val, "", &ConfigSource::Default, &mut default_sources);
        for (key, src) in default_sources {
            sources.entry(key).or_insert(src);
        }

        Ok((config, sources))
    }

    /// Build the executor command from template
    #[cfg(test)]
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
        assert_eq!(config.agent.model, "opus");
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
        assert!(cmd.contains("opus"));
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
        let config = MatrixConfig {
            homeserver_url: Some("https://matrix.org".to_string()),
            username: Some("@user:matrix.org".to_string()),
            access_token: Some("syt_abc123".to_string()),
            ..Default::default()
        };
        assert!(config.has_credentials());
    }

    #[test]
    fn test_default_agency_config() {
        let config = Config::default();
        assert!(!config.agency.auto_evaluate);
        assert!(!config.agency.auto_assign);
        assert!(config.agency.assigner_agent.is_none());
        assert!(config.agency.assigner_model.is_none());
        assert!(config.agency.evaluator_agent.is_none());
        assert!(config.agency.evaluator_model.is_none());
        assert!(config.agency.evolver_model.is_none());
        assert!(config.agency.evolver_agent.is_none());
        assert!(config.agency.retention_heuristics.is_none());
        // Run mode continuum defaults
        assert!((config.agency.run_mode - 0.2).abs() < f64::EPSILON);
        assert!((config.agency.min_exploration_rate - 0.05).abs() < f64::EPSILON);
        assert_eq!(config.agency.exploration_interval, 20);
        assert!((config.agency.cache_population_threshold - 0.8).abs() < f64::EPSILON);
        assert!((config.agency.ucb_exploration_constant - std::f64::consts::SQRT_2).abs() < f64::EPSILON);
        assert!((config.agency.novelty_bonus_multiplier - 1.5).abs() < f64::EPSILON);
        assert_eq!(config.agency.bizarre_ideation_interval, 10);
        assert!((config.agency.performance_threshold - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_agency_config() {
        let toml_str = r#"
[agency]
auto_evaluate = true
auto_assign = true
assigner_model = "haiku"
evaluator_model = "haiku"
evolver_model = "opus-4-5"
assigner_agent = "abc123"
evaluator_agent = "def456"
evolver_agent = "ghi789"
retention_heuristics = "Retire roles scoring below 0.3 after 10 evaluations"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.agency.auto_evaluate);
        assert!(config.agency.auto_assign);
        assert_eq!(config.agency.assigner_model, Some("haiku".to_string()));
        assert_eq!(config.agency.evaluator_model, Some("haiku".to_string()));
        assert_eq!(config.agency.evolver_model, Some("opus-4-5".to_string()));
        assert_eq!(config.agency.assigner_agent, Some("abc123".to_string()));
        assert_eq!(config.agency.evaluator_agent, Some("def456".to_string()));
        assert_eq!(config.agency.evolver_agent, Some("ghi789".to_string()));
        assert_eq!(
            config.agency.retention_heuristics,
            Some("Retire roles scoring below 0.3 after 10 evaluations".to_string())
        );
    }

    #[test]
    fn test_agency_config_roundtrip() {
        let temp_dir = TempDir::new().unwrap();

        let mut config = Config::default();
        config.agency.auto_evaluate = true;
        config.agency.evolver_agent = Some("abc123".to_string());
        config.agency.evaluator_model = Some("sonnet".to_string());
        config.save(temp_dir.path()).unwrap();

        let loaded = Config::load(temp_dir.path()).unwrap();
        assert!(loaded.agency.auto_evaluate);
        assert_eq!(loaded.agency.evolver_agent, Some("abc123".to_string()));
        assert_eq!(loaded.agency.evaluator_model, Some("sonnet".to_string()));
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

    // ---- Global config / merge tests ----

    #[test]
    fn test_merge_toml_basic() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "sonnet"
executor = "claude"
"#,
        )
        .unwrap();
        let local: toml::Value = toml::from_str(
            r#"
[coordinator]
max_agents = 8
"#,
        )
        .unwrap();
        let merged = merge_toml(global, local);
        let table = merged.as_table().unwrap();
        // Global agent section preserved
        let agent = table["agent"].as_table().unwrap();
        assert_eq!(agent["model"].as_str().unwrap(), "sonnet");
        // Local coordinator section present
        let coord = table["coordinator"].as_table().unwrap();
        assert_eq!(coord["max_agents"].as_integer().unwrap(), 8);
    }

    #[test]
    fn test_merge_toml_local_overrides_global() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "sonnet"
executor = "claude"
interval = 10
"#,
        )
        .unwrap();
        let local: toml::Value = toml::from_str(
            r#"
[agent]
model = "haiku"
"#,
        )
        .unwrap();
        let merged = merge_toml(global, local);
        let agent = merged.as_table().unwrap()["agent"].as_table().unwrap();
        // Local overrides model
        assert_eq!(agent["model"].as_str().unwrap(), "haiku");
        // Global's executor preserved
        assert_eq!(agent["executor"].as_str().unwrap(), "claude");
        // Global's interval preserved
        assert_eq!(agent["interval"].as_integer().unwrap(), 10);
    }

    #[test]
    fn test_merge_toml_nested_sections() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "sonnet"

[coordinator]
max_agents = 4
executor = "claude"
"#,
        )
        .unwrap();
        let local: toml::Value = toml::from_str(
            r#"
[agent]
model = "haiku"

[coordinator]
executor = "amplifier"
"#,
        )
        .unwrap();
        let merged = merge_toml(global, local);
        let t = merged.as_table().unwrap();
        assert_eq!(
            t["agent"].as_table().unwrap()["model"].as_str().unwrap(),
            "haiku"
        );
        assert_eq!(
            t["coordinator"].as_table().unwrap()["max_agents"]
                .as_integer()
                .unwrap(),
            4
        );
        assert_eq!(
            t["coordinator"].as_table().unwrap()["executor"]
                .as_str()
                .unwrap(),
            "amplifier"
        );
    }

    #[test]
    fn test_merge_toml_empty_local() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "sonnet"
"#,
        )
        .unwrap();
        let local = toml::Value::Table(toml::map::Map::new());
        let merged = merge_toml(global, local);
        assert_eq!(
            merged.as_table().unwrap()["agent"]
                .as_table()
                .unwrap()["model"]
                .as_str()
                .unwrap(),
            "sonnet"
        );
    }

    #[test]
    fn test_merge_toml_empty_global() {
        let global = toml::Value::Table(toml::map::Map::new());
        let local: toml::Value = toml::from_str(
            r#"
[agent]
model = "haiku"
"#,
        )
        .unwrap();
        let merged = merge_toml(global, local);
        assert_eq!(
            merged.as_table().unwrap()["agent"]
                .as_table()
                .unwrap()["model"]
                .as_str()
                .unwrap(),
            "haiku"
        );
    }

    #[test]
    fn test_load_merged_no_global_file() {
        // When no global config exists, load_merged should still work
        // (loads only local). We test with a temp dir as local.
        let temp_dir = TempDir::new().unwrap();
        let local_toml = r#"
[agent]
model = "haiku"
"#;
        fs::write(temp_dir.path().join("config.toml"), local_toml).unwrap();

        // This test depends on whether ~/.workgraph/config.toml exists on the
        // machine, but the merge should work either way.
        let config = Config::load_merged(temp_dir.path()).unwrap();
        assert_eq!(config.agent.model, "haiku");
    }

    #[test]
    fn test_load_merged_no_local_file() {
        // When no local config exists, merged should be global + defaults
        let temp_dir = TempDir::new().unwrap();
        // No config.toml in temp_dir
        let config = Config::load_merged(temp_dir.path()).unwrap();
        // Should at least have defaults
        assert_eq!(config.agent.executor, "claude");
    }

    #[test]
    fn test_global_config_path() {
        let path = Config::global_config_path().unwrap();
        assert!(path.ends_with(".workgraph/config.toml"));
    }

    #[test]
    fn test_config_source_display() {
        assert_eq!(ConfigSource::Global.to_string(), "global");
        assert_eq!(ConfigSource::Local.to_string(), "local");
        assert_eq!(ConfigSource::Default.to_string(), "default");
    }
}
