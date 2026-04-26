# Model Provider Registry with Quality Tiers

Design document for replacing the current single-model approach with a structured
registry of provider+model+endpoint combinations organized into quality tiers.

## Status

Draft — produced by `research-design-model` task.

---

## 1. Current State

### Model Resolution Hierarchy

`Config::resolve_model_for_role(role)` resolves a `ResolvedModel { model, provider }` via:

1. **`[models.<role>]`** — role-specific override in `config.toml`
2. **Legacy `[agency.*_model]`** — backward-compat fields (deprecated)
3. **Tier defaults** — hardcoded per-role: `triage→haiku`, `flip_comparison→haiku`, `flip_inference→sonnet`, `verification→opus`
4. **`[models.default]`** — project-wide default
5. **`agent.model`** — global fallback (`"sonnet"` by default)

Provider is resolved from `[models.<role>.provider]` or `[models.default.provider]`.

### DispatchRole Enum

10 roles in `src/config.rs:385`:

| Role | Default Tier | Purpose |
|------|-------------|---------|
| `Default` | — | Fallback for unset roles |
| `TaskAgent` | (inherits default) | Main implementation agents |
| `Evaluator` | (inherits default) | Post-task scoring |
| `Assigner` | (inherits default) | Agent identity assignment |
| `Evolver` | (inherits default) | Agency evolution |
| `Creator` | (inherits default) | Agent composition creation |
| `Triage` | haiku | Dead-agent summarization |
| `FlipInference` | sonnet | FLIP prompt reconstruction |
| `FlipComparison` | haiku | FLIP similarity scoring |
| `Verification` | opus | FLIP-triggered verification |

### Per-Task Overrides

`Task` struct (`src/graph.rs:258-261`) has `model: Option<String>` and
`provider: Option<String>`. During spawn, the hierarchy is:
`task.model > executor.model > coordinator/CLI model`.

### Endpoints Config

`EndpointsConfig` (config.toml `[llm_endpoints]`) stores provider+URL+API-key entries
but is **not connected** to model routing. It's a separate concept (endpoint management).

---

## 2. Quality Tiers

### Three tiers

| Tier | Intent | Default Model | Typical Cost | Use Cases |
|------|--------|---------------|-------------|-----------|
| **fast** | Cheap, low-latency | `haiku` | ~$0.25/$1.25 per MTok | Triage, FLIP comparison, simple classification, cache-hit assignment |
| **standard** | Balanced cost/quality | `sonnet` | ~$3/$15 per MTok | Implementation, evaluation, FLIP inference, most task work |
| **premium** | Highest capability | `opus` | ~$15/$75 per MTok | Verification, novel agent composition, complex architecture, design |

### Role-to-Tier Mapping (Defaults)

Each `DispatchRole` maps to a default tier. This replaces the current hardcoded
tier defaults in `resolve_model_for_role()`:

```rust
impl DispatchRole {
    pub fn default_tier(&self) -> Tier {
        match self {
            Self::Triage => Tier::Fast,
            Self::FlipComparison => Tier::Fast,
            Self::Assigner => Tier::Fast,       // was inheriting default
            Self::FlipInference => Tier::Standard,
            Self::TaskAgent => Tier::Standard,
            Self::Evaluator => Tier::Standard,
            Self::Evolver => Tier::Standard,
            Self::Creator => Tier::Premium,     // novel composition
            Self::Verification => Tier::Premium,
            Self::Default => Tier::Standard,
        }
    }
}
```

Users override the tier for any role, or override the model directly (which
takes priority over tier-based selection).

---

## 3. Registry Schema

### 3.1 Config Format

Registry entries live in `config.toml` under `[[model_registry]]`:

```toml
# Tier defaults — which model to use when a role resolves to a tier
[tiers]
fast = "haiku"              # model ID from registry
standard = "sonnet"         # model ID from registry
premium = "opus"            # model ID from registry

# Model registry entries
[[model_registry]]
id = "haiku"
provider = "anthropic"
model = "haiku"
tier = "fast"
context_window = 200000
max_output_tokens = 8192
cost_per_input_mtok = 0.25
cost_per_output_mtok = 1.25
prompt_caching = true
cache_read_discount = 0.1    # 90% off cached input
cache_write_premium = 1.25   # 25% more for cache writes
descriptors = ["classification", "triage", "simple-edits", "comparison"]

[[model_registry]]
id = "sonnet"
provider = "anthropic"
model = "sonnet"
tier = "standard"
context_window = 200000
max_output_tokens = 16384
cost_per_input_mtok = 3.0
cost_per_output_mtok = 15.0
prompt_caching = true
cache_read_discount = 0.1
cache_write_premium = 1.25
descriptors = ["implementation", "analysis", "code-review", "general"]

[[model_registry]]
id = "opus"
provider = "anthropic"
model = "claude-opus-4-6"
tier = "premium"
context_window = 200000
max_output_tokens = 32000
cost_per_input_mtok = 15.0
cost_per_output_mtok = 75.0
prompt_caching = true
cache_read_discount = 0.1
cache_write_premium = 1.25
descriptors = ["architecture", "novel-composition", "verification", "complex-design"]

[[model_registry]]
id = "gpt-4o"
provider = "openai"
model = "gpt-4o"
tier = "standard"
endpoint = "https://api.openai.com/v1"
context_window = 128000
max_output_tokens = 16384
cost_per_input_mtok = 2.5
cost_per_output_mtok = 10.0
prompt_caching = false
descriptors = ["implementation", "general"]

[[model_registry]]
id = "gemini-2.5-pro"
provider = "google"
model = "gemini-2.5-pro"
tier = "standard"
endpoint = "https://generativelanguage.googleapis.com/v1beta"
context_window = 1000000
max_output_tokens = 65536
cost_per_input_mtok = 1.25
cost_per_output_mtok = 10.0
prompt_caching = true
cache_read_discount = 0.25
descriptors = ["long-context", "implementation", "analysis"]

[[model_registry]]
id = "local-qwen"
provider = "local"
model = "qwen2.5-coder:32b"
tier = "fast"
endpoint = "http://localhost:11434/v1"
context_window = 32768
max_output_tokens = 8192
cost_per_input_mtok = 0.0
cost_per_output_mtok = 0.0
prompt_caching = false
descriptors = ["offline", "code-completion", "simple-edits"]
```

### 3.2 Rust Structs

```rust
/// Quality tier for model selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Fast,
    Standard,
    Premium,
}

/// A model registry entry describing a provider+model combination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRegistryEntry {
    /// Short identifier used in config references (e.g., "haiku", "sonnet", "gpt-4o")
    pub id: String,
    /// Provider: "anthropic", "openai", "google", "local", etc.
    pub provider: String,
    /// Full model identifier sent to the API
    pub model: String,
    /// Quality tier this model belongs to
    pub tier: Tier,
    /// API endpoint URL (None = use provider default)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Max input context window in tokens
    #[serde(default)]
    pub context_window: u64,
    /// Max output tokens
    #[serde(default)]
    pub max_output_tokens: u64,
    /// Cost per million input tokens (USD)
    #[serde(default)]
    pub cost_per_input_mtok: f64,
    /// Cost per million output tokens (USD)
    #[serde(default)]
    pub cost_per_output_mtok: f64,
    /// Whether the provider supports prompt caching
    #[serde(default)]
    pub prompt_caching: bool,
    /// Discount multiplier for cached reads (e.g., 0.1 = 90% off)
    #[serde(default)]
    pub cache_read_discount: f64,
    /// Premium multiplier for cache writes (e.g., 1.25 = 25% more)
    #[serde(default)]
    pub cache_write_premium: f64,
    /// Descriptors for when to use this model
    #[serde(default)]
    pub descriptors: Vec<String>,
}

/// Tier routing configuration: which model ID each tier resolves to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TierConfig {
    /// Model ID for fast tier
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<String>,
    /// Model ID for standard tier
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    /// Model ID for premium tier
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premium: Option<String>,
}
```

### 3.3 Config Struct Changes

Add to `Config`:

```rust
pub struct Config {
    // ... existing fields ...

    /// Quality tier defaults
    #[serde(default)]
    pub tiers: TierConfig,

    /// Model registry entries
    #[serde(default)]
    pub model_registry: Vec<ModelRegistryEntry>,
}
```

---

## 4. Selection Algorithm

### 4.1 Extended Resolution Hierarchy

The new `resolve_model_for_role()` inserts tier-based resolution between the
existing steps. Full cascade:

1. **`[models.<role>].model`** — explicit role model override (unchanged)
2. **Legacy `[agency.*_model]`** — backward compat (unchanged, eventually removed)
3. **`[models.<role>].tier`** — NEW: role configured to use a specific tier
4. **Tier-based default** — role's `default_tier()` → `tiers.<tier>` → registry lookup
5. **`[models.default].model`** — project-wide default (unchanged)
6. **`agent.model`** — global fallback (unchanged)

