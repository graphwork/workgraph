# Design: Generic OpenAI-Compatible Executor

## Status: Design (March 2026)

## Summary

Extend the existing native executor to support any OpenAI-compatible API endpoint (OpenRouter, OpenAI, Ollama, vLLM, LiteLLM, etc.), not just Anthropic's Messages API. This replaces the Amplifier (Python) dependency for running non-Claude models while keeping Amplifier as an optional executor for users who want its bundle ecosystem.

The key insight: we already have 90% of what we need. The native executor already implements the tool-use loop, tool registry, bundle system, and agent lifecycle. The remaining work is an API abstraction layer that can speak both Anthropic Messages and OpenAI Chat Completions wire formats.

## Motivation

Current state:
- **`claude` executor**: Calls Claude Code CLI. Works well but requires Node.js, npm, and a Claude subscription.
- **`native` executor**: Calls Anthropic Messages API directly from Rust. Has tool-use loop, tool registry, bundle filtering. Anthropic-only.
- **`amplifier` executor**: Calls Amplifier (Python) which supports OpenAI/OpenRouter/Ollama. Requires Python, `uv`, bundle loading complexity.
- **`shell` executor**: No LLM, just shell commands.

The gap: to use a non-Claude model (GPT-4o, DeepSeek, Gemini, Llama via OpenRouter), you need the full Amplifier stack. That's a Python dependency, patches for OpenRouter support that haven't been upstreamed, and a bundle composition system that duplicates what workgraph already does.

Goal: `wg config --coordinator-executor native --model openai/gpt-4o` should Just Work, routing through OpenRouter or any compatible endpoint.

## Design Decisions

### D1: API Format — Abstract over both, OpenAI Chat Completions as default

The two API formats are structurally different but semantically identical for tool use:

| Aspect | Anthropic Messages | OpenAI Chat Completions |
|--------|-------------------|------------------------|
| Endpoint | `POST /v1/messages` | `POST /v1/chat/completions` |
| Auth header | `x-api-key` | `Authorization: Bearer` |
| System prompt | Top-level `system` field | `{"role": "system", ...}` message |
| Tool definition | `{ name, description, input_schema }` | `{ type: "function", function: { name, description, parameters } }` |
| Tool call (response) | `content[]` block with `type: "tool_use"` | `tool_calls[]` array on the message |
| Tool result (request) | `{ role: "user", content: [{ type: "tool_result", ... }] }` | `{ role: "tool", tool_call_id, content }` |
| Stop reason | `stop_reason: "tool_use"` | `finish_reason: "tool_calls"` |
| Content blocks | Array of typed blocks (text, tool_use, etc.) | Single `content` string + separate `tool_calls` |

**Decision**: Introduce a `Provider` trait that abstracts the wire format. Two implementations: `AnthropicProvider` (existing client, refactored) and `OpenAIProvider` (new). The agent loop works against the `Provider` trait and never sees wire format details.

Why not OpenAI-only? Anthropic's native API has features that the OpenAI-compatible endpoint lacks: prompt caching with explicit `cache_control` markers, extended thinking blocks, and better streaming granularity. For Claude models, the native API is strictly better.

### D2: Tool implementation — Reuse existing ToolRegistry as-is

The native executor already has a complete tool system (`src/executor/native/tools/`):
- `ToolRegistry` with register/dispatch/filter
- `Tool` trait with `name()`, `definition()`, `execute()`
- File tools: `read_file`, `write_file`, `edit_file`, `glob`, `grep`
- Bash tool: shell execution with timeout
- Workgraph tools: `wg_show`, `wg_list`, `wg_add`, `wg_done`, `wg_fail`, `wg_log`, `wg_artifact`

**Decision**: No changes to the tool system. The `Provider` trait handles translating tool definitions and results between the internal format and the wire format. Tool definitions are stored in the Anthropic format internally (what we already have), and the `OpenAIProvider` converts them on the fly.

### D3: Streaming — Non-streaming for task agents, streaming optional

For task agents dispatched by the coordinator, non-streaming is simpler and sufficient. The agent runs, tools execute, task completes. Nobody is watching the output in real-time.

