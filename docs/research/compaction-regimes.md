# LLM Provider Compaction Regimes Research

**Date:** 2026-03-12  
**Task:** research-compaction-regimes  
**Motivation:** Inform workgraph's token-threshold-based compaction gating design

---

## Summary Table

| Provider | Model (Key Examples) | Context Window | Auto-Compaction | API for Querying Limits | Overflow Behavior |
|----------|---------------------|----------------|-----------------|-------------------------|-------------------|
| **Anthropic** | Claude Opus 4.6, Sonnet 4.6 | 200K (1M beta) | **Opt-in beta** — `compact-2026-01-12` header; summarizes at configurable threshold (default 150K) | `POST /v1/messages/count_tokens` (pre-flight token count); response `usage.*_tokens` fields | **400 error** `invalid_request_error` — no silent truncation (newer models ≥ Sonnet 3.7) |
| **Anthropic** | Claude Haiku 4.5 | 200K | Same beta feature (if enabled) | Same | Same |
| **OpenRouter** | Any proxied model | Proxied from provider | **Middle-out compression** — opt-in via `transforms: ["middle-out"]`; auto-enabled for ≤8K models | `GET /api/v1/models` returns `context_length` field per model | **Error** without transforms; with transforms: messages silently removed from middle |
| **OpenAI** | GPT-4o, GPT-4o-mini | 128K | **None** | No programmatic context-size field in Chat API; tiktoken for estimation | **400 error** `context_length_exceeded` — no auto-truncation |
| **OpenAI** | GPT-4.1, GPT-4.1-mini, -nano | ~1M (1,047,576) | None | Same | Same |
| **OpenAI** | GPT-5, GPT-5-mini, -nano | 400K | None | Same | Same |
| **OpenAI** | o1, o3, o4-mini | 200K | None | Same | Same |

---

## Anthropic (Claude) — Detailed Findings

### Context Window Sizes (as of 2026-03)

| Model | Context Window | Max Output | Notes |
|-------|---------------|------------|-------|
| claude-opus-4-6 | 200K (1M beta) | 128K | 1M requires `context-1m-2025-08-07` beta header, usage tier 4 |
| claude-sonnet-4-6 | 200K (1M beta) | 64K | Same 1M beta requirement |
| claude-haiku-4-5 | 200K | 64K | No 1M beta |
| (legacy) claude-sonnet-4.5, opus-4.5, opus-4.1 | 200K | 64K / 32K | |

### Auto-Compaction Behavior

**Does the Claude API auto-compact by default?** No. Compaction is opt-in.

**How to enable:** Include beta header `compact-2026-01-12` and add `context_management.edits` to request body:

```python
response = client.beta.messages.create(
    betas=["compact-2026-01-12"],
    model="claude-opus-4-6",
    max_tokens=4096,
    messages=messages,
    context_management={
        "edits": [{
            "type": "compact_20260112",
            "trigger": {"type": "input_tokens", "value": 150000},  # default; min 50,000
            "pause_after_compaction": False,   # if True, stops with stop_reason="compaction"
            "instructions": None,              # custom summarization prompt (replaces default)
        }]
    },
)
```

**How it works:**
1. API detects input tokens ≥ trigger threshold
2. Generates a summary of the conversation
3. Returns `compaction` block at start of assistant response content
4. On subsequent requests, all content prior to the `compaction` block is ignored by the API

**Compaction block in response:**
```json
{
  "content": [
    {
      "type": "compaction",
      "content": "Summary of the conversation: The user requested help building a web scraper..."
    },
    {
      "type": "text",
      "text": "Based on our conversation so far..."
    }
  ]
}
```

**Supported models:** claude-opus-4-6 and claude-sonnet-4-6 only (as of 2026-03).

**Streaming:** Compaction block arrives as `content_block_start` with type `compaction`, followed by a single `content_block_delta` with the complete summary (no incremental streaming), then `content_block_stop`.

### Overflow Without Compaction

For models ≥ Claude Sonnet 3.7: **validation error** (HTTP 400, `invalid_request_error`). No silent truncation. Prior models may have truncated silently.

### API for Querying Token Usage / Context Limits

**Pre-flight token counting:**
```bash
POST https://api.anthropic.com/v1/messages/count_tokens
Content-Type: application/json
anthropic-version: 2023-06-01

{
  "model": "claude-opus-4-6",
  "messages": [{"role": "user", "content": "..."}]
}
# Response: {"input_tokens": 1234}
```

**Response token usage fields:**
```json
{
  "usage": {
    "input_tokens": 12345,
    "output_tokens": 678,
    "cache_read_input_tokens": 0,
    "cache_creation_input_tokens": 0
  }
}
```

**No API endpoint for model context window size.** It must be looked up from documentation or hardcoded per model.

### Context Awareness (New in Sonnet 4.6, Sonnet 4.5, Haiku 4.5)

These models receive their remaining context budget via system injections during tool use:

```xml
<!-- Injected at conversation start -->
<budget:token_budget>200000</budget:token_budget>

<!-- After each tool call -->
<system_warning>Token usage: 35000/200000; 165000 remaining</system_warning>
```

