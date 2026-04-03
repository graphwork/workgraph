# Thinking Token Patterns Across OpenRouter Models

Research report for task `tb-thinking-research`.

## 1. Model Survey: Thinking/Reasoning Token Formats

167 OpenRouter models currently support the `reasoning` and/or `include_reasoning` parameters. The table below covers the major model families and their thinking token behavior.

### Summary Table

| Model Family | Example Model IDs | Think Tag Format | Mandatory? | Return Mechanism | Preservation Required? |
|---|---|---|---|---|---|
| **DeepSeek R1** | `deepseek/deepseek-r1`, `deepseek/deepseek-r1-0528` | `<think>...</think>` | Yes | `reasoning` field (OpenRouter) / `reasoning_content` field (native) | Between tool calls: yes. New user turns: no (strip) |
| **MiniMax M2.x** | `minimax/minimax-m2.7`, `minimax/minimax-m2.5` | `<think>...</think>` | Yes | `content-string` (inline in content) + `reasoning` field via OpenRouter | Must be preserved for optimal performance |
| **Qwen QwQ/Qwen3** | `qwen/qwq-32b`, `qwen/qwen3-32b`, `qwen/qwen3-max-thinking` | `<think>...</think>` | Yes (QwQ); configurable (Qwen3) | `reasoning` field | Configurable |
| **OpenAI o-series** | `openai/o1`, `openai/o3-mini`, `openai/o4-mini` | None (internal) | N/A | `reasoning_details` (encrypted/summarized) | Pass back `reasoning_details` array unchanged |
| **Anthropic Claude** | `anthropic/claude-sonnet-4.6`, `anthropic/claude-opus-4.6` | None (structured blocks) | No | `reasoning_details` with `format: anthropic-claude-v1` | Pass back via `reasoning_details` |
| **Google Gemini** | `google/gemini-2.5-pro`, `google/gemini-3.1-pro-preview` | None (internal) | Yes (2.5 Pro); configurable (3.x) | `reasoning_details` with `format: google-gemini-v1` | Pass back via `reasoning_details` |
| **xAI Grok** | `x-ai/grok-3-mini`, `x-ai/grok-4` | None (internal) | No | `reasoning` field via effort parameter | Effort-based control |
| **ByteDance Seed** | `bytedance-seed/seed-1.6` | Provider-specific | Configurable | `reasoning` field | Unknown |
| **NVIDIA Nemotron** | `nvidia/nemotron-3-nano-30b-a3b` | Provider-specific | Configurable | `reasoning` field | Unknown |
| **Moonshot Kimi** | `moonshotai/kimi-k2-thinking` | Provider-specific | Yes | `reasoning` field — **best transparency across all modes** | Yes |
| **Z-AI GLM** | `z-ai/glm-5`, `z-ai/glm-4.5` | Provider-specific | Configurable | `reasoning` field | Unknown |

### Key Observations

1. **No standard tag format.** Open-source reasoning models (DeepSeek, MiniMax, Qwen) converge on `<think>...</think>` tags. Proprietary models (OpenAI, Anthropic, Google) use structured API fields without exposed tags.

2. **OpenRouter normalizes heterogeneity.** Regardless of the underlying model's native format, OpenRouter exposes reasoning through two unified fields:
   - `message.reasoning` — plaintext string of reasoning steps
   - `message.reasoning_details` — structured array with typed entries (`reasoning.text`, `reasoning.summary`, `reasoning.encrypted`)

3. **Two `:thinking` variant models exist**: `anthropic/claude-3.7-sonnet:thinking` and `qwen/qwen-plus-2025-07-28:thinking`. These are separate model IDs with reasoning enabled by default.

## 2. OpenRouter API Behavior

### Request Parameters

```json
{
  "reasoning": {
    "effort": "high",           // "xhigh" | "high" | "medium" | "low" | "minimal" | "none"
    "max_tokens": 2000,         // For Anthropic/Gemini: direct token budget
    "exclude": false,           // Use reasoning internally but hide from response
    "enabled": true             // Simple toggle (defaults to "medium" effort)
  }
}
```

**Legacy parameter:** `"include_reasoning": true` is equivalent to `"reasoning": {}` and is deprecated.

