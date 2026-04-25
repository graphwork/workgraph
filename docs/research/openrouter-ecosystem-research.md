# OpenRouter Ecosystem Research: Model Discovery, Caching, Tool Use, Keys, Provider Features

**Task:** research-openrouter-ecosystem
**Date:** 2026-03-08
**Branch:** safety-mandatory-validation

---

## 1. Model Discovery

### API Endpoint

```
GET https://openrouter.ai/api/v1/models
Authorization: Bearer <OPENROUTER_API_KEY>
```

### Query Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `category` | string | Filter by use case: `programming`, `roleplay`, `marketing`, `marketing/seo`, `technology`, `science`, `translation`, `legal`, `finance`, `health`, `trivia`, `academia` |
| `supported_parameters` | string | Filter by capability (e.g., `tools` for tool-calling models) |

### Response Schema

Returns `{ "data": [Model, ...] }` where each `Model` has:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Unique identifier (e.g., `anthropic/claude-sonnet-4-latest`) |
| `canonical_slug` | string | URL-safe slug |
| `name` | string | Display name |
| `created` | number | Unix timestamp |
| `description` | string | Model summary |
| `pricing` | object | `{ prompt, completion, request, image, audio, web_search }` — per-token costs as strings |
| `context_length` | number? | Maximum tokens (nullable) |
| `architecture` | object | `{ tokenizer, instruction_type, modality }` — input/output modalities |
| `top_provider` | object | `{ context_length, max_completion_tokens, is_moderated }` |
| `per_request_limits` | object | `{ prompt_tokens, completion_tokens }` |
| `supported_parameters` | string[] | Capabilities: `temperature`, `top_p`, `tools`, `web_search`, etc. |
| `default_parameters` | object | Provider defaults for temperature, top_p, etc. |
| `expiration_date` | string? | ISO 8601 date or null |
| `hugging_face_id` | string? | Optional HuggingFace identifier |

### Querying by Capability

To find all models with tool use support:
```
GET https://openrouter.ai/api/v1/models?supported_parameters=tools
```

The `supported_parameters` array on each model is the definitive way to check capabilities before sending requests. Check for `"tools"` in this array before including tool definitions.

### Current Implementation Status

**What exists:**
- `src/models.rs`: Local `ModelRegistry` with 13 hardcoded models in `models.yaml`
- `src/commands/models.rs`: `wg models list/add/set-default/init` commands
- Models default to `provider: "openrouter"` when added

**Gap — No live model discovery:**
- The registry is entirely static. There is no `wg models search` or `wg models sync` that queries OpenRouter's `/api/v1/models` endpoint
- Model pricing in the registry can become stale
- No way to discover new models without manual `wg models add`
- No capability filtering (e.g., "show me all models that support tool use")

### Implementation Implications

A `wg models search` command should:
1. Call `GET /api/v1/models` (optionally with `?supported_parameters=tools&category=programming`)
2. Display results in a table matching the existing `wg models list` format
3. Optionally allow adding results to the local registry (`wg models search --add <model-id>`)

A `wg models sync` command could refresh pricing/capabilities for all models already in the local registry by cross-referencing the API response.

---

## 2. Caching

### How Caching Works on OpenRouter

OpenRouter supports prompt caching across multiple providers. The key mechanism is **provider sticky routing**: after a cached request, subsequent requests route to the same provider endpoint to maximize cache hits. This is automatic and operates at the account + model + conversation level.

### Provider-Specific Caching Details

| Provider | Write Cost | Read Cost | Min Tokens | TTL | Config Required |
|----------|-----------|-----------|------------|-----|-----------------|
| **OpenAI** | Free | 0.25x–0.50x input price | 1,024 | Automatic | None (automatic) |
| **Anthropic** | 1.25x (5min) or 2x (1hr) | Reduced multiplier | 1,024–4,096 (model-dependent) | 5min or 1hr | `cache_control` blocks |
| **DeepSeek** | 1x input price | Reduced multiplier | Varies | Automatic | None (automatic) |
| **Google Gemini 2.5** | Input + 5min storage | Reduced multiplier | Model-dependent | ~3-5 min | `cache_control` breakpoints |
| **Grok, Moonshot, Groq** | Free | Reduced multiplier | Varies | Automatic | None (automatic) |

### Enabling Caching

**Automatic (most providers):** No configuration needed. OpenAI, DeepSeek, Grok, Groq cache transparently.

**Manual (Anthropic):** Two approaches:
1. **Top-level auto-caching**: Add `cache_control` at request level — OpenRouter auto-applies to last cacheable block:
   ```json
   {
     "model": "anthropic/claude-sonnet-4-latest",
     "cache_control": {"type": "ephemeral"},
     "messages": [...]
   }
   ```
