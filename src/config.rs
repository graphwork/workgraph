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
    #[serde(default)]
    pub llm_endpoints: EndpointsConfig,

    /// Checkpoint configuration
    #[serde(default)]
    pub checkpoint: CheckpointConfig,

    /// Model routing: per-role model+provider assignments
    #[serde(default)]
    pub models: ModelRoutingConfig,
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
}

fn default_max_child_tasks_per_agent() -> u32 {
    10
}

fn default_max_task_depth() -> u32 {
    8
}

impl Default for GuardrailsConfig {
    fn default() -> Self {
        Self {
            max_child_tasks_per_agent: default_max_child_tasks_per_agent(),
            max_task_depth: default_max_task_depth(),
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
    /// Whether this is the default endpoint for new agents
    #[serde(default)]
    pub is_default: bool,
}

fn default_provider() -> String {
    "anthropic".to_string()
}

impl EndpointConfig {
    /// Return the API key masked for display: "sk-****...ab12"
    pub fn masked_key(&self) -> String {
        match &self.api_key {
            Some(key) if key.len() > 8 => {
                let prefix = &key[..3];
                let suffix = &key[key.len() - 4..];
                format!("{}****...{}", prefix, suffix)
            }
            Some(key) if !key.is_empty() => "****".to_string(),
            _ => "(not set)".to_string(),
        }
    }

    /// Default URL for known providers.
    pub fn default_url_for_provider(provider: &str) -> &'static str {
        match provider {
            "anthropic" => "https://api.anthropic.com",
            "openai" => "https://api.openai.com/v1",
            "openrouter" => "https://openrouter.ai/api/v1",
            "local" => "http://localhost:11434/v1",
            _ => "",
        }
    }
}

/// LLM endpoints configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EndpointsConfig {
    /// List of configured endpoints
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
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

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            auto_interval_turns: default_auto_interval_turns(),
            auto_interval_mins: default_auto_interval_mins(),
            max_checkpoints: default_max_checkpoints(),
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
    /// Evaluation agents for system/meta-tasks (dot-prefixed) — opus-tier default
    SystemEvaluator,
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
}

impl std::fmt::Display for DispatchRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => write!(f, "default"),
            Self::TaskAgent => write!(f, "task_agent"),
            Self::Evaluator => write!(f, "evaluator"),
            Self::SystemEvaluator => write!(f, "system_evaluator"),
            Self::FlipInference => write!(f, "flip_inference"),
            Self::FlipComparison => write!(f, "flip_comparison"),
            Self::Assigner => write!(f, "assigner"),
            Self::Evolver => write!(f, "evolver"),
            Self::Verification => write!(f, "verification"),
            Self::Triage => write!(f, "triage"),
            Self::Creator => write!(f, "creator"),
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
            "system_evaluator" => Ok(Self::SystemEvaluator),
            "flip_inference" => Ok(Self::FlipInference),
            "flip_comparison" => Ok(Self::FlipComparison),
            "assigner" => Ok(Self::Assigner),
            "evolver" => Ok(Self::Evolver),
            "verification" => Ok(Self::Verification),
            "triage" => Ok(Self::Triage),
            "creator" => Ok(Self::Creator),
            _ => Err(anyhow::anyhow!(
                "Unknown dispatch role '{}'. Valid roles: default, task_agent, evaluator, system_evaluator, \
                 flip_inference, flip_comparison, assigner, evolver, verification, triage, creator",
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
        Self::SystemEvaluator,
        Self::FlipInference,
        Self::FlipComparison,
        Self::Assigner,
        Self::Evolver,
        Self::Verification,
        Self::Triage,
        Self::Creator,
    ];
}

/// Per-role model+provider assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleModelConfig {
    /// Provider name (e.g., "anthropic", "openai", "openrouter", "local")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model name within the provider (e.g., "opus", "sonnet", "haiku", "gpt-4o")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
    pub system_evaluator: Option<RoleModelConfig>,

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
}

