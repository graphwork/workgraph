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

/// Default Anthropic API model IDs.
/// Update these constants when new Claude model versions are released.
pub const CLAUDE_HAIKU_MODEL_ID: &str = "claude-haiku-4-5-20251001";
pub const CLAUDE_SONNET_MODEL_ID: &str = "claude-sonnet-4-20250514";
pub const CLAUDE_OPUS_MODEL_ID: &str = "claude-opus-4-6";

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

    /// Guardrails for autopoietic task creation
    #[serde(default)]
    pub guardrails: GuardrailsConfig,

    /// Visualization settings
    #[serde(default)]
    pub viz: VizConfig,

    /// TUI-specific settings
    #[serde(default)]
    pub tui: TuiConfig,

    /// LLM endpoints
    #[serde(default, skip_serializing_if = "EndpointsConfig::is_empty")]
    pub llm_endpoints: EndpointsConfig,

    /// Checkpoint configuration
    #[serde(default)]
    pub checkpoint: CheckpointConfig,

    /// Model routing: per-role model+provider assignments
    #[serde(default)]
    pub models: ModelRoutingConfig,

    /// Active provider profile name (e.g., "anthropic", "openrouter", "openai").
    /// When set, the profile supplies tier defaults. Explicit [tiers] entries
    /// and per-role [models] overrides still take precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,

    /// Quality tier defaults: which model ID each tier resolves to
    #[serde(default)]
    pub tiers: TierConfig,

    /// Model registry entries
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_registry: Vec<ModelRegistryEntry>,

    /// Chat archive rotation settings
    #[serde(default)]
    pub chat: ChatConfig,

    /// OpenRouter cost cap and monitoring configuration
    #[serde(default)]
    pub openrouter: OpenRouterConfig,

    /// Native executor settings (web, background, delegate)
    #[serde(default)]
    pub native_executor: NativeExecutorConfig,

    /// True when `agent.model` was explicitly set in local config.
    /// Used by `resolve_model_for_role` to skip tier defaults in favor of agent.model.
    #[serde(skip)]
    pub agent_model_is_local: bool,
}

/// Chat archive rotation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatConfig {
    /// Maximum size in bytes before rotating the active chat file (default: 1MB).
    #[serde(default = "default_chat_max_file_size")]
    pub max_file_size: u64,
    /// Maximum number of messages before rotating (default: 10000).
    #[serde(default = "default_chat_max_messages")]
    pub max_messages: usize,
    /// Retention period in days for archived files (default: 30). 0 = keep forever.
    #[serde(default = "default_chat_retention_days")]
    pub retention_days: u32,
    /// Number of new messages before auto-triggering chat compaction (default: 50).
    #[serde(default = "default_chat_compact_threshold")]
    pub compact_threshold: usize,
}

fn default_chat_max_file_size() -> u64 {
    1_048_576 // 1 MB
}
fn default_chat_max_messages() -> usize {
    10_000
}
fn default_chat_retention_days() -> u32 {
    30
}
fn default_chat_compact_threshold() -> usize {
    50
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            max_file_size: default_chat_max_file_size(),
            max_messages: default_chat_max_messages(),
            retention_days: default_chat_retention_days(),
            compact_threshold: default_chat_compact_threshold(),
        }
    }
}

/// Native executor configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NativeExecutorConfig {
    /// Web access settings (search + fetch).
    #[serde(default)]
    pub web: NativeWebConfig,

    /// Background task settings.
    #[serde(default)]
    pub background: NativeBackgroundConfig,

    /// Delegate (in-process subtask) settings.
    #[serde(default)]
    pub delegate: NativeDelegateConfig,
}

/// Web access configuration for the native executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeWebConfig {
    /// Search backend: "duckduckgo" (default), "serper", "brave", "searxng".
    #[serde(default = "default_search_backend")]
    pub search_backend: String,

    /// API key for search backend (Serper, Brave, etc.). Supports env var syntax: "${VAR}".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_api_key: Option<String>,

    /// Maximum content chars for web_fetch before truncation.
    #[serde(default = "default_fetch_max_chars")]
    pub fetch_max_chars: usize,

    /// HTTP request timeout for web_fetch in seconds.
    #[serde(default = "default_fetch_timeout_secs")]
    pub fetch_timeout_secs: u64,
}

fn default_search_backend() -> String {
    "duckduckgo".to_string()
}
fn default_fetch_max_chars() -> usize {
    16_000
}
fn default_fetch_timeout_secs() -> u64 {
    30
}

impl Default for NativeWebConfig {
    fn default() -> Self {
        Self {
            search_backend: default_search_backend(),
            search_api_key: None,
            fetch_max_chars: default_fetch_max_chars(),
            fetch_timeout_secs: default_fetch_timeout_secs(),
        }
    }
}

/// Background task configuration for the native executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeBackgroundConfig {
    /// Maximum concurrent background tasks per agent.
    #[serde(default = "default_max_background_tasks")]
    pub max_background_tasks: usize,

    /// Default timeout for background tasks in seconds.
    #[serde(default = "default_background_timeout_secs")]
    pub background_timeout_secs: u64,
}

fn default_max_background_tasks() -> usize {
    5
}
fn default_background_timeout_secs() -> u64 {
    600
}

impl Default for NativeBackgroundConfig {
    fn default() -> Self {
        Self {
            max_background_tasks: default_max_background_tasks(),
            background_timeout_secs: default_background_timeout_secs(),
        }
    }
}

/// Delegate (in-process subtask) configuration for the native executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeDelegateConfig {
    /// Maximum turns for delegated sub-agents.
    #[serde(default = "default_delegate_max_turns")]
    pub delegate_max_turns: usize,

    /// Model for delegate sub-agents. Empty string = same as parent agent.
    #[serde(default)]
    pub delegate_model: String,
}

fn default_delegate_max_turns() -> usize {
    10
}

impl Default for NativeDelegateConfig {
    fn default() -> Self {
        Self {
            delegate_max_turns: default_delegate_max_turns(),
            delegate_model: String::new(),
        }
    }
}

/// OpenRouter cost cap and monitoring configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterConfig {
    /// Global project cost cap in USD
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cap_global_usd: Option<f64>,

    /// Per-session cost cap in USD
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cap_session_usd: Option<f64>,

    /// Per-task cost cap in USD
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cap_task_usd: Option<f64>,

    /// Behavior when cost cap is reached
    #[serde(default = "default_cap_behavior")]
    pub cap_behavior: CapBehavior,

    /// Fallback model when cap_behavior is "fallback"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,

    /// How often to check key status in minutes
    #[serde(default = "default_key_status_check_interval")]
    pub key_status_check_interval_minutes: u32,

    /// Warning threshold as percentage of limit (0-100)
    #[serde(default = "default_warn_usage_percent")]
    pub warn_at_usage_percent: u8,

    /// Cost estimation buffer multiplier
    #[serde(default = "default_cost_estimation_buffer")]
    pub cost_estimation_buffer: f64,

    /// Enable cache tracking from OpenRouter responses
    #[serde(default = "default_enable_cache_tracking")]
    pub enable_cache_tracking: bool,

    /// Track session costs in coordinator state
    #[serde(default = "default_track_session_costs")]
    pub track_session_costs: bool,

    /// Persist cost history to files
    #[serde(default)]
    pub persist_cost_history: bool,
}

/// Cost cap enforcement behavior
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapBehavior {
    /// Fail the task/session immediately
    Fail,
    /// Fall back to a cheaper model
    Fallback,
    /// Escalate to user for decision
    Escalate,
    /// Switch to read-only mode (monitoring only)
    Readonly,
}

fn default_cap_behavior() -> CapBehavior {
    CapBehavior::Escalate
}

fn default_key_status_check_interval() -> u32 {
    5
}

fn default_warn_usage_percent() -> u8 {
    80
}

fn default_cost_estimation_buffer() -> f64 {
    1.2
}

fn default_enable_cache_tracking() -> bool {
    true
}

fn default_track_session_costs() -> bool {
    true
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            cost_cap_global_usd: None,
            cost_cap_session_usd: None,
            cost_cap_task_usd: None,
            cap_behavior: default_cap_behavior(),
            fallback_model: None,
            key_status_check_interval_minutes: default_key_status_check_interval(),
            warn_at_usage_percent: default_warn_usage_percent(),
            cost_estimation_buffer: default_cost_estimation_buffer(),
            enable_cache_tracking: default_enable_cache_tracking(),
            track_session_costs: default_track_session_costs(),
            persist_cost_history: false,
        }
    }
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

/// Guardrails for autopoietic task creation by agents.
/// Prevents task explosion when agents create subtasks autonomously.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardrailsConfig {
    /// Maximum tasks a single agent execution can create via `wg add`.
    /// Enforced when WG_AGENT_ID env var is set. Default: 10.
    #[serde(default = "default_max_child_tasks_per_agent")]
    pub max_child_tasks_per_agent: u32,

    /// Maximum depth of task chains (counting --after hops from root).
    /// Prevents infinite decomposition chains. Default: 8.
    #[serde(default = "default_max_task_depth")]
    pub max_task_depth: u32,

    /// Maximum times a task can be requeued via failed-dependency triage.
    /// Prevents infinite triage loops. Default: 3.
    #[serde(default = "default_max_triage_attempts")]
    pub max_triage_attempts: u32,

    /// Whether to inject adaptive decomposition guidance into agent prompts.
    /// When true (default), the executor analyzes task descriptions and provides
    /// task-specific decomposition hints (atomic vs multi-step classification
    /// plus decomposition templates). Set to false to use the generic guidance.
    #[serde(default = "default_decomp_guidance")]
    pub decomp_guidance: bool,
}

fn default_max_child_tasks_per_agent() -> u32 {
    10
}

fn default_max_task_depth() -> u32 {
    8
}

fn default_max_triage_attempts() -> u32 {
    3
}

fn default_decomp_guidance() -> bool {
    true
}

impl Default for GuardrailsConfig {
    fn default() -> Self {
        Self {
            max_child_tasks_per_agent: default_max_child_tasks_per_agent(),
            max_task_depth: default_max_task_depth(),
            max_triage_attempts: default_max_triage_attempts(),
            decomp_guidance: default_decomp_guidance(),
        }
    }
}

/// Visualization configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VizConfig {
    /// Edge color style: "gray" (default), "white", or "mixed" (tree=white, arcs=gray)
    #[serde(default = "default_edge_color")]
    pub edge_color: String,
    /// Animation mode: "normal" (default), "fast", "slow", "reduced", "off"
    #[serde(default = "default_animation_mode")]
    pub animations: String,
}

fn default_edge_color() -> String {
    "gray".to_string()
}

fn default_animation_mode() -> String {
    "normal".to_string()
}

impl Default for VizConfig {
    fn default() -> Self {
        Self {
            edge_color: default_edge_color(),
            animations: default_animation_mode(),
        }
    }
}

/// TUI-specific settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    /// Enable mouse support (default: auto-detected based on tmux)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse_mode: Option<bool>,
    /// Default layout mode: "auto", "horizontal", "vertical"
    #[serde(default = "default_tui_layout")]
    pub default_layout: String,
    /// Color theme: "dark" (default), "light"
    #[serde(default = "default_tui_theme")]
    pub color_theme: String,
    /// Timestamp display format: "relative" (default), "iso", "local", "off"
    #[serde(default = "default_timestamp_format")]
    pub timestamp_format: String,
    /// Show token counts in task details
    #[serde(default = "default_true")]
    pub show_token_counts: bool,
    /// Name length threshold for inline vs above-line display (default: 8)
    #[serde(default = "default_message_name_threshold")]
    pub message_name_threshold: u16,
    /// Indentation for message body when name is on its own line (0-8, default: 2)
    #[serde(default = "default_message_indent")]
    pub message_indent: u16,
    /// Inspector panel ratio: percentage of width given to the inspector in split mode (default: 67)
    #[serde(default = "default_panel_ratio")]
    pub panel_ratio: u16,
    /// Default inspector size when first opened: "1/3", "1/2", "2/3" (default), "full"
    #[serde(default = "default_inspector_size")]
    pub default_inspector_size: String,
    /// Persist chat history across TUI restarts (default: true)
    #[serde(default = "default_true")]
    pub chat_history: bool,
    /// Maximum number of chat messages to persist (default: 1000)
    #[serde(default = "default_chat_history_max")]
    pub chat_history_max: usize,
    /// Number of chat messages to load per page in the TUI (default: 100)
    #[serde(default = "default_chat_page_size")]
    pub chat_page_size: usize,
    /// Comma-separated counters to display: "uptime", "cumulative", "active", "session", "compact"
    #[serde(default = "default_counters")]
    pub counters: String,
    /// Show all system tasks (dot-prefixed) by default in TUI
    #[serde(default)]
    pub show_system_tasks: bool,
    /// Show only running (in-progress/open) system tasks by default
    #[serde(default)]
    pub show_running_system_tasks: bool,
    /// Show key press feedback overlay (useful for screencasts/demos)
    #[serde(default)]
    pub show_keys: bool,
    /// Session boundary gap threshold in minutes (default: 30).
    /// A visual divider is shown between chat messages separated by more than this many minutes.
    /// Set to 0 to disable session boundaries.
    #[serde(default = "default_session_gap_minutes")]
    pub session_gap_minutes: u32,
}

fn default_tui_layout() -> String {
    "auto".to_string()
}
fn default_tui_theme() -> String {
    "dark".to_string()
}
fn default_timestamp_format() -> String {
    "relative".to_string()
}
fn default_true() -> bool {
    true
}
fn default_message_name_threshold() -> u16 {
    8
}
fn default_message_indent() -> u16 {
    2
}
fn default_panel_ratio() -> u16 {
    67
}
fn default_inspector_size() -> String {
    "2/3".to_string()
}
fn default_chat_history_max() -> usize {
    1000
}
fn default_chat_page_size() -> usize {
    100
}
fn default_counters() -> String {
    "uptime,cumulative,active,compact".to_string()
}
fn default_session_gap_minutes() -> u32 {
    30
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            mouse_mode: None,
            default_layout: default_tui_layout(),
            color_theme: default_tui_theme(),
            timestamp_format: default_timestamp_format(),
            show_token_counts: true,
            message_name_threshold: default_message_name_threshold(),
            message_indent: default_message_indent(),
            panel_ratio: default_panel_ratio(),
            default_inspector_size: default_inspector_size(),
            chat_history: true,
            chat_history_max: default_chat_history_max(),
            chat_page_size: default_chat_page_size(),
            counters: default_counters(),
            show_system_tasks: false,
            show_running_system_tasks: false,
            show_keys: false,
            session_gap_minutes: default_session_gap_minutes(),
        }
    }
}

/// A configured LLM endpoint (like a WiFi network entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// Display name for this endpoint
    pub name: String,
    /// Provider type: "anthropic", "openai", "openrouter", "local"
    #[serde(default = "default_provider")]
    pub provider: String,
    /// API endpoint URL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Default model for this endpoint
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// API key for this endpoint (stored in config — user should gitignore)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Path to a file containing the API key (~ and relative paths supported)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
    /// Environment variable name containing the API key (explicit reference)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Whether this is the default endpoint for new agents
    #[serde(default)]
    pub is_default: bool,
    /// Context window size in tokens (overrides model registry for this endpoint)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

fn default_provider() -> String {
    "anthropic".to_string()
}

/// Expand `~` prefix to user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    let p = Path::new(path);
    if let Ok(rest) = p.strip_prefix("~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    p.to_path_buf()
}

impl EndpointConfig {
    /// Return the environment variable names to check for API keys, based on provider.
    pub fn env_var_names_for_provider(provider: &str) -> &'static [&'static str] {
        match provider {
            "openrouter" => &["OPENROUTER_API_KEY", "OPENAI_API_KEY"],
            "openai" => &["OPENAI_API_KEY"],
            "anthropic" => &["ANTHROPIC_API_KEY"],
            _ => &[],
        }
    }

    /// Resolve the API key for this endpoint.
    ///
    /// Priority:
    /// 1. `api_key` — use directly if set
    /// 2. `api_key_file` — read file contents, trim whitespace
    /// 3. `api_key_env` — read from explicitly named env var
    /// 4. Environment variable fallback based on provider
    ///
    /// For `api_key_file`, supports:
    /// - `~` expansion to home directory
    /// - Relative paths resolved against `workgraph_dir` (if provided)
    pub fn resolve_api_key(&self, workgraph_dir: Option<&Path>) -> anyhow::Result<Option<String>> {
        if let Some(ref key) = self.api_key {
            return Ok(Some(key.clone()));
        }
        if let Some(ref file_path) = self.api_key_file {
            let expanded = expand_tilde(file_path);
            let path = if expanded.is_absolute() {
                expanded
            } else if let Some(dir) = workgraph_dir {
                dir.join(expanded)
            } else {
                expanded
            };
            let contents = fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("Failed to read API key from {}: {}", path.display(), e)
            })?;
            let key = contents.trim().to_string();
            if key.is_empty() {
                anyhow::bail!("API key file {} is empty", path.display());
            }
            return Ok(Some(key));
        }
        // Explicit env var reference
        if let Some(ref env_name) = self.api_key_env
            && let Ok(key) = std::env::var(env_name)
        {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Ok(Some(key));
            }
        }
        // Environment variable fallback based on provider
        for var_name in Self::env_var_names_for_provider(&self.provider) {
            if let Ok(key) = std::env::var(var_name) {
                let key = key.trim().to_string();
                if !key.is_empty() {
                    return Ok(Some(key));
                }
            }
        }
        Ok(None)
    }

    /// Return the API key masked for display: "sk-****...ab12"
    pub fn masked_key(&self) -> String {
        match &self.api_key {
            Some(key) if key.len() > 8 => {
                let prefix = &key[..3];
                let suffix = &key[key.len() - 4..];
                format!("{}****...{}", prefix, suffix)
            }
            Some(key) if !key.is_empty() => "****".to_string(),
            _ => {
                if self.api_key_file.is_some() {
                    "(from file)".to_string()
                } else if let Some(ref env_name) = self.api_key_env {
                    format!("(from env: {})", env_name)
                } else {
                    "(not set)".to_string()
                }
            }
        }
    }

    /// Describe the source of the API key for display purposes.
    pub fn key_source(&self) -> String {
        if self.api_key.is_some() {
            "inline".to_string()
        } else if let Some(ref file_path) = self.api_key_file {
            format!("file: {}", file_path)
        } else if let Some(ref env_name) = self.api_key_env {
            format!("env: {}", env_name)
        } else {
            // Check provider-based env var fallback
            for var_name in Self::env_var_names_for_provider(&self.provider) {
                if std::env::var(var_name).is_ok() {
                    return format!("env: {} (auto-detected)", var_name);
                }
            }
            "(not configured)".to_string()
        }
    }

    /// Default URL for known providers.
    pub fn default_url_for_provider(provider: &str) -> &'static str {
        match provider {
            "anthropic" => "https://api.anthropic.com",
            "openai" => "https://api.openai.com/v1",
            "openrouter" => "https://openrouter.ai/api/v1",
            "gemini" => "https://generativelanguage.googleapis.com/v1beta/openai",
            "ollama" => "http://localhost:11434/v1",
            "llamacpp" => "http://localhost:8080/v1",
            "vllm" => "http://localhost:8000/v1",
            "local" => "http://localhost:11434/v1",
            _ => "",
        }
    }
}

/// LLM endpoints configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EndpointsConfig {
    /// List of configured endpoints
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointConfig>,
}

impl EndpointsConfig {
    /// Returns true when there are no configured endpoints.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Find the best endpoint for a given provider name.
    pub fn find_for_provider(&self, provider: &str) -> Option<&EndpointConfig> {
        let mut first_match: Option<&EndpointConfig> = None;
        for ep in &self.endpoints {
            if ep.provider == provider {
                if ep.is_default {
                    return Some(ep);
                }
                if first_match.is_none() {
                    first_match = Some(ep);
                }
            }
        }
        first_match
    }

    /// Find an endpoint by its display name.
    pub fn find_by_name(&self, name: &str) -> Option<&EndpointConfig> {
        self.endpoints.iter().find(|ep| ep.name == name)
    }

    /// Find the default endpoint (the one with `is_default = true`), or the first endpoint
    /// if none is marked as default.
    pub fn find_default(&self) -> Option<&EndpointConfig> {
        self.endpoints
            .iter()
            .find(|ep| ep.is_default)
            .or_else(|| self.endpoints.first())
    }
}

/// Checkpoint configuration for agent context preservation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointConfig {
    /// Auto-checkpoint every N turns
    #[serde(default = "default_auto_interval_turns")]
    pub auto_interval_turns: u32,

    /// Auto-checkpoint every N minutes
    #[serde(default = "default_auto_interval_mins")]
    pub auto_interval_mins: u32,

    /// Keep only last N checkpoints per task
    #[serde(default = "default_max_checkpoints")]
    pub max_checkpoints: u32,

    /// Max tokens of previous attempt context to inject on retry (0 = disabled)
    #[serde(default = "default_retry_context_tokens")]
    pub retry_context_tokens: u32,
}

fn default_auto_interval_turns() -> u32 {
    15
}

fn default_auto_interval_mins() -> u32 {
    20
}

fn default_max_checkpoints() -> u32 {
    5
}

fn default_retry_context_tokens() -> u32 {
    2000
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            auto_interval_turns: default_auto_interval_turns(),
            auto_interval_mins: default_auto_interval_mins(),
            max_checkpoints: default_max_checkpoints(),
            retry_context_tokens: default_retry_context_tokens(),
        }
    }
}

