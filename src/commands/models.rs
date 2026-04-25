use anyhow::{Context, Result};
use std::path::Path;
use workgraph::models::{ModelEntry, ModelRegistry, ModelTier};

/// List all models in the registry
pub fn run_list(workgraph_dir: &Path, tier: Option<&str>, json: bool) -> Result<()> {
    let registry = ModelRegistry::load(workgraph_dir)?;

    let tier_filter = tier.map(|t| t.parse::<ModelTier>()).transpose()?;
    let models = registry.list(tier_filter.as_ref());

    if json {
        let json_val: Vec<_> = models
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "provider": m.provider,
                    "cost_per_1m_input": m.cost_per_1m_input,
                    "cost_per_1m_output": m.cost_per_1m_output,
                    "context_window": m.context_window,
                    "capabilities": m.capabilities,
                    "tier": m.tier,
                    "is_default": registry.default_model.as_deref() == Some(&*m.id),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_val)?);
        return Ok(());
    }

    if models.is_empty() {
        println!("No models found.");
        return Ok(());
    }

    // Table header
    println!(
        "{:<35} {:<8} {:>10} {:>11} {:>10} CAPABILITIES",
        "MODEL", "TIER", "IN/1M", "OUT/1M", "CTX"
    );
    println!("{}", "-".repeat(100));

    for model in &models {
        let is_default = registry.default_model.as_deref() == Some(&*model.id);
        let marker = if is_default { " *" } else { "" };
        let ctx = format_context_window(model.context_window);
        let caps = model.capabilities.join(", ");

        println!(
            "{:<35} {:<8} {:>9.2} {:>10.2} {:>10} {}",
            format!("{}{}", model.id, marker),
            model.tier,
            model.cost_per_1m_input,
            model.cost_per_1m_output,
            ctx,
            caps,
        );
    }

    if let Some(default) = &registry.default_model {
        println!("\n  * = default model ({})", default);
    }

    Ok(())
}

/// Add a custom model to the registry
#[allow(clippy::too_many_arguments)]
pub fn run_add(
    workgraph_dir: &Path,
    id: &str,
    provider: Option<&str>,
    cost_in: f64,
    cost_out: f64,
    context_window: Option<u64>,
    capabilities: &[String],
    tier: &str,
) -> Result<()> {
    let mut registry = ModelRegistry::load(workgraph_dir)?;

    let tier = tier.parse::<ModelTier>()?;

    let entry = ModelEntry {
        id: id.to_string(),
        provider: provider.unwrap_or("openrouter").to_string(),
        cost_per_1m_input: cost_in,
        cost_per_1m_output: cost_out,
        context_window: context_window.unwrap_or(128_000),
        capabilities: capabilities.to_vec(),
        tier,
    };

    let existed = registry.get(id).is_some();
    registry.add(entry);
    registry.save(workgraph_dir)?;

    if existed {
        println!("Updated model: {}", id);
    } else {
        println!("Added model: {}", id);
    }

    Ok(())
}

/// Set the default model for the coordinator
pub fn run_set_default(workgraph_dir: &Path, id: &str) -> Result<()> {
    let mut registry = ModelRegistry::load(workgraph_dir)?;
    registry.set_default(id)?;
    registry.save(workgraph_dir)?;
    println!("Default model set to: {}", id);
    Ok(())
}

/// Initialize the models.yaml file with defaults if it doesn't exist
pub fn run_init(workgraph_dir: &Path) -> Result<()> {
    let path = workgraph_dir.join("models.yaml");
    if path.exists() {
        println!("models.yaml already exists. Use 'wg models list' to view.");
        return Ok(());
    }

    let registry = ModelRegistry::with_defaults();
    registry.save(workgraph_dir)?;
    println!(
        "Created models.yaml with {} default models.",
        registry.models.len()
    );
    Ok(())
}

// ── Remote model discovery ──────────────────────────────────────────────

use workgraph::executor::native::openai_client::{
    self, OpenRouterModel, fetch_openrouter_models_blocking,
};

/// Cache of remote model data, stored in `.workgraph/model_cache.json`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ModelCache {
    /// ISO 8601 timestamp of when the cache was last updated.
    fetched_at: String,
    /// The cached model list.
    models: Vec<OpenRouterModel>,
}

const CACHE_FILE: &str = "model_cache.json";
const CACHE_MAX_AGE_SECS: i64 = 3600; // 1 hour

