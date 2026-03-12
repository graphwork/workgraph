# OpenRouter Provider Integration: Design Document

Based on [openrouter-research.md](openrouter-research.md). Covers provider architecture,
config changes, file-by-file change list, wire format, and test strategy.

---

## 1. Provider Architecture

### Decision: Extend `OpenAiClient`, no new provider type

The research confirms OpenRouter speaks the standard OpenAI chat completions protocol.
Tool calling, message format, streaming—all are identical. A separate `OpenRouterClient`
would be pure duplication.

Three viewpoints converge on the same answer:

| Viewpoint | Conclusion |
|---|---|
| **Minimalist** | Fix the URL bug, add `"local"` routing, add attribution headers. ~30 lines changed. |
| **Extensible** | Keep one `OpenAiClient` for all OpenAI-compatible backends. Future providers (Together, Fireworks, Groq) slot in with zero code changes—just config. |
| **User** | `wg config --set-provider default openrouter` + `OPENROUTER_API_KEY` env var. Done. |

### Architecture changes

```
Provider trait (unchanged)
├── AnthropicClient (unchanged)
└── OpenAiClient
    ├── provider_hint: Option<String>     ← NEW: "openrouter", "openai", "local", etc.
    ├── build_headers() → adds OpenRouter attribution headers when hint = "openrouter"
    └── from_env() → unchanged (already checks OPENROUTER_API_KEY first)
```

The `provider_hint` field lets `OpenAiClient` vary behavior by provider without
subclassing. Currently the only use is adding `HTTP-Referer` and `X-Title` headers
for OpenRouter. Future uses: skipping auth for local servers, adjusting retry behavior.

### Provider routing changes (`provider.rs`)

Current match arm:
```rust
"openai" | "openrouter" => { /* OpenAiClient */ }
```

New match arm:
```rust
"openai" | "openrouter" | "local" => { /* OpenAiClient */ }
```

The `provider_name` string is passed through to `OpenAiClient` as `provider_hint`
so it can add provider-specific headers.

### What stays unchanged

- `Provider` trait — no new methods needed
- `AnthropicClient` — untouched
- Agent loop (`agent.rs`) — uses `Provider` trait, provider-agnostic
- Canonical types (`client.rs`) — `Message`, `ContentBlock`, `ToolDefinition` are wire-format-agnostic
- Tool registry, tool execution — completely orthogonal

---

## 2. Config Changes

### Environment variables

| Variable | Purpose | Priority |
|---|---|---|
| `OPENROUTER_API_KEY` | OpenRouter API key | Checked first (existing behavior) |
| `OPENAI_API_KEY` | Fallback for any OpenAI-compatible endpoint | Checked second (existing) |
| `OPENAI_BASE_URL` | Override base URL | Existing |
| `OPENROUTER_BASE_URL` | Override base URL (OpenRouter-specific) | Existing |

No new env vars needed. The existing resolution chain already handles OpenRouter.

### Config file changes

**Endpoint configuration** (`config.toml`):
```toml
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
# url is optional — uses corrected default https://openrouter.ai/api
# api_key stored here or in OPENROUTER_API_KEY env var
```

**Model routing** (existing, no changes needed):
```toml
[models.default]
provider = "openrouter"
model = "minimax/minimax-m2.5"

[models.evaluator]
provider = "openrouter"
model = "openai/gpt-4o-mini"
```

### URL defaults (bug fix)

`EndpointConfig::default_url_for_provider`:

| Provider | Current | Fixed |
|---|---|---|
| `"openrouter"` | `https://openrouter.ai/api/v1` | `https://openrouter.ai/api` |
| `"openai"` | `https://api.openai.com/v1` | unchanged |
| `"local"` | `http://localhost:11434/v1` | unchanged |

The bug: `OpenAiClient` appends `/v1/chat/completions` to its `base_url`.
When `default_url_for_provider("openrouter")` returns `/api/v1`, the final URL
becomes `https://openrouter.ai/api/v1/v1/chat/completions` — a 404.

The fix is to return `https://openrouter.ai/api` (no `/v1` suffix), matching
`OpenAiClient::DEFAULT_BASE_URL`.

Note: `"openai"` default is `https://api.openai.com/v1` which also double-stacks
to `/v1/v1/chat/completions`. This is also a bug but is out of scope for this task.
We document it and recommend fixing alongside the OpenRouter fix. The clean fix
for both: change `OpenAiClient` to expect base URLs **with** `/v1` and only append
`/chat/completions`. This normalizes all providers:

| Provider | Base URL (with /v1) | Final URL |
|---|---|---|
| openrouter | `https://openrouter.ai/api/v1` | `.../v1/chat/completions` |
| openai | `https://api.openai.com/v1` | `.../v1/chat/completions` |
| local (ollama) | `http://localhost:11434/v1` | `.../v1/chat/completions` |

