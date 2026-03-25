# OpenRouter Model Configuration & Assignment Guide

Research report for task `research-openrouter-model`. Answers 7 specific questions
about OpenRouter model support in workgraph after the resolve-model-registry,
auto-detect-native, add-config-validation, and update-setup-wizard changes.

---

## 1. How does the model_registry in config.toml work?

There are **two separate model registries** in workgraph:

### A. Config-level registry (`config.toml` `[[model_registry]]`)

**Source:** `src/config.rs:768-838` — `ModelRegistryEntry` struct
**Storage:** `[[model_registry]]` array in `.workgraph/config.toml`

This is the **tier-based** registry used for model routing, cost tracking, and
dispatch role resolution. It maps short IDs (like "haiku", "sonnet") to full API
model names with metadata.

**Current state of the project config (`.workgraph/config.toml`):**
```toml
model_registry = []   # Empty — using built-in defaults only

[tiers]
fast = "haiku"
standard = "sonnet"
# premium not set — defaults to "opus"
```

**Built-in defaults** (hardcoded in `src/config.rs:1064-1108`, returned when `model_registry` is empty or merged with user entries):

| ID | Provider | Full Model | Tier | Cost (in/out per MTok) |
|----|----------|-----------|------|----------------------|
| `haiku` | anthropic | `claude-haiku-4-5-20251001` | fast | $0.25/$1.25 |
| `sonnet` | anthropic | `claude-sonnet-4-20250514` | standard | $3.00/$15.00 |
| `opus` | anthropic | `claude-opus-4-6` | premium | $15.00/$75.00 |

**Merging logic** (`effective_registry()` at `src/config.rs:1113-1126`):
User entries override built-in entries with the same ID. Built-in entries not
shadowed by user entries are included.

Example entry:
```toml
[[model_registry]]
id = "minimax-m2.5"
provider = "openrouter"
model = "minimax/minimax-m2.5"
tier = "standard"
endpoint = "openrouter"        # optional: named endpoint from llm_endpoints
context_window = 80000
cost_per_input_mtok = 0.50
cost_per_output_mtok = 2.00
prompt_caching = false
descriptors = ["coding", "reasoning"]
```

### B. YAML-file registry (`.workgraph/models.yaml`)

**Source:** `src/models.rs:1-250` — `ModelRegistry`/`ModelEntry` structs
**Storage:** `.workgraph/models.yaml` (created by `wg models init`)

This is the **model catalog** used by `wg models list/search/remote/add` commands
and for tool-use capability checking. Ships with **13+ built-in defaults** covering
Anthropic, OpenAI, Google, DeepSeek, Meta-Llama, and Qwen models — all using
`provider: "openrouter"` by default.

**Key difference:** The YAML registry uses `provider/model-name` format IDs (e.g.,
`anthropic/claude-opus-4-6`) and a 3-tier system (`frontier`/`mid`/`budget`), while
the config registry uses short IDs (`haiku`/`sonnet`/`opus`) and a 3-tier system
(`fast`/`standard`/`premium`).

**The YAML registry is checked by the native executor** (`src/commands/native_exec.rs:112-113`)
to determine if a model supports tool use:
```rust
let model_registry = ModelRegistry::load(workgraph_dir).unwrap_or_default();
let supports_tools = model_registry.supports_tool_use(&effective_model);
```

Unknown models default to `true` (tool support assumed).

---

## 2. How to add MiniMax 2.5 and other OpenRouter models to the registry

### Method 1: Via `wg models add` (YAML registry — recommended for most users)

```bash
# Add MiniMax M2.5 to the model catalog
wg models add "minimax/minimax-m2.5" \
  --cost-in 0.50 \
  --cost-out 2.00 \
  --tier mid \
  --context-window 80000 \
  --capability coding \
  --capability reasoning \
  --capability tool_use
```

This writes to `.workgraph/models.yaml`. The `tool_use` capability is important —
the native executor checks it to decide whether to send tools in the request
(`src/models.rs:79-81`, `src/commands/native_exec.rs:112-118`).

### Method 2: Via config.toml `[[model_registry]]` (for tier-based routing)

Add to `.workgraph/config.toml`:
```toml
[[model_registry]]
id = "minimax-m2.5"
provider = "openrouter"
model = "minimax/minimax-m2.5"
tier = "standard"
context_window = 80000
cost_per_input_mtok = 0.50
cost_per_output_mtok = 2.00
descriptors = ["coding", "reasoning"]
```