fn cache_path(workgraph_dir: &Path) -> std::path::PathBuf {
    workgraph_dir.join(CACHE_FILE)
}

fn load_cache(workgraph_dir: &Path) -> Option<ModelCache> {
    let path = cache_path(workgraph_dir);
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_cache(workgraph_dir: &Path, models: &[OpenRouterModel]) -> Result<()> {
    let cache = ModelCache {
        fetched_at: chrono::Utc::now().to_rfc3339(),
        models: models.to_vec(),
    };
    let content = serde_json::to_string(&cache).context("Failed to serialize model cache")?;
    std::fs::write(cache_path(workgraph_dir), content).context("Failed to write model cache")?;
    Ok(())
}

fn is_cache_fresh(cache: &ModelCache) -> bool {
    if let Ok(fetched) = chrono::DateTime::parse_from_rfc3339(&cache.fetched_at) {
        let age = chrono::Utc::now().signed_duration_since(fetched);
        age.num_seconds() < CACHE_MAX_AGE_SECS
    } else {
        false
    }
}

/// Fetch models from the remote API, using the cache when available and fresh.
fn get_remote_models(workgraph_dir: &Path, no_cache: bool) -> Result<Vec<OpenRouterModel>> {
    // Try cache first
    if !no_cache
        && let Some(cache) = load_cache(workgraph_dir)
        && is_cache_fresh(&cache)
    {
        eprintln!("Using cached model list (fetched {})", cache.fetched_at);
        return Ok(cache.models);
    }

    // Resolve API key
    let api_key = openai_client::resolve_openai_api_key_from_dir(workgraph_dir)?;

    // Resolve base URL
    let base_url = std::env::var("OPENAI_BASE_URL")
        .or_else(|_| std::env::var("OPENROUTER_BASE_URL"))
        .ok();

    eprintln!("Fetching models from API...");
    let models = fetch_openrouter_models_blocking(&api_key, base_url.as_deref())?;

    // Save to cache
    if let Err(e) = save_cache(workgraph_dir, &models) {
        eprintln!("Warning: failed to cache models: {}", e);
    }

    eprintln!("Fetched {} models", models.len());
    Ok(models)
}

/// Search remote models by query string and optional filters.
pub fn run_search(
    workgraph_dir: &Path,
    query: &str,
    tools_only: bool,
    no_cache: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    let all_models = get_remote_models(workgraph_dir, no_cache)?;

    let query_lower = query.to_lowercase();
    let mut matches: Vec<&OpenRouterModel> = all_models
        .iter()
        .filter(|m| {
            let id_match = m.id.to_lowercase().contains(&query_lower);
            let name_match = m.name.to_lowercase().contains(&query_lower);
            let desc_match = m.description.to_lowercase().contains(&query_lower);
            (id_match || name_match || desc_match)
                && (!tools_only || m.supported_parameters.iter().any(|p| p == "tools"))
        })
        .collect();

    // Sort by id for deterministic output
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    matches.truncate(limit);

    if json {
        let json_val: Vec<_> = matches.iter().map(|m| model_to_json(m)).collect();
        println!("{}", serde_json::to_string_pretty(&json_val)?);
        return Ok(());
    }

    if matches.is_empty() {
        println!("No models matching '{}' found.", query);
        return Ok(());
    }

    println!(
        "{:<45} {:>12} {:>12} {:>8} TOOLS",
        "MODEL", "IN/1M", "OUT/1M", "CTX"
    );
    println!("{}", "-".repeat(95));

    for model in &matches {
        print_remote_model(model);
    }

    println!("\n{} model(s) found.", matches.len());
    Ok(())
}

/// List all models from the remote API.
pub fn run_list_remote(
    workgraph_dir: &Path,
    tools_only: bool,
    no_cache: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    let all_models = get_remote_models(workgraph_dir, no_cache)?;

    let mut models: Vec<&OpenRouterModel> = all_models
        .iter()
        .filter(|m| !tools_only || m.supported_parameters.iter().any(|p| p == "tools"))
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.truncate(limit);

    if json {
        let json_val: Vec<_> = models.iter().map(|m| model_to_json(m)).collect();
        println!("{}", serde_json::to_string_pretty(&json_val)?);
        return Ok(());
    }

    if models.is_empty() {
        println!("No models found.");
        return Ok(());
    }

    println!(
        "{:<45} {:>12} {:>12} {:>8} TOOLS",
        "MODEL", "IN/1M", "OUT/1M", "CTX"
    );
    println!("{}", "-".repeat(95));

    for model in &models {
        print_remote_model(model);
    }

    println!("\n{} model(s) listed.", models.len());
    Ok(())
}

fn model_to_json(m: &OpenRouterModel) -> serde_json::Value {
    let (cost_in, cost_out) = parse_pricing(m);
    serde_json::json!({
        "id": m.id,
        "name": m.name,
        "description": m.description,
        "context_length": m.context_length,
        "cost_per_1m_input": cost_in,
        "cost_per_1m_output": cost_out,
        "supports_tools": m.supported_parameters.iter().any(|p| p == "tools"),
        "supported_parameters": m.supported_parameters,
        "modality": m.architecture.as_ref().and_then(|a| a.modality.clone()),
    })
}

fn print_remote_model(model: &OpenRouterModel) {
    let (cost_in, cost_out) = parse_pricing(model);
    let ctx = model
        .context_length
        .map(format_context_window)
        .unwrap_or_else(|| "?".to_string());
    let tools = if model.supported_parameters.iter().any(|p| p == "tools") {
        "yes"
    } else {
        "no"
    };

    let cost_in_str = if cost_in > 0.0 {
        format!("${:.4}", cost_in)
    } else {
        "free".to_string()
    };
    let cost_out_str = if cost_out > 0.0 {
        format!("${:.4}", cost_out)
    } else {
        "free".to_string()
    };

    println!(
        "{:<45} {:>12} {:>12} {:>8} {}",
        model.id, cost_in_str, cost_out_str, ctx, tools,
    );
}

/// Parse per-token pricing strings to per-1M-token USD.
fn parse_pricing(model: &OpenRouterModel) -> (f64, f64) {
    let pricing = match &model.pricing {
        Some(p) => p,
        None => return (0.0, 0.0),
    };

    let cost_in = pricing
        .prompt
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|per_token| per_token * 1_000_000.0)
        .unwrap_or(0.0);

    let cost_out = pricing
        .completion
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|per_token| per_token * 1_000_000.0)
        .unwrap_or(0.0);

    (cost_in, cost_out)
}

