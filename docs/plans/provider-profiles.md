# Design: Provider Profiles with OpenRouter Auto-Configuration

**Task:** `profile-design`
**Date:** 2026-04-01
**Status:** Design document

---

## 1. Overview

A **profile** is a named provider configuration that sets all model tiers at once.
Profiles eliminate the need to manually configure `tiers.fast`, `tiers.standard`,
and `tiers.premium` individually. They come in two flavors:

- **Static profiles** — Hardcoded tier mappings (e.g., `anthropic` always maps to Claude models)
- **Dynamic profiles** — Auto-select best models per tier by querying the benchmark registry (e.g., `openrouter`)

### Design Goals

1. One command to switch all model routing: `wg config --profile openrouter`
2. Static profiles work offline with no registry data
3. Dynamic profiles leverage `model_benchmarks.json` to follow the market
4. Per-role overrides (`models.<role>.model`) still take precedence over profiles
5. Minimal config.toml footprint — a single `profile = "openrouter"` field

---

## 2. Profile Schema

### 2.1 Config Representation

```toml
# config.toml

# Active profile name (optional — when set, provides tier defaults)
profile = "openrouter"
```

A single string field on the top-level `Config` struct. When set, the profile
supplies tier mappings that are used as defaults, but explicit `[tiers]` entries
and `[models]` per-role overrides still take precedence.

### 2.2 Profile Definition (Rust)

```rust
/// A provider profile: a named configuration that maps quality tiers to models.
pub struct Profile {
    /// Unique identifier (e.g., "anthropic", "openrouter", "openai")
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// How tier mappings are determined
    pub strategy: ProfileStrategy,
}

pub enum ProfileStrategy {
    /// Hardcoded tier → model mappings
    Static {
        /// Tier mappings: fast/standard/premium → model registry ID
        tiers: TierConfig,
    },
    /// Dynamic: consult benchmark registry with a ranking algorithm
    Dynamic {
        /// Provider filter: only consider models from this provider on OpenRouter
        /// (None = consider all providers)
        provider_filter: Option<Vec<String>>,
        /// Ranking configuration
        ranking: RankingConfig,
        /// Require tool/function-calling support
        require_tools: bool,
    },
}
```

### 2.3 Built-in Profiles

Profiles are defined in code (not config files) so they ship with the binary.
Users don't create profiles — they create per-role overrides or custom tier mappings.

#### `anthropic` (static, default)

```rust
Profile {
    name: "anthropic",
    description: "Anthropic Claude models via Claude CLI",
    strategy: ProfileStrategy::Static {
        tiers: TierConfig {
            fast: Some("claude:haiku"),
            standard: Some("claude:sonnet"),
            premium: Some("claude:opus"),
        },
    },
}
```

**Config equivalent:**
```toml
profile = "anthropic"
# Resolves to:
# tiers.fast     = "claude:haiku"     → claude-haiku-4-latest
# tiers.standard = "claude:sonnet"    → claude-sonnet-4-latest
# tiers.premium  = "claude:opus"      → claude-opus-4-latest
```

#### `openrouter` (dynamic)

```rust
Profile {
    name: "openrouter",
    description: "Auto-select best OpenRouter models by usage and benchmarks",
    strategy: ProfileStrategy::Dynamic {
        provider_filter: None, // All providers on OpenRouter
        ranking: RankingConfig::default(),
        require_tools: true,
    },
}
```

**Config equivalent (after resolution):**
```toml
profile = "openrouter"
# Dynamically resolves to (example, changes over time):
# tiers.fast     = "openrouter:qwen/qwen3-coder-flash"
# tiers.standard = "openrouter:anthropic/claude-sonnet-4"
# tiers.premium  = "openrouter:anthropic/claude-opus-4"
```

#### `openai` (static)

```rust
Profile {
    name: "openai",
    description: "OpenAI models via OpenRouter",
    strategy: ProfileStrategy::Static {
        tiers: TierConfig {
            fast: Some("openrouter:openai/gpt-4o-mini"),
            standard: Some("openrouter:openai/gpt-4o"),
            premium: Some("openrouter:openai/o3-pro"),
        },
    },
}
```

---

## 3. Ranking Algorithm for Dynamic Profiles

### 3.1 The Problem

