# Design: Unified `provider:model` Naming Convention

**Task:** design-unified-provider
**Date:** 2026-03-27
**Status:** Draft

## Problem Statement

Workgraph currently requires up to three separate values to route a model request:

1. **`provider`** — which LLM backend (anthropic, openrouter, openai, local)
2. **`model`** — the model identifier (opus, minimax/minimax-m2.5, deepseek/deepseek-v3.2)
3. **`endpoint`** — named endpoint config for URL and API key

These values are resolved via independent cascades (task → agent → executor → coordinator), leading to:

- **Inconsistency**: `provider = "openrouter"` but `model = "opus"` (an Anthropic-only alias) triggers a config diagnostic warning but not an error.
- **Heuristic fragility**: The system uses `model.contains('/')` to guess executor type (native vs. claude) and provider (anthropic vs. openai-compat). This works for `deepseek/deepseek-chat` but fails for models that don't follow the `org/name` convention.
- **Three-cascade confusion**: Each of provider, model, and endpoint has its own resolution cascade with different priority orders, making it hard to predict what the effective configuration will be.

### The Prefix-Stripping Bug (autohaiku)

In autohaiku's workgraph task templates, tasks are created with `--model deepseek-v3.2` (the registry short alias). This resolves through `resolve_model_via_registry()` to `deepseek/deepseek-v3.2` (the full API model ID from the registry entry). The resolved model then passes correctly through the native executor to OpenRouter.

However, if a user sets `model = "deepseek/deepseek-v3.2"` directly (the full ID, not the alias) without a registry entry or provider override, the system's heuristic path in `create_provider_ext()` (provider.rs:98-106) kicks in:

1. The `/` in the model name triggers the `"openai"` provider heuristic (correct for OpenRouter)
2. Since `provider_name != "anthropic"`, the model is NOT stripped (correct)
3. The model passes through to `OpenAiClient` with the full `deepseek/deepseek-v3.2` ID

So the model itself isn't incorrectly stripped. The real bug surfaces in a different scenario: when a user configures `provider = "anthropic"` but `model = "deepseek/deepseek-v3.2"`, the prefix-stripping code at provider.rs:110-114 strips `anthropic/` (which doesn't match), so it's also fine. The actual problem is **configuration ambiguity**: there's no single source of truth that says "this model lives on this provider at this endpoint."

The `provider:model` convention solves this by encoding the routing information directly in the model string.

## Naming Grammar

```
model_spec := provider_prefix? model_id
provider_prefix := provider_name ":"
provider_name := "claude" | "openrouter" | "openai" | "codex" | "gemini" | "ollama" | "native"
model_id := <opaque string passed to the provider's API>
```

### Examples

| User writes | Provider | Executor | API model ID sent |
|---|---|---|---|
| `claude:opus` | anthropic | claude (CLI) | `opus` |
| `claude:claude-sonnet-4-latest` | anthropic | claude (CLI) | `claude-sonnet-4-latest` |
| `openrouter:deepseek/deepseek-v3.2` | openrouter | native | `deepseek/deepseek-v3.2` |
| `openrouter:minimax/minimax-m2.5` | openrouter | native | `minimax/minimax-m2.5` |
| `openai:gpt-5` | openai | native | `gpt-5` |
| `codex:think-hard-model-1` | codex | codex (CLI) | `think-hard-model-1` |
| `gemini:gemini-2.0-flash-001` | google | native | `gemini-2.0-flash-001` |
| `ollama:llama3` | local | native | `llama3` |
| `ollama:deepseek-coder-v2:16b` | local | native | `deepseek-coder-v2:16b` |
| `llamacpp:my-model` | local | native | `my-model` |
| `vllm:meta-llama/Llama-3-70B` | local | native | `meta-llama/Llama-3-70B` |
| `local:whatever` | local | native | `whatever` |
| `native:deepseek/deepseek-v3.2` | (auto-detect) | native | `deepseek/deepseek-v3.2` |
| `opus` (bare) | anthropic | claude (CLI) | `opus` |
| `deepseek/deepseek-v3.2` (bare, legacy) | openai-compat (heuristic) | native (heuristic) | `deepseek/deepseek-v3.2` |