fn format_context_window(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else {
        format!("{}k", tokens / 1_000)
    }
}

// ── Benchmark registry commands ─────────────────────────────────────────

use workgraph::model_benchmarks::{self, BenchmarkRegistry};

/// Fetch model data from OpenRouter and write the benchmark registry.
pub fn run_fetch(workgraph_dir: &Path, no_cache: bool) -> Result<()> {
    let or_models = get_remote_models(workgraph_dir, no_cache)?;
    eprintln!(
        "Building benchmark registry from {} models...",
        or_models.len()
    );

    let mut registry = model_benchmarks::build_from_openrouter(&or_models);

    // If an existing registry has benchmark scores, preserve them.
    if let Some(existing) = BenchmarkRegistry::load(workgraph_dir)? {
        for (id, existing_model) in &existing.models {
            if let Some(new_model) = registry.models.get_mut(id) {
                // Preserve any benchmark scores that were manually or externally added.
                if existing_model.benchmarks.coding_index.is_some()
                    || existing_model.benchmarks.intelligence_index.is_some()
                    || existing_model.benchmarks.agentic.is_some()
                {
                    new_model.benchmarks = existing_model.benchmarks.clone();
                }
                if existing_model.popularity.provider_count.is_some()
                    || existing_model.popularity.weekly_rank.is_some()
                    || existing_model.popularity.request_count.is_some()
                {
                    new_model.popularity = existing_model.popularity.clone();
                }
            }
        }
    }

    // Compute fitness scores.
    model_benchmarks::compute_fitness_scores(&mut registry);

    let model_count = registry.models.len();
    let scored_count = registry
        .models
        .values()
        .filter(|m| m.fitness.score.is_some())
        .count();
    let tier_counts = count_tiers(&registry);

    registry.save(workgraph_dir)?;

    println!("Benchmark registry updated: {} models", model_count);
    if scored_count > 0 {
        println!("  Scored: {} models with fitness data", scored_count);
    }
    println!(
        "  Tiers: {} frontier, {} mid, {} budget",
        tier_counts.0, tier_counts.1, tier_counts.2
    );
    println!(
        "  Saved to: {}/model_benchmarks.json",
        workgraph_dir.display()
    );

    if scored_count == 0 {
        eprintln!();
        eprintln!(
            "Warning: No models have benchmark scores. The dynamic profile (`wg profile set \
             openrouter`) will not be able to rank models meaningfully."
        );
        eprintln!("  This can happen if no well-known models were found in the API response.");
        eprintln!(
            "  Consider using a static profile (`wg profile set anthropic`) or adding \
             scores manually."
        );
    }

    Ok(())
}