impl ModelRoutingConfig {
    /// Get the role-specific config for a dispatch role.
    pub fn get_role(&self, role: DispatchRole) -> Option<&RoleModelConfig> {
        match role {
            DispatchRole::Default => self.default.as_ref(),
            DispatchRole::TaskAgent => self.task_agent.as_ref(),
            DispatchRole::Evaluator => self.evaluator.as_ref(),
            DispatchRole::SystemEvaluator => self.system_evaluator.as_ref(),
            DispatchRole::FlipInference => self.flip_inference.as_ref(),
            DispatchRole::FlipComparison => self.flip_comparison.as_ref(),
            DispatchRole::Assigner => self.assigner.as_ref(),
            DispatchRole::Evolver => self.evolver.as_ref(),
            DispatchRole::Verification => self.verification.as_ref(),
            DispatchRole::Triage => self.triage.as_ref(),
            DispatchRole::Creator => self.creator.as_ref(),
        }
    }

    /// Get a mutable reference to a role's config, creating it if needed.
    pub fn get_role_mut(&mut self, role: DispatchRole) -> &mut Option<RoleModelConfig> {
        match role {
            DispatchRole::Default => &mut self.default,
            DispatchRole::TaskAgent => &mut self.task_agent,
            DispatchRole::Evaluator => &mut self.evaluator,
            DispatchRole::SystemEvaluator => &mut self.system_evaluator,
            DispatchRole::FlipInference => &mut self.flip_inference,
            DispatchRole::FlipComparison => &mut self.flip_comparison,
            DispatchRole::Assigner => &mut self.assigner,
            DispatchRole::Evolver => &mut self.evolver,
            DispatchRole::Verification => &mut self.verification,
            DispatchRole::Triage => &mut self.triage,
            DispatchRole::Creator => &mut self.creator,
        }
    }

    /// Set the model for a role.
    pub fn set_model(&mut self, role: DispatchRole, model: &str) {
        let slot = self.get_role_mut(role);
        if let Some(cfg) = slot {
            cfg.model = Some(model.to_string());
        } else {
            *slot = Some(RoleModelConfig {
                provider: None,
                model: Some(model.to_string()),
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
            });
        }
    }
}

/// Resolved model+provider for a dispatch.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub provider: Option<String>,
}