### Provider → Executor Mapping

| Provider prefix | Executor type | API endpoint default | Notes |
|---|---|---|---|
| `claude` | `claude` (CLI) | Anthropic API (via Claude CLI) | Delegates to `claude` binary; supports bare aliases (opus, sonnet, haiku) |
| `openrouter` | `native` | `https://openrouter.ai/api/v1` | OpenAI-compatible; model ID is the full `org/model` string |
| `openai` | `native` | `https://api.openai.com/v1` | OpenAI-compatible; model ID is e.g. `gpt-5` |
| `codex` | `codex` (CLI) | Codex CLI | New executor type; delegates to `codex` binary |
| `gemini` | `native` | `https://generativelanguage.googleapis.com/v1beta/openai` | Google's OpenAI-compatible endpoint |
| `ollama` | `native` | `http://localhost:11434/v1` | Local Ollama; OpenAI-compatible |
| `llamacpp` | `native` | `http://localhost:8080/v1` | llama.cpp server; OpenAI-compatible |
| `vllm` | `native` | `http://localhost:8000/v1` | vLLM server; OpenAI-compatible |
| `local` | `native` | (from endpoint config or `http://localhost:11434/v1`) | Generic local server; OpenAI-compatible. Doesn't require API key. |
| `native` | `native` | (from endpoint config) | Explicit native executor; auto-detect provider from model ID |

### Bare Name Handling (Backward Compatibility)

Bare model names (no `:` prefix) are handled by a compatibility layer:

1. **Built-in tier aliases** (`opus`, `sonnet`, `haiku`): Treated as `claude:<alias>`. These are the only bare names that default to the Claude CLI executor.

2. **Registry alias** (e.g., `deepseek-v3.2`, `minimax-m2.5`): Looked up in `[[model_registry]]`. The registry entry's `provider` field determines routing. This path is **unchanged** from today.

3. **`org/model` format** (e.g., `deepseek/deepseek-v3.2`): Legacy heuristic continues to work — `/` triggers native executor with OpenAI-compat provider auto-detection. Equivalent to `openrouter:<model>` when an OpenRouter endpoint is configured.

4. **Unknown bare name** (e.g., `my-custom-model`): Treated as `claude:<name>` for backward compat (matches current behavior where bare names default to Anthropic).

**Rule**: The `:` is the unambiguous signal that the provider is explicitly specified. Absence of `:` triggers the compatibility layer.

## Local Model Providers

Local inference servers (Ollama, llama.cpp, vLLM, LM Studio, etc.) all expose OpenAI-compatible API endpoints. The unified naming handles them naturally:

| Server | Provider prefix | Default endpoint | API key required? |
|---|---|---|---|
| Ollama | `ollama` | `http://localhost:11434/v1` | No |
| llama.cpp (`server`) | `llamacpp` | `http://localhost:8080/v1` | No |
| vLLM | `vllm` | `http://localhost:8000/v1` | No |
| LM Studio | `local` | `http://localhost:1234/v1` | No |
| Generic | `local` | (from endpoint config) | No |

All local providers map to the `native` executor (OpenAI-compatible client) with no API key validation. The `local` prefix is the generic catch-all for any OpenAI-compatible local server.

**Endpoint override**: If the server isn't on the default port, configure a named endpoint:

```toml
[[llm_endpoints.endpoints]]
name = "my-ollama"
provider = "ollama"
url = "http://gpu-box:11434/v1"
```

Then use: `--model ollama:llama3 --endpoint my-ollama`, or set it in the models config:

```toml
[models.task_agent]
model = "ollama:llama3"
endpoint = "my-ollama"
```

## Interaction with Tiers

Tier config uses model specs:

```toml
[tiers]
fast = "openrouter:qwen/qwen-turbo"
standard = "openrouter:minimax/minimax-m2.5"
premium = "claude:opus"
```

When a tier resolves to a model spec with a provider prefix, the provider prefix determines routing directly. No registry lookup or heuristic needed.

## Interaction with Model Registry

Registry entries gain an optional `provider_prefix` field for display, but the primary change is that the `id` field (short alias) can now include the provider prefix:

