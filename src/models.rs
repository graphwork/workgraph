//! Model registry for workgraph.
//!
//! Provides a catalog of AI models with cost, capability, and tier metadata.
//! The registry is loaded from `.workgraph/models.yaml` and ships with
//! sensible defaults covering popular models across providers and price tiers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;

use std::path::Path;

/// Tier classification for models
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Frontier,
    Mid,
    Budget,
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelTier::Frontier => write!(f, "frontier"),
            ModelTier::Mid => write!(f, "mid"),
            ModelTier::Budget => write!(f, "budget"),
        }
    }
}

impl std::str::FromStr for ModelTier {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "frontier" => Ok(ModelTier::Frontier),
            "mid" => Ok(ModelTier::Mid),
            "budget" => Ok(ModelTier::Budget),
            other => anyhow::bail!("Unknown tier '{}'. Must be: frontier, mid, budget", other),
        }
    }
}

/// A single model in the registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Model ID in provider/model-name format (e.g. "anthropic/claude-opus-4-6")
    pub id: String,

    /// Provider (e.g. "openrouter", "anthropic", "openai")
    #[serde(default = "default_provider")]
    pub provider: String,

    /// Cost per 1M input tokens (USD)
    pub cost_per_1m_input: f64,

    /// Cost per 1M output tokens (USD)
    pub cost_per_1m_output: f64,

    /// Context window size in tokens
    #[serde(default)]
    pub context_window: u64,

    /// Capability tags (e.g. "coding", "analysis", "creative")
    #[serde(default)]
    pub capabilities: Vec<String>,

    /// Tier classification
    pub tier: ModelTier,
}

impl ModelEntry {
    /// Extract a short name from the model ID for use in config and UI.
    pub fn short_name(&self) -> &str {
        self.id.rsplit('/').next().unwrap_or(&self.id)
    }

    /// Return a provider:short_name string suitable for config values.
    /// Maps the registry provider to the config provider prefix
    /// (e.g., "anthropic" → "claude", others stay as-is).
    pub fn config_spec(&self) -> String {
        let prefix = match self.provider.as_str() {
            "anthropic" => "claude",
            other => other,
        };
        format!("{}:{}", prefix, self.short_name())
    }

    /// Whether this model supports tool use (function calling).
    pub fn supports_tool_use(&self) -> bool {
        self.capabilities.iter().any(|c| c == "tool_use")
    }
}

fn default_provider() -> String {
    "openrouter".to_string()
}