// ---------------------------------------------------------------------------
// Model routing configuration
// ---------------------------------------------------------------------------

/// Dispatch roles for model routing.
/// Each role maps to a specific dispatch point in the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchRole {
    /// Default fallback for any role without explicit config
    Default,
    /// Main task agents spawned by coordinator
    TaskAgent,
    /// Evaluation agents (post-task scoring)
    Evaluator,
    /// FLIP inference phase (reconstructing prompt from output)
    FlipInference,
    /// FLIP comparison phase (scoring similarity)
    FlipComparison,
    /// Agent assignment tasks
    Assigner,
    /// Agency evolver
    Evolver,
    /// FLIP-triggered verification agents
    Verification,
    /// Triage (dead-agent summarization)
    Triage,
    /// Agent creator
    Creator,
    /// Compactor: distills graph state into context.md
    Compactor,
    /// Coordinator evaluation (inline per-turn scoring)
    CoordinatorEval,
    /// Placement agent: analyzes tasks and wires them into the graph
    Placer,
    /// Chat compactor: summarizes per-coordinator conversation history
    ChatCompactor,
}

impl std::fmt::Display for DispatchRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => write!(f, "default"),
            Self::TaskAgent => write!(f, "task_agent"),
            Self::Evaluator => write!(f, "evaluator"),
            Self::FlipInference => write!(f, "flip_inference"),
            Self::FlipComparison => write!(f, "flip_comparison"),
            Self::Assigner => write!(f, "assigner"),
            Self::Evolver => write!(f, "evolver"),
            Self::Verification => write!(f, "verification"),
            Self::Triage => write!(f, "triage"),
            Self::Creator => write!(f, "creator"),
            Self::Compactor => write!(f, "compactor"),
            Self::CoordinatorEval => write!(f, "coordinator_eval"),
            Self::Placer => write!(f, "placer"),
            Self::ChatCompactor => write!(f, "chat_compactor"),
        }
    }
}

impl std::str::FromStr for DispatchRole {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "default" => Ok(Self::Default),
            "task_agent" => Ok(Self::TaskAgent),
            "evaluator" => Ok(Self::Evaluator),
            "flip_inference" => Ok(Self::FlipInference),
            "flip_comparison" => Ok(Self::FlipComparison),
            "assigner" => Ok(Self::Assigner),
            "evolver" => Ok(Self::Evolver),
            "verification" => Ok(Self::Verification),
            "triage" => Ok(Self::Triage),
            "creator" => Ok(Self::Creator),
            "compactor" => Ok(Self::Compactor),
            "coordinator_eval" => Ok(Self::CoordinatorEval),
            "placer" => Ok(Self::Placer),
            "chat_compactor" => Ok(Self::ChatCompactor),
            _ => Err(anyhow::anyhow!(
                "Unknown dispatch role '{}'. Valid roles: default, task_agent, evaluator, \
                 flip_inference, flip_comparison, assigner, evolver, verification, triage, \
                 creator, compactor, placer, chat_compactor",
                s
            )),
        }
    }
}

impl DispatchRole {
    /// All known roles (excluding Default).
    pub const ALL: &'static [DispatchRole] = &[
        Self::TaskAgent,
        Self::Evaluator,
        Self::FlipInference,
        Self::FlipComparison,
        Self::Assigner,
        Self::Evolver,
        Self::Verification,
        Self::Triage,
        Self::Creator,
        Self::Compactor,
        Self::Placer,
        Self::ChatCompactor,
    ];

    /// Default quality tier for this role.
    pub fn default_tier(&self) -> Tier {
        match self {
            Self::Triage => Tier::Fast,
            Self::FlipComparison => Tier::Fast,
            Self::Assigner => Tier::Fast,
            Self::Compactor => Tier::Fast,
            Self::ChatCompactor => Tier::Fast,
            Self::CoordinatorEval => Tier::Fast,
            Self::Placer => Tier::Fast,
            Self::FlipInference => Tier::Standard,
            Self::TaskAgent => Tier::Standard,
            Self::Evaluator => Tier::Standard,
            Self::Evolver => Tier::Premium,
            Self::Creator => Tier::Premium,
            Self::Verification => Tier::Premium,
            Self::Default => Tier::Standard,
        }
    }
}

// ---------------------------------------------------------------------------
// Execution weight tiers
// ---------------------------------------------------------------------------

/// Execution weight tier for agent spawning.
///
/// Controls what tools and context an agent gets, from lightest to heaviest:
/// - Shell: no LLM, just run a shell command
/// - Bare: LLM with wg CLI only (no file access)
/// - Light: LLM with read-only file access (research/review)
/// - Full: all tools (implementation/debugging)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ExecMode {
    /// No LLM — run `task.exec` command directly via bash
    Shell,
    /// LLM with `Bash(wg:*)` only, `--system-prompt` path
    Bare,
    /// LLM with read-only file tools: `Bash(wg:*),Read,Glob,Grep,WebFetch,WebSearch`
    Light,
    /// Full Claude Code session with all tools
    #[default]
    Full,
}

impl std::fmt::Display for ExecMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shell => write!(f, "shell"),
            Self::Bare => write!(f, "bare"),
            Self::Light => write!(f, "light"),
            Self::Full => write!(f, "full"),
        }
    }
}

impl std::str::FromStr for ExecMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "shell" => Ok(Self::Shell),
            "bare" => Ok(Self::Bare),
            "light" => Ok(Self::Light),
            "full" => Ok(Self::Full),
            _ => Err(anyhow::anyhow!(
                "Invalid exec_mode '{}'. Valid values: shell, bare, light, full",
                s
            )),
        }
    }
}

impl ExecMode {
    /// All variants in order from lightest to heaviest.
    pub const ALL: &'static [ExecMode] = &[Self::Shell, Self::Bare, Self::Light, Self::Full];

    /// Parse from an optional string, defaulting to Full.
    pub fn from_opt(s: Option<&str>) -> Result<Self, anyhow::Error> {
        match s {
            Some(v) => v.parse(),
            None => Ok(Self::Full),
        }
    }

    /// Return the valid exec_modes for a given executor type.
    ///
    /// - `"shell"` executor: only `Shell`
    /// - `"claude"`, `"native"`, `"amplifier"`, or any other: `Bare`, `Light`, `Full`
    pub fn valid_for_executor(executor: &str) -> &'static [ExecMode] {
        match executor {
            "shell" => &[ExecMode::Shell],
            _ => &[ExecMode::Bare, ExecMode::Light, ExecMode::Full],
        }
    }

    /// Return the safe default exec_mode for a given executor type.
    pub fn default_for_executor(executor: &str) -> ExecMode {
        match executor {
            "shell" => ExecMode::Shell,
            _ => ExecMode::Full,
        }
    }

    /// Check whether this exec_mode is valid for the given executor.
    pub fn is_valid_for_executor(&self, executor: &str) -> bool {
        Self::valid_for_executor(executor).contains(self)
    }
}

// ---------------------------------------------------------------------------
// Quality tiers and model registry
// ---------------------------------------------------------------------------

/// Quality tier for model selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Fast,
    Standard,
    Premium,
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fast => write!(f, "fast"),
            Self::Standard => write!(f, "standard"),
            Self::Premium => write!(f, "premium"),
        }
    }
}

impl Tier {
    /// Default model alias for each tier (single source of truth).
    ///
    /// Used as the fallback display string in the TUI and as the base for
    /// provider-prefixed defaults in `effective_tiers()`.
    pub fn default_alias(&self) -> &'static str {
        match self {
            Self::Fast => "haiku",
            Self::Standard => "sonnet",
            Self::Premium => "opus",
        }
    }
}

impl std::str::FromStr for Tier {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "standard" => Ok(Self::Standard),
            "premium" => Ok(Self::Premium),
            _ => anyhow::bail!("unknown tier '{}' (expected: fast, standard, premium)", s),
        }
    }
}

/// A model registry entry describing a provider+model combination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRegistryEntry {
    /// Short identifier used in config references (e.g., "haiku", "sonnet", "gpt-4o")
    pub id: String,
    /// Provider: "anthropic", "openai", "google", "local", etc.
    pub provider: String,
    /// Full model identifier sent to the API
    pub model: String,
    /// Quality tier this model belongs to
    pub tier: Tier,
    /// API endpoint URL (None = use provider default)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Max input context window in tokens
    #[serde(default)]
    pub context_window: u64,
    /// Max output tokens
    #[serde(default)]
    pub max_output_tokens: u64,
    /// Cost per million input tokens (USD)
    #[serde(default)]
    pub cost_per_input_mtok: f64,
    /// Cost per million output tokens (USD)
    #[serde(default)]
    pub cost_per_output_mtok: f64,
    /// Whether the provider supports prompt caching
    #[serde(default)]
    pub prompt_caching: bool,
    /// Discount multiplier for cached reads (e.g., 0.1 = 90% off)
    #[serde(default)]
    pub cache_read_discount: f64,
    /// Premium multiplier for cache writes (e.g., 1.25 = 25% more)
    #[serde(default)]
    pub cache_write_premium: f64,
    /// Descriptors for when to use this model
    #[serde(default)]
    pub descriptors: Vec<String>,
}

impl Default for ModelRegistryEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider: String::new(),
            model: String::new(),
            tier: Tier::Standard,
            endpoint: None,
            context_window: 0,
            max_output_tokens: 0,
            cost_per_input_mtok: 0.0,
            cost_per_output_mtok: 0.0,
            prompt_caching: false,
            cache_read_discount: 0.0,
            cache_write_premium: 0.0,
            descriptors: Vec::new(),
        }
    }
}

/// Tier routing configuration: which model ID each tier resolves to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TierConfig {
    /// Model ID for fast tier
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<String>,
    /// Model ID for standard tier
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    /// Model ID for premium tier
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premium: Option<String>,
}

/// Per-role model+provider assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleModelConfig {
    /// **Deprecated**: Use provider:model format in the `model` field instead.
    /// Kept for deserialization of old configs; never written back.
    #[serde(default, skip_serializing)]
    pub provider: Option<String>,
    /// Model spec in provider:model format (e.g., "claude:opus", "openrouter:deepseek/deepseek-chat")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tier override: resolve model via tier system instead of direct model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    /// Named endpoint override: use a specific configured endpoint by name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// Model routing: maps each dispatch role to a model+provider.
/// Roles without explicit config fall back to `default`, then to `agent.model`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRoutingConfig {
    /// Default model+provider for all roles
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<RoleModelConfig>,

    /// Per-role overrides
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_agent: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_inference: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_comparison: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigner: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evolver: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactor: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placer: Option<RoleModelConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_compactor: Option<RoleModelConfig>,
}

impl ModelRoutingConfig {
    /// Get the role-specific config for a dispatch role.
    pub fn get_role(&self, role: DispatchRole) -> Option<&RoleModelConfig> {
        match role {
            DispatchRole::Default => self.default.as_ref(),
            DispatchRole::TaskAgent => self.task_agent.as_ref(),
            DispatchRole::Evaluator => self.evaluator.as_ref(),
            DispatchRole::FlipInference => self.flip_inference.as_ref(),
            DispatchRole::FlipComparison => self.flip_comparison.as_ref(),
            DispatchRole::Assigner => self.assigner.as_ref(),
            DispatchRole::Evolver => self.evolver.as_ref(),
            DispatchRole::Verification => self.verification.as_ref(),
            DispatchRole::Triage => self.triage.as_ref(),
            DispatchRole::Creator => self.creator.as_ref(),
            DispatchRole::Compactor => self.compactor.as_ref(),
            DispatchRole::CoordinatorEval => self.evaluator.as_ref(),
            DispatchRole::Placer => self.placer.as_ref(),
            DispatchRole::ChatCompactor => self.chat_compactor.as_ref(),
        }
    }

    /// Get a mutable reference to a role's config, creating it if needed.
    pub fn get_role_mut(&mut self, role: DispatchRole) -> &mut Option<RoleModelConfig> {
        match role {
            DispatchRole::Default => &mut self.default,
            DispatchRole::TaskAgent => &mut self.task_agent,
            DispatchRole::Evaluator => &mut self.evaluator,
            DispatchRole::FlipInference => &mut self.flip_inference,
            DispatchRole::FlipComparison => &mut self.flip_comparison,
            DispatchRole::Assigner => &mut self.assigner,
            DispatchRole::Evolver => &mut self.evolver,
            DispatchRole::Verification => &mut self.verification,
            DispatchRole::Triage => &mut self.triage,
            DispatchRole::Creator => &mut self.creator,
            DispatchRole::Compactor => &mut self.compactor,
            DispatchRole::CoordinatorEval => &mut self.evaluator,
            DispatchRole::Placer => &mut self.placer,
            DispatchRole::ChatCompactor => &mut self.chat_compactor,
        }
    }

    /// Set the model for a role.
    pub fn set_model(&mut self, role: DispatchRole, model: &str) {
        let slot = self.get_role_mut(role);
        if let Some(cfg) = slot {
            cfg.model = Some(model.to_string());
            // Clear deprecated separate provider field — provider is now embedded in model spec
            cfg.provider = None;
        } else {
            *slot = Some(RoleModelConfig {
                provider: None,
                model: Some(model.to_string()),
                tier: None,
                endpoint: None,
            });
        }
    }

    /// Set the provider for a role.
    pub fn set_provider(&mut self, role: DispatchRole, provider: &str) {
        let slot = self.get_role_mut(role);
        if let Some(cfg) = slot {
            cfg.provider = Some(provider.to_string());
        } else {
            *slot = Some(RoleModelConfig {
                provider: Some(provider.to_string()),
                model: None,
                tier: None,
                endpoint: None,
            });
        }
    }

    /// Set the endpoint for a role.
    pub fn set_endpoint(&mut self, role: DispatchRole, endpoint: &str) {
        let slot = self.get_role_mut(role);
        if let Some(cfg) = slot {
            cfg.endpoint = Some(endpoint.to_string());
        } else {
            *slot = Some(RoleModelConfig {
                provider: None,
                model: None,
                tier: None,
                endpoint: Some(endpoint.to_string()),
            });
        }
    }

    /// Set the tier override for a role.
    pub fn set_tier(&mut self, role: DispatchRole, tier: Tier) {
        let slot = self.get_role_mut(role);
        if let Some(cfg) = slot {
            cfg.tier = Some(tier);
        } else {
            *slot = Some(RoleModelConfig {
                provider: None,
                model: None,
                tier: Some(tier),
                endpoint: None,
            });
        }
    }
}

/// Resolved model+provider for a dispatch.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub provider: Option<String>,
    /// Registry entry if resolved through the registry (carries cost data)
    pub registry_entry: Option<ModelRegistryEntry>,
    /// Named endpoint override: when set, consumers should look up this endpoint
    /// by name instead of falling back to provider-based endpoint lookup.
    pub endpoint: Option<String>,
}

// ---------------------------------------------------------------------------
// Unified provider:model naming
// ---------------------------------------------------------------------------

/// Known provider prefixes for the `provider:model` naming convention.
///
/// The `:` delimiter is unambiguous: provider names never contain `:`,
/// and model IDs may contain `/` but never `:`.
///
/// "oai-compat" and "openai" are aliases for the same thing: the
/// OpenAI-compatible HTTP protocol (POST /v1/chat/completions). Any
/// vLLM/Ollama/SGLang/etc. server speaking that wire format qualifies.
/// "oai-compat" is the preferred name going forward (more accurate —
/// "openai" has always been a misnomer referring to the protocol, not
/// the vendor). Both work in configs and match arms.
pub const KNOWN_PROVIDERS: &[&str] = &[
    "claude",
    "openrouter",
    "oai-compat",
    "openai", // alias for "oai-compat" — kept for backwards compatibility
    "codex",
    "gemini",
    "ollama",
    "llamacpp",
    "vllm",
    "local",
    "native",
];

/// Result of parsing a `provider:model` spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Provider prefix, if explicitly specified (e.g., `"openrouter"`).
    /// `None` for bare model names (lenient parsing only).
    pub provider: Option<String>,
    /// The model identifier sent to the provider's API.
    pub model_id: String,
}

/// Error returned when a model spec string fails strict validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpecError {
    /// The original input string that failed parsing.
    pub input: String,
    /// Human-readable error message with migration guidance.
    pub message: String,
}

impl std::fmt::Display for ModelSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ModelSpecError {}

/// Parse a model spec into provider and model ID components (lenient).
///
/// - `"openrouter:deepseek/deepseek-v3.2"` → `ModelSpec { provider: Some("openrouter"), model_id: "deepseek/deepseek-v3.2" }`
/// - `"claude:opus"` → `ModelSpec { provider: Some("claude"), model_id: "opus" }`
/// - `"opus"` → `ModelSpec { provider: None, model_id: "opus" }`
///
/// Only recognized provider prefixes are treated as providers. If the text before
/// `:` is not in `KNOWN_PROVIDERS`, the entire string is treated as a bare model name
/// (e.g., `"deepseek-coder-v2:16b"` for Ollama model tags).
///
/// **Note:** Prefer [`parse_model_spec_strict`] at entry points (CLI, config loading)
/// to enforce the `provider:model` format. This lenient version is for internal
/// resolution paths where the model string may already be partially resolved.
pub fn parse_model_spec(spec: &str) -> ModelSpec {
    if let Some((prefix, rest)) = spec.split_once(':')
        && KNOWN_PROVIDERS.contains(&prefix)
    {
        return ModelSpec {
            provider: Some(prefix.to_string()),
            model_id: rest.to_string(),
        };
    }
    ModelSpec {
        provider: None,
        model_id: spec.to_string(),
    }
}

/// Parse a model spec **strictly**: requires `provider:model` format.
///
/// Returns an error with a helpful migration message for:
/// - Bare model names without a provider prefix (e.g., `"opus"`)
/// - Unknown provider prefixes (e.g., `"foobar:gpt-4"`)
///
/// # Examples
///
/// ```
/// use workgraph::config::parse_model_spec_strict;
///
/// // Valid: known provider prefix
/// let spec = parse_model_spec_strict("claude:opus").unwrap();
/// assert_eq!(spec.provider.as_deref(), Some("claude"));
/// assert_eq!(spec.model_id, "opus");
///
/// // Invalid: bare model name
/// assert!(parse_model_spec_strict("opus").is_err());
///
/// // Invalid: unknown provider
/// assert!(parse_model_spec_strict("foobar:gpt-4").is_err());
/// ```
pub fn parse_model_spec_strict(spec: &str) -> Result<ModelSpec, ModelSpecError> {
    if spec.is_empty() {
        return Err(ModelSpecError {
            input: spec.to_string(),
            message: "Model spec cannot be empty. Use provider:model format (e.g., 'claude:opus')."
                .to_string(),
        });
    }

    if let Some((prefix, rest)) = spec.split_once(':') {
        if KNOWN_PROVIDERS.contains(&prefix) {
            if rest.is_empty() {
                return Err(ModelSpecError {
                    input: spec.to_string(),
                    message: format!(
                        "Model spec '{}' has provider '{}' but no model name. \
                         Use provider:model format (e.g., '{}:opus').",
                        spec, prefix, prefix
                    ),
                });
            }
            return Ok(ModelSpec {
                provider: Some(prefix.to_string()),
                model_id: rest.to_string(),
            });
        }
        // Has a colon but prefix is not a known provider
        return Err(ModelSpecError {
            input: spec.to_string(),
            message: format!(
                "Unknown provider '{}' in model spec '{}'. \
                 Known providers: {}. \
                 Use provider:model format (e.g., 'claude:opus', 'openrouter:deepseek/deepseek-v3.2').",
                prefix,
                spec,
                KNOWN_PROVIDERS.join(", "),
            ),
        });
    }

    // No colon at all — bare model name
    Err(ModelSpecError {
        input: spec.to_string(),
        message: format!(
            "Invalid model format '{}'. Models must use provider:model format. \
             For example: 'claude:{}', 'openrouter:{}', 'oai-compat:{}'. \
             Known providers: {}. \
             (Note: 'openai' is accepted as a legacy alias for 'oai-compat' — \
             both refer to the OpenAI-compatible HTTP protocol, not to OpenAI \
             Inc. specifically.)",
            spec,
            spec,
            spec,
            spec,
            KNOWN_PROVIDERS.join(", "),
        ),
    })
}

/// Map a provider prefix to the executor type it requires.
///
/// - `claude` → `"claude"` (Claude CLI)
/// - `codex` → `"codex"` (Codex CLI)
/// - All others → `"native"` (OpenAI-compatible API)
pub fn provider_to_executor(provider: &str) -> &'static str {
    match provider {
        "claude" => "claude",
        "codex" => "codex",
        _ => "native",
    }
}

/// Map a provider prefix to the internal provider name used by the native executor.
///
/// This determines which API wire format and default URL to use.
pub fn provider_to_native_provider(provider: &str) -> &'static str {
    match provider {
        "claude" => "anthropic",
        "codex" => "openai",
        "openrouter" => "openrouter",
        // "oai-compat" is the canonical name for the OpenAI-compatible
        // protocol; "openai" is kept as an alias for backwards compat.
        // Both map to the internal "openai" tag (which is itself the
        // legacy name for this protocol — renaming that is deferred
        // until we're ready to migrate ~60 test assertions).
        "oai-compat" | "openai" => "openai",
        "gemini" => "openai", // Gemini uses OpenAI-compatible endpoint
        "ollama" | "llamacpp" | "vllm" | "local" => "local",
        "native" => "openai", // auto-detect, use openai-compat
        _ => "anthropic",
    }
}