**Provider mapping:**
- `effort` → OpenAI/Grok models (maps to `reasoning_effort`)
- `max_tokens` → Anthropic (minimum 1024), Gemini (`thinkingLevel`), some Qwen models
- Both are available on most models; OpenRouter maps to the appropriate native parameter

### Non-Streaming Response Format

```json
{
  "choices": [{
    "message": {
      "role": "assistant",
      "content": "The final answer...",
      "reasoning": "Step 1: ... Step 2: ...",
      "reasoning_details": [
        {
          "type": "reasoning.text",
          "text": "Step 1: analyze the problem...",
          "id": "rs_abc123",
          "format": "deepseek-r1-v1",
          "index": 0
        }
      ]
    }
  }],
  "usage": {
    "output_tokens_details": {
      "reasoning_tokens": 45
    }
  }
}
```

### Streaming Response Format

Reasoning chunks arrive as `choices[].delta.reasoning_details` events before content deltas.

### Conversation History Preservation

To maintain reasoning context across multi-turn conversations with tool use:

```json
{
  "role": "assistant",
  "content": "...",
  "reasoning_details": [/* pass back unchanged from previous response */]
}
```

**Critical rules:**
- The `reasoning_details` sequence must be passed back exactly as received — no reordering or modification
- For tool-use flows, reasoning MUST be preserved between the assistant response and the tool result (DeepSeek returns 400 errors otherwise)
- For new user turns (non-tool), DeepSeek recommends stripping `reasoning_content` to avoid error accumulation
- OpenAI o-series uses encrypted reasoning blocks that cannot be inspected but must be passed back verbatim

## 3. Preservation Requirements

### Model-Specific Behavior

| Model | Strip reasoning on new user turn? | Strip reasoning between tool calls? | Degrades without preservation? |
|---|---|---|---|
| DeepSeek R1 | Yes (recommended) | **No — causes 400 error** | Yes (tool flows break) |
| MiniMax M2.7 | No (preserve) | No (preserve) | Yes ("must be preserved for optimal performance") |
| OpenAI o-series | Pass back encrypted blocks | Pass back encrypted blocks | Unknown (opaque) |
| Anthropic Claude | Pass back `reasoning_details` | Pass back `reasoning_details` | Mild (46-token gap in tool calling) |
| Qwen QwQ/3 | Configurable | Configurable | Depends on model |
| Kimi K2.5 | Preserve all | Preserve all | No — best transparency |

### Independent Testing Results (Feb 2026)

Testing 5 models via OpenRouter (GLM-5, Kimi K2.5, MiniMax M2.5, Claude Sonnet 4.6, GPT-5.2):
- **3 of 5 models silently stop returning reasoning tokens** in JSON schema mode
- Tool calling mode also degrades reasoning visibility on most models
- **Kimi K2.5** maintained reasoning across all test modes (best performer)
- **GPT-5.2** was worst: drops reasoning fields during tool calls and JSON modes

## 4. Current State in Workgraph

### Executor Code Audit

**Finding: The workgraph native executor has ZERO handling of reasoning/thinking tokens.**

Specific gaps:

1. **`OaiResponseMessage` struct** (`src/executor/native/openai_client.rs:97-103`): Only deserializes `content` and `tool_calls`. The `reasoning` and `reasoning_details` fields from OpenRouter responses are silently dropped by serde.

2. **`OaiStreamDelta` struct** (`src/executor/native/openai_client.rs:176-184`): Only captures `content` and `tool_calls` deltas. Streaming `reasoning_details` chunks are silently ignored.

3. **`ContentBlock` enum** (`src/executor/native/client.rs:29-44`): Has only `Text`, `ToolUse`, and `ToolResult` variants. No `Thinking` or `Reasoning` variant exists.

4. **`translate_messages()`** (`src/executor/native/openai_client.rs:361-476`): When converting conversation history back to API format, assistant messages only include `content` and `tool_calls`. Any reasoning that was in the original response is lost and cannot be passed back.

5. **`translate_response()`** (`src/executor/native/openai_client.rs:478-580`): Converts OAI responses to canonical format. Only extracts `content` and `tool_calls` from `choice.message`. Reasoning fields are discarded.

6. **Anthropic client** (`src/executor/native/client.rs`): Also has no thinking/reasoning handling. Extended thinking for Claude models is not supported.