For the coordinator chat (`wg chat`), streaming is desirable for responsiveness. But that's a separate concern — the chat system can use a streaming-capable method on the provider while the agent loop uses the simpler non-streaming path.

**Decision**: The `Provider` trait exposes `complete()` (non-streaming, returns full response) and optionally `complete_stream()` (streaming, returns event iterator). The agent loop uses `complete()`. The chat interface can use `complete_stream()` when available.

### D4: Context management — The executor manages conversation history

The agent loop already maintains `Vec<Message>` across turns. This doesn't change. The `Provider` trait works with an internal `Message` type that gets serialized to the appropriate wire format.

For context window management: start with "let the API return a 400 and handle it" (current behavior). If this becomes a problem, add sliding-window context pruning as a follow-up task.

### D5: Model selection — Per-task model override via existing hierarchy

Resolution priority (already implemented):
1. Task-level: `task.model` field
2. Role-level: `role.default_model`
3. Coordinator-level: `coordinator.model` in config
4. Default: `claude-sonnet-4-latest-20250514`

The generic executor extends this by allowing the model string to encode the provider. Convention:
- `claude-sonnet-4-latest-20250514` → Anthropic provider (native API)
- `openai/gpt-4o` → OpenAI provider (Chat Completions API, base_url from config)
- `anthropic/claude-sonnet-4` → OpenAI provider via OpenRouter
- `deepseek/deepseek-chat-v3-0324` → OpenAI provider via OpenRouter
- Bare model name without `/` → Anthropic provider (backward compatible)

### D6: Executor-weight-tier integration — Bundles work unchanged

The bundle system (`src/executor/native/bundle.rs`) already handles tool filtering by exec_mode:
- `full` → all tools
- `light` → read-only + wg tools
- `bare` → wg tools only
- `shell` → bash + wg tools

This is provider-agnostic. The bundle filters the tool registry before the provider sees it. No changes needed.

### D7: Amplifier bundle compatibility — Not a goal

Per the research report: Amplifier's bundle ecosystem is small (< 30 repos, Microsoft-only, no registry), tightly coupled to Python, and not worth pursuing compatibility with. The patterns are worth understanding (composition, context injection), but workgraph's own config system already covers these use cases.

**Decision**: No Amplifier bundle loading. The Amplifier executor remains available as `executor = "amplifier"` for users who want the full Amplifier stack. The native/generic executor is independent.

## Architecture

### Module Layout

```
src/executor/native/
├── mod.rs              # Re-exports
├── provider.rs         # NEW: Provider trait + model routing
├── client.rs           # REFACTORED: Becomes AnthropicProvider
├── openai_client.rs    # NEW: OpenAIProvider implementation
├── agent.rs            # MODIFIED: Uses Provider trait instead of AnthropicClient
├── bundle.rs           # UNCHANGED
└── tools/
    ├── mod.rs           # UNCHANGED
    ├── file.rs          # UNCHANGED
    ├── bash.rs          # UNCHANGED
    └── wg.rs            # UNCHANGED
```

### Provider Trait

```rust
// src/executor/native/provider.rs

use async_trait::async_trait;
use anyhow::Result;
use super::client::{Message, ToolDefinition, Usage, StopReason};

/// Result of a single LLM completion call.
pub struct CompletionResponse {
    pub id: String,
    /// Text content from the response (may be empty if only tool calls).
    pub text: Option<String>,
    /// Tool calls requested by the model.
    pub tool_calls: Vec<ToolCall>,
    /// Why the model stopped generating.
    pub stop_reason: StopReason,
    /// Token usage for this call.
    pub usage: Usage,
}

/// A tool call from the model.
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Result of a tool execution, to be sent back to the model.
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// Abstraction over LLM API providers.
///
/// Implementations handle wire format differences (headers, request/response
/// serialization, tool call encoding) while the agent loop works with a
/// uniform interface.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider name for logging (e.g., "anthropic", "openai").
    fn name(&self) -> &str;

    /// The model being used.
    fn model(&self) -> &str;

    /// Maximum tokens per response.
    fn max_tokens(&self) -> u32;

    /// Send a completion request and return the response.
    ///
    /// The provider translates between its internal message format and the
    /// wire protocol. `system_prompt` is sent as a system message. `messages`
    /// is the conversation history. `tools` are the available tool definitions.
    async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<CompletionResponse>;
}
```

