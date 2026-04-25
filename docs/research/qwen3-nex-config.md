# Qwen3 Endpoint Configuration for wg nex

**Date**: 2026-04-21
**Task**: research-qwen3-nex-config

## Summary

Two qwen3 endpoints are reachable. The SGLang endpoint on lambda01 works end-to-end
with `wg nex` via the named endpoint config. Local Ollama is reachable via curl but
blocked from `wg nex` by two bugs. Two bugs were discovered: the `-e <url>` flag
strips `/v1` causing 404s, and the default endpoint overrides provider-specific URLs.

## Reachable Endpoints

### 1. SGLang on lambda01 (via Tailscale) — WORKING

| Property | Value |
|----------|-------|
| URL | `https://lambda01.tail334fe6.ts.net:30000/v1` |
| Model | `qwen3-coder-30b` |
| Context window | 32768 |
| Auth | `api_key = "none"` (no auth) |
| Status | **Reachable, responsive (~2s latency)** |

**Note**: Plain `lambda01:30000` does NOT resolve. Must use Tailscale FQDN.

**Verified**: Non-streaming curl, streaming curl, and `wg nex` session all succeed.
Session produced correct output with `result.success=true` in ~2 seconds.

### 2. Ollama (local) — REACHABLE but blocked from nex

| Property | Value |
|----------|-------|
| URL | `http://localhost:11434/v1` |
| Models | `qwen3:32b` (Q4_K_M, ~20GB), `qwen3:4b` (Q4_K_M, ~2.6GB) |
| Auth | None needed |
| Status | **Reachable via curl, blocked from nex by bugs** |

Both streaming and non-streaming curl requests work perfectly:
```bash
curl http://localhost:11434/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3:32b","messages":[{"role":"user","content":"hi"}],"max_tokens":50}'
```

### 3. Ollama alternate port (11435) — NOT RUNNING

Configured in `.workgraph.1/config.toml` as `local-model` endpoint.
Start with: `bash terminal-bench/start-local-model.sh`
Would serve `qwen3-coder:30b-a3b-q8_0` (same architecture as SGLang model).

## Exact nex Invocations

### Working: SGLang via named endpoint
```bash
wg nex -m qwen3-coder-30b -e lambda01
```
This is the recommended invocation. Uses the `lambda01` named endpoint
from config, which sets the correct URL, auth, and context window.

### Working: SGLang via default endpoint (when env is clean)
```bash
env -u WG_LLM_PROVIDER wg nex -m qwen3-coder-30b
```
The `lambda01` endpoint is configured as `is_default = true`, so bare model
names resolve there. Must unset `WG_LLM_PROVIDER=anthropic` which is set
in the agent environment and forces the Anthropic client.

### Broken: Local Ollama via `-e` URL flag
```bash
wg nex -m qwen3:4b -e http://localhost:11434      # 404
wg nex -m qwen3:4b -e http://localhost:11434/v1    # 404 (same bug)
```
**Bug**: `build_inline_url_client()` in `src/executor/native/provider.rs:71`
strips `/v1`, but `OpenAiClient` only appends `/chat/completions` (not
`/v1/chat/completions`). Result: `http://localhost:11434/chat/completions` → 404.

### Broken: Local Ollama via `ollama:` prefix
```bash
wg nex -m ollama:qwen3:4b     # Hits lambda01 instead of localhost
wg nex -m ollama:qwen3:32b    # Same issue
```
**Bug**: The `lambda01` default endpoint URL overrides the `ollama` provider's
default URL via `config.llm_endpoints.find_default()` fallback in
`create_provider_ext()`.

## Bugs Found

### Bug 1: `-e <url>` strips `/v1` causing 404 (HIGH PRIORITY)

**File**: `src/executor/native/provider.rs:63-86`

The `build_inline_url_client()` function strips `/v1` from the URL:
```rust
let base = url
    .trim_end_matches('/')
    .trim_end_matches("/v1")  // ← strips /v1
    .to_string();
```

But `OpenAiClient::send()` constructs URLs as `format!("{}/chat/completions", self.base_url)`,
without re-adding `/v1`. This means ALL OpenAI-compatible servers (Ollama, vLLM, SGLang,
llama.cpp) fail via the `-e` flag because their endpoints are at `/v1/chat/completions`.

