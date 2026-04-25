# OpenAI-Compatible Endpoints: Research Report

Research for generic OpenAI-compatible endpoint support in workgraph's native executor.
OpenRouter is the first cloud target; local serving (ollama, vLLM, etc.) is also in scope.

---

## 1. OpenRouter API

### Base URL
```
https://openrouter.ai/api/v1/chat/completions
```

The base URL prefix is `https://openrouter.ai/api` — our `OpenAiClient` appends `/v1/chat/completions`, matching since `DEFAULT_BASE_URL` is already `https://openrouter.ai/api`.

### Authentication

**Required:**
```
Authorization: Bearer <OPENROUTER_API_KEY>
```

**Optional (recommended for leaderboard/attribution):**
```
HTTP-Referer: <YOUR_SITE_URL>
X-OpenRouter-Title: <YOUR_APP_NAME>
```

### Model ID Format

`provider/model-name` format:
```
openai/gpt-5.2
anthropic/claude-sonnet-4-latest-20250514
minimax/minimax-m2.5-20260211
deepseek/deepseek-chat-v3
google/gemini-2.5-pro-preview
```

**Variant suffixes:** `:free`, `:extended`, `:thinking`, `:online`, `:nitro`

### Request Format

Standard OpenAI chat completions:

```json
{
  "model": "minimax/minimax-m2.5-20260211",
  "messages": [
    {"role": "system", "content": "You are a helpful assistant."},
    {"role": "user", "content": "Hello"}
  ],
  "max_tokens": 16384,
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
  "stream": false
}
```

### Response Format

Standard OpenAI response (text):
```json
{
  "id": "gen-abc123",
  "choices": [{
    "message": {"role": "assistant", "content": "Hello!", "tool_calls": null},
    "finish_reason": "stop"
  }],
  "usage": {"prompt_tokens": 42, "completion_tokens": 8}
}
```

Tool call response:
```json
{
  "id": "gen-def456",
  "choices": [{
    "message": {
      "role": "assistant",
      "content": null,
      "tool_calls": [{
        "id": "call_789",
        "type": "function",
        "function": {"name": "bash", "arguments": "{\"command\": \"ls -la\"}"}
      }]
    },
    "finish_reason": "tool_calls"
  }],
  "usage": {"prompt_tokens": 100, "completion_tokens": 25}
}
```

### Rate Limits
- Credit-based: more credits → higher limits
- Free models: daily request caps, increased with any credit purchase
- Auto-fallback: provider errors trigger automatic routing to next provider
- Default routing: load balanced across providers, price-weighted

### Provider Preferences (OpenRouter-specific)

Optional `provider` object in request body:
```json
{
  "provider": {
    "order": ["atlascloud"],
    "allow_fallbacks": true,
    "sort": "price",
    "data_collection": "deny"
  }
}
```

Not required — our `OaiRequest` omits this safely.

---

## 2. MiniMax M2.5

| Property | Value |
|---|---|
| **Model ID** | `minimax/minimax-m2.5-20260211` (alias: `minimax/minimax-m2.5`) |
| **Context Window** | 196,608 tokens |
| **Max Completion** | 196,608 tokens |
| **Input Pricing** | $0.295 / M tokens |
| **Output Pricing** | $1.20 / M tokens |
| **Tool Calling** | Yes (`tools`, `tool_choice` in supported_parameters) |
| **Structured Outputs** | Yes |
| **Reasoning** | Mandatory (`<think>` tags in responses) |
| **Modalities** | Text in / Text out |
| **SWE-Bench Verified** | 80.2% |
| **Provider** | AtlasCloud |

**Reasoning quirk:** M2.5 has mandatory reasoning mode. Responses include `<think>...</think>` blocks as normal text content in the OpenAI response format. Downstream consumers parsing `final_text` literally will see thinking content.

---

## 3. Prompt Caching

### How it works on OpenRouter

OpenRouter uses **provider sticky routing**: after a cached request, subsequent requests for the same model route to the same provider endpoint. Conversations are identified by hashing the first system + first non-system message.