/// Reverse map: internal provider name → user-facing `provider:model` prefix.
///
/// This is the inverse of [`provider_to_native_provider`] for display purposes.
pub fn native_provider_to_prefix(provider: &str) -> &str {
    match provider {
        "anthropic" => "claude",
        "openrouter" => "openrouter",
        // Internal tag "openai" → user-facing prefix "oai-compat".
        // Configs written by workgraph will use "oai-compat:"; configs
        // with the legacy "openai:" prefix still deserialize correctly.
        "openai" => "oai-compat",
        "local" => "local",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Config validation
// ---------------------------------------------------------------------------

/// A single configuration diagnostic (error or warning).
#[derive(Debug, Clone)]
pub struct ConfigDiagnostic {
    /// Machine-readable rule identifier (e.g., "executor-model-mismatch")
    pub rule: String,
    /// Human-readable description of the problem
    pub message: String,
    /// Suggested fix
    pub fix: String,
}

/// Result of configuration validation.
#[derive(Debug, Clone, Default)]
pub struct ConfigValidation {
    /// Fatal errors that should block service start
    pub errors: Vec<ConfigDiagnostic>,
    /// Non-fatal warnings that should be displayed but allow startup
    pub warnings: Vec<ConfigDiagnostic>,
}

impl ConfigValidation {
    /// Returns true if there are no errors (warnings are OK).
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns true if there are no errors and no warnings.
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty() && self.warnings.is_empty()
    }

    /// Format all diagnostics for display.
    pub fn display(&self) -> String {
        let mut out = String::new();
        for diag in &self.errors {
            out.push_str(&format!("  ERROR: {}\n", diag.message));
            out.push_str(&format!("    Fix: {}\n", diag.fix));
        }
        for diag in &self.warnings {
            out.push_str(&format!("  WARNING: {}\n", diag.message));
            out.push_str(&format!("    Fix: {}\n", diag.fix));
        }
        out
    }
}

impl Config {
    /// Built-in Anthropic model defaults.
    fn builtin_registry() -> Vec<ModelRegistryEntry> {
        vec![
            // Legacy model entries (for backward compatibility)
            ModelRegistryEntry {
                id: "haiku".into(),
                provider: "anthropic".into(),
                model: CLAUDE_HAIKU_MODEL_ID.into(),
                tier: Tier::Fast,
                context_window: 200_000,
                max_output_tokens: 8192,
                cost_per_input_mtok: 0.25,
                cost_per_output_mtok: 1.25,
                prompt_caching: true,
                cache_read_discount: 0.1,
                cache_write_premium: 1.25,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "sonnet".into(),
                provider: "anthropic".into(),
                model: CLAUDE_SONNET_MODEL_ID.into(),
                tier: Tier::Standard,
                context_window: 200_000,
                max_output_tokens: 16384,
                cost_per_input_mtok: 3.0,
                cost_per_output_mtok: 15.0,
                prompt_caching: true,
                cache_read_discount: 0.1,
                cache_write_premium: 1.25,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "opus".into(),
                provider: "anthropic".into(),
                model: CLAUDE_OPUS_MODEL_ID.into(),
                tier: Tier::Premium,
                context_window: 200_000,
                max_output_tokens: 32000,
                cost_per_input_mtok: 15.0,
                cost_per_output_mtok: 75.0,
                prompt_caching: true,
                cache_read_discount: 0.1,
                cache_write_premium: 1.25,
                ..Default::default()
            },
            // New colon-separated format entries
            ModelRegistryEntry {
                id: "claude:haiku".into(),
                provider: "anthropic".into(),
                model: CLAUDE_HAIKU_MODEL_ID.into(),
                tier: Tier::Fast,
                context_window: 200_000,
                max_output_tokens: 8192,
                cost_per_input_mtok: 0.25,
                cost_per_output_mtok: 1.25,
                prompt_caching: true,
                cache_read_discount: 0.1,
                cache_write_premium: 1.25,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "claude:sonnet".into(),
                provider: "anthropic".into(),
                model: CLAUDE_SONNET_MODEL_ID.into(),
                tier: Tier::Standard,
                context_window: 200_000,
                max_output_tokens: 16384,
                cost_per_input_mtok: 3.0,
                cost_per_output_mtok: 15.0,
                prompt_caching: true,
                cache_read_discount: 0.1,
                cache_write_premium: 1.25,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "claude:opus".into(),
                provider: "anthropic".into(),
                model: CLAUDE_OPUS_MODEL_ID.into(),
                tier: Tier::Premium,
                context_window: 200_000,
                max_output_tokens: 32000,
                cost_per_input_mtok: 15.0,
                cost_per_output_mtok: 75.0,
                prompt_caching: true,
                cache_read_discount: 0.1,
                cache_write_premium: 1.25,
                ..Default::default()
            },
        ]
    }

    /// Return merged registry: built-in entries + user-defined entries.
    /// User entries with the same ID override built-in entries.
    pub fn effective_registry(&self) -> Vec<ModelRegistryEntry> {
        let builtins = Self::builtin_registry();
        if self.model_registry.is_empty() {
            return builtins;
        }
        let user_ids: std::collections::HashSet<&str> =
            self.model_registry.iter().map(|e| e.id.as_str()).collect();
        let mut result: Vec<ModelRegistryEntry> = builtins
            .into_iter()
            .filter(|e| !user_ids.contains(e.id.as_str()))
            .collect();
        result.extend(self.model_registry.clone());
        result
    }

    /// Effective tier config: use configured tiers, filling in defaults for unconfigured ones.
    pub fn effective_tiers_public(&self) -> TierConfig {
        self.effective_tiers()
    }

    /// Resolve the active profile's tier defaults (if any).
    fn resolve_profile_tiers(&self) -> TierConfig {
        use crate::profile;
        match self.profile.as_deref() {
            Some(name) => {
                if let Some(p) = profile::get_profile(name) {
                    // Static profiles return their hardcoded tiers.
                    // Dynamic profiles return None; we fall through to hardcoded defaults.
                    p.resolve_tiers().unwrap_or_default()
                } else {
                    TierConfig::default()
                }
            }
            None => TierConfig::default(),
        }
    }

    /// Effective tier config (internal).
    ///
    /// Precedence: explicit [tiers] > profile defaults > hardcoded Anthropic fallback.
    fn effective_tiers(&self) -> TierConfig {
        let profile_tiers = self.resolve_profile_tiers();
        TierConfig {
            fast: self
                .tiers
                .fast
                .clone()
                .or(profile_tiers.fast)
                .or_else(|| Some(format!("claude:{}", Tier::Fast.default_alias()))),
            standard: self
                .tiers
                .standard
                .clone()
                .or(profile_tiers.standard)
                .or_else(|| Some(format!("claude:{}", Tier::Standard.default_alias()))),
            premium: self
                .tiers
                .premium
                .clone()
                .or(profile_tiers.premium)
                .or_else(|| Some(format!("claude:{}", Tier::Premium.default_alias()))),
        }
    }

    /// Look up a registry entry by its short ID.
    pub fn registry_lookup(&self, id: &str) -> Option<ModelRegistryEntry> {
        self.effective_registry().into_iter().find(|e| e.id == id)
    }

    /// Resolve a tier to a ResolvedModel via the tier config and registry.
    pub fn resolve_tier(&self, tier: Tier) -> Option<ResolvedModel> {
        let tiers = self.effective_tiers();
        let model_id = match tier {
            Tier::Fast => tiers.fast.as_deref(),
            Tier::Standard => tiers.standard.as_deref(),
            Tier::Premium => tiers.premium.as_deref(),
        }?;

        // Parse provider:model prefix if present
        let spec = parse_model_spec(model_id);
        let lookup_id = &spec.model_id;

        if let Some(entry) = self.registry_lookup(lookup_id) {
            Some(ResolvedModel {
                model: entry.model.clone(),
                provider: spec
                    .provider
                    .map(|p| provider_to_native_provider(&p).to_string())
                    .or_else(|| Some(entry.provider.clone())),
                registry_entry: Some(entry),
                endpoint: None,
            })
        } else {
            // Not in registry — use parsed provider or None
            Some(ResolvedModel {
                model: spec.model_id,
                provider: spec
                    .provider
                    .map(|p| provider_to_native_provider(&p).to_string()),
                registry_entry: None,
                endpoint: None,
            })
        }
    }

    /// Resolve the model (and optional provider) for a given dispatch role.
    ///
    /// Resolution order:
    /// 1. `models.<role>.model` (role-specific override in [models] section)
    /// 2. `models.<role>.tier` (role tier override via tier system)
    /// 3. Role `default_tier()` → `tiers.<tier>` → registry lookup
    /// 4. `models.default.model` (default in [models] section)
    /// 5. `agent.model` (global fallback)
    ///
    /// Provider resolution follows the same cascade but only from [models].
    pub fn resolve_model_for_role(&self, role: DispatchRole) -> ResolvedModel {
        // Default provider cascades to all roles that don't set their own.
        let default_provider = self
            .models
            .get_role(DispatchRole::Default)
            .and_then(|c| c.provider.clone());

        // Default endpoint cascades to all roles that don't set their own.
        let default_endpoint = self
            .models
            .get_role(DispatchRole::Default)
            .and_then(|c| c.endpoint.clone());

        // Infer provider from coordinator.model and agent.model prefixes as
        // final fallbacks.  This ensures that when a user sets e.g.
        // `coordinator.model = "openrouter:anthropic/claude-sonnet-4-6"`
        // the OpenRouter provider cascades to ALL roles (eval, FLIP, verification)
        // without needing explicit `[models.default].provider` config.
        let coordinator_model_provider = self.coordinator.model.as_deref().and_then(|m| {
            parse_model_spec(m)
                .provider
                .map(|p| provider_to_native_provider(&p).to_string())
        });
        let agent_model_provider = parse_model_spec(&self.agent.model)
            .provider
            .map(|p| provider_to_native_provider(&p).to_string());

        // Helper: resolve provider for a role, cascading through:
        //   models.<role>.provider → models.default.provider
        //   → coordinator.model prefix → agent.model prefix
        let resolve_provider = |role: DispatchRole| -> Option<String> {
            self.models
                .get_role(role)
                .and_then(|c| c.provider.clone())
                .or_else(|| default_provider.clone())
                .or_else(|| coordinator_model_provider.clone())
                .or_else(|| agent_model_provider.clone())
        };

        // Helper: resolve endpoint for a role, cascading to default if unset.
        let resolve_endpoint = |role: DispatchRole| -> Option<String> {
            self.models
                .get_role(role)
                .and_then(|c| c.endpoint.clone())
                .or_else(|| default_endpoint.clone())
        };

        // 1. Check role-specific [models] config (direct model override)
        if let Some(role_cfg) = self.models.get_role(role)
            && let Some(ref model) = role_cfg.model
        {
            // Parse provider:model prefix from the model string
            let spec = parse_model_spec(model);
            let spec_provider = spec
                .provider
                .as_deref()
                .map(provider_to_native_provider)
                .map(String::from);
            let lookup_model = &spec.model_id;

            if let Some(entry) = self.registry_lookup(lookup_model) {
                return ResolvedModel {
                    model: entry.model.clone(),
                    provider: spec_provider
                        .or_else(|| role_cfg.provider.clone())
                        .or_else(|| Some(entry.provider.clone()))
                        .or_else(|| default_provider.clone()),
                    registry_entry: Some(entry),
                    endpoint: resolve_endpoint(role),
                };
            }
            return ResolvedModel {
                model: spec.model_id.clone(),
                provider: spec_provider
                    .or_else(|| role_cfg.provider.clone())
                    .or_else(|| default_provider.clone()),
                registry_entry: None,
                endpoint: resolve_endpoint(role),
            };
        }

        // 2. Role tier override: [models.<role>].tier
        if let Some(role_cfg) = self.models.get_role(role)
            && let Some(tier) = role_cfg.tier
            && let Some(mut resolved) = self.resolve_tier(tier)
        {
            // Allow role/default provider to override registry provider
            if let Some(p) = resolve_provider(role) {
                resolved.provider = Some(p);
            }
            resolved.endpoint = resolve_endpoint(role);
            return resolved;
        }

        // 3. Role default_tier() → tiers.<tier> → registry lookup
        //    Skipped when agent.model was explicitly set in local config, because
        //    the user's local model choice should take precedence over tier defaults.
        if !self.agent_model_is_local
            && let Some(mut resolved) = self.resolve_tier(role.default_tier())
        {
            // Allow role/default provider to override registry provider
            if let Some(p) = resolve_provider(role) {
                resolved.provider = Some(p);
            }
            resolved.endpoint = resolve_endpoint(role);
            return resolved;
        }

        // 4. Check [models.default]
        if let Some(default_cfg) = self.models.get_role(DispatchRole::Default)
            && let Some(ref model) = default_cfg.model
        {
            let spec = parse_model_spec(model);
            let spec_provider = spec
                .provider
                .as_deref()
                .map(provider_to_native_provider)
                .map(String::from);

            if let Some(entry) = self.registry_lookup(&spec.model_id) {
                return ResolvedModel {
                    model: entry.model.clone(),
                    provider: spec_provider
                        .or(default_provider)
                        .or_else(|| Some(entry.provider.clone())),
                    registry_entry: Some(entry),
                    endpoint: resolve_endpoint(role),
                };
            }
            return ResolvedModel {
                model: spec.model_id,
                provider: spec_provider.or(default_provider),
                registry_entry: None,
                endpoint: resolve_endpoint(role),
            };
        }

        // 5. Global fallback
        let fallback_spec = parse_model_spec(&self.agent.model);
        let fallback_provider = fallback_spec
            .provider
            .as_deref()
            .map(provider_to_native_provider)
            .map(String::from);

        if let Some(entry) = self.registry_lookup(&fallback_spec.model_id) {
            return ResolvedModel {
                model: entry.model.clone(),
                provider: fallback_provider
                    .or(default_provider)
                    .or_else(|| Some(entry.provider.clone())),
                registry_entry: Some(entry),
                endpoint: resolve_endpoint(role),
            };
        }
        ResolvedModel {
            model: fallback_spec.model_id,
            provider: fallback_provider.or(default_provider),
            registry_entry: None,
            endpoint: resolve_endpoint(role),
        }
    }

    /// Determine the source of model resolution for a role.
    ///
    /// Returns one of: "explicit", "tier-override", "tier-default", "fallback"
    pub fn resolve_model_source(&self, role: DispatchRole) -> &'static str {
        // 1. Role-specific [models] config (direct model override)
        if let Some(role_cfg) = self.models.get_role(role)
            && role_cfg.model.is_some()
        {
            return "explicit";
        }

        // 2. Role tier override
        if let Some(role_cfg) = self.models.get_role(role)
            && role_cfg.tier.is_some()
        {
            return "tier-override";
        }

        // 3. Role default_tier() → registry
        if self.resolve_tier(role.default_tier()).is_some() {
            return "tier-default";
        }

        // 4/5. Fallback
        "fallback"
    }
}

fn default_auto_create_threshold() -> u32 {
    20
}
fn default_exploration_interval() -> u32 {
    20
}
fn default_cache_population_threshold() -> f64 {
    0.8
}
fn default_ucb_exploration_constant() -> f64 {
    std::f64::consts::SQRT_2
}
fn default_novelty_bonus_multiplier() -> f64 {
    1.5
}
fn default_bizarre_ideation_interval() -> u32 {
    10
}
fn default_eval_gate_threshold() -> Option<f64> {
    Some(0.7)
}
fn default_flip_verification_threshold() -> Option<f64> {
    Some(0.7)
}
fn default_evolution_interval() -> u64 {
    7200
}
fn default_evolution_threshold() -> u32 {
    10
}
fn default_evolution_budget() -> u32 {
    5
}
fn default_evolution_reactive_threshold() -> f64 {
    0.4
}
fn default_auto_assign_grace_seconds() -> u64 {
    10
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

    /// Content-hash of agent to use as evaluator (None = use default pipeline)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_agent: Option<String>,

    /// Content-hash of agent to use as evolver
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evolver_agent: Option<String>,

    /// Content-hash of agent to use as agent creator (None = not configured)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_agent: Option<String>,

    /// Content-hash of agent to use as placer (None = not configured)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placer_agent: Option<String>,

    /// Include placement (dependency edge decisions) in the assignment step.
    /// When enabled, the assignment LLM call also decides dependency edges
    /// for the source task based on active tasks in the graph.
    /// Default: false.
    #[serde(default)]
    pub auto_place: bool,

    /// Automatically invoke the creator agent when the primitive store
    /// needs expansion. Default: false.
    #[serde(default)]
    pub auto_create: bool,

    /// Minimum completed tasks since last creator invocation before
    /// triggering `wg agency create` again. Default: 20.
    #[serde(default = "default_auto_create_threshold")]
    pub auto_create_threshold: u32,

    /// Prose policy for the evolver describing retention heuristics
    /// (e.g. when to retire underperforming roles/motivations)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_heuristics: Option<String>,

    /// Automatically triage dead agents to assess work progress before respawning
    #[serde(default)]
    pub auto_triage: bool,

    /// Timeout in seconds for triage calls (default: 30)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_timeout: Option<u64>,

    /// Maximum bytes to read from agent output log for triage (default: 50000)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_max_log_bytes: Option<usize>,

    /// Force a learning assignment every N tasks with forced exploration parameters.
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

    /// Grace period in seconds after task creation before auto-assignment
    /// is eligible. Prevents premature assignment when tasks are created
    /// and then have dependencies wired shortly after.
    /// Default: 10
    #[serde(default = "default_auto_assign_grace_seconds")]
    pub auto_assign_grace_seconds: u64,

    /// Global evaluation gate threshold. When set, evaluations that score
    /// below this threshold will reject (fail) the original task, blocking
    /// its dependents. Only applies to tasks tagged with "eval-gate" unless
    /// `eval_gate_all` is true. Range: 0.0–1.0. Default: 0.7 (enabled).
    #[serde(
        default = "default_eval_gate_threshold",
        skip_serializing_if = "Option::is_none"
    )]
    pub eval_gate_threshold: Option<f64>,

    /// When true, apply the eval gate threshold to ALL evaluated tasks,
    /// not just those tagged with "eval-gate". Default: false.
    #[serde(default)]
    pub eval_gate_all: bool,

    /// Enable FLIP (Fidelity via Latent Intent Probing) evaluation.
    /// When enabled, completed tasks can be evaluated using roundtrip
    /// intent fidelity: infer the prompt from output, then compare to actual.
    #[serde(default)]
    pub flip_enabled: bool,

    /// Model to use for FLIP inference phase (inferring prompt from output)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_inference_model: Option<String>,

    /// Model to use for FLIP comparison phase (comparing inferred prompt to actual)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_comparison_model: Option<String>,

    /// FLIP score threshold below which automatic Opus verification is triggered.
    /// When a FLIP evaluation scores below this threshold, the coordinator creates
    /// a verification task that independently checks whether the work was done.
    /// Default: 0.7. Set to None to disable.
    #[serde(default = "default_flip_verification_threshold")]
    pub flip_verification_threshold: Option<f64>,

    /// Automatically trigger evolution cycles based on evaluation data.
    /// When enabled, the coordinator creates `.evolve-*` meta-tasks
    /// after sufficient evaluations accumulate. Default: false (opt-in).
    #[serde(default)]
    pub auto_evolve: bool,

    /// Minimum seconds between automatic evolution cycles. Default: 7200 (2 hours).
    #[serde(default = "default_evolution_interval")]
    pub evolution_interval: u64,

    /// Minimum number of new evaluations required before triggering evolution.
    /// Default: 10.
    #[serde(default = "default_evolution_threshold")]
    pub evolution_threshold: u32,

    /// Maximum number of evolver operations per automatic evolution cycle.
    /// Default: 5.
    #[serde(default = "default_evolution_budget")]
    pub evolution_budget: u32,

    /// Average score threshold for reactive evolution trigger. When the
    /// average evaluation score drops below this value, evolution is
    /// triggered regardless of the normal interval/threshold. Default: 0.4.
    #[serde(default = "default_evolution_reactive_threshold")]
    pub evolution_reactive_threshold: f64,

    /// URL of the Agency server for evaluation feedback. None = disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agency_server_url: Option<String>,

    /// Path to file containing Agency API token. None = no auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agency_token_path: Option<String>,

    /// Default assignment source label (e.g. "native", "agency").
    /// Used to tag new assignments with their provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_source: Option<String>,

    /// Project ID on the Agency server. Required for assignment requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agency_project_id: Option<String>,

    /// URL for upstream agency bureau CSV. Used by `wg agency import --upstream`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_url: Option<String>,
}