### Internal Message Format

The existing `Message` and `ContentBlock` types from `client.rs` remain the canonical internal representation. The `Provider` trait implementations convert to/from wire format:

```
Agent Loop
    ↕  Message, ToolDefinition, CompletionResponse (internal types)
Provider Trait
    ↕
┌───────────────────┬───────────────────┐
│ AnthropicProvider │  OpenAIProvider   │
│ (x-api-key,       │  (Bearer auth,    │
│  /v1/messages,     │  /v1/chat/compl,  │
│  content blocks)   │  tool_calls[])    │
└───────────────────┴───────────────────┘
    ↕                     ↕
  Anthropic API      OpenAI / OpenRouter / Ollama / vLLM
```

### AnthropicProvider (refactored from existing client.rs)

The existing `AnthropicClient` is refactored to implement `Provider`. The HTTP/retry logic stays the same. The main change is implementing `complete()` which maps to the existing `messages()` method with response transformation:

```rust
// Anthropic response.content[] → CompletionResponse
//   ContentBlock::Text { text } → response.text
//   ContentBlock::ToolUse { id, name, input } → response.tool_calls[]
//
// Anthropic tool_result in next request:
//   Message { role: User, content: [ToolResult { tool_use_id, content, is_error }] }
```

This is a mechanical refactor — the existing code already does exactly this, it's just wired directly into the agent loop instead of going through a trait.

### OpenAIProvider (new)

Implements `Provider` for the OpenAI Chat Completions format:

```rust
pub struct OpenAIProvider {
    http: reqwest::Client,
    api_key: String,
    base_url: String,    // e.g., "https://openrouter.ai/api/v1"
    model: String,
    max_tokens: u32,
}
```

**Request translation** (internal → OpenAI wire):

```rust
fn build_request(
    system_prompt: &str,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let mut oai_messages = vec![
        json!({ "role": "system", "content": system_prompt }),
    ];

    for msg in messages {
        match msg.role {
            Role::User => {
                // Check if this is a tool result message
                let tool_results: Vec<_> = msg.content.iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                            Some(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": if *is_error {
                                    format!("Error: {}", content)
                                } else {
                                    content.clone()
                                }
                            }))
                        }
                        _ => None,
                    })
                    .collect();

                if !tool_results.is_empty() {
                    oai_messages.extend(tool_results);
                } else {
                    // Regular user message
                    let text = msg.content.iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    oai_messages.push(json!({ "role": "user", "content": text }));
                }
            }
            Role::Assistant => {
                let text = msg.content.iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");

                let tool_calls: Vec<_> = msg.content.iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": input.to_string()
                                }
                            }))
                        }
                        _ => None,
                    })
                    .collect();

                let mut assistant_msg = json!({ "role": "assistant" });
                if !text.is_empty() {
                    assistant_msg["content"] = json!(text);
                }
                if !tool_calls.is_empty() {
                    assistant_msg["tool_calls"] = json!(tool_calls);
                }
                oai_messages.push(assistant_msg);
            }
        }
    }

    // Convert tool definitions
    let oai_tools: Vec<_> = tools.iter().map(|t| {
        json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema
            }
        })
    }).collect();

    json!({
        "model": self.model,
        "max_tokens": self.max_tokens,
        "messages": oai_messages,
        "tools": oai_tools,
        "tool_choice": "auto"
    })
}
```

**Response translation** (OpenAI wire → internal):

```rust
fn parse_response(body: &serde_json::Value) -> Result<CompletionResponse> {
    let choice = &body["choices"][0];
    let message = &choice["message"];
    let finish_reason = choice["finish_reason"].as_str().unwrap_or("stop");

    let text = message["content"].as_str().map(String::from);

    let tool_calls = message["tool_calls"]
        .as_array()
        .map(|arr| arr.iter().map(|tc| {
            ToolCall {
                id: tc["id"].as_str().unwrap_or("").to_string(),
                name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
                input: serde_json::from_str(
                    tc["function"]["arguments"].as_str().unwrap_or("{}")
                ).unwrap_or(json!({})),
            }
        }).collect())
        .unwrap_or_default();

    let stop_reason = match finish_reason {
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    };

    let usage = Usage {
        input_tokens: body["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        output_tokens: body["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    };

    Ok(CompletionResponse {
        id: body["id"].as_str().unwrap_or("").to_string(),
        text,
        tool_calls,
        stop_reason,
        usage,
    })
}
```

