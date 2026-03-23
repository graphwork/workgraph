# Model, Endpoint, and API Key Management

How to configure which AI models workgraph uses, where to send requests, and how to manage API keys securely.

## Quick Start: I Have an API Key, How Do I Start?

### OpenRouter (recommended — access to all major models)

```bash
# 1. Initialize your project
wg init

# 2. Set your API key (stored securely, never in config files)
export OPENROUTER_API_KEY="sk-or-v1-your-key-here"

# 3. Start the service
wg service start
```

That's it. Workgraph ships with built-in defaults for popular models via OpenRouter.

**Want to be more explicit?** Add a named endpoint:

```bash
wg endpoints add openrouter --provider openrouter --default
wg config --set-key openrouter --file ~/.secrets/openrouter.key
wg key check  # optional: verify your key works
wg service start
```

### Direct Anthropic API

```bash
export ANTHROPIC_API_KEY="sk-ant-your-key-here"
wg init
wg service start
```

The Claude executor picks up `ANTHROPIC_API_KEY` automatically. No endpoint or model configuration needed.

### OpenAI

```bash
wg endpoints add openai --provider openai --default
export OPENAI_API_KEY="sk-your-key-here"
wg service start
```

### Local Models (Ollama)

```bash
# Make sure Ollama is running: ollama serve
wg endpoints add ollama --provider local --url http://localhost:11434/v1 --default
wg service start
```

No API key needed for local models.

---

## Concepts: How Models, Endpoints, and Keys Relate

```
┌─────────────────────────────────────────────────────────────┐
│                     Model Registry                          │
│  "What models exist and what do they cost?"                 │
│                                                             │
│  anthropic/claude-opus-4-6    frontier  $5.00/$25.00/MTok   │
│  anthropic/claude-sonnet-4-6  mid       $3.00/$15.00/MTok   │
│  openai/gpt-4o               mid       $2.50/$10.00/MTok   │
│  ...                                                        │
└─────────────┬───────────────────────────────────────────────┘
              │ "which model to use for this role?"
              ▼
┌─────────────────────────────────────────────────────────────┐
│                     Model Routing                           │
│  "Per-role model selection"                                 │
│                                                             │
│  default  → sonnet    (most tasks)                          │
│  triage   → haiku     (cheap, fast classification)          │
│  evaluator→ sonnet    (balanced quality)                    │
│  ...                                                        │
└─────────────┬───────────────────────────────────────────────┘
              │ "where to send the request?"
              ▼
┌─────────────────────────────────────────────────────────────┐
│                      Endpoints                              │
│  "Connection targets: URL + provider + auth"                │
│                                                             │
│  openrouter → https://openrouter.ai/api/v1  [key: ✓]       │
│  anthropic  → https://api.anthropic.com     [key: ✓]       │
│  ollama     → http://localhost:11434/v1     [no key]        │
└─────────────┬───────────────────────────────────────────────┘
              │ "authenticate with..."
              ▼
┌─────────────────────────────────────────────────────────────┐
│                      API Keys                               │
│  "Credentials resolved at runtime"                          │
│                                                             │
│  Priority: inline → key file → env var → provider fallback  │
│                                                             │
│  openrouter: from env OPENROUTER_API_KEY                    │
│  anthropic:  from file ~/.workgraph/keys/anthropic.key      │
└─────────────────────────────────────────────────────────────┘
```

**Model Registry** — A catalog of available models with metadata (cost, capabilities, tier). Workgraph ships with 13+ built-in models. You can add custom ones.

**Model Routing** — Maps dispatch roles (evaluator, triage, etc.) to specific models. Controls which model each type of agent uses.

**Endpoints** — Connection targets where API requests are sent. Each endpoint has a provider type, URL, and associated authentication.

**API Keys** — Credentials for authenticating with endpoints. Resolved at runtime from multiple sources with a defined priority order.

---

## CLI Reference

### `wg models` — Browse and Search Models

#### `wg models list`

Show all models in the local registry (built-in + custom).

```bash
# List all models
wg models list

# Filter by tier
wg models list --tier frontier

# JSON output
wg models list --json
```

Example output:

```
MODEL                               TIER          IN/1M      OUT/1M        CTX CAPABILITIES
----------------------------------------------------------------------------------------------------
anthropic/claude-haiku-4-5          budget        0.80       4.00       200k coding, analysis
anthropic/claude-opus-4-6           frontier      5.00      25.00         1M coding, analysis, creative, reasoning
anthropic/claude-sonnet-4-6 *       mid           3.00      15.00         1M coding, analysis, creative
openai/gpt-4o                       mid           2.50      10.00       128k coding, analysis, creative
google/gemini-2.5-pro               mid           1.25      10.00         1M coding, analysis, creative, reasoning
deepseek/deepseek-chat-v3           budget        0.30       0.88       164k coding, analysis
...

  * = default model (anthropic/claude-sonnet-4-6)
```