Or via CLI:
```bash
wg config --registry-add \
  --id minimax-m2.5 \
  --provider openrouter \
  --reg-model minimax/minimax-m2.5 \
  --reg-tier standard \
  --cost-input 0.50 \
  --cost-output 2.00 \
  --context-window 80000
```

### Method 3: Search OpenRouter and add from remote

```bash
# Search for MiniMax models on OpenRouter
wg models search minimax

# Or list all remote models
wg models remote --limit 20
```

### Important: Register in BOTH registries for full support

For full functionality, add the model to **both** registries:
1. **YAML registry** (`wg models add`) — enables tool-use detection and `wg models list`
2. **Config registry** (`[[model_registry]]`) — enables tier-based routing and `--model minimax-m2.5` shorthand

---

## 3. How does model assignment work per-task via --model?

**Yes, you can force a specific model per-task.**

### Setting model on task creation:
```bash
wg add "Research MiniMax capabilities" --model minimax-m2.5
```

This stores `model: "minimax-m2.5"` on the `Task` struct (`src/graph.rs`).

### Model resolution hierarchy (at spawn time)

Source: `src/commands/spawn/execution.rs:150-165`

```
1. task.model           (highest — from `wg add --model` or `wg edit --model`)
2. agent.preferred_model (from agency agent identity assigned via `wg assign`)
3. executor.model       (from executor config file)
4. CLI/coordinator model (from `wg service start --model` or coordinator.model)
```

After this raw resolution, the result goes through **registry alias resolution**
(`resolve_model_via_registry` at `src/commands/spawn/execution.rs:1067-1109`):

- **Built-in tier aliases** (haiku/sonnet/opus) are **kept as-is** — the Claude CLI
  executor understands them natively
- **Custom aliases** (like `minimax-m2.5`) are **resolved** to their full API model
  ID (e.g., `minimax/minimax-m2.5`) plus provider and endpoint from the registry entry
- **Unknown task models** produce an **error** asking the user to register them first
- **Unknown non-task models** (from executor/coordinator) pass through unchanged

### Example flow:
```bash
wg add "Complex task" --model minimax-m2.5
# At spawn: "minimax-m2.5" → registry lookup → "minimax/minimax-m2.5" + provider="openrouter"
# → OpenAI-compatible client → OpenRouter endpoint
```

### Using full model IDs directly:
```bash
wg add "Task" --model "minimax/minimax-m2.5"
```
The `/` in the model string causes `create_provider_ext()` (`src/executor/native/provider.rs:87`)
to auto-detect the `openai` provider (which routes to OpenRouter via the endpoint config).

**Caveat:** If using `--model` with a string that's not in the config registry
(`[[model_registry]]`) and was set explicitly on the task, it will error. Either
register it first or use the full `provider/model` format which passes through
without registry lookup when the model doesn't match a registered alias.

---

## 4. How does the agency tier resolution interact with the model registry?

### DispatchRole → Tier mapping

Source: `src/config.rs:552-640` — `DispatchRole` enum with 13 roles

Each role resolves its model through a 6-step cascade
(`resolve_model_for_role` at `src/config.rs:1189-1240+`):

1. `[models.<role>].model` — direct model override
2. Legacy `agency.*_model` fields (deprecated)
3. `[models.<role>].tier` — role-level tier override
4. Role's `default_tier()` → `[tiers]` config → registry lookup
5. `[models.default].model` — project-wide default
6. `agent.model` — global fallback

### Can you map tiers to OpenRouter models?

**Yes.** Set the tier defaults in config.toml:

```toml
[tiers]
fast = "minimax-m2.5"        # maps to [[model_registry]] id = "minimax-m2.5"
standard = "sonnet"
premium = "opus"
```

This means all roles that resolve to `Tier::Fast` (triage, assigner, flip_comparison, placer, compactor)
will use MiniMax M2.5 instead of Haiku.

### Per-role tier override:
```toml
[models.evaluator]
tier = "fast"    # Use fast tier (MiniMax) for evaluations
```

### Per-role direct model override:
```toml
[models.evaluator]
model = "minimax-m2.5"   # Direct alias, resolved via registry
provider = "openrouter"  # Optional: explicit provider
```