**Recommended approach**: Change `chat_completion()` from:
```rust
let url = format!("{}/v1/chat/completions", self.base_url);
```
to:
```rust
let url = format!("{}/chat/completions", self.base_url);
```
And update `DEFAULT_BASE_URL` from `https://openrouter.ai/api` to
`https://openrouter.ai/api/v1`. This makes all default URLs in
`EndpointConfig::default_url_for_provider` correct as-is, and aligns with the
OpenAI SDK convention where base_url includes `/v1`.

### User workflow

```bash
# 1. Set API key
export OPENROUTER_API_KEY=sk-or-v1-...

# 2. Configure provider + model
wg config --set-provider default openrouter
wg config --set-model default minimax/minimax-m2.5

# 3. Use as normal — agents will use OpenRouter
wg service start
```

---

## 3. File-by-File Change List

### `src/executor/native/openai_client.rs`

| Change | Lines | Description |
|---|---|---|
| Add `provider_hint` field | ~131-136 | `pub provider_hint: Option<String>` on `OpenAiClient` struct |
| Update constructors | ~140-168 | `new()` and `from_env()` accept/default `provider_hint` |
| Add `with_provider_hint()` builder | new | `pub fn with_provider_hint(mut self, hint: &str) -> Self` |
| Fix URL construction | ~388 | Change `"{}/v1/chat/completions"` to `"{}/chat/completions"` |
| Update `DEFAULT_BASE_URL` | ~124 | Change to `"https://openrouter.ai/api/v1"` |
| Add OpenRouter headers | ~458-467 | In `build_headers()`, when `provider_hint == Some("openrouter")`, add `HTTP-Referer: https://github.com/anthropics/workgraph` and `X-Title: workgraph` |
| Update `name()` | ~472-474 | Return `provider_hint` if set, else `"openai"` |
| Update tests | ~584+ | Update `DEFAULT_BASE_URL` references in tests if any |

### `src/executor/native/provider.rs`

| Change | Lines | Description |
|---|---|---|
| Extend match arm | ~102 | `"openai" \| "openrouter" \| "local" =>` |
| Pass provider hint | ~103-122 | After building `OpenAiClient`, call `.with_provider_hint(&provider_name)` |
| Handle local auth | ~103-110 | For `"local"` provider, if no API key found, use placeholder `"local"` instead of erroring |

### `src/config.rs`

| Change | Lines | Description |
|---|---|---|
| Fix OpenRouter default URL | ~331 | Change `"https://openrouter.ai/api/v1"` to `"https://openrouter.ai/api/v1"` — **no change needed** if we adopt the `/v1`-in-base-url convention (see section 2). If we keep the current convention, change to `"https://openrouter.ai/api"`. |

### `src/executor/native/client.rs`

No changes. Canonical types are provider-agnostic.

### `src/executor/native/agent.rs`

No changes. Uses `Provider` trait, unaware of backend.

### `src/executor/native/mod.rs`

No changes. Module re-exports are fine.

### Summary

| File | Type | Lines changed (est.) |
|---|---|---|
| `openai_client.rs` | Modify | ~25 |
| `provider.rs` | Modify | ~10 |
| `config.rs` | Modify | ~1 (URL fix, only if not changing convention) |
| **Total** | | **~36 lines** |

---

## 4. Wire Format Differences

### Request format

**Identical.** OpenRouter accepts standard OpenAI chat completions requests. Our
`OaiRequest` struct works as-is. No changes to request serialization.

One optional OpenRouter extension we do NOT need:
```json
{"provider": {"order": ["atlascloud"], "allow_fallbacks": true}}
```
This can be added later if users want provider routing preferences. Our `OaiRequest`
correctly omits it via `skip_serializing_if`.

### Response format

**Identical.** OpenRouter returns standard OpenAI responses. Our `OaiResponse` parsing
works unchanged.

Minor differences that our current code already handles:
- `usage` may be `null` → handled by `#[serde(default)]` on `Option<OaiUsage>`
- `finish_reason` may be `null` → handled by `Option<String>`
- `tool_calls` may be absent → handled by `#[serde(default)]`

### OpenRouter-specific response fields (informational)