### Cost savings by provider

| Provider | Write Cost | Read Cost | Min Tokens | TTL |
|---|---|---|---|---|
| **OpenAI** | Free | 0.25x–0.50x input price | 1,024 | Automatic |
| **Anthropic** | 1x (5min) or 2x (1hr) input price | Reduced multiplier | 1,024–4,096 (model-dependent) | 5min or 1hr |
| **DeepSeek** | 1x input price | Reduced multiplier | Varies | Automatic |
| **Google Gemini 2.5** | Free (implicit) | Reduced multiplier | Model-dependent | Automatic |
| **Grok, Moonshot, Groq** | Free | Reduced multiplier | Varies | Automatic |

### Enabling caching

**Automatic** (OpenAI, DeepSeek, Gemini, Grok, Groq): No configuration needed.

**Manual** (Anthropic): Add `cache_control` to content blocks:
```json
{
  "role": "system",
  "content": [
    {
      "type": "text",
      "text": "System prompt...",
      "cache_control": {"type": "ephemeral"}
    }
  ]
}
```

1-hour TTL variant: `{"type": "ephemeral", "ttl": "1h"}`

Max 4 cache breakpoints per request.

### Tracking cache usage

Response includes `prompt_tokens_details`:
- `cached_tokens`: tokens read from cache
- `cache_write_tokens`: tokens written to cache
- `cache_discount`: cost reduction from caching

### Impact on our architecture

Our `OpenAiClient` currently sends system as a flat `{"role": "system", "content": "..."}` message. To support Anthropic-style caching through OpenRouter, we'd need to support the array-of-content-blocks format for system messages. However, **auto-caching providers (OpenAI, DeepSeek, Gemini) work without any client changes** — the caching is transparent.

For our agent loop (which sends the same system prompt and growing conversation history every turn), automatic caching should provide significant cost reduction without code changes.

---

## 4. Tool/Function Calling Across Providers

### Universal aspects (truly standardized)

These work identically across all OpenAI-compatible endpoints:

| Feature | Format |
|---|---|
| Tool definition | `{"type": "function", "function": {"name", "description", "parameters"}}` |
| Tool calls in response | `message.tool_calls[].{id, type, function.{name, arguments}}` |
| Tool results | `{"role": "tool", "tool_call_id": "...", "content": "..."}` |
| Finish reason | `"tool_calls"` when model wants to use tools |
| Tool choice | `"auto"`, `"none"`, `"required"`, or `{"type": "function", "function": {"name": "..."}}` |

### Provider-specific differences

| Aspect | OpenRouter | Ollama | vLLM | LM Studio | llama.cpp |
|---|---|---|---|---|---|
| **Tool support** | Model-dependent (check `supported_parameters`) | Llama 3.1+, Mistral, Command-R+ | Extensive (20+ model families) | Supported with `tool_choice` | Limited, model-dependent |
| **Parallel tool calls** | Yes (model-dependent) | Not confirmed | Yes | Yes | Limited |
| **Streaming tool calls** | Yes | Planned, not yet | Yes | Yes (argument streaming) | Unknown |
| **`tool_choice: required`** | Yes | Unknown | Yes | Yes | Unknown |

### What's truly universal vs provider-specific

**Universal (safe to rely on):**
- `tools` array in request body
- `tool_calls` array in response
- `role: "tool"` for results
- `finish_reason: "tool_calls"`
- `tool_choice: "auto"` / `"none"`

**Provider-specific (check before using):**
- `tool_choice: "required"` — not all local servers support it
- Parallel tool calls — model and server dependent
- Streaming tool call deltas — format varies between servers
- `strict` mode for tool schemas — OpenAI-specific
- `cache_control` on tool definitions — Anthropic-specific via OpenRouter

### Fallback strategy for models without tool support

Some models (especially local/small models) don't support the `tools` parameter. Options:
1. **Check `supported_parameters`** before sending tools (OpenRouter provides this via model API)
2. **Prompt-based tool use**: embed tool schemas in the system prompt and parse structured output
3. **Graceful degradation**: catch errors from unsupported `tools` parameter and retry without