This helps agents self-manage before hitting limits, but does not trigger compaction on its own.

### Claude Code Compaction

Claude Code (the CLI tool) implements its own compaction mechanism independent of the API beta. Workgraph exposes this as a visible `.compact-0` cycle task (coordinator → compact → coordinator loop). This is separate from and complementary to the API-level `compact-2026-01-12` beta feature.

---

## OpenRouter — Detailed Findings

### Context Window Sizes

OpenRouter exposes context window sizes via API and proxies them from underlying providers:

```bash
GET https://openrouter.ai/api/v1/models
# Returns JSON array; each model object includes:
# - context_length: integer (total context window)
# - top_provider.context_length: integer (provider-reported limit)
```

Selected examples (2026-03):

| Model ID | Context Length |
|----------|---------------|
| anthropic/claude-sonnet-4.6 | 1,000,000 |
| anthropic/claude-opus-4.6 | 1,000,000 |
| anthropic/claude-haiku-4.5 | 200,000 |
| openai/gpt-4o | 128,000 |
| openai/gpt-4o-mini | 128,000 |
| openai/gpt-4.1 | 1,047,576 |
| openai/gpt-5 | 400,000 |
| openai/o3 | 200,000 |

```python
import requests

def get_openrouter_context_length(model_id: str) -> int | None:
    resp = requests.get("https://openrouter.ai/api/v1/models")
    models = {m["id"]: m for m in resp.json()["data"]}
    return models.get(model_id, {}).get("context_length")
```

### Auto-Compaction: Middle-Out Compression

OpenRouter offers **middle-out compression** — not summarization, but message removal.

**Algorithm:** Removes messages from the **middle** of the prompt (preserving beginning and end). Rationale: LLMs pay less attention to middle of sequences. For Anthropic models with message count limits, keeps half from start and half from end.

**How to enable:**
```json
{
  "model": "anthropic/claude-sonnet-4.6",
  "messages": [...],
  "transforms": ["middle-out"]
}
```

**How to disable (override auto-enable for ≤8K models):**
```json
{
  "transforms": []
}
```

**Auto-enabled for:** All OpenRouter endpoints with ≤8K context length.

**This is NOT summarization** — it is silent deletion of middle messages. Loss of information is not surfaced to the caller.

### Context Overflow Behavior

- **Without transforms:** Request fails with error suggesting to reduce context or enable middle-out.
- **With middle-out:** Messages silently deleted from the middle; response continues without signaling what was removed.
- **Provider selection:** OpenRouter prefers providers whose context ≥ half of total required tokens; falls back to highest-context-length provider available.

### Differences vs. Direct API Access

- Middle-out compression is OpenRouter-side; the underlying provider receives a truncated request
- Adds routing/failover; the actual provider can vary per request
- No additional transparency about which messages were dropped
- Context window sizes may differ slightly from provider-reported values

---

## OpenAI — Detailed Findings

### Context Window Sizes (as of 2026-03, via OpenRouter)

| Model Family | Context Window |
|-------------|---------------|
| GPT-4.1, GPT-4.1-mini, GPT-4.1-nano | ~1,047,576 |
| GPT-5.4, GPT-5.4-pro | 1,050,000 |
| GPT-5, GPT-5-mini, GPT-5-nano, GPT-5-pro | 400,000 |
| GPT-4o, GPT-4o-mini (current) | 128,000 |
| GPT-4-turbo | 128,000 |
| o1, o3, o4-mini, o3-pro | 200,000 |
| GPT-3.5-turbo | 16,385 |

### Auto-Compaction / Auto-Summarization

**Chat Completions API:** **None.** No auto-truncation, no auto-summarization. Exceeding context returns an error.

**Assistants API:** Maintains conversation threads server-side, but **no auto-summarization**. Threads can have configurable truncation strategies (drop oldest messages), not summarization.

**Responses API (stateful):** Stores conversation history server-side but also **no auto-compaction**. Still fails with an error if context is exceeded.

### Context Overflow Behavior

Returns HTTP 400 with error type `context_length_exceeded`:
```json
{
  "error": {
    "message": "This model's maximum context length is 128000 tokens. However, your messages resulted in 135000 tokens. Please reduce the length of the messages.",
    "type": "invalid_request_error",
    "code": "context_length_exceeded"
  }
}
```

No silent truncation in modern models.

### API for Querying Context Window Size

**The standard `/v1/models` endpoint does NOT return context window size.** It only returns model `id`, `object`, `created`, and `owned_by`.

**Workarounds:**
1. **Hardcode per model** (error-prone as models change)
2. **Use tiktoken** to estimate token counts before sending
3. **Use OpenRouter's `/api/v1/models`** as an authoritative catalog with `context_length`

```python
# Estimate token count with tiktoken (OpenAI models)
import tiktoken

def count_tokens(messages: list[dict], model: str) -> int:
    enc = tiktoken.encoding_for_model(model)
    count = 0
    for msg in messages:
        count += 4  # message overhead
        for key, value in msg.items():
            count += len(enc.encode(value))
    return count + 2  # reply overhead
```

