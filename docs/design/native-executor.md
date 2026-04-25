# Native Executor: Rust-Native LLM Client Design

## Status: Design (March 2026)

## Overview

A `native` executor that calls the Anthropic Messages API directly from Rust, implementing a tool-use loop with both file tools and in-process workgraph tools. This eliminates the dependency on Claude CLI and Amplifier for agent execution, giving workgraph full control over the LLM interaction lifecycle.

## Motivation

Current executors depend on external processes:
- **`claude`**: Requires Claude Code CLI (Node.js, npm). Spawns a full session per agent. Expensive startup, opaque tool handling, limited control over context management.
- **`amplifier`**: Requires Amplifier (Python). Different tool ecosystem, separate configuration.
- **`shell`**: No LLM — just shell commands.

A native executor provides:
- **Zero external dependencies** — no Node.js, no Python, no CLI installation
- **In-process wg tools** — `wg_add`, `wg_done`, etc. call library functions directly (microseconds vs subprocess overhead)
- **Full control** — streaming, retries, context management, token tracking all in Rust
- **Bundle-native design** — tool filtering is a Rust-level allowlist, not `--tools` flag parsing

## Architecture

### Module Layout

```
src/executor/
├── mod.rs              # Re-exports, NativeExecutor registration
└── native/
    ├── mod.rs          # NativeExecutor struct, implements spawn interface
    ├── client.rs       # Anthropic HTTP client (reqwest + SSE streaming)
    ├── agent.rs        # Tool-use loop (message → tool_call → execute → result → loop)
    ├── tools/
    │   ├── mod.rs      # ToolRegistry, ToolDefinition, dispatch
    │   ├── file.rs     # read_file, write_file, edit_file, glob, grep
    │   ├── bash.rs     # Shell command execution
    │   └── wg.rs       # In-process workgraph operations
    └── bundle.rs       # Bundle loading and tool filtering
```

### Integration Point

The native executor registers alongside existing executors in the `ExecutorRegistry`:

```rust
// In ExecutorRegistry::default_config()
"native" => Ok(ExecutorConfig {
    executor: ExecutorSettings {
        executor_type: "native".to_string(),
        command: "__native__".to_string(),  // sentinel: not a subprocess
        args: vec![],
        env: HashMap::new(),
        prompt_template: None,
        working_dir: Some("{{working_dir}}".to_string()),
        timeout: None,
        model: None,
    },
})
```

However, unlike `claude`/`amplifier`/`shell` executors which spawn a subprocess via `Command::new()`, the `native` executor runs **in-process** within the service daemon. This requires a different code path in `spawn_agent_inner`:

```rust
// In spawn/execution.rs
match settings.executor_type.as_str() {
    "native" => spawn_native_agent(dir, task_id, &vars, scope, &scope_ctx, &settings),
    _ => spawn_subprocess_agent(/* existing logic */),
}
```

The native agent runs as a `tokio::spawn` task within the daemon's async runtime, not as a detached subprocess. This means:
- No wrapper script, no PID-based tracking
- Agent liveness is tracked by `JoinHandle` instead of `kill(pid, 0)`
- Output is written to the same `agents/<id>/output.log` path
- The agent registry gets a new variant for in-process agents

### Spawn Flow (Native)

```
coordinator tick
  → ready task found
  → spawn_native_agent()
    → claim task (same as today)
    → assemble prompt (build_prompt, same as today)
    → resolve bundle → tool allowlist
    → tokio::spawn(agent_loop(client, prompt, tools, task_id))
    → register agent (in-process variant, with JoinHandle)
  → tick returns
```

The agent loop runs concurrently. When it completes, it writes results and the triage system picks it up on the next tick (same as dead-agent cleanup today, but checking JoinHandle::is_finished() instead of is_process_alive()).

## Anthropic API Client

### HTTP Client (`client.rs`)