Steps 1-2 are direct model overrides (highest priority). Step 3 lets users
override which tier a role uses. Step 4 is the new tier-based default that
replaces the current hardcoded per-role defaults. Steps 5-6 are fallbacks.

### 4.2 Tier Resolution

When a role resolves to a tier (step 3 or 4):

```
tier → tiers.<tier> config → model ID → registry entry → ResolvedModel
```

Example: `Triage` role → `default_tier() = Fast` → `tiers.fast = "haiku"` →
registry lookup for `id = "haiku"` → `ResolvedModel { model: "haiku", provider: Some("anthropic") }`.

### 4.3 Registry Lookup

```rust
impl Config {
    /// Look up a registry entry by its short ID.
    fn registry_lookup(&self, id: &str) -> Option<&ModelRegistryEntry> {
        self.model_registry.iter().find(|e| e.id == id)
    }

    /// Resolve a tier to a model ID, then look up the registry entry.
    fn resolve_tier(&self, tier: Tier) -> Option<ResolvedModel> {
        let model_id = match tier {
            Tier::Fast => self.tiers.fast.as_deref(),
            Tier::Standard => self.tiers.standard.as_deref(),
            Tier::Premium => self.tiers.premium.as_deref(),
        }?;

        if let Some(entry) = self.registry_lookup(model_id) {
            Some(ResolvedModel {
                model: entry.model.clone(),
                provider: Some(entry.provider.clone()),
                registry_entry: Some(entry.clone()),
            })
        } else {
            // Model ID not in registry — treat as a bare model name
            Some(ResolvedModel {
                model: model_id.to_string(),
                provider: None,
                registry_entry: None,
            })
        }
    }
}
```

### 4.4 Extended RoleModelConfig

Allow roles to specify a tier instead of (or in addition to) a direct model:

```rust
pub struct RoleModelConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// NEW: tier override — resolve model via tier system instead of direct model
    pub tier: Option<Tier>,
}
```

Config example:

```toml
[models.task_agent]
tier = "premium"    # Use premium tier for all task agents

[models.triage]
model = "haiku"     # Direct override (takes precedence over tier)
```

### 4.5 Cost-Aware Selection

When multiple registry entries exist for the same tier, the selection can be
cost-aware. This is relevant for the assignment flow where the assigner decides
model quality.

```rust
/// Select the cheapest model in a given tier.
fn cheapest_in_tier(&self, tier: Tier) -> Option<&ModelRegistryEntry> {
    self.model_registry
        .iter()
        .filter(|e| e.tier == tier)
        .min_by(|a, b| {
            let cost_a = a.cost_per_input_mtok + a.cost_per_output_mtok;
            let cost_b = b.cost_per_input_mtok + b.cost_per_output_mtok;
            cost_a.partial_cmp(&cost_b).unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Select a model matching descriptors within a tier.
fn select_by_descriptors(&self, tier: Tier, tags: &[&str]) -> Option<&ModelRegistryEntry> {
    self.model_registry
        .iter()
        .filter(|e| e.tier == tier)
        .max_by_key(|e| {
            tags.iter()
                .filter(|t| e.descriptors.iter().any(|d| d == *t))
                .count()
        })
}
```

### 4.6 Integration with Assignment

The auto-assign flow (`build_auto_assign_tasks` in coordinator.rs) currently
creates assignment tasks using `DispatchRole::Assigner`. The assignment agent
itself selects which task-agent model to use.

With the registry, the assignment agent has richer information:

1. **Assigner runs on its own tier** — `Assigner.default_tier() = Fast` (cheap
   for cache-hit deployments, upgradeable to Standard for novel composition).
2. **Task complexity estimation** — assigner can inspect task description length,
   dependency depth, tags, and descriptors to estimate difficulty.
3. **Registry query** — assigner selects from available models in the
   appropriate tier, considering cost and descriptors.
4. **Budget enforcement** — if a cost budget is configured, the assigner
   checks accumulated spend before selecting expensive models.

The assigner communicates model selection by setting `task.model` via `wg assign`:

```bash
# Assigner picks from registry based on task analysis
wg assign <task-id> --agent <agent-id> --model opus
```

The coordinator's spawn logic already reads `task.model` as highest priority.

---

## 5. Cost Tracking and Budget Hooks

### 5.1 Cost Tracking Schema

Add to `Task`:

```rust
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}
```

This already exists. Extend `ResolvedModel` to carry cost metadata:

```rust
pub struct ResolvedModel {
    pub model: String,
    pub provider: Option<String>,
    /// Registry entry if resolved through the registry (carries cost data)
    pub registry_entry: Option<ModelRegistryEntry>,
}
```