**Tiers:**
- **frontier** — Most capable models (opus, o3). Best for complex architecture, verification, novel composition.
- **mid** — Balanced cost/quality (sonnet, gpt-4o, gemini-2.5-pro). Good for implementation, analysis, general tasks.
- **budget** — Cheapest (haiku, gpt-4o-mini, gemini-flash). Good for triage, classification, simple edits.

#### `wg models search <query>`

Search models available on OpenRouter by name, ID, or description.

```bash
# Search for Claude models
wg models search claude

# Only models with tool use support
wg models search claude --tools

# Skip cache and fetch fresh data
wg models search gemini --no-cache

# Limit results
wg models search deepseek --limit 10
```

Requires an OpenRouter API key (uses the `/models` API).

#### `wg models remote`

List all models available on OpenRouter.

```bash
wg models remote
wg models remote --tools          # Only tool-capable models
wg models remote --limit 20      # Limit results
wg models remote --json           # JSON output
```

#### `wg models add <id>`

Add a custom model to the local registry.

```bash
# Add a model with cost info
wg models add "custom/my-model" \
  --provider custom \
  --cost-in 1.0 \
  --cost-out 5.0 \
  --tier mid \
  --context-window 64000 \
  --capability coding \
  --capability reasoning

# Add an OpenRouter model not in the defaults
wg models add "mistral/mistral-large" \
  --cost-in 2.0 \
  --cost-out 6.0 \
  --tier mid
```

**Options:**
| Flag | Description |
|------|-------------|
| `--provider <name>` | Provider name (default: openrouter) |
| `--cost-in <usd>` | Cost per 1M input tokens (required) |
| `--cost-out <usd>` | Cost per 1M output tokens (required) |
| `--tier <tier>` | Tier: frontier, mid, budget (default: mid) |
| `--context-window <tokens>` | Context window size (default: 128000) |
| `-c, --capability <cap>` | Capability tag (repeatable) |

#### `wg models set-default <id>`

Set the default model for the coordinator.

```bash
wg models set-default "anthropic/claude-sonnet-4-6"
```

The model must exist in the registry (built-in or custom).

#### `wg models init`

Create a `models.yaml` file with the built-in defaults. Normally not needed — the registry works without this file.

```bash
wg models init
```

---

### `wg endpoints` — Manage LLM Endpoints

Endpoints define where API requests are sent. Each endpoint has a name, provider type, URL, and optional API key.

#### `wg endpoints add <name>`

```bash
# Add an OpenRouter endpoint
wg endpoints add openrouter --provider openrouter --default

# Add a direct Anthropic endpoint
wg endpoints add anthropic --provider anthropic

# Add with an API key (prefer --api-key-file for security)
wg endpoints add myep --provider openai --api-key-file ~/.secrets/openai.key

# Add a local Ollama endpoint
wg endpoints add ollama --provider local --url http://localhost:11434/v1

# Set a default model for the endpoint
wg endpoints add openrouter --provider openrouter --model anthropic/claude-sonnet-4-6

# Add to global config (shared across all projects)
wg endpoints add openrouter --provider openrouter --global
```

**Options:**
| Flag | Description |
|------|-------------|
| `--provider <type>` | Provider: anthropic, openai, openrouter, local (default: anthropic) |
| `--url <url>` | API endpoint URL (defaults based on provider) |
| `--model <model>` | Default model for this endpoint |
| `--api-key <key>` | API key (prefer --api-key-file for security) |
| `--api-key-file <path>` | Path to a file containing the API key |
| `--default` | Set as the default endpoint |
| `--global` | Write to global config (~/.workgraph/config.toml) |

**Default URLs by provider:**
| Provider | Default URL |
|----------|-------------|
| anthropic | `https://api.anthropic.com` |
| openai | `https://api.openai.com/v1` |
| openrouter | `https://openrouter.ai/api/v1` |
| local | `http://localhost:11434/v1` |

The first endpoint added is automatically set as default. Additional endpoints require `--default` to become default.

#### `wg endpoints list`

```bash
wg endpoints list
wg endpoints list --json
```

Example output:

```
Configured endpoints:

  openrouter (default)
    provider: openrouter
    url:      https://openrouter.ai/api/v1
    model:    (not set)
    api_key:  sk-****...ab12

  anthropic-direct
    provider: anthropic
    url:      https://api.anthropic.com
    model:    (not set)
    api_key:  (from file)
```

