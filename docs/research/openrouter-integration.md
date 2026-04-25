# OpenRouter Integration: Status & Configuration Guide

**Task:** research-openrouter-integration
**Date:** 2026-03-05

## Current Capability Status

| Feature | Status | Notes |
|---------|--------|-------|
| OpenAI-compatible HTTP client | Working | `src/executor/native/openai_client.rs` — full tool-use support |
| API key resolution | Working | Env vars `OPENROUTER_API_KEY` / `OPENAI_API_KEY`, or `[native_executor] api_key` in config.toml |
| Model routing per role | Working | `wg config --set-model <role> <model> --set-provider <role> openrouter` |
| Lightweight LLM dispatch | Working | `src/service/llm.rs` — routes "openai"/"openrouter" providers to `call_openai_native()` |
| TUI add-endpoint form | Working | Press `a` in Config tab to add an endpoint (name, provider, url, model, api_key) |
| Endpoint storage in config | Working | `[llm_endpoints] endpoints = [...]` in config.toml |
| Streaming JSON | **NOT implemented** | `OpenAiClient` hardcodes `stream: false`; Anthropic client has streaming |
| Endpoint → role binding | **NOT implemented** | Endpoints are stored but never consulted by `resolve_model_for_role()` or executors |
| CLI endpoint management | **NOT implemented** | No `wg config --add-endpoint` CLI command; TUI-only or manual TOML editing |
| `.openrouter.key` file loading | **NOT implemented** | Key resolution checks env vars and `[native_executor] api_key` only |

## Detailed Findings

### 1. API Key Loading

**Current resolution order** (`resolve_openai_api_key()` at `openai_client.rs:490-515`):
1. `OPENROUTER_API_KEY` env var
2. `OPENAI_API_KEY` env var
3. `[native_executor] api_key` in `.workgraph/config.toml`

**The `.openrouter.key` file in the project root is NOT read by any code path.** To use it today, you must either:
- Export it: `export OPENROUTER_API_KEY=$(cat .openrouter.key)`
- Copy it into config: add `[native_executor]\napi_key = "sk-or-v1-..."` to `.workgraph/config.toml`

**`EndpointConfig.api_key` is stored but never consumed.** The endpoint config has an `api_key` field (`config.rs:294`), but no code reads it when making API calls. The native executor (`native_exec.rs:32-104`) reads from `[native_executor]` section, not `[llm_endpoints]`.

**Gap:** Need to wire `EndpointConfig.api_key` into the client creation path, or at minimum support reading from a key file path.

### 2. Endpoint Configuration

#### Via TUI (works today)
1. Open `wg viz` and navigate to the Config tab
2. Press `a` to start the add-endpoint flow
3. Fill in fields: Name, Provider, URL, Model, API Key (Tab between fields)
4. Press `Ctrl+S` to save

The TUI writes directly to `config.llm_endpoints.endpoints` and saves to disk (`state.rs:5590-5621`).

#### Via CLI (not supported)
There is **no** `--add-endpoint` or similar CLI flag in `wg config`. The `update()` function in `config_cmd.rs` has no endpoint-related parameters. You would need to add a new subcommand or flag.

#### Via Direct TOML Editing (works today)
Edit `.workgraph/config.toml`:
```toml
[llm_endpoints]
[[llm_endpoints.endpoints]]
name = "OpenRouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
model = "anthropic/claude-sonnet-4-latest"
api_key = "sk-or-v1-..."
is_default = false
```

**Caveat:** This stores the endpoint but nothing reads it for actual API calls yet.

### 3. Per-Role Endpoint Binding

**`resolve_model_for_role()`** (`config.rs:589-665`) resolves a `ResolvedModel { model, provider }` per dispatch role. The `provider` field is a string like `"openrouter"`, `"openai"`, or `"anthropic"`.

**How it's consumed:**
- **Lightweight dispatch** (`llm.rs:31-48`): Checks `provider` string and routes to `call_openai_native()` for "openai"/"openrouter", or `call_anthropic_native()` for "anthropic". Falls back to Claude CLI.
- **Native executor** (`native_exec.rs:32-104`): Does NOT use `resolve_model_for_role()`. Instead reads `[native_executor] provider` from config.toml or `WG_LLM_PROVIDER` env var.

**To use OpenRouter for evaluator/assigner roles (lightweight dispatch):**
```bash
wg config --set-model evaluator "anthropic/claude-sonnet-4-latest" --set-provider evaluator openrouter
wg config --set-model assigner "anthropic/claude-sonnet-4-latest" --set-provider assigner openrouter
```
This works today for lightweight calls. The `call_openai_native()` function will use `OpenAiClient::from_env()`, which requires `OPENROUTER_API_KEY` in the environment.

**For task agents (native executor):** The native executor ignores per-role routing. It uses `[native_executor] provider` globally. You cannot assign "use OpenRouter for this task but Anthropic for that one" at the executor level.

**Gap:** The `ResolvedModel.provider` from role resolution is not plumbed into the native executor spawn path. Endpoint configs (with their per-endpoint API keys) are never consulted.

### 4. Streaming JSON

**Anthropic client** (`client.rs`): Full streaming support via `messages_streaming()` and `messages_stream_raw()`. Parses SSE events, accumulates content blocks.

**OpenAI client** (`openai_client.rs`): **No streaming support.** The `stream` field in `OaiRequest` is always set to `false` (`line 385`). The `LlmClient::send()` trait method is non-streaming only.

**Impact:** Native executor agents using OpenRouter cannot stream progress. The agent loop (`agent.rs`) calls `client.send()` which is non-streaming for both providers, but the Anthropic client at least has the streaming infrastructure for future use.

