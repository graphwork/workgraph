# Research: Current Tier/Config System and Profile Requirements

**Task:** `profile-research`
**Date:** 2026-04-01

---

## 1. Current Config Schema for Tiers/Models

### Config Structure (`src/config.rs:16`)

The `Config` struct contains three tier/model-related sections:

```toml
[models]          # ModelRoutingConfig — per-role model+provider assignments
[tiers]           # TierConfig — which model ID each tier resolves to
[[model_registry]] # Vec<ModelRegistryEntry> — model catalog with cost/capability data
```

### Tier System (`src/config.rs:826-855`)

Three quality tiers defined as `enum Tier`:
- **`fast`** — lightweight/cheap models (haiku-class)
- **`standard`** — balanced capability (sonnet-class)
- **`premium`** — highest capability (opus-class)

The `TierConfig` struct (`src/config.rs:919-929`) maps each tier to a model ID:
```toml
[tiers]
fast = "claude:haiku"         # Optional<String>
standard = "claude:sonnet"    # Optional<String>
premium = "claude:opus"       # Optional<String>
```
Defaults (when unconfigured) are set in `effective_tiers()` (`src/config.rs:1472-1490`):
- fast → `"claude:haiku"`
- standard → `"claude:sonnet"`
- premium → `"claude:opus"`

### Dispatch Roles (`src/config.rs:636-752`)

14 `DispatchRole` variants, each with a `default_tier()`:

| Role | Default Tier |
|------|-------------|
| Triage | Fast |
| FlipComparison | Fast |
| Assigner | Fast |
| Compactor | Fast |
| ChatCompactor | Fast |
| CoordinatorEval | Fast |
| Placer | Fast |
| FlipInference | Standard |
| TaskAgent | Standard |
| Evaluator | Standard |
| Default | Standard |
| Evolver | Premium |
| Creator | Premium |
| Verification | Premium |

### Per-Role Model Routing (`src/config.rs:932-1097`)

`RoleModelConfig` per role:
- `model: Option<String>` — provider:model spec (e.g., `"claude:opus"`)
- `tier: Option<Tier>` — tier override
- `endpoint: Option<String>` — named endpoint override
- `provider: Option<String>` — **deprecated** (kept for deserializing old configs)

`ModelRoutingConfig` holds `Option<RoleModelConfig>` for each of the 14 dispatch roles plus a `default` slot.

### Model Registry Entries (`src/config.rs:859-915`)

`ModelRegistryEntry` fields in `config.toml`:
```
id                  # Short identifier (e.g., "deepseek-v3.2")
provider            # Provider name (e.g., "openrouter")
model               # Full API model ID (e.g., "deepseek/deepseek-v3.2")
tier                # Tier enum: fast | standard | premium
endpoint            # Optional named endpoint
context_window      # u64
max_output_tokens   # u64
cost_per_input_mtok # f64 (USD per million tokens)
cost_per_output_mtok# f64
prompt_caching      # bool
cache_read_discount # f64
cache_write_premium # f64
descriptors         # Vec<String> (e.g., ["auto-update", "flash"])
```

Built-in entries (`builtin_registry()` at `src/config.rs:1358-1447`): 6 entries — haiku, sonnet, opus (in both bare and `claude:` prefixed forms).

User-defined entries in `[[model_registry]]` sections of config.toml override built-ins by ID.

---

## 2. Current Model Selection Flow

### `resolve_model_for_role()` (`src/config.rs:1543-1682`)

Resolution cascade (5 steps):

1. **Role-specific model override** — `models.<role>.model` (e.g., `models.evaluator.model = "claude:sonnet"`)
   - Parses `provider:model` prefix
   - Looks up in registry for full API model name
   - Falls back to raw string if not in registry

2. **Role tier override** — `models.<role>.tier` (e.g., `models.evaluator.tier = "fast"`)
   - Resolves via `resolve_tier()` → `effective_tiers()` → registry lookup

3. **Role default_tier()** — Each role's hardcoded default tier
   - E.g., Triage → Fast → `tiers.fast` → "claude:haiku" → registry → `claude-haiku-4-latest`

4. **Default model** — `models.default.model`
   - Same parse/lookup logic as step 1

5. **Global fallback** — `agent.model`
   - From `[agent]` section

At each step, provider resolution follows:
- Provider prefix in model spec (e.g., `openrouter:deepseek/deepseek-chat`)
- Role-specific `provider` field (deprecated)
- Default role `provider` field
- Registry entry's `provider` field

Returns `ResolvedModel` (`src/config.rs:1100-1109`):
```rust
pub struct ResolvedModel {
    pub model: String,           // Full API model name
    pub provider: Option<String>, // e.g., "anthropic", "openrouter"
    pub registry_entry: Option<ModelRegistryEntry>, // Cost data
    pub endpoint: Option<String>, // Named endpoint override
}
```