Given ~320 models in the benchmark registry across three pricing tiers, select
the best model for each quality tier. The user's key insight:

> "The current amount of use is really, really informative. If we use that as
> one of the strongest signals, we can follow the market."

### 3.2 Available Signals

| Signal | Source | Current Status |
|--------|--------|---------------|
| **Pricing** (input/output per Mtok) | OpenRouter API | Available for all 320 models |
| **Tool support** | OpenRouter API | Available (231 of 320 support tools) |
| **Context window / max output** | OpenRouter API | Available |
| **Provider count** | OpenRouter API | Schema exists, **currently unpopulated** |
| **Benchmark scores** (coding, intelligence, math, agentic) | Artificial Analysis | Schema exists, **mostly empty** |
| **Fitness score** (composite) | Computed locally | Depends on benchmarks — mostly null |
| **Request count / weekly rank** | OpenRouter rankings endpoint | **Not yet fetched** — needs new API integration |

### 3.3 Phased Approach

The ranking algorithm must work today with available data and improve as more
signals become available.

#### Phase 1: Pricing + Tools (works now)

Use price as the tier classifier and tool support as a hard filter.
Within each tier, sort by value (cheapest first for budget, balanced for mid,
capability-prioritized for frontier). This is the **minimum viable ranking**.

#### Phase 2: + Popularity Data (requires API work)

