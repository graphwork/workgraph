//! Per-route default configurations for `wg setup` / `wg init`.
//!
//! Five named routes that each produce a complete, working `Config`:
//! `openrouter`, `claude-cli`, `codex-cli`, `local`, `nex-custom`.
//!
//! Every route fills in `[agent]`, `[dispatcher]`, `[tiers]` (all three
//! tiers — fast / standard / premium), `[models]` evaluator + assigner,
//! `[[llm_endpoints.endpoints]]` (when applicable), and
//! `[[model_registry]]` (when applicable). No empty sections, no
//! half-set fields.

use crate::config::{
    Config, EndpointConfig, EndpointsConfig, ModelRegistryEntry, ModelRoutingConfig,
    RoleModelConfig, Tier, TierConfig,
};

/// One of the five smooth setup routes. Each route returns a complete,
/// working `Config` end-to-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupRoute {
    /// nex executor → openrouter.ai with API key from env or file.
    Openrouter,
    /// claude executor → local `claude` CLI login (no API key in config).
    ClaudeCli,
    /// codex executor → local `codex` CLI login.
    CodexCli,
    /// nex executor → local OAI-compatible endpoint (Ollama default).
    Local,
    /// nex executor → user-supplied URL + key + model.
    NexCustom,
}

impl SetupRoute {
    /// Canonical kebab-case route name used in CLI args + display.
    pub fn as_name(&self) -> &'static str {
        match self {
            Self::Openrouter => "openrouter",
            Self::ClaudeCli => "claude-cli",
            Self::CodexCli => "codex-cli",
            Self::Local => "local",
            Self::NexCustom => "nex-custom",
        }
    }

    /// One-line description for menus / summaries.
    pub fn description(&self) -> &'static str {
        match self {
            Self::Openrouter => "OpenRouter — one API key, every major provider (uses nex)",
            Self::ClaudeCli => "Claude Code CLI — local `claude` login, Anthropic models",
            Self::CodexCli => "OpenAI Codex CLI — local `codex` login, OpenAI models",
            Self::Local => "Local — Ollama / vLLM / llama.cpp on localhost (uses nex)",
            Self::NexCustom => {
                "Custom — bring your own OAI-compatible URL + key + model (uses nex)"
            }
        }
    }

    /// Parse a route name from the CLI. Accepts a few aliases.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "openrouter" | "openrouter-cli" | "or" => Some(Self::Openrouter),
            "claude-cli" | "claude" | "anthropic" => Some(Self::ClaudeCli),
            "codex-cli" | "codex" | "openai-cli" => Some(Self::CodexCli),
            "local" | "ollama" | "llama" | "vllm" => Some(Self::Local),
            "nex-custom" | "nex" | "custom" | "oai-compat" => Some(Self::NexCustom),
            _ => None,
        }
    }

    /// All five routes, in display order.
    pub fn all() -> &'static [SetupRoute] {
        &[
            Self::Openrouter,
            Self::ClaudeCli,
            Self::CodexCli,
            Self::Local,
            Self::NexCustom,
        ]
    }

    /// Canonical executor name written into `agent.executor` / `dispatcher.executor`.
    pub fn executor(&self) -> &'static str {
        match self {
            Self::Openrouter | Self::Local | Self::NexCustom => "native",
            Self::ClaudeCli => "claude",
            Self::CodexCli => "codex",
        }
    }

    /// Best-effort match from an `executor` name. Falls back to
    /// `ClaudeCli` for unknown executors — used by `wg config reset`
    /// when there's no current executor at all.
    ///
    /// For *picking a route from a known executor* (e.g. `wg init -x
    /// claude`), prefer [`SetupRoute::try_from_executor`] which returns
    /// `None` for executors that don't have a route mapping, so the
    /// caller can fall through to the legacy path instead of writing
    /// claude defaults to a shell-executor project.
    pub fn from_executor(executor: &str) -> Self {
        Self::try_from_executor(executor).unwrap_or(Self::ClaudeCli)
    }

    /// Conservative version of [`SetupRoute::from_executor`]: returns
    /// `None` for executors that don't map to any of the 5 routes
    /// (e.g. `shell`, `amplifier`, custom executor names). Callers
    /// should fall back to the legacy path when this returns `None`
    /// rather than substituting a default route.
    pub fn try_from_executor(executor: &str) -> Option<Self> {
        match executor {
            "claude" => Some(Self::ClaudeCli),
            "codex" => Some(Self::CodexCli),
            "native" | "nex" => Some(Self::Openrouter),
            _ => None,
        }
    }
}