Keys are always masked in output.

#### `wg endpoints remove <name>`

```bash
wg endpoints remove openai
wg endpoints remove openai --global  # Remove from global config
```

If you remove the default endpoint, the next remaining endpoint is automatically promoted to default.

#### `wg endpoints set-default <name>`

```bash
wg endpoints set-default anthropic
```

#### `wg endpoints test <name>`

Test endpoint connectivity by hitting the provider's `/models` API.

```bash
wg endpoints test openrouter
```

Example output:

```
Testing endpoint 'openrouter' ...
  URL: https://openrouter.ai/api/v1/models
  Status: 200 OK
  Connectivity: OK
  Authentication: OK
```

If authentication fails:

```
Testing endpoint 'openrouter' ...
  URL: https://openrouter.ai/api/v1/models
  Status: 401
  Connectivity: OK
  Authentication: FAILED — check your API key
```

---

### `wg config` — Model Routing and Keys

The `wg config` command handles model routing, tier assignments, and API key configuration.

#### Model Routing

Control which model each dispatch role uses.

```bash
# Show all model routing assignments
wg config --models

# Set model for a specific role
wg config --set-model evaluator sonnet
wg config --set-model triage haiku
wg config --set-model task_agent opus

# Alternative key=value syntax
wg config --role-model evaluator=sonnet

# Set provider for a role
wg config --set-provider evaluator openrouter
wg config --role-provider evaluator=openrouter

# Set endpoint for a role (binds a named endpoint)
wg config --set-endpoint evaluator openrouter

# Set the default model for all roles
wg config --model sonnet
```

**Dispatch roles:**
| Role | Purpose | Typical Model |
|------|---------|---------------|
| `default` | Fallback for unset roles | sonnet |
| `task_agent` | Main implementation agents | sonnet |
| `evaluator` | Post-task evaluation scoring | sonnet |
| `triage` | Dead-agent summarization | haiku |
| `assigner` | Agent identity assignment | haiku |
| `evolver` | Agency evolution | sonnet |
| `creator` | Novel agent composition | opus |
| `flip_inference` | FLIP prompt reconstruction | sonnet |
| `flip_comparison` | FLIP similarity scoring | haiku |
| `verification` | FLIP-triggered verification | opus |

#### Model Registry (config.toml)

Manage the model registry entries in `config.toml` (used for cost tracking, tier resolution, and dispatch).

```bash
# Show all registry entries
wg config --registry

# Show tier → model assignments
wg config --tiers

# Set which model a tier uses
wg config --tier fast=haiku
wg config --tier standard=sonnet
wg config --tier premium=opus

# Add a model to the registry
wg config --registry-add \
  --id gpt-4o \
  --provider openai \
  --reg-model gpt-4o \
  --reg-tier standard \
  --cost-input 2.5 \
  --cost-output 10.0 \
  --context-window 128000

# Remove a model from the registry
wg config --registry-remove gpt-4o
```

#### API Key Management

```bash
# Set API key file for a provider
wg config --set-key openrouter --file ~/.secrets/openrouter.key

# Check OpenRouter API key validity and credit status
wg config --check-key
```

The `--check-key` command validates that an API key is present and (for OpenRouter) checks credit balance.

---

## API Key Security

### How Keys Are Resolved

When workgraph needs to authenticate with an endpoint, it resolves the API key using this priority chain:

1. **Inline key** (`api_key` in config) — highest priority, but **not recommended** (can be committed to git)
2. **Key file** (`api_key_file` in config) — reads key from a file, supports `~` and relative paths
3. **Environment variable** — automatic fallback based on provider type

**Environment variable fallback by provider:**

| Provider | Environment Variables Checked |
|----------|-------------------------------|
| openrouter | `OPENROUTER_API_KEY`, then `OPENAI_API_KEY` |
| openai | `OPENAI_API_KEY` |
| anthropic | `ANTHROPIC_API_KEY` |
| local | (none needed) |

### Best Practices

**Do this:**
```bash
# Option A: Use environment variables (recommended for most setups)
export OPENROUTER_API_KEY="sk-or-v1-your-key"

# Option B: Use key files (good for multi-project setups)
echo "sk-or-v1-your-key" > ~/.secrets/openrouter.key
chmod 600 ~/.secrets/openrouter.key
wg config --set-key openrouter --file ~/.secrets/openrouter.key
```

**Avoid this:**
```bash
# Bad: inline key in config.toml (can be committed to git)
wg endpoints add openrouter --provider openrouter --api-key sk-or-v1-your-key
```

### What Gets Stored Where