```toml
[[model_registry]]
id = "openrouter:minimax-m2.5"  # or just "minimax-m2.5" for backward compat
provider = "openrouter"
model = "minimax/minimax-m2.5"
```

When the user writes `--model minimax-m2.5` (bare alias) and it resolves to a registry entry with `provider = "openrouter"`, the behavior is identical to writing `--model openrouter:minimax/minimax-m2.5`.

## Interaction with `[models.*]` Role Config

The `[models.*]` sections currently have separate `provider` and `model` fields:

```toml
[models.evaluator]
provider = "openrouter"
model = "qwen/qwen-turbo"
endpoint = "openrouter"
```

With unified naming, this simplifies to:

```toml
[models.evaluator]
model = "openrouter:qwen/qwen-turbo"
endpoint = "openrouter"   # still available for explicit endpoint override
```

The `provider` field becomes optional/deprecated — if present, it's used as a fallback when the model string doesn't contain a `:`. The explicit `endpoint` field remains for cases where a provider has multiple configured endpoints.

## Parsing Implementation

A single parse function replaces scattered heuristics:

```rust
/// Parse a model spec into (provider, model_id) components.
/// Returns None for provider if it's a bare name (backward compat path).
pub fn parse_model_spec(spec: &str) -> (Option<&str>, &str) {
    // The ':' delimiter is unambiguous because:
    // - Provider names never contain ':'
    // - Model IDs may contain '/' but never ':'
    if let Some((provider, model)) = spec.split_once(':') {
        (Some(provider), model)
    } else {
        (None, spec)
    }
}
```

This replaces:
- The `model.contains('/')` heuristic in `requires_native_executor()` (coordinator.rs:2683-2695)
- The `model.starts_with("anthropic/")` check in `create_provider_ext()` (provider.rs:98-106)
- The `model.strip_prefix("anthropic/")` in `create_provider_ext()` (provider.rs:110-114)
- The separate `resolve_provider()` cascade (execution.rs:1110-1120)

## Root Cause of the Autohaiku Prefix Bug

The autohaiku config has:
```toml
[coordinator]
executor = "native"
model = "minimax/minimax-m2.5"
provider = "openrouter"
```

And observer tasks specify `--model deepseek-v3.2` (the registry alias). The resolution chain:

1. `resolve_model()` picks the task model: `deepseek-v3.2`
2. `resolve_model_via_registry()` finds the registry entry → returns `deepseek/deepseek-v3.2`
3. `create_provider_ext()` receives the resolved model `deepseek/deepseek-v3.2` with `WG_LLM_PROVIDER=openrouter`
4. Provider is `openrouter` (from env), model passes through un-stripped → correct

The **actual** prefix-stripping scenario that causes issues: if a user creates a task with `--model deepseek/deepseek-v3.2` (the full ID, not the alias) and the provider resolution cascade returns a different provider than expected (e.g., no `WG_LLM_PROVIDER` is set, native_executor config has no provider), the heuristic at provider.rs:98-106 kicks in and sets provider to `"openai"` based on the `/`. This works for OpenRouter (OpenAI-compatible), but for a provider that's NOT OpenAI-compatible (e.g., Gemini's native API), the heuristic would send the request to the wrong endpoint.

**Root cause**: The model string `deepseek/deepseek-v3.2` encodes the OpenRouter org/model namespace, but the system interprets the `/` as a provider signal. These are different semantic layers that happen to share the same delimiter. The unified `provider:model` naming explicitly separates these layers: `openrouter:deepseek/deepseek-v3.2`.

## Migration Plan

### Phase 1: Parser + Backward Compat (non-breaking)

**Code changes:**
1. Add `parse_model_spec()` function to `src/config.rs`
2. Wire it into `resolve_model_via_registry()` — extract provider prefix before registry lookup
3. Wire it into `build_inner_command()` — use parsed provider to select executor type
4. Wire it into `create_provider_ext()` — use parsed provider instead of heuristic
5. Keep all existing heuristic paths as fallbacks when provider is `None` (bare names)

**Config changes:** None required. Existing configs work unchanged.