/// Display the benchmark registry with fitness scores and tiers.
pub fn run_benchmarks(
    workgraph_dir: &Path,
    tier: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let registry = BenchmarkRegistry::load(workgraph_dir)?
        .context("No benchmark registry found. Run `wg models fetch` first.")?;

    let models: Vec<_> = if let Some(tier_filter) = tier {
        registry
            .ranked_by_tier(tier_filter)
            .into_iter()
            .take(limit)
            .collect()
    } else {
        registry.ranked().into_iter().take(limit).collect()
    };

    if json {
        let json_val: Vec<serde_json::Value> = models
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "name": m.name,
                    "tier": m.tier,
                    "fitness_score": m.fitness.score,
                    "quality": m.fitness.components.quality,
                    "value": m.fitness.components.value,
                    "reliability": m.fitness.components.reliability,
                    "input_per_mtok": m.pricing.input_per_mtok,
                    "output_per_mtok": m.pricing.output_per_mtok,
                    "context_window": m.context_window,
                    "supports_tools": m.supports_tools,
                    "benchmarks": {
                        "coding_index": m.benchmarks.coding_index,
                        "intelligence_index": m.benchmarks.intelligence_index,
                        "agentic": m.benchmarks.agentic,
                    },
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_val)?);
        return Ok(());
    }

    if models.is_empty() {
        println!("No models found.");
        return Ok(());
    }

    println!(
        "{:<42} {:<10} {:>8} {:>10} {:>11} {:>8} TOOLS",
        "MODEL", "TIER", "FITNESS", "IN/1M", "OUT/1M", "CTX"
    );
    println!("{}", "-".repeat(105));

    for model in &models {
        let fitness_str = model
            .fitness
            .score
            .map(|s| format!("{:.1}", s))
            .unwrap_or_else(|| "—".to_string());
        let ctx = model
            .context_window
            .map(format_context_window)
            .unwrap_or_else(|| "?".to_string());
        let tools = if model.supports_tools { "yes" } else { "no" };
        let cost_in = if model.pricing.input_per_mtok > 0.0 {
            format!("${:.4}", model.pricing.input_per_mtok)
        } else {
            "free".to_string()
        };
        let cost_out = if model.pricing.output_per_mtok > 0.0 {
            format!("${:.4}", model.pricing.output_per_mtok)
        } else {
            "free".to_string()
        };

        println!(
            "{:<42} {:<10} {:>8} {:>10} {:>11} {:>8} {}",
            model.id, model.tier, fitness_str, cost_in, cost_out, ctx, tools,
        );
    }

    println!(
        "\n{} model(s) shown. Fetched: {}",
        models.len(),
        registry.fetched_at
    );

    Ok(())
}