impl Config {
    /// Resolve the model (and optional provider) for a given dispatch role.
    ///
    /// Resolution order:
    /// 1. `models.<role>.model` (role-specific override in [models] section)
    /// 2. Legacy per-role config (e.g., `agency.evaluator_model` for Evaluator)
    /// 3. `models.default.model` (default in [models] section)
    /// 4. `agent.model` (global fallback)
    ///
    /// Provider resolution follows the same cascade but only from [models].
    pub fn resolve_model_for_role(&self, role: DispatchRole) -> ResolvedModel {
        // 1. Check role-specific [models] config
        if let Some(role_cfg) = self.models.get_role(role)
            && let Some(ref model) = role_cfg.model
        {
            return ResolvedModel {
                model: model.clone(),
                provider: role_cfg.provider.clone(),
            };
        }

        // 2. Legacy per-role config (backward compatibility)
        let legacy_model = match role {
            DispatchRole::Evaluator => self.agency.evaluator_model.as_ref(),
            DispatchRole::Assigner => self.agency.assigner_model.as_ref(),
            DispatchRole::Evolver => self.agency.evolver_model.as_ref(),
            DispatchRole::Creator => self.agency.creator_model.as_ref(),
            DispatchRole::Triage => self.agency.triage_model.as_ref(),
            DispatchRole::FlipInference => self.agency.flip_inference_model.as_ref(),
            DispatchRole::FlipComparison => self.agency.flip_comparison_model.as_ref(),
            // Verification: use the non-optional legacy field only if it differs
            // from the old hardcoded default (meaning the user explicitly set it)
            DispatchRole::Verification => {
                if self.agency.flip_verification_model != "opus" {
                    Some(&self.agency.flip_verification_model)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(model) = legacy_model {
            // Legacy config has no provider — check if [models] role has provider set
            let provider = self.models.get_role(role).and_then(|c| c.provider.clone());
            return ResolvedModel {
                model: model.clone(),
                provider,
            };
        }

        // 2.5. Tier-appropriate defaults for roles that historically had
        // hardcoded fallbacks. This ensures these roles get sensible models
        // even without explicit config, while keeping defaults in one place.
        let tier_default = match role {
            DispatchRole::Triage => Some("haiku"),
            DispatchRole::FlipComparison => Some("haiku"),
            DispatchRole::FlipInference => Some("sonnet"),
            DispatchRole::Verification => Some("opus"),
            DispatchRole::SystemEvaluator => Some("opus"),
            _ => None,
        };
        if let Some(default_model) = tier_default {
            let provider = self.models.get_role(role).and_then(|c| c.provider.clone());
            return ResolvedModel {
                model: default_model.to_string(),
                provider,
            };
        }

        // 3. Check [models.default]
        if let Some(default_cfg) = self.models.get_role(DispatchRole::Default)
            && let Some(ref model) = default_cfg.model
        {
            return ResolvedModel {
                model: model.clone(),
                provider: default_cfg.provider.clone(),
            };
        }

        // 4. Global fallback
        ResolvedModel {
            model: self.agent.model.clone(),
            provider: self
                .models
                .get_role(DispatchRole::Default)
                .and_then(|c| c.provider.clone()),
        }
    }

    /// Check for legacy `agency.*_model` fields and emit deprecation warnings to stderr.
    /// Returns the list of deprecated fields found (useful for testing).
    pub fn check_legacy_deprecations(&self) -> Vec<String> {
        let legacy_fields: &[(&str, &Option<String>, &str)] = &[
            (
                "agency.evaluator_model",
                &self.agency.evaluator_model,
                "evaluator",
            ),
            (
                "agency.assigner_model",
                &self.agency.assigner_model,
                "assigner",
            ),
            (
                "agency.evolver_model",
                &self.agency.evolver_model,
                "evolver",
            ),
            (
                "agency.creator_model",
                &self.agency.creator_model,
                "creator",
            ),
            ("agency.triage_model", &self.agency.triage_model, "triage"),
            (
                "agency.flip_inference_model",
                &self.agency.flip_inference_model,
                "flip_inference",
            ),
            (
                "agency.flip_comparison_model",
                &self.agency.flip_comparison_model,
                "flip_comparison",
            ),
        ];

        let mut deprecated = Vec::new();

        for (field, value, role) in legacy_fields {
            if value.is_some() {
                eprintln!(
                    "Warning: {} is deprecated. Use [models.{}] model = \"{}\" instead. \
                     Migrate with: wg config --set-model {} {}",
                    field,
                    role,
                    value.as_ref().unwrap(),
                    role,
                    value.as_ref().unwrap(),
                );
                deprecated.push(field.to_string());
            }
        }

        // Special case: flip_verification_model is non-optional with default "opus"
        // Only warn if user explicitly changed it from default
        if self.agency.flip_verification_model != "opus" {
            eprintln!(
                "Warning: agency.flip_verification_model is deprecated. Use [models.verification] model = \"{}\" instead. \
                 Migrate with: wg config --set-model verification {}",
                self.agency.flip_verification_model, self.agency.flip_verification_model,
            );
            deprecated.push("agency.flip_verification_model".to_string());
        }

        deprecated
    }
}

fn default_auto_create_threshold() -> u32 {
    20
}
fn default_run_mode() -> f64 {
    0.2
}
fn default_min_exploration_rate() -> f64 {
    0.05
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
fn default_performance_threshold() -> f64 {
    0.7
}
fn default_flip_verification_threshold() -> Option<f64> {
    Some(0.7)
}
fn default_flip_verification_model() -> String {
    "opus".to_string()
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

    /// Model to use for triage (default: "haiku")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_model: Option<String>,

    /// Timeout in seconds for triage calls (default: 30)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_timeout: Option<u64>,

    /// Timeout in seconds for evaluation LLM calls (default: 120).
    /// Eval prompts are larger than triage prompts and need more time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_timeout: Option<u64>,

    /// Maximum bytes to read from agent output log for triage (default: 50000)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_max_log_bytes: Option<usize>,

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

    /// Grace period in seconds after task creation before auto-assignment
    /// is eligible. Prevents premature assignment when tasks are created
    /// and then have dependencies wired shortly after.
    /// Default: 10
    #[serde(default = "default_auto_assign_grace_seconds")]
    pub auto_assign_grace_seconds: u64,

    /// Global evaluation gate threshold. When set, evaluations that score
    /// below this threshold will reject (fail) the original task, blocking
    /// its dependents. Only applies to tasks tagged with "eval-gate" unless
    /// `eval_gate_all` is true. Range: 0.0–1.0. Default: None (disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

    /// Model to use for FLIP inference phase (reconstructing the prompt from output).
    /// Default: "sonnet" (needs creative reconstruction ability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_inference_model: Option<String>,

    /// Model to use for FLIP comparison phase (scoring similarity).
    /// Default: "haiku" (comparison is simpler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flip_comparison_model: Option<String>,

    /// FLIP score threshold below which automatic Opus verification is triggered.
    /// When a FLIP evaluation scores below this threshold, the coordinator creates
    /// a verification task that independently checks whether the work was done.
    /// Default: 0.7. Set to None to disable.
    #[serde(default = "default_flip_verification_threshold")]
    pub flip_verification_threshold: Option<f64>,

    /// Model to use for FLIP-triggered verification agents.
    /// Default: "opus" (highest capability for independent verification).
    #[serde(default = "default_flip_verification_model")]
    pub flip_verification_model: String,

    /// Weight of cost in assignment scoring (0.0 = ignore cost, 1.0 = equal to quality).
    #[serde(default)]
    pub cost_weight: f64,

    /// Maximum USD budget per task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_per_task: Option<f64>,

    /// Maximum USD budget for the entire project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_budget_usd: Option<f64>,
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
            auto_create: false,
            auto_create_threshold: default_auto_create_threshold(),
            retention_heuristics: None,
            auto_triage: false,
            triage_model: None,
            triage_timeout: None,
            eval_timeout: None,
            triage_max_log_bytes: None,
            run_mode: default_run_mode(),
            min_exploration_rate: default_min_exploration_rate(),
            exploration_interval: default_exploration_interval(),
            cache_population_threshold: default_cache_population_threshold(),
            ucb_exploration_constant: default_ucb_exploration_constant(),
            novelty_bonus_multiplier: default_novelty_bonus_multiplier(),
            bizarre_ideation_interval: default_bizarre_ideation_interval(),
            performance_threshold: default_performance_threshold(),
            auto_assign_grace_seconds: default_auto_assign_grace_seconds(),
            eval_gate_threshold: None,
            eval_gate_all: false,
            flip_enabled: false,
            flip_inference_model: None,
            flip_comparison_model: None,
            flip_verification_threshold: default_flip_verification_threshold(),
            flip_verification_model: default_flip_verification_model(),
            cost_weight: 0.0,
            max_cost_per_task: None,
            project_budget_usd: None,
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

    /// Minutes of awake-time with no stream activity before counting as stale (default: 10)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_threshold: Option<u64>,

    /// Seconds after system wake before checking liveness (default: 120)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_grace_period: Option<u64>,

    /// Seconds of wall-vs-monotonic divergence to detect sleep (default: 30)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_gap_threshold: Option<u64>,

    /// Consecutive stale ticks before triggering triage (default: 2)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_tick_threshold: Option<u32>,
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

    /// Whether to spawn a persistent LLM coordinator agent for chat.
    /// When true, the daemon launches a Claude CLI session that interprets
    /// user chat messages and manages the graph conversationally.
    /// When false, chat uses a simple stub response.
    /// Default: true.
    #[serde(default = "default_coordinator_agent")]
    pub coordinator_agent: bool,
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

fn default_coordinator_agent() -> bool {
    true
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
            coordinator_agent: default_coordinator_agent(),
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
            stale_threshold: None,
            wake_grace_period: None,
            sleep_gap_threshold: None,
            stale_tick_threshold: None,
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
        let content = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config at {}: {}", path.display(), e))?;
        let val: toml::Value = content
            .parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse config at {}: {}", path.display(), e))?;
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
        let config: Config = merged
            .try_into()
            .map_err(|e| anyhow::anyhow!("Failed to deserialize merged config: {}", e))?;

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
            Ok(config) => {
                config.check_legacy_deprecations();
                config
            }
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
        let config: Config = merged
            .try_into()
            .map_err(|e| anyhow::anyhow!("Failed to deserialize merged config: {}", e))?;

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
        assert!(
            (config.agency.ucb_exploration_constant - std::f64::consts::SQRT_2).abs()
                < f64::EPSILON
        );
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

    #[test]
    fn test_resolve_triage_default() {
        // With no config at all, triage should resolve to "haiku" (budget tier)
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, "haiku");
        assert!(resolved.provider.is_none());
    }

    #[test]
    fn test_resolve_flip_inference_default() {
        // With no config, flip_inference should resolve to "sonnet" (mid tier)
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::FlipInference);
        assert_eq!(resolved.model, "sonnet");
    }

    #[test]
    fn test_resolve_flip_comparison_default() {
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::FlipComparison);
        assert_eq!(resolved.model, "haiku");
    }

    #[test]
    fn test_resolve_verification_default() {
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Verification);
        assert_eq!(resolved.model, "opus");
    }

    #[test]
    fn test_resolve_triage_legacy_override() {
        // Legacy agency.triage_model should take priority over tier default
        let mut config = Config::default();
        config.agency.triage_model = Some("gpt-4o-mini".to_string());
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, "gpt-4o-mini");
    }

