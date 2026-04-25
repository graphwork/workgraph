# OpenRouter Executor Research: Model Config & Discovery

**Task:** or-research
**Date:** 2026-03-25

## 1. Hardcoded / Default Model References

### Files with hardcoded model names

| File | Line | Reference | Context |
|------|------|-----------|---------|
| `src/commands/native_exec.rs` | 24 | `claude-sonnet-4-latest-20250514` | `DEFAULT_MODEL` for native executor fallback |
| `src/executor/native/openai_client.rs` | 212 | `https://openrouter.ai/api/v1` | `DEFAULT_BASE_URL` for OpenAI-compatible client |
| `src/executor/native/client.rs` | 182 | `https://api.anthropic.com` | `DEFAULT_BASE_URL` for Anthropic client |
| `src/config.rs` | 459 | `https://openrouter.ai/api/v1` | `default_url_for_provider("openrouter")` |
| `src/config.rs` | 1073-1117 | haiku/sonnet/opus | `builtin_registry()` — 3 Anthropic entries |
| `src/models.rs` | 110-260 | 12 models | `ModelRegistry::with_defaults()` — full catalog |

### The `deepseek-v3.2` reference

The `deepseek-v3.2` string appears **only in test code** at `src/config.rs:4140-4156`. It is a test-only fixture for verifying custom registry entry resolution — it is NOT a real model ID used in production. The actual DeepSeek model in the registry is `deepseek/deepseek-chat-v3`.

No invalid model references exist in production code. All hardcoded model IDs use valid OpenRouter format (`provider/model-name`).

## 2. Model Selection Flow

### Flow: config.toml → executor → API call

```
User specifies model via:
  1. Per-task: wg add --model <model>
  2. Per-role: config.toml [models.<role>] model = "..."
  3. Global: config.toml [agent] model = "..."
  4. Env var: WG_MODEL
  5. Fallback: DEFAULT_MODEL (claude-sonnet-4-latest-20250514)

     ┌─────────────────────┐
     │ Config Resolution   │
     │ (config.rs)         │
     ├─────────────────────┤
     │ resolve_model_for_  │   6-step cascade:
     │ role(DispatchRole)  │   1. models.<role>.model
     │                     │   2. Legacy per-role fields
     │                     │   3. models.<role>.tier → tier system
     │                     │   4. Role default_tier → tiers → registry
     │                     │   5. models.default.model
     │                     │   6. agent.model global fallback
     │                     │
     │ Returns:            │
     │   ResolvedModel {   │
     │     model,          │   Full model ID (e.g., "deepseek/deepseek-chat-v3")
     │     provider,       │   Optional provider override
     │     registry_entry, │   Optional registry metadata
     │     endpoint,       │   Optional endpoint name
     │   }                 │
     └─────────┬───────────┘
               │
     ┌─────────▼───────────┐
     │ Provider Routing    │
     │ (provider.rs)       │
     ├─────────────────────┤
     │ create_provider_ext │   Resolves provider from:
     │                     │   1. provider_override param
     │                     │   2. [native_executor] provider field
     │                     │   3. WG_LLM_PROVIDER env var
     │                     │   4. Heuristic: contains '/' → "openai"
     │                     │              else → "anthropic"
     │                     │
     │ Routes to:          │
     │   "openai"|         │
     │   "openrouter"|     │ → OpenAiClient
     │   "local"           │
     │   _                 │ → AnthropicClient
     └─────────┬───────────┘
               │
     ┌─────────▼───────────┐
     │ API Call            │
     │ (openai_client.rs)  │
     ├─────────────────────┤
     │ OpenAiClient.send() │
     │                     │
     │ POST {base_url}/    │
     │   chat/completions  │
     │                     │
     │ Headers:            │
     │   Authorization:    │
     │     Bearer {key}    │
     │   HTTP-Referer      │  (OpenRouter attribution)
     │   X-Title           │  (OpenRouter attribution)
     │                     │
     │ Body: { model, ...} │
     └─────────────────────┘
```

### Two Parallel Model Systems

There are **two separate model registry systems**:

1. **`ModelRegistry`** (`src/models.rs`) — Used by `wg models` subcommand. Stores models in `.workgraph/models.yaml`. Has 12 hardcoded defaults with pricing, context window, capabilities, tier. Primarily used for the `wg models list/search/add` CLI.

2. **`ModelRegistryEntry` + `Config.model_registry`** (`src/config.rs`) — Used by the coordinator for dispatch routing. Stored in `config.toml` as `[[model_registry]]` entries. Has 3 built-in Anthropic entries (haiku/sonnet/opus). Used by `resolve_model_for_role()` and `wg model list`.

These two systems are **not connected**. The `ModelRegistry` in `src/models.rs` is a standalone catalog. The config-based `model_registry` is the one actually used for dispatch routing.