### 5.2 Cost Calculation

```rust
impl ModelRegistryEntry {
    /// Calculate cost for a given token usage in USD.
    pub fn calculate_cost(&self, usage: &TokenUsage) -> f64 {
        let input_cost = (usage.input_tokens as f64 / 1_000_000.0) * self.cost_per_input_mtok;
        let output_cost = (usage.output_tokens as f64 / 1_000_000.0) * self.cost_per_output_mtok;
        let cache_read_cost = (usage.cache_read_tokens as f64 / 1_000_000.0)
            * self.cost_per_input_mtok * self.cache_read_discount;
        let cache_write_cost = (usage.cache_write_tokens as f64 / 1_000_000.0)
            * self.cost_per_input_mtok * self.cache_write_premium;
        input_cost + output_cost + cache_read_cost + cache_write_cost
    }
}
```

### 5.3 Budget Configuration (Future)

```toml
[budget]
max_daily_usd = 50.0       # Hard cap per day
warn_daily_usd = 30.0      # Warning threshold
max_per_task_usd = 5.0     # Per-task cap
```

Budget enforcement is **not implemented in this phase** but the registry schema
provides the cost metadata needed to support it later.

---

## 6. Migration Path

### Phase 1: Registry Addition (Non-Breaking)

Add `model_registry`, `tiers`, and `tier` field to `RoleModelConfig`. All existing
config continues to work unchanged.

**Resolution cascade** adds new steps but existing steps still have higher
priority. Users who never configure `[tiers]` or `[[model_registry]]` see no
change.

### Phase 2: Built-in Defaults

Ship a built-in default registry for Anthropic models (haiku, sonnet, opus) that
matches the current hardcoded tier defaults. The hardcoded match in
`resolve_model_for_role()` at step 2.5 (triage→haiku, etc.) is replaced by
tier-based resolution using the built-in defaults.

```rust
impl Config {
    /// Provide built-in registry entries when none are configured.
    fn effective_registry(&self) -> Vec<ModelRegistryEntry> {
        if !self.model_registry.is_empty() {
            return self.model_registry.clone();
        }
        // Built-in defaults
        vec![
            ModelRegistryEntry {
                id: "haiku".into(),
                provider: "anthropic".into(),
                model: "haiku".into(),
                tier: Tier::Fast,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "sonnet".into(),
                provider: "anthropic".into(),
                model: "sonnet".into(),
                tier: Tier::Standard,
                ..Default::default()
            },
            ModelRegistryEntry {
                id: "opus".into(),
                provider: "anthropic".into(),
                model: "opus".into(),
                tier: Tier::Premium,
                ..Default::default()
            },
        ]
    }
}
```

### Phase 3: Legacy Deprecation

The existing `agency.*_model` fields are already deprecated with warnings.
After the tier system is stable:

1. `agency.*_model` fields → replaced by `[models.<role>]` (already done)
2. Hardcoded tier defaults in `resolve_model_for_role()` → replaced by `DispatchRole::default_tier()` + tier config
3. `agent.model` → becomes true last-resort fallback (for backwards compat)

### Backward Compatibility

| Current Config | After Migration | Behavior |
|----------------|-----------------|----------|
| `agent.model = "sonnet"` | Unchanged | Still works as final fallback |
| `coordinator.model = "opus"` | Unchanged | CLI param, overrides tier |
| `[models.triage] model = "haiku"` | Unchanged | Direct override, highest priority |
| `agency.evaluator_model = "sonnet"` | Deprecated → `[models.evaluator] model = "sonnet"` | Works via legacy path |
| (no config) | Tier defaults kick in | `Triage` → `Fast` tier → `haiku` |

Short model names ("haiku", "sonnet", "opus") continue to work everywhere.
The registry only adds structure — it doesn't require users to use full model IDs.

---

## 7. CLI Integration

### `wg config` Extensions

```bash
# View current tier assignments
wg config --tiers

# Set tier defaults
wg config --tier fast=haiku
wg config --tier standard=sonnet
wg config --tier premium=opus

# Set role tier (instead of direct model)
wg config --model-role triage --tier fast
wg config --model-role task_agent --tier premium

# List registry entries
wg config --registry

# Add a registry entry
wg config --registry-add --id "gpt-4o" --provider openai --model gpt-4o --tier standard
```

### `wg status` Cost Display