**Gap:** Need to add streaming SSE parsing to `OpenAiClient`, similar to the Anthropic client's `messages_streaming()`. OpenRouter and OpenAI both support SSE streaming with the same format.

### 5. OpenRouter-Specific Concerns

#### Caching
OpenRouter does **not** support Anthropic-style prompt caching (`cache_control` blocks). The `Usage` struct has `cache_creation_input_tokens` and `cache_read_input_tokens` fields, but the OpenAI client's `translate_response()` always sets these to `None` (`openai_client.rs:365-366`). This is correct — OpenRouter doesn't pass through Anthropic's caching headers.

**Implication:** Using OpenRouter for high-volume lightweight dispatch (triage, evaluation) will cost more than direct Anthropic API calls because there's no caching.

#### Tool Use
OpenRouter passes through `tool_calls` correctly for models that support it. The `OpenAiClient` properly translates between Anthropic-style `ToolUse`/`ToolResult` content blocks and OpenAI-format `tool_calls`/`tool` messages. The code handles:
- Function definitions (`translate_tools()`, line 183)
- Assistant tool calls (`translate_messages()`, line 266)
- Tool results as `role: "tool"` messages (line 221)
- Response tool calls parsing (`translate_response()`, line 333)

This should work with Claude models via OpenRouter and with OpenAI models.

#### Model Naming
OpenRouter expects **provider-prefixed names**: `anthropic/claude-sonnet-4-latest`, `openai/gpt-4o`, `google/gemini-2.0-flash`, etc. The current code passes the model string directly — no transformation.

The model registry (`src/commands/models.rs:85`) defaults new models to provider `"openrouter"`, suggesting OpenRouter-format names are expected when using that provider.

**Working examples:**
- `anthropic/claude-sonnet-4-latest`
- `anthropic/claude-haiku-4-latest`
- `openai/gpt-4o`
- `google/gemini-2.0-flash`

#### Rate Limits & Pricing
OpenRouter applies its own rate limits per API key (separate from upstream provider limits). Pricing is typically slightly higher than direct API access (OpenRouter adds a small margin). Check https://openrouter.ai/models for current pricing.

### 6. Concrete Gaps (Implementation Needed)

Listed in priority order:

1. **Endpoint-aware API key resolution**: `OpenAiClient::from_env()` and `create_client()` in `native_exec.rs` don't read from `[llm_endpoints]` endpoints. Need to plumb `EndpointConfig.api_key` into client creation when a matching endpoint exists.

2. **Endpoint → role binding**: `resolve_model_for_role()` returns `(model, provider)` but not an endpoint reference. Need to either:
   - Add an `endpoint` field to `RoleModelConfig` / `ResolvedModel`
   - Or auto-match endpoints by provider name

3. **CLI endpoint management**: Add `wg config --add-endpoint <name> --provider openrouter --url <url> --api-key <key>` (and `--remove-endpoint`, `--list-endpoints`).

4. **Key file support**: Add ability to read API keys from file paths (e.g., `.openrouter.key`) rather than only env vars or inline config values. This avoids putting raw keys in config.toml.

5. **OpenAI client streaming**: Add `messages_streaming()` to `OpenAiClient` using OpenAI SSE format (similar to the Anthropic client). Low priority since the agent loop currently uses non-streaming for both providers.

6. **Native executor per-role routing**: The spawn path for task agents doesn't use `resolve_model_for_role()`. It reads `[native_executor] provider` globally. To support "OpenRouter for evaluators, Anthropic for task agents," the executor spawn needs to accept provider from the resolved role config.

## How to Use OpenRouter Today (Workaround)

### For lightweight dispatch (evaluation, triage, assignment):

```bash
# 1. Set the API key in environment
export OPENROUTER_API_KEY=$(cat .openrouter.key)

# 2. Configure roles to use OpenRouter
wg config --set-model evaluator "anthropic/claude-haiku-4-latest" \
          --set-provider evaluator openrouter
wg config --set-model triage "anthropic/claude-haiku-4-latest" \
          --set-provider triage openrouter
```

### For native executor task agents:

```bash
# 1. Set the API key
export OPENROUTER_API_KEY=$(cat .openrouter.key)

# 2. Configure native executor to use OpenRouter
# Edit .workgraph/config.toml manually:
#   [native_executor]
#   provider = "openrouter"
#   api_base = "https://openrouter.ai/api"

# 3. Set model with OpenRouter naming
wg config --model "anthropic/claude-sonnet-4-latest"
```

### For Claude CLI executor (default):

The Claude CLI executor doesn't use OpenRouter — it shells out to the `claude` binary. To use OpenRouter here you'd need to switch to the native executor.

## Recommended Implementation Plan

### Phase 1: Wire endpoints to client creation (enables basic use)
- Make `create_client()` in `native_exec.rs` check `config.llm_endpoints.endpoints` for a matching endpoint by provider
- Use the endpoint's `api_key` and `url` when creating the client
- Also update `call_openai_native()` in `llm.rs` to check endpoints

### Phase 2: CLI endpoint management
- Add `wg config --add-endpoint` / `--remove-endpoint` / `--list-endpoints` CLI commands
- Support `--api-key-file <path>` to read key from a file

### Phase 3: Endpoint-aware role routing
- Add optional `endpoint` field to `RoleModelConfig`
- Update `resolve_model_for_role()` to return endpoint info
- Plumb endpoint through to native executor spawn

### Phase 4: Streaming support
- Add SSE parsing to `OpenAiClient` matching OpenAI's streaming format
- Wire into agent loop for progress reporting