**Response usage fields:**
```json
{
  "usage": {
    "prompt_tokens": 12345,
    "completion_tokens": 678,
    "total_tokens": 13023
  }
}
```

No field for "remaining context" or "% of context used."

### Recommended Pattern for Long-Running Conversations

OpenAI recommends client-side context management:
- Manually drop old messages from the array
- Summarize older history client-side and inject as a system message
- Use the Responses API with `store: true` for multi-session continuity (no auto-compaction)
- For Assistants API: configure `truncation_strategy` (drops old messages, not summarizes)

---

## Cross-Provider: Querying Context Window Size Programmatically

```python
# Anthropic: no model metadata API for context size
# Must hardcode or use this mapping:
ANTHROPIC_CONTEXT_WINDOWS = {
    "claude-opus-4-6": 200_000,
    "claude-sonnet-4-6": 200_000,
    "claude-haiku-4-5": 200_000,
    "claude-haiku-4-5-20251001": 200_000,
}

# OpenRouter: programmatic API available
import requests

def openrouter_context_lengths() -> dict[str, int]:
    resp = requests.get("https://openrouter.ai/api/v1/models")
    return {
        m["id"]: m["context_length"]
        for m in resp.json()["data"]
    }

# OpenAI: no context-size field in /v1/models
# Use tiktoken to count tokens, hardcode limits per model
OPENAI_CONTEXT_WINDOWS = {
    "gpt-4o": 128_000,
    "gpt-4o-mini": 128_000,
    "gpt-4.1": 1_047_576,
    "gpt-4.1-mini": 1_047_576,
    "gpt-4.1-nano": 1_047_576,
    "gpt-5": 400_000,
    "o1": 200_000,
    "o3": 200_000,
    "o4-mini": 200_000,
}
```

**Provider-specific headers/fields signaling context capacity:**

| Provider | Mechanism | Notes |
|----------|-----------|-------|
| Anthropic | `usage.input_tokens` in response | Compute remaining = context_window - input_tokens |
| Anthropic (Sonnet 4.6+) | `<system_warning>Token usage: X/Y; Z remaining</system_warning>` | Injected by API into tool-use flows; not in raw API response |
| OpenRouter | `context_length` in model metadata | Pre-flight check only |
| OpenAI | `usage.prompt_tokens` in response | Compute remaining = context_window - prompt_tokens |

No provider currently returns a "you are at X% of context capacity" field in the response.

---

## Recommendation for Workgraph

### Current State

Workgraph already implements client-side compaction gating:
- `config.compaction_token_threshold` (default: 100,000 tokens)
- Coordinator tracks token usage from LLM responses and triggers compaction when accumulated tokens exceed threshold
- Compaction runs as a visible `.compact-0` cycle task

### Strategic Recommendation

**Use workgraph's own compaction, not provider auto-compaction. Optionally add the Anthropic beta as an enhancement.**

#### Rationale

1. **OpenAI has no auto-compaction.** Any system that relies on provider-side compaction is OpenAI-incompatible.

2. **OpenRouter's middle-out is lossy and silent.** Deletes messages without notification or summarization. Unacceptable for task context fidelity — workgraph agents need their full task context.

3. **Anthropic's beta compaction is promising but limited.** Only supports Opus 4.6 and Sonnet 4.6 (no Haiku). It produces proper summaries (not deletions), but the workgraph compaction system is more sophisticated: it produces structured `context.md` artifacts that are injected into subsequent coordinator context, which the API-level compaction cannot replicate.

4. **Workgraph needs cross-provider portability.** The system must work the same whether using Claude, OpenAI, or any OpenRouter-proxied model.

#### Recommended Architecture

```
Tier 1 (Primary): Workgraph client-side compaction
  - Token threshold gating (already implemented, config.compaction_token_threshold)
  - Produces structured context.md artifacts
  - Works with all providers
  - Full control over what gets summarized

Tier 2 (Enhancement, Anthropic only): Anthropic API beta compaction
  - Enable compact-2026-01-12 on Opus 4.6 / Sonnet 4.6 coordinator sessions
  - Use pause_after_compaction=True to intercept and supplement with workgraph context
  - Use custom instructions to preserve task structure
  - Treat as defense-in-depth for very long sessions

Tier 3 (Hard limit): Never rely on OpenRouter middle-out
  - Disable explicitly: transforms: [] on all workgraph requests
  - Silent message deletion is incompatible with task fidelity
```

#### For Context Window Size Lookup

Implement a tiered resolution strategy:
1. **OpenRouter API** (`/api/v1/models`) when using OpenRouter — reliable, programmatic
2. **Hardcoded table** for Anthropic direct and OpenAI direct — update on model releases
3. **Fallback default** of 200,000 tokens for unknown models (conservative)

The workgraph `ModelEntry.context_window` field (already in `config.rs`) is the right place to store these values.

---

## Validation Checklist

- [x] All three providers investigated (Anthropic, OpenRouter, OpenAI)
- [x] Summary table produced with concrete answers
- [x] API endpoints/methods for querying context window size documented
- [x] Recommendation for workgraph's approach documented