7. **Journal** (`src/executor/native/journal.rs`): Records `ContentBlock` entries. Since there's no reasoning variant, reasoning tokens are never persisted.

8. **TUI** (`src/tui/`): No rendering for thinking/reasoning content.

### Terminal Bench Impact

**TB experiments are losing thinking tokens.** Evidence:

- Only **1 instance** of `<think>` tags found across all 78 NDJSON result files (in `make-mips-interpreter` task with MiniMax M2.7)
- That single instance appears as inline text in the `content` field — it was a fragment where MiniMax's `</think>` tag leaked into the visible output
- **We never send `reasoning: {}` or `include_reasoning: true`** in requests, so OpenRouter doesn't return the `reasoning` field
- For models with mandatory reasoning (MiniMax M2.7, DeepSeek R1, QwQ), the reasoning is happening server-side but we never see it
- The `content-string` return mechanism for MiniMax means `<think>...</think>` blocks may be in the content, but our executor doesn't parse or separate them

**Token accounting impact:** Without `include_reasoning`, reasoning tokens are still consumed and billed as output tokens, but we can't measure them separately. The `usage.output_tokens_details.reasoning_tokens` field is never captured.

## 5. Recommendations

### Should We Preserve Reasoning Tokens?

**Yes — always capture; optionally display.**

Rationale:
1. **Correctness:** DeepSeek R1 requires reasoning preservation between tool calls (400 error without it). MiniMax documents degradation without preservation.
2. **Observability:** Reasoning tokens are billed output tokens. Without capturing them, we can't accurately report costs or understand model behavior in TB experiments.
3. **Debuggability:** Reasoning traces are invaluable for understanding agent failures and evaluating quality.
4. **Minimal overhead:** The `reasoning` field is a string. Capturing it adds negligible memory/storage cost.

### Proposed Approach

#### Phase 1: Capture (executor changes)

1. **Add `Thinking` variant to `ContentBlock`:**
   ```rust
   pub enum ContentBlock {
       Text { text: String },
       Thinking { thinking: String },  // NEW
       ToolUse { id: String, name: String, input: serde_json::Value },
       ToolResult { tool_use_id: String, content: String, is_error: bool },
   }
   ```

2. **Deserialize `reasoning` + `reasoning_details` in `OaiResponseMessage`:**
   ```rust
   struct OaiResponseMessage {
       role: String,
       content: Option<String>,
       tool_calls: Option<Vec<OaiToolCall>>,
       reasoning: Option<String>,              // NEW
       reasoning_details: Option<Vec<serde_json::Value>>,  // NEW (opaque pass-through)
   }
   ```

3. **Emit `Thinking` blocks in `translate_response()`:** Before text/tool blocks, emit a `Thinking` block from the `reasoning` field.

4. **Pass `reasoning_details` back in `translate_messages()`:** When serializing assistant messages for the API, include the `reasoning_details` array if present.

5. **Add `reasoning_details` to streaming delta:** Capture `reasoning_details` chunks in `OaiStreamDelta` and accumulate them alongside text content.

6. **Send `reasoning: {}` by default** for models that support it (check `supported_parameters` from model metadata).

#### Phase 2: Display (TUI + journal)

7. **Journal:** Persist `Thinking` blocks in conversation.jsonl. Add `reasoning_details` as an opaque JSON field on assistant message entries.

8. **TUI agent viewer:** Render thinking blocks in a collapsed/dimmed style, distinguishable from regular output.

9. **Stream events:** Add a `thinking` event type so the harness and TUI can display reasoning in real-time.

#### Phase 3: TB experiment integration

10. **Update `tb-harness.sh`:** Ensure `reasoning: {}` is passed for all models. Capture reasoning token counts in result.json.

11. **Token accounting:** Parse `usage.output_tokens_details.reasoning_tokens` and report separately from content tokens.

12. **Re-run MiniMax M2.7 experiments** with reasoning capture enabled to get accurate thinking token data.

### Priority

- **Phase 1 is critical** — without it, DeepSeek R1 tool flows are broken and all reasoning data is lost
- **Phase 2 is important** — needed for debugging and evaluation
- **Phase 3 is needed for TB** — current experiment data is missing reasoning token metrics