2. **Explicit breakpoints**: Place `cache_control` on individual content blocks (max 4 per request):
   ```json
   {"type": "text", "text": "...", "cache_control": {"type": "ephemeral"}}
   ```
   1-hour TTL variant: `{"type": "ephemeral", "ttl": "1h"}`

**Google Gemini:** Uses same `cache_control` breakpoint format as Anthropic. Only last breakpoint is used.

### Cache Usage Tracking (Response Fields)

OpenRouter returns cache metrics in `usage.prompt_tokens_details`:

| Field | Type | Description |
|-------|------|-------------|
| `cached_tokens` | number | Tokens read from cache (cache hit) |
| `cache_write_tokens` | number | Tokens written to cache (cache miss, initial request) |
| `cache_discount` | number | Cost reduction from caching |

### Current Implementation Status

**What exists:**
- `OaiUsage` struct (`openai_client.rs:102-107`) only has `prompt_tokens` and `completion_tokens`
- The `Usage` canonical type (`client.rs`) has `cache_read_input_tokens` and `cache_creation_input_tokens` fields
- `translate_response()` always sets cache fields to `None` for OpenAI client responses
- The Anthropic client properly tracks cache metrics

**Gap — Cache tracking not wired for OpenRouter:**
- `OaiUsage` does **not** parse `prompt_tokens_details` from responses
- Cache hit/miss data from OpenRouter is silently discarded
- No way to surface cache savings in `wg cost` or token usage summaries
- Auto-caching providers (OpenAI, DeepSeek, Gemini) would benefit from tracking with zero code changes on the request side — only response parsing needs updating

**Gap — No Anthropic-style cache_control in OpenAI client requests:**
- System messages are sent as flat `{"role": "system", "content": "..."}` strings
- To leverage Anthropic caching through OpenRouter, need array-of-content-blocks format with `cache_control`
- Not needed for auto-caching providers, but significant cost reduction for Anthropic models

### Implementation Implications

**Minimal change (high value):** Add `prompt_tokens_details` to `OaiUsage`:
```rust
struct OaiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<OaiPromptTokenDetails>,
}

struct OaiPromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_write_tokens: Option<u32>,
    #[serde(default)]
    cache_discount: Option<f64>,
}
```

Then map these to the existing `Usage.cache_read_input_tokens` / `cache_creation_input_tokens` in `translate_response()`.

**Future enhancement:** Add `cache_control` support to system message serialization for Anthropic-via-OpenRouter caching.

---

## 3. Tool Use

### Which Models Support Tool Use

Check via the `/api/v1/models` endpoint — models with `"tools"` in their `supported_parameters` array support function calling. Key models with tool support include:

- All Anthropic Claude models (Opus, Sonnet, Haiku 3.5+)
- OpenAI GPT-4o, GPT-4o-mini, o3
- Google Gemini 2.5 Pro/Flash
- DeepSeek Chat v3, R1
- Meta Llama 4 Maverick/Scout
- Qwen 3 235B
- MiniMax M2.5

OpenRouter standardizes the tool calling interface across all providers.

### Request Format (Standardized)

```json
{
  "model": "anthropic/claude-sonnet-4-latest",
  "messages": [...],
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "bash",
        "description": "Execute a shell command",
        "parameters": {
          "type": "object",
          "properties": {"command": {"type": "string"}},
          "required": ["command"]
        }
      }
    }
  ],
  "tool_choice": "auto"
}
```

**Important:** The `tools` parameter must be included in every request in a conversation — not just the first one. OpenRouter validates tool schemas on each call.

### Tool Choice Options

| Value | Behavior |
|-------|----------|
| `"auto"` | Model decides whether to use tools (default) |
| `"none"` | Disables tool usage |
| `"required"` | Forces the model to call at least one tool |
| `{"type": "function", "function": {"name": "..."}}` | Forces a specific tool |

### Response Format (Standardized)

```json
{
  "choices": [{
    "message": {
      "role": "assistant",
      "content": null,
      "tool_calls": [{
        "id": "call_789",
        "type": "function",
        "function": {"name": "bash", "arguments": "{\"command\": \"ls\"}"}
      }]
    },
    "finish_reason": "tool_calls"
  }]
}
```

Tool results use `role: "tool"` with matching `tool_call_id`.

### Advanced Features

| Feature | Support | Notes |
|---------|---------|-------|
| Parallel tool calls | Yes (model-dependent) | `parallel_tool_calls` param (default: true), set to false for sequential |
| Streaming tool calls | Yes | Handle via `delta.tool_calls`, monitor `finish_reason` for completion |
| Interleaved thinking | Select models | Reasoning between tool calls; increases token usage and latency |