fn count_tiers(registry: &BenchmarkRegistry) -> (usize, usize, usize) {
    let mut frontier = 0;
    let mut mid = 0;
    let mut budget = 0;
    for model in registry.models.values() {
        match model.tier.as_str() {
            "frontier" => frontier += 1,
            "mid" => mid += 1,
            _ => budget += 1,
        }
    }
    (frontier, mid, budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openai_client::{OpenRouterArchitecture, OpenRouterPricing};

    fn sample_models() -> Vec<OpenRouterModel> {
        vec![
            OpenRouterModel {
                id: "anthropic/claude-sonnet-4-latest".into(),
                name: "Claude Sonnet 4.6".into(),
                description: "A balanced model for coding and analysis".into(),
                context_length: Some(200_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0.000003".into()),
                    completion: Some("0.000015".into()),
                }),
                supported_parameters: vec!["temperature".into(), "tools".into()],
                architecture: Some(OpenRouterArchitecture {
                    modality: Some("text->text".into()),
                    tokenizer: Some("claude".into()),
                }),
                top_provider: None,
            },
            OpenRouterModel {
                id: "openai/gpt-4o".into(),
                name: "GPT-4o".into(),
                description: "OpenAI flagship model".into(),
                context_length: Some(128_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0.0000025".into()),
                    completion: Some("0.00001".into()),
                }),
                supported_parameters: vec!["temperature".into(), "tools".into()],
                architecture: None,
                top_provider: None,
            },
            OpenRouterModel {
                id: "deepseek/deepseek-r1".into(),
                name: "DeepSeek R1".into(),
                description: "Reasoning model without tool support".into(),
                context_length: Some(164_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0.00000055".into()),
                    completion: Some("0.00000219".into()),
                }),
                supported_parameters: vec!["temperature".into()],
                architecture: None,
                top_provider: None,
            },
            OpenRouterModel {
                id: "meta-llama/llama-4-maverick:free".into(),
                name: "Llama 4 Maverick (free)".into(),
                description: "Free tier Meta model".into(),
                context_length: Some(1_000_000),
                pricing: Some(OpenRouterPricing {
                    prompt: Some("0".into()),
                    completion: Some("0".into()),
                }),
                supported_parameters: vec!["temperature".into(), "tools".into()],
                architecture: None,
                top_provider: None,
            },
        ]
    }

    #[test]
    fn test_model_search_filters_by_query() {
        let models = sample_models();
        let query = "claude";
        let query_lower = query.to_lowercase();
        let matches: Vec<_> = models
            .iter()
            .filter(|m| {
                m.id.to_lowercase().contains(&query_lower)
                    || m.name.to_lowercase().contains(&query_lower)
            })
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "anthropic/claude-sonnet-4-latest");
    }

    #[test]
    fn test_model_search_filters_by_tools() {
        let models = sample_models();
        let with_tools: Vec<_> = models
            .iter()
            .filter(|m| m.supported_parameters.iter().any(|p| p == "tools"))
            .collect();
        assert_eq!(with_tools.len(), 3);
        // DeepSeek R1 should not appear
        assert!(with_tools.iter().all(|m| m.id != "deepseek/deepseek-r1"));
    }

    #[test]
    fn test_parse_pricing() {
        let models = sample_models();
        let (cost_in, cost_out) = parse_pricing(&models[0]); // Claude Sonnet
        assert!((cost_in - 3.0).abs() < 0.01);
        assert!((cost_out - 15.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_pricing_free() {
        let models = sample_models();
        let (cost_in, cost_out) = parse_pricing(&models[3]); // Llama free
        assert_eq!(cost_in, 0.0);
        assert_eq!(cost_out, 0.0);
    }

    #[test]
    fn test_parse_pricing_none() {
        let model = OpenRouterModel {
            id: "test/no-pricing".into(),
            name: "Test".into(),
            description: "".into(),
            context_length: None,
            pricing: None,
            supported_parameters: vec![],
            architecture: None,
            top_provider: None,
        };
        let (cost_in, cost_out) = parse_pricing(&model);
        assert_eq!(cost_in, 0.0);
        assert_eq!(cost_out, 0.0);
    }

    #[test]
    fn test_model_cache_serialization() {
        let models = sample_models();
        let cache = ModelCache {
            fetched_at: "2026-03-08T12:00:00Z".into(),
            models: models.clone(),
        };
        let json = serde_json::to_string(&cache).unwrap();
        let parsed: ModelCache = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.models.len(), models.len());
        assert_eq!(parsed.fetched_at, "2026-03-08T12:00:00Z");
    }

    #[test]
    fn test_cache_freshness() {
        let fresh = ModelCache {
            fetched_at: chrono::Utc::now().to_rfc3339(),
            models: vec![],
        };
        assert!(is_cache_fresh(&fresh));

        let stale = ModelCache {
            fetched_at: "2020-01-01T00:00:00Z".into(),
            models: vec![],
        };
        assert!(!is_cache_fresh(&stale));
    }

    #[test]
    fn test_cache_save_and_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = sample_models();
        save_cache(dir.path(), &models).unwrap();

        let loaded = load_cache(dir.path()).unwrap();
        assert_eq!(loaded.models.len(), models.len());
        assert!(is_cache_fresh(&loaded));
    }

    #[test]
    fn test_format_context_window() {
        assert_eq!(format_context_window(128_000), "128k");
        assert_eq!(format_context_window(1_000_000), "1M");
        assert_eq!(format_context_window(200_000), "200k");
    }

    #[test]
    fn test_model_to_json() {
        let models = sample_models();
        let json = model_to_json(&models[0]);
        assert_eq!(json["id"], "anthropic/claude-sonnet-4-latest");
        assert_eq!(json["supports_tools"], true);
        assert!((json["cost_per_1m_input"].as_f64().unwrap() - 3.0).abs() < 0.01);
    }
}