/// The full model registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRegistry {
    /// The default model ID for the coordinator
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    /// All registered models, keyed by model ID
    pub models: BTreeMap<String, ModelEntry>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl ModelRegistry {
    /// Create a registry with the default model catalog
    pub fn with_defaults() -> Self {
        let mut models = BTreeMap::new();

        let defaults = vec![
            ModelEntry {
                id: "anthropic/claude-opus-4-6".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 5.0,
                cost_per_1m_output: 25.0,
                context_window: 1_000_000,
                capabilities: vec![
                    "coding".into(),
                    "analysis".into(),
                    "creative".into(),
                    "reasoning".into(),
                    "tool_use".into(),
                ],
                tier: ModelTier::Frontier,
            },
            ModelEntry {
                id: "anthropic/claude-sonnet-4-6".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 3.0,
                cost_per_1m_output: 15.0,
                context_window: 1_000_000,
                capabilities: vec![
                    "coding".into(),
                    "analysis".into(),
                    "creative".into(),
                    "tool_use".into(),
                ],
                tier: ModelTier::Mid,
            },
            ModelEntry {
                id: "anthropic/claude-haiku-4-5".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 0.80,
                cost_per_1m_output: 4.0,
                context_window: 200_000,
                capabilities: vec!["coding".into(), "analysis".into(), "tool_use".into()],
                tier: ModelTier::Budget,
            },
            ModelEntry {
                id: "openai/gpt-4o".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 2.50,
                cost_per_1m_output: 10.0,
                context_window: 128_000,
                capabilities: vec![
                    "coding".into(),
                    "analysis".into(),
                    "creative".into(),
                    "tool_use".into(),
                ],
                tier: ModelTier::Mid,
            },
            ModelEntry {
                id: "openai/gpt-4o-mini".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 0.15,
                cost_per_1m_output: 0.60,
                context_window: 128_000,
                capabilities: vec!["coding".into(), "analysis".into(), "tool_use".into()],
                tier: ModelTier::Budget,
            },
            ModelEntry {
                id: "openai/o3".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 2.0,
                cost_per_1m_output: 8.0,
                context_window: 200_000,
                capabilities: vec![
                    "coding".into(),
                    "analysis".into(),
                    "reasoning".into(),
                    "tool_use".into(),
                ],
                tier: ModelTier::Frontier,
            },
            ModelEntry {
                id: "google/gemini-2.5-pro".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 1.25,
                cost_per_1m_output: 10.0,
                context_window: 1_000_000,
                capabilities: vec![
                    "coding".into(),
                    "analysis".into(),
                    "creative".into(),
                    "reasoning".into(),
                    "tool_use".into(),
                ],
                tier: ModelTier::Mid,
            },
            ModelEntry {
                id: "google/gemini-2.0-flash".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 0.10,
                cost_per_1m_output: 0.40,
                context_window: 1_000_000,
                capabilities: vec!["coding".into(), "analysis".into(), "tool_use".into()],
                tier: ModelTier::Budget,
            },
            ModelEntry {
                id: "deepseek/deepseek-chat".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 0.30,
                cost_per_1m_output: 0.88,
                context_window: 164_000,
                capabilities: vec!["coding".into(), "analysis".into(), "tool_use".into()],
                tier: ModelTier::Budget,
            },
            ModelEntry {
                id: "deepseek/deepseek-r1".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 0.55,
                cost_per_1m_output: 2.19,
                context_window: 164_000,
                capabilities: vec!["coding".into(), "analysis".into(), "reasoning".into()],
                tier: ModelTier::Mid,
            },
            ModelEntry {
                id: "meta-llama/llama-4-maverick".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 0.20,
                cost_per_1m_output: 0.60,
                context_window: 1_000_000,
                capabilities: vec!["coding".into(), "analysis".into(), "tool_use".into()],
                tier: ModelTier::Budget,
            },
            ModelEntry {
                id: "meta-llama/llama-4-scout".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 0.10,
                cost_per_1m_output: 0.30,
                context_window: 512_000,
                capabilities: vec!["coding".into(), "analysis".into(), "tool_use".into()],
                tier: ModelTier::Budget,
            },
            ModelEntry {
                id: "qwen/qwen3-235b-a22b".into(),
                provider: "openrouter".into(),
                cost_per_1m_input: 0.20,
                cost_per_1m_output: 0.60,
                context_window: 131_072,
                capabilities: vec![
                    "coding".into(),
                    "analysis".into(),
                    "reasoning".into(),
                    "tool_use".into(),
                ],
                tier: ModelTier::Budget,
            },
        ];

        for entry in defaults {
            models.insert(entry.id.clone(), entry);
        }

        Self {
            default_model: None,
            models,
        }
    }

    /// Load registry from .workgraph/models.yaml, creating defaults if missing
    pub fn load(workgraph_dir: &Path) -> Result<Self> {
        let path = workgraph_dir.join("models.yaml");

        if !path.exists() {
            return Ok(Self::with_defaults());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;

        let registry: ModelRegistry = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;

        Ok(registry)
    }

    /// Save registry to .workgraph/models.yaml
    pub fn save(&self, workgraph_dir: &Path) -> Result<()> {
        let path = workgraph_dir.join("models.yaml");

        let content = serde_yaml::to_string(self).context("Failed to serialize model registry")?;

        fs::write(&path, content).with_context(|| format!("Failed to write {}", path.display()))?;

        Ok(())
    }

    /// Get a model by ID
    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.get(id)
    }

    /// Find a model by its ID (alias for get).
    pub fn find_by_model(&self, model: &str) -> Option<&ModelEntry> {
        self.models.get(model)
    }

    /// Get the default model entry
    pub fn get_default(&self) -> Option<&ModelEntry> {
        self.default_model
            .as_ref()
            .and_then(|id| self.models.get(id))
    }

    /// Set the default model, returning an error if the model isn't in the registry
    pub fn set_default(&mut self, id: &str) -> Result<()> {
        if !self.models.contains_key(id) {
            anyhow::bail!(
                "Model '{}' not found in registry.\n  \
                 Try: `wg models search {}` to find valid alternatives\n  \
                 Or:  `wg models list` to see the local registry\n  \
                 Tip: `openrouter/auto` is a safe default that auto-routes to the best model.",
                id,
                id.split('/').next_back().unwrap_or(id),
            );
        }
        self.default_model = Some(id.to_string());
        Ok(())
    }

    /// Add or update a model entry
    pub fn add(&mut self, entry: ModelEntry) {
        self.models.insert(entry.id.clone(), entry);
    }

    /// Check if a model supports tool use by its ID.
    ///
    /// Returns `true` if the model has the `tool_use` capability, or if the
    /// model is not in the registry (unknown models default to tool support
    /// to avoid silently breaking existing behavior).
    pub fn supports_tool_use(&self, model_id: &str) -> bool {
        match self.models.get(model_id) {
            Some(entry) => entry.supports_tool_use(),
            None => true, // Unknown models default to tool support
        }
    }

    /// List all models, optionally filtered by tier
    pub fn list(&self, tier: Option<&ModelTier>) -> Vec<&ModelEntry> {
        self.models
            .values()
            .filter(|m| tier.is_none_or(|t| &m.tier == t))
            .collect()
    }

    /// Return short model names for config choice lists, ordered by tier.
    pub fn model_choices(&self) -> Vec<String> {
        let tier_order = |t: &ModelTier| -> u8 {
            match t {
                ModelTier::Frontier => 0,
                ModelTier::Mid => 1,
                ModelTier::Budget => 2,
            }
        };
        let mut entries: Vec<&ModelEntry> = self.models.values().collect();
        entries.sort_by_key(|e| (tier_order(&e.tier), e.id.clone()));
        entries.iter().map(|e| e.config_spec()).collect()
    }

    /// Return (config_spec, description) pairs for setup wizard display.
    pub fn model_choices_with_descriptions(&self) -> Vec<(String, String)> {
        let tier_order = |t: &ModelTier| -> u8 {
            match t {
                ModelTier::Frontier => 0,
                ModelTier::Mid => 1,
                ModelTier::Budget => 2,
            }
        };
        let mut entries: Vec<&ModelEntry> = self.models.values().collect();
        entries.sort_by_key(|e| (tier_order(&e.tier), e.id.clone()));
        entries
            .iter()
            .map(|e| {
                let desc = format!("{} tier, {}", e.tier, e.provider);
                (e.config_spec(), desc)
            })
            .collect()
    }
}