### Current Implementation Status

**What works well:**
- `OpenAiClient` properly translates between Anthropic-style `ToolUse`/`ToolResult` content blocks and OpenAI-format `tool_calls`/`tool` messages
- `translate_tools()` correctly builds function definitions
- `translate_messages()` handles tool calls in assistant messages and tool results as `role: "tool"` messages
- `translate_response()` parses `tool_calls` from responses
- Streaming tool call accumulation works (`assemble_oai_stream_response()`)

**Gap — No tool support detection:**
- The agent always sends `tools` in requests regardless of whether the model supports them
- For models without tool support, this will cause errors
- Should check `supported_parameters` before sending tools, or catch errors and retry without tools

**Gap — No `parallel_tool_calls` control:**
- No way to disable parallel tool calls for models that handle them poorly
- Could be a per-model config option

### Provider-Specific Differences (Handled Transparently by OpenRouter)

The wire format is identical across all providers when accessed through OpenRouter. Differences in how providers natively implement tool calling (Anthropic-style vs OpenAI-style) are abstracted by OpenRouter's unified interface. Our single `OpenAiClient` is the correct architecture.

---

## 4. Key Management

### API Key Types

| Key Type | Purpose | Can Make Completions | Can Manage Keys |
|----------|---------|---------------------|-----------------|
| **Standard API Key** | LLM inference calls | Yes | No |
| **Management API Key** | Programmatic key CRUD | No | Yes |

### Key Management API

```
POST   /api/v1/keys          — Create a new key
GET    /api/v1/keys           — List keys (with pagination via offset)
GET    /api/v1/keys/{hash}    — Get key details
PATCH  /api/v1/keys/{hash}    — Update key (name, disabled, limit_reset)
DELETE /api/v1/keys/{hash}    — Delete key
```

### Key Properties

| Field | Description |
|-------|-------------|
| `name` | Descriptive label |
| `disabled` | Boolean on/off |
| `credit_limit` | Maximum spend (null = unlimited) |
| `limit_remaining` | Credits left under limit |
| `usage` | All-time credit consumption |
| `usage_daily/weekly/monthly` | Time-period breakdowns |
| `limit_reset` | Reset schedule: daily/weekly/monthly at midnight UTC |
| `created_at`, `updated_at` | Timestamps |

### Checking Key Status

```
GET https://openrouter.ai/api/v1/key
Authorization: Bearer <your-key>
```

Returns: `limit`, `limit_remaining`, `usage`, `usage_daily/weekly/monthly`, `is_free_tier`.

### Rate Limits

| Tier | Rate Limits |
|------|------------|
| **Free tier** | 20 req/min, 50 req/day (1000/day with 10+ credits purchased) |
| **Pay-as-you-go** | No platform-level rate limits |
| **Enterprise** | No platform-level rate limits |

Rate limits are **account-level**, not per-key. Creating additional keys or accounts does not increase limits.

DDoS protection via Cloudflare blocks requests that dramatically exceed reasonable usage.

### Key Rotation

Zero-downtime rotation process:
1. Create new key via Management API
2. Deploy new key across infrastructure
3. Verify new key works in production
4. Delete old key via Management API

### BYOK (Bring Your Own Key)

Supported providers: Azure AI, Amazon Bedrock, Google Vertex AI. BYOK endpoints are prioritized first in routing. Fallback to OpenRouter credits if rate-limited (configurable). 1M free BYOK requests per month.

### Best Practices for Workgraph

- **Single key per deployment** is sufficient for pay-as-you-go (no per-key rate limits)
- Use **credit limits** on keys to prevent runaway costs from agent loops
- Use **Management API** for automated key rotation in CI/CD
- Set `limit_reset: "daily"` for daily budget caps
- Monitor via `GET /api/v1/key` to check remaining credits

### Current Implementation Status

**What exists:**
- Key resolution: `OPENROUTER_API_KEY` > `OPENAI_API_KEY` > config file (`openai_client.rs:699-727`)
- Endpoint-aware key loading via `config.llm_endpoints.find_for_provider()` in `provider.rs` and `llm.rs`
- Clear error messages when no key is found

**Gap — No key status checking:**
- No `wg config --check-key` to verify key validity and show remaining credits
- No pre-flight validation before starting agent runs

**Gap — No credit tracking:**
- `GET /api/v1/key` response data (usage, remaining credits) is never fetched
- Could be integrated into `wg cost` or `wg service status`

---

## 5. Provider-Specific Features

### App Attribution Headers

OpenRouter tracks usage by application for leaderboard rankings and analytics:

| Header | Value | Purpose |
|--------|-------|---------|
| `HTTP-Referer` | Your app URL | Attribution |
| `X-Title` (or `X-OpenRouter-Title`) | Your app name | Display name in rankings |