**Files modified:**
- `src/config.rs` — `parse_model_spec()`, `ResolvedModel` gains parsed provider
- `src/commands/spawn/execution.rs` — `resolve_model_via_registry()` uses parsed provider
- `src/commands/service/coordinator.rs` — `requires_native_executor()` uses parsed provider
- `src/executor/native/provider.rs` — `create_provider_ext()` uses parsed provider
- `src/commands/service/coordinator_agent.rs` — coordinator agent model resolution

### Phase 2: Config Surface (minor breaking)

**Code changes:**
1. `[models.*]` sections accept `model = "provider:model_id"` and auto-populate `provider`
2. `[tiers]` values accept `provider:model_id` format
3. `wg config --model` accepts `provider:model_id` and stores it as-is
4. Add `wg config --show-model-resolution <role>` diagnostic to show the full resolution chain

**Config migration tool:**
```bash
wg config migrate-models  # Rewrites provider+model pairs to provider:model format
```

### Phase 3: Deprecation

1. Emit deprecation warnings when separate `provider` field is used alongside a `model` that already has a `:` prefix
2. Emit deprecation warnings for `model.contains('/')` heuristic matches (suggest adding explicit prefix)
3. After one release cycle, remove heuristic fallbacks

### Phase 4: Codex Executor

1. Add `codex` executor type to `ExecutorRegistry::default_config()`
2. Map `codex:*` provider prefix to the codex executor
3. Add codex CLI invocation pattern to `build_inner_command()`

## Config Changes Summary

### Before (current)

```toml
[coordinator]
executor = "native"
model = "minimax/minimax-m2.5"
provider = "openrouter"

[models.evaluator]
provider = "openrouter"
model = "qwen/qwen-turbo"
endpoint = "openrouter"

[tiers]
fast = "qwen/qwen-turbo"    # bare alias
standard = "minimax-m2.5"    # bare alias
premium = "opus"              # built-in alias
```

### After (unified)

```toml
[coordinator]
model = "openrouter:minimax/minimax-m2.5"
# executor and provider are inferred from the prefix

[models.evaluator]
model = "openrouter:qwen/qwen-turbo"
endpoint = "openrouter"   # only needed if provider has multiple endpoints

[tiers]
fast = "openrouter:qwen/qwen-turbo"
standard = "openrouter:minimax/minimax-m2.5"
premium = "claude:opus"
```

### Task-level

```bash
# Before
wg add "My task" --model "deepseek/deepseek-v3.2"

# After (explicit, recommended)
wg add "My task" --model "openrouter:deepseek/deepseek-v3.2"

# Still works (bare name, registry lookup)
wg add "My task" --model "deepseek-v3.2"

# Still works (legacy heuristic, deprecated)
wg add "My task" --model "deepseek/deepseek-v3.2"
```

## Impact on Autohaiku

The autohaiku shell scripts (`disk-observer.sh`, etc.) call OpenRouter directly via `curl` with `MODEL="${DISK_OBSERVER_MODEL:-deepseek/deepseek-v3.2}"`. These are not affected by workgraph model resolution — they bypass it entirely.

The workgraph task templates in `haiku-system-design.md` use `--model deepseek-v3.2` (bare alias). Post-migration, these could be updated to `--model openrouter:deepseek/deepseek-v3.2` for explicitness, but the bare alias path continues to work via registry lookup.

## Open Questions

1. **Should `codex` be a new executor type or reuse the `claude` executor?** Codex has a different CLI interface. Likely needs its own executor.

2. **Should the `native` prefix be special?** Using `native:model` says "use the native executor, figure out the provider from the model ID." This is useful as an escape hatch but may reintroduce heuristic ambiguity.

3. **Multiple endpoints per provider**: A user might have two OpenRouter endpoints (different API keys for different teams). The `endpoint` field remains the mechanism for this — the provider prefix alone isn't enough. Format could be `openrouter@team-b:model-id` but that adds complexity.

4. **Amplifier executor**: Currently the amplifier executor uses `provider:model` format already (execution.rs:816). Should it be a provider prefix too? E.g., `amplifier:provider-openai:minimax/minimax-m2.5`? This gets nested. Better to keep amplifier as a separate executor type.