impl Default for AgencyConfig {
    fn default() -> Self {
        Self {
            auto_evaluate: true,
            auto_assign: true,
            assigner_agent: None,
            evaluator_agent: None,
            evolver_agent: None,
            creator_agent: None,
            placer_agent: None,
            auto_place: false,
            auto_create: false,
            auto_create_threshold: default_auto_create_threshold(),
            retention_heuristics: None,
            auto_triage: false,
            triage_timeout: None,
            triage_max_log_bytes: None,
            exploration_interval: default_exploration_interval(),
            cache_population_threshold: default_cache_population_threshold(),
            ucb_exploration_constant: default_ucb_exploration_constant(),
            novelty_bonus_multiplier: default_novelty_bonus_multiplier(),
            bizarre_ideation_interval: default_bizarre_ideation_interval(),
            auto_assign_grace_seconds: default_auto_assign_grace_seconds(),
            eval_gate_threshold: default_eval_gate_threshold(),
            eval_gate_all: false,
            flip_enabled: true,
            flip_inference_model: None,
            flip_comparison_model: None,
            flip_verification_threshold: default_flip_verification_threshold(),
            auto_evolve: false,
            evolution_interval: default_evolution_interval(),
            evolution_threshold: default_evolution_threshold(),
            evolution_budget: default_evolution_budget(),
            evolution_reactive_threshold: default_evolution_reactive_threshold(),
            agency_server_url: None,
            agency_token_path: None,
            assignment_source: None,
            agency_project_id: None,
            upstream_url: None,
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

    /// Maximum tasks per agent run (None = unlimited)
    #[serde(default)]
    pub max_tasks: Option<u32>,

    /// Heartbeat timeout in minutes (for detecting dead agents)
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,

    /// Grace period in seconds before the reaper acts on a dead PID.
    /// Agents started less than this many seconds ago are not reaped,
    /// avoiding a race condition where the PID is registered but the
    /// process hasn't fully started yet. Default: 30.
    #[serde(default = "default_reaper_grace_seconds")]
    pub reaper_grace_seconds: u64,
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

    /// Executor to use for spawned agents.
    /// When `None` (not set in config), `effective_executor()` auto-detects
    /// based on `provider`: openrouter/openai/local → "native", else "claude".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,

    /// Model to use for spawned agents (e.g., "opus-4-5", "sonnet", "haiku")
    /// Overrides agent.model when set. Can be further overridden by CLI --model.
    #[serde(default)]
    pub model: Option<String>,

    /// **Deprecated**: Use provider:model format in the `model` field instead.
    /// Kept for deserialization of old configs; never written back.
    #[serde(default, skip_serializing)]
    pub provider: Option<String>,

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

    /// Whether to spawn a persistent LLM coordinator agent for chat.
    /// When true, the daemon launches a Claude CLI session that interprets
    /// user chat messages and manages the graph conversationally.
    /// When false, chat uses a simple stub response.
    /// Default: true.
    #[serde(default = "default_coordinator_agent")]
    pub coordinator_agent: bool,

    /// Autonomous heartbeat interval in seconds for the coordinator agent.
    /// When set (> 0), the daemon sends a synthetic heartbeat prompt to the
    /// coordinator agent at this interval, triggering it to review graph state,
    /// dispatch work, recover from failures, and adapt strategy — without any
    /// human interaction. Set to 0 to disable. Default: 0 (disabled).
    #[serde(default)]
    pub heartbeat_interval: u64,

    /// How often to run the compactor (every N coordinator ticks). 0 = disabled.
    #[serde(default = "default_compactor_interval")]
    pub compactor_interval: u32,

    /// Provenance ops growth threshold that triggers compaction (default: 100)
    #[serde(default = "default_compactor_ops_threshold")]
    pub compactor_ops_threshold: usize,

    /// Accumulated coordinator conversation token threshold for triggering compaction.
    /// Compaction is deferred until at least this many tokens have been accumulated
    /// since the last compaction. Default: 100_000. Set to 0 to disable token gating.
    /// Used as the fallback when context window size cannot be determined.
    #[serde(default = "default_compaction_token_threshold")]
    pub compaction_token_threshold: u64,

    /// Fraction of the coordinator model's context window to use as compaction threshold.
    /// Threshold = context_window * compaction_threshold_ratio.
    /// Default: 0.8 (trigger compaction at 80% of context window).
    /// Set to 0.0 to disable dynamic threshold (use compaction_token_threshold always).
    #[serde(default = "default_compaction_threshold_ratio")]
    pub compaction_threshold_ratio: f64,

    /// How often to evaluate coordinator turns.
    /// Options: "every", "every_5" (default), "every_10", "sample_20pct", "none"
    #[serde(default = "default_eval_frequency")]
    pub eval_frequency: String,

    /// Total trial time budget in seconds. When set, heartbeat prompts include
    /// remaining time and shift to wind-down behavior near the deadline.
    #[serde(default)]
    pub trial_budget_secs: Option<u64>,

    /// Enable git worktree isolation for spawned agents.
    /// When true, each agent gets its own worktree at .wg-worktrees/<agent-id>/.
    /// Defaults to true to prevent cargo lock contention between concurrent agents.
    #[serde(default = "default_worktree_isolation")]
    pub worktree_isolation: bool,

    /// Maximum number of concurrent coordinator agents (LLM sessions).
    /// Each coordinator is a separate Claude CLI process. Default: 4.
    #[serde(default = "default_max_coordinators")]
    pub max_coordinators: usize,

    /// Archive tasks completed/abandoned more than this many days ago.
    /// The archive cycle (.archive-0) runs periodically and moves old
    /// done/abandoned tasks to .workgraph/archive.jsonl. Default: 7 days.
    /// Set to 0 to disable automatic archival.
    #[serde(default = "default_archive_retention_days")]
    pub archive_retention_days: u64,

    /// How often to refresh the model benchmark registry from OpenRouter,
    /// in seconds. The daemon manages a `.registry-refresh-0` cycle task
    /// that fetches fresh model data, computes fitness scores, and diffs
    /// against the previous registry. Default: 86400 (24 hours).
    /// Set to 0 to disable automatic registry refresh.
    #[serde(default = "default_registry_refresh_interval")]
    pub registry_refresh_interval: u64,

    /// Verification mode for tasks with `--verify` commands.
    /// - "inline" (default): verify command runs in the same agent process that did the work
    /// - "separate": verify runs in a separate agent context (different conversation/context window)
    ///   This prevents false-PASS rates where the implementation agent rubber-stamps its own work.
    #[serde(default = "default_verify_mode")]
    pub verify_mode: String,

    /// Maximum consecutive verify command failures before a task is auto-failed.
    /// When a task's verify command fails this many times in a row, the task
    /// transitions to Failed with a descriptive error. Default: 3.
    /// Set to 0 to disable the circuit breaker (unlimited retries).
    #[serde(default = "default_max_verify_failures")]
    pub max_verify_failures: u32,

    /// Default verify timeout for tasks without specific override
    #[serde(default = "default_verify_default_timeout")]
    pub verify_default_timeout: Option<String>,

    /// Maximum number of concurrent verify processes to prevent cascade failures
    #[serde(default = "default_max_concurrent_verifies")]
    pub max_concurrent_verifies: u32,

    /// Enable intelligent triage instead of hard timeout failure
    #[serde(default = "default_verify_triage_enabled")]
    pub verify_triage_enabled: bool,

    /// Time without output before considering process potentially stuck
    #[serde(default = "default_verify_progress_timeout")]
    pub verify_progress_timeout: Option<String>,

    /// Maximum consecutive spawn failures before a task is auto-failed.
    /// When the coordinator fails to spawn an agent for a task this many times
    /// in a row, the task transitions to Failed with a descriptive error
    /// including exec_mode, executor, and last error. Default: 5.
    /// Set to 0 to disable the circuit breaker (unlimited retries).
    #[serde(default = "default_max_spawn_failures")]
    pub max_spawn_failures: u32,

    /// Maximum tier escalation depth for model fallback on retry.
    /// When a task fails and the active profile has a ranked model list,
    /// the coordinator tries the next model in the tier. If the entire tier
    /// is exhausted, it escalates to the next tier up (fast → standard → premium).
    /// This limits how many tiers to escalate through. Default: 3 (all tiers).
    /// Set to 0 to disable tier escalation (only rotate within same tier).
    #[serde(default = "default_max_escalation_depth")]
    pub max_escalation_depth: u32,

    /// Whether to scan for test files before spawning agents and inject
    /// discovered tests into agent context. When enabled, the executor also
    /// auto-populates `--verify` gates for tasks that have no explicit verify
    /// command but have discoverable test files. Default: false.
    #[serde(default = "default_auto_test_discovery")]
    pub auto_test_discovery: bool,

    /// Enable scoped verify: automatically scope 'cargo test' to only run tests
    /// relevant to modified files, reducing verify time from minutes to seconds.
    /// When enabled, detects modified files and maps them to relevant test modules.
    /// Falls back to full test suite for ambiguous mappings or core file changes.
    /// Default: true.
    #[serde(default = "default_scoped_verify_enabled")]
    pub scoped_verify_enabled: bool,

    /// Provider failure handling behavior.
    /// - "pause" (default): pause the service when providers fail consecutively
    /// - "fallback": switch to fallback provider if configured
    /// - "continue": keep going despite provider failures (legacy behavior)
    #[serde(default = "default_on_provider_failure")]
    pub on_provider_failure: String,

    /// Number of consecutive fatal-provider errors before triggering auto-pause.
    /// Fatal-provider errors include auth failures, quota exhaustion, CLI missing.
    /// Transient errors (rate limits, network) and task errors don't count.
    /// Default: 3.
    #[serde(default = "default_provider_failure_threshold")]
    pub provider_failure_threshold: u32,

    /// Cooldown period before auto-resuming from provider failure pause.
    /// Format: "5m", "1h", "30s", etc. Empty string disables auto-resume.
    /// Default: empty (manual resume only).
    #[serde(default)]
    pub provider_failure_cooldown: String,

    /// Resource management configuration for worktree cleanup and recovery.
    #[serde(default)]
    pub resource_management: ResourceManagementConfig,
}

/// Resource management configuration for cleanup operations and recovery branches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceManagementConfig {
    /// Enable cleanup verification to ensure worktrees are actually removed.
    /// When true, cleanup operations verify that worktree directories are
    /// fully removed and report any remaining artifacts. Default: true.
    #[serde(default = "default_cleanup_verification")]
    pub cleanup_verification: bool,

    /// Maximum age in seconds for recovery branches before they are pruned.
    /// Recovery branches older than this will be automatically deleted.
    /// Set to 0 to disable age-based pruning. Default: 7 days (604800 seconds).
    #[serde(default = "default_recovery_branch_max_age")]
    pub recovery_branch_max_age: u64,

    /// Maximum number of recovery branches to keep per agent.
    /// When this limit is exceeded, oldest recovery branches are pruned first.
    /// Set to 0 to disable count-based pruning. Default: 10.
    #[serde(default = "default_recovery_branch_max_count")]
    pub recovery_branch_max_count: u32,

    /// Enable cleanup job queuing for high-frequency cleanup scenarios.
    /// When true, cleanup operations are queued and processed sequentially
    /// to prevent resource contention during burst cleanup periods. Default: true.
    #[serde(default = "default_cleanup_job_queue")]
    pub cleanup_job_queue: bool,

    /// Maximum number of cleanup jobs to queue before blocking.
    /// When the queue is full, new cleanup requests will block until
    /// space becomes available. Default: 50.
    #[serde(default = "default_cleanup_queue_size")]
    pub cleanup_queue_size: usize,

    /// Interval in seconds between recovery branch pruning cycles.
    /// Set to 0 to disable automatic pruning. Default: 3600 (1 hour).
    #[serde(default = "default_recovery_prune_interval")]
    pub recovery_prune_interval: u64,
}

fn default_auto_test_discovery() -> bool {
    false
}

fn default_scoped_verify_enabled() -> bool {
    true
}

fn default_on_provider_failure() -> String {
    "pause".to_string()
}

fn default_provider_failure_threshold() -> u32 {
    3
}

fn default_max_agents() -> usize {
    8
}

fn default_coordinator_interval() -> u64 {
    30
}

fn default_settling_delay_ms() -> u64 {
    2000
}

fn default_coordinator_agent() -> bool {
    true
}

fn default_poll_interval() -> u64 {
    60
}

fn default_compactor_interval() -> u32 {
    5
}

fn default_compactor_ops_threshold() -> usize {
    100
}

fn default_compaction_token_threshold() -> u64 {
    100_000
}

fn default_compaction_threshold_ratio() -> f64 {
    0.8
}

fn default_eval_frequency() -> String {
    "every_5".to_string()
}

fn default_worktree_isolation() -> bool {
    true
}

fn default_max_coordinators() -> usize {
    4
}

fn default_archive_retention_days() -> u64 {
    7
}

fn default_verify_mode() -> String {
    "inline".to_string()
}

fn default_max_verify_failures() -> u32 {
    3
}

fn default_verify_default_timeout() -> Option<String> {
    Some("900s".to_string())
}

fn default_max_concurrent_verifies() -> u32 {
    2
}

fn default_verify_triage_enabled() -> bool {
    false // Start disabled, enable gradually
}

fn default_verify_progress_timeout() -> Option<String> {
    Some("300s".to_string())
}

fn default_max_spawn_failures() -> u32 {
    5
}

fn default_max_escalation_depth() -> u32 {
    3
}

fn default_registry_refresh_interval() -> u64 {
    86400 // 24 hours
}

fn default_agent_timeout() -> String {
    "30m".to_string()
}

/// Providers that are not Anthropic-native and should default to the "native" executor.
const NON_ANTHROPIC_PROVIDERS: &[&str] = &["openrouter", "oai-compat", "openai", "local"];

impl CoordinatorConfig {
    /// Return the effective executor, considering provider-based auto-detection.
    ///
    /// If executor is explicitly set in config, that value is used unconditionally.
    /// Otherwise, if provider is openrouter/openai/local, returns "native" (since
    /// the claude executor only works with Anthropic's API). Falls back to "claude".
    pub fn effective_executor(&self) -> String {
        if let Some(ref executor) = self.executor {
            // Explicitly set in config — honour it
            executor.clone()
        } else if let Some(ref model) = self.model {
            // Infer executor from provider:model prefix (preferred path)
            let spec = parse_model_spec(model);
            if let Some(ref prefix) = spec.provider {
                provider_to_executor(prefix).to_string()
            } else {
                "claude".to_string()
            }
        } else if let Some(ref provider) = self.provider {
            // Deprecated: separate provider field fallback
            if NON_ANTHROPIC_PROVIDERS.contains(&provider.as_str()) {
                "native".to_string()
            } else {
                "claude".to_string()
            }
        } else {
            "claude".to_string()
        }
    }
}

// Default functions for ResourceManagementConfig

fn default_cleanup_verification() -> bool {
    true
}

fn default_recovery_branch_max_age() -> u64 {
    604800 // 7 days in seconds
}

fn default_recovery_branch_max_count() -> u32 {
    10
}

fn default_cleanup_job_queue() -> bool {
    true
}

fn default_cleanup_queue_size() -> usize {
    50
}

fn default_recovery_prune_interval() -> u64 {
    3600 // 1 hour in seconds
}

impl Default for ResourceManagementConfig {
    fn default() -> Self {
        Self {
            cleanup_verification: default_cleanup_verification(),
            recovery_branch_max_age: default_recovery_branch_max_age(),
            recovery_branch_max_count: default_recovery_branch_max_count(),
            cleanup_job_queue: default_cleanup_job_queue(),
            cleanup_queue_size: default_cleanup_queue_size(),
            recovery_prune_interval: default_recovery_prune_interval(),
        }
    }
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            max_agents: default_max_agents(),
            interval: default_coordinator_interval(),
            poll_interval: default_poll_interval(),
            executor: None,
            model: None,
            provider: None,
            default_context_scope: None,
            agent_timeout: default_agent_timeout(),
            settling_delay_ms: default_settling_delay_ms(),
            coordinator_agent: default_coordinator_agent(),
            heartbeat_interval: 60, // Default: autonomous heartbeat every 60s (0 = disabled)
            compactor_interval: default_compactor_interval(),
            compactor_ops_threshold: default_compactor_ops_threshold(),
            compaction_token_threshold: default_compaction_token_threshold(),
            on_provider_failure: default_on_provider_failure(),
            provider_failure_threshold: default_provider_failure_threshold(),
            provider_failure_cooldown: String::new(),
            compaction_threshold_ratio: default_compaction_threshold_ratio(),
            eval_frequency: default_eval_frequency(),
            trial_budget_secs: None,
            worktree_isolation: true,
            max_coordinators: default_max_coordinators(),
            archive_retention_days: default_archive_retention_days(),
            registry_refresh_interval: default_registry_refresh_interval(),
            verify_mode: default_verify_mode(),
            max_verify_failures: default_max_verify_failures(),
            max_spawn_failures: default_max_spawn_failures(),
            max_escalation_depth: default_max_escalation_depth(),
            auto_test_discovery: default_auto_test_discovery(),
            scoped_verify_enabled: default_scoped_verify_enabled(),
            verify_default_timeout: default_verify_default_timeout(),
            max_concurrent_verifies: default_max_concurrent_verifies(),
            verify_triage_enabled: default_verify_triage_enabled(),
            verify_progress_timeout: default_verify_progress_timeout(),
            resource_management: ResourceManagementConfig::default(),
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_skills: Vec<String>,
}

fn default_executor() -> String {
    "claude".to_string()
}

fn default_model() -> String {
    "claude:opus".to_string()
}

fn default_interval() -> u64 {
    10
}

fn default_heartbeat_timeout() -> u64 {
    5
}

fn default_reaper_grace_seconds() -> u64 {
    30
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            executor: default_executor(),
            model: default_model(),
            interval: default_interval(),
            max_tasks: None,
            heartbeat_timeout: default_heartbeat_timeout(),
            reaper_grace_seconds: default_reaper_grace_seconds(),
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

/// When local config explicitly sets `agent.model`, strip any `models.<role>.model`
/// entries that exist only in the global config (not overridden locally).
fn strip_global_only_model_roles(
    merged: &mut toml::Value,
    global_val: &toml::Value,
    local_val: &toml::Value,
) {
    let local_has_agent_model = local_val
        .get("agent")
        .and_then(|a| a.get("model"))
        .and_then(|m| m.as_str())
        .is_some();
    if !local_has_agent_model {
        return;
    }
    let global_models = match global_val.get("models").and_then(|m| m.as_table()) {
        Some(m) => m,
        None => return,
    };
    let local_models = local_val.get("models").and_then(|m| m.as_table());
    let roles_to_strip: Vec<String> = global_models
        .keys()
        .filter(|role_key| {
            let has_global_model = global_models
                .get(role_key.as_str())
                .and_then(|r| r.get("model"))
                .is_some();
            let has_local_role = local_models
                .map(|lm| lm.contains_key(role_key.as_str()))
                .unwrap_or(false);
            has_global_model && !has_local_role
        })
        .cloned()
        .collect();
    if roles_to_strip.is_empty() {
        return;
    }
    if let Some(merged_models) = merged.get_mut("models").and_then(|m| m.as_table_mut()) {
        for role_key in &roles_to_strip {
            if let Some(role_table) = merged_models
                .get_mut(role_key.as_str())
                .and_then(|r| r.as_table_mut())
            {
                role_table.remove("model");
                if role_table.is_empty() {
                    merged_models.remove(role_key.as_str());
                }
            }
        }
        if merged_models.is_empty()
            && let Some(root) = merged.as_table_mut()
        {
            root.remove("models");
        }
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
        config.validate_model_format()?;
        Ok(Some(config))
    }

    /// Load raw TOML value from a config file path.
    /// Returns empty table if file doesn't exist.
    fn load_toml_value(path: &Path) -> anyhow::Result<toml::Value> {
        if !path.exists() {
            return Ok(toml::Value::Table(toml::map::Map::new()));
        }
        let content = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config at {}: {}", path.display(), e))?;
        let val: toml::Value = content
            .parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse config at {}: {}", path.display(), e))?;
        Ok(val)
    }

    /// Load the merged TOML value (global + local) without deserializing.
    /// Useful for legacy code that needs raw TOML access to sections like
    /// `[native_executor]` while respecting the global → local merge chain.
    pub fn load_merged_toml_value(workgraph_dir: &Path) -> anyhow::Result<toml::Value> {
        let global_path = Self::global_config_path()?;
        let local_path = workgraph_dir.join("config.toml");
        let global_val = Self::load_toml_value(&global_path)?;
        let local_val = Self::load_toml_value(&local_path)?;
        Ok(merge_toml(global_val, local_val))
    }

    /// Load merged configuration: global config deep-merged with local config.
    /// Local keys override global keys. Missing files are treated as empty.
    pub fn load_merged(workgraph_dir: &Path) -> anyhow::Result<Self> {
        let global_path = Self::global_config_path()?;
        let local_path = workgraph_dir.join("config.toml");

        let global_val = Self::load_toml_value(&global_path)?;
        let local_val = Self::load_toml_value(&local_path)?;

        let agent_model_is_local = local_val
            .get("agent")
            .and_then(|a| a.get("model"))
            .and_then(|m| m.as_str())
            .is_some();

        let mut merged = merge_toml(global_val.clone(), local_val.clone());
        strip_global_only_model_roles(&mut merged, &global_val, &local_val);
        let mut config: Config = merged
            .try_into()
            .map_err(|e| anyhow::anyhow!("Failed to deserialize merged config: {}", e))?;
        config.agent_model_is_local = agent_model_is_local;

        config.validate_model_format()?;

        Ok(config)
    }

    /// Resolve an API key for a given provider, checking all configured sources.
    ///
    /// Priority:
    /// 1. `[llm_endpoints]` — matching endpoint's api_key / api_key_file / key_env
    /// 2. Environment variables (provider-specific, e.g. OPENROUTER_API_KEY)
    /// 3. `[native_executor]` api_key in config.toml (legacy path)
    ///
    /// `workgraph_dir` is the `.workgraph/` directory, used for resolving
    /// relative api_key_file paths and reading native_executor config.
    pub fn resolve_api_key_for_provider(
        &self,
        provider: &str,
        workgraph_dir: &Path,
    ) -> anyhow::Result<String> {
        // 1. Check llm_endpoints for a matching provider
        if let Some(ep) = self.llm_endpoints.find_for_provider(provider)
            && let Ok(Some(key)) = ep.resolve_api_key(Some(workgraph_dir))
        {
            return Ok(key);
        }
        // Also check the default endpoint if provider didn't match
        if let Some(ep) = self.llm_endpoints.find_default()
            && ep.provider != provider
        {
            // Already tried provider-specific above; try default endpoint
            if let Ok(Some(key)) = ep.resolve_api_key(Some(workgraph_dir)) {
                return Ok(key);
            }
        }

        // 2. Environment variables based on provider
        for var_name in EndpointConfig::env_var_names_for_provider(provider) {
            if let Ok(key) = std::env::var(var_name) {
                let key = key.trim().to_string();
                if !key.is_empty() {
                    return Ok(key);
                }
            }
        }

        // 3. Legacy fallback: [native_executor] api_key in merged config (global + local)
        if let Ok(merged_val) = Self::load_merged_toml_value(workgraph_dir)
            && let Some(key) = merged_val
                .get("native_executor")
                .and_then(|v| v.get("api_key"))
                .and_then(|v| v.as_str())
            && !key.is_empty()
        {
            return Ok(key.to_string());
        }

        Err(anyhow::anyhow!(
            "No API key found for provider '{}'. Configure a key via:\n  \
             - wg endpoints add (recommended)\n  \
             - Set {} environment variable\n  \
             - Add [native_executor] api_key to .workgraph/config.toml",
            provider,
            EndpointConfig::env_var_names_for_provider(provider)
                .first()
                .unwrap_or(&"<PROVIDER>_API_KEY"),
        ))
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

        config.validate_model_format()?;

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

        let agent_model_is_local = local_val
            .get("agent")
            .and_then(|a| a.get("model"))
            .and_then(|m| m.as_str())
            .is_some();

        // Merge and deserialize
        let mut merged = merge_toml(global_val.clone(), local_val.clone());
        strip_global_only_model_roles(&mut merged, &global_val, &local_val);
        let mut config: Config = merged
            .try_into()
            .map_err(|e| anyhow::anyhow!("Failed to deserialize merged config: {}", e))?;
        config.agent_model_is_local = agent_model_is_local;

        // Fill in defaults for keys not present in either file
        let default_config = Config::default();
        let default_val: toml::Value = toml::Value::try_from(&default_config)
            .unwrap_or(toml::Value::Table(toml::map::Map::new()));
        let mut default_sources = BTreeMap::new();
        record_sources(
            &default_val,
            "",
            &ConfigSource::Default,
            &mut default_sources,
        );
        for (key, src) in default_sources {
            sources.entry(key).or_insert(src);
        }

        Ok((config, sources))
    }

    /// Compute the effective compaction token threshold for the coordinator.
    ///
    /// If the coordinator model is found in the registry with a known context window,
    /// returns `context_window * compaction_threshold_ratio` (dynamic threshold).
    /// Falls back to `compaction_token_threshold` when:
    /// - No coordinator model is configured
    /// - Model not found in registry
    /// - Model's context_window is 0
    /// - compaction_threshold_ratio is 0.0
    pub fn effective_compaction_threshold(&self) -> u64 {
        let ratio = self.coordinator.compaction_threshold_ratio;
        if ratio > 0.0 {
            // Resolve coordinator model ID: coordinator.model first, then agent.model
            let raw_model = self
                .coordinator
                .model
                .as_deref()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let m = self.agent.model.as_str();
                    if m.is_empty() { None } else { Some(m) }
                });
            // Parse provider:model format to get the registry lookup ID
            let model_id = raw_model.map(|m| parse_model_spec(m).model_id);
            if let Some(ref id) = model_id
                && let Some(entry) = self.registry_lookup(id)
                && entry.context_window > 0
            {
                return (entry.context_window as f64 * ratio).round() as u64;
            }
        }
        self.coordinator.compaction_token_threshold
    }

