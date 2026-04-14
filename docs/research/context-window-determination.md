# Research: How Context Length Is Determined for OpenAI-Compatible Endpoints

**Task:** `research-how-context`
**Date:** 2026-04-13
**Status:** Complete

## Executive Summary

Context window sizes for OpenAI-compatible endpoints are determined through a **multi-layer fallback chain** with three independent data sources. The resolved value feeds into two distinct consumer paths: (1) the **OpenAI client** (per-request max_tokens capping) and (2) the **coordinator compaction threshold** (when to trigger context distillation). There are significant gaps in how custom/unknown models get context window values, particularly for non-OpenRouter endpoints.

---

## 1. Data Sources (Where Context Window Sizes Are Stored)

### 1.1 Built-in Config Registry (`config.rs` builtin_registry)

**Location:** `src/config.rs:1540–1630` (hardcoded in `Config::builtin_registry()`)

The `ModelRegistryEntry` struct (`src/config.rs:1040–1078`) includes a `context_window: u64` field. Built-in entries cover only **Anthropic Claude models** (haiku, sonnet, opus), all hardcoded at **200,000 tokens**.

This registry is used by `effective_registry()` → `registry_lookup()` for compaction threshold calculation.

**Gap:** No built-in entries exist for OpenAI, Google, DeepSeek, or other OpenAI-compatible models. A user running `openrouter:deepseek/deepseek-chat` will get `context_window: 0` from this registry, causing the compaction threshold to fall back to the static `compaction_token_threshold` (default 100,000).

### 1.2 Models YAML Registry (`models.yaml` / `ModelRegistry`)

**Location:** `src/models.rs:46–72` (`ModelEntry` struct), loaded from `.workgraph/models.yaml`

The `ModelRegistry` (separate from `Config::model_registry`) has `ModelEntry.context_window: u64` with hardcoded defaults for 13 models (`src/models.rs:123–273`):
- Claude opus/sonnet: 1,000,000
- Claude haiku: 200,000
- GPT-4o/4o-mini: 128,000
- DeepSeek chat/R1: 164,000
- Gemini 2.5-pro/2.0-flash: 1,000,000
- Llama 4 maverick: 1,000,000 / scout: 512,000
- Qwen3-235b: 131,072

**Note:** This registry is primarily used by `wg models` commands and the benchmark scoring system. It is **NOT** used by the provider creation path (`create_provider_ext`) or the compaction threshold calculation. This is a separate data silo.

### 1.3 OpenRouter API (`/api/v1/models`)

**Location:** `src/executor/native/openai_client.rs:2011–2027` (`OpenRouterModel` struct)

The OpenRouter API returns `context_length: Option<u64>` per model. This data is:
- Fetched by `fetch_openrouter_models()` (`openai_client.rs:2061`)
- Consumed by the benchmark system (`model_benchmarks.rs:1329`) which maps `context_length` → `BenchmarkModel.context_window`
- **NOT** directly consumed by the provider creation path or compaction threshold

### 1.4 Endpoint Config (`config.toml` [[llm_endpoints]])

**Location:** `src/config.rs:540–542`

```toml
[[llm_endpoints]]
name = "my-local-server"
provider = "local"
url = "http://localhost:8080/v1"
context_window = 32768  # Optional override
```

The `EndpointConfig.context_window: Option<u64>` field is the **only user-configurable per-endpoint override**. When set, it takes highest priority in the resolution chain.

---

## 2. Resolution Chain (How Context Window Is Determined at Runtime)

### 2.1 Provider Creation Path (`create_provider_ext`)

**Location:** `src/executor/native/provider.rs:77–255`

When creating an OpenAI-compatible provider, context window is resolved at lines 147–161:

```
Endpoint config context_window (highest priority)
  → Config model_registry entry context_window (if model found and > 0)
    → OpenAiClient default: 128,000 (hardcoded in openai_client.rs:288)
```

Specifically:

1. **`endpoint_context_window`** = `endpoint.context_window` from matching `[[llm_endpoints]]` entry
2. **`registry_context_window`** = `config.effective_registry()` lookup by model ID, only if `context_window > 0`
3. **`resolved_context_window`** = `endpoint_context_window.or(registry_context_window)` — first non-None wins
4. If `resolved_context_window` is `Some(cw)`, call `client.with_context_window(cw as usize)` (line 230–232)
5. If `None`, OpenAiClient keeps its **hardcoded default of 128,000** (line 288)