**Current status:** Implemented. `build_headers()` in `openai_client.rs` adds these when `provider_hint == "openrouter"`.

### Provider Routing Preferences

Optional `provider` object in request body:

```json
{
  "provider": {
    "order": ["anthropic", "openai"],
    "allow_fallbacks": true,
    "sort": "price",
    "only": ["anthropic"],
    "ignore": ["databutton"],
    "require_parameters": true,
    "data_collection": "deny",
    "zdr": true,
    "quantizations": ["fp8", "fp16"],
    "preferred_min_throughput": 100,
    "preferred_max_latency": 5,
    "max_price": {"prompt": "0.01", "completion": "0.05"}
  }
}
```

| Field | Description |
|-------|-------------|
| `order` | Provider sequence preference |
| `allow_fallbacks` | Enable/disable fallback routing (default: true) |
| `sort` | Optimize by `"price"`, `"throughput"`, or `"latency"` |
| `only` | Restrict to specific providers |
| `ignore` | Exclude specific providers |
| `require_parameters` | Only use providers supporting all request parameters |
| `data_collection` | `"allow"` or `"deny"` data retention |
| `zdr` | Zero Data Retention enforcement |
| `quantizations` | Filter by precision level |
| `preferred_min_throughput` | Minimum tokens/sec threshold |
| `preferred_max_latency` | Maximum latency in seconds |
| `max_price` | Maximum acceptable per-token pricing |

### Model Variant Suffixes

| Suffix | Effect |
|--------|--------|
| `:free` | Free tier variant (lower rate limits) |
| `:extended` | Extended context variant |
| `:thinking` | Reasoning/thinking variant |
| `:online` | Web-connected variant |
| `:nitro` | Equivalent to `sort: "throughput"` |
| `:floor` | Equivalent to `sort: "price"` |

### Default Load Balancing

Without provider preferences, OpenRouter uses:
1. **Uptime filtering**: Excludes providers with outages in last 30 seconds
2. **Cost weighting**: Inverse-square pricing (cheaper providers exponentially preferred)
3. **Fallback chain**: Remaining providers as backup

### Current Implementation Status

**What exists:**
- Attribution headers (`HTTP-Referer`, `X-Title`) — implemented
- `OaiRequest` correctly omits the `provider` field via `skip_serializing_if`
- Default routing works without any provider preferences

**Gap — No provider routing preferences:**
- No way to specify `provider.sort`, `provider.only`, `provider.data_collection`, etc.
- Could be useful for: forcing low-latency routing for triage, enforcing ZDR for sensitive tasks, setting max price budgets
- Implementation: Add optional `OaiProviderPrefs` struct to `OaiRequest`

**Gap — No model variant suffix support:**
- Users can manually specify `:nitro` or `:floor` suffixes in model names, but there's no first-class UI for it
- Could add `wg config --set-model evaluator anthropic/claude-haiku-4-latest:nitro`

---

## Summary of Implementation Gaps

### High Priority (Directly Blocking Downstream Tasks)

| Gap | Impact | Downstream Task |
|-----|--------|-----------------|
| Cache tracking in `OaiUsage` | Lost cost savings visibility | `implement-openrouter-caching` |
| Model discovery via API | Static registry, stale pricing | `implement-openrouter-model` |

### Medium Priority

| Gap | Impact |
|-----|--------|
| Tool support detection | Errors when sending tools to unsupported models |
| Key status checking (`GET /api/v1/key`) | No pre-flight validation or credit monitoring |
| Provider routing preferences | No control over cost/latency/data-retention tradeoffs |

### Low Priority

| Gap | Impact |
|-----|--------|
| Anthropic-style `cache_control` in requests | Manual caching for Anthropic-via-OpenRouter (auto-caching works without) |
| `parallel_tool_calls` control | Edge cases with models that handle parallel calls poorly |
| Model variant suffix UI | Users can type suffixes manually |

---

## Files Referenced

| File | What It Contains |
|------|------------------|
| `src/executor/native/openai_client.rs` | OpenAI-compatible HTTP client, `OaiUsage`, `OaiRequest`, tool translation |
| `src/executor/native/provider.rs` | Provider routing (`create_provider_ext()`), "openai"/"openrouter"/"local" match |
| `src/service/llm.rs` | Lightweight LLM dispatch for internal roles |
| `src/models.rs` | Model registry, `ModelEntry`, `ModelRegistry` |
| `src/commands/models.rs` | `wg models list/add/set-default/init` commands |
| `src/config.rs` | `EndpointConfig`, `resolve_model_for_role()`, URL defaults |
| `src/graph.rs` | `TokenUsage` struct with cache fields |