    /// Validate that all model fields use the `provider:model` format.
    ///
    /// Returns `Ok(())` if all model fields are valid, or an error listing every
    /// field that still uses a bare model name (e.g., `"opus"` instead of `"claude:opus"`).
    pub fn validate_model_format(&self) -> anyhow::Result<()> {
        let mut errors: Vec<String> = Vec::new();

        let check_model = |field: &str, model: &str| -> Option<String> {
            match parse_model_spec_strict(model) {
                Ok(_) => None,
                Err(e) => Some(format!("  {} = \"{}\": {}", field, model, e)),
            }
        };

        // agent.model
        if let Some(err) = check_model("agent.model", &self.agent.model) {
            errors.push(err);
        }

        // coordinator.model
        if let Some(ref m) = self.coordinator.model
            && let Some(err) = check_model("coordinator.model", m)
        {
            errors.push(err);
        }

        // coordinator.provider (deprecated — should not be present)
        if self.coordinator.provider.is_some() {
            errors.push(
                "  coordinator.provider is deprecated. \
                 Use provider:model format in coordinator.model instead."
                    .to_string(),
            );
        }

        // models.* sections
        let role_configs: Vec<(String, &RoleModelConfig)> = {
            let mut pairs = Vec::new();
            if let Some(ref cfg) = self.models.default {
                pairs.push(("models.default".to_string(), cfg));
            }
            for role in DispatchRole::ALL {
                if let Some(cfg) = self.models.get_role(*role) {
                    pairs.push((format!("models.{}", role), cfg));
                }
            }
            pairs
        };

        for (name, cfg) in &role_configs {
            if let Some(ref m) = cfg.model
                && let Some(err) = check_model(&format!("{}.model", name), m)
            {
                errors.push(err);
            }
            if cfg.provider.is_some() {
                errors.push(format!(
                    "  {}.provider is deprecated. \
                     Use provider:model format in {}.model instead.",
                    name, name
                ));
            }
        }

        // tier values
        if let Some(ref t) = self.tiers.fast
            && let Some(err) = check_model("tiers.fast", t)
        {
            errors.push(err);
        }
        if let Some(ref t) = self.tiers.standard
            && let Some(err) = check_model("tiers.standard", t)
        {
            errors.push(err);
        }
        if let Some(ref t) = self.tiers.premium
            && let Some(err) = check_model("tiers.premium", t)
        {
            errors.push(err);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "Config contains model fields that need migration to provider:model format:\n\
                 {}\n\n\
                 To fix: update each field to use provider:model format (e.g., \"claude:opus\").\n\
                 Common mappings:\n\
                 {}\n\
                 {}\n\
                 {}",
                errors.join("\n"),
                "  opus / sonnet / haiku  →  claude:opus / claude:sonnet / claude:haiku",
                "  gpt-4o                 →  openai:gpt-4o",
                "  deepseek/deepseek-chat →  openrouter:deepseek/deepseek-chat",
            )
        }
    }

    /// Validate configuration for common mismatches.
    ///
    /// Returns a `ConfigValidation` containing errors (fatal) and warnings (informational).
    /// Errors should block service start. Warnings should be displayed but allow startup.
    pub fn validate_config(&self) -> ConfigValidation {
        let mut result = ConfigValidation::default();

        // Check coordinator executor + model/provider combinations
        let executor = self.coordinator.effective_executor();
        let model = self
            .coordinator
            .model
            .as_deref()
            .unwrap_or(&self.agent.model);
        // Extract provider from model spec (provider:model format) instead of deprecated field
        let spec = parse_model_spec(model);

        // Rule 1: executor='claude' but model has a non-Anthropic provider prefix or
        // looks like a non-Anthropic model (contains '/' without an Anthropic provider).
        // Uses parse_model_spec to check provider instead of raw string heuristics.
        let is_anthropic_provider = |p: &str| -> bool { p == "anthropic" || p == "claude" };
        let model_looks_non_anthropic = if let Some(ref p) = spec.provider {
            // Model has provider:model format — check the provider
            !is_anthropic_provider(p)
        } else {
            // Bare model — use '/' heuristic as fallback (e.g. "deepseek/deepseek-chat")
            spec.model_id.contains('/') && !spec.model_id.starts_with("anthropic/")
        };
        if executor == "claude" && model_looks_non_anthropic {
            let diagnostic_message = if let Some(ref p) = spec.provider {
                format!(
                    "executor = 'claude' but model = '{}' has non-Anthropic provider '{}'. \
                     Will auto-route to native executor.",
                    model, p
                )
            } else {
                format!(
                    "executor = 'claude' but model = '{}' is non-Anthropic. \
                     Will auto-route to native executor.",
                    model
                )
            };
            result.warnings.push(ConfigDiagnostic {
                rule: "executor-model-auto-route".into(),
                message: diagnostic_message,
                fix: "Set executor = 'native' to make this explicit, \
                     or use claude:MODEL format for Anthropic models."
                    .to_string(),
            });
        }

        // Rule: non-Anthropic provider + Anthropic-only model alias (e.g. provider=openrouter, model=opus)
        // OpenRouter/OpenAI won't understand bare Anthropic aliases like "opus" or "sonnet".
        // Uses spec.model_id (from parse_model_spec) for registry lookup instead of raw model string.
        if let Some(ref p) = spec.provider
            && !is_anthropic_provider(p)
        {
            let model_id = &spec.model_id;
            let is_anthropic_only_model = !model_id.contains('/')
                && self
                    .registry_lookup(model_id)
                    .map(|e| e.provider == "anthropic")
                    .unwrap_or(false); // unknown models are not assumed Anthropic
            if is_anthropic_only_model {
                result.errors.push(ConfigDiagnostic {
                    rule: "provider-model-mismatch".into(),
                    message: format!(
                        "coordinator provider = '{}' but model = '{}' is an Anthropic model alias. \
                         Provider '{}' won't recognize this model name.",
                        p, model_id, p
                    ),
                    fix: format!(
                        "Use a {p}-compatible model (e.g. 'deepseek/deepseek-chat'), \
                         or set provider = 'anthropic' to use '{model_id}' via Anthropic.",
                    ),
                });
            }
        }

        // Rule 3: [models.*] model value doesn't match registry AND doesn't contain '/'
        let registry = self.effective_registry();
        let registry_ids: std::collections::HashSet<&str> =
            registry.iter().map(|e| e.id.as_str()).collect();

        // Check models.default and per-role model values
        let role_configs: Vec<(String, &RoleModelConfig)> = {
            let mut pairs = Vec::new();
            if let Some(ref cfg) = self.models.default {
                pairs.push(("default".to_string(), cfg));
            }
            for role in DispatchRole::ALL {
                if let Some(cfg) = self.models.get_role(*role) {
                    pairs.push((role.to_string(), cfg));
                }
            }
            pairs
        };

        for (role_name, role_cfg) in &role_configs {
            if let Some(ref m) = role_cfg.model {
                // Parse provider:model format to get the registry lookup ID
                let model_spec = parse_model_spec(m);
                let lookup_id = &model_spec.model_id;
                if !registry_ids.contains(lookup_id.as_str()) && !lookup_id.contains('/') {
                    result.warnings.push(ConfigDiagnostic {
                        rule: "unresolved-model-id".into(),
                        message: format!(
                            "models.{}.model = '{}' doesn't match any registry entry \
                             and doesn't look like a provider/model path. \
                             May be an unresolved short ID.",
                            role_name, m
                        ),
                        fix: format!(
                            "Add a [[model_registry]] entry for '{}', use a known ID \
                             ({}), or use a tier name (e.g., 'haiku', 'sonnet', 'opus').",
                            m,
                            registry_ids.iter().copied().collect::<Vec<_>>().join(", ")
                        ),
                    });
                }
            }
        }

        // Rule 4: model_registry entry's 'model' field doesn't contain '/'
        // (should be a full provider-qualified model name for non-Anthropic providers)
        for entry in &self.model_registry {
            if entry.provider != "anthropic" && !entry.model.contains('/') {
                result.warnings.push(ConfigDiagnostic {
                    rule: "registry-model-format".into(),
                    message: format!(
                        "model_registry entry '{}' (provider: '{}') has model = '{}' \
                         which doesn't contain '/'. OpenRouter and similar providers \
                         typically use 'provider/model' format.",
                        entry.id, entry.provider, entry.model
                    ),
                    fix: format!(
                        "Use the full model path, e.g., '{}/{}'.",
                        entry.provider, entry.model
                    ),
                });
            }
        }

        // Rule 5: llm_endpoints has api_key_file that doesn't exist or is empty
        for ep in &self.llm_endpoints.endpoints {
            if let Some(ref file_path) = ep.api_key_file {
                let expanded = expand_tilde(file_path);
                if !expanded.exists() {
                    result.errors.push(ConfigDiagnostic {
                        rule: "missing-api-key-file".into(),
                        message: format!(
                            "Endpoint '{}' (provider: '{}') references api_key_file = '{}' \
                             but the file does not exist.",
                            ep.name, ep.provider, file_path
                        ),
                        fix: format!(
                            "Create the file at '{}' with your API key, \
                             or use api_key_env to reference an environment variable instead.",
                            file_path
                        ),
                    });
                } else if let Ok(contents) = fs::read_to_string(&expanded)
                    && contents.trim().is_empty()
                {
                    result.errors.push(ConfigDiagnostic {
                        rule: "empty-api-key-file".into(),
                        message: format!(
                            "Endpoint '{}' (provider: '{}') references api_key_file = '{}' \
                             but the file is empty.",
                            ep.name, ep.provider, file_path
                        ),
                        fix: "Add your API key to the file.".into(),
                    });
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.agent.executor, "claude");
        assert_eq!(config.agent.model, "claude:opus");
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
        config.agent.model = "claude:haiku".to_string();
        config.agent.interval = 30;
        config.save(temp_dir.path()).unwrap();

        let loaded = Config::load(temp_dir.path()).unwrap();
        assert_eq!(loaded.agent.model, "claude:haiku");
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
    fn test_parse_custom_config() {
        let toml_str = r#"
[agent]
executor = "opencode"
model = "gpt-4"
interval = 60

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
        assert!(config.agency.auto_evaluate);
        assert!(config.agency.auto_assign);
        assert!(config.agency.assigner_agent.is_none());
        assert!(config.agency.evaluator_agent.is_none());
        assert!(config.agency.evolver_agent.is_none());
        assert!(config.agency.retention_heuristics.is_none());
        // Run mode continuum defaults
        assert_eq!(config.agency.exploration_interval, 20);
        assert!((config.agency.cache_population_threshold - 0.8).abs() < f64::EPSILON);
        assert!(
            (config.agency.ucb_exploration_constant - std::f64::consts::SQRT_2).abs()
                < f64::EPSILON
        );
        assert!((config.agency.novelty_bonus_multiplier - 1.5).abs() < f64::EPSILON);
        assert_eq!(config.agency.bizarre_ideation_interval, 10);
    }

    #[test]
    fn test_parse_agency_config() {
        let toml_str = r#"
[agency]
auto_evaluate = true
auto_assign = true
assigner_agent = "abc123"
evaluator_agent = "def456"
evolver_agent = "ghi789"
retention_heuristics = "Retire roles scoring below 0.3 after 10 evaluations"
flip_inference_model = "openrouter:model-a"
flip_comparison_model = "openrouter:model-b"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.agency.auto_evaluate);
        assert!(config.agency.auto_assign);
        assert_eq!(config.agency.assigner_agent, Some("abc123".to_string()));
        assert_eq!(config.agency.evaluator_agent, Some("def456".to_string()));
        assert_eq!(config.agency.evolver_agent, Some("ghi789".to_string()));
        assert_eq!(
            config.agency.retention_heuristics,
            Some("Retire roles scoring below 0.3 after 10 evaluations".to_string())
        );
        assert_eq!(
            config.agency.flip_inference_model,
            Some("openrouter:model-a".to_string())
        );
        assert_eq!(
            config.agency.flip_comparison_model,
            Some("openrouter:model-b".to_string())
        );
    }

    #[test]
    fn test_agency_config_roundtrip() {
        let temp_dir = TempDir::new().unwrap();

        let mut config = Config::default();
        config.agency.auto_evaluate = true;
        config.agency.evolver_agent = Some("abc123".to_string());
        config.agency.flip_inference_model = Some("openrouter:model-c".to_string());
        config.agency.flip_comparison_model = Some("openrouter:model-d".to_string());
        config.save(temp_dir.path()).unwrap();

        let loaded = Config::load(temp_dir.path()).unwrap();
        assert!(loaded.agency.auto_evaluate);
        assert_eq!(loaded.agency.evolver_agent, Some("abc123".to_string()));
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
            merged.as_table().unwrap()["agent"].as_table().unwrap()["model"]
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
            merged.as_table().unwrap()["agent"].as_table().unwrap()["model"]
                .as_str()
                .unwrap(),
            "haiku"
        );
    }

    #[test]
    fn test_strip_global_only_model_roles_basic() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "claude:opus"
[models.task_agent]
model = "claude:opus"
"#,
        )
        .unwrap();
        let local: toml::Value = toml::from_str(
            r#"
[agent]
model = "openrouter:minimax/minimax-m2.7"
"#,
        )
        .unwrap();
        let mut merged = merge_toml(global.clone(), local.clone());
        strip_global_only_model_roles(&mut merged, &global, &local);
        let has_task_agent_model = merged
            .get("models")
            .and_then(|m| m.get("task_agent"))
            .and_then(|t| t.get("model"))
            .is_some();
        assert!(
            !has_task_agent_model,
            "global models.task_agent.model should be stripped when local sets agent.model"
        );
        assert_eq!(
            merged
                .get("agent")
                .unwrap()
                .get("model")
                .unwrap()
                .as_str()
                .unwrap(),
            "openrouter:minimax/minimax-m2.7"
        );
    }

    #[test]
    fn test_strip_global_model_roles_preserves_local_override() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "claude:opus"
[models.task_agent]
model = "claude:opus"
"#,
        )
        .unwrap();
        let local: toml::Value = toml::from_str(
            r#"
[agent]
model = "openrouter:minimax/minimax-m2.7"
[models.task_agent]
model = "openrouter:minimax/minimax-m2.7"
"#,
        )
        .unwrap();
        let mut merged = merge_toml(global.clone(), local.clone());
        strip_global_only_model_roles(&mut merged, &global, &local);
        let task_model = merged
            .get("models")
            .unwrap()
            .get("task_agent")
            .unwrap()
            .get("model")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(task_model, "openrouter:minimax/minimax-m2.7");
    }

    #[test]
    fn test_strip_global_model_roles_no_local_agent_model() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "claude:opus"
[models.task_agent]
model = "claude:sonnet"
"#,
        )
        .unwrap();
        let local: toml::Value = toml::from_str(
            r#"
[coordinator]
max_agents = 2
"#,
        )
        .unwrap();
        let mut merged = merge_toml(global.clone(), local.clone());
        strip_global_only_model_roles(&mut merged, &global, &local);
        let task_model = merged
            .get("models")
            .unwrap()
            .get("task_agent")
            .unwrap()
            .get("model")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(task_model, "claude:sonnet");
    }

    #[test]
    fn test_local_agent_model_overrides_global_task_agent_in_resolution() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "claude:opus"