```rust
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,   // default: https://api.anthropic.com
    model: String,
    max_tokens: u32,    // default: 16384
}

impl AnthropicClient {
    pub fn from_env(model: &str) -> Result<Self>;
    pub fn from_config(api_key: &str, model: &str) -> Result<Self>;

    /// Send a messages request and return the full response.
    /// Used for non-streaming (simpler, good for bare/light tiers).
    pub async fn messages(
        &self,
        request: &MessagesRequest,
    ) -> Result<MessagesResponse>;

    /// Send a messages request with streaming (SSE).
    /// Returns a stream of ServerSentEvent items.
    /// Used for full tier to show progress.
    pub async fn messages_stream(
        &self,
        request: &MessagesRequest,
    ) -> Result<impl Stream<Item = Result<StreamEvent>>>;
}
```

### Request/Response Types

```rust
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub stream: bool,
}

pub struct Message {
    pub role: Role,           // "user" | "assistant"
    pub content: Vec<ContentBlock>,
}

pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

pub struct MessagesResponse {
    pub id: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}
```

### Streaming Events

```rust
pub enum StreamEvent {
    MessageStart { message: MessagesResponse },
    ContentBlockStart { index: usize, content_block: ContentBlock },
    ContentBlockDelta { index: usize, delta: ContentDelta },
    ContentBlockStop { index: usize },
    MessageDelta { delta: MessageDelta, usage: Usage },
    MessageStop,
    Ping,
    Error { error: ApiError },
}

pub enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
}
```

### API Interaction Details

**Endpoint**: `POST /v1/messages`

**Headers**:
```
x-api-key: <key>
anthropic-version: 2023-06-01
content-type: application/json
```

**Retry policy**:
- 429 (rate limit): Retry with `retry-after` header, exponential backoff (1s, 2s, 4s, max 60s)
- 529 (overloaded): Same retry policy as 429
- 500/502/503: Retry up to 3 times with exponential backoff
- All other errors: Fail immediately

**API key resolution** (priority order):
1. `ANTHROPIC_API_KEY` environment variable
2. `.workgraph/config.toml` field: `[native_executor] api_key = "..."`
3. `~/.config/anthropic/api_key` file (single line)

**Model resolution**: Same hierarchy as existing executors — `task.model > executor.model > coordinator.model > "claude-sonnet-4-latest"`

## Tool-Use Loop

### Agent Loop (`agent.rs`)

```rust
pub struct AgentLoop {
    client: AnthropicClient,
    tools: ToolRegistry,
    system_prompt: String,
    max_turns: usize,       // default: 200
    output_log: PathBuf,
}

pub struct AgentResult {
    pub final_text: String,
    pub turns: usize,
    pub total_usage: Usage,
    pub tool_calls: Vec<ToolCallRecord>,
}

impl AgentLoop {
    pub async fn run(&self, initial_message: &str) -> Result<AgentResult> {
        let mut messages: Vec<Message> = vec![
            Message { role: Role::User, content: vec![ContentBlock::Text {
                text: initial_message.to_string()
            }]},
        ];
        let mut total_usage = Usage::default();
        let mut tool_calls = Vec::new();
        let mut turns = 0;

        loop {
            if turns >= self.max_turns {
                // Log warning, return what we have
                break;
            }

            let response = self.client.messages(&MessagesRequest {
                model: self.client.model.clone(),
                max_tokens: self.client.max_tokens,
                system: Some(self.system_prompt.clone()),
                messages: messages.clone(),
                tools: self.tools.definitions(),
                stream: false,
            }).await?;

            total_usage.add(&response.usage);
            turns += 1;

            // Log assistant response to output file
            self.log_turn(&response);

            // Add assistant response to conversation
            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            match response.stop_reason {
                StopReason::EndTurn | StopReason::StopSequence => {
                    // Agent is done — extract final text
                    let final_text = response.content.iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Ok(AgentResult { final_text, turns, total_usage, tool_calls });
                }
                StopReason::ToolUse => {
                    // Execute all tool_use blocks and collect results
                    let mut results = Vec::new();
                    for block in &response.content {
                        if let ContentBlock::ToolUse { id, name, input } = block {
                            let result = self.tools.execute(name, input).await;
                            tool_calls.push(ToolCallRecord {
                                name: name.clone(),
                                input: input.clone(),
                                output: result.clone(),
                                is_error: result.is_error,
                            });
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: result.content,
                                is_error: result.is_error,
                            });
                        }
                    }
                    messages.push(Message {
                        role: Role::User,
                        content: results,
                    });
                }
                StopReason::MaxTokens => {
                    // Context too long or output truncated.
                    // Add a continuation prompt.
                    messages.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: "Your response was truncated. Please continue.".to_string()
                        }],
                    });
                }
            }
        }

        // Max turns reached
        Ok(AgentResult {
            final_text: "[max turns reached]".to_string(),
            turns,
            total_usage,
            tool_calls,
        })
    }
}
```