### Agent Loop Changes

The agent loop (`agent.rs`) changes from `client: AnthropicClient` to `provider: Box<dyn Provider>`:

```rust
pub struct AgentLoop {
    provider: Box<dyn Provider>,  // was: client: AnthropicClient
    tools: ToolRegistry,
    system_prompt: String,
    max_turns: usize,
    output_log: PathBuf,
}

impl AgentLoop {
    pub async fn run(&self, initial_message: &str) -> Result<AgentResult> {
        let mut messages: Vec<Message> = vec![/* ... */];

        loop {
            let response = self.provider.complete(
                &self.system_prompt,
                &messages,
                &self.tools.definitions(),
            ).await?;

            // Add assistant message to history
            // (reconstruct Message from CompletionResponse)
            let mut content_blocks = Vec::new();
            if let Some(text) = &response.text {
                content_blocks.push(ContentBlock::Text { text: text.clone() });
            }
            for tc in &response.tool_calls {
                content_blocks.push(ContentBlock::ToolUse {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.input.clone(),
                });
            }
            messages.push(Message {
                role: Role::Assistant,
                content: content_blocks,
            });

            match response.stop_reason {
                StopReason::EndTurn | StopReason::StopSequence => {
                    // Done
                    return Ok(/* ... */);
                }
                StopReason::ToolUse => {
                    // Execute tools, add results
                    let mut results = Vec::new();
                    for tc in &response.tool_calls {
                        let output = self.tools.execute(&tc.name, &tc.input).await;
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: output.content,
                            is_error: output.is_error,
                        });
                    }
                    messages.push(Message {
                        role: Role::User,
                        content: results,
                    });
                }
                StopReason::MaxTokens => {
                    // Continuation prompt
                    messages.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: "Your response was truncated. Please continue.".into(),
                        }],
                    });
                }
            }
        }
    }
}
```

### Provider Routing

Given a model string, resolve which provider to use:

```rust
// src/executor/native/provider.rs

pub fn create_provider(
    model: &str,
    config: &NativeExecutorConfig,
) -> Result<Box<dyn Provider>> {
    if model.contains('/') {
        // Model has a provider prefix → OpenAI-compatible endpoint
        // e.g., "openai/gpt-4o", "anthropic/claude-sonnet-4", "deepseek/deepseek-chat-v3"
        let api_key = resolve_openai_api_key(config)?;
        let base_url = config.openai_base_url
            .as_deref()
            .unwrap_or("https://openrouter.ai/api/v1");

        Ok(Box::new(OpenAIProvider::new(
            api_key,
            base_url,
            model,  // Pass full model string — OpenRouter uses "provider/model" format
            config.max_tokens,
        )?))
    } else {
        // Bare model name → Anthropic native API
        let api_key = resolve_anthropic_api_key(config)?;
        let base_url = config.anthropic_base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com");

        Ok(Box::new(AnthropicProvider::new(
            api_key,
            base_url,
            model,
            config.max_tokens,
        )?))
    }
}
```

## Configuration Schema

### Config.toml Extension

```toml
[native_executor]
# Anthropic API (used for bare model names like "claude-sonnet-4-latest-20250514")
# api_key = "sk-ant-..."  # or set ANTHROPIC_API_KEY env var
# anthropic_base_url = "https://api.anthropic.com"

# OpenAI-compatible API (used for prefixed model names like "openai/gpt-4o")
# openai_api_key = "sk-..."  # or set OPENAI_API_KEY / OPENROUTER_API_KEY env var
# openai_base_url = "https://openrouter.ai/api/v1"

# Default model
model = "claude-sonnet-4-latest-20250514"

# Max tokens per response
max_tokens = 16384

# Max turns per agent conversation
max_turns = 200
```