**Critical behavior of `with_context_window`** (openai_client.rs:360–374):
- Sets `context_window_tokens`
- Caps `max_tokens` to `context_window / 4` if the default would exceed 50% of the window
- Example: 32k window → max_tokens capped from 16,384 to 8,192
- This prevents SGLang/vLLM HTTP 400 errors where `input_tokens + max_tokens > context_window`

### 2.2 Compaction Threshold Path (`effective_compaction_threshold`)

**Location:** `src/config.rs:3269–3291`

Completely independent from the provider creation path. Uses:

```
Config model_registry entry context_window * compaction_threshold_ratio (default 0.8)
  → compaction_token_threshold (default 100,000)
```

1. Resolve coordinator model: `coordinator.model` → `agent.model` → None
2. Parse provider:model spec to get registry lookup ID
3. Look up in `effective_registry()` (built-in + user config `[model_registry]` entries)
4. If found and `context_window > 0`: return `context_window * ratio` (e.g., 200,000 × 0.8 = 160,000)
5. Otherwise: fall back to static `compaction_token_threshold` (default 100,000)

**Critical gap:** Only built-in entries have `context_window > 0`, and those are only Anthropic models. Any OpenAI-compatible model (OpenRouter, local, etc.) will silently fall back to 100k unless the user manually adds a `[[model_registry]]` entry with `context_window` in their config.

### 2.3 Agent-Level Context Pressure (`ContextBudget`)

**Location:** `src/executor/native/resume.rs:716–748`, used in `agent.rs:193`

The `ContextBudget` is initialized from the provider's `context_window()` method:

```rust
let context_budget = ContextBudget::with_window_size(client.context_window());
```

This drives tiered context pressure management:
- **70%** of window → warning injection
- **75%** → emergency compaction trigger
- **95%** → clean exit

The provider's `context_window()` comes from the resolution chain in §2.1.

### 2.4 Agent-Level Heuristic Fallback (`get_model_context_window`)

**Location:** `src/executor/native/agent.rs:1304–1318`

A **separate, hardcoded heuristic** used only for `inject_context_warnings()`:

```rust
fn get_model_context_window(&self) -> usize {
    let model = self.client.model().to_lowercase();
    if model.contains("minimax") || model.contains("qwen-2.5") { 28_000 }
    else if model.contains("deepseek") { 56_000 }
    else if model.contains("llama-3.1") { 120_000 }
    else if model.contains("claude") { 180_000 }
    else { 28_000 } // Conservative default
}
```

This function is **not connected** to the registry or endpoint config. It uses conservative string-matching on model names. Unknown models get 28,000 tokens. This is only used for OpenRouter-specific context warning injection (separate from the ContextBudget system).

---

## 3. Data Flow Diagram

```
                           ┌──────────────────────┐
                           │  EndpointConfig       │
                           │  context_window       │
                           │  (config.toml)        │
                           └──────────┬───────────┘
                                      │ highest priority
                                      ▼
┌─────────────────────┐    ┌──────────────────────┐    ┌─────────────────────┐
│ models.yaml         │    │  Config model_registry│    │ OpenRouter API      │
│ (ModelRegistry)     │    │  (effective_registry) │    │ context_length      │
│ NOT used by         │    │  built-in: Claude only│    │ via /api/v1/models  │
│ provider creation   │    └──────────┬───────────┘    │ NOT used at runtime │
│ or compaction       │               │ second priority │ (benchmarks only)   │
└─────────────────────┘               ▼                └─────────────────────┘
                           ┌──────────────────────┐
                           │  resolved_context_    │
                           │  window               │
                           └──────┬───────┬───────┘
                                  │       │
                    ┌─────────────┘       └──────────────────┐
                    ▼                                        ▼
           ┌───────────────┐                      ┌──────────────────┐
           │ OpenAiClient   │                      │ effective_       │
           │ .with_context_ │                      │ compaction_      │
           │ window()       │                      │ threshold()      │
           │ → max_tokens   │                      │ = cw * 0.8       │
           │   capping      │                      │ fallback: 100k   │
           └───────┬───────┘                      └──────────────────┘
                   │
                   ▼
           ┌───────────────┐
           │ ContextBudget  │
           │ .with_window_  │
           │ size()         │
           │ → 70%/75%/95%  │
           │   thresholds   │
           └───────────────┘
```

---

## 4. Identified Gaps and Issues

### 4.1 Three Disconnected Registries