**Fix**: Remove the `/v1` stripping, or change to:
```rust
let base = url.trim_end_matches('/').to_string();
let base = if base.ends_with("/v1") { base } else { format!("{}/v1", base) };
```

### Bug 2: Default endpoint overrides provider-specific URL (MEDIUM)

**File**: `src/executor/native/provider.rs:288-291`

When using `ollama:qwen3:4b`:
1. Provider resolves correctly to `local` with default URL `http://localhost:11434/v1`
2. Endpoint resolution: `find_by_name(None)` → `find_for_provider("local")` (no match,
   lambda01 is `oai-compat`) → `find_default()` → returns lambda01
3. Lambda01's URL overrides the Ollama default

The `find_default()` fallback should NOT apply when the model string explicitly
names a provider (via prefix like `ollama:`). The provider-specific default URL
should take priority over the default endpoint's URL.

## Config Entries

### Already configured (in .workgraph.1/config.toml):

```toml
[[llm_endpoints.endpoints]]
name = "lambda01"
provider = "oai-compat"
url = "https://lambda01.tail334fe6.ts.net:30000/v1"
api_key = "none"
is_default = true
context_window = 32768

[[model_registry]]
id = "qwen3-coder-30b"
provider = "oai-compat"
model = "qwen3-coder-30b"
endpoint = "lambda01"
context_window = 32768
```

### Needed for local Ollama (add to config.toml):

```toml
[[llm_endpoints.endpoints]]
name = "ollama-local"
provider = "oai-compat"
url = "http://localhost:11434/v1"
api_key = "local"
is_default = false
context_window = 32768

# Optional: register specific models
[[model_registry]]
id = "qwen3-32b"
provider = "oai-compat"
model = "qwen3:32b"
endpoint = "ollama-local"
context_window = 32768
```

Then use: `wg nex -m qwen3:32b -e ollama-local`

## Environment Variables (Agent Context)

The coordinator sets these env vars for spawned agents:
- `WG_ENDPOINT_URL=https://lambda01.tail334fe6.ts.net:30000/v1` — overrides endpoint URLs
- `WG_LLM_PROVIDER=anthropic` — forces Anthropic provider for bare model names
- `WG_API_KEY=none` — dummy API key
- `WG_MODEL=claude-opus-4-latest` — default model for the agent

These must be unset or overridden when targeting local models from agent context.

## Fallback Plan (if lambda01 unavailable)

1. **Add `ollama-local` named endpoint** (see config above), then:
   `wg nex -m qwen3:32b -e ollama-local`
2. **Start alt Ollama on 11435**: `bash terminal-bench/start-local-model.sh`
   Then: `wg nex -m qwen3-coder:30b-a3b-q8_0 -e local-model`
3. **Use OpenRouter**: `wg nex -m openrouter:qwen/qwen3-coder`
4. **Fix the `-e` URL bug** and use: `wg nex -m qwen3:32b -e http://localhost:11434`

## Terminal-Bench Integration

The existing TB infrastructure (`run_pilot_qwen3_local_10.py`) uses:
- Model spec: `local:qwen3-coder-30b`
- Endpoint: `http://lambda01:30000/v1` (plain hostname, not Tailscale FQDN)
- Config: Sets `[native_executor] api_base` in per-trial config.toml
- Context window: 32768

This path works because it writes a clean per-trial config that explicitly sets
`api_base`, bypassing the endpoint resolution chain entirely. For nex integration,
the named endpoint approach (`-e lambda01`) is cleaner.

## Token Limits & Context

| Endpoint | Model | Context Window | Max Output |
|----------|-------|---------------|------------|
| lambda01 SGLang | qwen3-coder-30b | 32768 | 32768 |
| Ollama local | qwen3:32b | 32768 (default, configurable) | N/A |
| Ollama local | qwen3:4b | 32768 (default, configurable) | N/A |

Ollama's context can be expanded via `num_ctx` parameter in requests or via
`/api/generate` with `options.num_ctx`. The Q4_K_M quantization of qwen3:32b
uses ~20GB VRAM, leaving room on most GPUs for larger context.