### API Key Resolution

**Anthropic provider** (bare model names):
1. `ANTHROPIC_API_KEY` env var
2. `[native_executor] api_key` in config.toml
3. `~/.config/anthropic/api_key` file

**OpenAI provider** (prefixed model names):
1. `OPENROUTER_API_KEY` env var (preferred for OpenRouter)
2. `OPENAI_API_KEY` env var
3. `[native_executor] openai_api_key` in config.toml

### CLI Configuration

```bash
# Use Claude via native Anthropic API (default)
wg config --model claude-sonnet-4-latest-20250514

# Use GPT-4o via OpenRouter
wg config --model openai/gpt-4o

# Use DeepSeek via OpenRouter
wg config --model deepseek/deepseek-chat-v3-0324

# Use a local model via Ollama
wg config --native-executor-openai-base-url http://localhost:11434/v1
wg config --model local/llama3

# Direct OpenAI API
wg config --native-executor-openai-base-url https://api.openai.com/v1
wg config --model gpt-4o  # Hmm, no prefix — this would go to Anthropic
```

Note: The `contains('/')` heuristic means direct OpenAI API usage requires `openai/gpt-4o` format even when talking directly to OpenAI. This is intentional — it makes provider selection explicit and prevents ambiguity. An alternative is an explicit `--provider` flag, but the model prefix convention is simpler and matches OpenRouter's format.

### Per-Task Model Override

The existing model hierarchy already supports this:

```bash
# This implementation task should use a capable model
wg add "Implement feature X" --model claude-sonnet-4-latest-20250514

# This research task can use a cheap model
wg add "Research: look up API docs" --model deepseek/deepseek-chat-v3-0324
```

## Comparison with Amplifier

| Dimension | Generic Executor (this design) | Amplifier Executor |
|-----------|-------------------------------|-------------------|
| Language | Rust (native, no runtime deps) | Python (requires uv, pip) |
| Startup time | Instant (in-process) | ~3-5s (Python process, module loading) |
| Provider support | Anthropic native + any OpenAI-compatible | Same, plus Azure, Gemini, Ollama native |
| Tool system | Rust-native ToolRegistry (in-process) | Python Tool protocol (subprocess) |
| Bundle system | TOML files in .workgraph/bundles/ | Markdown+YAML, git-based composition |
| Context management | Vec<Message> in memory | Python ContextManager with persistence |
| Agent delegation | Workgraph graph (native) | Amplifier sub-sessions |
| Session resume | Not supported (single-shot) | Supported via session persistence |
| Streaming | Supported (Anthropic SSE) | Supported (orchestrator-driven) |
| Extended thinking | Supported (Anthropic native only) | Supported (provider module) |
| MCP support | Not yet | Supported (tool-mcp module) |
| Lines of code | ~800 (estimated addition) | ~15,000 (kernel + foundation + modules) |
| Maintenance | Part of workgraph, one codebase | Separate project, upstream dependency |

**When to use Amplifier**: Users who want the full Amplifier ecosystem (bundles, recipes, agents, session persistence) or need provider-specific features not yet in the generic executor (Azure AD auth, Gemini-specific features, MCP tools).

**When to use Generic Executor**: Everyone else. It's faster, simpler, zero-dependency, and covers the 90% use case of "give an LLM a prompt and tools, let it work."

## Implementation Phases

### Phase 1: Provider Trait + Anthropic Refactor

**Files**: `src/executor/native/provider.rs` (new), `src/executor/native/client.rs` (refactor)

1. Define the `Provider` trait, `CompletionResponse`, `ToolCall`, `ToolResult` types
2. Wrap existing `AnthropicClient` as `AnthropicProvider` implementing `Provider`
3. Add `create_provider()` routing function
4. Update `AgentLoop` to use `Box<dyn Provider>` instead of `AnthropicClient`
5. Update `native_exec.rs` entry point to use `create_provider()`

**Test**: Existing native executor behavior is unchanged. All existing tests pass.

**Scope**: ~200 lines new code, ~50 lines refactored.

### Phase 2: OpenAI Provider

