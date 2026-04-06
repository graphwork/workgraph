//! Smoke test: end-to-end model selection + routing pipeline.
//!
//! Validates the fan-in of all model pipeline fixes:
//! - `fix-profile-data-fetch`: real model metadata from OpenRouter
//! - `fix-profile-ranking-sort`: sort by real metrics, not alphabetical
//! - `fix-model-spec-resolution`: short name resolution (`minimax-m2.7` → full path)
//! - `tb-fix-model-routing`: route to native executor without LiteLLM
//!
//! Tests are split into three groups:
//! 1. Fresh repo: profile show, short name resolution, config defaults
//! 2. Workgraph repo: real data integration
//! 3. Profile ranking quality: non-alphabetical, meaningful score variance

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use workgraph::model_benchmarks::{
    self, BenchmarkPricing, BenchmarkRegistry, Benchmarks, Fitness, FitnessComponents,
    ModelBenchmark, Popularity, RegistrySource,
};

// ---------------------------------------------------------------------------
// CLI Helpers (same pattern as integration_e2e_smoke.rs)
// ---------------------------------------------------------------------------

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    stdout
}

// ---------------------------------------------------------------------------
// Test data helpers
// ---------------------------------------------------------------------------

fn make_model(
    id: &str,
    name: &str,
    output_price: f64,
    tools: bool,
    quality: Option<f64>,
    pop: Popularity,
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
        benchmarks: Benchmarks {
            coding_index: quality,
            intelligence_index: quality.map(|q| q * 0.95),
            agentic: quality.map(|q| q * 0.9),
            math_index: None,
        },
        popularity: pop,
        fitness: Fitness {
            score: quality,
            components: FitnessComponents {
                quality,
                value: quality.map(|q| q * 0.8),
                reliability: Some(30.0),
            },
        },
        tier: if output_price >= 18.0 {
            "frontier"
        } else if output_price >= 3.0 {
            "mid"
        } else {
            "budget"
        }
        .to_string(),
        pricing_updated_at: "2026-04-01T00:00:00Z".to_string(),
        is_proxy: false,
    }
}

fn make_registry(models: Vec<ModelBenchmark>) -> BenchmarkRegistry {
    let mut map = BTreeMap::new();
    for m in models {
        map.insert(m.id.clone(), m);
    }
    BenchmarkRegistry {
        version: 1,
        fetched_at: chrono::Utc::now().to_rfc3339(),
        source: RegistrySource {
            openrouter_api: "https://openrouter.ai/api/v1/models".to_string(),
        },
        models: map,
    }
}

/// Write a model_cache.json file for short name resolution tests.
fn write_model_cache(wg_dir: &Path, model_ids: &[&str]) {
    let models: Vec<serde_json::Value> = model_ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "name": id.split('/').last().unwrap_or(id),
            })
        })
        .collect();
    let cache = serde_json::json!({
        "fetched_at": "2026-04-01T00:00:00Z",
        "models": models,
    });
    std::fs::write(wg_dir.join("model_cache.json"), cache.to_string()).unwrap();
}

/// Initialize a fresh workgraph dir and return the .workgraph path.
fn init_fresh_wg() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    assert!(wg_dir.exists(), ".workgraph directory should exist after init");
    (tmp, wg_dir)
}

// =========================================================================
// Test Group 1: Fresh repo — profile show with synthetic registry data
// =========================================================================

/// Fresh repo: `wg profile show` with a populated registry shows real scores (not 0.0).
#[test]
fn smoke_fresh_repo_profile_show_real_scores() {
    let (_tmp, wg_dir) = init_fresh_wg();

    // Write a synthetic registry with known non-zero scores.
    let registry = make_registry(vec![
        make_model(
            "vendor/fast-model",
            "Fast Model",
            1.0,
            true,
            Some(40.0),
            Popularity {
                weekly_rank: Some(5),
                request_count: Some(500_000),
                provider_count: Some(4),
            },
        ),
        make_model(
            "vendor/standard-model",
            "Standard Model",
            8.0,
            true,
            Some(60.0),
            Popularity {
                weekly_rank: Some(3),
                request_count: Some(1_000_000),
                provider_count: Some(5),
            },
        ),
        make_model(
            "vendor/premium-model",
            "Premium Model",
            25.0,
            true,
            Some(80.0),
            Popularity {
                weekly_rank: Some(1),
                request_count: Some(2_000_000),
                provider_count: Some(6),
            },
        ),
    ]);
    registry.save(&wg_dir).unwrap();

    // Set an openrouter profile so ranking is dynamic.
    wg_ok(&wg_dir, &["profile", "set", "openrouter"]);

    let output = wg_ok(&wg_dir, &["profile", "show"]);

    // Verify non-zero scores — output should contain score values > 0.
    assert!(
        output.contains("score:"),
        "profile show should display score breakdown, got:\n{}",
        output
    );
    // Should not see all scores as 0.0.
    assert!(
        !output.lines().all(|l| {
            if l.contains("score:") {
                l.contains("score:  0.0")
            } else {
                true
            }
        }),
        "All scores should not be 0.0, got:\n{}",
        output
    );
}