/// Load model choices from registry, falling back to defaults if unavailable.
pub fn load_model_choices(workgraph_dir: &std::path::Path) -> Vec<String> {
    ModelRegistry::load(workgraph_dir)
        .map(|r| r.model_choices())
        .unwrap_or_else(|_| {
            vec![
                "claude:opus".into(),
                "claude:sonnet".into(),
                "claude:haiku".into(),
            ]
        })
}

/// Load model choices with descriptions, falling back to defaults.
pub fn load_model_choices_with_descriptions(
    workgraph_dir: &std::path::Path,
) -> Vec<(String, String)> {
    ModelRegistry::load(workgraph_dir)
        .map(|r| r.model_choices_with_descriptions())
        .unwrap_or_else(|_| {
            vec![
                (
                    "claude:opus".into(),
                    "Most capable, best for complex tasks".into(),
                ),
                (
                    "claude:sonnet".into(),
                    "Balanced capability and speed".into(),
                ),
                (
                    "claude:haiku".into(),
                    "Fastest, best for simple tasks".into(),
                ),
            ]
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const OPENROUTER_OPUS_ID: &str = "anthropic/claude-opus-4-6";

    #[test]
    fn test_default_registry_has_models() {
        let reg = ModelRegistry::with_defaults();
        assert!(reg.models.len() >= 10);
        assert!(reg.models.contains_key(OPENROUTER_OPUS_ID));
        assert!(reg.models.contains_key("openai/gpt-4o"));
        assert!(reg.models.contains_key("deepseek/deepseek-chat"));
    }

    #[test]
    fn test_tier_roundtrip() {
        assert_eq!(
            "frontier".parse::<ModelTier>().unwrap(),
            ModelTier::Frontier
        );
        assert_eq!("mid".parse::<ModelTier>().unwrap(), ModelTier::Mid);
        assert_eq!("budget".parse::<ModelTier>().unwrap(), ModelTier::Budget);
        assert!("unknown".parse::<ModelTier>().is_err());
    }

    #[test]
    fn test_tier_display() {
        assert_eq!(ModelTier::Frontier.to_string(), "frontier");
        assert_eq!(ModelTier::Mid.to_string(), "mid");
        assert_eq!(ModelTier::Budget.to_string(), "budget");
    }

    #[test]
    fn test_save_and_load() {
        let dir = TempDir::new().unwrap();
        let reg = ModelRegistry::with_defaults();
        reg.save(dir.path()).unwrap();

        let loaded = ModelRegistry::load(dir.path()).unwrap();
        assert_eq!(loaded.models.len(), reg.models.len());
        assert!(loaded.models.contains_key(OPENROUTER_OPUS_ID));
    }

    #[test]
    fn test_load_missing_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let reg = ModelRegistry::load(dir.path()).unwrap();
        assert!(reg.models.len() >= 10);
    }

    #[test]
    fn test_set_default() {
        let mut reg = ModelRegistry::with_defaults();
        assert!(reg.default_model.is_none());

        reg.set_default("openai/gpt-4o").unwrap();
        assert_eq!(reg.default_model.as_deref(), Some("openai/gpt-4o"));

        assert!(reg.set_default("nonexistent/model").is_err());
    }

    #[test]
    fn test_get_default() {
        let mut reg = ModelRegistry::with_defaults();
        assert!(reg.get_default().is_none());

        reg.set_default("openai/gpt-4o").unwrap();
        let model = reg.get_default().unwrap();
        assert_eq!(model.id, "openai/gpt-4o");
    }

    #[test]
    fn test_add_model() {
        let mut reg = ModelRegistry::with_defaults();
        let count = reg.models.len();

        reg.add(ModelEntry {
            id: "custom/my-model".into(),
            provider: "custom".into(),
            cost_per_1m_input: 1.0,
            cost_per_1m_output: 2.0,
            context_window: 32_000,
            capabilities: vec!["coding".into()],
            tier: ModelTier::Mid,
        });

        assert_eq!(reg.models.len(), count + 1);
        assert!(reg.models.contains_key("custom/my-model"));
    }

    #[test]
    fn test_list_filter_by_tier() {
        let reg = ModelRegistry::with_defaults();

        let frontier = reg.list(Some(&ModelTier::Frontier));
        assert!(frontier.iter().all(|m| m.tier == ModelTier::Frontier));
        assert!(!frontier.is_empty());

        let all = reg.list(None);
        assert_eq!(all.len(), reg.models.len());
    }

    #[test]
    fn test_yaml_roundtrip() {
        let mut reg = ModelRegistry::with_defaults();
        reg.set_default(OPENROUTER_OPUS_ID).unwrap();

        let yaml = serde_yaml::to_string(&reg).unwrap();
        let parsed: ModelRegistry = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.default_model, reg.default_model);
        assert_eq!(parsed.models.len(), reg.models.len());
    }

    #[test]
    fn test_supports_tool_use() {
        let reg = ModelRegistry::with_defaults();

        // Models with tool_use capability
        assert!(reg.supports_tool_use(OPENROUTER_OPUS_ID));
        assert!(reg.supports_tool_use("openai/gpt-4o"));
        assert!(reg.supports_tool_use("google/gemini-2.5-pro"));
        assert!(reg.supports_tool_use("deepseek/deepseek-chat"));

        // DeepSeek R1 reasoning model does NOT support tools
        assert!(!reg.supports_tool_use("deepseek/deepseek-r1"));

        // Unknown models default to true
        assert!(reg.supports_tool_use("unknown/model-xyz"));
    }

    #[test]
    fn test_model_entry_supports_tool_use() {
        let with_tools = ModelEntry {
            id: "test/with-tools".into(),
            provider: "test".into(),
            cost_per_1m_input: 1.0,
            cost_per_1m_output: 2.0,
            context_window: 32_000,
            capabilities: vec!["coding".into(), "tool_use".into()],
            tier: ModelTier::Mid,
        };
        assert!(with_tools.supports_tool_use());

        let without_tools = ModelEntry {
            id: "test/no-tools".into(),
            provider: "test".into(),
            cost_per_1m_input: 1.0,
            cost_per_1m_output: 2.0,
            context_window: 32_000,
            capabilities: vec!["coding".into(), "reasoning".into()],
            tier: ModelTier::Mid,
        };
        assert!(!without_tools.supports_tool_use());
    }

    #[test]
    fn test_model_pricing_sanity() {
        let reg = ModelRegistry::with_defaults();
        for model in reg.models.values() {
            assert!(
                model.cost_per_1m_input >= 0.0,
                "Negative input cost for {}",
                model.id
            );
            assert!(
                model.cost_per_1m_output >= 0.0,
                "Negative output cost for {}",
                model.id
            );
            assert!(
                model.context_window > 0,
                "Zero context window for {}",
                model.id
            );
        }
    }
}