/// Optional, per-route inputs collected by `wg setup` / `wg init` and
/// folded into the route's defaults. All fields are optional; routes
/// that *require* an input (e.g. `nex-custom` needs a URL) will fall
/// back to a placeholder value with a `# FIXME` marker rather than
/// emit a half-set field.
#[derive(Debug, Clone, Default)]
pub struct RouteParams {
    /// Path to a file containing the API key (`~`-expansion supported).
    pub api_key_file: Option<String>,
    /// Environment variable name holding the API key.
    pub api_key_env: Option<String>,
    /// Endpoint base URL (e.g. `http://localhost:11434/v1`).
    pub url: Option<String>,
    /// Model identifier (interpretation depends on route — see each builder).
    pub model: Option<String>,
}

/// Build a complete `Config` for the given route + user-supplied params.
///
/// The returned config round-trips through TOML and is safe to write to
/// `config.toml` directly. Routes never leave `[tiers]` empty.
pub fn config_for_route(route: SetupRoute, params: RouteParams) -> Config {
    let mut config = match route {
        SetupRoute::Openrouter => openrouter_config(&params),
        SetupRoute::ClaudeCli => claude_cli_config(&params),
        SetupRoute::CodexCli => codex_cli_config(&params),
        SetupRoute::Local => local_config(&params),
        SetupRoute::NexCustom => nex_custom_config(&params),
    };

    // Common: every route gets agency disabled by default; users opt
    // in via the wizard's later "Enable agency?" prompt or `wg config
    // --auto-evaluate true`. Setup routes are about getting a working
    // *executor*, not about the optional evolutionary identity layer.
    config.agency.auto_assign = false;
    config.agency.auto_evaluate = false;

    config
}

// ---------------------------------------------------------------------------
// Route builders
// ---------------------------------------------------------------------------

fn openrouter_config(params: &RouteParams) -> Config {
    let mut config = Config::default();

    // Executor: nex (canonical: "native")
    config.coordinator.executor = Some("native".to_string());
    config.agent.executor = "native".to_string();

    // Endpoint: openrouter.ai
    let api_key_env = params
        .api_key_env
        .clone()
        .or_else(|| Some("OPENROUTER_API_KEY".to_string()));
    let url = params
        .url
        .clone()
        .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
    config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
        endpoints: vec![EndpointConfig {
            name: "openrouter".to_string(),
            provider: "openrouter".to_string(),
            url: Some(url),
            model: None,
            api_key: None,
            api_key_file: params.api_key_file.clone(),
            api_key_env,
            is_default: true,
            context_window: None,
        }],
    };

    // Model registry: 3 standard Claude models via OpenRouter.
    config.model_registry = openrouter_default_registry();

    // Tiers fully populated. Stored in provider:model format so the
    // strict model-spec validator accepts them on reload.
    config.tiers = TierConfig {
        fast: Some("openrouter:anthropic/claude-haiku-4-5".to_string()),
        standard: Some("openrouter:anthropic/claude-sonnet-4-6".to_string()),
        premium: Some("openrouter:anthropic/claude-opus-4-7".to_string()),
    };

    // Worker default: premium tier (opus) — real implementation needs the
    // strongest model. User --model overrides.
    let agent_model = params
        .model
        .clone()
        .unwrap_or_else(|| "openrouter:anthropic/claude-opus-4-7".to_string());
    let agent_model = ensure_provider_prefix(&agent_model, "openrouter");
    config.agent.model = agent_model.clone();
    config.coordinator.model = Some(agent_model.clone());

    // Eval / assign default to haiku — summarization + scoring is fine on
    // the cheap tier, ~10x cost vs sonnet for nearly identical scores.
    config.models = split_role_models_routing(
        &agent_model,
        "openrouter:anthropic/claude-haiku-4-5",
        "openrouter:anthropic/claude-haiku-4-5",
    );

    config
}