| File | Contains Keys? | In Git? |
|------|---------------|---------|
| `.workgraph/config.toml` | May contain `api_key_file` paths | Yes (should be) |
| `~/.workgraph/config.toml` | May contain `api_key_file` paths | No (home dir) |
| `~/.secrets/*.key` | Yes (actual key values) | No (home dir) |
| Environment variables | Yes (at runtime) | No |

### Key Display

All commands that display key information use masking: `sk-****...ab12`. Full key values are never shown in CLI output, TUI, or logs.

---

## TUI Settings Panel

The TUI (`wg tui`) includes a Settings panel for managing models, endpoints, and keys visually.

### Accessing Settings

1. Launch the TUI: `wg tui`
2. Press `Tab` to cycle between tabs until you reach the **Settings** tab
3. Use `↑`/`↓` to navigate between settings entries
4. Press `Space` to collapse/expand sections

### Settings Sections

The Settings panel is organized into collapsible sections:

#### LLM Endpoints

Manage your API endpoints. Each endpoint shows its name, provider, URL, model, and key status.

- **Navigate** to an endpoint entry and press `Enter` to edit
- Press **`+`** to add a new endpoint (opens inline form)
- Press **`x`** to remove the selected endpoint
- Press **`t`** to test endpoint connectivity
- Press **`d`** to set the selected endpoint as default

When adding a new endpoint, fill in:
- Name (e.g., "openrouter")
- Provider (anthropic, openai, openrouter, local)
- URL (auto-filled based on provider)
- API Key (optional)

#### API Keys

Shows key status for each configured provider with indicators:
- `✓` — Key is present and resolved
- `✗` — Key is missing or unresolvable

Press `Enter` to edit key source. Press `c` to check key validity (live API call for OpenRouter).

#### Model Tiers

Shows which model each tier (fast, standard, premium) resolves to. Edit to remap tiers.

#### Model Routing

Shows per-role model assignments. Navigate to a role and press `Enter` to change its model. A dropdown presents available models from the registry.

#### Model Registry

Browse all registered models with their ID, provider, full model name, and tier. Press `+` to add a new model, `x` to remove, `Enter` to edit.

#### Service Settings

Coordinator configuration: max agents, poll interval, executor type, model.

#### TUI Settings

Visual preferences: time counters, chat history, edge colors, etc.

### Keyboard Shortcuts (Settings Tab)

| Key | Action |
|-----|--------|
| `↑`/`↓` | Navigate entries |
| `Enter` | Edit selected entry / confirm edit |
| `Esc` | Cancel edit |
| `Tab` | Cycle tabs |
| `Space` | Collapse/expand section |
| `+` | Add new endpoint or model (context-dependent) |
| `x` | Remove selected endpoint or model |
| `t` | Test selected endpoint |
| `d` | Set selected endpoint as default |

---

## Coordinator Conversation Examples

When using `wg tui` or the coordinator service, you can manage models through natural language.

### Setting Up a Provider

```
You:  Set up OpenRouter with my key sk-or-v1-abc123
Bot:  ✓ Added endpoint 'openrouter' [openrouter]
      ✓ Stored API key
      Ready to dispatch tasks via OpenRouter.
```

### Changing Models

```
You:  Use opus for all tasks
Bot:  Setting default model to opus...
      ✓ Updated coordinator model to opus

You:  Use haiku for triage to save money
Bot:  ✓ Set triage model to haiku

You:  What models am I using?
Bot:  Current model routing:
      • default: sonnet (anthropic)
      • triage: haiku (anthropic)
      • evaluator: sonnet (anthropic)
      ...
```

### Checking Status

```
You:  Are my API keys working?
Bot:  Checking keys...
      • openrouter: ✓ valid ($12.34 credits)
      • anthropic: ✓ valid (from env ANTHROPIC_API_KEY)

You:  List my endpoints
Bot:  Configured endpoints:
      • openrouter (default) — https://openrouter.ai/api/v1
      • anthropic — https://api.anthropic.com
```

### Adding Custom Models

```
You:  Add Mistral Large from OpenRouter
Bot:  ✓ Added mistral/mistral-large to registry (mid tier, $2.00/$6.00 per MTok)
      Use 'wg models set-default mistral/mistral-large' to make it default.
```

---

## Config File Reference

All model/endpoint/key configuration lives in `.workgraph/config.toml` (project-level) or `~/.workgraph/config.toml` (global). Project-level settings override global.

### Endpoints

```toml
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key_file = "~/.secrets/openrouter.key"
is_default = true

[[llm_endpoints.endpoints]]
name = "anthropic-direct"
provider = "anthropic"
# url defaults to https://api.anthropic.com
# key resolved from ANTHROPIC_API_KEY env var
```