Our current code always sends `tools` if the agent has tool definitions. For local models without tool support, this will fail. We need at minimum an error-handling path.

---

## 5. Local Model Serving

### Ollama

| Property | Value |
|---|---|
| **Base URL** | `http://localhost:11434/v1` |
| **Auth** | Required but unused — any value works (e.g., `Bearer ollama`) |
| **Endpoints** | `/v1/chat/completions`, `/v1/completions`, `/v1/models`, `/v1/embeddings` |
| **Tool Calling** | Yes — Llama 3.1+, Mistral Nemo, Command-R+, Firefunction v2 |
| **Streaming** | Yes, SSE format |
| **Model Names** | Bare names: `llama3.1`, `mistral`, `codellama`, `qwen2.5-coder` |

**Setup:**
```bash
ollama pull llama3.1
# API available immediately at http://localhost:11434/v1
```

**Config for workgraph:**
```bash
export OPENAI_API_KEY=ollama  # Required but ignored
wg config --model llama3.1 --set-provider local
# Or via config.toml:
# [[llm_endpoints.endpoints]]
# name = "ollama"
# provider = "local"
# url = "http://localhost:11434/v1"
```

**What works:** Chat completions, tool calling (with supported models), streaming.
**What breaks:** Small models may not follow tool schemas reliably. No rate limits but constrained by GPU memory / CPU. Model must be pulled first.

### vLLM

| Property | Value |
|---|---|
| **Base URL** | `http://localhost:8000/v1` (default) |
| **Auth** | Optional, configurable via `--api-key` flag |
| **Endpoints** | Full OpenAI API surface: chat, completions, embeddings, etc. |
| **Tool Calling** | Extensive — 20+ model families including MiniMax, DeepSeek, Qwen, Llama |
| **Streaming** | Yes, including streaming tool call arguments |

**Setup:**
```bash
vllm serve meta-llama/Llama-3.1-70B-Instruct --port 8000
# Optionally: --api-key my-secret-key
# Tool calling: --enable-auto-tool-choice --tool-call-parser llama3_json
```

**Config for workgraph:**
```bash
export OPENAI_API_KEY=my-secret-key  # If configured
wg config --model meta-llama/Llama-3.1-70B-Instruct --set-provider openai
# Set base URL via endpoint config or OPENAI_BASE_URL
```

**What works:** Full OpenAI compatibility, tool calling with proper parser, streaming.
**What breaks:** Requires significant GPU memory. Model loading takes time (minutes for large models). Tool calling requires explicit `--tool-call-parser` flag.

### LM Studio

| Property | Value |
|---|---|
| **Base URL** | `http://localhost:1234/v1` (default) |
| **Auth** | Optional, token-based (`Authorization: Bearer $LM_API_TOKEN`) |
| **Endpoints** | `/v1/chat/completions`, `/v1/completions`, `/v1/models`, `/v1/embeddings` |
| **Tool Calling** | Yes — `tool_choice: auto/none/required`, parallel tool calls, streaming args |
| **Streaming** | Yes, SSE with `stream_options.include_usage` |

**Setup:** GUI-based model management. Just-In-Time model loading — auto-loads when API request arrives.

**Config for workgraph:**
```bash
export OPENAI_API_KEY=lm-studio  # Or real token if configured
wg config --model <model-name> --set-provider local
```

### llama.cpp server

| Property | Value |
|---|---|
| **Base URL** | `http://localhost:8080/v1` (default) |
| **Auth** | Optional, `--api-key` flag |
| **Endpoints** | `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings` |
| **Tool Calling** | Limited — requires grammar-based constrained decoding, model-dependent |
| **Streaming** | Yes |

**Setup:**
```bash
llama-server -m model.gguf --port 8080
```

Most bare-bones option. Tool calling is experimental and model-dependent.

### LocalAI

| Property | Value |
|---|---|
| **Base URL** | `http://localhost:8080/v1` (default) |
| **Auth** | Optional |
| **Tool Calling** | Yes, via grammar-constrained generation |
| **Streaming** | Yes |