There are three independent sources of model context window data:
1. `Config::model_registry` / `effective_registry()` — used by compaction threshold
2. `ModelRegistry` (models.yaml) — used by `wg models` commands only
3. OpenRouter API `context_length` — used by benchmarks only

These **never cross-pollinate**. A model fetched from OpenRouter with `context_length: 128000` does not update the Config registry, so compaction threshold calculation doesn't benefit from it.

### 4.2 Hardcoded Fallbacks for Unknown Models

When a model is not in any registry:
- **OpenAiClient** defaults to 128,000 tokens (`openai_client.rs:288`)
- **Compaction threshold** defaults to 100,000 tokens (`config.rs:2533`)
- **Context warnings** default to 28,000 tokens (`agent.rs:1316`)

These three defaults are inconsistent and none of them attempt to discover the actual context window from the endpoint.

### 4.3 No Runtime Discovery

The OpenAI-compatible `/models` endpoint typically returns `context_length` per model. Workgraph fetches this for OpenRouter (via `fetch_openrouter_models`) but only feeds it to the benchmark system. The provider creation path and compaction threshold path do not query `/models` at runtime to discover context window sizes.

### 4.4 Endpoint Config `context_window` Never Set by CLI

The `wg endpoint add` command (`src/commands/endpoints.rs:155`) always sets `context_window: None`. There's no `--context-window` flag on the endpoint add command. Users must manually edit `config.toml` to set this value.

### 4.5 Duplicate Heuristic in Agent

`get_model_context_window()` (`agent.rs:1304`) is a separate model-name-based heuristic that duplicates (and contradicts) the registry-based resolution. It uses conservative values that don't match actual model capabilities (e.g., DeepSeek → 56k, actual is 164k).

---

## 5. Recommendations for Downstream Tasks

1. **Unify registries:** Consider making the compaction threshold path and provider creation path read from a single authoritative source that includes OpenAI-compatible models.

2. **Runtime discovery:** When creating an OpenAI-compatible provider, query `/models` to discover `context_length` if not already configured. Cache the result.

3. **Propagate OpenRouter data:** The benchmark system already fetches context_length from OpenRouter. This data could be propagated to the Config registry.

4. **Add `--context-window` to `wg endpoint add`:** Allow users to set this at endpoint creation time.

5. **Remove or update `get_model_context_window`:** Replace the hardcoded heuristic with a call to `client.context_window()` which already has the resolved value.

---

## 6. Key File References

| File | Lines | Purpose |
|------|-------|---------|
| `src/config.rs` | 540–542 | `EndpointConfig.context_window` definition |
| `src/config.rs` | 1040–1098 | `ModelRegistryEntry` struct with `context_window` |
| `src/config.rs` | 1540–1630 | Built-in registry (Claude models only) |
| `src/config.rs` | 1635–1648 | `effective_registry()` — merges built-in + user entries |
| `src/config.rs` | 1700–1701 | `registry_lookup()` — short ID lookup |
| `src/config.rs` | 3269–3291 | `effective_compaction_threshold()` — compaction decision |
| `src/config.rs` | 2317–2325 | `compaction_token_threshold` / `compaction_threshold_ratio` config |
| `src/executor/native/provider.rs` | 147–161 | Context window resolution chain |
| `src/executor/native/provider.rs` | 230–232 | Wiring resolved context window to OpenAI client |
| `src/executor/native/openai_client.rs` | 264, 288 | `context_window_tokens` field, 128k default |
| `src/executor/native/openai_client.rs` | 360–374 | `with_context_window()` — capping logic |
| `src/executor/native/openai_client.rs` | 1350–1352 | `context_window()` trait impl |
| `src/executor/native/openai_client.rs` | 2011–2027 | `OpenRouterModel.context_length` from API |
| `src/executor/native/agent.rs` | 193 | `ContextBudget::with_window_size()` initialization |
| `src/executor/native/agent.rs` | 1304–1318 | `get_model_context_window()` hardcoded heuristic |
| `src/executor/native/resume.rs` | 716–748 | `ContextBudget` struct and thresholds |
| `src/models.rs` | 46–72 | `ModelEntry` struct (separate models.yaml registry) |
| `src/models.rs` | 123–273 | Default model entries with context_window values |
| `src/model_benchmarks.rs` | 1329 | OpenRouter `context_length` → benchmark model |
| `src/commands/endpoints.rs` | 155 | Endpoint add always sets `context_window: None` |