**Files**: `src/executor/native/openai_client.rs` (new)

1. Implement `OpenAIProvider` with `complete()` method
2. Request translation: internal Message → OpenAI Chat Completions JSON
3. Response translation: OpenAI JSON → `CompletionResponse`
4. Auth: `Authorization: Bearer` header
5. Retry logic: same as Anthropic (429, 500, 502, 503)

**Test**: Unit tests with mock HTTP responses. Integration test with OpenRouter if API key available.

**Scope**: ~300 lines new code.

### Phase 3: Configuration + API Key Resolution

**Files**: `src/config.rs` (extend), `src/executor/native/provider.rs` (extend)

1. Add `NativeExecutorConfig` to config.toml schema
2. Add `openai_api_key`, `openai_base_url`, `anthropic_base_url` fields
3. Implement `resolve_openai_api_key()` with env var fallback chain
4. Wire config into `create_provider()` and `native_exec.rs`

**Test**: Config parsing tests, API key resolution tests.

**Scope**: ~100 lines.

### Phase 4: CLI Integration

**Files**: `src/commands/mod.rs`, `src/cli.rs` (extend)

1. Add `wg config --native-executor-openai-base-url` command
2. Ensure `wg config --model openai/gpt-4o` works (already should, model is a string)
3. Update `wg service start` logging to show provider being used

**Scope**: ~50 lines.

### Phase 5: Validation + Edge Cases

1. Test with OpenRouter: GPT-4o, DeepSeek, Claude-via-OpenRouter
2. Test with Ollama (local)
3. Handle provider-specific quirks:
   - OpenRouter `HTTP-Referer` and `X-Title` headers
   - Ollama doesn't return `usage` in all responses
   - Some models don't support tool use — fail gracefully with clear error
4. Handle `arguments` field as string (some models return JSON string instead of parsed JSON)

**Scope**: ~100 lines of edge case handling.

## Security Considerations

- **API keys**: Never logged. Resolved from env vars first (preferred), config file second.
- **OpenRouter headers**: `HTTP-Referer` and `X-Title` are optional metadata headers, not credentials.
- **Trust model**: Same as current native executor — tools run with user privileges, no sandboxing beyond filesystem boundaries.
- **Config file permissions**: The config file may contain API keys. Users should set restrictive permissions (`chmod 600`). Consider printing a warning if permissions are too open.

## Open Questions

1. **Anthropic's OpenAI-compatible endpoint** (`https://api.anthropic.com/v1/`): Should we route through this for models like `anthropic/claude-sonnet-4` when used with the OpenAI provider? This would lose prompt caching but simplify the provider selection. **Recommendation**: No — if you're using Claude, use the native Anthropic API for best performance.

2. **Extended thinking**: The Anthropic API supports `thinking` blocks in responses. The OpenAI API doesn't have a standard equivalent. Some providers (DeepSeek) have `reasoning_content`. **Recommendation**: Handle thinking blocks in `AnthropicProvider` only. Ignore them for now in `OpenAIProvider`. Add support for provider-specific thinking/reasoning blocks as a follow-up.

3. **MCP tools**: Amplifier supports MCP (Model Context Protocol) tools via `tool-mcp` module. Should the native executor support MCP? **Recommendation**: Not in this design. MCP support could be added later as a separate tool type in the `ToolRegistry`. The `bash` tool already provides escape-hatch access to any MCP server via CLI.

4. **Cost tracking**: Different providers have different pricing. Should we track per-model costs? **Recommendation**: Track token usage (already done). Cost calculation can be added later with a pricing config table.

## Non-Goals

- **Amplifier bundle loading**: Not pursuing compatibility with Amplifier's bundle format.
- **Session persistence/resume**: Agents are single-shot. State is in the workgraph graph.
- **Provider-specific features beyond tool use**: Azure AD auth, Gemini safety filters, etc. These belong in the Amplifier executor if needed.
- **Automatic model fallback**: If one model fails, auto-retry with a different model. This adds complexity and should be a separate feature if needed.
- **Prompt caching for OpenAI provider**: OpenAI's caching model is automatic (server-side). No client-side changes needed.