OpenRouter exposes model rankings at `/api/v1/models/rankings` (or via the
`/api/v1/models` endpoint's sorting parameters). When this data is integrated
into the benchmark registry's `Popularity` struct, usage becomes the primary
ranking signal within each price tier.

#### Phase 3: + Benchmark Scores (requires AA integration)

When Artificial Analysis data populates the `Benchmarks` fields, the full
composite scoring formula activates.

### 3.4 Ranking Formula

```
rank_score(model, tier) =
    popularity_weight * normalized_popularity(model)     // Phase 2+
  + benchmark_weight  * normalized_benchmarks(model)     // Phase 3+
  + value_weight      * normalized_value(model, tier)    // Phase 1+
  + reliability_weight * normalized_reliability(model)   // Phase 2+
```

**Weights by phase:**

| Component | Phase 1 | Phase 2 | Phase 3 (full) |
|-----------|---------|---------|-----------------|
| Popularity | 0.00 | **0.45** | **0.35** |
| Benchmarks | 0.00 | 0.00 | **0.30** |
| Value | **0.80** | **0.35** | **0.20** |
| Reliability | 0.20 | **0.20** | **0.15** |

Weights adjust automatically based on data availability. If popularity data is
present for >=50% of models, Phase 2 weights activate. If benchmark data is
present for >=30% of models, Phase 3 weights activate.

### 3.5 Tier Assignment

Models are assigned to workgraph tiers based on **output pricing** relative to
the median, mapping the benchmark registry's three-tier classification
(frontier/mid/budget) to workgraph's three tiers (premium/standard/fast):

| Benchmark Tier | Workgraph Tier | Pricing Range (current) |
|---------------|----------------|------------------------|
| `budget` | `fast` | $0.02 – $0.95/Mtok output |
| `mid` | `standard` | $0.97 – $3.40/Mtok output |
| `frontier` | `premium` | $3.90 – $600.00/Mtok output |

The system produces an **ordered list per tier** so it can fall through to
alternatives if the top pick is unavailable or rate-limited:

```
fast_candidates:     [model_a, model_b, model_c, ...]
standard_candidates: [model_d, model_e, model_f, ...]
premium_candidates:  [model_g, model_h, model_i, ...]
```

The profile resolves `tiers.fast` to `fast_candidates[0]`, etc.

### 3.6 Hard Filters (applied before ranking)

1. **Tool support required** — models without `supports_tools: true` are excluded
   (workgraph agents need function calling)
2. **Minimum context window** — exclude models with `context_window < 32_000`
3. **Minimum output tokens** — exclude models with `max_output_tokens < 4_000`
4. **Non-zero pricing** — exclude models with $0 pricing (typically deprecated)

### 3.7 Normalization

Each signal is normalized to 0.0–1.0 within its tier cohort:

- **Popularity**: `(model_pop - min_pop) / (max_pop - min_pop)` within tier
- **Benchmarks**: Use existing fitness score, normalized to tier range
- **Value**: `1.0 - (model_cost - min_cost) / (max_cost - min_cost)` (lower cost = higher value)
- **Reliability**: `min(provider_count / 5.0, 1.0)`

---

## 4. Resolution Order (how profiles fit into the existing cascade)

The current `resolve_model_for_role()` has a 5-step cascade. Profiles insert
at step 3, replacing the hardcoded Anthropic defaults:

```
1. models.<role>.model          (explicit per-role override — unchanged)
2. models.<role>.tier           (role tier override — unchanged)
3. Role default_tier() →        (NOW: profile-aware tier resolution)
   └─ if profile set:  profile.resolve_tier(tier)
   └─ else:            tiers.<tier> from config (existing behavior)
4. models.default.model         (default override — unchanged)
5. agent.model                  (global fallback — unchanged)
```

### Profile-Aware Tier Resolution

```rust
fn effective_tiers(&self) -> TierConfig {
    // Start with profile defaults (if a profile is active)
    let profile_tiers = self.resolve_profile_tiers();

    // Explicit [tiers] config overrides profile selections
    TierConfig {
        fast: self.tiers.fast.clone()
            .or(profile_tiers.fast)
            .or_else(|| Some("claude:haiku".into())),
        standard: self.tiers.standard.clone()
            .or(profile_tiers.standard)
            .or_else(|| Some("claude:sonnet".into())),
        premium: self.tiers.premium.clone()
            .or(profile_tiers.premium)
            .or_else(|| Some("claude:opus".into())),
    }
}

fn resolve_profile_tiers(&self) -> TierConfig {
    match self.profile.as_deref() {
        Some("anthropic") | None => {
            // Static: return hardcoded Anthropic tiers
            TierConfig {
                fast: Some("claude:haiku".into()),
                standard: Some("claude:sonnet".into()),
                premium: Some("claude:opus".into()),
            }
        }
        Some("openrouter") => {
            // Dynamic: load benchmark registry + run ranking
            match self.resolve_dynamic_profile() {
                Ok(tiers) => tiers,
                Err(_) => {
                    // Fallback: if registry unavailable, use Anthropic defaults
                    // via OpenRouter (same models, different provider)
                    TierConfig {
                        fast: Some("openrouter:anthropic/claude-haiku-4-latest".into()),
                        standard: Some("openrouter:anthropic/claude-sonnet-4".into()),
                        premium: Some("openrouter:anthropic/claude-opus-4".into()),
                    }
                }
            }
        }
        Some(name) => {
            // Look up other built-in static profiles
            builtin_profiles().get(name).map(|p| p.tiers()).unwrap_or_default()
        }
    }
}
```

### Precedence (highest to lowest)

1. `models.<role>.model` — always wins
2. `models.<role>.tier` — role-level tier override
3. `tiers.<tier>` in config.toml — explicit user tier settings
4. **Profile tier resolution** — new layer
5. Hardcoded Anthropic defaults — final fallback

This means users can set `profile = "openrouter"` and still override
individual tiers: `tiers.premium = "openrouter:anthropic/claude-opus-4"`.

---

## 5. Config Persistence

### 5.1 Stored in config.toml

```toml
# .workgraph/config.toml

# Provider profile: "anthropic" (default), "openrouter", "openai"
profile = "openrouter"
```

Single field, top-level on `Config`. Serialization:

```rust
pub struct Config {
    /// Active provider profile name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,

    // ... existing fields unchanged ...
}
```

### 5.2 Cached Resolution

Dynamic profiles cache their resolved tier mappings to avoid re-computing on
every model resolution call:

```
.workgraph/service/profile_cache.json
```

```json
{
  "profile": "openrouter",
  "resolved_at": "2026-04-01T18:00:00Z",
  "registry_fetched_at": "2026-04-01T17:55:00Z",
  "tiers": {
    "fast": {
      "model_id": "openrouter:qwen/qwen3-coder-flash",
      "candidates": [
        "qwen/qwen3-coder-flash",
        "mistralai/mistral-nemo",
        "meta-llama/llama-3.1-8b-instruct"
      ]
    },
    "standard": {
      "model_id": "openrouter:qwen/qwen3-coder",
      "candidates": [
        "qwen/qwen3-coder",
        "minimax/minimax-m2.5",
        "anthropic/claude-sonnet-4"
      ]
    },
    "premium": {
      "model_id": "openrouter:anthropic/claude-opus-4",
      "candidates": [
        "anthropic/claude-opus-4",
        "openai/gpt-5-pro",
        "openai/o3-pro"
      ]
    }
  }
}
```

The cache stores the full ordered candidate list per tier so the system can
fall through to alternatives without re-ranking.

---

## 6. Refresh Behavior

### 6.1 When Does Re-evaluation Happen?

| Trigger | What happens |
|---------|-------------|
| **Service start** (`wg service start`) | If profile is dynamic and cache is >24h old, re-resolve from registry |
| **Registry refresh** (`wg models update`) | Invalidate profile cache; next resolution re-ranks |
| **Manual** (`wg config --profile openrouter`) | Force re-resolve immediately |
| **Config change** (`tiers.*` or `profile` modified) | Invalidate cache |

### 6.2 Cache Invalidation Rules

The cache is valid when ALL of these hold:
1. `profile_cache.profile` matches `config.profile`
2. `profile_cache.registry_fetched_at` matches `model_benchmarks.json`'s `fetched_at`
3. `profile_cache.resolved_at` is less than 24 hours old

If any condition fails, the cache is stale and resolution runs fresh.

### 6.3 Offline / No Registry Fallback

If `profile = "openrouter"` but `model_benchmarks.json` doesn't exist:
1. Try to use the cached resolution if available
2. If no cache, fall back to Anthropic models routed through OpenRouter
3. Log a warning: "Dynamic profile active but no benchmark data — run `wg models update`"

---

## 7. CLI UX

### 7.1 Commands

Profile management uses the existing `wg config` command (consistent with current
patterns) plus a new `--profile` display mode:

```bash
# Set active profile
wg config --profile openrouter

# Show current profile and what it resolved to
wg config --profile-show

# List available profiles
wg config --profile-list
```

### 7.2 Example Outputs

#### `wg config --profile-list`

```
Available profiles:

  anthropic    Anthropic Claude models via Claude CLI (static)
               fast: claude:haiku  standard: claude:sonnet  premium: claude:opus

  openrouter   Auto-select best OpenRouter models by usage and benchmarks (dynamic)
               fast: qwen/qwen3-coder-flash  standard: qwen/qwen3-coder  premium: anthropic/claude-opus-4
               (resolved 2h ago from 320 models)

  openai       OpenAI models via OpenRouter (static)
               fast: openai/gpt-4o-mini  standard: openai/gpt-4o  premium: openai/o3-pro

  Active: openrouter
```

#### `wg config --profile-show`

```
Profile: openrouter (dynamic)
  Auto-select best OpenRouter models by usage and benchmarks
  Resolved: 2026-04-01T18:00:00Z (2h ago)
  Registry: 320 models, fetched 2026-04-01T17:55:00Z

  Tier Mappings:
    fast     → qwen/qwen3-coder-flash         ($0.97/Mtok out, tools: yes)
               Alternatives: mistralai/mistral-nemo, meta-llama/llama-3.1-8b-instruct
    standard → qwen/qwen3-coder               ($1.00/Mtok out, tools: yes)
               Alternatives: minimax/minimax-m2.5, anthropic/claude-sonnet-4
    premium  → anthropic/claude-opus-4         ($75.00/Mtok out, tools: yes)
               Alternatives: openai/gpt-5-pro, openai/o3-pro

  Ranking (Phase 1: pricing + tools):
    Signals: pricing (80%), reliability (20%)
    Filters: tools required, context >= 32k, output >= 4k

  Overrides active:
    models.evaluator.model = "claude:sonnet"  (overrides standard tier for this role)
```

#### `wg config --profile openrouter`

```
Profile set: openrouter
  Resolved tier mappings:
    fast     → openrouter:qwen/qwen3-coder-flash
    standard → openrouter:qwen/qwen3-coder
    premium  → openrouter:anthropic/claude-opus-4

  Note: Per-role overrides in [models] still take precedence.
  Run `wg config --profile-show` for full details.
```

### 7.3 Interaction with Existing Commands

- `wg config --tiers` — Shows effective tiers (profile-resolved + overrides)
- `wg config --models` — Shows per-role routing (unchanged, but sources show "profile" where applicable)
- `wg config --tier standard=deepseek/deepseek-v3` — Still works; explicit tier overrides profile

---

## 8. Implementation Scope

### 8.1 Files to Modify

| File | Changes |
|------|---------|
| `src/config.rs` | Add `profile: Option<String>` to `Config`, `Profile`/`ProfileStrategy`/`RankingConfig` types, `resolve_profile_tiers()`, modify `effective_tiers()` |
| `src/cli.rs` | Add `--profile`, `--profile-show`, `--profile-list` flags to Config command |
| `src/commands/config_cmd.rs` | Implement profile set/show/list handlers, profile resolution logic |
| `src/model_benchmarks.rs` | Add `rank_for_tier()` method that applies the ranking formula |

### 8.2 New Files

| File | Purpose |
|------|---------|
| `src/profile.rs` | Profile definitions, ranking algorithm, cache management |

### 8.3 Files Unchanged

- `src/commands/service/coordinator.rs` — No changes; profiles work through `effective_tiers()`
- `src/service/llm.rs` — No changes; calls `resolve_model_for_role()` which uses `effective_tiers()`
- `src/commands/spawn/execution.rs` — No changes; same reason

The design intentionally funnels through `effective_tiers()` so all downstream
consumers benefit without modification.

---

## 9. Data Flow

```
User: wg config --profile openrouter
  │
  ├─ Writes profile = "openrouter" to config.toml
  │
  ├─ Loads model_benchmarks.json (if exists)
  │
  ├─ Runs ranking algorithm:
  │   ├─ Filter: tools=true, context>=32k, output>=4k, price>0
  │   ├─ Classify into tiers by pricing (budget→fast, mid→standard, frontier→premium)
  │   ├─ Rank within each tier by weighted score
  │   └─ Select top candidate per tier + ordered alternatives
  │
  ├─ Writes profile_cache.json
  │
  └─ Prints resolved mappings

Later: resolve_model_for_role(Triage)
  │
  ├─ Step 1-2: No per-role override → skip
  │
  ├─ Step 3: default_tier(Triage) = Fast
  │   └─ effective_tiers().fast
  │       ├─ config.tiers.fast? → No
  │       ├─ profile_tiers().fast? → "openrouter:qwen/qwen3-coder-flash"
  │       └─ Resolved!
  │
  └─ Returns ResolvedModel {
       model: "qwen/qwen3-coder-flash",
       provider: Some("openrouter"),
       registry_entry: Some(...),
     }
```

---

## 10. Future Extensions

### 10.1 Popularity Data Integration

When OpenRouter rankings data is integrated (tracked separately):
- Add `request_count: Option<u64>` and `weekly_rank: Option<u32>` to `Popularity`
- Fetch from `/api/v1/models/rankings` or model detail endpoints
- Ranking weights automatically shift to Phase 2 (popularity-primary)

### 10.2 User-Defined Profiles

If demand exists, support custom profiles in config:
```toml
[profiles.my-cheap]
description = "Budget models only"
tiers = { fast = "openrouter:meta-llama/llama-3.1-8b-instruct", standard = "openrouter:qwen/qwen3-coder", premium = "openrouter:deepseek/deepseek-v3" }
```

This is not in the initial scope — built-in profiles + per-tier overrides cover
the same use cases with less complexity.

### 10.3 Per-Role Profile Awareness

Profiles could eventually specify role-specific preferences:
```rust
// "For the openrouter profile, use a cheaper model for triage specifically"
role_overrides: HashMap<DispatchRole, String>,
```

Not needed initially — the existing `models.<role>.model` system handles this.

---

## 11. Open Questions (resolved in this design)

| Question | Resolution |
|----------|-----------|
| Built-in vs user-defined profiles? | Built-in only for v1; user profiles deferred |
| Scope: tiers only or also agent/coordinator? | Tiers only — profile sets tier defaults, `agent.model` and `coordinator.model` stay independent |
| Persistence: track active profile or one-shot apply? | Track active profile (`profile = "openrouter"` persists in config.toml); dynamic re-resolution on cache expiry |
| Two registry problem? | Profiles use `model_benchmarks.json` (BenchmarkRegistry) for ranking; resolved models are referenced via `openrouter:<id>` in the config.toml `[tiers]` system |
| What if no popularity data? | Phase 1 ranking works on pricing + tool support alone; weights auto-adjust as data becomes available |
