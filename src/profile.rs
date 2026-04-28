//! Provider profiles: named configurations that map quality tiers to models.
//!
//! A profile supplies default tier→model mappings. Static profiles have hardcoded
//! mappings; dynamic profiles (like `openrouter`) resolve at runtime via the
//! benchmark registry. Explicit `[tiers]` entries and per-role `[models]` overrides
//! always take precedence over profile defaults.

use crate::config::{Tier, TierConfig};
use crate::model_benchmarks::RankedTiers;
use anyhow::{Context, Result};
use std::path::Path;

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
                description: "Queries benchmark registry to rank models by pricing and reliability",
            },
        },
        Profile {
            name: "openrouter-open",
            description: "OpenRouter open-weight models only",
            strategy: ProfileStrategy::Static {
                tiers: TierConfig {
                    fast: Some("openrouter:deepseek/deepseek-v3.2".into()),
                    standard: Some("openrouter:qwen/qwen3-coder".into()),
                    premium: Some("openrouter:qwen/qwen3.5-397b-a17b".into()),
                },
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
        Profile {
            name: "codex",
            description: "OpenAI Codex CLI models via Codex executor",
            strategy: ProfileStrategy::Static {
                tiers: TierConfig {
                    fast: Some("codex:gpt-5.4-mini".into()),
                    // gpt-5-codex sunsets 2026-07-23; gpt-5.4 is the CLI default as of v0.124.0
                    standard: Some("codex:gpt-5.4".into()),
                    // gpt-5.5 (released 2026-04-23) supersedes gpt-5.4-pro at lower cost
                    premium: Some("codex:gpt-5.5".into()),
                },
            },
        },
    ]
}

/// Look up a built-in profile by name.
pub fn get_profile(name: &str) -> Option<Profile> {
    builtin_profiles().into_iter().find(|p| p.name == name)
}

/// File name for the cached ranked tiers (inside .workgraph/).
const RANKED_TIERS_FILE: &str = "profile_ranked_tiers.json";

/// Load ranked tiers from `.workgraph/profile_ranked_tiers.json`.
pub fn load_ranked_tiers(dir: &Path) -> Result<Option<RankedTiers>> {
    let path = dir.join(RANKED_TIERS_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let ranked: RankedTiers = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(Some(ranked))
}

/// Result of a model escalation attempt.
#[derive(Debug, Clone)]
pub struct EscalationResult {
    /// The next model to try (with provider prefix, e.g. "openrouter:google/gemini-2.0-flash-001").
    pub model: String,
    /// Human-readable reason for the escalation.
    pub reason: String,
}

/// Tier escalation order: fast → standard → premium.
fn tier_escalation_order() -> Vec<Tier> {
    vec![Tier::Fast, Tier::Standard, Tier::Premium]
}

/// Get the tier name string for a `Tier` variant.
fn tier_name(tier: &Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Standard => "standard",
        Tier::Premium => "premium",
    }
}

/// Get the ranked list for a given tier from `RankedTiers`.
fn ranked_for_tier<'a>(
    ranked: &'a RankedTiers,
    tier: &Tier,
) -> &'a [crate::model_benchmarks::RankedModel] {
    match tier {
        Tier::Fast => &ranked.fast,
        Tier::Standard => &ranked.standard,
        Tier::Premium => &ranked.premium,
    }
}

/// Attempt to escalate to the next model in the ranked tier list.
///
/// Given a list of already-tried models, finds the next untried model in the
/// current tier's ranked list. If the current tier is exhausted, escalates to the
/// next tier up (fast → standard → premium), up to `max_escalation_depth` tiers.
///
/// Returns `None` if:
/// - The profile is static (no ranked lists)
/// - No ranked tiers file exists
/// - All models in all tiers up to max_escalation_depth have been tried
pub fn escalate_model(
    dir: &Path,
    profile_name: Option<&str>,
    current_model: Option<&str>,
    tried_models: &[String],
    max_escalation_depth: u32,
) -> Option<EscalationResult> {
    // Only escalate for dynamic profiles
    let profile = profile_name.and_then(get_profile)?;
    if profile.is_static() {
        return None;
    }

    let ranked = load_ranked_tiers(dir).ok()??;

    // Determine the starting tier from the current model
    let starting_tier = current_model
        .and_then(|m| find_model_tier(&ranked, m))
        .unwrap_or(Tier::Fast);

    let escalation_order = tier_escalation_order();
    let start_idx = escalation_order
        .iter()
        .position(|t| *t == starting_tier)
        .unwrap_or(0);

    // Walk tiers from starting tier upward, limited by max_escalation_depth
    let max_tiers = if max_escalation_depth == 0 {
        1 // Only within current tier
    } else {
        max_escalation_depth as usize
    };

    for tier_offset in 0..max_tiers.min(escalation_order.len()) {
        let tier_idx = start_idx + tier_offset;
        if tier_idx >= escalation_order.len() {
            break;
        }
        let tier = &escalation_order[tier_idx];
        let candidates = ranked_for_tier(&ranked, tier);

        for (rank, candidate) in candidates.iter().enumerate() {
            let prefixed_id = format!("openrouter:{}", candidate.id);
            // Skip models already tried (check both raw and prefixed forms)
            if tried_models
                .iter()
                .any(|t| t == &candidate.id || t == &prefixed_id)
            {
                continue;
            }
            let is_same_tier = tier_offset == 0;
            let reason = if is_same_tier {
                format!("rank {} in {}-class", rank + 1, tier_name(tier),)
            } else {
                format!(
                    "escalated to {}-class rank {} (exhausted {}-class)",
                    tier_name(tier),
                    rank + 1,
                    tier_name(&escalation_order[start_idx]),
                )
            };
            return Some(EscalationResult {
                model: prefixed_id,
                reason,
            });
        }
    }

    None
}