### Turn Logging

Each turn is logged to `agents/<id>/output.log` in NDJSON format for compatibility with existing agent output parsing:

```json
{"type":"turn","turn":1,"role":"assistant","content":[...],"usage":{"input":1234,"output":567}}
{"type":"tool_call","name":"bash","input":{"command":"cargo build"},"output":"...","is_error":false}
{"type":"turn","turn":2,"role":"assistant","content":[...],"usage":{"input":2345,"output":678}}
{"type":"result","final_text":"Task completed.","turns":5,"total_usage":{"input":12000,"output":3000}}
```

## Tool System

### Tool Registry (`tools/mod.rs`)

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput;
}

pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,  // JSON Schema
}

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolRegistry {
    pub fn new() -> Self;

    /// Register a tool.
    pub fn register(&mut self, tool: Box<dyn Tool>);

    /// Get JSON Schema definitions for all registered tools (for API request).
    pub fn definitions(&self) -> Vec<ToolDefinition>;

    /// Execute a tool by name.
    pub async fn execute(&self, name: &str, input: &serde_json::Value) -> ToolOutput;

    /// Create a filtered registry containing only the named tools.
    pub fn filter(&self, allowed: &[String]) -> ToolRegistry;

    /// Create the full default registry with all tools.
    pub fn default_all(workgraph_dir: &Path) -> Self;
}
```

### Core File Tools (`tools/file.rs`)

| Tool | Input Schema | Description |
|------|-------------|-------------|
| `read_file` | `{ path: string, offset?: number, limit?: number }` | Read file contents. Returns numbered lines. |
| `write_file` | `{ path: string, content: string }` | Write/overwrite a file. |
| `edit_file` | `{ path: string, old_string: string, new_string: string }` | String replacement edit. Fails if old_string not found or not unique. |
| `glob` | `{ pattern: string, path?: string }` | Find files matching glob pattern. |
| `grep` | `{ pattern: string, path?: string, glob?: string }` | Search file contents with regex. |

These are straightforward filesystem operations. They mirror Claude Code's tools closely so agent prompts remain portable.

**Implementation notes**:
- `read_file`: Use `std::fs::read_to_string`, add line numbers, truncate lines >2000 chars
- `write_file`: Use `std::fs::write`, create parent dirs if needed
- `edit_file`: Read file, find unique occurrence of `old_string`, replace, write back
- `glob`: Use the `glob` crate or `walkdir` + pattern matching
- `grep`: Use `regex` crate, walk files, return matching lines with context

### Bash Tool (`tools/bash.rs`)

```rust
pub struct BashTool {
    default_timeout: Duration,  // 120s
    working_dir: PathBuf,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `command` | `string` | Shell command to execute |
| `timeout` | `number?` | Timeout in milliseconds (max 600000) |

Executes via `tokio::process::Command::new("bash").arg("-c").arg(command)`.
Captures stdout + stderr. Kills on timeout via SIGTERM, then SIGKILL after 5s.

### Workgraph Tools (`tools/wg.rs`)

These call workgraph library functions directly — **no subprocess, no CLI parsing overhead**.

| Tool | Maps to | Description |
|------|---------|-------------|
| `wg_show` | `workgraph::commands::show` | Show task details |
| `wg_list` | `workgraph::query::*` | List tasks with optional status filter |
| `wg_add` | `workgraph::commands::add` | Create a new task |
| `wg_edit` | `workgraph::commands::edit` | Edit task fields |
| `wg_done` | `workgraph::commands::done` | Mark task as done |
| `wg_fail` | `workgraph::commands::fail` | Mark task as failed |
| `wg_log` | `workgraph::commands::log` | Append a log entry |
| `wg_artifact` | `workgraph::commands::artifact` | Record an artifact |
| `wg_context` | `workgraph::commands::context` | Get task context |

**Input schemas** (representative):

```json
// wg_add
{
  "type": "object",
  "properties": {
    "title": { "type": "string" },
    "description": { "type": "string" },
    "after": { "type": "string", "description": "Comma-separated dependency task IDs" },
    "tags": { "type": "string", "description": "Comma-separated tags" },
    "skills": { "type": "string" }
  },
  "required": ["title"]
}

// wg_done
{
  "type": "object",
  "properties": {
    "task_id": { "type": "string" },
    "converged": { "type": "boolean", "description": "Use for cycle convergence" }
  },
  "required": ["task_id"]
}

// wg_log
{
  "type": "object",
  "properties": {
    "task_id": { "type": "string" },
    "message": { "type": "string" }
  },
  "required": ["task_id", "message"]
}
```

**Implementation approach**: Each wg tool function takes a `&Path` to the workgraph directory and calls the corresponding library function. The workgraph dir is captured in the tool closure at registration time:

```rust
pub fn register_wg_tools(registry: &mut ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(WgAddTool { dir: workgraph_dir.clone() }));
    registry.register(Box::new(WgDoneTool { dir: workgraph_dir.clone() }));
    // ...
}
```

Currently, many `wg` commands in `src/commands/` are structured as CLI entry points that parse args and call into `workgraph::*` library functions. The wg tools reuse those library functions directly, bypassing CLI arg parsing. Where a clean library function doesn't yet exist (some commands do all their logic inline), we'll extract one during implementation.

## Bundle System

### Bundle Configuration Format

Bundles define tool allowlists and context for the native executor. Stored as TOML in `.workgraph/bundles/`:

```toml
# .workgraph/bundles/research.toml
[bundle]
name = "research"
description = "Read-only research and analysis"
tools = [
    "read_file",
    "glob",
    "grep",
    "wg_show",
    "wg_list",
    "wg_log",
    "wg_done",
    "wg_fail",
    "wg_artifact",
]
context_scope = "graph"
system_prompt_suffix = """
You are a research agent. Your job is to investigate, analyze, and report findings.
You cannot modify files or create tasks — only read and observe.
"""

# .workgraph/bundles/implementer.toml
[bundle]
name = "implementer"
description = "Full implementation agent with all tools"
tools = ["*"]    # wildcard: all registered tools
context_scope = "full"

# .workgraph/bundles/bare.toml
[bundle]
name = "bare"
description = "Minimal agent with wg tools only"
tools = [
    "wg_show",
    "wg_list",
    "wg_add",
    "wg_edit",
    "wg_done",
    "wg_fail",
    "wg_log",
    "wg_artifact",
    "wg_context",
]
context_scope = "task"

# .workgraph/bundles/shell.toml
[bundle]
name = "shell"
description = "Agent with bash access and wg tools"
tools = [
    "bash",
    "wg_show",
    "wg_list",
    "wg_done",
    "wg_fail",
    "wg_log",
]
context_scope = "clean"
```

### Rust Types (`bundle.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleConfig {
    pub bundle: BundleSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleSettings {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,           // tool names or "*" for all
    pub context_scope: String,        // maps to ContextScope enum
    #[serde(default)]
    pub system_prompt_suffix: String,
}

impl BundleConfig {
    pub fn load(path: &Path) -> Result<Self>;
    pub fn load_by_name(workgraph_dir: &Path, name: &str) -> Result<Self>;
}
```

### Mapping exec_mode to Bundles

The existing `exec_mode` tiers (full/light/bare/shell) map directly to bundles:

| exec_mode | Default Bundle | Tools | Context Scope |
|-----------|---------------|-------|---------------|
| `full` | `implementer` | `*` (all) | `full` |
| `light` | `research` | read_file, glob, grep, wg_* | `graph` |
| `bare` | `bare` | wg_* only | `task` |
| `shell` | `shell` | bash, wg_show/list/done/fail/log | `clean` |

When the native executor spawns an agent:
1. Resolve `exec_mode` for the task (existing logic: `task.exec_mode > role.default_exec_mode > "full"`)
2. Map exec_mode to bundle name (or use task's explicit bundle if set)
3. Load bundle config
4. Create filtered `ToolRegistry` with only the allowed tools
5. Set context scope from bundle
6. Append `system_prompt_suffix` to prompt if present

### Custom Bundles

Users can create custom bundles beyond the four default tiers. A task or role can reference a bundle by name:

```yaml
# In task
bundle: my-custom-bundle

# In role
default_bundle: research
```

This extends the existing `exec_mode` system without breaking it. The `exec_mode` field continues to work as before — it just maps to a default bundle. Explicit `bundle` field overrides the exec_mode-derived bundle.

## Configuration

### Executor Registration (`.workgraph/executors/native.toml`)

```toml
[executor]
type = "native"
# No command/args needed — runs in-process

# Default model for native executor
model = "claude-sonnet-4-latest"

# Timeout per agent (seconds). 0 = disabled.
timeout = 3600

[executor.env]
WG_TASK_ID = "{{task_id}}"
WG_AGENT_ID = "{{agent_id}}"
```

### Config.toml Extension

```toml
[native_executor]
# API key (prefer ANTHROPIC_API_KEY env var instead)
# api_key = "sk-ant-..."

# Default model
model = "claude-sonnet-4-latest"

# Max tokens per response
max_tokens = 16384

# Max turns per agent conversation
max_turns = 200

# Base URL (for proxies or alternative endpoints)
# base_url = "https://api.anthropic.com"
```

## Token Usage Tracking

The native executor has direct access to API usage data. After each agent run:

```rust
// Update task with token usage
let task = graph.get_task_mut_or_err(task_id)?;
task.token_usage = Some(TokenUsage {
    input_tokens: result.total_usage.input_tokens,
    output_tokens: result.total_usage.output_tokens,
    cache_creation_tokens: result.total_usage.cache_creation_input_tokens,
    cache_read_tokens: result.total_usage.cache_read_input_tokens,
    turns: result.turns as u32,
});
```

The existing `token_usage` field on `Task` already supports this — it's populated today by parsing Claude CLI stream-json output. The native executor can populate it directly and precisely.

## Error Handling

### Agent-Level Errors

| Error | Recovery |
|-------|----------|
| API key missing | Fail immediately with clear message |
| Network error (connection refused, DNS) | Retry 3x with backoff, then fail |
| 401 Unauthorized | Fail immediately — bad API key |
| 429 Rate Limited | Retry with `retry-after`, max 5 retries |
| 529 Overloaded | Same as 429 |
| 500/502/503 Server Error | Retry 3x with exponential backoff |
| Max turns exceeded | Return partial result, log warning |
| Tool execution error | Return error to LLM as tool_result with `is_error: true` |
| Context too long (400 with message about tokens) | Attempt context truncation, retry once |

### Daemon-Level Errors

The native executor runs in-process, so a panic in the agent loop could crash the daemon. Mitigations:
- `tokio::spawn` with `catch_unwind` wrapper
- Agent panics are caught, logged, and the task is marked Failed
- Each agent gets its own `JoinHandle` for clean lifecycle management

## Dependency Additions (Cargo.toml)

```toml
# Required (reqwest already optional for matrix-lite)
reqwest = { version = "0.12", features = ["json", "stream"] }  # promote from optional
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "process"] }
regex = "1"                    # for grep tool
async-trait = "0.1"            # for Tool trait
futures-util = { version = "0.3" }  # for stream processing (already optional dep)
```

`reqwest` is already an optional dependency (used by `matrix-lite` feature). For the native executor, it becomes a required dependency. `tokio` is already required. `futures-util` is already an optional dependency.

New required dependencies: `regex` (for grep tool), `async-trait` (for Tool trait). Both are widely used, well-maintained crates with no transitive bloat.

## Implementation Phases

### Phase 4a: API Client + Agent Loop
- `src/executor/native/client.rs` — HTTP client, request/response types, retry logic
- `src/executor/native/agent.rs` — Tool-use loop, turn logging
- `src/executor/native/tools/mod.rs` — ToolRegistry, Tool trait, dispatch
- Integration test: send a message, get a response (mocked or real API)

### Phase 4b: Core Tools
- `src/executor/native/tools/file.rs` — read_file, write_file, edit_file, glob, grep
- `src/executor/native/tools/bash.rs` — Shell execution with timeout
- Unit tests for each tool

### Phase 4c: Workgraph Tools
- `src/executor/native/tools/wg.rs` — In-process wg operations
- Extract library functions where commands currently do everything inline
- Integration test: native agent creates a task, logs progress, marks done

### Phase 4d: Bundle System + Integration
- `src/executor/native/bundle.rs` — Bundle loading, tool filtering
- Default bundle configs in `.workgraph/bundles/`
- Native executor registration in `ExecutorRegistry`
- Spawn path in `spawn_agent_inner`
- End-to-end test: coordinator dispatches a task to native executor, agent completes it

## Security Considerations

- **API key handling**: Never log the API key. Use `secrecy` crate if warranted, but env var + config file with restrictive permissions is standard practice.
- **Bash tool**: Same sandboxing as today (none — agents run with user privileges). The native executor doesn't change the trust model.
- **File tools**: Operate within the working directory. No path traversal protection beyond what the filesystem provides (same as Claude CLI).
- **Tool result size**: Truncate tool outputs to 100KB to prevent context overflow. Log truncation events.

## Open Design Decisions

1. **Streaming vs non-streaming for tool-use loop**: Non-streaming is simpler (single response object). Streaming lets us show real-time progress in TUI and detect hangs earlier. **Recommendation**: Start non-streaming for Phase 4a, add streaming in Phase 4d or later.

2. **In-process vs subprocess for native agents**: This design specifies in-process (`tokio::spawn`). Alternative: spawn a separate Rust binary (`wg native-agent`) as a subprocess. In-process is more efficient but tightly couples agent lifecycle to daemon. **Recommendation**: In-process, with crash isolation via `catch_unwind`. If stability is a concern, the subprocess approach is a fallback.

3. **Context management for long conversations**: The agent loop accumulates messages. For long-running agents (200 turns), this could exceed context limits. Options: (a) let the API return a 400 and handle it, (b) implement sliding-window context pruning, (c) summarize older turns. **Recommendation**: Start with (a), implement (b) if needed.

4. **Prompt caching**: The Anthropic API supports prompt caching for system prompts. The native executor could cache the system prompt across turns automatically, reducing input token costs significantly. **Recommendation**: Add cache_control markers to system prompt blocks.
