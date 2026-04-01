//! Provider profiles: named configurations that map quality tiers to models.
//!
//! A profile supplies default tier→model mappings. Static profiles have hardcoded
//! mappings; dynamic profiles (like `openrouter`) resolve at runtime via the
//! benchmark registry. Explicit `[tiers]` entries and per-role `[models]` overrides
//! always take precedence over profile defaults.

use crate::config::TierConfig;

/// A provider profile: a named configuration that maps quality tiers to models.
#[derive(Debug, Clone)]
pub struct Profile {
    /// Unique identifier (e.g., "anthropic", "openrouter", "openai")
    pub name: &'static str,
    /// Human-readable description
    pub description: &'static str,
    /// How tier mappings are determined
    pub strategy: ProfileStrategy,
}

/// How a profile determines its tier→model mappings.
#[derive(Debug, Clone)]
pub enum ProfileStrategy {
    /// Hardcoded tier → model mappings
    Static { tiers: TierConfig },
    /// Dynamic: consult benchmark registry with a ranking algorithm.
    /// Implementation deferred to profile-openrouter task.
    Dynamic {
        /// Short description of the dynamic strategy
        description: &'static str,
    },
}

impl Profile {
    /// Resolve this profile's tier mappings.
    /// For static profiles, returns the hardcoded tiers.
    /// For dynamic profiles, returns None (caller should use fallback/cache).
    pub fn resolve_tiers(&self) -> Option<TierConfig> {
        match &self.strategy {
            ProfileStrategy::Static { tiers } => Some(tiers.clone()),
            ProfileStrategy::Dynamic { .. } => None,
        }
    }

    /// Whether this profile is static (hardcoded mappings).
    pub fn is_static(&self) -> bool {
        matches!(self.strategy, ProfileStrategy::Static { .. })
    }

    /// Whether this profile is dynamic (runtime resolution).
    pub fn is_dynamic(&self) -> bool {
        matches!(self.strategy, ProfileStrategy::Dynamic { .. })
    }

    /// Human-readable strategy label.
    pub fn strategy_label(&self) -> &'static str {
        match self.strategy {
            ProfileStrategy::Static { .. } => "static",
            ProfileStrategy::Dynamic { .. } => "dynamic",
        }
    }
}

/// Return all built-in profiles.
pub fn builtin_profiles() -> Vec<Profile> {
    vec![
        Profile {
            name: "anthropic",
            description: "Anthropic Claude models via Claude CLI",
            strategy: ProfileStrategy::Static {
                tiers: TierConfig {
                    fast: Some("claude:haiku".into()),
                    standard: Some("claude:sonnet".into()),
                    premium: Some("claude:opus".into()),
                },
            },
        },
        Profile {
            name: "openrouter",
            description: "Auto-select best OpenRouter models by usage and benchmarks",
            strategy: ProfileStrategy::Dynamic {
                description:
                    "Queries benchmark registry to rank models by pricing and reliability",
            },
        },
        Profile {
            name: "openai",
            description: "OpenAI models via OpenRouter",
            strategy: ProfileStrategy::Static {
                tiers: TierConfig {
                    fast: Some("openrouter:openai/gpt-4o-mini".into()),
                    standard: Some("openrouter:openai/gpt-4o".into()),
                    premium: Some("openrouter:openai/o3-pro".into()),
                },
            },
        },
    ]
}

/// Look up a built-in profile by name.
pub fn get_profile(name: &str) -> Option<Profile> {
    builtin_profiles().into_iter().find(|p| p.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_profiles_exist() {
        let profiles = builtin_profiles();
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].name, "anthropic");
        assert_eq!(profiles[1].name, "openrouter");
        assert_eq!(profiles[2].name, "openai");
    }

    #[test]
    fn test_anthropic_profile_is_static() {
        let profile = get_profile("anthropic").unwrap();
        assert!(profile.is_static());
        let tiers = profile.resolve_tiers().unwrap();
        assert_eq!(tiers.fast.as_deref(), Some("claude:haiku"));
        assert_eq!(tiers.standard.as_deref(), Some("claude:sonnet"));
        assert_eq!(tiers.premium.as_deref(), Some("claude:opus"));
    }

    #[test]
    fn test_openrouter_profile_is_dynamic() {
        let profile = get_profile("openrouter").unwrap();
        assert!(profile.is_dynamic());
        assert!(profile.resolve_tiers().is_none());
    }

    #[test]
    fn test_openai_profile_is_static() {
        let profile = get_profile("openai").unwrap();
        assert!(profile.is_static());
        let tiers = profile.resolve_tiers().unwrap();
        assert_eq!(tiers.fast.as_deref(), Some("openrouter:openai/gpt-4o-mini"));
        assert_eq!(tiers.standard.as_deref(), Some("openrouter:openai/gpt-4o"));
        assert_eq!(tiers.premium.as_deref(), Some("openrouter:openai/o3-pro"));
    }

    #[test]
    fn test_unknown_profile_returns_none() {
        assert!(get_profile("nonexistent").is_none());
    }

    #[test]
    fn test_strategy_labels() {
        let anthropic = get_profile("anthropic").unwrap();
        assert_eq!(anthropic.strategy_label(), "static");
        let openrouter = get_profile("openrouter").unwrap();
        assert_eq!(openrouter.strategy_label(), "dynamic");
    }
}
