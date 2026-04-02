# wg config profiles — Setting a Complete Model Profile

## Quick Answer: One Command to Switch Everything

```bash
wg profile set openrouter
```

This single command:
1. Sets `profile = "openrouter"` in `.workgraph/config.toml`
2. Loads the benchmark registry, ranks all registered OpenRouter models by popularity + benchmarks
3. Writes the top-ranked model for each tier (fast/standard/premium) into `[tiers]`
4. Saves full ranked lists to `.workgraph/profile_ranked_tiers.json` for fallback/escalation

After running it, every dispatch role (triage, evaluator, task_agent, etc.) that hasn't been explicitly overridden via `[models]` will resolve through the tier system to an OpenRouter model.

## The Profile System

### What It Is

A **profile** is a named configuration that maps quality tiers (fast, standard, premium) to specific models. Profiles sit in the resolution chain between the hardcoded defaults and per-role overrides.

**Source:** `src/profile.rs:13-65`

### Built-in Profiles

| Profile | Strategy | fast | standard | premium |
|---------|----------|------|----------|---------|
| `anthropic` | static | `claude:haiku` | `claude:sonnet` | `claude:opus` |
| `openrouter` | dynamic | *(auto-selected from registry)* | *(auto-selected)* | *(auto-selected)* |
| `openai` | static | `openrouter:openai/gpt-4o-mini` | `openrouter:openai/gpt-4o` | `openrouter:openai/o3-pro` |

**Source:** `src/profile.rs:68-101`

List them with:
```bash
wg profile list
```

### Static vs Dynamic Profiles

- **Static profiles** (anthropic, openai) have hardcoded tier→model mappings. Setting them is instant.
- **Dynamic profiles** (openrouter) consult the benchmark registry at set-time, rank models by a composite score (popularity + benchmarks), and write the winners into config. This requires a populated registry (`wg models fetch` or manual `--registry-add` entries).

**Source:** `src/profile.rs:24-35`, `src/commands/profile_cmd.rs:67-92`

## Model Resolution Precedence

When the coordinator needs a model for a dispatch role, resolution follows this chain:

```
1. [models.<role>].model     — per-role explicit override (--set-model)
2. [models.<role>].tier      — per-role tier override
3. role.default_tier() → [tiers.<tier>]  — tier system (profile fills these)
4. [models.default].model    — default model section
5. agent.model               — global fallback
```

**Source:** `src/config.rs:1586-1594`

The profile populates step 3. Steps 1-2 always win over the profile.

### Dispatch Roles and Their Default Tiers

Each role maps to a quality tier, which determines which model it gets from the profile:

| Role | Default Tier | Typical Use |
|------|-------------|-------------|
| `triage` | fast | Dead agent triage |
| `flip_comparison` | fast | FLIP score comparison |
| `assigner` | fast | Agent assignment |
| `compactor` | fast | Context compaction |
| `chat_compactor` | fast | Chat compaction |
| `coordinator_eval` | fast | Coordinator self-eval |
| `placer` | fast | Placement analysis |
| `flip_inference` | standard | FLIP intent inference |
| `task_agent` | standard | Main task execution |
| `evaluator` | standard | Task evaluation |
| `default` | standard | Fallback |
| `evolver` | premium | Agency evolution |
| `creator` | premium | Agent identity creation |
| `verification` | premium | Task verification |

**Source:** `src/config.rs:740-758`

## Complete CLI Flows

### Flow 1: "I want this repo to use OpenRouter models for everything"

```bash
# 1. Ensure you have an OpenRouter endpoint configured
wg config --set-key openrouter --file ~/.openrouter.key

# 2. Populate the model registry (if not already done)
#    Either fetch from OpenRouter API:
wg models fetch
#    Or add models manually:
wg config --registry-add --id my-model --provider openrouter \
  --reg-model vendor/model-name --reg-tier standard \
  --endpoint openrouter

# 3. Set the profile — this auto-configures tiers from the registry
wg profile set openrouter

# 4. Verify the result
wg profile show          # Shows tier mappings + ranked alternatives
wg config --models       # Shows per-role resolved models
wg config --tiers        # Shows tier→model assignments
```