## 3. OpenRouter API Documentation Summary

### Models Endpoint

**`GET {base_url}/models`** (typically `https://openrouter.ai/api/v1/models`)

- **Auth:** `Authorization: Bearer {api_key}`
- **Response format:**
```json
{
  "data": [
    {
      "id": "provider/model-name",        // e.g., "anthropic/claude-sonnet-4-latest"
      "name": "Human Readable Name",
      "description": "Model description",
      "context_length": 200000,            // nullable
      "pricing": {
        "prompt": "0.000003",              // per-token USD string, nullable
        "completion": "0.000015"           // per-token USD string, nullable
      },
      "supported_parameters": ["temperature", "tools", ...],
      "architecture": {
        "modality": "text->text",          // nullable
        "tokenizer": "claude"              // nullable
      },
      "top_provider": {
        "max_completion_tokens": 16384,    // nullable
        "is_moderated": false              // nullable
      }
    }
  ]
}
```

### Auto-routing Mode

OpenRouter supports `openrouter/auto` as a model ID, which auto-selects the best model for the prompt. **No references to `openrouter/auto` exist in the codebase** — this is not currently supported.

### Model ID Format

OpenRouter model IDs follow the `provider/model-name` pattern:
- `anthropic/claude-sonnet-4-latest`
- `openai/gpt-4o`
- `deepseek/deepseek-chat-v3`
- `meta-llama/llama-4-maverick`
- `meta-llama/llama-4-maverick:free` (free tier suffix)

## 4. Existing Model Discovery Implementation

Model discovery is **already partially implemented**:

### Implemented (`src/commands/models.rs`)
- `wg models search <query>` — Search remote models by query string
- `wg models list-remote` — List all remote models
- Caching in `.workgraph/model_cache.json` with 1-hour TTL
- `--tools-only` filter for tool-supporting models
- `--no-cache` to force refresh
- JSON output mode

### API Types (`src/executor/native/openai_client.rs:1198-1292`)
- `OpenRouterModel` struct with full response fields
- `OpenRouterPricing`, `OpenRouterArchitecture`, `OpenRouterTopProvider`
- `fetch_openrouter_models()` async function
- `fetch_openrouter_models_blocking()` sync wrapper

### What's Missing
- **No validation at dispatch time.** When a user sets a model like `deepseek/nonexistent-model`, it's sent directly to the API with no pre-flight check.
- **No connection between discovery and the config registry.** The `wg models` catalog and the `config.toml` `[[model_registry]]` are separate systems.
- **No auto-import from discovery to registry.** Users must manually add models to their config after discovering them.

## 5. Existing Model Validation

### What exists (`src/config.rs:2439-2548` — `validate_config()`)
- **Rule 1:** Catches executor=`claude` with `provider/model` format models
- **Rule 2:** Catches executor=`claude` with non-Anthropic provider
- **Rule 3:** Warns when `models.<role>.model` doesn't match registry and lacks `/`
- **Rule 4:** Warns when `model_registry` entries for non-Anthropic providers lack `/` in model field
- **Rule 5:** Checks `api_key_file` existence

### What doesn't exist
- No validation that a model ID actually exists on OpenRouter
- No validation at task creation or dispatch time
- No "did you mean?" suggestions for typos
- No check against the cached model list from `/models` endpoint

## 6. Files That Need Modification for Model Discovery + Validation

### Core changes needed

| File | Change |
|------|--------|
| `src/config.rs` | Add optional model-existence validation using cached model list |
| `src/commands/models.rs` | Already has discovery — potentially add `wg models validate` subcommand |
| `src/executor/native/openai_client.rs` | Types are already defined; no changes needed |
| `src/models.rs` | Bridge gap between `ModelRegistry` and config-based registry (optional) |

### Integration points

| File | Change |
|------|--------|
| `src/commands/service/coordinator.rs` | Could validate model before spawning agent |
| `src/commands/spawn/execution.rs` | Could validate model in spawn path |
| `src/executor/native/provider.rs` | Could warn when model not found in cache |

### CLI additions

| File | Change |
|------|--------|
| `src/cli.rs` | Add `wg models validate` subcommand if desired |
| `src/commands/models.rs` | Add validation function using cache |

### Proposed approach for downstream tasks

1. **`or-model-discovery`** should build on the existing `src/commands/models.rs` infrastructure (cache, fetch, types are all there). The main work is:
   - Add a `validate_model_id(workgraph_dir, model_id) -> Result<Option<OpenRouterModel>>` function
   - Optionally integrate validation into `create_provider_ext()` as a warning (not error) when model isn't in the cached list
   - Consider adding a `wg models validate <id>` CLI command
   - Wire validation into `validate_config()` for proactive detection
