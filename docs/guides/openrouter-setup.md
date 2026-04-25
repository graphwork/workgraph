# OpenRouter Setup Guide

Use [OpenRouter](https://openrouter.ai/) to access hundreds of models from a single API key. Workgraph treats OpenRouter as an OpenAI-compatible provider, so any model available on OpenRouter works with the native executor.

---

## Quick Start

### 1. Get an API key

Sign up at [openrouter.ai](https://openrouter.ai/) and create an API key from the **Keys** page.

### 2. Configure the endpoint

```bash
wg endpoints add openrouter \
  --provider openrouter \
  --api-key-file ~/.config/openrouter/api_key \
  --default
```

This stores the endpoint in `.workgraph/config.toml`. Alternatively, set the key via environment variable:

```bash
export OPENROUTER_API_KEY="sk-or-v1-..."
```

### 3. Create an agent with a model preference

```bash
wg agent create "Deep Researcher" \
  --role <role-hash> \
  --tradeoff <tradeoff-hash> \
  --model deepseek/deepseek-chat-v3 \
  --provider openrouter
```

The `--model` flag sets the agent's preferred model. The `--provider` flag tells workgraph to route requests through OpenRouter.

### 4. Assign the agent to a task

```bash
wg assign my-research-task <agent-hash>
```

When the coordinator spawns this agent, it will use the agent's preferred model and provider.

### 5. Verify it works

```bash
wg endpoints test openrouter
```

This hits the OpenRouter `/models` API to confirm connectivity and authentication.

---

## Configuration Reference

### config.toml endpoint format

Endpoints live under `[[llm_endpoints.endpoints]]` in `.workgraph/config.toml`:

```toml
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
model = "anthropic/claude-sonnet-4-latest"
api_key_file = "~/.config/openrouter/api_key"
is_default = false
```

Fields:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Display name (used with `wg endpoints test <name>`) |
| `provider` | Yes | Must be `"openrouter"` |
| `url` | No | Defaults to `https://openrouter.ai/api/v1` |
| `model` | No | Default model for this endpoint |
| `api_key` | No | Inline API key (prefer `api_key_file`) |
| `api_key_file` | No | Path to file containing the key (`~` expansion supported) |
| `is_default` | No | If `true`, this endpoint is used when provider is `"openrouter"` |

### CLI endpoint commands

```bash
# Add an endpoint
wg endpoints add openrouter --provider openrouter --api-key-file ~/.config/openrouter/api_key

# List configured endpoints
wg endpoints list

# Set as default
wg endpoints set-default openrouter

# Test connectivity
wg endpoints test openrouter

# Remove
wg endpoints remove openrouter
```

Use `--global` on `add`, `remove`, or `set-default` to target the global config (`~/.workgraph/config.toml`) instead of the project-local one.

### Environment variables

| Variable | Purpose |
|----------|---------|
| `OPENROUTER_API_KEY` | API key (highest priority for OpenRouter provider) |
| `OPENAI_API_KEY` | Fallback API key (checked if `OPENROUTER_API_KEY` is unset) |
| `WG_LLM_PROVIDER` | Override provider detection (set to `"openrouter"`) |
| `WG_ENDPOINT_URL` | Override the base URL for API requests |
| `WG_ENDPOINT_NAME` | Select a named endpoint from config |
| `OPENROUTER_BASE_URL` | Alternative base URL (fallback after `WG_ENDPOINT_URL`) |

These are also exposed to spawned agents so they inherit the provider context.

### Model ID format

OpenRouter uses the `provider/model` naming convention:

```
anthropic/claude-sonnet-4-latest
google/gemini-2.5-pro
deepseek/deepseek-chat-v3
meta-llama/llama-4-maverick
mistralai/mistral-large-latest
```

When a model string contains a `/`, workgraph automatically selects the OpenAI-compatible provider (which includes OpenRouter). You can also use OpenRouter-specific suffixed variants when available.

### API key resolution priority

When the provider is `"openrouter"` (or any OpenAI-compatible provider), the API key is resolved in this order:

1. `OPENROUTER_API_KEY` environment variable
2. `OPENAI_API_KEY` environment variable (fallback)
3. Matching endpoint entry in config (by name or provider)
4. `[native_executor]` section in config (legacy fallback)

---

## Agent Model Binding

### Creating agents with model preferences

Agents can declare a preferred model and provider:

```bash
wg agent create "OpenRouter Coder" \
  --role <role-hash> \
  --tradeoff <tradeoff-hash> \
  --model anthropic/claude-sonnet-4-latest \
  --provider openrouter \
  --capabilities coding,testing
```

View an agent's model binding:

```bash
wg agent show <agent-hash>
# Output includes:
#   Model: anthropic/claude-sonnet-4-latest (preferred)
#   Provider: openrouter (preferred)
```

### Model precedence chain

Models are selected in priority order when spawning an agent:

1. **Task model** (`wg add --model` or `wg edit --model`) — highest priority
2. **Agent preferred model** (set with `wg agent create --model`)
3. **Executor config model** (model field in the executor's config file)
4. **Coordinator model** (`coordinator.model` in config.toml, or `--model` on `wg service start`)
5. **Executor default** (if nothing else is set, no `--model` flag is passed)

### Example: composing an agent that always uses a specific model

```bash
# 1. Create a role for research tasks
wg role list   # find a suitable role hash

# 2. Create the agent with an explicit model + provider
wg agent create "DeepSeek Researcher" \
  --role <role-hash> \
  --tradeoff <tradeoff-hash> \
  --model deepseek/deepseek-chat-v3 \
  --provider openrouter

# 3. Assign it to a task
wg assign research-task <agent-hash>

# 4. When the coordinator spawns this agent, it uses deepseek/deepseek-chat-v3
#    via OpenRouter automatically
```

To override the agent's preference for a specific task, set the model on the task itself:

```bash
wg edit research-task --model anthropic/claude-sonnet-4-latest
```

Task-level model always wins.

---

## Troubleshooting

### Common errors and fixes

**"Failed to initialize OpenAI-compatible client"**
- No API key found. Set `OPENROUTER_API_KEY` or configure an endpoint with `wg endpoints add`.

**401 Unauthorized**
- API key is invalid or expired. Verify at [openrouter.ai/keys](https://openrouter.ai/keys).
- Check that the key is being resolved correctly. The resolution chain checks env vars first, then endpoint config.

**404 Not Found / Model not available**
- The model ID may be wrong. Use `wg models search <query>` to find the correct ID.
- Some models may be temporarily unavailable on OpenRouter.

**Timeout or connection errors**
- Check your network connectivity.
- Verify the endpoint URL: `wg endpoints list` should show `https://openrouter.ai/api/v1`.
- Try `wg endpoints test openrouter` to isolate the issue.

**Wrong model being used**
- Check the precedence chain. Task model overrides agent model, which overrides coordinator model.
- Run `wg show <task-id>` to see the resolved model for a task.
- Run `wg agent show <agent-hash>` to verify the agent's preferred model.

### How to test connectivity

```bash
wg endpoints test openrouter
```

This sends a request to the `/models` endpoint and confirms that:
- The URL is reachable
- The API key is valid
- The endpoint responds correctly

### Streaming vs non-streaming mode

Workgraph uses **streaming mode** by default for all OpenAI-compatible providers, including OpenRouter. Streaming provides:
- Real-time output as tokens are generated
- Usage statistics in the final stream chunk (via `stream_options.include_usage`)

Streaming is always enabled and does not need to be configured. OpenRouter supports streaming for all models.

OpenRouter also supports auto-caching for Anthropic and Gemini models via `cache_control`, which workgraph enables automatically to reduce costs on repeated prompts.

---

## Available Models

OpenRouter aggregates models from many providers. Browse the full list at:

**[openrouter.ai/models](https://openrouter.ai/models)**

### Naming convention

Models follow the `provider/model-name` pattern:

| Provider | Example model ID |
|----------|-----------------|
| Anthropic | `anthropic/claude-sonnet-4-latest` |
| Google | `google/gemini-2.5-pro` |
| DeepSeek | `deepseek/deepseek-chat-v3` |
| Meta | `meta-llama/llama-4-maverick` |
| Mistral | `mistralai/mistral-large-latest` |
| OpenAI | `openai/gpt-4o` |

### Searching models from the CLI

Workgraph includes built-in model browsing:

```bash
# Search by name or description
wg models search "claude"
wg models search "deepseek"

# Filter to models with tool use support
wg models search "claude" --tools

# List all remote models
wg models remote

# List locally registered models
wg models list
wg models list --tier frontier
```

### Model tiers

When using workgraph's model selection (e.g., `--model haiku`), short tier names are resolved to full model IDs. For OpenRouter models, always use the full `provider/model` ID to avoid ambiguity.