### Consumers

- `run_lightweight_llm_call()` (`src/service/llm.rs:31-77`) — All internal LLM dispatch (triage, compaction, evaluation, FLIP, etc.)
- `src/commands/spawn/execution.rs` — Agent spawning (uses coordinator model or task-level model)
- `src/commands/service/coordinator.rs` — Coordinator agent model selection

---

## 3. Registry Data Fields Available for Auto-Ranking

### Benchmark Registry (`src/model_benchmarks.rs`)

File: `.workgraph/model_benchmarks.json`  
Schema struct: `BenchmarkRegistry`

**Per-model fields (`ModelBenchmark`):**

| Field | Type | Status |
|-------|------|--------|
| `id` | String | Available (OpenRouter model ID) |
| `name` | String | Available (human name) |
| `pricing.input_per_mtok` | f64 | Available |
| `pricing.output_per_mtok` | f64 | Available |
| `pricing.cache_read_per_mtok` | Option<f64> | Available (sparse) |
| `pricing.cache_write_per_mtok` | Option<f64> | Available (sparse) |
| `context_window` | Option<u64> | Available |
| `max_output_tokens` | Option<u64> | Available |
| `supports_tools` | bool | Available |
| `tier` | String | Available ("frontier"/"mid"/"budget") |
| `pricing_updated_at` | String | Available |
| **Benchmarks** | | |
| `benchmarks.intelligence_index` | Option<f64> | Mostly empty (pending AA integration) |
| `benchmarks.coding_index` | Option<f64> | Mostly empty |
| `benchmarks.math_index` | Option<f64> | Mostly empty |
| `benchmarks.agentic` | Option<f64> | Mostly empty |
| **Popularity** | | |
| `popularity.provider_count` | Option<u32> | Available for some models |
| **Fitness** | | |
| `fitness.score` | Option<f64> | Computed (0–100, null if no benchmarks) |
| `fitness.components.quality` | Option<f64> | Computed |
| `fitness.components.value` | Option<f64> | Computed |
| `fitness.components.reliability` | Option<f64> | Computed (from provider_count) |

**Not currently available but mentioned in code comments:**
- `request_count` / `weekly_rank` — referenced in `model_benchmarks.rs:223` as not available from OpenRouter
- The popularity struct only has `provider_count`; no request volume or ranking data

### Separate YAML Registry (`src/models.rs`)