fn claude_cli_config(params: &RouteParams) -> Config {
    let mut config = Config::default();

    // Executor: claude
    config.coordinator.executor = Some("claude".to_string());
    config.agent.executor = "claude".to_string();

    // No endpoint, no model registry — claude CLI handles auth/models itself.
    config.llm_endpoints = EndpointsConfig::default();
    config.model_registry = Vec::new();

    // Tiers: provider-prefixed claude aliases (the CLI resolves them).
    config.tiers = TierConfig {
        fast: Some("claude:haiku".to_string()),
        standard: Some("claude:sonnet".to_string()),
        premium: Some("claude:opus".to_string()),
    };

    // Worker default: claude:opus (premium tier) — workers do real
    // implementation. User --model overrides.
    let agent_model = params
        .model
        .clone()
        .unwrap_or_else(|| "claude:opus".to_string());
    let agent_model = ensure_provider_prefix(&agent_model, "claude");
    config.agent.model = agent_model.clone();
    config.coordinator.model = Some(agent_model.clone());

    // Eval / assign default to haiku — scoring + assignment is mostly
    // summarization, sonnet adds ~10x cost for nearly identical scores.
    config.models = split_role_models_routing(&agent_model, "claude:haiku", "claude:haiku");

    config
}

fn codex_cli_config(params: &RouteParams) -> Config {
    let mut config = Config::default();

    // Executor: codex
    config.coordinator.executor = Some("codex".to_string());
    config.agent.executor = "codex".to_string();

    config.llm_endpoints = EndpointsConfig::default();

    // Model registry: codex CLI accepts these as of 2026-04 — gpt-5-mini,
    // gpt-5, o1-pro per task description.
    config.model_registry = codex_default_registry();

    config.tiers = TierConfig {
        fast: Some("codex:gpt-5-mini".to_string()),
        standard: Some("codex:gpt-5".to_string()),
        premium: Some("codex:o1-pro".to_string()),
    };

    // Worker default: codex:o1-pro (premium tier). User --model overrides.
    let agent_model = params
        .model
        .clone()
        .unwrap_or_else(|| "codex:o1-pro".to_string());
    let agent_model = ensure_provider_prefix(&agent_model, "codex");
    config.agent.model = agent_model.clone();
    config.coordinator.model = Some(agent_model.clone());

    // Eval / assign default to the cheap tier (gpt-5-mini).
    config.models =
        split_role_models_routing(&agent_model, "codex:gpt-5-mini", "codex:gpt-5-mini");

    config
}

fn local_config(params: &RouteParams) -> Config {
    let mut config = Config::default();

    config.coordinator.executor = Some("native".to_string());
    config.agent.executor = "native".to_string();

    let url = params
        .url
        .clone()
        .unwrap_or_else(|| "http://localhost:11434/v1".to_string());

    config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
        endpoints: vec![EndpointConfig {
            name: "local".to_string(),
            provider: "local".to_string(),
            url: Some(url),
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        }],
    };

    // Local models: typically only one is loaded at a time. Use whatever
    // the user supplied (or a sensible Ollama default) for all 3 tiers and
    // surface a registry entry so users can edit it later.
    let model_id = params
        .model
        .clone()
        .unwrap_or_else(|| "qwen2.5-coder:7b".to_string());

    config.model_registry = vec![ModelRegistryEntry {
        id: model_id.clone(),
        provider: "local".to_string(),
        model: model_id.clone(),
        tier: Tier::Standard,
        ..Default::default()
    }];

    let default_model = format!("local:{}", model_id);

    // Single local model fills all tiers — user should adjust as they
    // load more models. This is honest about the local single-model
    // reality, not pretending haiku/sonnet/opus exist locally. Stored in
    // provider:model format to satisfy the strict validator.
    config.tiers = TierConfig {
        fast: Some(default_model.clone()),
        standard: Some(default_model.clone()),
        premium: Some(default_model.clone()),
    };
    config.agent.model = default_model.clone();
    config.coordinator.model = Some(default_model.clone());

    config.models = standard_models_routing(&default_model);

    config
}