```
$ wg status
Tasks: 12 done, 3 in-progress, 5 open
Agents: 3 alive
Cost (session): $4.23 (input: $1.12, output: $3.11)
Cost (24h): $18.45
```

---

## 8. Design Decisions

### Why tiers instead of just per-role models?

Per-role models require configuring 10+ individual roles. Tiers let users
set 3 quality levels and have all roles automatically route to the right tier.
Direct per-role overrides are still available for fine-tuning.

### Why registry entries in config.toml instead of separate YAML files?

Simplicity. One config file to manage. The registry is expected to have 3-10
entries, not hundreds. If it grows, we can add `include` support later.

### Why short IDs instead of full model names?

Short IDs ("haiku", "sonnet", "gpt-4o") are ergonomic for CLI and config.
The full model identifier is stored in the registry entry and sent to the API.
This also provides a stable reference — when Anthropic releases a new Sonnet
version, the user updates the registry entry's `model` field and all roles
using "sonnet" automatically get the new version.

### Why not auto-discover models from providers?

Auto-discovery adds API dependencies, rate limits, and complexity. The registry
is user-managed and explicit. Users know exactly what models are available and
at what cost.

---

## 9. Implementation Plan

### Step 1: Add Structs (Non-Breaking)

- Add `Tier`, `ModelRegistryEntry`, `TierConfig` to `src/config.rs`
- Add `model_registry: Vec<ModelRegistryEntry>` and `tiers: TierConfig` to `Config`
- Add `tier: Option<Tier>` to `RoleModelConfig`
- Add `registry_entry: Option<ModelRegistryEntry>` to `ResolvedModel`

### Step 2: Extend Resolution

- Add `DispatchRole::default_tier()`
- Modify `resolve_model_for_role()` to insert tier resolution
- Add `effective_registry()` with built-in defaults
- Remove hardcoded tier defaults (step 2.5 in current code)

### Step 3: CLI Commands

- Extend `wg config` with `--tiers`, `--tier`, `--registry`, `--registry-add`
- Add cost display to `wg status`

### Step 4: Cost Tracking

- Pipe `ResolvedModel.registry_entry` through to spawn/completion
- Calculate and store per-task cost after agent completion
- Aggregate for `wg status` display

### Step 5: Assigner Integration

- Expose registry to assignment agents via task context
- Let assigner select model from registry based on task complexity

---

## Appendix A: Example Configurations

### Minimal (No Registry — Current Behavior)

```toml
[agent]
model = "sonnet"

[models.triage]
model = "haiku"
```

Behavior: Identical to today. No registry, no tiers. Direct model names.

### Basic Tiers

```toml
[tiers]
fast = "haiku"
standard = "sonnet"
premium = "opus"
```

Behavior: All roles use their default tier. Triage → fast → haiku.
TaskAgent → standard → sonnet. Verification → premium → opus.

### Mixed Providers

```toml
[tiers]
fast = "haiku"
standard = "gpt-4o"
premium = "opus"

[[model_registry]]
id = "haiku"
provider = "anthropic"
model = "haiku"
tier = "fast"
cost_per_input_mtok = 0.25
cost_per_output_mtok = 1.25

[[model_registry]]
id = "gpt-4o"
provider = "openai"
model = "gpt-4o"
tier = "standard"
cost_per_input_mtok = 2.5
cost_per_output_mtok = 10.0

[[model_registry]]
id = "opus"
provider = "anthropic"
model = "claude-opus-4-6"
tier = "premium"
cost_per_input_mtok = 15.0
cost_per_output_mtok = 75.0
```

### Cost-Conscious with Local Fallback

```toml
[tiers]
fast = "local-qwen"
standard = "sonnet"
premium = "sonnet"     # Use standard for premium too to save cost

[[model_registry]]
id = "local-qwen"
provider = "local"
model = "qwen2.5-coder:32b"
tier = "fast"
endpoint = "http://localhost:11434/v1"
cost_per_input_mtok = 0.0
cost_per_output_mtok = 0.0

[[model_registry]]
id = "sonnet"
provider = "anthropic"
model = "sonnet"
tier = "standard"
cost_per_input_mtok = 3.0
cost_per_output_mtok = 15.0

# Override: verification still needs opus despite premium=sonnet tier
[models.verification]
model = "opus"
```

### Role-Level Tier Override

```toml
[tiers]
fast = "haiku"
standard = "sonnet"
premium = "opus"

# Override: run task agents on premium tier for this project
[models.task_agent]
tier = "premium"

# Override: evaluator on fast tier (save money, evaluations are structured)
[models.evaluator]
tier = "fast"
```