    #[test]
    fn test_resolve_models_section_override() {
        // [models.triage] should take highest priority
        let mut config = Config::default();
        config.agency.triage_model = Some("legacy-model".to_string());
        config.models.triage = Some(RoleModelConfig {
            model: Some("routing-model".to_string()),
            provider: Some("openrouter".to_string()),
        });
        let resolved = config.resolve_model_for_role(DispatchRole::Triage);
        assert_eq!(resolved.model, "routing-model");
        assert_eq!(resolved.provider, Some("openrouter".to_string()));
    }

    #[test]
    fn test_resolve_verification_legacy_override() {
        // If user explicitly sets flip_verification_model to non-default, it should be used
        let mut config = Config::default();
        config.agency.flip_verification_model = "sonnet".to_string();
        let resolved = config.resolve_model_for_role(DispatchRole::Verification);
        assert_eq!(resolved.model, "sonnet");
    }

    #[test]
    fn test_resolve_evaluator_falls_to_global() {
        // Evaluator has no tier default, should fall through to agent.model
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, config.agent.model);
    }

    #[test]
    fn test_resolve_system_evaluator_defaults_to_opus() {
        let config = Config::default();
        let resolved = config.resolve_model_for_role(DispatchRole::SystemEvaluator);
        assert_eq!(resolved.model, "opus");
    }


    #[test]
    fn test_deprecation_no_warnings_on_default() {
        let config = Config::default();
        let deprecated = config.check_legacy_deprecations();
        assert!(
            deprecated.is_empty(),
            "Default config should have no deprecation warnings"
        );
    }

    #[test]
    fn test_deprecation_warning_evaluator_model() {
        let mut config = Config::default();
        config.agency.evaluator_model = Some("haiku".to_string());
        let deprecated = config.check_legacy_deprecations();
        assert!(deprecated.contains(&"agency.evaluator_model".to_string()));
    }

    #[test]
    fn test_deprecation_warning_multiple_fields() {
        let mut config = Config::default();
        config.agency.evaluator_model = Some("haiku".to_string());
        config.agency.assigner_model = Some("sonnet".to_string());
        config.agency.triage_model = Some("haiku".to_string());
        let deprecated = config.check_legacy_deprecations();
        assert_eq!(deprecated.len(), 3);
        assert!(deprecated.contains(&"agency.evaluator_model".to_string()));
        assert!(deprecated.contains(&"agency.assigner_model".to_string()));
        assert!(deprecated.contains(&"agency.triage_model".to_string()));
    }

    #[test]
    fn test_deprecation_warning_flip_verification_non_default() {
        let mut config = Config::default();
        config.agency.flip_verification_model = "sonnet".to_string();
        let deprecated = config.check_legacy_deprecations();
        assert!(deprecated.contains(&"agency.flip_verification_model".to_string()));
    }

    #[test]
    fn test_deprecation_no_warning_flip_verification_default() {
        let mut config = Config::default();
        config.agency.flip_verification_model = "opus".to_string();
        let deprecated = config.check_legacy_deprecations();
        assert!(
            !deprecated.contains(&"agency.flip_verification_model".to_string()),
            "Default 'opus' should not trigger deprecation"
        );
    }

    #[test]
    fn test_deprecation_no_warning_default_config() {
        let config = Config::default();
        let deprecated = config.check_legacy_deprecations();
        assert!(
            deprecated.is_empty(),
            "Default config should have no deprecation warnings"
        );
    }

    #[test]
    fn test_legacy_fields_still_resolve() {
        // Legacy fields should still work through resolve_model_for_role
        let mut config = Config::default();
        config.agency.evaluator_model = Some("haiku".to_string());
        let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
        assert_eq!(resolved.model, "haiku");
    }
}