fn nex_custom_config(params: &RouteParams) -> Config {
    let mut config = Config::default();

    config.coordinator.executor = Some("native".to_string());
    config.agent.executor = "native".to_string();

    let url = params
        .url
        .clone()
        .unwrap_or_else(|| "https://example.com/v1".to_string());
    let model_id = params
        .model
        .clone()
        .unwrap_or_else(|| "custom-model".to_string());

    config.llm_endpoints = EndpointsConfig {
        inherit_global: false,
        endpoints: vec![EndpointConfig {
            name: "custom".to_string(),
            provider: "oai-compat".to_string(),
            url: Some(url),
            model: None,
            api_key: None,
            api_key_file: params.api_key_file.clone(),
            api_key_env: params.api_key_env.clone(),
            is_default: true,
            context_window: None,
        }],
    };

    config.model_registry = vec![ModelRegistryEntry {
        id: model_id.clone(),
        provider: "oai-compat".to_string(),
        model: model_id.clone(),
        tier: Tier::Standard,
        ..Default::default()
    }];

    let default_model = format!("oai-compat:{}", model_id);

    // Single custom model — same single-model treatment as local.
    // provider:model format to satisfy the strict validator.
    config.tiers = TierConfig {
        fast: Some(default_model.clone()),
        standard: Some(default_model.clone()),
        premium: Some(default_model.clone()),
    };
    config.agent.model = default_model.clone();
    config.coordinator.model = Some(default_model.clone());

    config.models = standard_models_routing(&default_model);

    config
}

// ---------------------------------------------------------------------------
// Shared registry helpers
// ---------------------------------------------------------------------------

fn openrouter_default_registry() -> Vec<ModelRegistryEntry> {
    vec![
        ModelRegistryEntry {
            id: "haiku".to_string(),
            provider: "openrouter".to_string(),
            model: "anthropic/claude-haiku-4-5".to_string(),
            tier: Tier::Fast,
            context_window: 200_000,
            max_output_tokens: 8_192,
            ..Default::default()
        },
        ModelRegistryEntry {
            id: "sonnet".to_string(),
            provider: "openrouter".to_string(),
            model: "anthropic/claude-sonnet-4-6".to_string(),
            tier: Tier::Standard,
            context_window: 200_000,
            max_output_tokens: 64_000,
            ..Default::default()
        },
        ModelRegistryEntry {
            id: "opus".to_string(),
            provider: "openrouter".to_string(),
            model: "anthropic/claude-opus-4-7".to_string(),
            tier: Tier::Premium,
            context_window: 200_000,
            max_output_tokens: 32_000,
            ..Default::default()
        },
    ]
}

fn codex_default_registry() -> Vec<ModelRegistryEntry> {
    vec![
        ModelRegistryEntry {
            id: "gpt-5-mini".to_string(),
            provider: "codex".to_string(),
            model: "gpt-5-mini".to_string(),
            tier: Tier::Fast,
            ..Default::default()
        },
        ModelRegistryEntry {
            id: "gpt-5".to_string(),
            provider: "codex".to_string(),
            model: "gpt-5".to_string(),
            tier: Tier::Standard,
            ..Default::default()
        },
        ModelRegistryEntry {
            id: "o1-pro".to_string(),
            provider: "codex".to_string(),
            model: "o1-pro".to_string(),
            tier: Tier::Premium,
            ..Default::default()
        },
    ]
}

/// `[models.evaluator]` + `[models.assigner]` pinned to the given default
/// model spec, so eval / assign runs don't fall back to a different
/// model the user hasn't authorized. Used by routes where cost ≈ 0
/// (local, nex-custom) — same model fills every role.
fn standard_models_routing(default_model: &str) -> ModelRoutingConfig {
    split_role_models_routing(default_model, default_model, default_model)
}