OpenRouter includes extra fields we can safely ignore:
- `id` prefix is `gen-` instead of `chatcmpl-`
- `model` field in response shows actual model used (we don't read this)
- `system_fingerprint` may be present (we don't read this)
- `usage.prompt_tokens_details.cached_tokens` for cache tracking (future enhancement)

### Headers

| Header | Direction | Required | Purpose |
|---|---|---|---|
| `Authorization: Bearer <key>` | Request | Yes | Authentication |
| `Content-Type: application/json` | Request | Yes | Already sent |
| `HTTP-Referer` | Request | No | Attribution/ranking |
| `X-Title` | Request | No | Attribution/ranking |

### Streaming (future — separate task)

When streaming is added, the SSE format is identical to OpenAI:
```
data: {"choices":[{"delta":{"content":"Hello"}}]}
data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"..."}}]}}]}
data: [DONE]
```

No OpenRouter-specific streaming format differences. The `implement-openrouter-streaming`
downstream task can build on standard OpenAI SSE parsing.

### MiniMax M2.5 reasoning content

M2.5 responses include `<think>...</think>` blocks in the `content` field. This is
**not** a wire format difference — it's model behavior within the standard format.

Filtering these tags is optional and should be a separate task if needed. The tags
don't break anything; they just add visible reasoning to the agent's text output.

---

## 5. Test Strategy

### Unit tests (no API calls, no credits)

**1. URL construction test**
```rust
#[test]
fn test_openrouter_url_construction() {
    let client = OpenAiClient::new("test-key".into(), "minimax/minimax-m2.5", None).unwrap();
    // Verify the URL built by chat_completion is correct
    assert!(client.base_url.ends_with("/v1"));
    // Final URL should be {base_url}/chat/completions
}
```

**2. Header tests**
```rust
#[test]
fn test_openrouter_headers_included() {
    let client = OpenAiClient::new("test-key".into(), "test/model", None)
        .unwrap()
        .with_provider_hint("openrouter");
    let headers = client.build_headers();
    assert!(headers.contains_key("http-referer"));
    assert!(headers.contains_key("x-title"));
}

#[test]
fn test_non_openrouter_no_extra_headers() {
    let client = OpenAiClient::new("test-key".into(), "gpt-4o", None)
        .unwrap()
        .with_provider_hint("openai");
    let headers = client.build_headers();
    assert!(!headers.contains_key("http-referer"));
}
```

**3. Provider routing test**
```rust
#[test]
fn test_provider_name_resolution() {
    // "openrouter" routes to OpenAiClient
    // "local" routes to OpenAiClient
    // "anthropic" routes to AnthropicClient
    // Model with "/" defaults to "openai"
}
```

**4. Response parsing tests (existing, verify they still pass)**

The existing tests in `openai_client.rs` (lines 584-754) cover:
- `test_translate_tools`
- `test_translate_messages_with_system`
- `test_translate_messages_with_tool_results`
- `test_translate_messages_with_assistant_tool_calls`
- `test_translate_response_text_only`
- `test_translate_response_with_tool_calls`
- `test_translate_response_max_tokens`

These all test serialization/deserialization with no API calls. They validate that
the wire format translation works correctly for any OpenAI-compatible backend.

**5. Local provider fallback test**
```rust
#[test]
fn test_local_provider_no_auth_required() {
    // When provider_hint is "local", should not require a real API key
    // Uses placeholder "local" if no key in env
}
```

### Integration test approach (optional, manual)

For actual API verification without burning significant credits:

```bash
# Cheapest possible integration test: single non-streaming call
export OPENROUTER_API_KEY=sk-or-v1-...
wg config --set-provider default openrouter
wg config --set-model default minimax/minimax-m2.5

# Manual: run a trivial task and verify it completes
wg add "Say hello" -d "Reply with 'hello world' and nothing else"
wg service start
# Cost: ~$0.001 (a few hundred tokens)
```

For local model testing:
```bash
# Free — uses local GPU
ollama pull llama3.1
wg config --set-provider default local
wg config --set-model default llama3.1
# Same test as above
```

### Mock-based test (recommended for CI)

Use `mockito` or `wiremock` to stand up a fake OpenAI-compatible endpoint:

```rust
#[tokio::test]
async fn test_openrouter_end_to_end() {
    let mock_server = wiremock::MockServer::start().await;
    // Register mock for /v1/chat/completions
    // Send a request through OpenAiClient with the mock URL
    // Verify headers, request body, and response parsing
}
```

This gives full integration coverage with zero API cost and runs in CI.

### Test matrix

| Test type | Cost | Coverage | Runs in CI |
|---|---|---|---|
| Unit (serde/translation) | Free | Wire format | Yes |
| Unit (headers/routing) | Free | Provider config | Yes |
| Mock server | Free | Full round-trip | Yes |
| Manual integration | ~$0.001 | Real API | No (manual) |

---

## Appendix: Implementation Order for Downstream Tasks

The three downstream implementation tasks should be completed in this order:

1. **implement-openrouter-config** — URL fix, `"local"` routing, `provider_hint` field.
   This unblocks everything else. Files: `config.rs`, `provider.rs`, `openai_client.rs` (struct + constructors only).

2. **implement-openrouter-provider** — Attribution headers, `name()` update, local
   auth fallback. Files: `openai_client.rs` (build_headers, name), `provider.rs` (local auth).

3. **implement-openrouter-streaming** — SSE streaming for `OpenAiClient`. Files:
   `openai_client.rs` (new streaming methods), `agent.rs` (wire up streaming path).

Tasks 1 and 2 modify overlapping files (`openai_client.rs`, `provider.rs`), so they
should be serialized. Task 3 is larger but touches different code paths.
