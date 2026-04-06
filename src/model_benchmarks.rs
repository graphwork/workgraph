//! Model benchmark registry with fitness scoring.
//!
//! Stores benchmark data, pricing, and computed fitness scores fetched from
//! the OpenRouter API. Lives in `.workgraph/model_benchmarks.json` as a
//! machine-managed sidecar to the static `models.yaml` catalog.
//!
//! See `docs/plans/model-registry-and-update-trace.md` for the full design.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The benchmark registry file name.
pub const BENCHMARKS_FILE: &str = "model_benchmarks.json";

// ── Schema types ────────────────────────────────────────────────────────

/// Top-level benchmark registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRegistry {
    /// Schema version (currently 1).
    pub version: u32,
    /// ISO 8601 timestamp of when data was last fetched.
    pub fetched_at: String,
    /// Data sources used.
    pub source: RegistrySource,
    /// Per-model benchmark data, keyed by OpenRouter model ID.
    pub models: BTreeMap<String, ModelBenchmark>,
}

/// Data source URLs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrySource {
    pub openrouter_api: String,
}

/// Benchmark + fitness data for a single model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelBenchmark {
    /// OpenRouter model ID (e.g. "anthropic/claude-opus-4-6").
    pub id: String,
    /// Human-readable name.
    pub name: String,

    /// Pricing per million tokens (USD).
    pub pricing: BenchmarkPricing,

    /// Architecture info.
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub supports_tools: bool,

    /// Benchmarks (mostly null until AA integration).
    #[serde(default)]
    pub benchmarks: Benchmarks,

    /// Popularity signals from OpenRouter.
    #[serde(default)]
    pub popularity: Popularity,

    /// Computed fitness.
    #[serde(default)]
    pub fitness: Fitness,

    /// Tier classification (frontier / mid / budget).
    pub tier: String,

    /// When pricing was last updated.
    pub pricing_updated_at: String,

    /// Whether benchmark scores are proxy estimates (true) or curated/measured (false).
    #[serde(default)]
    pub is_proxy: bool,
}

/// Per-million-token pricing in USD.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkPricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<f64>,
}

/// Benchmark scores (nullable — populated by Artificial Analysis or similar).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Benchmarks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence_index: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding_index: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub math_index: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentic: Option<f64>,
}

/// Popularity signals.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Popularity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_count: Option<u32>,
    /// Total completions/requests (from OpenRouter stats or manual annotation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_count: Option<u64>,
    /// Weekly rank by usage on OpenRouter (1 = most used).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_rank: Option<u32>,
}

/// Computed fitness score and components.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Fitness {
    /// Composite score (0–100), null if no benchmarks available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default)]
    pub components: FitnessComponents,
}

/// Individual fitness components.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FitnessComponents {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reliability: Option<f64>,
}

// ── Loading / Saving ────────────────────────────────────────────────────