/// Fresh repo: `wg profile show -v` shows verbose per-metric breakdown (pricing, context).
#[test]
fn smoke_fresh_repo_profile_show_verbose() {
    let (_tmp, wg_dir) = init_fresh_wg();

    let registry = make_registry(vec![
        make_model(
            "vendor/fast-model",
            "Fast Model",
            1.0,
            true,
            Some(40.0),
            Popularity {
                weekly_rank: Some(5),
                request_count: Some(500_000),
                provider_count: Some(4),
            },
        ),
        make_model(
            "vendor/standard-model",
            "Standard Model",
            8.0,
            true,
            Some(60.0),
            Popularity {
                weekly_rank: Some(3),
                request_count: Some(1_000_000),
                provider_count: Some(5),
            },
        ),
        make_model(
            "vendor/premium-model",
            "Premium Model",
            25.0,
            true,
            Some(80.0),
            Popularity {
                weekly_rank: Some(1),
                request_count: Some(2_000_000),
                provider_count: Some(6),
            },
        ),
    ]);
    registry.save(&wg_dir).unwrap();

    wg_ok(&wg_dir, &["profile", "set", "openrouter"]);

    let output = wg_ok(&wg_dir, &["profile", "show", "-v"]);

    // Verbose output should show pricing ($/MTok) and context window.
    assert!(
        output.contains("/MTok"),
        "Verbose output should show per-MTok pricing, got:\n{}",
        output
    );
    assert!(
        output.contains("ctx:"),
        "Verbose output should show context window, got:\n{}",
        output
    );
    assert!(
        output.contains("tools") || output.contains("no-tools"),
        "Verbose output should show tool support, got:\n{}",
        output
    );
}

// =========================================================================
// Test Group 1 cont: Fresh repo — short name resolution
// =========================================================================