File: `.workgraph/models.yaml`  
Schema: `ModelRegistry` with `ModelEntry` items  
**Three-tier system**: Frontier / Mid / Budget (different from config's Fast/Standard/Premium)  
Fields: `id`, `provider`, `cost_per_1m_input`, `cost_per_1m_output`, `context_window`, `capabilities: Vec<String>`, `tier: ModelTier`

This is a **separate, older** registry with hardcoded defaults (Anthropic, OpenAI, Google, DeepSeek, Meta, Qwen models). It stores to `models.yaml` while the benchmark system stores to `model_benchmarks.json`.

### Fitness Scoring (`src/model_benchmarks.rs:180-247`)

Composite formula:
```
quality = coding_index * 0.50 + intelligence_index * 0.30 + agentic * 0.20
value = quality / (cost_factor normalized to median)
reliability = provider_count / 5.0 (capped at 50.0)
score = quality * 0.70 + value * 0.20 + reliability * 0.10
```

Tier classification from fitness:
- `frontier`: fitness >= 65 OR (coding >= 48 AND intelligence >= 50)
- `mid`: fitness >= 40 OR coding >= 35
- `budget`: everything else

---

## 4. Current `wg config` Command

### Subcommand Structure (`src/cli.rs:1102-1387`)

Not a subcommand-based design — uses flat `--flag` arguments on `wg config`:

**Display:**
- `--show` — Print full config
- `--list` — Merged config with source annotations (global/local/default)
- `--registry` — Show all model registry entries
- `--tiers` — Show current tier→model assignments
- `--models` — Show per-role model routing
- `--check-key` — Verify OpenRouter API key

**Init/Global:**
- `--init` — Create default config file
- `--global` / `--local` — Target scope
- `--install-global [--force]` — Copy project config to global

**Model routing:**
- `--model <MODEL>` — Set agent.model
- `--coordinator-model <MODEL>` — Set coordinator.model
- `--coordinator-provider <PROVIDER>` — Deprecated
- `--set-model <ROLE> <MODEL>` — Per-role model
- `--set-provider <ROLE> <PROVIDER>` — Deprecated
- `--set-endpoint <ROLE> <ENDPOINT>` — Per-role endpoint
- `--role-model <ROLE=MODEL>` — Key=value variant

**Tier management:**
- `--tier <TIER=MODEL_ID>` — Set tier model mapping (e.g., `--tier standard=gpt-4o`)

**Registry management:**
- `--registry-add` with `--id`, `--provider`, `--reg-model`, `--reg-tier`, `--endpoint`, `--context-window`, `--cost-input`, `--cost-output`
- `--registry-remove <ID>`

**Other settings:**
- Executor, interval, max_agents, max_coordinators, agency toggles, eval gate, FLIP, TUI, guardrails, Matrix, API keys...

### Implementation (`src/commands/config_cmd.rs`)

Three functions:
1. `show()` — Pretty-prints config sections
2. `init()` — Creates default config.toml
3. `update()` — Takes ~30+ optional parameters, updates matching fields, saves

---

## 5. What Would Need to Change for Profile Support

### Concept

A "profile" is a named preset that reconfigures all tiers at once, potentially including:
- `tiers.fast`, `tiers.standard`, `tiers.premium` 
- Default `models.default.model`
- `agent.model` and `coordinator.model`
- Possibly per-role overrides for specific profiles

### Files That Would Need Changes

| File | Change |
|------|--------|
| **`src/config.rs`** | Add `ProfileConfig` struct, add `profiles: HashMap<String, ProfileConfig>` to Config, add `apply_profile()` method |
| **`src/cli.rs`** | Add `--profile <NAME>` flag to Config command, possibly a top-level `Profile` subcommand |
| **`src/commands/config_cmd.rs`** | Add `apply_profile()` function, profile listing, profile creation |
| **`src/model_benchmarks.rs`** | May need integration for auto-generating profiles from benchmark data (e.g., "cheapest", "best-value", "frontier-only") |
| **`src/commands/models.rs`** | Profile could reference models from the benchmark registry for auto-configuration |
| **`src/commands/setup.rs`** | Setup wizard could offer profile selection |
| **`src/commands/quickstart.rs`** | Document profile usage in quickstart |

### Proposed CLI UX

**Option A: Flag on `wg config`**
```bash
wg config --profile claude-only    # Apply named profile
wg config --profile cheapest       # Auto-generated from benchmarks
wg config --show-profiles          # List available profiles
```

**Option B: Dedicated subcommand**
```bash
wg profile list                    # List available profiles
wg profile show claude-only        # Show what a profile would set
wg profile set claude-only         # Apply profile
wg profile create my-profile       # Create custom profile from current config
wg profile auto cheapest           # Generate from benchmark data
```

**Recommendation:** Option A for initial implementation (fewer code changes, consistent with existing `wg config` pattern), with Option B as a future enhancement if profiles become complex enough to warrant their own subcommand.

### Profile Data Shape (Proposed)

```toml
[profiles.claude-only]
description = "All Anthropic models via Claude CLI"
tiers = { fast = "claude:haiku", standard = "claude:sonnet", premium = "claude:opus" }
agent_model = "claude:opus"
coordinator_model = "claude:opus"
executor = "claude"

[profiles.openrouter-budget]
description = "Cheapest OpenRouter models with tool support"
tiers = { fast = "qwen-turbo", standard = "qwen3-flash", premium = "qwen3-coder" }
agent_model = "openrouter:qwen/qwen3-coder"
coordinator_model = "openrouter:qwen/qwen3-coder"

[profiles.openrouter-best]
description = "Best OpenRouter models by fitness score"
# Auto-populated from model_benchmarks.json
tiers = { fast = "...", standard = "...", premium = "..." }
```

### Key Design Decisions for Downstream Task

1. **Built-in vs. user-defined profiles** — Should profiles ship with defaults (like the registry), or be purely user-created?
2. **Auto-generation from benchmarks** — Can profiles auto-select models using fitness scores and tier classification from `model_benchmarks.json`?
3. **Scope** — Should profile apply to tiers only, or also agent/coordinator/per-role settings?
4. **Persistence** — Should the "active profile" be tracked, or is it a one-shot apply?
5. **Two registry problem** — `models.yaml` (ModelRegistry) and `model_benchmarks.json` (BenchmarkRegistry) exist in parallel. Profiles should work with the config.toml `[[model_registry]]` + `[tiers]` system, not the older `models.yaml`.

---

## Validation Checklist

- [x] All tier-related config fields documented (Tier enum, TierConfig, ModelRegistryEntry, RoleModelConfig.tier)
- [x] Model selection code path traced (resolve_model_for_role 5-step cascade)
- [x] Registry data fields catalogued (BenchmarkRegistry: pricing, benchmarks, popularity, fitness)
- [x] File change list produced (7 files identified)
- [x] CLI UX proposed (two options with recommendation)