### Model Routing

```toml
[models.default]
model = "sonnet"
provider = "anthropic"

[models.triage]
model = "haiku"

[models.evaluator]
model = "sonnet"

[models.verification]
model = "opus"
```

### Provider Selection (Native Executor)

The native executor auto-detects the provider from the model string:

| Model string format | Detected provider | Example |
|---|---|---|
| Bare name (no `/`) | `anthropic` | `claude-sonnet-4-20250514` |
| `anthropic/` prefix | `anthropic` (prefix stripped) | `anthropic/claude-sonnet-4-20250514` |
| Other `provider/model` | `openai`-compatible | `openai/gpt-4o`, `deepseek/deepseek-chat-v3` |

You can override auto-detection per role:

```toml
[models.triage]
model = "gpt-4o-mini"
provider = "openai"         # Force OpenAI-compatible provider

[models.evaluator]
model = "claude-sonnet-4-20250514"
provider = "anthropic"      # Explicit Anthropic (same as auto-detected)
```

Or globally via `[native_executor]` or environment variable:

```toml
[native_executor]
provider = "anthropic"      # Default provider for all requests
```

```bash
export WG_LLM_PROVIDER=openrouter   # Override via environment
```

**Resolution order:** per-role `provider` > `[native_executor]` provider > `WG_LLM_PROVIDER` env > model string heuristic.

### Model Registry Entries

```toml
[[model_registry]]
id = "gpt-4o"
provider = "openai"
model = "gpt-4o"
tier = "standard"
endpoint = "https://api.openai.com/v1"
context_window = 128000
cost_per_input_mtok = 2.5
cost_per_output_mtok = 10.0

[[model_registry]]
id = "local-llama"
provider = "local"
model = "llama3:latest"
tier = "fast"
endpoint = "http://localhost:11434/v1"
cost_per_input_mtok = 0.0
cost_per_output_mtok = 0.0
```

### Tier Assignments

```toml
[tiers]
fast = "haiku"
standard = "sonnet"
premium = "opus"
```

### Coordinator Model

```toml
[coordinator]
model = "opus"        # Model for the coordinator itself
executor = "claude"   # Executor type
```

---

## Common Configurations

### Cost-Conscious Setup

Use budget models for most roles, mid-tier only where quality matters:

```toml
[tiers]
fast = "haiku"
standard = "haiku"      # downgrade standard to budget
premium = "sonnet"      # downgrade premium to mid

[models.verification]
model = "opus"           # keep verification at full power
```

### Multi-Provider Setup

Route different roles through different providers:

```toml
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
is_default = true

[[llm_endpoints.endpoints]]
name = "anthropic"
provider = "anthropic"

[models.default]
model = "sonnet"
provider = "openrouter"

[models.verification]
model = "opus"
provider = "anthropic"     # use direct API for verification
```

### Local + Cloud Hybrid

Use local models for cheap tasks, cloud for important ones:

```toml
[[llm_endpoints.endpoints]]
name = "ollama"
provider = "local"
url = "http://localhost:11434/v1"

[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
is_default = true

[models.triage]
model = "llama3"
provider = "local"           # free triage via local model

[models.default]
model = "sonnet"
provider = "openrouter"      # cloud for real work
```

### Per-Task Model Override

When creating tasks, you can specify the model directly:

```bash
# Use opus for a specific complex task
wg add "Design new auth system" --model opus

# Use a specific provider for a task
wg add "Quick fix" --model haiku
```

Task-level model overrides take highest priority in the resolution chain:
`task.model > executor.model > coordinator/CLI model > role routing > tier default > agent.model`

---

## Troubleshooting

### "No API key found"

Check that your key is accessible:

```bash
# Verify environment variable is set
echo $OPENROUTER_API_KEY

# Or check endpoint configuration
wg endpoints list

# Test the endpoint
wg endpoints test openrouter
```

### "Model not found in registry"

The model ID must match exactly. Check available models:

```bash
wg models list                    # Local registry
wg models search "model-name"    # Search OpenRouter
```

### Endpoint test shows "Authentication: FAILED"

Your API key is invalid or expired:

```bash
# Check key status
wg config --check-key

# Re-set the key
export OPENROUTER_API_KEY="sk-or-v1-your-new-key"
```

### Tasks use wrong model

Check the resolution chain:

```bash
# See per-role routing
wg config --models

# See tier assignments
wg config --tiers

# See the full merged config
wg config --list
```

### "Connection refused" to local endpoint

Make sure your local model server is running:

```bash
# For Ollama
ollama serve

# Test the endpoint
wg endpoints test ollama
```