### Flow 2: "I want OpenRouter but with specific models I choose"

```bash
# Set the profile (optional — gives you escalation/fallback behavior)
wg profile set openrouter

# Override specific tiers
wg config --tier fast=openrouter:qwen/qwen-turbo
wg config --tier standard=openrouter:qwen/qwen3-coder
wg config --tier premium=openrouter:anthropic/claude-3.5-sonnet

# Verify
wg config --tiers
```

### Flow 3: "I want to override specific roles, not just tiers"

```bash
# Set a role to a specific model
wg config --set-model task_agent openrouter:qwen/qwen3-coder
wg config --set-model evaluator openrouter:google/gemini-2.5-flash
wg config --set-model triage openrouter:qwen/qwen-turbo

# These overrides take precedence over the profile's tier defaults
wg config --models   # Shows SOURCE column: "explicit" vs "tier-default"
```

### Flow 4: "Switch back to Anthropic defaults"

```bash
wg profile set anthropic
```

## Inspecting Current State

```bash
# What profile is active?
wg profile show

# What model does each role resolve to, and why?
wg config --models

# What are the tier→model assignments?
wg config --tiers

# JSON output for scripting
wg profile show --json
wg profile list --json
```

## How It Works Internally

### Config File Structure (`.workgraph/config.toml`)

```toml
profile = "openrouter"           # Active profile name

[tiers]                          # Tier→model mappings (profile writes these for dynamic profiles)
fast = "openrouter:qwen/qwen-turbo"
standard = "openrouter:qwen/qwen3-coder"
premium = "openrouter:anthropic/claude-3.5-sonnet"

[models.default]                 # Fallback for all roles
model = "claude:opus"

[models.task_agent]              # Per-role overrides (wins over tiers)
model = "openrouter:qwen/qwen3-coder"

[models.triage]
model = "openrouter:qwen/qwen-turbo"
```

### Dynamic Profile Auto-Configuration (`src/commands/profile_cmd.rs:67-92`)

When you run `wg profile set openrouter`:
1. Loads `BenchmarkRegistry` from `.workgraph/` (populated by `wg models fetch` or `--registry-add`)
2. Calls `rank_models_for_profile()` — scores models by popularity × benchmarks
3. Writes the #1 ranked model per tier into `config.tiers`
4. Saves full ranked lists to `.workgraph/profile_ranked_tiers.json`

The ranked lists enable **model escalation**: if a task fails with one model, the coordinator tries the next-ranked model in the same tier, then escalates to higher tiers. This is controlled by `max_tier_escalation_depth` in coordinator config.

**Source:** `src/profile.rs:156-236` (escalation logic)

### Profile Tier Resolution (`src/config.rs:1501-1542`)

```
effective_tiers = explicit [tiers] > profile defaults > hardcoded Anthropic fallback
```

For the `openrouter` dynamic profile, `resolve_profile_tiers()` returns `None` (dynamic profiles don't have hardcoded tiers), so the explicit `[tiers]` entries written during `wg profile set` are what's used. The hardcoded fallback (`claude:haiku/sonnet/opus`) only kicks in if both the profile and explicit tiers are empty.

## Key Files

| File | Purpose |
|------|---------|
| `src/profile.rs` | Profile definitions, tier resolution, model escalation |
| `src/commands/profile_cmd.rs` | `wg profile set/show/list` implementation |
| `src/config.rs` | Config model, `DispatchRole`, `resolve_model_for_role()` |
| `src/commands/config_cmd.rs` | `wg config --set-model/--tiers/--models` |
| `src/model_benchmarks.rs` | Benchmark registry, model ranking algorithm |
| `.workgraph/config.toml` | Persisted configuration |
| `.workgraph/profile_ranked_tiers.json` | Cached ranked model lists for escalation |