Drop-in OpenAI replacement. Supports multiple model backends (llama.cpp, transformers, etc.).

### Common patterns across local servers

| Aspect | Cloud (OpenRouter) | Local (Ollama/vLLM/etc.) |
|---|---|---|
| **Auth** | Real API key required | None or placeholder |
| **Rate limits** | Credit-based | None (resource-constrained instead) |
| **Caching** | Provider-dependent, automatic | None (or vLLM prefix caching) |
| **Model availability** | Always ready | Must pull/load first |
| **Startup time** | Instant | Seconds (Ollama) to minutes (vLLM) |
| **GPU memory** | N/A (cloud) | Critical constraint |
| **Cost** | Per-token pricing | Free (electricity + hardware) |
| **Tool reliability** | Model-dependent but generally good | Highly model-dependent |

---

## 6. Current Native Executor Architecture

### Provider abstraction (`provider.rs`)

```
Provider trait
├── AnthropicClient (client.rs) — Anthropic Messages API
└── OpenAiClient (openai_client.rs) — OpenAI-compatible (OpenRouter, OpenAI, Ollama, etc.)
```

`create_provider_ext()` routes by provider name:
- `"openai"` or `"openrouter"` → `OpenAiClient`
- anything else → `AnthropicClient`

**OpenRouter is already a first-class routing target.** The match at `provider.rs:102` explicitly includes `"openrouter"`.

### Provider name resolution (`provider.rs:52-82`)

```
provider = override > config [native_executor].provider > $WG_LLM_PROVIDER > heuristic
```

Heuristic: model contains `/` → `"openai"`, else → `"anthropic"`. OpenRouter models auto-route correctly.

### API key resolution (`openai_client.rs:494-519`)

Priority: `OPENROUTER_API_KEY` > `OPENAI_API_KEY` > config file

### Endpoint configuration (`config.rs:286-336`)

`EndpointConfig` knows about OpenRouter:
```rust
"openrouter" => "https://openrouter.ai/api/v1"  // Note: /v1 suffix
```

**Bug:** Default URL is `https://openrouter.ai/api/v1`, but `OpenAiClient::DEFAULT_BASE_URL` is `https://openrouter.ai/api`. Client appends `/v1/chat/completions`:
- Client default: `https://openrouter.ai/api` + `/v1/chat/completions` = **correct**
- Endpoint default: `https://openrouter.ai/api/v1` + `/v1/chat/completions` = **double /v1 → 404**

### Model routing (`config.rs:415-700+`)

Per-role model+provider assignments:
```toml
[models.evaluator]
provider = "openrouter"
model = "minimax/minimax-m2.5-20260211"
```

---

## 7. API Compatibility Matrix

### What works as-is with our OpenAI client

| Feature | Cloud (OpenRouter) | Local (Ollama/vLLM) |
|---|---|---|
| Request format | **Works** | **Works** |
| Response parsing | **Works** | **Works** |
| Tool definitions | **Works** | **Works** (if model supports) |
| Tool call responses | **Works** | **Works** (if model supports) |
| Tool results | **Works** | **Works** |
| Finish reasons | **Works** | **Works** |
| Usage data | **Works** | **Partial** (some servers omit) |
| Auth header | **Works** | **Works** (placeholder OK) |
| Retry logic | **Works** | **Works** |

### What needs changes

| Feature | What's needed | Priority |
|---|---|---|
| Endpoint URL bug | Fix `default_url_for_provider("openrouter")` `/api/v1` → `/api` | **High** |
| Provider routing for "local" | Add `"local"` to the `match` arm in `provider.rs:101-102` | **High** |
| Missing usage graceful handling | `OaiUsage` fields may be absent from local servers | **Medium** |
| OpenRouter attribution headers | `HTTP-Referer`, `X-OpenRouter-Title` | **Low** |
| Reasoning content filtering | Strip `<think>...</think>` from M2.5 responses | **Low** |
| Prompt caching (Anthropic via OR) | Array content blocks for system message | **Low** (auto-cache works without) |
| Tool support detection | Check model capabilities before sending tools | **Medium** |