### Current project config

The current `.workgraph/config.toml` uses **direct model overrides** for every role
(haiku/sonnet/opus), bypassing the tier system entirely. This is step 1 in the
resolution cascade, so tier config doesn't come into play. To use tier-based routing
with OpenRouter models, you'd change from direct model names to tier references.

---

## 5. Current config.toml [llm_endpoints] and [models] setup

### Endpoints (`.workgraph/config.toml:90-94`):
```toml
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
api_key_env = "OPENROUTER_API_KEY"
is_default = true
```

**Yes, OpenRouter IS configured as an endpoint.** It's the default endpoint, using
`OPENROUTER_API_KEY` env var for authentication.

### Models routing (`.workgraph/config.toml:102-138`):
```toml
[models.default]
provider = "anthropic"
model = "opus"

[models.task_agent]
model = "opus"

[models.evaluator]
model = "sonnet"

[models.flip_inference]
model = "sonnet"

[models.flip_comparison]
model = "haiku"

[models.assigner]
model = "haiku"

[models.evolver]
model = "opus"

[models.verification]
model = "opus"

[models.triage]
model = "haiku"

[models.creator]
model = "opus"

[models.compactor]
model = "haiku"

[models.placer]
model = "haiku"
```

### Coordinator config:
```toml
[coordinator]
executor = "claude"    # Uses Claude CLI, NOT native executor
model = "opus"
provider = "anthropic"
```

### Key observation:
The coordinator currently uses the **Claude CLI executor** (`executor = "claude"`),
not the native executor. The Claude CLI executor handles model names like "opus"
natively. For OpenRouter models, you'd need to either:
1. Switch to `executor = "native"` (recommended for OpenRouter)
2. Use a `command_template` that routes to the native executor

---

## 6. Native executor spawn path — tool use and structured output

### Tool use: Fully supported

Source: `src/executor/native/openai_client.rs`

The `OpenAiClient` sends tools in the OpenAI function-calling format:

```rust
// OaiRequest (line 52-72):
struct OaiRequest {
    tools: Vec<OaiToolDef>,        // Function definitions
    tool_choice: Option<String>,    // "auto" when tools present
    stream: bool,                   // Always true
    stream_options: ...,            // include_usage for token counting
    cache_control: ...,             // OpenRouter auto-caching
}
```

Key behaviors:
- **`tool_choice = "auto"`** is always set when tools are present — many OpenRouter-proxied
  models silently ignore tools without it (`openai_client.rs:60-61`)
- **Streaming** is always enabled with `include_usage` for token tracking
- **Cache control** is sent for OpenRouter (enables Anthropic/Gemini auto-caching)
- **Tool-use detection**: The native executor checks `ModelRegistry.supports_tool_use()`
  (`native_exec.rs:112-113`). If false, tools are omitted entirely.
- **MiniMax-specific handling**: The OpenAI client has special parsing for `</minimax:tool_call>`
  XML variants (`openai_client.rs:1095, 1122`) — MiniMax M2.5 is already explicitly supported.

### Structured output: Via tool responses

The agent loop uses tool calls for structured interaction (bash, file read/write,
grep, etc.) rather than JSON-mode structured output. The tool result is fed back
as a tool response message in the conversation.

### No-tool fallback

When `supports_tools = false` (e.g., DeepSeek R1), the agent loop runs without
tools and the model can only produce text responses. It can still use `wg` CLI
commands via text-based instructions.

---

## 7. Command template for native executor tasks

### Native executor command assembly

Source: `src/commands/spawn/execution.rs:767-808`

When `executor_type = "native"`, the spawn path builds this command:

```bash
wg native-exec \
  --prompt-file <output_dir>/prompt.txt \
  --exec-mode <exec_mode> \
  --task-id <task_id> \
  --model <effective_model> \
  --provider <effective_provider> \
  --endpoint-name <endpoint_name> \
  --endpoint-url <endpoint_url> \
  --api-key <api_key>
```

The prompt is written to a file, and all resolution (model, provider, endpoint, API
key) happens in the spawn path before the command is assembled. The `native-exec`
subcommand then:

1. Reads the prompt file
2. Resolves the bundle for `exec_mode` (tool filtering)
3. Calls `create_provider_ext()` to build the LLM client
4. Runs the `AgentLoop` to completion