/// Find which tier a model belongs to in the ranked lists.
fn find_model_tier(ranked: &RankedTiers, model: &str) -> Option<Tier> {
    // Strip "openrouter:" prefix if present for matching
    let bare = model.strip_prefix("openrouter:").unwrap_or(model);

    for tier in tier_escalation_order() {
        let candidates = ranked_for_tier(ranked, &tier);
        if candidates.iter().any(|c| c.id == bare || c.id == model) {
            return Some(tier);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_profiles_exist() {
        let profiles = builtin_profiles();
        assert_eq!(profiles.len(), 5);
        assert_eq!(profiles[0].name, "anthropic");
        assert_eq!(profiles[1].name, "openrouter");
        assert_eq!(profiles[2].name, "openrouter-open");
        assert_eq!(profiles[3].name, "openai");
        assert_eq!(profiles[4].name, "codex");
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
    fn test_openrouter_open_profile_is_static() {
        let profile = get_profile("openrouter-open").unwrap();
        assert!(profile.is_static());
        let tiers = profile.resolve_tiers().unwrap();
        assert_eq!(
            tiers.fast.as_deref(),
            Some("openrouter:deepseek/deepseek-v3.2")
        );
        assert_eq!(
            tiers.standard.as_deref(),
            Some("openrouter:qwen/qwen3-coder")
        );
        assert_eq!(
            tiers.premium.as_deref(),
            Some("openrouter:qwen/qwen3.5-397b-a17b")
        );
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
    fn test_codex_profile_is_static() {
        let profile = get_profile("codex").unwrap();
        assert!(profile.is_static());
        let tiers = profile.resolve_tiers().unwrap();
        assert_eq!(tiers.fast.as_deref(), Some("codex:gpt-5.4-mini"));
        assert_eq!(tiers.standard.as_deref(), Some("codex:gpt-5.4"));
        assert_eq!(tiers.premium.as_deref(), Some("codex:gpt-5.5"));
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

    // ── Escalation tests ───────────────────────────────────────────────

    use crate::model_benchmarks::{RankedModel, RankedTiers};
    use tempfile::TempDir;

    /// Write a RankedTiers fixture to the temp dir so escalate_model can load it.
    fn write_ranked_tiers(dir: &std::path::Path, ranked: &RankedTiers) {
        let path = dir.join(RANKED_TIERS_FILE);
        let json = serde_json::to_string(ranked).unwrap();
        std::fs::write(path, json).unwrap();
    }

    fn make_ranked_model(id: &str, tier: &str) -> RankedModel {
        RankedModel {
            id: id.to_string(),
            name: id.to_string(),
            popularity_score: 80.0,
            benchmark_score: 70.0,
            composite_score: 75.0,
            tier: tier.to_string(),
            input_per_mtok: None,
            output_per_mtok: None,
            context_window: None,
            supports_tools: true,
            is_curated: true,
        }
    }

    fn sample_ranked_tiers() -> RankedTiers {
        RankedTiers {
            fast: vec![
                make_ranked_model("vendor/fast-a", "fast"),
                make_ranked_model("vendor/fast-b", "fast"),
            ],
            standard: vec![
                make_ranked_model("vendor/std-a", "standard"),
                make_ranked_model("vendor/std-b", "standard"),
                make_ranked_model("vendor/std-c", "standard"),
            ],
            premium: vec![make_ranked_model("vendor/prem-a", "premium")],
        }
    }

    #[test]
    fn test_escalate_static_profile_returns_none() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());
        // Static profiles should never escalate
        let result = escalate_model(tmp.path(), Some("anthropic"), Some("claude:sonnet"), &[], 3);
        assert!(result.is_none());
    }

    #[test]
    fn test_escalate_no_profile_returns_none() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());
        let result = escalate_model(tmp.path(), None, Some("openrouter:vendor/std-a"), &[], 3);
        assert!(result.is_none());
    }

    #[test]
    fn test_escalate_no_ranked_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        // Don't write the file
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/std-a"),
            &[],
            3,
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_escalate_picks_first_untried_in_same_tier() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // Current model is std-a, no models tried yet → should get std-a (rank 1)
        // because std-a hasn't been recorded as tried
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/std-a"),
            &[],
            3,
        );
        let r = result.unwrap();
        assert_eq!(r.model, "openrouter:vendor/std-a");
        assert!(r.reason.contains("rank 1"));
        assert!(r.reason.contains("standard-class"));
    }

    #[test]
    fn test_escalate_skips_tried_model() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // std-a already tried → should get std-b
        let tried = vec!["openrouter:vendor/std-a".to_string()];
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/std-a"),
            &tried,
            3,
        );
        let r = result.unwrap();
        assert_eq!(r.model, "openrouter:vendor/std-b");
        assert!(r.reason.contains("rank 2"));
        assert!(r.reason.contains("standard-class"));
    }

    #[test]
    fn test_escalate_rotates_through_full_tier() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // std-a and std-b tried → should get std-c
        let tried = vec![
            "openrouter:vendor/std-a".to_string(),
            "openrouter:vendor/std-b".to_string(),
        ];
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/std-a"),
            &tried,
            3,
        );
        let r = result.unwrap();
        assert_eq!(r.model, "openrouter:vendor/std-c");
        assert!(r.reason.contains("rank 3"));
    }

    #[test]
    fn test_escalate_to_next_tier_when_current_exhausted() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // All standard models tried → should escalate to premium
        let tried = vec![
            "openrouter:vendor/std-a".to_string(),
            "openrouter:vendor/std-b".to_string(),
            "openrouter:vendor/std-c".to_string(),
        ];
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/std-a"),
            &tried,
            3,
        );
        let r = result.unwrap();
        assert_eq!(r.model, "openrouter:vendor/prem-a");
        assert!(r.reason.contains("escalated to premium-class"));
        assert!(r.reason.contains("exhausted standard-class"));
    }

    #[test]
    fn test_escalate_returns_none_when_all_exhausted() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // All standard and premium models tried
        let tried = vec![
            "openrouter:vendor/std-a".to_string(),
            "openrouter:vendor/std-b".to_string(),
            "openrouter:vendor/std-c".to_string(),
            "openrouter:vendor/prem-a".to_string(),
        ];
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/std-a"),
            &tried,
            3,
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_escalate_depth_zero_stays_in_same_tier() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // All standard models tried, depth=0 → no tier escalation
        let tried = vec![
            "openrouter:vendor/std-a".to_string(),
            "openrouter:vendor/std-b".to_string(),
            "openrouter:vendor/std-c".to_string(),
        ];
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/std-a"),
            &tried,
            0, // no escalation
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_escalate_depth_one_allows_one_tier_only() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // Starting from fast tier, depth=1 → only fast tier, no escalation to standard
        let tried = vec![
            "openrouter:vendor/fast-a".to_string(),
            "openrouter:vendor/fast-b".to_string(),
        ];
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/fast-a"),
            &tried,
            1,
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_escalate_depth_two_allows_one_tier_up() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // Starting from fast, depth=2 → fast + standard
        let tried = vec![
            "openrouter:vendor/fast-a".to_string(),
            "openrouter:vendor/fast-b".to_string(),
        ];
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/fast-a"),
            &tried,
            2,
        );
        let r = result.unwrap();
        assert_eq!(r.model, "openrouter:vendor/std-a");
        assert!(r.reason.contains("escalated to standard-class"));
    }

    #[test]
    fn test_escalate_bare_model_id_matches_tried() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // tried_models uses bare IDs (without "openrouter:" prefix)
        let tried = vec!["vendor/std-a".to_string()];
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:vendor/std-a"),
            &tried,
            3,
        );
        let r = result.unwrap();
        // Should skip std-a (matched by bare ID) and return std-b
        assert_eq!(r.model, "openrouter:vendor/std-b");
    }

    #[test]
    fn test_escalate_unknown_current_model_starts_from_fast() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        // Unknown model → defaults to fast tier
        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            Some("openrouter:unknown/model"),
            &[],
            3,
        );
        let r = result.unwrap();
        assert_eq!(r.model, "openrouter:vendor/fast-a");
        assert!(r.reason.contains("fast-class"));
    }

    #[test]
    fn test_escalate_no_current_model_starts_from_fast() {
        let tmp = TempDir::new().unwrap();
        write_ranked_tiers(tmp.path(), &sample_ranked_tiers());

        let result = escalate_model(
            tmp.path(),
            Some("openrouter"),
            None, // no current model
            &[],
            3,
        );
        let r = result.unwrap();
        assert_eq!(r.model, "openrouter:vendor/fast-a");
    }

    #[test]
    fn test_find_model_tier_with_prefix() {
        let ranked = sample_ranked_tiers();
        assert_eq!(
            find_model_tier(&ranked, "openrouter:vendor/std-a"),
            Some(Tier::Standard)
        );
    }

    #[test]
    fn test_find_model_tier_without_prefix() {
        let ranked = sample_ranked_tiers();
        assert_eq!(find_model_tier(&ranked, "vendor/fast-a"), Some(Tier::Fast));
    }

    #[test]
    fn test_find_model_tier_unknown() {
        let ranked = sample_ranked_tiers();
        assert_eq!(find_model_tier(&ranked, "unknown/model"), None);
    }
}