/// Build a `[models.*]` routing block with role-specific models. Used by
/// paid routes (claude-cli, openrouter, codex-cli) where worker cost
/// dominates: workers run premium for real implementation; eval / assign
/// run cheap because scoring + assignment is mostly summarization.
fn split_role_models_routing(
    default_model: &str,
    evaluator_model: &str,
    assigner_model: &str,
) -> ModelRoutingConfig {
    ModelRoutingConfig {
        default: Some(RoleModelConfig {
            provider: None,
            model: Some(default_model.to_string()),
            tier: None,
            endpoint: None,
        }),
        evaluator: Some(RoleModelConfig {
            provider: None,
            model: Some(evaluator_model.to_string()),
            tier: None,
            endpoint: None,
        }),
        assigner: Some(RoleModelConfig {
            provider: None,
            model: Some(assigner_model.to_string()),
            tier: None,
            endpoint: None,
        }),
        ..Default::default()
    }
}

/// If `model` is already in `provider:model` form, return it unchanged.
/// Otherwise, prepend the route's expected provider prefix.
fn ensure_provider_prefix(model: &str, provider: &str) -> String {
    if model.contains(':') && crate::config::KNOWN_PROVIDERS.iter().any(|p| {
        let prefix = format!("{}:", p);
        model.starts_with(&prefix)
    }) {
        model.to_string()
    } else {
        format!("{}:{}", provider, model)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn round_trip(config: &Config) -> Config {
        let toml_str = toml::to_string_pretty(config).expect("serialize");
        toml::from_str(&toml_str).expect("re-parse round-trip")
    }

    fn assert_tiers_filled(config: &Config) {
        assert!(config.tiers.fast.is_some(), "fast tier must be set");
        assert!(config.tiers.standard.is_some(), "standard tier must be set");
        assert!(config.tiers.premium.is_some(), "premium tier must be set");
    }

    fn assert_models_evaluator_and_assigner_pinned(config: &Config) {
        assert!(
            config.models.evaluator.is_some(),
            "models.evaluator must be set"
        );
        assert!(
            config.models.assigner.is_some(),
            "models.assigner must be set"
        );
    }

    // ── openrouter ───────────────────────────────────────────────────

    #[test]
    fn test_route_openrouter_complete_config() {
        let config = config_for_route(
            SetupRoute::Openrouter,
            RouteParams {
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                ..Default::default()
            },
        );

        // Executor wired
        assert_eq!(config.coordinator.executor.as_deref(), Some("native"));
        assert_eq!(config.agent.executor, "native");

        // Endpoint complete
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.provider, "openrouter");
        assert_eq!(ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(ep.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert!(ep.is_default);

        // Model registry has all three Claude tiers
        assert_eq!(config.model_registry.len(), 3);
        let ids: Vec<&str> = config.model_registry.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"haiku"));
        assert!(ids.contains(&"sonnet"));
        assert!(ids.contains(&"opus"));

        // Tiers all filled (provider:model format so strict validator accepts)
        assert_tiers_filled(&config);
        assert_eq!(
            config.tiers.fast.as_deref(),
            Some("openrouter:anthropic/claude-haiku-4-5")
        );
        assert_eq!(
            config.tiers.standard.as_deref(),
            Some("openrouter:anthropic/claude-sonnet-4-6")
        );
        assert_eq!(
            config.tiers.premium.as_deref(),
            Some("openrouter:anthropic/claude-opus-4-7")
        );

        // models.* pinned
        assert_models_evaluator_and_assigner_pinned(&config);

        // Round-trips through TOML cleanly
        let reloaded = round_trip(&config);
        assert_eq!(reloaded.coordinator.executor, config.coordinator.executor);
        assert_eq!(reloaded.tiers.fast, config.tiers.fast);
        assert_eq!(
            reloaded.llm_endpoints.endpoints.len(),
            config.llm_endpoints.endpoints.len()
        );
    }

    // ── claude-cli ───────────────────────────────────────────────────

    #[test]
    fn test_route_claude_cli_complete_config() {
        let config = config_for_route(SetupRoute::ClaudeCli, RouteParams::default());

        assert_eq!(config.coordinator.executor.as_deref(), Some("claude"));
        assert_eq!(config.agent.executor, "claude");

        // No endpoint required — the `claude` CLI handles auth itself.
        assert!(config.llm_endpoints.endpoints.is_empty());

        // Tiers filled with provider-prefixed claude aliases (CLI resolves them).
        assert_tiers_filled(&config);
        assert_eq!(config.tiers.fast.as_deref(), Some("claude:haiku"));
        assert_eq!(config.tiers.standard.as_deref(), Some("claude:sonnet"));
        assert_eq!(config.tiers.premium.as_deref(), Some("claude:opus"));

        // Worker / dispatcher default to opus (premium tier).
        assert_eq!(config.agent.model, "claude:opus");
        assert_eq!(config.coordinator.model.as_deref(), Some("claude:opus"));

        assert_models_evaluator_and_assigner_pinned(&config);

        let reloaded = round_trip(&config);
        assert_eq!(reloaded.tiers.standard, config.tiers.standard);
        assert_eq!(reloaded.agent.model, config.agent.model);
    }

    #[test]
    fn test_route_claude_cli_agent_is_opus() {
        let config = config_for_route(SetupRoute::ClaudeCli, RouteParams::default());
        assert_eq!(
            config.agent.model, "claude:opus",
            "claude-cli worker agent should default to opus (premium) for real implementation"
        );
    }

    #[test]
    fn test_route_claude_cli_evaluator_is_haiku() {
        let config = config_for_route(SetupRoute::ClaudeCli, RouteParams::default());
        let evaluator = config
            .models
            .evaluator
            .as_ref()
            .expect("models.evaluator must be set");
        assert_eq!(
            evaluator.model.as_deref(),
            Some("claude:haiku"),
            "claude-cli evaluator should default to haiku (cheap, sufficient for scoring)"
        );
    }

    #[test]
    fn test_route_claude_cli_assigner_is_haiku() {
        let config = config_for_route(SetupRoute::ClaudeCli, RouteParams::default());
        let assigner = config
            .models
            .assigner
            .as_ref()
            .expect("models.assigner must be set");
        assert_eq!(
            assigner.model.as_deref(),
            Some("claude:haiku"),
            "claude-cli assigner should default to haiku (cheap, sufficient for assignment)"
        );
    }

    #[test]
    fn test_route_openrouter_role_split() {
        let config = config_for_route(SetupRoute::Openrouter, RouteParams::default());
        assert_eq!(
            config.agent.model, "openrouter:anthropic/claude-opus-4-7",
            "openrouter agent should default to opus equivalent"
        );
        let evaluator = config.models.evaluator.as_ref().unwrap();
        assert_eq!(
            evaluator.model.as_deref(),
            Some("openrouter:anthropic/claude-haiku-4-5"),
            "openrouter evaluator should default to haiku equivalent"
        );
        let assigner = config.models.assigner.as_ref().unwrap();
        assert_eq!(
            assigner.model.as_deref(),
            Some("openrouter:anthropic/claude-haiku-4-5"),
            "openrouter assigner should default to haiku equivalent"
        );
    }

    #[test]
    fn test_route_codex_cli_role_split() {
        let config = config_for_route(SetupRoute::CodexCli, RouteParams::default());
        assert_eq!(
            config.agent.model, "codex:o1-pro",
            "codex-cli agent should default to o1-pro (premium)"
        );
        let evaluator = config.models.evaluator.as_ref().unwrap();
        assert_eq!(
            evaluator.model.as_deref(),
            Some("codex:gpt-5-mini"),
            "codex-cli evaluator should default to gpt-5-mini (cheap)"
        );
        let assigner = config.models.assigner.as_ref().unwrap();
        assert_eq!(
            assigner.model.as_deref(),
            Some("codex:gpt-5-mini"),
            "codex-cli assigner should default to gpt-5-mini (cheap)"
        );
    }

    #[test]
    fn test_route_local_uses_same_model_everywhere() {
        let config = config_for_route(
            SetupRoute::Local,
            RouteParams {
                model: Some("qwen3-coder".to_string()),
                url: Some("http://lambda01.example/v1".to_string()),
                ..Default::default()
            },
        );
        let expected = "local:qwen3-coder";
        assert_eq!(config.agent.model, expected);
        assert_eq!(config.coordinator.model.as_deref(), Some(expected));
        assert_eq!(config.tiers.fast.as_deref(), Some(expected));
        assert_eq!(config.tiers.standard.as_deref(), Some(expected));
        assert_eq!(config.tiers.premium.as_deref(), Some(expected));
        assert_eq!(
            config.models.default.as_ref().unwrap().model.as_deref(),
            Some(expected),
            "local: models.default should match the single local model"
        );
        assert_eq!(
            config.models.evaluator.as_ref().unwrap().model.as_deref(),
            Some(expected),
            "local: cost ≈ 0, evaluator should reuse the same model — no need for a tier split"
        );
        assert_eq!(
            config.models.assigner.as_ref().unwrap().model.as_deref(),
            Some(expected),
            "local: cost ≈ 0, assigner should reuse the same model — no need for a tier split"
        );
    }

    #[test]
    fn test_route_nex_custom_uses_same_model_everywhere() {
        let config = config_for_route(
            SetupRoute::NexCustom,
            RouteParams {
                url: Some("https://my.endpoint.example/v1".to_string()),
                model: Some("my-model".to_string()),
                ..Default::default()
            },
        );
        let expected = "oai-compat:my-model";
        assert_eq!(
            config.models.evaluator.as_ref().unwrap().model.as_deref(),
            Some(expected),
            "nex-custom: user-supplied model used everywhere by default"
        );
        assert_eq!(
            config.models.assigner.as_ref().unwrap().model.as_deref(),
            Some(expected),
        );
    }

    // ── codex-cli ────────────────────────────────────────────────────

    #[test]
    fn test_route_codex_cli_complete_config() {
        let config = config_for_route(SetupRoute::CodexCli, RouteParams::default());

        assert_eq!(config.coordinator.executor.as_deref(), Some("codex"));
        assert_eq!(config.agent.executor, "codex");

        assert!(config.llm_endpoints.endpoints.is_empty());

        // Codex registry: 3 entries.
        assert_eq!(config.model_registry.len(), 3);

        assert_tiers_filled(&config);

        // Default = premium tier model = codex:o1-pro (worker runs premium for real implementation).
        assert_eq!(config.agent.model, "codex:o1-pro");
        assert_eq!(config.coordinator.model.as_deref(), Some("codex:o1-pro"));

        assert_models_evaluator_and_assigner_pinned(&config);

        let reloaded = round_trip(&config);
        assert_eq!(reloaded.coordinator.executor, config.coordinator.executor);
        assert_eq!(reloaded.tiers.standard, config.tiers.standard);
    }

    // ── local ────────────────────────────────────────────────────────

    #[test]
    fn test_route_local_complete_config() {
        let config = config_for_route(
            SetupRoute::Local,
            RouteParams {
                model: Some("qwen3:4b".to_string()),
                url: Some("http://localhost:11434/v1".to_string()),
                ..Default::default()
            },
        );

        assert_eq!(config.coordinator.executor.as_deref(), Some("native"));
        assert_eq!(config.agent.executor, "native");

        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.provider, "local");
        assert_eq!(ep.url.as_deref(), Some("http://localhost:11434/v1"));
        assert!(ep.is_default);
        // No API key for local
        assert!(ep.api_key_env.is_none());
        assert!(ep.api_key_file.is_none());

        assert_tiers_filled(&config);
        // Single local model fills all tiers (provider:model format)
        assert_eq!(config.tiers.fast.as_deref(), Some("local:qwen3:4b"));
        assert_eq!(config.tiers.standard.as_deref(), Some("local:qwen3:4b"));
        assert_eq!(config.tiers.premium.as_deref(), Some("local:qwen3:4b"));

        assert_eq!(config.agent.model, "local:qwen3:4b");

        assert_models_evaluator_and_assigner_pinned(&config);

        let reloaded = round_trip(&config);
        assert_eq!(reloaded.tiers.fast, config.tiers.fast);
    }

    #[test]
    fn test_route_local_uses_default_model_when_none_provided() {
        let config = config_for_route(SetupRoute::Local, RouteParams::default());
        assert!(config.tiers.fast.is_some());
        assert!(config.tiers.standard.is_some());
        assert!(config.tiers.premium.is_some());
    }

    // ── nex-custom ───────────────────────────────────────────────────

    #[test]
    fn test_route_nex_custom_complete_config() {
        let config = config_for_route(
            SetupRoute::NexCustom,
            RouteParams {
                url: Some("https://my.endpoint.example/v1".to_string()),
                api_key_env: Some("MY_API_KEY".to_string()),
                model: Some("my-special-model".to_string()),
                ..Default::default()
            },
        );

        assert_eq!(config.coordinator.executor.as_deref(), Some("native"));
        assert_eq!(config.agent.executor, "native");

        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.provider, "oai-compat");
        assert_eq!(ep.url.as_deref(), Some("https://my.endpoint.example/v1"));
        assert_eq!(ep.api_key_env.as_deref(), Some("MY_API_KEY"));
        assert!(ep.is_default);

        assert_tiers_filled(&config);
        assert_eq!(
            config.tiers.fast.as_deref(),
            Some("oai-compat:my-special-model")
        );
        assert_eq!(
            config.tiers.standard.as_deref(),
            Some("oai-compat:my-special-model")
        );
        assert_eq!(
            config.tiers.premium.as_deref(),
            Some("oai-compat:my-special-model")
        );

        assert_eq!(config.agent.model, "oai-compat:my-special-model");

        assert_models_evaluator_and_assigner_pinned(&config);

        let reloaded = round_trip(&config);
        assert_eq!(reloaded.agent.model, config.agent.model);
    }

    // ── route metadata ───────────────────────────────────────────────

    #[test]
    fn test_route_from_name_accepts_aliases() {
        assert_eq!(
            SetupRoute::from_name("claude"),
            Some(SetupRoute::ClaudeCli)
        );
        assert_eq!(
            SetupRoute::from_name("claude-cli"),
            Some(SetupRoute::ClaudeCli)
        );
        assert_eq!(SetupRoute::from_name("codex"), Some(SetupRoute::CodexCli));
        assert_eq!(SetupRoute::from_name("local"), Some(SetupRoute::Local));
        assert_eq!(SetupRoute::from_name("ollama"), Some(SetupRoute::Local));
        assert_eq!(
            SetupRoute::from_name("openrouter"),
            Some(SetupRoute::Openrouter)
        );
        assert_eq!(
            SetupRoute::from_name("nex-custom"),
            Some(SetupRoute::NexCustom)
        );
        assert_eq!(SetupRoute::from_name("nope"), None);
    }

    #[test]
    fn test_route_all_returns_five_routes() {
        assert_eq!(SetupRoute::all().len(), 5);
    }

    #[test]
    fn test_route_executor_mapping() {
        assert_eq!(SetupRoute::Openrouter.executor(), "native");
        assert_eq!(SetupRoute::ClaudeCli.executor(), "claude");
        assert_eq!(SetupRoute::CodexCli.executor(), "codex");
        assert_eq!(SetupRoute::Local.executor(), "native");
        assert_eq!(SetupRoute::NexCustom.executor(), "native");
    }

    #[test]
    fn test_route_from_executor_reverse_mapping() {
        assert_eq!(SetupRoute::from_executor("claude"), SetupRoute::ClaudeCli);
        assert_eq!(SetupRoute::from_executor("codex"), SetupRoute::CodexCli);
        assert_eq!(SetupRoute::from_executor("native"), SetupRoute::Openrouter);
        assert_eq!(SetupRoute::from_executor("nex"), SetupRoute::Openrouter);
        // Unknown -> claude-cli (sane default for new users)
        assert_eq!(SetupRoute::from_executor("unknown"), SetupRoute::ClaudeCli);
    }

    #[test]
    fn test_ensure_provider_prefix_idempotent() {
        assert_eq!(ensure_provider_prefix("claude:opus", "claude"), "claude:opus");
        assert_eq!(
            ensure_provider_prefix("openrouter:anthropic/claude-haiku-4", "openrouter"),
            "openrouter:anthropic/claude-haiku-4"
        );
    }

    #[test]
    fn test_ensure_provider_prefix_adds_prefix() {
        assert_eq!(ensure_provider_prefix("opus", "claude"), "claude:opus");
        assert_eq!(
            ensure_provider_prefix("qwen3:4b", "local"),
            "local:qwen3:4b",
            "ollama-style tag (model:tag) without a known provider prefix should still get prefixed"
        );
    }
}