### For Claude CLI executor (current default)

The Claude CLI executor uses `command_template` from config:
```toml
[agent]
command_template = 'claude --model {model} --print "{prompt}"'
```

This is different from the native executor path.

### Switching to native executor

To use OpenRouter models with the native executor:
```toml
[coordinator]
executor = "native"
model = "minimax/minimax-m2.5"
provider = "openrouter"
```

---

## Step-by-Step Guide: Adding MiniMax M2.5 (and other OpenRouter models)

### Prerequisites
- OpenRouter API key set: `export OPENROUTER_API_KEY="sk-or-v1-..."`
- Workgraph initialized: `wg init`

### Step 1: Verify endpoint exists

```bash
wg endpoints list
```

Should show:
```
openrouter (default)
  provider: openrouter
  api_key: sk-****...
```

If not:
```bash
wg endpoints add openrouter --provider openrouter --default
```

### Step 2: Register MiniMax M2.5 in the YAML model catalog

```bash
wg models add "minimax/minimax-m2.5" \
  --cost-in 0.50 \
  --cost-out 2.00 \
  --tier mid \
  --context-window 80000 \
  --capability coding \
  --capability reasoning \
  --capability tool_use
```

This enables:
- Tool-use detection (native executor won't strip tools from requests)
- `wg models list` visibility

### Step 3: Register in config registry (for short alias support)

Add to `.workgraph/config.toml`:
```toml
[[model_registry]]
id = "minimax-m2.5"
provider = "openrouter"
model = "minimax/minimax-m2.5"
tier = "standard"
context_window = 80000
cost_per_input_mtok = 0.50
cost_per_output_mtok = 2.00
```

Or via CLI:
```bash
wg config --registry-add \
  --id minimax-m2.5 \
  --provider openrouter \
  --reg-model minimax/minimax-m2.5 \
  --reg-tier standard \
  --cost-input 0.50 \
  --cost-output 2.00 \
  --context-window 80000
```

### Step 4: Use per-task

```bash
wg add "Test with MiniMax" --model minimax-m2.5
```

### Step 5: Use as tier default (optional)

```toml
[tiers]
fast = "minimax-m2.5"     # Use MiniMax for all fast-tier roles
standard = "sonnet"
premium = "opus"
```

### Step 6: Use with native executor (required for non-Claude models)

Change coordinator to native:
```toml
[coordinator]
executor = "native"
model = "minimax/minimax-m2.5"
provider = "openrouter"
```

Or keep Claude CLI for the coordinator and use native only for tasks:
```bash
wg add "Task for MiniMax" --model "minimax/minimax-m2.5"
# The spawn path auto-detects OpenAI-compatible provider from the "/" in the model name
```

**Note:** When using the Claude CLI executor (`executor = "claude"`), models like
"haiku"/"sonnet"/"opus" work because the Claude CLI understands them natively.
Non-Anthropic models REQUIRE the native executor.

---

## Gaps and Missing Functionality

### 1. Two separate registries (models.yaml vs config.toml model_registry)
There are two model registries serving different purposes. A model must be
registered in **both** for full functionality (tool-use detection from YAML,
alias resolution from config). This is confusing and error-prone.

**Impact:** If you add a model to `[[model_registry]]` but not `models.yaml`,
the native executor won't know about its tool-use capabilities (defaults to true,
so it works, but the behavior is implicit).

### 2. Tier naming mismatch
- Config registry: `fast`/`standard`/`premium`
- YAML registry: `frontier`/`mid`/`budget`

These are not aligned and serve different purposes, but the naming overlap is confusing.

### 3. Executor type determines model compatibility
The Claude CLI executor (`executor = "claude"`) only supports Anthropic model names.
Non-Anthropic OpenRouter models require `executor = "native"`. This isn't validated
at config time — you'd get a runtime error.

The config validation (`src/config.rs:2481+`) does check for some mismatches but
could be more explicit about this constraint.

### 4. MiniMax-specific tool call XML parsing
The OpenAI client already handles `</minimax:tool_call>` XML variants
(`openai_client.rs:1095, 1122`), which is good — MiniMax M2.5 is a known model
with tool-use support via OpenRouter.

### 5. No automatic YAML registry population from config registry
When you add a `[[model_registry]]` entry, the YAML `models.yaml` isn't updated
automatically, and vice versa.