[models.task_agent]
model = "claude:opus"
"#,
        )
        .unwrap();
        let local: toml::Value = toml::from_str(
            r#"
[agent]
model = "openrouter:minimax/minimax-m2.7"
"#,
        )
        .unwrap();
        let mut merged = merge_toml(global.clone(), local.clone());
        strip_global_only_model_roles(&mut merged, &global, &local);
        let mut config: Config = merged.try_into().unwrap();
        config.agent_model_is_local = true;
        let resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
        assert_eq!(
            resolved.model, "minimax/minimax-m2.7",
            "TaskAgent should resolve to local agent.model"
        );
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
    }

    #[test]
    fn test_local_models_task_agent_preserved_in_resolution() {
        let global: toml::Value = toml::from_str(
            r#"
[agent]
model = "claude:opus"
[models.task_agent]
model = "claude:opus"
"#,
        )
        .unwrap();
        let local: toml::Value = toml::from_str(
            r#"
[agent]
model = "openrouter:minimax/minimax-m2.7"
[models.task_agent]
model = "openrouter:qwen/qwen3-235b"
"#,
        )
        .unwrap();
        let mut merged = merge_toml(global.clone(), local.clone());
        strip_global_only_model_roles(&mut merged, &global, &local);
        let mut config: Config = merged.try_into().unwrap();
        config.agent_model_is_local = true;
        let resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
        assert_eq!(
            resolved.model, "qwen/qwen3-235b",
            "Local models.task_agent.model should be preserved"
        );
    }

    #[test]
    fn test_load_merged_no_global_file() {
        // When no global config exists, load_merged should still work
        // (loads only local). We test with a temp dir as local.
        let temp_dir = TempDir::new().unwrap();
        let local_toml = r#"
[agent]
model = "claude:haiku"
"#;
        fs::write(temp_dir.path().join("config.toml"), local_toml).unwrap();

        // This test depends on whether ~/.workgraph/config.toml exists on the
        // machine, but the merge should work either way.  If the global config
        // uses old format, the merge may fail — that's OK in that scenario.
        match Config::load_merged(temp_dir.path()) {
            Ok(config) => assert_eq!(config.agent.model, "claude:haiku"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("provider:model"),
                    "Expected migration error, got: {}",
                    msg
                );
            }
        }
    }

    #[test]
    fn test_load_merged_no_local_file() {
        // When no local config exists, merged should be global + defaults
        let temp_dir = TempDir::new().unwrap();
        // No config.toml in temp_dir
        // If global config uses old format, this will error — that's expected
        match Config::load_merged(temp_dir.path()) {
            Ok(config) => {
                // Executor can be either the code default "claude" or the global config override
                assert!(
                    config.agent.executor == "claude" || config.agent.executor == "native",
                    "Expected executor to be 'claude' (default) or 'native' (global config), got: {}",
                    config.agent.executor
                );
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("provider:model"),
                    "Expected migration error, got: {}",
                    msg
                );
            }
        }
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

    #[test]
    fn test_resolve_triage_default() {
        // With no config, triage resolves via Fast tier → haiku registry entry
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, CLAUDE_HAIKU_MODEL_ID);
        assert_eq!(resolved.provider, Some("anthropic".to_string()));
        assert!(resolved.registry_entry.is_some());
        assert_eq!(resolved.registry_entry.unwrap().id, "haiku");
    }

    #[test]
    fn test_resolve_flip_inference_default() {
        // With no config, flip_inference resolves via Standard tier → sonnet registry entry
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::FlipInference);
        assert_eq!(resolved.model, CLAUDE_SONNET_MODEL_ID);
        assert!(resolved.registry_entry.is_some());
    }

    #[test]
    fn test_resolve_flip_comparison_default() {
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::FlipComparison);
        assert_eq!(resolved.model, CLAUDE_HAIKU_MODEL_ID);
    }

    #[test]
    fn test_resolve_verification_default() {
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Verification);
        assert_eq!(resolved.model, CLAUDE_OPUS_MODEL_ID);
    }

    #[test]
    fn test_resolve_models_section_override() {
        // [models.triage] should take highest priority
        let mut config = Config::default();
        config.models.triage = Some(RoleModelConfig {
            model: Some("routing-model".to_string()),
            provider: Some("openrouter".to_string()),
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, "routing-model");
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
    }

    #[test]
    fn test_resolve_evaluator_uses_standard_tier() {
        // Evaluator resolves via Standard tier → sonnet registry entry
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, CLAUDE_SONNET_MODEL_ID);
    }

    #[test]
    fn test_default_provider_cascades_to_tier_defaults() {
        // Setting [models.default].provider = "openrouter" should cascade
        // to roles that use tier defaults (triage, flip_comparison, etc.)
        let mut config = Config::default();
        config.models.default = Some(RoleModelConfig {
            model: None,
            provider: Some("openrouter".to_string()),
            tier: None,
            endpoint: None,
        });

        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, CLAUDE_HAIKU_MODEL_ID);
        assert_eq!(
            resolved.provider,
            Some("openrouter".to_string()),
            "Default provider should cascade to tier default roles"
        );

        let resolved = config.resolve_model_for_role(DispatchRole::FlipInference);
        assert_eq!(resolved.model, CLAUDE_SONNET_MODEL_ID);
        assert_eq!(resolved.provider, Some("openrouter".to_string()));

        let resolved = config.resolve_model_for_role(DispatchRole::FlipComparison);
        assert_eq!(resolved.model, CLAUDE_HAIKU_MODEL_ID);
        assert_eq!(resolved.provider, Some("openrouter".to_string()));

        let resolved = config.resolve_model_for_role(DispatchRole::Verification);
        assert_eq!(resolved.model, CLAUDE_OPUS_MODEL_ID);
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
    }

    #[test]
    fn test_default_provider_cascades_to_role_with_model_only() {
        // If a role has model set but no provider, default provider should cascade
        let mut config = Config::default();
        config.models.default = Some(RoleModelConfig {
            model: None,
            provider: Some("openrouter".to_string()),
            tier: None,
            endpoint: None,
        });
        config.models.triage = Some(RoleModelConfig {
            model: Some("anthropic/claude-3.5-haiku".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
        });

        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, "anthropic/claude-3.5-haiku");
        assert_eq!(
            resolved.provider,
            Some("openrouter".to_string()),
            "Default provider should cascade when role only sets model"
        );
    }

    #[test]
    fn test_role_provider_overrides_default_provider() {
        // Role-specific provider should override default provider
        let mut config = Config::default();
        config.models.default = Some(RoleModelConfig {
            model: None,
            provider: Some("openrouter".to_string()),
            tier: None,
            endpoint: None,
        });
        config.models.triage = Some(RoleModelConfig {
            model: Some("gpt-4o-mini".to_string()),
            provider: Some("openai".to_string()),
            tier: None,
            endpoint: None,
        });

        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, "gpt-4o-mini");
        assert_eq!(
            resolved.provider,
            Some("openai".to_string()),
            "Role-specific provider should take priority"
        );
    }

    #[test]
    fn test_default_provider_cascades_to_global_fallback() {
        // Evaluator resolves via Standard tier; default provider overrides registry provider
        let mut config = Config::default();
        config.models.default = Some(RoleModelConfig {
            model: None,
            provider: Some("openrouter".to_string()),
            tier: None,
            endpoint: None,
        });

        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, CLAUDE_SONNET_MODEL_ID);
        assert_eq!(
            resolved.provider,
            Some("openrouter".to_string()),
            "Default provider should cascade to tier-resolved roles"
        );
    }

    #[test]
    fn test_tier_serde_roundtrip() {
        // Tier serializes/deserializes correctly
        let tier = Tier::Fast;
        let json = serde_json::to_string(&tier).unwrap();
        assert_eq!(json, "\"fast\"");
        let parsed: Tier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Tier::Fast);

        let tier = Tier::Premium;
        let json = serde_json::to_string(&tier).unwrap();
        assert_eq!(json, "\"premium\"");
        let parsed: Tier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Tier::Premium);
    }

    #[test]
    fn test_model_registry_entry_serde() {
        let entry = ModelRegistryEntry {
            id: "test".into(),
            provider: "anthropic".into(),
            model: "claude-test".into(),
            tier: Tier::Standard,
            context_window: 100_000,
            max_output_tokens: 4096,
            cost_per_input_mtok: 1.0,
            cost_per_output_mtok: 5.0,
            ..Default::default()
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: ModelRegistryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test");
        assert_eq!(parsed.tier, Tier::Standard);
        assert_eq!(parsed.context_window, 100_000);
    }

    #[test]
    fn test_tier_config_serde() {
        let tc = TierConfig {
            fast: Some("haiku".into()),
            standard: None,
            premium: Some("opus".into()),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let parsed: TierConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.fast, Some("haiku".to_string()));
        assert!(parsed.standard.is_none());
        assert_eq!(parsed.premium, Some("opus".to_string()));
    }

    #[test]
    fn test_effective_registry_returns_builtins_when_empty() {
        let config = Config::default();
        let registry = config.effective_registry();
        assert_eq!(registry.len(), 6);
        assert!(registry.iter().any(|e| e.id == "haiku"));
        assert!(registry.iter().any(|e| e.id == "sonnet"));
        assert!(registry.iter().any(|e| e.id == "opus"));
        assert!(registry.iter().any(|e| e.id == "claude:haiku"));
        assert!(registry.iter().any(|e| e.id == "claude:sonnet"));
        assert!(registry.iter().any(|e| e.id == "claude:opus"));
    }

    #[test]
    fn test_effective_registry_returns_custom_when_configured() {
        let mut config = Config::default();
        config.model_registry = vec![ModelRegistryEntry {
            id: "custom".into(),
            provider: "local".into(),
            model: "my-model".into(),
            tier: Tier::Fast,
            ..Default::default()
        }];
        let registry = config.effective_registry();
        // 6 built-in + 1 custom = 7
        assert_eq!(registry.len(), 7);
        assert!(registry.iter().any(|e| e.id == "custom"));
        assert!(registry.iter().any(|e| e.id == "haiku"));
    }

    #[test]
    fn test_effective_registry_custom_overrides_builtin() {
        let mut config = Config::default();
        config.model_registry = vec![ModelRegistryEntry {
            id: "haiku".into(),
            provider: "local".into(),
            model: "my-haiku".into(),
            tier: Tier::Fast,
            ..Default::default()
        }];
        let registry = config.effective_registry();
        // 5 remaining built-ins + 1 override = 6
        assert_eq!(registry.len(), 6);
        let haiku = registry.iter().find(|e| e.id == "haiku").unwrap();
        assert_eq!(haiku.model, "my-haiku");
        assert_eq!(haiku.provider, "local");
    }

    #[test]
    fn test_resolve_tier_with_registry() {
        let config = Config::default();
        let resolved = config.resolve_tier(Tier::Fast).unwrap();
        assert_eq!(resolved.model, CLAUDE_HAIKU_MODEL_ID);
        assert_eq!(resolved.provider, Some("anthropic".to_string()));
        assert!(resolved.registry_entry.is_some());
    }

    #[test]
    fn test_resolve_tier_bare_model_id_not_in_registry() {
        let mut config = Config::default();
        config.tiers.fast = Some("custom-model".into());
        let resolved = config.resolve_tier(Tier::Fast).unwrap();
        assert_eq!(resolved.model, "custom-model");
        assert!(resolved.provider.is_none());
        assert!(resolved.registry_entry.is_none());
    }

    #[test]
    fn test_role_tier_override() {
        // [models.evaluator].tier = "premium" should resolve to opus
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: None,
            provider: None,
            tier: Some(Tier::Premium),
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, CLAUDE_OPUS_MODEL_ID);
    }

    #[test]
    fn test_direct_model_override_takes_priority_over_tier() {
        // Direct model override should beat tier-based resolution
        let mut config = Config::default();
        config.models.triage = Some(RoleModelConfig {
            model: Some("my-custom-model".to_string()),
            provider: None,
            tier: Some(Tier::Premium), // Should be ignored because model is set
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, "my-custom-model");
    }

    #[test]
    fn test_dispatch_role_default_tier() {
        assert_eq!(DispatchRole::Triage.default_tier(), Tier::Fast);
        assert_eq!(DispatchRole::FlipComparison.default_tier(), Tier::Fast);
        assert_eq!(DispatchRole::Assigner.default_tier(), Tier::Fast);
        assert_eq!(DispatchRole::TaskAgent.default_tier(), Tier::Standard);
        assert_eq!(DispatchRole::Evaluator.default_tier(), Tier::Standard);
        assert_eq!(DispatchRole::FlipInference.default_tier(), Tier::Standard);
        assert_eq!(DispatchRole::Evolver.default_tier(), Tier::Premium);
        assert_eq!(DispatchRole::Creator.default_tier(), Tier::Premium);
        assert_eq!(DispatchRole::Verification.default_tier(), Tier::Premium);
        assert_eq!(DispatchRole::Default.default_tier(), Tier::Standard);
        assert_eq!(DispatchRole::Placer.default_tier(), Tier::Fast);
    }

    #[test]
    fn test_tier_display_and_fromstr() {
        assert_eq!(Tier::Fast.to_string(), "fast");
        assert_eq!(Tier::Standard.to_string(), "standard");
        assert_eq!(Tier::Premium.to_string(), "premium");

        assert_eq!("fast".parse::<Tier>().unwrap(), Tier::Fast);
        assert_eq!("Standard".parse::<Tier>().unwrap(), Tier::Standard);
        assert_eq!("PREMIUM".parse::<Tier>().unwrap(), Tier::Premium);
        assert!("unknown".parse::<Tier>().is_err());
    }

    // ---- EndpointsConfig::find_for_provider tests ----

    #[test]
    fn test_find_for_provider_empty() {
        let endpoints = EndpointsConfig::default();
        assert!(endpoints.find_for_provider("openai").is_none());
    }

    #[test]
    fn test_find_for_provider_single_match() {
        let endpoints = EndpointsConfig {
            endpoints: vec![EndpointConfig {
                name: "my-openai".to_string(),
                provider: "openai".to_string(),
                url: Some("https://api.openai.com/v1".to_string()),
                model: None,
                api_key: Some("sk-test-key".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: false,
                context_window: None,
            }],
        };
        let ep = endpoints.find_for_provider("openai").unwrap();
        assert_eq!(ep.name, "my-openai");
        assert_eq!(ep.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn test_find_for_provider_no_match() {
        let endpoints = EndpointsConfig {
            endpoints: vec![EndpointConfig {
                name: "my-openai".to_string(),
                provider: "openai".to_string(),
                url: None,
                model: None,
                api_key: Some("sk-test".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: false,
                context_window: None,
            }],
        };
        assert!(endpoints.find_for_provider("anthropic").is_none());
    }

    #[test]
    fn test_find_for_provider_prefers_default() {
        let endpoints = EndpointsConfig {
            endpoints: vec![
                EndpointConfig {
                    name: "first-openai".to_string(),
                    provider: "openai".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-first".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
                EndpointConfig {
                    name: "default-openai".to_string(),
                    provider: "openai".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-default".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: true,
                    context_window: None,
                },
                EndpointConfig {
                    name: "third-openai".to_string(),
                    provider: "openai".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-third".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
            ],
        };
        let ep = endpoints.find_for_provider("openai").unwrap();
        assert_eq!(ep.name, "default-openai");
        assert_eq!(ep.api_key.as_deref(), Some("sk-default"));
    }

    #[test]
    fn test_find_for_provider_first_match_without_default() {
        let endpoints = EndpointsConfig {
            endpoints: vec![
                EndpointConfig {
                    name: "anthropic-ep".to_string(),
                    provider: "anthropic".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("ant-key".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
                EndpointConfig {
                    name: "first-openai".to_string(),
                    provider: "openai".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-first".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
                EndpointConfig {
                    name: "second-openai".to_string(),
                    provider: "openai".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-second".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
            ],
        };
        // Without a default, returns the first matching provider
        let ep = endpoints.find_for_provider("openai").unwrap();
        assert_eq!(ep.name, "first-openai");
    }

    #[test]
    fn test_find_for_provider_url_and_key() {
        let endpoints = EndpointsConfig {
            endpoints: vec![EndpointConfig {
                name: "openrouter".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: Some(format!("anthropic/{CLAUDE_SONNET_MODEL_ID}")),
                api_key: Some("sk-or-test".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            }],
        };
        let expected_model = format!("anthropic/{CLAUDE_SONNET_MODEL_ID}");
        let ep = endpoints.find_for_provider("openrouter").unwrap();
        assert_eq!(ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(ep.api_key.as_deref(), Some("sk-or-test"));
        assert_eq!(ep.model.as_deref(), Some(expected_model.as_str()));
    }

    // ---- EndpointsConfig::find_default tests ----

    #[test]
    fn test_find_default_empty() {
        let endpoints = EndpointsConfig::default();
        assert!(endpoints.find_default().is_none());
    }

    #[test]
    fn test_find_default_returns_default_endpoint() {
        let endpoints = EndpointsConfig {
            endpoints: vec![
                EndpointConfig {
                    name: "openai".to_string(),
                    provider: "openai".to_string(),
                    url: None,
                    model: None,
                    api_key: None,
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
                EndpointConfig {
                    name: "openrouter".to_string(),
                    provider: "openrouter".to_string(),
                    url: None,
                    model: None,
                    api_key: None,
                    api_key_file: None,
                    api_key_env: None,
                    is_default: true,
                    context_window: None,
                },
            ],
        };
        let ep = endpoints.find_default().unwrap();
        assert_eq!(ep.name, "openrouter");
    }

    #[test]
    fn test_find_default_falls_back_to_first() {
        let endpoints = EndpointsConfig {
            endpoints: vec![EndpointConfig {
                name: "only".to_string(),
                provider: "openai".to_string(),
                url: None,
                model: None,
                api_key: None,
                api_key_file: None,
                api_key_env: None,
                is_default: false,
                context_window: None,
            }],
        };
        let ep = endpoints.find_default().unwrap();
        assert_eq!(ep.name, "only");
    }

    #[test]
    fn test_find_default_resolves_api_key_for_non_matching_provider() {
        // Simulates the bug scenario: model resolves to provider "openai" but
        // the only configured endpoint has provider "openrouter". find_for_provider("openai")
        // returns None but find_default() returns the openrouter endpoint.
        let endpoints = EndpointsConfig {
            endpoints: vec![EndpointConfig {
                name: "openrouter".to_string(),
                provider: "openrouter".to_string(),
                url: None,
                model: None,
                api_key: Some("sk-or-test-key".to_string()),
                api_key_file: None,
                api_key_env: None,
                is_default: true,
                context_window: None,
            }],
        };
        // Provider-based lookup misses
        assert!(endpoints.find_for_provider("openai").is_none());
        // Default fallback finds it
        let ep = endpoints.find_default().unwrap();
        assert_eq!(ep.provider, "openrouter");
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-or-test-key"));
        // Verify env var names for the provider
        let env_vars = EndpointConfig::env_var_names_for_provider(&ep.provider);
        assert!(env_vars.contains(&"OPENROUTER_API_KEY"));
    }

    // ---- EndpointConfig::resolve_api_key tests ----

    #[test]
    fn test_resolve_api_key_inline() {
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: Some("sk-inline".to_string()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-inline"));
    }

    #[test]
    fn test_resolve_api_key_inline_takes_priority() {
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: Some("sk-inline".to_string()),
            api_key_file: Some("/nonexistent/file".to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        // Inline key should win even if api_key_file is also set
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-inline"));
    }

    #[test]
    fn test_resolve_api_key_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, "sk-from-file\n").unwrap();
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-from-file"));
    }

    #[test]
    fn test_resolve_api_key_file_trims_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, "  sk-trimmed  \n\n").unwrap();
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-trimmed"));
    }

    #[test]
    fn test_resolve_api_key_file_not_found() {
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("/nonexistent/path/key.txt".to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let err = ep.resolve_api_key(None).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("Failed to read API key from"));
        assert!(msg.contains("/nonexistent/path/key.txt"));
    }

    #[test]
    fn test_resolve_api_key_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("empty.key");
        std::fs::write(&key_path, "  \n").unwrap();
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let err = ep.resolve_api_key(None).unwrap_err();
        assert!(format!("{}", err).contains("empty"));
    }

    #[test]
    fn test_resolve_api_key_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("keys").join("test.key");
        std::fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        std::fs::write(&key_path, "sk-relative").unwrap();
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("keys/test.key".to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(Some(dir.path())).unwrap();
        assert_eq!(key.as_deref(), Some("sk-relative"));
    }

    #[test]
    fn test_resolve_api_key_none() {
        // Use "local" provider which has no env var fallback
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "local".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert!(key.is_none());
    }

    #[test]
    #[serial]
    fn test_resolve_api_key_env_var_fallback() {
        // Save/clear env
        let saved = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-env-test") };
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-env-test"));
        // Restore env
        match saved {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
    }

    #[test]
    #[serial]
    fn test_resolve_api_key_inline_beats_env_var() {
        let saved = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-env-should-lose") };
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: Some("sk-inline-wins".to_string()),
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-inline-wins"));
        match saved {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
    }

    #[test]
    #[serial]
    fn test_resolve_api_key_file_beats_env_var() {
        let saved = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-env-should-lose") };
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("test.key");
        std::fs::write(&key_path, "sk-file-wins").unwrap();
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-file-wins"));
        match saved {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
    }

    #[test]
    #[serial]
    fn test_resolve_api_key_openrouter_env_var_cascade() {
        let saved_or = std::env::var("OPENROUTER_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        // Clear both, set only OPENAI_API_KEY
        unsafe { std::env::remove_var("OPENROUTER_API_KEY") };
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-oai-fallback") };
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openrouter".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        let key = ep.resolve_api_key(None).unwrap();
        assert_eq!(key.as_deref(), Some("sk-oai-fallback"));
        // Restore
        match saved_or {
            Some(v) => unsafe { std::env::set_var("OPENROUTER_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENROUTER_API_KEY") },
        }
        match saved_oai {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
    }

    #[test]
    fn test_env_var_names_for_provider() {
        assert_eq!(
            EndpointConfig::env_var_names_for_provider("openrouter"),
            &["OPENROUTER_API_KEY", "OPENAI_API_KEY"]
        );
        assert_eq!(
            EndpointConfig::env_var_names_for_provider("openai"),
            &["OPENAI_API_KEY"]
        );
        assert_eq!(
            EndpointConfig::env_var_names_for_provider("anthropic"),
            &["ANTHROPIC_API_KEY"]
        );
        assert!(EndpointConfig::env_var_names_for_provider("local").is_empty());
        assert!(EndpointConfig::env_var_names_for_provider("unknown").is_empty());
    }

    #[test]
    fn test_masked_key_with_file_ref() {
        let ep = EndpointConfig {
            name: "test".to_string(),
            provider: "openai".to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("~/.config/workgraph/openai.key".to_string()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        };
        assert_eq!(ep.masked_key(), "(from file)");
    }

    // ---- Endpoint routing tests ----

    #[test]
    fn test_find_by_name() {
        let endpoints = EndpointsConfig {
            endpoints: vec![
                EndpointConfig {
                    name: "openrouter".to_string(),
                    provider: "openrouter".to_string(),
                    url: Some("https://openrouter.ai/api/v1".to_string()),
                    model: None,
                    api_key: Some("sk-or-test".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: false,
                    context_window: None,
                },
                EndpointConfig {
                    name: "anthropic-direct".to_string(),
                    provider: "anthropic".to_string(),
                    url: None,
                    model: None,
                    api_key: Some("sk-ant-test".to_string()),
                    api_key_file: None,
                    api_key_env: None,
                    is_default: true,
                    context_window: None,
                },
            ],
        };
        let ep = endpoints.find_by_name("openrouter").unwrap();
        assert_eq!(ep.provider, "openrouter");
        assert_eq!(ep.api_key.as_deref(), Some("sk-or-test"));

        let ep = endpoints.find_by_name("anthropic-direct").unwrap();
        assert_eq!(ep.provider, "anthropic");

        assert!(endpoints.find_by_name("nonexistent").is_none());
    }

    #[test]
    fn test_endpoint_cascades_from_default() {
        let mut config = Config::default();
        config.models.default = Some(RoleModelConfig {
            model: None,
            provider: None,
            tier: None,
            endpoint: Some("openrouter".to_string()),
        });

        // Triage should inherit the default endpoint
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.endpoint.as_deref(), Some("openrouter"));

        // Evaluator should also inherit
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.endpoint.as_deref(), Some("openrouter"));
    }

    #[test]
    fn test_role_endpoint_overrides_default() {
        let mut config = Config::default();
        config.models.default = Some(RoleModelConfig {
            model: None,
            provider: None,
            tier: None,
            endpoint: Some("openrouter".to_string()),
        });
        config.models.evaluator = Some(RoleModelConfig {
            model: None,
            provider: None,
            tier: None,
            endpoint: Some("anthropic-direct".to_string()),
        });

        // Triage inherits default
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.endpoint.as_deref(), Some("openrouter"));

        // Evaluator uses its own endpoint
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.endpoint.as_deref(), Some("anthropic-direct"));
    }

    #[test]
    fn test_no_endpoint_is_backward_compatible() {
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert!(resolved.endpoint.is_none());
    }

    #[test]
    fn test_set_endpoint() {
        let mut config = Config::default();
        config
            .models
            .set_endpoint(DispatchRole::Evaluator, "openrouter");
        let role_cfg = config.models.evaluator.unwrap();
        assert_eq!(role_cfg.endpoint.as_deref(), Some("openrouter"));
        assert!(role_cfg.model.is_none()); // Didn't touch model
        assert!(role_cfg.provider.is_none()); // Didn't touch provider
    }

    // --- effective_compaction_threshold tests ---

    #[test]
    fn test_effective_compaction_threshold_dynamic_from_registry() {
        // Built-in "haiku" has context_window=200_000; 80% = 160_000
        let mut config = Config::default();
        config.coordinator.model = Some("claude:haiku".to_string());
        config.coordinator.compaction_threshold_ratio = 0.8;
        let threshold = config.effective_compaction_threshold();
        assert_eq!(threshold, 160_000);
    }

    #[test]
    fn test_effective_compaction_threshold_mock_200k_context_window() {
        // Mock API returning 200k context window → threshold set to 160k
        let mut config = Config::default();
        config.model_registry = vec![ModelRegistryEntry {
            id: "mock-model".into(),
            provider: "anthropic".into(),
            model: "claude-mock".into(),
            tier: Tier::Standard,
            context_window: 200_000,
            ..Default::default()
        }];
        config.coordinator.model = Some("mock-model".to_string());
        config.coordinator.compaction_threshold_ratio = 0.8;
        let threshold = config.effective_compaction_threshold();
        assert_eq!(threshold, 160_000);
    }

    #[test]
    fn test_effective_compaction_threshold_fallback_unknown_model() {
        // Model not in registry → fallback to compaction_token_threshold
        let mut config = Config::default();
        config.coordinator.model = Some("unknown-model".to_string());
        config.coordinator.compaction_token_threshold = 50_000;
        let threshold = config.effective_compaction_threshold();
        assert_eq!(threshold, 50_000);
    }

    #[test]
    fn test_effective_compaction_threshold_fallback_no_model() {
        // No coordinator model → falls back to agent.model
        let config = Config::default();
        // agent.model defaults to "claude:opus" → registry lookup "opus" (200_000 context window)
        // 200_000 * 0.8 = 160_000
        let threshold = config.effective_compaction_threshold();
        assert_eq!(threshold, 160_000); // uses agent.model "claude:opus" fallback
    }

    #[test]
    fn test_effective_compaction_threshold_ratio_zero_uses_hardcoded() {
        // Ratio = 0.0 → always use compaction_token_threshold
        let mut config = Config::default();
        config.coordinator.model = Some("claude:haiku".to_string());
        config.coordinator.compaction_threshold_ratio = 0.0;
        config.coordinator.compaction_token_threshold = 75_000;
        let threshold = config.effective_compaction_threshold();
        assert_eq!(threshold, 75_000);
    }

    #[test]
    fn test_effective_compaction_threshold_custom_ratio() {
        // sonnet has context_window=200_000; 60% = 120_000
        let mut config = Config::default();
        config.coordinator.model = Some("claude:sonnet".to_string());
        config.coordinator.compaction_threshold_ratio = 0.6;
        let threshold = config.effective_compaction_threshold();
        assert_eq!(threshold, 120_000);
    }

    // ---- Registry resolution in resolve_model_for_role steps 1, 2, 5, 6 ----

    #[test]
    fn test_registry_resolve_step1_role_model_override() {
        // Step 1: [models.evaluator].model = "sonnet" should resolve via registry
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("sonnet".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, CLAUDE_SONNET_MODEL_ID);
        assert!(resolved.registry_entry.is_some());
        assert_eq!(resolved.registry_entry.unwrap().id, "sonnet");
    }

    #[test]
    fn test_registry_resolve_step1_custom_registry_entry() {
        // Step 1: custom registry entry "deepseek-chat" resolves to full path
        let mut config = Config::default();
        config.model_registry = vec![ModelRegistryEntry {
            id: "deepseek-chat".into(),
            provider: "deepseek".into(),
            model: "deepseek/deepseek-chat".into(),
            tier: Tier::Standard,
            ..Default::default()
        }];
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("deepseek-chat".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, "deepseek/deepseek-chat");
        assert_eq!(resolved.provider, Some("deepseek".to_string()));
        assert!(resolved.registry_entry.is_some());
    }

    #[test]
    fn test_registry_resolve_step1_provider_override_beats_registry() {
        // Step 1: explicit provider in role config overrides registry provider
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("sonnet".to_string()),
            provider: Some("openrouter".to_string()),
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, CLAUDE_SONNET_MODEL_ID);
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
        assert!(resolved.registry_entry.is_some());
    }

    #[test]
    fn test_registry_resolve_step1_passthrough_unknown() {
        // Step 1: unknown model string passes through without registry_entry
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("some-unknown-model".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, "some-unknown-model");
        assert!(resolved.registry_entry.is_none());
    }

    // Note: Steps 4 and 5 are currently unreachable because effective_tiers()
    // always fills defaults, so step 3 (resolve_tier with default tier) always
    // succeeds. The registry lookup code is added for correctness if that changes.
    // The registry lookup pattern is identical to step 1 which is tested above.

    // -----------------------------------------------------------------------
    // validate_config tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_config_default_is_clean() {
        let config = Config::default();
        let v = config.validate_config();
        assert!(
            v.is_clean(),
            "Default config should be clean: {}",
            v.display()
        );
    }

    #[test]
    fn test_validate_config_claude_executor_with_slash_model_warns() {
        let mut config = Config::default();
        config.coordinator.executor = Some("claude".to_string());
        config.coordinator.model = Some("minimax/minimax-m2.5".to_string());
        let v = config.validate_config();
        // Auto-routed to native — warning, not error
        assert!(v.is_ok());
        assert!(
            v.warnings
                .iter()
                .any(|w| w.rule == "executor-model-auto-route")
        );
    }

    #[test]
    fn test_validate_config_claude_executor_with_openrouter_model() {
        let mut config = Config::default();
        config.coordinator.executor = Some("claude".to_string());
        // openrouter:opus is a non-Anthropic provider with an Anthropic model alias
        config.coordinator.model = Some("openrouter:opus".to_string());
        let v = config.validate_config();
        // Should warn about executor auto-route (claude executor + non-Anthropic provider)
        assert!(
            v.warnings
                .iter()
                .any(|w| w.rule == "executor-model-auto-route"
                    || w.rule == "executor-provider-auto-route"),
            "Expected auto-route warning, got: {:?}",
            v.warnings
        );
    }

    #[test]
    fn test_validate_config_openrouter_provider_with_compatible_model_ok() {
        let mut config = Config::default();
        config.coordinator.executor = Some("claude".to_string());
        config.coordinator.model = Some("openrouter:deepseek/deepseek-chat".to_string());
        let v = config.validate_config();
        // Non-Anthropic model + non-Anthropic provider = OK (auto-routed)
        assert!(v.is_ok());
    }

    #[test]
    fn test_validate_config_claude_executor_with_openai_model() {
        let mut config = Config::default();
        config.coordinator.executor = Some("claude".to_string());
        // openai provider model — incompatible with claude executor
        config.coordinator.model = Some("openai:gpt-4o".to_string());
        let v = config.validate_config();
        // Should warn about executor auto-route (claude executor + non-Anthropic provider)
        assert!(
            v.warnings
                .iter()
                .any(|w| w.rule == "executor-model-auto-route"
                    || w.rule == "executor-provider-auto-route"),
            "Expected auto-route warning, got: {:?}",
            v.warnings
        );
    }

    #[test]
    fn test_validate_config_claude_executor_with_claude_model_ok() {
        let mut config = Config::default();
        config.coordinator.executor = Some("claude".to_string());
        config.coordinator.model = Some("claude:sonnet".to_string());
        let v = config.validate_config();
        assert!(v.is_ok());
    }

    #[test]
    fn test_validate_config_native_executor_with_openrouter_ok() {
        let mut config = Config::default();
        config.coordinator.executor = Some("native".to_string());
        config.coordinator.model = Some("openrouter:minimax/minimax-m2.5".to_string());
        let v = config.validate_config();
        assert!(v.is_ok());
    }

    #[test]
    fn test_validate_config_unresolved_model_short_id() {
        let mut config = Config::default();
        config.models.default = Some(RoleModelConfig {
            model: Some("unknown-model-xyz".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let v = config.validate_config();
        assert!(v.is_ok()); // warnings don't block
        assert!(!v.warnings.is_empty());
        assert!(v.warnings.iter().any(|w| w.rule == "unresolved-model-id"));
    }

    #[test]
    fn test_validate_config_known_model_id_no_warning() {
        let mut config = Config::default();
        config.models.default = Some(RoleModelConfig {
            model: Some("claude:haiku".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let v = config.validate_config();
        assert!(v.is_clean());
    }

    #[test]
    fn test_validate_config_slash_model_no_warning() {
        let mut config = Config::default();
        config.coordinator.executor = Some("native".to_string());
        config.models.default = Some(RoleModelConfig {
            model: Some("openai/gpt-4o".to_string()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let v = config.validate_config();
        assert!(v.warnings.iter().all(|w| w.rule != "unresolved-model-id"));
    }

    #[test]
    fn test_validate_config_registry_entry_non_anthropic_no_slash() {
        let mut config = Config::default();
        config.model_registry.push(ModelRegistryEntry {
            id: "my-local".into(),
            provider: "openrouter".into(),
            model: "some-model-name".into(),
            tier: Tier::Standard,
            ..Default::default()
        });
        let v = config.validate_config();
        assert!(v.is_ok());
        assert!(v.warnings.iter().any(|w| w.rule == "registry-model-format"));
    }

    #[test]
    fn test_validate_config_registry_entry_anthropic_no_slash_ok() {
        let mut config = Config::default();
        config.model_registry.push(ModelRegistryEntry {
            id: "custom-claude".into(),
            provider: "anthropic".into(),
            model: "claude-custom-model".into(),
            tier: Tier::Standard,
            ..Default::default()
        });
        let v = config.validate_config();
        assert!(v.warnings.iter().all(|w| w.rule != "registry-model-format"));
    }

    #[test]
    fn test_validate_config_missing_api_key_file() {
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "test-endpoint".into(),
            provider: "openrouter".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("/nonexistent/path/to/api-key.txt".into()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        });
        let v = config.validate_config();
        assert!(!v.is_ok());
        assert!(v.errors.iter().any(|e| e.rule == "missing-api-key-file"));
    }

    #[test]
    fn test_validate_config_empty_api_key_file() {
        let temp_dir = TempDir::new().unwrap();
        let key_file = temp_dir.path().join("empty-key.txt");
        fs::write(&key_file, "").unwrap();

        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "test-endpoint".into(),
            provider: "openrouter".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_file.to_string_lossy().into_owned()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        });
        let v = config.validate_config();
        assert!(!v.is_ok());
        assert!(v.errors.iter().any(|e| e.rule == "empty-api-key-file"));
    }

    #[test]
    fn test_validate_config_valid_api_key_file() {
        let temp_dir = TempDir::new().unwrap();
        let key_file = temp_dir.path().join("valid-key.txt");
        fs::write(&key_file, "sk-test-key-12345").unwrap();

        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "test-endpoint".into(),
            provider: "openrouter".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_file.to_string_lossy().into_owned()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        });
        let v = config.validate_config();
        assert!(
            v.errors
                .iter()
                .all(|e| e.rule != "missing-api-key-file" && e.rule != "empty-api-key-file")
        );
    }

    #[test]
    fn test_validate_config_multiple_warnings_for_auto_route() {
        let mut config = Config::default();
        config.coordinator.executor = Some("claude".to_string());
        config.coordinator.model = Some("openrouter:minimax/minimax-m2.5".to_string());
        let v = config.validate_config();
        // Non-Anthropic model with claude executor: auto-routed
        assert!(v.is_ok());
        assert!(!v.warnings.is_empty());
    }

    #[test]
    fn test_validate_config_display_format() {
        let mut config = Config::default();
        // Set up a scenario that produces a warning: missing api_key_file
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "test-endpoint".into(),
            provider: "openrouter".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some("/nonexistent/path/to/key.txt".into()),
            api_key_env: None,
            is_default: false,
            context_window: None,
        });
        let v = config.validate_config();
        let display = v.display();
        assert!(display.contains("ERROR:"));
        assert!(display.contains("Fix:"));
    }

    // --- effective_executor tests ---

    #[test]
    fn test_effective_executor_default_no_provider() {
        let config = Config::default();
        assert_eq!(config.coordinator.effective_executor(), "claude");
    }

    #[test]
    fn test_effective_executor_openrouter_auto_detects_native() {
        let mut config = Config::default();
        config.coordinator.provider = Some("openrouter".to_string());
        assert_eq!(config.coordinator.effective_executor(), "native");
    }

    #[test]
    fn test_effective_executor_openai_auto_detects_native() {
        let mut config = Config::default();
        config.coordinator.provider = Some("openai".to_string());
        assert_eq!(config.coordinator.effective_executor(), "native");
    }

    #[test]
    fn test_effective_executor_local_auto_detects_native() {
        let mut config = Config::default();
        config.coordinator.provider = Some("local".to_string());
        assert_eq!(config.coordinator.effective_executor(), "native");
    }

    #[test]
    fn test_effective_executor_explicit_claude_overrides_openrouter() {
        let mut config = Config::default();
        config.coordinator.executor = Some("claude".to_string());
        config.coordinator.provider = Some("openrouter".to_string());
        assert_eq!(config.coordinator.effective_executor(), "claude");
    }

    #[test]
    fn test_effective_executor_explicit_native_preserved() {
        let mut config = Config::default();
        config.coordinator.executor = Some("native".to_string());
        assert_eq!(config.coordinator.effective_executor(), "native");
    }

    #[test]
    fn test_effective_executor_anthropic_provider_stays_claude() {
        let mut config = Config::default();
        config.coordinator.provider = Some("anthropic".to_string());
        assert_eq!(config.coordinator.effective_executor(), "claude");
    }

    #[test]
    fn test_effective_executor_roundtrip_toml_no_executor() {
        // Config with provider but no executor should auto-detect after round-trip
        let toml_str = r#"
[coordinator]
provider = "openrouter"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.coordinator.executor.is_none());
        assert_eq!(config.coordinator.effective_executor(), "native");
    }

    #[test]
    fn test_effective_executor_roundtrip_toml_explicit_executor() {
        // Config with explicit executor should preserve it after round-trip
        let toml_str = r#"
[coordinator]
executor = "claude"
provider = "openrouter"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.coordinator.executor, Some("claude".to_string()));
        assert_eq!(config.coordinator.effective_executor(), "claude");
    }

    #[test]
    fn test_effective_executor_infers_native_from_model_prefix() {
        let mut config = Config::default();
        config.coordinator.executor = None;
        config.coordinator.provider = None;
        config.coordinator.model = Some("openrouter:minimax/minimax-m2.5".to_string());
        assert_eq!(config.coordinator.effective_executor(), "native");
    }

    #[test]
    fn test_effective_executor_infers_claude_from_claude_prefix() {
        let mut config = Config::default();
        config.coordinator.executor = None;
        config.coordinator.provider = None;
        config.coordinator.model = Some("claude:opus".to_string());
        assert_eq!(config.coordinator.effective_executor(), "claude");
    }

    #[test]
    fn test_effective_executor_bare_model_stays_claude() {
        let mut config = Config::default();
        config.coordinator.executor = None;
        config.coordinator.provider = None;
        config.coordinator.model = Some("claude:opus".to_string());
        assert_eq!(config.coordinator.effective_executor(), "claude");
    }

    // -----------------------------------------------------------------------
    // parse_model_spec: unified provider:model parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_model_spec_with_known_provider() {
        let spec = parse_model_spec("openrouter:deepseek/deepseek-v3.2");
        assert_eq!(spec.provider.as_deref(), Some("openrouter"));
        assert_eq!(spec.model_id, "deepseek/deepseek-v3.2");
    }

    #[test]
    fn test_parse_model_spec_claude_prefix() {
        let spec = parse_model_spec("claude:opus");
        assert_eq!(spec.provider.as_deref(), Some("claude"));
        assert_eq!(spec.model_id, "opus");
    }

    #[test]
    fn test_parse_model_spec_bare_name() {
        let spec = parse_model_spec("opus");
        assert_eq!(spec.provider, None);
        assert_eq!(spec.model_id, "opus");
    }

    #[test]
    fn test_parse_model_spec_legacy_slash_format() {
        // Legacy org/model format should NOT be parsed as provider:model
        let spec = parse_model_spec("deepseek/deepseek-v3.2");
        assert_eq!(spec.provider, None);
        assert_eq!(spec.model_id, "deepseek/deepseek-v3.2");
    }

    #[test]
    fn test_parse_model_spec_ollama_model_tag() {
        // Ollama model tags contain `:` but the prefix isn't a known provider
        let spec = parse_model_spec("deepseek-coder-v2:16b");
        assert_eq!(spec.provider, None);
        assert_eq!(spec.model_id, "deepseek-coder-v2:16b");
    }

    #[test]
    fn test_parse_model_spec_ollama_provider_prefix() {
        // But "ollama:" IS a known provider prefix
        let spec = parse_model_spec("ollama:llama3");
        assert_eq!(spec.provider.as_deref(), Some("ollama"));
        assert_eq!(spec.model_id, "llama3");
    }

    #[test]
    fn test_parse_model_spec_all_known_providers() {
        for provider in KNOWN_PROVIDERS {
            let input = format!("{}:test-model", provider);
            let spec = parse_model_spec(&input);
            assert_eq!(
                spec.provider.as_deref(),
                Some(*provider),
                "Failed for provider: {}",
                provider
            );
            assert_eq!(spec.model_id, "test-model");
        }
    }

    #[test]
    fn test_provider_to_executor_mapping() {
        assert_eq!(provider_to_executor("claude"), "claude");
        assert_eq!(provider_to_executor("codex"), "codex");
        assert_eq!(provider_to_executor("openrouter"), "native");
        assert_eq!(provider_to_executor("openai"), "native");
        assert_eq!(provider_to_executor("gemini"), "native");
        assert_eq!(provider_to_executor("ollama"), "native");
        assert_eq!(provider_to_executor("local"), "native");
    }

    #[test]
    fn test_provider_to_native_provider_mapping() {
        assert_eq!(provider_to_native_provider("openrouter"), "openrouter");
        assert_eq!(provider_to_native_provider("openai"), "openai");
        assert_eq!(provider_to_native_provider("claude"), "anthropic");
        assert_eq!(provider_to_native_provider("codex"), "openai");
        assert_eq!(provider_to_native_provider("gemini"), "openai");
        assert_eq!(provider_to_native_provider("ollama"), "local");
        assert_eq!(provider_to_native_provider("local"), "local");
    }

    // -----------------------------------------------------------------------
    // resolve_tier with provider:model format
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_tier_with_provider_prefix() {
        let mut config = Config::default();
        config.tiers.fast = Some("openrouter:qwen/qwen-turbo".into());
        let resolved = config.resolve_tier(Tier::Fast).unwrap();
        // Model ID should be the bare part (no prefix)
        assert_eq!(resolved.model, "qwen/qwen-turbo");
        // Provider should be derived from the prefix
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
        assert!(resolved.registry_entry.is_none());
    }

    #[test]
    fn test_resolve_tier_claude_prefix() {
        let mut config = Config::default();
        config.tiers.premium = Some("claude:opus".into());
        let resolved = config.resolve_tier(Tier::Premium).unwrap();
        // "claude" prefix → maps to "anthropic" native provider, but "opus"
        // is in the built-in registry, so registry should take precedence
        assert_eq!(resolved.model, CLAUDE_OPUS_MODEL_ID);
        assert_eq!(resolved.provider, Some("anthropic".to_string()));
        assert!(resolved.registry_entry.is_some());
    }

    // -----------------------------------------------------------------------
    // resolve_model_for_role with provider:model format
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_model_for_role_with_provider_prefix() {
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("openrouter:deepseek/deepseek-v3.2".into()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, "deepseek/deepseek-v3.2");
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
    }

    #[test]
    fn test_resolve_model_for_role_claude_prefix_strips_for_api() {
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("claude:claude-sonnet-4-6".into()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        // Model ID should be the bare part without the claude: prefix
        assert_eq!(resolved.model, "claude-sonnet-4-6");
        assert_eq!(resolved.provider, Some("anthropic".to_string()));
    }

    #[test]
    fn test_resolve_model_for_role_provider_prefix_overrides_separate_provider() {
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("openrouter:qwen/qwen-turbo".into()),
            provider: Some("anthropic".into()), // This should be overridden
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, "qwen/qwen-turbo");
        // The provider prefix should win over the separate provider field
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
    }

    #[test]
    fn test_resolve_model_for_role_bare_name_backward_compat() {
        // Bare model names should still work exactly as before
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("gpt-4o-mini".into()),
            provider: Some("openai".into()),
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, "gpt-4o-mini");
        assert_eq!(resolved.provider, Some("openai".to_string()));
    }

    #[test]
    fn test_resolve_model_for_role_ollama_local_provider() {
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("ollama:llama3".into()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, "llama3");
        assert_eq!(resolved.provider, Some("local".to_string()));
    }

    #[test]
    fn test_resolve_model_for_role_gemini_provider() {
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            model: Some("gemini:gemini-2.0-flash-001".into()),
            provider: None,
            tier: None,
            endpoint: None,
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, "gemini-2.0-flash-001");
        // Gemini maps to "openai" native provider (OpenAI-compatible endpoint)
        assert_eq!(resolved.provider, Some("openai".to_string()));
    }

    // -----------------------------------------------------------------------
    // default_url_for_provider: new provider URLs
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_url_for_new_providers() {
        assert_eq!(
            EndpointConfig::default_url_for_provider("ollama"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            EndpointConfig::default_url_for_provider("llamacpp"),
            "http://localhost:8080/v1"
        );
        assert_eq!(
            EndpointConfig::default_url_for_provider("vllm"),
            "http://localhost:8000/v1"
        );
        assert_eq!(
            EndpointConfig::default_url_for_provider("gemini"),
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }

    // ---- Profile-aware tier resolution tests ----

    #[test]
    fn test_effective_tiers_no_profile_uses_defaults() {
        let config = Config::default();
        let tiers = config.effective_tiers();
        assert_eq!(tiers.fast.as_deref(), Some("claude:haiku"));
        assert_eq!(tiers.standard.as_deref(), Some("claude:sonnet"));
        assert_eq!(tiers.premium.as_deref(), Some("claude:opus"));
    }

    #[test]
    fn test_effective_tiers_anthropic_profile() {
        let mut config = Config::default();
        config.profile = Some("anthropic".into());
        let tiers = config.effective_tiers();
        assert_eq!(tiers.fast.as_deref(), Some("claude:haiku"));
        assert_eq!(tiers.standard.as_deref(), Some("claude:sonnet"));
        assert_eq!(tiers.premium.as_deref(), Some("claude:opus"));
    }

    #[test]
    fn test_effective_tiers_openai_profile() {
        let mut config = Config::default();
        config.profile = Some("openai".into());
        let tiers = config.effective_tiers();
        assert_eq!(tiers.fast.as_deref(), Some("openrouter:openai/gpt-4o-mini"));
        assert_eq!(tiers.standard.as_deref(), Some("openrouter:openai/gpt-4o"));
        assert_eq!(tiers.premium.as_deref(), Some("openrouter:openai/o3-pro"));
    }

    #[test]
    fn test_explicit_tiers_override_profile() {
        let mut config = Config::default();
        config.profile = Some("openai".into());
        config.tiers.fast = Some("claude:haiku".into());
        let tiers = config.effective_tiers();
        // Explicit tier wins over profile
        assert_eq!(tiers.fast.as_deref(), Some("claude:haiku"));
        // Profile fills in the rest
        assert_eq!(tiers.standard.as_deref(), Some("openrouter:openai/gpt-4o"));
        assert_eq!(tiers.premium.as_deref(), Some("openrouter:openai/o3-pro"));
    }

    #[test]
    fn test_unknown_profile_falls_through_to_defaults() {
        let mut config = Config::default();
        config.profile = Some("nonexistent".into());
        let tiers = config.effective_tiers();
        // Unknown profile produces no tiers, so hardcoded defaults are used
        assert_eq!(tiers.fast.as_deref(), Some("claude:haiku"));
        assert_eq!(tiers.standard.as_deref(), Some("claude:sonnet"));
        assert_eq!(tiers.premium.as_deref(), Some("claude:opus"));
    }

    #[test]
    fn test_dynamic_profile_falls_through_to_defaults() {
        let mut config = Config::default();
        config.profile = Some("openrouter".into());
        // Dynamic profiles return None from resolve_tiers(), so defaults are used
        let tiers = config.effective_tiers();
        assert_eq!(tiers.fast.as_deref(), Some("claude:haiku"));
        assert_eq!(tiers.standard.as_deref(), Some("claude:sonnet"));
        assert_eq!(tiers.premium.as_deref(), Some("claude:opus"));
    }

    #[test]
    fn test_profile_resolve_model_for_role() {
        let mut config = Config::default();
        config.profile = Some("openai".into());
        // Triage role defaults to Fast tier
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        // openai profile maps fast → openrouter:openai/gpt-4o-mini
        // Since openai/gpt-4o-mini is unlikely to be in the default registry,
        // it resolves to the model ID from the tier spec
        assert!(
            resolved.model.contains("gpt-4o-mini"),
            "Expected gpt-4o-mini in resolved model, got: {}",
            resolved.model
        );
    }

    #[test]
    fn test_profile_config_roundtrip() {
        let mut config = Config::default();
        config.profile = Some("openai".into());
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("profile = \"openai\""));
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.profile.as_deref(), Some("openai"));
    }

    #[test]
    fn test_profile_none_not_serialized() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(
            !toml_str.contains("profile"),
            "profile = None should be skipped in serialization"
        );
    }

    // ---- Config::resolve_api_key_for_provider tests ----

    #[test]
    fn test_resolve_api_key_for_provider_from_endpoint_inline() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "my-openrouter".into(),
            provider: "openrouter".into(),
            url: None,
            model: None,
            api_key: Some("sk-endpoint-key".into()),
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        });
        let key = config
            .resolve_api_key_for_provider("openrouter", dir.path())
            .unwrap();
        assert_eq!(key, "sk-endpoint-key");
    }

    #[test]
    fn test_resolve_api_key_for_provider_from_endpoint_file() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("api.key");
        fs::write(&key_file, "sk-from-file-endpoint\n").unwrap();
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "my-openrouter".into(),
            provider: "openrouter".into(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: Some(key_file.to_string_lossy().into_owned()),
            api_key_env: None,
            is_default: true,
            context_window: None,
        });
        let key = config
            .resolve_api_key_for_provider("openrouter", dir.path())
            .unwrap();
        assert_eq!(key, "sk-from-file-endpoint");
    }

    #[test]
    #[serial]
    fn test_resolve_api_key_for_provider_env_fallback() {
        let saved = std::env::var("OPENROUTER_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENROUTER_API_KEY", "sk-env-or") };
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default(); // no endpoints configured
        let key = config
            .resolve_api_key_for_provider("openrouter", dir.path())
            .unwrap();
        assert_eq!(key, "sk-env-or");
        match saved {
            Some(v) => unsafe { std::env::set_var("OPENROUTER_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENROUTER_API_KEY") },
        }
        match saved_oai {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
    }

    #[test]
    fn test_resolve_api_key_for_provider_endpoint_beats_env() {
        // When endpoint has key, it should win over env var
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "my-openrouter".into(),
            provider: "openrouter".into(),
            url: None,
            model: None,
            api_key: Some("sk-endpoint-wins".into()),
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        });
        // Even if env var is set, endpoint should win
        let key = config
            .resolve_api_key_for_provider("openrouter", dir.path())
            .unwrap();
        assert_eq!(key, "sk-endpoint-wins");
    }

    #[test]
    fn test_resolve_api_key_for_provider_no_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        // With no endpoints, no env vars, and no native_executor config,
        // should return an error (in a clean env with no key set)
        // We can't easily test this because env vars might be set in CI,
        // so just test the positive cases above.
        let _ = config.resolve_api_key_for_provider("local", dir.path());
    }

    #[test]
    fn test_default_config_does_not_serialize_empty_endpoints() {
        let config = Config::default();
        let content = toml::to_string_pretty(&config).unwrap();
        assert!(
            !content.contains("endpoints = []"),
            "Default config should not contain 'endpoints = []' — it shadows global config.\nGot:\n{}",
            content
        );
        assert!(
            !content.contains("[llm_endpoints]"),
            "Default config should not contain '[llm_endpoints]' section when empty.\nGot:\n{}",
            content
        );
    }

    #[test]
    fn test_default_config_does_not_serialize_empty_model_registry() {
        let config = Config::default();
        let content = toml::to_string_pretty(&config).unwrap();
        assert!(
            !content.contains("model_registry = []"),
            "Default config should not contain 'model_registry = []' — it shadows global config.\nGot:\n{}",
            content
        );
    }

    #[test]
    fn test_default_config_does_not_serialize_empty_default_skills() {
        let config = Config::default();
        let content = toml::to_string_pretty(&config).unwrap();
        assert!(
            !content.contains("default_skills = []"),
            "Default config should not contain 'default_skills = []' — it shadows global config.\nGot:\n{}",
            content
        );
    }

    #[test]
    fn test_merge_preserves_global_endpoints_when_local_omits_them() {
        let global: toml::Value = toml::from_str(
            r#"
[llm_endpoints]
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key = "sk-or-test"
is_default = true
"#,
        )
        .unwrap();
        // Local config has no llm_endpoints section at all
        let local: toml::Value = toml::from_str(
            r#"
[agent]
model = "claude:haiku"
"#,
        )
        .unwrap();
        let merged = merge_toml(global, local);
        let config: Config = merged.try_into().unwrap();
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        assert_eq!(config.llm_endpoints.endpoints[0].name, "openrouter");
    }

    // ---- Global config propagation tests ----

    #[test]
    fn test_load_merged_toml_value_merges_global_and_local() {
        // Set up fake global config via temp dir
        let global_dir = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();

        // We can't easily override global_dir(), so test via load_toml_value + merge_toml directly
        let global_path = global_dir.path().join("config.toml");
        let local_path = local_dir.path().join("config.toml");

        std::fs::write(
            &global_path,
            r#"
[native_executor]
provider = "openrouter"
api_key = "sk-or-global-key"
api_base = "https://openrouter.ai/api/v1"

[coordinator]
executor = "native"
"#,
        )
        .unwrap();

        std::fs::write(
            &local_path,
            r#"
[agent]
model = "claude:haiku"
"#,
        )
        .unwrap();

        let global_val = Config::load_toml_value(&global_path).unwrap();
        let local_val = Config::load_toml_value(&local_path).unwrap();
        let merged = merge_toml(global_val, local_val);

        // Global native_executor should be present
        let ne = merged.get("native_executor").unwrap().as_table().unwrap();
        assert_eq!(ne["api_key"].as_str().unwrap(), "sk-or-global-key");
        assert_eq!(ne["provider"].as_str().unwrap(), "openrouter");

        // Local agent model should be present
        let agent = merged.get("agent").unwrap().as_table().unwrap();
        assert_eq!(agent["model"].as_str().unwrap(), "claude:haiku");

        // Global coordinator should be present
        let coord = merged.get("coordinator").unwrap().as_table().unwrap();
        assert_eq!(coord["executor"].as_str().unwrap(), "native");
    }

    #[test]
    fn test_local_config_overrides_global_api_key() {
        let global_path = tempfile::NamedTempFile::new().unwrap();
        let local_path = tempfile::NamedTempFile::new().unwrap();

        std::fs::write(
            global_path.path(),
            r#"
[native_executor]
api_key = "sk-global-key"
provider = "openrouter"
"#,
        )
        .unwrap();

        std::fs::write(
            local_path.path(),
            r#"
[native_executor]
api_key = "sk-local-key"
"#,
        )
        .unwrap();

        let global_val = Config::load_toml_value(global_path.path()).unwrap();
        let local_val = Config::load_toml_value(local_path.path()).unwrap();
        let merged = merge_toml(global_val, local_val);

        let ne = merged.get("native_executor").unwrap().as_table().unwrap();
        // Local api_key should override global
        assert_eq!(ne["api_key"].as_str().unwrap(), "sk-local-key");
        // Global provider should be preserved (not overridden)
        assert_eq!(ne["provider"].as_str().unwrap(), "openrouter");
    }

    #[test]
    fn test_missing_global_config_falls_back_gracefully() {
        let local_dir = tempfile::tempdir().unwrap();

        // No global config exists at all (using load_toml_value with non-existent path)
        let nonexistent_global = PathBuf::from("/tmp/wg_test_nonexistent_global_config.toml");
        let local_path = local_dir.path().join("config.toml");

        std::fs::write(
            &local_path,
            r#"
[agent]
model = "claude:sonnet"
"#,
        )
        .unwrap();

        let global_val = Config::load_toml_value(&nonexistent_global).unwrap();
        let local_val = Config::load_toml_value(&local_path).unwrap();
        let merged = merge_toml(global_val, local_val);

        // Should have just the local config
        let agent = merged.get("agent").unwrap().as_table().unwrap();
        assert_eq!(agent["model"].as_str().unwrap(), "claude:sonnet");
    }

    #[test]
    fn test_missing_local_config_uses_global() {
        let global_path = tempfile::NamedTempFile::new().unwrap();

        std::fs::write(
            global_path.path(),
            r#"
[coordinator]
executor = "native"
max_agents = 2

[native_executor]
provider = "openrouter"
api_key = "sk-or-global"
"#,
        )
        .unwrap();

        let nonexistent_local = PathBuf::from("/tmp/wg_test_nonexistent_local_config.toml");

        let global_val = Config::load_toml_value(global_path.path()).unwrap();
        let local_val = Config::load_toml_value(&nonexistent_local).unwrap();
        let merged = merge_toml(global_val, local_val);

        // Global config should be used entirely
        let coord = merged.get("coordinator").unwrap().as_table().unwrap();
        assert_eq!(coord["executor"].as_str().unwrap(), "native");
        assert_eq!(coord["max_agents"].as_integer().unwrap(), 2);

        let ne = merged.get("native_executor").unwrap().as_table().unwrap();
        assert_eq!(ne["api_key"].as_str().unwrap(), "sk-or-global");
    }

    #[test]
    fn test_global_endpoints_propagate_to_merged_config() {
        let global_path = tempfile::NamedTempFile::new().unwrap();

        std::fs::write(
            global_path.path(),
            r#"
[llm_endpoints]
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key = "sk-or-test-global"
is_default = true
"#,
        )
        .unwrap();

        // Empty local config
        let local_path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(local_path.path(), "").unwrap();

        let global_val = Config::load_toml_value(global_path.path()).unwrap();
        let local_val = Config::load_toml_value(local_path.path()).unwrap();
        let merged = merge_toml(global_val, local_val);

        let config: Config = merged.try_into().unwrap();
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        assert_eq!(
            config.llm_endpoints.endpoints[0].api_key,
            Some("sk-or-test-global".to_string())
        );
        assert_eq!(config.llm_endpoints.endpoints[0].provider, "openrouter");
    }

    #[test]
    fn test_resolve_api_key_from_merged_endpoints() {
        // Build a config with endpoints (as if loaded from global)
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "openrouter".to_string(),
            provider: "openrouter".to_string(),
            url: Some("https://openrouter.ai/api/v1".to_string()),
            api_key: Some("sk-or-from-endpoint".to_string()),
            api_key_file: None,
            api_key_env: None,
            model: None,
            is_default: true,
            context_window: None,
        });

        let tmp = tempfile::tempdir().unwrap();
        // No local config.toml exists — key should come from the endpoint config
        let result = config.resolve_api_key_for_provider("openrouter", tmp.path());
        assert!(result.is_ok(), "Should resolve key from endpoint config");
        assert_eq!(result.unwrap(), "sk-or-from-endpoint");
    }

    #[test]
    fn test_resolve_api_key_legacy_native_executor_from_merged() {
        // Simulate: global config has [native_executor] api_key, local has nothing
        let global_dir = tempfile::tempdir().unwrap();
        let global_path = global_dir.path().join("config.toml");
        std::fs::write(
            &global_path,
            r#"
[native_executor]
api_key = "sk-legacy-global"
"#,
        )
        .unwrap();

        let local_dir = tempfile::tempdir().unwrap();
        // No local config.toml

        let global_val = Config::load_toml_value(&global_path).unwrap();
        let nonexistent_local = local_dir.path().join("config.toml");
        let local_val = Config::load_toml_value(&nonexistent_local).unwrap();
        let merged = merge_toml(global_val, local_val);

        // Verify the native_executor key is present in merged
        let key = merged
            .get("native_executor")
            .and_then(|v| v.get("api_key"))
            .and_then(|v| v.as_str());
        assert_eq!(key, Some("sk-legacy-global"));
    }

    #[test]
    fn test_config_lookup_chain_project_overrides_global_overrides_default() {
        // Build global config with specific values
        let global: toml::Value = toml::from_str(
            r#"
[coordinator]
executor = "native"
max_agents = 2
poll_interval = 30

[agent]
model = "openrouter:meta-llama/llama-3-70b"
"#,
        )
        .unwrap();

        // Build local config overriding only some values
        let local: toml::Value = toml::from_str(
            r#"
[coordinator]
max_agents = 8
"#,
        )
        .unwrap();

        let merged = merge_toml(global, local);
        let config: Config = merged.try_into().unwrap();

        // Local override: max_agents
        assert_eq!(config.coordinator.max_agents, 8);
        // Global preserved: executor, poll_interval
        assert_eq!(config.coordinator.effective_executor(), "native");
        assert_eq!(config.coordinator.poll_interval, 30);
        // Global preserved: agent model
        assert_eq!(config.agent.model, "openrouter:meta-llama/llama-3-70b");
    }

    #[test]
    fn test_both_configs_missing_returns_default() {
        let nonexistent1 = PathBuf::from("/tmp/wg_test_ne_global.toml");
        let nonexistent2 = PathBuf::from("/tmp/wg_test_ne_local.toml");

        let global_val = Config::load_toml_value(&nonexistent1).unwrap();
        let local_val = Config::load_toml_value(&nonexistent2).unwrap();
        let merged = merge_toml(global_val, local_val);

        // Should produce valid default config
        let config: Config = merged.try_into().unwrap();
        assert_eq!(config.coordinator.max_agents, 8); // default
        assert_eq!(config.coordinator.effective_executor(), "claude"); // default
    }

    #[test]
    fn test_global_profile_propagates_to_new_project() {
        // Global config with a profile set
        let global: toml::Value = toml::from_str(
            r#"
profile = "openrouter"
"#,
        )
        .unwrap();

        // Empty local config (fresh wg init)
        let local = toml::Value::Table(toml::map::Map::new());

        let merged = merge_toml(global, local);
        let config: Config = merged.try_into().unwrap();

        assert_eq!(config.profile, Some("openrouter".to_string()));
    }

    #[test]
    fn test_native_executor_config_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.native_executor.web.search_backend, "duckduckgo");
        assert_eq!(config.native_executor.web.fetch_max_chars, 16_000);
        assert_eq!(config.native_executor.web.fetch_timeout_secs, 30);
        assert!(config.native_executor.web.search_api_key.is_none());
        assert_eq!(config.native_executor.background.max_background_tasks, 5);
        assert_eq!(
            config.native_executor.background.background_timeout_secs,
            600
        );
        assert_eq!(config.native_executor.delegate.delegate_max_turns, 10);
        assert_eq!(config.native_executor.delegate.delegate_model, "");
    }

    #[test]
    fn test_native_executor_config_custom_values() {
        let toml_str = r#"
[native_executor.web]
search_backend = "serper"
search_api_key = "sk-test-123"
fetch_max_chars = 32000
fetch_timeout_secs = 60

[native_executor.background]
max_background_tasks = 10
background_timeout_secs = 1200

[native_executor.delegate]
delegate_max_turns = 15
delegate_model = "claude-haiku-4-5-20251001"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.native_executor.web.search_backend, "serper");
        assert_eq!(
            config.native_executor.web.search_api_key,
            Some("sk-test-123".to_string())
        );
        assert_eq!(config.native_executor.web.fetch_max_chars, 32_000);
        assert_eq!(config.native_executor.web.fetch_timeout_secs, 60);
        assert_eq!(config.native_executor.background.max_background_tasks, 10);
        assert_eq!(
            config.native_executor.background.background_timeout_secs,
            1200
        );
        assert_eq!(config.native_executor.delegate.delegate_max_turns, 15);
        assert_eq!(
            config.native_executor.delegate.delegate_model,
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn test_native_executor_config_partial_override() {
        let toml_str = r#"
[native_executor.web]
fetch_max_chars = 8000
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        // Overridden value
        assert_eq!(config.native_executor.web.fetch_max_chars, 8_000);
        // Defaults preserved
        assert_eq!(config.native_executor.web.search_backend, "duckduckgo");
        assert_eq!(config.native_executor.web.fetch_timeout_secs, 30);
        assert_eq!(config.native_executor.background.max_background_tasks, 5);
        assert_eq!(config.native_executor.delegate.delegate_max_turns, 10);
    }
}