---

## 8. Architecture Recommendation

### Recommendation: Extend OpenAiClient, not a new provider

The OpenAI chat completions API is the universal standard. OpenRouter, Ollama, vLLM, LM Studio, and llama.cpp all implement it. A single `OpenAiClient` with minor provider-aware tweaks is the right approach.

**Concrete changes:**

1. **Fix endpoint URL bug** (`config.rs:331`) — change `"openrouter"` default from `/api/v1` to `/api`

2. **Add `"local"` provider routing** (`provider.rs:102`) — extend the match arm:
   ```rust
   "openai" | "openrouter" | "local" => { /* OpenAiClient */ }
   ```

3. **Handle missing usage data** (`openai_client.rs`) — make all `OaiUsage` fields `#[serde(default)]` (already done) and handle `None` usage in response

4. **Optional: provider-aware headers** — if `base_url` contains `openrouter.ai`, add attribution headers

### What NOT to do

- **Don't create separate provider types** (OpenRouterClient, OllamaClient, etc.) — they all speak the same protocol
- **Don't add streaming** to `OpenAiClient` yet — current non-streaming works fine, streaming is a separate feature
- **Don't add prompt caching support** as a first step — auto-caching works transparently for most providers

### Tradeoffs

| Approach | Pros | Cons |
|---|---|---|
| **Config-only** | Zero code changes | Endpoint URL bug, no "local" routing |
| **Extend OpenAiClient** (recommended) | Small diff, one client for all | Minor complexity in build_headers |
| **New provider per backend** | Clean separation | Massive duplication, protocol is identical |

---

## 9. Files That Need Modification

### Required

| File | Change |
|---|---|
| `src/config.rs:331` | Fix `default_url_for_provider("openrouter")` → `"https://openrouter.ai/api"` |
| `src/executor/native/provider.rs:102` | Add `"local"` to OpenAI-compatible match arm |

### Recommended

| File | Change |
|---|---|
| `src/executor/native/openai_client.rs:458-467` | Add OpenRouter attribution headers in `build_headers()` |
| `src/executor/native/openai_client.rs:124` | Consider making `DEFAULT_BASE_URL` a more neutral default or configurable |

### No changes needed

| File | Reason |
|---|---|
| `src/executor/native/client.rs` | Canonical types are provider-agnostic |
| `src/executor/native/agent.rs` | Tool loop works with any `Provider` impl |
| `src/executor/native/mod.rs` | Module structure is fine |

---

## 10. Risk Areas and Gotchas

### High Priority
- **Endpoint URL double-/v1 bug:** Explicit OpenRouter endpoint config produces 404. Fix before shipping.
- **"local" provider not routed:** `"local"` falls through to `AnthropicClient`, which will fail. Must add to match arm.

### Medium Priority
- **Tool calling on weak models:** Small local models (< 7B) often can't follow tool schemas. Need graceful error handling or tool-support detection.
- **Reasoning content pollution:** M2.5's `<think>` tags appear in `final_text`. Could confuse downstream task processing.
- **Rate limits:** OpenRouter's credit-based limits differ from Anthropic. Heavy concurrent agent usage may hit limits. Retry logic handles 429s but backoff may need tuning.
- **Missing usage data:** Some local servers don't return `usage` in responses. Already handled by `#[serde(default)]` but `Usage::default()` fallback means zero counts, which could affect cost tracking.

### Low Priority
- **Model availability:** Local models must be pulled/loaded before use. First request may timeout during model loading.
- **`retry_after` parsing:** `parse_retry_after_oai()` looks at `error.metadata.retry_after`. OpenRouter may use a different path.
- **vLLM tool parser:** vLLM requires explicit `--tool-call-parser` flag. Without it, tool calls silently don't work.
- **Ollama auth:** Requires an API key header value, but ignores it. Our key resolution will fail if no env var is set. May need a "no auth required" mode.