impl BenchmarkRegistry {
    /// Load the benchmark registry from `.workgraph/model_benchmarks.json`.
    /// Returns `None` if the file doesn't exist.
    pub fn load(workgraph_dir: &Path) -> Result<Option<Self>> {
        let path = workgraph_dir.join(BENCHMARKS_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let registry: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(Some(registry))
    }

    /// Save the benchmark registry to `.workgraph/model_benchmarks.json`.
    pub fn save(&self, workgraph_dir: &Path) -> Result<()> {
        let path = workgraph_dir.join(BENCHMARKS_FILE);
        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize benchmark registry")?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }

    /// Check if the registry data is stale (older than `ttl_hours`).
    pub fn is_stale(&self, ttl_hours: u64) -> bool {
        if let Ok(fetched) = chrono::DateTime::parse_from_rfc3339(&self.fetched_at) {
            let age = chrono::Utc::now().signed_duration_since(fetched);
            age.num_hours() >= ttl_hours as i64
        } else {
            true // Can't parse timestamp → treat as stale
        }
    }

    /// Models sorted by fitness score descending (unscored models last).
    pub fn ranked(&self) -> Vec<&ModelBenchmark> {
        let mut models: Vec<&ModelBenchmark> = self.models.values().collect();
        models.sort_by(|a, b| {
            let sa = a.fitness.score.unwrap_or(f64::NEG_INFINITY);
            let sb = b.fitness.score.unwrap_or(f64::NEG_INFINITY);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        models
    }

    /// Models filtered by tier, sorted by fitness.
    pub fn ranked_by_tier(&self, tier: &str) -> Vec<&ModelBenchmark> {
        let mut models: Vec<&ModelBenchmark> = self
            .models
            .values()
            .filter(|m| m.tier == tier)
            .collect();
        models.sort_by(|a, b| {
            let sa = a.fitness.score.unwrap_or(f64::NEG_INFINITY);
            let sb = b.fitness.score.unwrap_or(f64::NEG_INFINITY);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        models
    }
}

// ── Fitness Scoring ─────────────────────────────────────────────────────

/// Compute fitness scores for all models in the registry.
///
/// Follows the formula from the design doc:
///   fitness = quality * 0.70 + value * 0.20 + reliability * 0.10
pub fn compute_fitness_scores(registry: &mut BenchmarkRegistry) {
    // First pass: compute quality scores and collect cost factors for median.
    let mut cost_factors: Vec<f64> = Vec::new();
    let mut quality_scores: BTreeMap<String, Option<f64>> = BTreeMap::new();

    for (id, model) in &registry.models {
        let quality = compute_quality(&model.benchmarks);
        quality_scores.insert(id.clone(), quality);

        let cost = model.pricing.input_per_mtok * 0.3 + model.pricing.output_per_mtok * 0.7;
        if cost > 0.0 {
            cost_factors.push(cost);
        }
    }

    let median_cost = median(&cost_factors).unwrap_or(1.0);

    // Second pass: compute full fitness.
    for (id, model) in registry.models.iter_mut() {
        let quality = quality_scores.get(id).copied().flatten();

        // Value: quality / cost_factor, normalized to 0–100.
        let raw_cost =
            model.pricing.input_per_mtok * 0.3 + model.pricing.output_per_mtok * 0.7;
        let cost_factor = if median_cost > 0.0 && raw_cost > 0.0 {
            raw_cost / median_cost
        } else {
            1.0
        };
        let value = quality.map(|q| (q / cost_factor).min(100.0));

        // Reliability: provider_count signal + base availability.
        let provider_signal = model
            .popularity
            .provider_count
            .map(|pc| (pc as f64 / 5.0).min(1.0) * 50.0)
            .unwrap_or(0.0);
        // Without request_count data from OpenRouter, we use a simplified reliability.
        let reliability = provider_signal;

        // Composite.
        let score = quality.map(|q| {
            let v = value.unwrap_or(0.0);
            q * 0.70 + v * 0.20 + reliability * 0.10
        });

        model.fitness = Fitness {
            score,
            components: FitnessComponents {
                quality,
                value,
                reliability: Some(reliability),
            },
        };

        // Only reclassify tier when we have benchmark data; otherwise
        // keep the pricing-based tier from build_from_openrouter.
        if model.fitness.score.is_some() {
            model.tier = classify_tier(&model.benchmarks, model.fitness.score);
        }
    }
}

/// Compute the quality component from benchmark scores.
///
/// quality = coding_index * 0.50 + intelligence_index * 0.30 + agentic * 0.20
fn compute_quality(benchmarks: &Benchmarks) -> Option<f64> {
    let coding = benchmarks.coding_index.or_else(|| {
        benchmarks.intelligence_index.map(|ii| ii * 0.9)
    });
    let intelligence = benchmarks.intelligence_index.or_else(|| {
        benchmarks.coding_index.map(|ci| (ci * 1.1).min(100.0))
    });

    match (coding, intelligence, benchmarks.agentic) {
        (Some(c), Some(i), Some(a)) => Some(c * 0.50 + i * 0.30 + a * 0.20),
        (Some(c), Some(i), None) => {
            // Redistribute agentic weight: 55% coding, 45% intelligence.
            Some(c * 0.55 + i * 0.45)
        }
        (None, None, _) => None,
        (Some(c), None, Some(a)) => Some(c * 0.70 + a * 0.30),
        (None, Some(i), Some(a)) => Some(i * 0.60 + a * 0.40),
        (Some(c), None, None) => Some(c),
        (None, Some(i), None) => Some(i),
    }
}

/// Classify a model into a tier based on benchmarks and fitness.
fn classify_tier(benchmarks: &Benchmarks, fitness_score: Option<f64>) -> String {
    let coding = benchmarks.coding_index.unwrap_or(0.0);
    let intelligence = benchmarks.intelligence_index.unwrap_or(0.0);
    let fitness = fitness_score.unwrap_or(0.0);

    if fitness >= 65.0 || (coding >= 48.0 && intelligence >= 50.0) {
        "frontier".to_string()
    } else if fitness >= 40.0 || coding >= 35.0 {
        "mid".to_string()
    } else {
        "budget".to_string()
    }
}

/// Compute the median of a slice.
fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    } else {
        Some(sorted[mid])
    }
}

// ── Curated benchmark data for well-known models ───────────────────────

/// Curated benchmark + popularity data for well-known models.
///
/// Scores are normalized to 0–100 based on public benchmarks (SWE-bench,
/// MMLU, HumanEval, LiveCodeBench, etc.) as of 2025-Q2/Q3. These provide
/// a sensible fallback when the OpenRouter API doesn't supply scores.
///
/// Each entry is keyed by a **prefix** of the OpenRouter model ID so it
/// matches across version suffixes (e.g. "anthropic/claude-sonnet-4" matches
/// "anthropic/claude-sonnet-4-6").
struct CuratedEntry {
    prefix: &'static str,
    benchmarks: Benchmarks,
    popularity: Popularity,
}

fn curated_benchmarks() -> Vec<CuratedEntry> {
    vec![
        // ── Frontier: Anthropic ───────────────────────────────────────
        CuratedEntry {
            prefix: "anthropic/claude-opus-4",
            benchmarks: Benchmarks {
                coding_index: Some(75.0),
                intelligence_index: Some(78.0),
                agentic: Some(80.0),
                math_index: Some(72.0),
            },
            popularity: Popularity {
                weekly_rank: Some(8),
                request_count: Some(500_000),
                provider_count: Some(5),
            },
        },
        CuratedEntry {
            prefix: "anthropic/claude-sonnet-4",
            benchmarks: Benchmarks {
                coding_index: Some(72.0),
                intelligence_index: Some(74.0),
                agentic: Some(76.0),
                math_index: Some(68.0),
            },
            popularity: Popularity {
                weekly_rank: Some(2),
                request_count: Some(2_000_000),
                provider_count: Some(6),
            },
        },
        CuratedEntry {
            prefix: "anthropic/claude-haiku-4",
            benchmarks: Benchmarks {
                coding_index: Some(52.0),
                intelligence_index: Some(55.0),
                agentic: Some(48.0),
                math_index: Some(50.0),
            },
            popularity: Popularity {
                weekly_rank: Some(5),
                request_count: Some(3_000_000),
                provider_count: Some(5),
            },
        },
        CuratedEntry {
            prefix: "anthropic/claude-3.5-sonnet",
            benchmarks: Benchmarks {
                coding_index: Some(65.0),
                intelligence_index: Some(68.0),
                agentic: Some(70.0),
                math_index: Some(62.0),
            },
            popularity: Popularity {
                weekly_rank: Some(4),
                request_count: Some(2_500_000),
                provider_count: Some(6),
            },
        },
        // ── Frontier: OpenAI ──────────────────────────────────────────
        CuratedEntry {
            prefix: "openai/gpt-4o",
            benchmarks: Benchmarks {
                coding_index: Some(62.0),
                intelligence_index: Some(65.0),
                agentic: Some(64.0),
                math_index: Some(60.0),
            },
            popularity: Popularity {
                weekly_rank: Some(3),
                request_count: Some(5_000_000),
                provider_count: Some(7),
            },
        },
        CuratedEntry {
            prefix: "openai/gpt-4.1",
            benchmarks: Benchmarks {
                coding_index: Some(68.0),
                intelligence_index: Some(70.0),
                agentic: Some(72.0),
                math_index: Some(65.0),
            },
            popularity: Popularity {
                weekly_rank: Some(6),
                request_count: Some(800_000),
                provider_count: Some(5),
            },
        },
        CuratedEntry {
            prefix: "openai/o3",
            benchmarks: Benchmarks {
                coding_index: Some(70.0),
                intelligence_index: Some(76.0),
                agentic: Some(65.0),
                math_index: Some(80.0),
            },
            popularity: Popularity {
                weekly_rank: Some(12),
                request_count: Some(300_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "openai/o4-mini",
            benchmarks: Benchmarks {
                coding_index: Some(66.0),
                intelligence_index: Some(68.0),
                agentic: Some(62.0),
                math_index: Some(74.0),
            },
            popularity: Popularity {
                weekly_rank: Some(10),
                request_count: Some(400_000),
                provider_count: Some(4),
            },
        },
        // ── Frontier: Google ──────────────────────────────────────────
        CuratedEntry {
            prefix: "google/gemini-2.5-pro",
            benchmarks: Benchmarks {
                coding_index: Some(70.0),
                intelligence_index: Some(72.0),
                agentic: Some(68.0),
                math_index: Some(75.0),
            },
            popularity: Popularity {
                weekly_rank: Some(7),
                request_count: Some(600_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "google/gemini-2.5-flash",
            benchmarks: Benchmarks {
                coding_index: Some(55.0),
                intelligence_index: Some(58.0),
                agentic: Some(52.0),
                math_index: Some(60.0),
            },
            popularity: Popularity {
                weekly_rank: Some(9),
                request_count: Some(800_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "google/gemini-2.0-flash",
            benchmarks: Benchmarks {
                coding_index: Some(48.0),
                intelligence_index: Some(50.0),
                agentic: Some(45.0),
                math_index: Some(52.0),
            },
            popularity: Popularity {
                weekly_rank: Some(15),
                request_count: Some(400_000),
                provider_count: Some(4),
            },
        },
        // ── Frontier: DeepSeek ────────────────────────────────────────
        CuratedEntry {
            prefix: "deepseek/deepseek-r1",
            benchmarks: Benchmarks {
                coding_index: Some(60.0),
                intelligence_index: Some(68.0),
                agentic: Some(40.0), // no tool use
                math_index: Some(78.0),
            },
            popularity: Popularity {
                weekly_rank: Some(1),
                request_count: Some(8_000_000),
                provider_count: Some(8),
            },
        },
        CuratedEntry {
            prefix: "deepseek/deepseek-chat",
            benchmarks: Benchmarks {
                coding_index: Some(58.0),
                intelligence_index: Some(62.0),
                agentic: Some(55.0),
                math_index: Some(65.0),
            },
            popularity: Popularity {
                weekly_rank: Some(6),
                request_count: Some(1_500_000),
                provider_count: Some(7),
            },
        },
        CuratedEntry {
            prefix: "deepseek/deepseek-v3",
            benchmarks: Benchmarks {
                coding_index: Some(58.0),
                intelligence_index: Some(62.0),
                agentic: Some(55.0),
                math_index: Some(65.0),
            },
            popularity: Popularity {
                weekly_rank: Some(11),
                request_count: Some(700_000),
                provider_count: Some(6),
            },
        },
        // ── Mid-tier: Qwen ────────────────────────────────────────────
        CuratedEntry {
            prefix: "qwen/qwen3-max",
            benchmarks: Benchmarks {
                coding_index: Some(55.0),
                intelligence_index: Some(58.0),
                agentic: Some(50.0),
                math_index: Some(60.0),
            },
            popularity: Popularity {
                weekly_rank: Some(20),
                request_count: Some(200_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "qwen/qwen3-coder",
            benchmarks: Benchmarks {
                coding_index: Some(52.0),
                intelligence_index: Some(48.0),
                agentic: Some(45.0),
                math_index: Some(50.0),
            },
            popularity: Popularity {
                weekly_rank: Some(25),
                request_count: Some(150_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "qwen/qwen3-235b",
            benchmarks: Benchmarks {
                coding_index: Some(56.0),
                intelligence_index: Some(60.0),
                agentic: Some(52.0),
                math_index: Some(62.0),
            },
            popularity: Popularity {
                weekly_rank: Some(18),
                request_count: Some(250_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "qwen/qwen-2.5-coder-32b",
            benchmarks: Benchmarks {
                coding_index: Some(50.0),
                intelligence_index: Some(45.0),
                agentic: Some(42.0),
                math_index: Some(48.0),
            },
            popularity: Popularity {
                weekly_rank: Some(30),
                request_count: Some(100_000),
                provider_count: Some(4),
            },
        },
        // ── Mid-tier: Meta Llama ──────────────────────────────────────
        CuratedEntry {
            prefix: "meta-llama/llama-4-maverick",
            benchmarks: Benchmarks {
                coding_index: Some(48.0),
                intelligence_index: Some(52.0),
                agentic: Some(45.0),
                math_index: Some(50.0),
            },
            popularity: Popularity {
                weekly_rank: Some(14),
                request_count: Some(500_000),
                provider_count: Some(5),
            },
        },
        CuratedEntry {
            prefix: "meta-llama/llama-4-scout",
            benchmarks: Benchmarks {
                coding_index: Some(42.0),
                intelligence_index: Some(46.0),
                agentic: Some(40.0),
                math_index: Some(44.0),
            },
            popularity: Popularity {
                weekly_rank: Some(22),
                request_count: Some(200_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "meta-llama/llama-3.3-70b",
            benchmarks: Benchmarks {
                coding_index: Some(44.0),
                intelligence_index: Some(48.0),
                agentic: Some(42.0),
                math_index: Some(46.0),
            },
            popularity: Popularity {
                weekly_rank: Some(16),
                request_count: Some(600_000),
                provider_count: Some(6),
            },
        },
        // ── Mid-tier: OpenAI small ────────────────────────────────────
        CuratedEntry {
            prefix: "openai/gpt-4o-mini",
            benchmarks: Benchmarks {
                coding_index: Some(50.0),
                intelligence_index: Some(52.0),
                agentic: Some(48.0),
                math_index: Some(50.0),
            },
            popularity: Popularity {
                weekly_rank: Some(4),
                request_count: Some(4_000_000),
                provider_count: Some(6),
            },
        },
        CuratedEntry {
            prefix: "openai/gpt-4.1-mini",
            benchmarks: Benchmarks {
                coding_index: Some(54.0),
                intelligence_index: Some(56.0),
                agentic: Some(52.0),
                math_index: Some(55.0),
            },
            popularity: Popularity {
                weekly_rank: Some(13),
                request_count: Some(350_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "openai/gpt-4.1-nano",
            benchmarks: Benchmarks {
                coding_index: Some(38.0),
                intelligence_index: Some(40.0),
                agentic: Some(35.0),
                math_index: Some(38.0),
            },
            popularity: Popularity {
                weekly_rank: Some(28),
                request_count: Some(120_000),
                provider_count: Some(3),
            },
        },
        // ── Budget: Mistral ───────────────────────────────────────────
        CuratedEntry {
            prefix: "mistralai/mistral-large",
            benchmarks: Benchmarks {
                coding_index: Some(48.0),
                intelligence_index: Some(52.0),
                agentic: Some(46.0),
                math_index: Some(50.0),
            },
            popularity: Popularity {
                weekly_rank: Some(24),
                request_count: Some(180_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "mistralai/mistral-medium",
            benchmarks: Benchmarks {
                coding_index: Some(42.0),
                intelligence_index: Some(45.0),
                agentic: Some(40.0),
                math_index: Some(44.0),
            },
            popularity: Popularity {
                weekly_rank: Some(35),
                request_count: Some(80_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "mistralai/mistral-small",
            benchmarks: Benchmarks {
                coding_index: Some(35.0),
                intelligence_index: Some(38.0),
                agentic: Some(32.0),
                math_index: Some(36.0),
            },
            popularity: Popularity {
                weekly_rank: Some(40),
                request_count: Some(60_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "mistralai/codestral",
            benchmarks: Benchmarks {
                coding_index: Some(52.0),
                intelligence_index: Some(46.0),
                agentic: Some(44.0),
                math_index: Some(48.0),
            },
            popularity: Popularity {
                weekly_rank: Some(26),
                request_count: Some(150_000),
                provider_count: Some(3),
            },
        },
        // ── Budget: xAI ───────────────────────────────────────────────
        CuratedEntry {
            prefix: "x-ai/grok-3",
            benchmarks: Benchmarks {
                coding_index: Some(55.0),
                intelligence_index: Some(58.0),
                agentic: Some(50.0),
                math_index: Some(55.0),
            },
            popularity: Popularity {
                weekly_rank: Some(19),
                request_count: Some(200_000),
                provider_count: Some(2),
            },
        },
        CuratedEntry {
            prefix: "x-ai/grok-2",
            benchmarks: Benchmarks {
                coding_index: Some(45.0),
                intelligence_index: Some(48.0),
                agentic: Some(42.0),
                math_index: Some(46.0),
            },
            popularity: Popularity {
                weekly_rank: Some(32),
                request_count: Some(100_000),
                provider_count: Some(2),
            },
        },
        CuratedEntry {
            prefix: "x-ai/grok-3-mini",
            benchmarks: Benchmarks {
                coding_index: Some(48.0),
                intelligence_index: Some(50.0),
                agentic: Some(44.0),
                math_index: Some(52.0),
            },
            popularity: Popularity {
                weekly_rank: Some(27),
                request_count: Some(150_000),
                provider_count: Some(2),
            },
        },
        // ── Amazon / Nova ────────────────────────────────────────────
        CuratedEntry {
            prefix: "amazon/nova-pro",
            benchmarks: Benchmarks {
                coding_index: Some(42.0),
                intelligence_index: Some(45.0),
                agentic: Some(40.0),
                math_index: Some(44.0),
            },
            popularity: Popularity {
                weekly_rank: Some(38),
                request_count: Some(80_000),
                provider_count: Some(2),
            },
        },
        CuratedEntry {
            prefix: "amazon/nova-lite",
            benchmarks: Benchmarks {
                coding_index: Some(32.0),
                intelligence_index: Some(35.0),
                agentic: Some(30.0),
                math_index: Some(34.0),
            },
            popularity: Popularity {
                weekly_rank: Some(45),
                request_count: Some(50_000),
                provider_count: Some(2),
            },
        },
        CuratedEntry {
            prefix: "amazon/nova-micro",
            benchmarks: Benchmarks {
                coding_index: Some(25.0),
                intelligence_index: Some(28.0),
                agentic: Some(22.0),
                math_index: Some(26.0),
            },
            popularity: Popularity {
                weekly_rank: Some(55),
                request_count: Some(30_000),
                provider_count: Some(2),
            },
        },
        // ── Cohere ───────────────────────────────────────────────────
        CuratedEntry {
            prefix: "cohere/command-r-plus",
            benchmarks: Benchmarks {
                coding_index: Some(44.0),
                intelligence_index: Some(48.0),
                agentic: Some(46.0),
                math_index: Some(42.0),
            },
            popularity: Popularity {
                weekly_rank: Some(33),
                request_count: Some(90_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "cohere/command-r",
            benchmarks: Benchmarks {
                coding_index: Some(38.0),
                intelligence_index: Some(42.0),
                agentic: Some(40.0),
                math_index: Some(36.0),
            },
            popularity: Popularity {
                weekly_rank: Some(42),
                request_count: Some(60_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "cohere/command-a",
            benchmarks: Benchmarks {
                coding_index: Some(50.0),
                intelligence_index: Some(52.0),
                agentic: Some(48.0),
                math_index: Some(46.0),
            },
            popularity: Popularity {
                weekly_rank: Some(28),
                request_count: Some(120_000),
                provider_count: Some(3),
            },
        },
        // ── Microsoft ────────────────────────────────────────────────
        CuratedEntry {
            prefix: "microsoft/phi-4",
            benchmarks: Benchmarks {
                coding_index: Some(40.0),
                intelligence_index: Some(42.0),
                agentic: Some(35.0),
                math_index: Some(44.0),
            },
            popularity: Popularity {
                weekly_rank: Some(34),
                request_count: Some(100_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "microsoft/phi-3",
            benchmarks: Benchmarks {
                coding_index: Some(35.0),
                intelligence_index: Some(38.0),
                agentic: Some(30.0),
                math_index: Some(38.0),
            },
            popularity: Popularity {
                weekly_rank: Some(44),
                request_count: Some(50_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "microsoft/mai",
            benchmarks: Benchmarks {
                coding_index: Some(55.0),
                intelligence_index: Some(58.0),
                agentic: Some(52.0),
                math_index: Some(56.0),
            },
            popularity: Popularity {
                weekly_rank: Some(21),
                request_count: Some(180_000),
                provider_count: Some(3),
            },
        },
        // ── Perplexity ───────────────────────────────────────────────
        CuratedEntry {
            prefix: "perplexity/sonar-pro",
            benchmarks: Benchmarks {
                coding_index: Some(42.0),
                intelligence_index: Some(50.0),
                agentic: Some(38.0),
                math_index: Some(44.0),
            },
            popularity: Popularity {
                weekly_rank: Some(29),
                request_count: Some(120_000),
                provider_count: Some(1),
            },
        },
        CuratedEntry {
            prefix: "perplexity/sonar",
            benchmarks: Benchmarks {
                coding_index: Some(36.0),
                intelligence_index: Some(42.0),
                agentic: Some(34.0),
                math_index: Some(38.0),
            },
            popularity: Popularity {
                weekly_rank: Some(36),
                request_count: Some(80_000),
                provider_count: Some(1),
            },
        },
        // ── NousResearch ─────────────────────────────────────────────
        CuratedEntry {
            prefix: "nousresearch/hermes-3",
            benchmarks: Benchmarks {
                coding_index: Some(40.0),
                intelligence_index: Some(44.0),
                agentic: Some(42.0),
                math_index: Some(38.0),
            },
            popularity: Popularity {
                weekly_rank: Some(37),
                request_count: Some(70_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "nousresearch/hermes-2",
            benchmarks: Benchmarks {
                coding_index: Some(35.0),
                intelligence_index: Some(38.0),
                agentic: Some(36.0),
                math_index: Some(34.0),
            },
            popularity: Popularity {
                weekly_rank: Some(48),
                request_count: Some(40_000),
                provider_count: Some(4),
            },
        },
        // ── DeepSeek additional ──────────────────────────────────────
        CuratedEntry {
            prefix: "deepseek/deepseek-prover",
            benchmarks: Benchmarks {
                coding_index: Some(52.0),
                intelligence_index: Some(55.0),
                agentic: Some(35.0),
                math_index: Some(82.0),
            },
            popularity: Popularity {
                weekly_rank: Some(30),
                request_count: Some(100_000),
                provider_count: Some(4),
            },
        },
        // ── Qwen additional ──────────────────────────────────────────
        CuratedEntry {
            prefix: "qwen/qwen-2.5-72b",
            benchmarks: Benchmarks {
                coding_index: Some(48.0),
                intelligence_index: Some(52.0),
                agentic: Some(44.0),
                math_index: Some(50.0),
            },
            popularity: Popularity {
                weekly_rank: Some(23),
                request_count: Some(180_000),
                provider_count: Some(5),
            },
        },
        CuratedEntry {
            prefix: "qwen/qwq",
            benchmarks: Benchmarks {
                coding_index: Some(54.0),
                intelligence_index: Some(58.0),
                agentic: Some(42.0),
                math_index: Some(72.0),
            },
            popularity: Popularity {
                weekly_rank: Some(17),
                request_count: Some(250_000),
                provider_count: Some(4),
            },
        },
        // ── Meta Llama additional ────────────────────────────────────
        CuratedEntry {
            prefix: "meta-llama/llama-3.1-405b",
            benchmarks: Benchmarks {
                coding_index: Some(52.0),
                intelligence_index: Some(56.0),
                agentic: Some(48.0),
                math_index: Some(54.0),
            },
            popularity: Popularity {
                weekly_rank: Some(13),
                request_count: Some(400_000),
                provider_count: Some(5),
            },
        },
        CuratedEntry {
            prefix: "meta-llama/llama-3.1-70b",
            benchmarks: Benchmarks {
                coding_index: Some(44.0),
                intelligence_index: Some(48.0),
                agentic: Some(42.0),
                math_index: Some(46.0),
            },
            popularity: Popularity {
                weekly_rank: Some(17),
                request_count: Some(300_000),
                provider_count: Some(6),
            },
        },
        CuratedEntry {
            prefix: "meta-llama/llama-3.1-8b",
            benchmarks: Benchmarks {
                coding_index: Some(30.0),
                intelligence_index: Some(34.0),
                agentic: Some(28.0),
                math_index: Some(32.0),
            },
            popularity: Popularity {
                weekly_rank: Some(10),
                request_count: Some(2_000_000),
                provider_count: Some(8),
            },
        },
        // ── OpenAI reasoning ─────────────────────────────────────────
        CuratedEntry {
            prefix: "openai/o1",
            benchmarks: Benchmarks {
                coding_index: Some(68.0),
                intelligence_index: Some(72.0),
                agentic: Some(58.0),
                math_index: Some(78.0),
            },
            popularity: Popularity {
                weekly_rank: Some(11),
                request_count: Some(400_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "openai/o1-mini",
            benchmarks: Benchmarks {
                coding_index: Some(55.0),
                intelligence_index: Some(58.0),
                agentic: Some(48.0),
                math_index: Some(65.0),
            },
            popularity: Popularity {
                weekly_rank: Some(15),
                request_count: Some(300_000),
                provider_count: Some(4),
            },
        },
        // ── Google additional ────────────────────────────────────────
        CuratedEntry {
            prefix: "google/gemini-1.5-pro",
            benchmarks: Benchmarks {
                coding_index: Some(55.0),
                intelligence_index: Some(58.0),
                agentic: Some(52.0),
                math_index: Some(56.0),
            },
            popularity: Popularity {
                weekly_rank: Some(12),
                request_count: Some(400_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "google/gemini-1.5-flash",
            benchmarks: Benchmarks {
                coding_index: Some(42.0),
                intelligence_index: Some(45.0),
                agentic: Some(40.0),
                math_index: Some(44.0),
            },
            popularity: Popularity {
                weekly_rank: Some(8),
                request_count: Some(1_500_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "google/gemma-2-27b",
            benchmarks: Benchmarks {
                coding_index: Some(38.0),
                intelligence_index: Some(42.0),
                agentic: Some(32.0),
                math_index: Some(40.0),
            },
            popularity: Popularity {
                weekly_rank: Some(28),
                request_count: Some(120_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "google/gemma-2-9b",
            benchmarks: Benchmarks {
                coding_index: Some(30.0),
                intelligence_index: Some(34.0),
                agentic: Some(26.0),
                math_index: Some(32.0),
            },
            popularity: Popularity {
                weekly_rank: Some(22),
                request_count: Some(200_000),
                provider_count: Some(5),
            },
        },
        // ── Minimax ──────────────────────────────────────────────────
        CuratedEntry {
            prefix: "minimax/minimax-m1",
            benchmarks: Benchmarks {
                coding_index: Some(48.0),
                intelligence_index: Some(52.0),
                agentic: Some(44.0),
                math_index: Some(50.0),
            },
            popularity: Popularity {
                weekly_rank: Some(26),
                request_count: Some(140_000),
                provider_count: Some(2),
            },
        },
        // ── Anthropic legacy ─────────────────────────────────────────
        CuratedEntry {
            prefix: "anthropic/claude-3-opus",
            benchmarks: Benchmarks {
                coding_index: Some(58.0),
                intelligence_index: Some(62.0),
                agentic: Some(55.0),
                math_index: Some(56.0),
            },
            popularity: Popularity {
                weekly_rank: Some(18),
                request_count: Some(200_000),
                provider_count: Some(4),
            },
        },
        CuratedEntry {
            prefix: "anthropic/claude-3-haiku",
            benchmarks: Benchmarks {
                coding_index: Some(38.0),
                intelligence_index: Some(42.0),
                agentic: Some(36.0),
                math_index: Some(38.0),
            },
            popularity: Popularity {
                weekly_rank: Some(7),
                request_count: Some(3_000_000),
                provider_count: Some(5),
            },
        },
        // ── Mistral additional ───────────────────────────────────────
        CuratedEntry {
            prefix: "mistralai/ministral-8b",
            benchmarks: Benchmarks {
                coding_index: Some(32.0),
                intelligence_index: Some(35.0),
                agentic: Some(30.0),
                math_index: Some(34.0),
            },
            popularity: Popularity {
                weekly_rank: Some(43),
                request_count: Some(55_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "mistralai/ministral-3b",
            benchmarks: Benchmarks {
                coding_index: Some(25.0),
                intelligence_index: Some(28.0),
                agentic: Some(22.0),
                math_index: Some(26.0),
            },
            popularity: Popularity {
                weekly_rank: Some(50),
                request_count: Some(35_000),
                provider_count: Some(3),
            },
        },
        CuratedEntry {
            prefix: "mistralai/pixtral",
            benchmarks: Benchmarks {
                coding_index: Some(40.0),
                intelligence_index: Some(44.0),
                agentic: Some(38.0),
                math_index: Some(42.0),
            },
            popularity: Popularity {
                weekly_rank: Some(31),
                request_count: Some(100_000),
                provider_count: Some(3),
            },
        },
        // ── AI21 ─────────────────────────────────────────────────────
        CuratedEntry {
            prefix: "ai21/jamba-1.5-large",
            benchmarks: Benchmarks {
                coding_index: Some(38.0),
                intelligence_index: Some(42.0),
                agentic: Some(36.0),
                math_index: Some(40.0),
            },
            popularity: Popularity {
                weekly_rank: Some(46),
                request_count: Some(40_000),
                provider_count: Some(2),
            },
        },
        CuratedEntry {
            prefix: "ai21/jamba-1.5-mini",
            benchmarks: Benchmarks {
                coding_index: Some(30.0),
                intelligence_index: Some(34.0),
                agentic: Some(28.0),
                math_index: Some(32.0),
            },
            popularity: Popularity {
                weekly_rank: Some(52),
                request_count: Some(25_000),
                provider_count: Some(2),
            },
        },
    ]
}

/// Apply curated benchmark data to models in the registry.
///
/// Uses longest-prefix matching against OpenRouter model IDs so entries
/// like "anthropic/claude-sonnet-4" match "anthropic/claude-sonnet-4-6".
/// Only fills in fields that are currently `None`/`default` — never
/// overwrites data that was already populated (e.g. from a previous manual edit).
fn apply_curated_benchmarks(registry: &mut BenchmarkRegistry) -> usize {
    let curated = curated_benchmarks();
    let mut applied = 0;

    for model in registry.models.values_mut() {
        // Find the best (longest) matching prefix.
        let best_match = curated
            .iter()
            .filter(|c| model.id.starts_with(c.prefix))
            .max_by_key(|c| c.prefix.len());

        if let Some(entry) = best_match {
            let mut changed = false;

            // Apply benchmarks only if all are None (don't overwrite partial data).
            if model.benchmarks.coding_index.is_none()
                && model.benchmarks.intelligence_index.is_none()
                && model.benchmarks.agentic.is_none()
            {
                model.benchmarks = entry.benchmarks.clone();
                changed = true;
            }

            // Apply popularity only if all fields are None.
            if model.popularity.weekly_rank.is_none()
                && model.popularity.request_count.is_none()
                && model.popularity.provider_count.is_none()
            {
                model.popularity = entry.popularity.clone();
                changed = true;
            }

            if changed {
                applied += 1;
            }
        }
    }

    applied
}

// ── Build from OpenRouter data ──────────────────────────────────────────

use crate::executor::native::openai_client::OpenRouterModel;

/// Build a `BenchmarkRegistry` from OpenRouter API model data.
///
/// Populates pricing, architecture, tool support, and curated benchmark/
/// popularity data for well-known models. Models without curated data
/// get default (empty) benchmark scores and a pricing-heuristic tier.
pub fn build_from_openrouter(models: &[OpenRouterModel]) -> BenchmarkRegistry {
    let now = chrono::Utc::now().to_rfc3339();
    let mut entries = BTreeMap::new();

    for model in models {
        let pricing = parse_or_pricing(model);

        // Skip models with no pricing data (typically deprecated or placeholder entries).
        if pricing.input_per_mtok <= 0.0 && pricing.output_per_mtok <= 0.0 {
            continue;
        }

        let context_window = model.context_length;
        let max_output_tokens = model
            .top_provider
            .as_ref()
            .and_then(|tp| tp.max_completion_tokens);
        let supports_tools = model.supported_parameters.iter().any(|p| p == "tools");

        let entry = ModelBenchmark {
            id: model.id.clone(),
            name: model.name.clone(),
            pricing,
            context_window,
            max_output_tokens,
            supports_tools,
            benchmarks: Benchmarks::default(),
            popularity: Popularity {
                provider_count: None,
                request_count: None,
                weekly_rank: None,
            },
            fitness: Fitness::default(),
            tier: "budget".to_string(), // Will be reclassified after scoring.
            pricing_updated_at: now.clone(),
            is_proxy: false,
        };

        entries.insert(model.id.clone(), entry);
    }

    let mut registry = BenchmarkRegistry {
        version: 1,
        fetched_at: now,
        source: RegistrySource {
            openrouter_api: "https://openrouter.ai/api/v1/models".to_string(),
        },
        models: entries,
    };

    // Seed curated benchmark/popularity data for well-known models.
    apply_curated_benchmarks(&mut registry);

    // Generate proxy scores for models without curated data.
    apply_proxy_scores(&mut registry);

    // Classify tiers based on pricing heuristics (no benchmark data yet).
    classify_tiers_from_pricing(&mut registry);

    registry
}

/// Generate proxy benchmark/popularity scores for models without curated data.
///
/// Uses available signals (pricing, context window, tool support) as proxies
/// for quality and popularity. These are lower-confidence estimates but much
/// better than 0.0 for ranking purposes.
///
/// Proxy quality scoring (0–100):
///   - Pricing signal (40%): More expensive models tend to be higher quality.
///     Maps output pricing to a 0–100 score using a logarithmic scale.
///   - Context window signal (30%): Larger context = more capable architecture.
///     Maps context length (log-scaled) to 0–100.
///   - Tool support signal (15%): Models supporting tools are typically more
///     capable and better-maintained.
///   - Output capacity signal (15%): Higher max output tokens indicates
///     more capable generation.
///
/// Proxy popularity scoring:
///   - Uses pricing tier as a rough indicator: cheaper models with tool
///     support tend to be more popular. Assigns estimated request counts
///     and provider counts from pricing position.
fn apply_proxy_scores(registry: &mut BenchmarkRegistry) -> usize {
    // Collect pricing stats for relative scoring.
    let output_prices: Vec<f64> = registry
        .models
        .values()
        .map(|m| m.pricing.output_per_mtok)
        .filter(|p| *p > 0.0)
        .collect();
    let max_output_price = output_prices
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let context_lengths: Vec<f64> = registry
        .models
        .values()
        .filter_map(|m| m.context_window.map(|c| c as f64))
        .filter(|c| *c > 0.0)
        .collect();
    let max_context = context_lengths
        .iter()
        .copied()
        .fold(1.0_f64, f64::max);

    let mut applied = 0;

    // Collect keys first to avoid borrow issues.
    let model_ids: Vec<String> = registry.models.keys().cloned().collect();

    for id in &model_ids {
        let model = registry.models.get(id).unwrap();

        // Skip models that already have curated benchmark data.
        if model.benchmarks.coding_index.is_some()
            || model.benchmarks.intelligence_index.is_some()
        {
            continue;
        }

        let out_price = model.pricing.output_per_mtok;
        let ctx = model.context_window.unwrap_or(0) as f64;
        let has_tools = model.supports_tools;
        let max_out = model.max_output_tokens.unwrap_or(0) as f64;

        // Pricing signal: log-scaled relative to max.
        let price_score = if out_price > 0.0 && max_output_price > 0.0 {
            let log_price = (out_price + 1.0).ln();
            let log_max = (max_output_price + 1.0).ln();
            (log_price / log_max).min(1.0) * 100.0
        } else {
            10.0 // Free models get a low base score
        };

        // Context window signal: log-scaled.
        let ctx_score = if ctx > 0.0 && max_context > 0.0 {
            let log_ctx = (ctx).ln();
            let log_max = (max_context).ln();
            (log_ctx / log_max).min(1.0) * 100.0
        } else {
            0.0
        };

        // Tool support signal.
        let tool_score = if has_tools { 80.0 } else { 20.0 };

        // Output capacity signal.
        let output_score = if max_out >= 64_000.0 {
            90.0
        } else if max_out >= 16_000.0 {
            70.0
        } else if max_out >= 4_000.0 {
            50.0
        } else if max_out > 0.0 {
            30.0
        } else {
            20.0
        };

        // Composite proxy quality (0–100, but capped at 45 to stay below
        // curated scores which represent actual benchmark measurements).
        let raw_quality =
            price_score * 0.40 + ctx_score * 0.30 + tool_score * 0.15 + output_score * 0.15;
        let proxy_quality = raw_quality.min(45.0);

        // Map proxy quality to individual benchmark proxies.
        // These are intentionally rough — they're proxies, not measurements.
        let coding_proxy = proxy_quality * 0.95;
        let intelligence_proxy = proxy_quality;
        let agentic_proxy = if has_tools {
            proxy_quality * 0.90
        } else {
            proxy_quality * 0.40
        };

        // Proxy popularity from pricing tier position.
        let (est_rank, est_requests, est_providers) = if out_price >= 18.0 {
            // Premium tier: niche usage
            (50u32, 50_000u64, 2u32)
        } else if out_price >= 3.0 {
            // Standard tier: moderate usage
            (35, 100_000, 3)
        } else if has_tools {
            // Fast tier with tools: likely popular
            (25, 200_000, 3)
        } else {
            // Fast tier without tools: less popular
            (45, 30_000, 2)
        };

        let model = registry.models.get_mut(id).unwrap();

        model.benchmarks = Benchmarks {
            coding_index: Some(coding_proxy),
            intelligence_index: Some(intelligence_proxy),
            agentic: Some(agentic_proxy),
            math_index: Some(proxy_quality * 0.85),
        };

        // Only set popularity if not already populated.
        if model.popularity.weekly_rank.is_none()
            && model.popularity.request_count.is_none()
            && model.popularity.provider_count.is_none()
        {
            model.popularity = Popularity {
                weekly_rank: Some(est_rank),
                request_count: Some(est_requests),
                provider_count: Some(est_providers),
            };
        }

        model.is_proxy = true;
        applied += 1;
    }

    applied
}

/// Classify tiers heuristically from pricing when no benchmark data is available.
///
/// Uses cost as a proxy for capability: expensive models tend to be frontier.
fn classify_tiers_from_pricing(registry: &mut BenchmarkRegistry) {
    // Compute median output cost.
    let costs: Vec<f64> = registry
        .models
        .values()
        .map(|m| m.pricing.output_per_mtok)
        .filter(|c| *c > 0.0)
        .collect();
    let median_out = median(&costs).unwrap_or(1.0);

    for model in registry.models.values_mut() {
        let out = model.pricing.output_per_mtok;
        model.tier = if out >= median_out * 3.0 {
            "frontier".to_string()
        } else if out >= median_out * 0.8 {
            "mid".to_string()
        } else {
            "budget".to_string()
        };
    }
}

/// Parse OpenRouter pricing strings to per-million-token USD values.
fn parse_or_pricing(model: &OpenRouterModel) -> BenchmarkPricing {
    let pricing = match &model.pricing {
        Some(p) => p,
        None => {
            return BenchmarkPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            }
        }
    };

    let input = pricing
        .prompt
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|per_tok| per_tok * 1_000_000.0)
        .unwrap_or(0.0);

    let output = pricing
        .completion
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|per_tok| per_tok * 1_000_000.0)
        .unwrap_or(0.0);

    BenchmarkPricing {
        input_per_mtok: input,
        output_per_mtok: output,
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
    }
}

// ── Registry Diff ──────────────────────────────────────────────────────

/// A single change detected between two registry snapshots.
#[derive(Debug, Clone)]
pub enum RegistryChange {
    /// A model entered the top-N by fitness score.
    EnteredTopN { model_id: String, rank: usize, score: f64 },
    /// A model exited the top-N by fitness score.
    ExitedTopN { model_id: String, old_rank: usize },
    /// A model's fitness score changed significantly.
    ScoreDelta { model_id: String, old_score: f64, new_score: f64, delta: f64 },
    /// A model's tier changed.
    TierChanged { model_id: String, old_tier: String, new_tier: String },
    /// A new model appeared in the registry.
    ModelAdded { model_id: String, tier: String },
    /// A model was removed from the registry.
    ModelRemoved { model_id: String, tier: String },
}

impl std::fmt::Display for RegistryChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryChange::EnteredTopN { model_id, rank, score } => {
                write!(f, "  + {} entered top-N (rank {}, score {:.1})", model_id, rank, score)
            }
            RegistryChange::ExitedTopN { model_id, old_rank } => {
                write!(f, "  - {} exited top-N (was rank {})", model_id, old_rank)
            }
            RegistryChange::ScoreDelta { model_id, old_score, new_score, delta } => {
                let arrow = if *delta > 0.0 { "↑" } else { "↓" };
                write!(f, "  ~ {} score {:.1} → {:.1} ({}{:.1})", model_id, old_score, new_score, arrow, delta.abs())
            }
            RegistryChange::TierChanged { model_id, old_tier, new_tier } => {
                write!(f, "  * {} tier {} → {}", model_id, old_tier, new_tier)
            }
            RegistryChange::ModelAdded { model_id, tier } => {
                write!(f, "  + {} added ({})", model_id, tier)
            }
            RegistryChange::ModelRemoved { model_id, tier } => {
                write!(f, "  - {} removed ({})", model_id, tier)
            }
        }
    }
}

/// Compare two registries and return a list of significant changes.
///
/// `top_n` controls how many models are considered for enter/exit tracking (default 20).
/// `score_threshold` is the minimum absolute score change to report (default 2.0).
pub fn diff_registries(
    old: &BenchmarkRegistry,
    new: &BenchmarkRegistry,
    top_n: usize,
    score_threshold: f64,
) -> Vec<RegistryChange> {
    let mut changes = Vec::new();

    // Build ranked lists for top-N tracking.
    let old_ranked = old.ranked();
    let new_ranked = new.ranked();

    let old_top: Vec<&str> = old_ranked.iter().take(top_n).map(|m| m.id.as_str()).collect();
    let new_top: Vec<&str> = new_ranked.iter().take(top_n).map(|m| m.id.as_str()).collect();

    // Models that entered the top-N.
    for (rank, &model_id) in new_top.iter().enumerate() {
        if !old_top.contains(&model_id) {
            if let Some(m) = new.models.get(model_id) {
                changes.push(RegistryChange::EnteredTopN {
                    model_id: model_id.to_string(),
                    rank: rank + 1,
                    score: m.fitness.score.unwrap_or(0.0),
                });
            }
        }
    }

    // Models that exited the top-N.
    for (rank, &model_id) in old_top.iter().enumerate() {
        if !new_top.contains(&model_id) {
            changes.push(RegistryChange::ExitedTopN {
                model_id: model_id.to_string(),
                old_rank: rank + 1,
            });
        }
    }

    // Score deltas and tier changes for models present in both.
    for (id, new_model) in &new.models {
        if let Some(old_model) = old.models.get(id) {
            // Score delta.
            if let (Some(old_score), Some(new_score)) =
                (old_model.fitness.score, new_model.fitness.score)
            {
                let delta = new_score - old_score;
                if delta.abs() >= score_threshold {
                    changes.push(RegistryChange::ScoreDelta {
                        model_id: id.clone(),
                        old_score,
                        new_score,
                        delta,
                    });
                }
            }

            // Tier change.
            if old_model.tier != new_model.tier {
                changes.push(RegistryChange::TierChanged {
                    model_id: id.clone(),
                    old_tier: old_model.tier.clone(),
                    new_tier: new_model.tier.clone(),
                });
            }
        } else {
            // Newly added model.
            changes.push(RegistryChange::ModelAdded {
                model_id: id.clone(),
                tier: new_model.tier.clone(),
            });
        }
    }

    // Removed models.
    for (id, old_model) in &old.models {
        if !new.models.contains_key(id) {
            changes.push(RegistryChange::ModelRemoved {
                model_id: id.clone(),
                tier: old_model.tier.clone(),
            });
        }
    }

    changes
}

/// Format a list of registry changes as a human-readable summary.
pub fn format_changes(changes: &[RegistryChange]) -> String {
    if changes.is_empty() {
        return "No significant changes detected.".to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!("{} change(s) detected:", changes.len()));
    for change in changes {
        lines.push(change.to_string());
    }
    lines.join("\n")
}

// ── Popularity-weighted ranking for OpenRouter profile ─────────────────

/// A model ranked by the popularity-weighted algorithm for a specific tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedModel {
    /// OpenRouter model ID.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Popularity score component (0–100, PRIMARY signal).
    pub popularity_score: f64,
    /// Benchmark score component (0–100, SECONDARY signal).
    pub benchmark_score: f64,
    /// Composite ranking score.
    pub composite_score: f64,
    /// Assigned pricing tier: "fast", "standard", or "premium".
    pub tier: String,
    // ── Raw metrics for verbose display ──────────────────────────
    /// Input pricing per million tokens (USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    /// Output pricing per million tokens (USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
    /// Context window size in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Whether the model supports tool use.
    #[serde(default)]
    pub supports_tools: bool,
    /// Whether scores are from curated benchmarks (true) or proxy estimates (false).
    #[serde(default)]
    pub is_curated: bool,
}

/// Ranked model lists per pricing tier.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RankedTiers {
    /// Models ranked for the fast (haiku-class) tier.
    pub fast: Vec<RankedModel>,
    /// Models ranked for the standard (sonnet-class) tier.
    pub standard: Vec<RankedModel>,
    /// Models ranked for the premium (opus-class) tier.
    pub premium: Vec<RankedModel>,
}

/// Pricing tier boundaries for OpenRouter models (output $/MTok).
///
/// Models are bucketed by output pricing into haiku-class, sonnet-class, or opus-class.
const TIER_BOUNDARY_FAST_MAX: f64 = 3.0;
const TIER_BOUNDARY_PREMIUM_MIN: f64 = 18.0;

/// Compute a popularity score (0–100) for a model.
///
/// Popularity is the PRIMARY ranking signal. It captures real-world reliability,
/// API quality, and community trust that benchmarks miss.
///
/// Signals (weighted):
///   - weekly_rank: 50% (lower is better; rank 1 → 100, rank 200+ → 0)
///   - request_count: 30% (log-scaled relative to max in registry)
///   - provider_count: 20% (more providers = more trusted/available)
fn compute_popularity_score(pop: &Popularity, max_request_count: u64) -> f64 {
    let rank_score = pop
        .weekly_rank
        .map(|r| ((200.0 - r.min(200) as f64) / 200.0) * 100.0)
        .unwrap_or(0.0);

    let request_score = if max_request_count > 0 {
        pop.request_count
            .map(|rc| {
                if rc == 0 {
                    0.0
                } else {
                    let log_rc = (rc as f64).ln();
                    let log_max = (max_request_count as f64).ln();
                    (log_rc / log_max).min(1.0) * 100.0
                }
            })
            .unwrap_or(0.0)
    } else {
        0.0
    };

    let provider_score = pop
        .provider_count
        .map(|pc| (pc.min(10) as f64 / 10.0) * 100.0)
        .unwrap_or(0.0);

    rank_score * 0.50 + request_score * 0.30 + provider_score * 0.20
}

/// Assign a pricing-based tier label for the OpenRouter profile.
pub fn pricing_tier_label(output_per_mtok: f64) -> &'static str {
    if output_per_mtok >= TIER_BOUNDARY_PREMIUM_MIN {
        "premium"
    } else if output_per_mtok >= TIER_BOUNDARY_FAST_MAX {
        "standard"
    } else {
        "fast"
    }
}

/// Run the popularity-weighted ranking algorithm on the benchmark registry.
///
/// Returns ranked lists per tier (fast/standard/premium), ordered best-first.
/// Only includes models that support tool use (required for agentic work).
///
/// Composite score = popularity * 0.70 + benchmarks * 0.30
/// (Popularity is the PRIMARY signal per design principle.)
pub fn rank_models_for_profile(registry: &BenchmarkRegistry) -> RankedTiers {
    let max_request_count = registry
        .models
        .values()
        .filter_map(|m| m.popularity.request_count)
        .max()
        .unwrap_or(0);

    let mut fast = Vec::new();
    let mut standard = Vec::new();
    let mut premium = Vec::new();

    for model in registry.models.values() {
        if !model.supports_tools {
            continue;
        }

        let ptier = pricing_tier_label(model.pricing.output_per_mtok);
        let popularity_score =
            compute_popularity_score(&model.popularity, max_request_count);
        let benchmark_score = model.fitness.components.quality.unwrap_or(0.0);
        let composite_score = popularity_score * 0.70 + benchmark_score * 0.30;

        let ranked = RankedModel {
            id: model.id.clone(),
            name: model.name.clone(),
            popularity_score,
            benchmark_score,
            composite_score,
            tier: ptier.to_string(),
            input_per_mtok: Some(model.pricing.input_per_mtok),
            output_per_mtok: Some(model.pricing.output_per_mtok),
            context_window: model.context_window,
            supports_tools: model.supports_tools,
            is_curated: !model.is_proxy,
        };

        match ptier {
            "fast" => fast.push(ranked),
            "standard" => standard.push(ranked),
            "premium" => premium.push(ranked),
            _ => {}
        }
    }

    fast.sort_by(|a, b| {
        b.composite_score
            .partial_cmp(&a.composite_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    standard.sort_by(|a, b| {
        b.composite_score
            .partial_cmp(&a.composite_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    premium.sort_by(|a, b| {
        b.composite_score
            .partial_cmp(&a.composite_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    RankedTiers {
        fast,
        standard,
        premium,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CLAUDE_OPUS_MODEL_ID;

    #[test]
    fn test_compute_quality_all_present() {
        let b = Benchmarks {
            coding_index: Some(50.0),
            intelligence_index: Some(50.0),
            agentic: Some(50.0),
            math_index: None,
        };
        let q = compute_quality(&b).unwrap();
        // 50*0.5 + 50*0.3 + 50*0.2 = 25 + 15 + 10 = 50
        assert!((q - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_quality_no_agentic() {
        let b = Benchmarks {
            coding_index: Some(60.0),
            intelligence_index: Some(40.0),
            agentic: None,
            math_index: None,
        };
        let q = compute_quality(&b).unwrap();
        // 60*0.55 + 40*0.45 = 33 + 18 = 51
        assert!((q - 51.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_quality_missing_coding() {
        let b = Benchmarks {
            coding_index: None,
            intelligence_index: Some(50.0),
            agentic: Some(60.0),
            math_index: None,
        };
        let q = compute_quality(&b).unwrap();
        // coding proxied from intelligence: 50*0.9 = 45
        // 45*0.5 + 50*0.3 + 60*0.2 = 22.5 + 15 + 12 = 49.5
        assert!((q - 49.5).abs() < 0.01);
    }

    #[test]
    fn test_compute_quality_all_missing() {
        let b = Benchmarks::default();
        assert!(compute_quality(&b).is_none());
    }

    #[test]
    fn test_classify_tier_frontier() {
        let b = Benchmarks {
            coding_index: Some(50.0),
            intelligence_index: Some(52.0),
            ..Default::default()
        };
        assert_eq!(classify_tier(&b, Some(70.0)), "frontier");
    }

    #[test]
    fn test_classify_tier_mid() {
        let b = Benchmarks {
            coding_index: Some(36.0),
            ..Default::default()
        };
        assert_eq!(classify_tier(&b, Some(42.0)), "mid");
    }

    #[test]
    fn test_classify_tier_budget() {
        let b = Benchmarks::default();
        assert_eq!(classify_tier(&b, Some(20.0)), "budget");
    }

    #[test]
    fn test_median() {
        assert_eq!(median(&[1.0, 2.0, 3.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
        assert!(median(&[]).is_none());
    }

    #[test]
    fn test_fitness_scoring_round_trip() {
        let mut registry = BenchmarkRegistry {
            version: 1,
            fetched_at: "2026-04-01T00:00:00Z".to_string(),
            source: RegistrySource {
                openrouter_api: "https://openrouter.ai/api/v1/models".to_string(),
            },
            models: BTreeMap::new(),
        };

        registry.models.insert(
            "test/model-a".to_string(),
            ModelBenchmark {
                id: "test/model-a".to_string(),
                name: "Model A".to_string(),
                pricing: BenchmarkPricing {
                    input_per_mtok: 3.0,
                    output_per_mtok: 15.0,
                    cache_read_per_mtok: None,
                    cache_write_per_mtok: None,
                },
                context_window: Some(200_000),
                max_output_tokens: Some(32_000),
                supports_tools: true,
                benchmarks: Benchmarks {
                    coding_index: Some(50.0),
                    intelligence_index: Some(50.0),
                    agentic: Some(60.0),
                    math_index: None,
                },
                popularity: Popularity {
                    provider_count: Some(3),
                    ..Default::default()
                },
                fitness: Fitness::default(),
                tier: "mid".to_string(),
                pricing_updated_at: "2026-04-01T00:00:00Z".to_string(),
                is_proxy: false,
            },
        );

        compute_fitness_scores(&mut registry);

        let model = registry.models.get("test/model-a").unwrap();
        assert!(model.fitness.score.is_some());
        assert!(model.fitness.score.unwrap() > 0.0);
        assert!(model.fitness.components.quality.is_some());
        assert!(model.fitness.components.value.is_some());
        assert!(model.fitness.components.reliability.is_some());
    }

    #[test]
    fn test_registry_save_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = BenchmarkRegistry {
            version: 1,
            fetched_at: "2026-04-01T00:00:00Z".to_string(),
            source: RegistrySource {
                openrouter_api: "https://openrouter.ai/api/v1/models".to_string(),
            },
            models: BTreeMap::new(),
        };
        registry.save(dir.path()).unwrap();
        let loaded = BenchmarkRegistry::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.models.len(), 0);
    }

    #[test]
    fn test_registry_load_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = BenchmarkRegistry::load(dir.path()).unwrap();
        assert!(result.is_none());
    }

    fn make_test_model(id: &str, tier: &str, score: Option<f64>) -> ModelBenchmark {
        ModelBenchmark {
            id: id.to_string(),
            name: id.to_string(),
            pricing: BenchmarkPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: Some(128_000),
            max_output_tokens: Some(8_000),
            supports_tools: true,
            benchmarks: Benchmarks::default(),
            popularity: Popularity::default(),
            fitness: Fitness {
                score,
                components: FitnessComponents::default(),
            },
            tier: tier.to_string(),
            pricing_updated_at: "2026-04-01T00:00:00Z".to_string(),
            is_proxy: false,
        }
    }

    fn make_test_registry(models: Vec<ModelBenchmark>) -> BenchmarkRegistry {
        let mut map = BTreeMap::new();
        for m in models {
            map.insert(m.id.clone(), m);
        }
        BenchmarkRegistry {
            version: 1,
            fetched_at: "2026-04-01T00:00:00Z".to_string(),
            source: RegistrySource {
                openrouter_api: "https://openrouter.ai/api/v1/models".to_string(),
            },
            models: map,
        }
    }

    #[test]
    fn test_diff_no_changes() {
        let reg = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(80.0)),
        ]);
        let changes = diff_registries(&reg, &reg, 20, 2.0);
        // Same registry → no score deltas, no tier changes, no adds/removes
        // (top-N enter/exit won't fire either since sets are identical)
        assert!(changes.is_empty(), "Expected no changes, got: {:?}", changes);
    }

    #[test]
    fn test_diff_model_added() {
        let old = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(80.0)),
        ]);
        let new = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(80.0)),
            make_test_model("b/model-2", "mid", Some(50.0)),
        ]);
        let changes = diff_registries(&old, &new, 20, 2.0);
        assert!(changes.iter().any(|c| matches!(c, RegistryChange::ModelAdded { model_id, .. } if model_id == "b/model-2")));
    }

    #[test]
    fn test_diff_model_removed() {
        let old = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(80.0)),
            make_test_model("b/model-2", "mid", Some(50.0)),
        ]);
        let new = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(80.0)),
        ]);
        let changes = diff_registries(&old, &new, 20, 2.0);
        assert!(changes.iter().any(|c| matches!(c, RegistryChange::ModelRemoved { model_id, .. } if model_id == "b/model-2")));
    }

    #[test]
    fn test_diff_tier_changed() {
        let old = make_test_registry(vec![
            make_test_model("a/model-1", "mid", Some(50.0)),
        ]);
        let new = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(50.0)),
        ]);
        let changes = diff_registries(&old, &new, 20, 2.0);
        assert!(changes.iter().any(|c| matches!(c, RegistryChange::TierChanged { model_id, old_tier, new_tier, .. }
            if model_id == "a/model-1" && old_tier == "mid" && new_tier == "frontier")));
    }

    #[test]
    fn test_diff_score_delta() {
        let old = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(70.0)),
        ]);
        let new = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(75.0)),
        ]);
        let changes = diff_registries(&old, &new, 20, 2.0);
        assert!(changes.iter().any(|c| matches!(c, RegistryChange::ScoreDelta { delta, .. } if (*delta - 5.0).abs() < 0.01)));
    }

    #[test]
    fn test_diff_score_below_threshold() {
        let old = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(70.0)),
        ]);
        let new = make_test_registry(vec![
            make_test_model("a/model-1", "frontier", Some(71.0)),
        ]);
        let changes = diff_registries(&old, &new, 20, 2.0);
        assert!(!changes.iter().any(|c| matches!(c, RegistryChange::ScoreDelta { .. })));
    }

    #[test]
    fn test_format_changes_empty() {
        assert_eq!(format_changes(&[]), "No significant changes detected.");
    }

    #[test]
    fn test_format_changes_non_empty() {
        let changes = vec![
            RegistryChange::ModelAdded { model_id: "test/m".to_string(), tier: "mid".to_string() },
        ];
        let text = format_changes(&changes);
        assert!(text.contains("1 change(s) detected"));
        assert!(text.contains("test/m added"));
    }

    // ── Popularity-weighted ranking tests ──────────────────────────────

    fn make_ranked_model(
        id: &str,
        name: &str,
        output_price: f64,
        tools: bool,
        popularity: Popularity,
        quality: Option<f64>,
    ) -> ModelBenchmark {
        ModelBenchmark {
            id: id.to_string(),
            name: name.to_string(),
            pricing: BenchmarkPricing {
                input_per_mtok: output_price * 0.2,
                output_per_mtok: output_price,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: Some(128_000),
            max_output_tokens: Some(8_000),
            supports_tools: tools,
            benchmarks: Benchmarks::default(),
            popularity,
            fitness: Fitness {
                score: quality,
                components: FitnessComponents {
                    quality,
                    value: None,
                    reliability: None,
                },
            },
            tier: "budget".to_string(),
            pricing_updated_at: "2026-04-01T00:00:00Z".to_string(),
            is_proxy: false,
        }
    }

    #[test]
    fn test_pricing_tier_label() {
        assert_eq!(pricing_tier_label(0.5), "fast");
        assert_eq!(pricing_tier_label(2.99), "fast");
        assert_eq!(pricing_tier_label(3.0), "standard");
        assert_eq!(pricing_tier_label(15.0), "standard");
        assert_eq!(pricing_tier_label(18.0), "premium");
        assert_eq!(pricing_tier_label(25.0), "premium");
    }

    #[test]
    fn test_popularity_score_full_data() {
        let pop = Popularity {
            provider_count: Some(5),
            request_count: Some(1_000_000),
            weekly_rank: Some(1),
        };
        let score = compute_popularity_score(&pop, 1_000_000);
        // rank: (200-1)/200 * 100 * 0.5 = 49.75
        // request: ln(1M)/ln(1M) * 100 * 0.3 = 30.0
        // provider: 5/10 * 100 * 0.2 = 10.0
        // total ≈ 89.75
        assert!(score > 85.0 && score < 95.0, "score was {}", score);
    }

    #[test]
    fn test_popularity_score_no_data() {
        let pop = Popularity::default();
        let score = compute_popularity_score(&pop, 1_000_000);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_popularity_score_only_providers() {
        let pop = Popularity {
            provider_count: Some(10),
            request_count: None,
            weekly_rank: None,
        };
        let score = compute_popularity_score(&pop, 0);
        // provider: 10/10 * 100 * 0.2 = 20.0
        assert!((score - 20.0).abs() < 0.01);
    }

    #[test]
    fn test_rank_models_filters_no_tools() {
        let registry = make_test_registry(vec![
            make_ranked_model(
                "a/with-tools",
                "With Tools",
                1.0,
                true,
                Popularity { provider_count: Some(5), request_count: Some(1000), weekly_rank: Some(1) },
                Some(50.0),
            ),
            make_ranked_model(
                "b/no-tools",
                "No Tools",
                1.0,
                false,
                Popularity { provider_count: Some(10), request_count: Some(10000), weekly_rank: Some(1) },
                Some(90.0),
            ),
        ]);

        let ranked = rank_models_for_profile(&registry);
        // b/no-tools should be excluded
        let all_ids: Vec<&str> = ranked.fast.iter()
            .chain(ranked.standard.iter())
            .chain(ranked.premium.iter())
            .map(|r| r.id.as_str())
            .collect();
        assert!(all_ids.contains(&"a/with-tools"));
        assert!(!all_ids.contains(&"b/no-tools"));
    }

    #[test]
    fn test_rank_models_tier_assignment() {
        let registry = make_test_registry(vec![
            make_ranked_model("a/cheap", "Cheap", 1.0, true, Popularity::default(), None),
            make_ranked_model("b/mid", "Mid", 10.0, true, Popularity::default(), None),
            make_ranked_model("c/premium", "Premium", 25.0, true, Popularity::default(), None),
        ]);

        let ranked = rank_models_for_profile(&registry);
        assert_eq!(ranked.fast.len(), 1);
        assert_eq!(ranked.fast[0].id, "a/cheap");
        assert_eq!(ranked.standard.len(), 1);
        assert_eq!(ranked.standard[0].id, "b/mid");
        assert_eq!(ranked.premium.len(), 1);
        assert_eq!(ranked.premium[0].id, "c/premium");
    }

    #[test]
    fn test_rank_models_popularity_dominates() {
        // Model A has high popularity, low benchmarks.
        // Model B has low popularity, high benchmarks.
        // With 70% popularity weight, A should rank higher.
        let registry = make_test_registry(vec![
            make_ranked_model(
                "a/popular",
                "Popular",
                1.0,
                true,
                Popularity { provider_count: Some(8), request_count: Some(500_000), weekly_rank: Some(2) },
                Some(30.0),
            ),
            make_ranked_model(
                "b/benchmark-king",
                "Benchmark King",
                1.0,
                true,
                Popularity { provider_count: Some(1), request_count: Some(100), weekly_rank: Some(150) },
                Some(90.0),
            ),
        ]);

        let ranked = rank_models_for_profile(&registry);
        assert!(ranked.fast.len() >= 2);
        assert_eq!(ranked.fast[0].id, "a/popular", "Popular model should rank first");
    }

    #[test]
    fn test_rank_models_sorted_descending() {
        let registry = make_test_registry(vec![
            make_ranked_model(
                "a/low",
                "Low",
                1.0,
                true,
                Popularity { provider_count: Some(1), ..Default::default() },
                None,
            ),
            make_ranked_model(
                "b/high",
                "High",
                1.0,
                true,
                Popularity { provider_count: Some(10), ..Default::default() },
                None,
            ),
            make_ranked_model(
                "c/mid",
                "Mid",
                1.0,
                true,
                Popularity { provider_count: Some(5), ..Default::default() },
                None,
            ),
        ]);

        let ranked = rank_models_for_profile(&registry);
        assert_eq!(ranked.fast.len(), 3);
        // Verify descending order
        for i in 0..ranked.fast.len() - 1 {
            assert!(
                ranked.fast[i].composite_score >= ranked.fast[i + 1].composite_score,
                "Expected descending order at index {}: {} >= {}",
                i, ranked.fast[i].composite_score, ranked.fast[i + 1].composite_score,
            );
        }
    }

    // ── Curated benchmark tests ─────────────────────────────────────

    #[test]
    fn test_curated_benchmarks_not_empty() {
        let curated = curated_benchmarks();
        assert!(curated.len() >= 20, "Expected at least 20 curated models, got {}", curated.len());
    }

    #[test]
    fn test_apply_curated_benchmarks_exact_match() {
        let mut registry = make_test_registry(vec![
            make_test_model("anthropic/claude-sonnet-4-6", "budget", None),
        ]);
        let applied = apply_curated_benchmarks(&mut registry);
        assert_eq!(applied, 1);

        let model = registry.models.get("anthropic/claude-sonnet-4-6").unwrap();
        assert!(model.benchmarks.coding_index.is_some());
        assert!(model.benchmarks.intelligence_index.is_some());
        assert!(model.benchmarks.agentic.is_some());
        assert!(model.popularity.weekly_rank.is_some());
        assert!(model.popularity.request_count.is_some());
    }

    #[test]
    fn test_apply_curated_benchmarks_prefix_match() {
        // "anthropic/claude-opus-4" prefix should match the full opus model ID
        let opus_key = format!("anthropic/{CLAUDE_OPUS_MODEL_ID}");
        let mut registry = make_test_registry(vec![
            make_test_model(&opus_key, "budget", None),
        ]);
        let applied = apply_curated_benchmarks(&mut registry);
        assert_eq!(applied, 1);

        let model = registry.models.get(&opus_key).unwrap();
        assert!(model.benchmarks.coding_index.unwrap() > 70.0);
    }

    #[test]
    fn test_apply_curated_benchmarks_no_match() {
        let mut registry = make_test_registry(vec![
            make_test_model("unknown/some-random-model", "budget", None),
        ]);
        let applied = apply_curated_benchmarks(&mut registry);
        assert_eq!(applied, 0);

        let model = registry.models.get("unknown/some-random-model").unwrap();
        assert!(model.benchmarks.coding_index.is_none());
    }

    #[test]
    fn test_apply_curated_benchmarks_does_not_overwrite() {
        let mut model = make_test_model("anthropic/claude-sonnet-4-6", "frontier", None);
        model.benchmarks = Benchmarks {
            coding_index: Some(99.0),
            intelligence_index: Some(99.0),
            agentic: Some(99.0),
            math_index: None,
        };
        model.popularity = Popularity {
            weekly_rank: Some(1),
            request_count: Some(999),
            provider_count: Some(10),
        };
        let mut registry = make_test_registry(vec![model]);

        let applied = apply_curated_benchmarks(&mut registry);
        assert_eq!(applied, 0, "Should not overwrite existing benchmark data");

        let model = registry.models.get("anthropic/claude-sonnet-4-6").unwrap();
        assert_eq!(model.benchmarks.coding_index, Some(99.0));
        assert_eq!(model.popularity.weekly_rank, Some(1));
    }

    #[test]
    fn test_build_from_openrouter_seeds_curated_data() {
        use crate::executor::native::openai_client::{OpenRouterModel, OpenRouterPricing};

        let models = vec![
            OpenRouterModel {
                id: "anthropic/claude-sonnet-4-6".into(),
                name: "Claude Sonnet 4.6".into(),
                description: "".into(),
                context_length: Some(200_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0.000003".into()),
                    completion: Some("0.000015".into()),
                }),
                supported_parameters: vec!["tools".into()],
                architecture: None,
                top_provider: None,
            },
            OpenRouterModel {
                id: "unknown/random-model".into(),
                name: "Random".into(),
                description: "".into(),
                context_length: Some(32_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0.000001".into()),
                    completion: Some("0.000002".into()),
                }),
                supported_parameters: vec!["tools".into()],
                architecture: None,
                top_provider: None,
            },
        ];

        let registry = build_from_openrouter(&models);

        // Claude Sonnet should have curated benchmarks.
        let sonnet = registry.models.get("anthropic/claude-sonnet-4-6").unwrap();
        assert!(sonnet.benchmarks.coding_index.is_some(),
            "Known model should have curated benchmark data");
        assert!(sonnet.popularity.weekly_rank.is_some(),
            "Known model should have curated popularity data");

        // Unknown model should have proxy benchmark scores (not None, but capped below curated).
        let random = registry.models.get("unknown/random-model").unwrap();
        assert!(random.benchmarks.coding_index.is_some(),
            "Unknown model should have proxy benchmark data");
        assert!(random.is_proxy,
            "Unknown model should be marked as proxy");
        // Proxy scores are capped at 45.0 to stay below curated scores.
        assert!(random.benchmarks.coding_index.unwrap() <= 45.0,
            "Proxy coding_index should be capped at 45.0");
        // Curated model should NOT be marked as proxy.
        assert!(!sonnet.is_proxy,
            "Curated model should not be marked as proxy");
    }

    #[test]
    fn test_curated_models_get_fitness_scores() {
        use crate::executor::native::openai_client::{OpenRouterModel, OpenRouterPricing};

        let models = vec![
            OpenRouterModel {
                id: "anthropic/claude-sonnet-4-6".into(),
                name: "Claude Sonnet 4.6".into(),
                description: "".into(),
                context_length: Some(200_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0.000003".into()),
                    completion: Some("0.000015".into()),
                }),
                supported_parameters: vec!["tools".into()],
                architecture: None,
                top_provider: None,
            },
        ];

        let mut registry = build_from_openrouter(&models);
        compute_fitness_scores(&mut registry);

        let sonnet = registry.models.get("anthropic/claude-sonnet-4-6").unwrap();
        assert!(sonnet.fitness.score.is_some(), "Curated model should get a fitness score");
        assert!(sonnet.fitness.score.unwrap() > 0.0, "Fitness score should be positive");
        assert!(sonnet.fitness.components.quality.is_some());
    }

    #[test]
    fn test_curated_models_rank_above_unknown() {
        use crate::executor::native::openai_client::{OpenRouterModel, OpenRouterPricing};

        // Two fast-tier models (cheap output): one known, one unknown.
        let models = vec![
            OpenRouterModel {
                id: "openai/gpt-4o-mini".into(),
                name: "GPT-4o Mini".into(),
                description: "".into(),
                context_length: Some(128_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0.00000015".into()),
                    completion: Some("0.0000006".into()),
                }),
                supported_parameters: vec!["tools".into()],
                architecture: None,
                top_provider: None,
            },
            OpenRouterModel {
                id: "alibaba/tongyi-deepresearch-30b-a3b".into(),
                name: "Tongyi DeepResearch".into(),
                description: "".into(),
                context_length: Some(32_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0.00000010".into()),
                    completion: Some("0.0000005".into()),
                }),
                supported_parameters: vec!["tools".into()],
                architecture: None,
                top_provider: None,
            },
        ];

        let mut registry = build_from_openrouter(&models);
        compute_fitness_scores(&mut registry);
        let ranked = rank_models_for_profile(&registry);

        // GPT-4o Mini should rank above the unknown model.
        assert!(ranked.fast.len() >= 2, "Expected at least 2 fast models");
        assert_eq!(
            ranked.fast[0].id, "openai/gpt-4o-mini",
            "Known model (GPT-4o Mini) should rank above unknown (tongyi): {:?}",
            ranked.fast.iter().map(|r| (&r.id, r.composite_score)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_longest_prefix_wins() {
        // If we had both "openai/gpt-4o" and "openai/gpt-4o-mini" prefixes,
        // "openai/gpt-4o-mini" should match the longer one.
        let curated = curated_benchmarks();
        let gpt4o_mini_match = curated
            .iter()
            .filter(|c| "openai/gpt-4o-mini".starts_with(c.prefix))
            .max_by_key(|c| c.prefix.len());
        assert!(gpt4o_mini_match.is_some());
        assert_eq!(gpt4o_mini_match.unwrap().prefix, "openai/gpt-4o-mini");
    }
}