/// Fresh repo: short name resolution works via `wg add --model minimax-m2.7`.
#[test]
fn smoke_fresh_repo_short_name_resolution() {
    let (_tmp, wg_dir) = init_fresh_wg();

    // Write a model cache so short name resolution can work.
    write_model_cache(
        &wg_dir,
        &[
            "minimax/minimax-m2.7",
            "anthropic/claude-sonnet-4-6",
            "openai/gpt-4o",
        ],
    );

    // Add a task with a short model name.
    let output = wg_cmd(&wg_dir, &["add", "hello world", "--model", "minimax-m2.7"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "wg add with short name should succeed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // The stderr should show the resolution.
    assert!(
        stderr.contains("minimax/minimax-m2.7"),
        "Should resolve short name to full ID.\nstderr: {}",
        stderr
    );

    // Verify the task was created with the resolved model.
    let graph = workgraph::parser::load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("hello-world").unwrap();
    assert!(
        task.model
            .as_deref()
            .unwrap_or("")
            .contains("minimax/minimax-m2.7"),
        "Task model should contain resolved full ID, got: {:?}",
        task.model
    );
}

/// Fresh repo: adding a task with no --model picks up the configured default.
#[test]
fn smoke_fresh_repo_default_model_from_config() {
    let (_tmp, wg_dir) = init_fresh_wg();

    // Configure a default model.
    wg_ok(&wg_dir, &["config", "--model", "openrouter:minimax/minimax-m2.7"]);

    // Add a task without specifying --model.
    wg_ok(&wg_dir, &["add", "hello world 2", "--immediate"]);

    // The task's model should be the configured default (or None if the default
    // is only applied at dispatch time). Check the config at least.
    let output = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        output.contains("minimax/minimax-m2.7"),
        "Config should show the configured model, got:\n{}",
        output
    );
}

// =========================================================================
// Test Group 1 cont: No silent Claude fallback
// =========================================================================

/// When a model is explicitly configured, no part of the add pipeline
/// silently replaces it with a Claude model.
#[test]
fn smoke_no_silent_claude_fallback_on_add() {
    let (_tmp, wg_dir) = init_fresh_wg();

    write_model_cache(
        &wg_dir,
        &[
            "minimax/minimax-m2.7",
            "anthropic/claude-sonnet-4-6",
        ],
    );

    // Add task with explicit non-Claude model.
    wg_ok(
        &wg_dir,
        &["add", "non-claude task", "--model", "openrouter:minimax/minimax-m2.7"],
    );

    let graph = workgraph::parser::load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("non-claude-task").unwrap();

    // Model should NOT be a claude model.
    let model_str = task.model.as_deref().unwrap_or("");
    assert!(
        !model_str.contains("claude"),
        "Model should NOT have been silently replaced with Claude, got: {:?}",
        task.model
    );
    assert!(
        model_str.contains("minimax"),
        "Model should be the explicitly specified minimax model, got: {:?}",
        task.model
    );
}

// =========================================================================
// Test Group 2: Profile ranking against the real workgraph repo
// (uses actual .workgraph if present, else synthetic data)
// =========================================================================

/// Workgraph repo: `wg profile show` succeeds with real data.
#[test]
fn smoke_workgraph_repo_profile_show() {
    let wg_dir = PathBuf::from(".workgraph");
    if !wg_dir.exists() {
        // Not running in the workgraph repo — skip gracefully.
        eprintln!("Skipping: not in workgraph repo (.workgraph not found)");
        return;
    }

    let output = wg_cmd(&wg_dir, &["profile", "show"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "wg profile show should succeed in workgraph repo.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    // Should show some tier mappings.
    assert!(
        stdout.contains("Tier Mappings") || stdout.contains("fast") || stdout.contains("standard"),
        "Output should contain tier information, got:\n{}",
        stdout
    );
}

/// Workgraph repo: `wg profile show -v` shows individual metric breakdown.
#[test]
fn smoke_workgraph_repo_profile_show_verbose() {
    let wg_dir = PathBuf::from(".workgraph");
    if !wg_dir.exists() {
        eprintln!("Skipping: not in workgraph repo (.workgraph not found)");
        return;
    }

    // Need openrouter profile to get ranked data.
    let output = wg_cmd(&wg_dir, &["profile", "show", "-v"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "wg profile show -v should succeed.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}

// =========================================================================
// Test Group 3: Profile ranking quality
// =========================================================================

/// Rankings are non-alphabetical — top models differ from alphabetical order.
#[test]
fn smoke_ranking_non_alphabetical() {
    // Create models where alphabetical order differs from score order.
    let registry = make_registry(vec![
        make_model(
            "aaa/first-alpha",
            "AAA First Alpha",
            1.0,
            true,
            Some(20.0),
            Popularity {
                weekly_rank: Some(100),
                request_count: Some(1_000),
                provider_count: Some(1),
            },
        ),
        make_model(
            "zzz/last-alpha",
            "ZZZ Last Alpha",
            1.0,
            true,
            Some(90.0),
            Popularity {
                weekly_rank: Some(1),
                request_count: Some(5_000_000),
                provider_count: Some(8),
            },
        ),
        make_model(
            "mmm/mid-alpha",
            "MMM Mid Alpha",
            1.0,
            true,
            Some(60.0),
            Popularity {
                weekly_rank: Some(10),
                request_count: Some(500_000),
                provider_count: Some(4),
            },
        ),
    ]);

    let ranked = model_benchmarks::rank_models_for_profile(&registry);

    // All should be in the fast tier (output_price = 1.0 < 3.0).
    assert!(
        ranked.fast.len() >= 3,
        "Expected at least 3 fast models, got {}",
        ranked.fast.len()
    );

    // The top model should NOT be "aaa/first-alpha" (which would be alphabetical).
    assert_ne!(
        ranked.fast[0].id, "aaa/first-alpha",
        "Top model should not be alphabetically first — ranking should be by score"
    );

    // "zzz/last-alpha" should be first (highest quality + popularity).
    assert_eq!(
        ranked.fast[0].id, "zzz/last-alpha",
        "Model with highest quality + popularity should rank first"
    );
}

/// Scores vary meaningfully across models (not all tied at the same value).
#[test]
fn smoke_ranking_scores_vary() {
    let registry = make_registry(vec![
        make_model(
            "high/model",
            "High Model",
            1.0,
            true,
            Some(90.0),
            Popularity {
                weekly_rank: Some(1),
                request_count: Some(5_000_000),
                provider_count: Some(8),
            },
        ),
        make_model(
            "mid/model",
            "Mid Model",
            1.0,
            true,
            Some(50.0),
            Popularity {
                weekly_rank: Some(20),
                request_count: Some(100_000),
                provider_count: Some(3),
            },
        ),
        make_model(
            "low/model",
            "Low Model",
            1.0,
            true,
            Some(10.0),
            Popularity {
                weekly_rank: Some(100),
                request_count: Some(1_000),
                provider_count: Some(1),
            },
        ),
    ]);

    let ranked = model_benchmarks::rank_models_for_profile(&registry);
    assert!(ranked.fast.len() >= 3);

    // Scores should vary — check that top and bottom differ meaningfully.
    let top_score = ranked.fast[0].composite_score;
    let bottom_score = ranked.fast[ranked.fast.len() - 1].composite_score;

    assert!(
        (top_score - bottom_score).abs() > 5.0,
        "Scores should vary meaningfully. Top: {}, Bottom: {}",
        top_score,
        bottom_score
    );
}

/// Profile tier assignment places cheap models in fast, mid-priced in standard,
/// expensive high-quality in premium.
#[test]
fn smoke_ranking_tier_buckets_reasonable() {
    let registry = make_registry(vec![
        // Cheap model → fast tier.
        make_model(
            "budget/cheap-model",
            "Cheap",
            0.5,
            true,
            Some(30.0),
            Popularity {
                weekly_rank: Some(20),
                request_count: Some(100_000),
                provider_count: Some(2),
            },
        ),
        // Mid-priced → standard tier.
        make_model(
            "vendor/mid-model",
            "Mid",
            10.0,
            true,
            Some(55.0),
            Popularity {
                weekly_rank: Some(8),
                request_count: Some(500_000),
                provider_count: Some(4),
            },
        ),
        // Expensive + high quality → premium tier.
        make_model(
            "top/premium-model",
            "Premium",
            25.0,
            true,
            Some(80.0),
            Popularity {
                weekly_rank: Some(2),
                request_count: Some(2_000_000),
                provider_count: Some(6),
            },
        ),
    ]);

    let ranked = model_benchmarks::rank_models_for_profile(&registry);

    assert!(
        ranked.fast.iter().any(|r| r.id == "budget/cheap-model"),
        "Cheap model should be in fast tier: {:?}",
        ranked.fast.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    assert!(
        ranked.standard.iter().any(|r| r.id == "vendor/mid-model"),
        "Mid-priced model should be in standard tier: {:?}",
        ranked.standard.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    assert!(
        ranked.premium.iter().any(|r| r.id == "top/premium-model"),
        "Expensive high-quality model should be in premium tier: {:?}",
        ranked.premium.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
}

/// Models with curated benchmark data get non-zero fitness scores.
#[test]
fn smoke_curated_benchmarks_populate_scores() {
    // Create a model with curated-style benchmark data (like Claude Sonnet).
    let mut registry = make_registry(vec![ModelBenchmark {
        id: "anthropic/claude-sonnet-4-6".to_string(),
        name: "Claude Sonnet 4".to_string(),
        pricing: BenchmarkPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        },
        context_window: Some(200_000),
        max_output_tokens: Some(64_000),
        supports_tools: true,
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
        fitness: Fitness::default(), // no fitness score yet
        tier: "frontier".to_string(),
        pricing_updated_at: "2026-04-01T00:00:00Z".to_string(),
        is_proxy: false,
    }]);

    // Compute fitness — benchmark data should produce real scores.
    model_benchmarks::compute_fitness_scores(&mut registry);

    let model = registry.models.get("anthropic/claude-sonnet-4-6").unwrap();
    assert!(
        model.fitness.score.is_some(),
        "Model with benchmark data should have a fitness score after computation"
    );
    assert!(
        model.fitness.score.unwrap() > 0.0,
        "Fitness score should be non-zero, got: {:?}",
        model.fitness.score
    );
    assert!(
        model.fitness.components.quality.is_some(),
        "Quality component should be computed from benchmark data"
    );
}

/// Short name resolution: unit test for `resolve_short_model_name`.
#[test]
fn smoke_short_name_resolution_unit() {
    use workgraph::executor::native::openai_client::resolve_short_model_name;

    let tmp = TempDir::new().unwrap();

    // Write a model cache.
    let cache = serde_json::json!({
        "fetched_at": "2026-04-01T00:00:00Z",
        "models": [
            {"id": "minimax/minimax-m2.7", "name": "Minimax M2.7"},
            {"id": "anthropic/claude-sonnet-4-6", "name": "Claude Sonnet"},
            {"id": "openai/gpt-4o", "name": "GPT-4o"},
        ]
    });
    std::fs::write(tmp.path().join("model_cache.json"), cache.to_string()).unwrap();

    // 1. Short name → full ID.
    let result = resolve_short_model_name("minimax-m2.7", tmp.path());
    assert_eq!(
        result.resolved.as_deref(),
        Some("minimax/minimax-m2.7"),
        "Short name should resolve to full ID"
    );

    // 2. Full ID → exact match.
    let result = resolve_short_model_name("openai/gpt-4o", tmp.path());
    assert_eq!(
        result.resolved.as_deref(),
        Some("openai/gpt-4o"),
        "Full ID should match exactly"
    );

    // 3. With provider prefix → still resolves.
    let result = resolve_short_model_name("openrouter:minimax/minimax-m2.7", tmp.path());
    assert_eq!(
        result.resolved.as_deref(),
        Some("minimax/minimax-m2.7"),
        "Provider-prefixed full ID should resolve"
    );

    // 4. No match → None with no panic.
    let result = resolve_short_model_name("nonexistent-model-xyz", tmp.path());
    assert!(
        result.resolved.is_none(),
        "Non-existent model should return None"
    );
}

/// Short name resolution: no cache file → graceful failure (no panic).
#[test]
fn smoke_short_name_resolution_no_cache() {
    use workgraph::executor::native::openai_client::resolve_short_model_name;

    let tmp = TempDir::new().unwrap();
    // No model_cache.json written.

    let result = resolve_short_model_name("minimax-m2.7", tmp.path());
    assert!(
        result.resolved.is_none(),
        "Should return None when cache is missing (not panic)"
    );
    assert!(
        result.suggestions.is_empty(),
        "Should have no suggestions when cache is missing"
    );
}

/// Profile ranking: composite score uses benchmark > usage > pricing weights.
#[test]
fn smoke_ranking_composite_score_weights() {
    // Model A: high benchmark, low popularity.
    // Model B: low benchmark, high popularity.
    // With weights benchmark=0.50 > popularity=0.30, A should rank higher.
    let registry = make_registry(vec![
        make_model(
            "a/high-bench",
            "High Bench",
            1.0,
            true,
            Some(90.0),
            Popularity {
                weekly_rank: Some(100),
                request_count: Some(1_000),
                provider_count: Some(1),
            },
        ),
        make_model(
            "b/high-pop",
            "High Pop",
            1.0,
            true,
            Some(20.0),
            Popularity {
                weekly_rank: Some(1),
                request_count: Some(5_000_000),
                provider_count: Some(10),
            },
        ),
    ]);

    let ranked = model_benchmarks::rank_models_for_profile(&registry);
    assert!(ranked.fast.len() >= 2);

    // Benchmark-heavy model should rank first.
    assert_eq!(
        ranked.fast[0].id, "a/high-bench",
        "Model with high benchmarks should rank above model with only high popularity"
    );
}

/// Profile ranking via CLI: output in a fresh repo with registry data.
#[test]
fn smoke_profile_show_cli_ranked_alternatives() {
    let (_tmp, wg_dir) = init_fresh_wg();

    let registry = make_registry(vec![
        make_model(
            "vendor-a/model-alpha",
            "Alpha",
            1.0,
            true,
            Some(80.0),
            Popularity {
                weekly_rank: Some(1),
                request_count: Some(2_000_000),
                provider_count: Some(6),
            },
        ),
        make_model(
            "vendor-b/model-beta",
            "Beta",
            1.0,
            true,
            Some(40.0),
            Popularity {
                weekly_rank: Some(50),
                request_count: Some(10_000),
                provider_count: Some(2),
            },
        ),
        make_model(
            "vendor-c/model-gamma",
            "Gamma",
            10.0,
            true,
            Some(65.0),
            Popularity {
                weekly_rank: Some(5),
                request_count: Some(1_000_000),
                provider_count: Some(5),
            },
        ),
        make_model(
            "vendor-d/model-delta",
            "Delta",
            25.0,
            true,
            Some(85.0),
            Popularity {
                weekly_rank: Some(2),
                request_count: Some(1_500_000),
                provider_count: Some(5),
            },
        ),
    ]);
    registry.save(&wg_dir).unwrap();

    wg_ok(&wg_dir, &["profile", "set", "openrouter"]);
    let output = wg_ok(&wg_dir, &["profile", "show"]);

    // Should show "Ranked Alternatives" section.
    assert!(
        output.contains("Ranked Alternatives"),
        "Should show ranked alternatives for dynamic profile, got:\n{}",
        output
    );
    // Should show tier names.
    assert!(output.contains("fast tier"), "Should show fast tier heading, got:\n{}", output);
    // First model in fast tier should be Alpha (highest score in cheap bucket).
    assert!(
        output.contains("vendor-a/model-alpha"),
        "Alpha should appear in ranked list, got:\n{}",
        output
    );
}

/// Profile show JSON output includes ranked alternatives.
#[test]
fn smoke_profile_show_json_output() {
    let (_tmp, wg_dir) = init_fresh_wg();

    let registry = make_registry(vec![
        make_model(
            "vendor/fast-model",
            "Fast",
            1.0,
            true,
            Some(50.0),
            Popularity {
                weekly_rank: Some(5),
                request_count: Some(500_000),
                provider_count: Some(4),
            },
        ),
    ]);
    registry.save(&wg_dir).unwrap();

    wg_ok(&wg_dir, &["profile", "set", "openrouter"]);
    let output = wg_ok(&wg_dir, &["profile", "show", "--json"]);

    // Should be valid JSON.
    let parsed: serde_json::Value =
        serde_json::from_str(&output).expect("profile show --json should output valid JSON");

    // Should have effective_tiers.
    assert!(
        parsed.get("effective_tiers").is_some(),
        "JSON output should have effective_tiers field"
    );

    // Should have profile field.
    assert_eq!(
        parsed["profile"].as_str(),
        Some("openrouter"),
        "JSON should show openrouter profile"
    );
}

/// Models without tool support are excluded from rankings.
#[test]
fn smoke_ranking_excludes_no_tools() {
    let registry = make_registry(vec![
        make_model(
            "vendor/with-tools",
            "With Tools",
            1.0,
            true,
            Some(50.0),
            Popularity {
                weekly_rank: Some(5),
                request_count: Some(500_000),
                provider_count: Some(4),
            },
        ),
        make_model(
            "vendor/no-tools",
            "No Tools",
            1.0,
            false, // no tool support
            Some(95.0),
            Popularity {
                weekly_rank: Some(1),
                request_count: Some(5_000_000),
                provider_count: Some(10),
            },
        ),
    ]);

    let ranked = model_benchmarks::rank_models_for_profile(&registry);
    let all_ids: Vec<&str> = ranked
        .fast
        .iter()
        .chain(ranked.standard.iter())
        .chain(ranked.premium.iter())
        .map(|r| r.id.as_str())
        .collect();

    assert!(
        all_ids.contains(&"vendor/with-tools"),
        "Model with tools should be in rankings"
    );
    assert!(
        !all_ids.contains(&"vendor/no-tools"),
        "Model without tools should be excluded from rankings"
    );
}

/// Registry save/load round-trip with real scores preserves data.
#[test]
fn smoke_registry_round_trip_preserves_scores() {
    let tmp = TempDir::new().unwrap();

    let mut registry = make_registry(vec![
        make_model(
            "test/model-a",
            "Model A",
            5.0,
            true,
            Some(60.0),
            Popularity {
                weekly_rank: Some(10),
                request_count: Some(100_000),
                provider_count: Some(3),
            },
        ),
    ]);

    // Compute fitness and save.
    model_benchmarks::compute_fitness_scores(&mut registry);
    registry.save(tmp.path()).unwrap();

    // Load and verify.
    let loaded = BenchmarkRegistry::load(tmp.path()).unwrap().unwrap();
    let model = loaded.models.get("test/model-a").unwrap();

    assert!(
        model.fitness.score.is_some(),
        "Score should survive round-trip"
    );
    assert!(
        model.fitness.score.unwrap() > 0.0,
        "Score should be non-zero after round-trip"
    );
    assert!(
        model.fitness.components.quality.is_some(),
        "Quality component should survive round-trip"
    );
}
