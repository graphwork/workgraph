# Design: Model, Endpoint, and API Key Management UX

**Task:** design-model-endpoint  
**Date:** 2026-03-17  
**Status:** Proposed  

## Table of Contents

1. [Current State](#current-state)
2. [Design Goals](#design-goals)
3. [CLI Command Spec](#cli-command-spec)
4. [TUI Settings Panel](#tui-settings-panel)
5. [Coordinator Conversation UX](#coordinator-conversation-ux)
6. [Config Schema Changes](#config-schema-changes)
7. [Security Model for Keys](#security-model-for-keys)
8. [New User Onboarding Flow](#new-user-onboarding-flow)
9. [Migration & Backward Compatibility](#migration--backward-compatibility)

---

## Current State

The codebase has **three partially overlapping systems** for model management:

| System | Storage | Purpose | Status |
|--------|---------|---------|--------|
| `config.toml` `[models.*]` | `ModelRoutingConfig` | Per-role model+provider routing (evaluator→sonnet, triage→haiku) | Mature, well-integrated |
| `config.toml` `model_registry` | `ModelRegistryEntry` | Built-in + user short IDs with cost/tier data (haiku→claude-haiku-4-latest) | Mature, used by cost tracking |
| `models.yaml` | `ModelRegistry` / `ModelEntry` | Browsable catalog with capabilities, OpenRouter discovery | Newer, partially redundant |

**Endpoints** (`wg endpoints add/list/remove/set-default/test`) are fully implemented.

**Key management** is split:
- `wg config --set-key <provider> --file <path>` — sets `api_key_file` on provider endpoint
- `wg config --check-key` — validates OpenRouter key only
- `EndpointConfig` supports `api_key` (inline), `api_key_file`, and env var fallback
- No keyring/encrypted storage exists

**TUI** has a config panel (`ConfigPanelState`) with sections: Endpoints, ApiKeys, ModelTiers, ModelRouting, Service, etc. It supports adding/removing/testing endpoints inline.

### Key Gaps

1. **No `wg model` top-level command family** — model operations are split between `wg models` (catalog) and `wg config --registry-*` (routing registry)
2. **No `wg key` top-level command** — key ops buried in `wg config --set-key/--check-key`
3. **`wg config --check-key` only checks OpenRouter** — should be multi-provider
4. **No `--key-env` flag** — can't reference env var names explicitly (only implicit provider-based fallback)
5. **Two model registries** — `config.toml model_registry` and `models.yaml` are confusingly parallel
6. **No secure key storage** — keys stored in plain text config files or require external file management
7. **No guided setup flow** — new users must know the config.toml schema

---

## Design Goals

1. **Dead simple** — zero-to-running in under 5 minutes
2. **Three surfaces, one config** — CLI, TUI, coordinator all read/write the same state
3. **Secure by default** — keys never land in plain text in config files that get committed

---

## CLI Command Spec

### Design Principle: Top-Level Noun Commands

Move from buried `wg config --flag` operations to discoverable top-level commands:

```
wg endpoint ...    # Connection targets (URL + auth)
wg model ...       # Model catalog + routing
wg key ...         # API key management
```

The existing `wg endpoints` (plural) and `wg models` (plural) commands are kept as aliases for backward compatibility but the canonical forms are singular.

---

### `wg endpoint` — Connection Management

Wraps the existing `EndpointsCommands` with no behavioral changes, just promotion to singular noun:

```
wg endpoint add <name> [flags]
wg endpoint list
wg endpoint remove <name>
wg endpoint set-default <name>
wg endpoint test <name>
```

#### `wg endpoint add <name>`

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--provider` | string | `anthropic` | Provider type: anthropic, openai, openrouter, local |
| `--url` | string | (provider default) | API endpoint URL |
| `--model` | string | (none) | Default model for this endpoint |
| `--api-key` | string | (none) | API key (prefer `--api-key-file` or `wg key set`) |
| `--api-key-file` | string | (none) | Path to file containing API key |
| `--key-env` | string | (none) | **NEW:** Env var name to read key from (e.g. `OPENROUTER_API_KEY`) |
| `--default` | bool | false | Set as default endpoint |
| `--global` | bool | false | Write to `~/.workgraph/config.toml` |

**New: `--key-env` flag.** Stores the env var name in a new `api_key_env` field on `EndpointConfig`. Resolution priority becomes:

1. `api_key` (inline) — highest priority
2. `api_key_file` (from file)
3. `api_key_env` (explicit env var name)
4. Provider-based env var fallback (e.g., `ANTHROPIC_API_KEY`)

**Error messages:**
```
$ wg endpoint add myep
Added endpoint 'myep' [anthropic] (set as default)

$ wg endpoint add myep
Error: Endpoint 'myep' already exists. Remove it first or use a different name.

$ wg endpoint add myep --provider unknown
Error: Unknown provider 'unknown'. Supported: anthropic, openai, openrouter, local
```

#### `wg endpoint list`

Current behavior preserved. Output:

```
Configured endpoints:

  openrouter (default)
    provider: openrouter
    url:      https://openrouter.ai/api/v1
    model:    (not set)
    api_key:  sk-****...ab12
    key_env:  OPENROUTER_API_KEY

  anthropic-direct
    provider: anthropic
    url:      https://api.anthropic.com
    model:    (not set)
    api_key:  (from env: ANTHROPIC_API_KEY) ✓
```

**New:** Shows `key_env` field if set, and a `✓`/`✗` indicator of whether the resolved key is present at runtime.

#### `wg endpoint test <name>`

Current behavior preserved. Tests connectivity by hitting `/models` API.

---

### `wg model` — Model Catalog & Routing

Unifies the two registries. `wg model` operates on the **config.toml `model_registry`** (the one used for cost tracking, tier resolution, and dispatch). The `models.yaml` system (`wg models` with plural) remains as a separate catalog for OpenRouter browsing/discovery and is not modified.

```
wg model list                  # Show config.toml registry + built-ins
wg model add <alias> [flags]   # Add/update a model in config.toml registry
wg model remove <alias>        # Remove from config.toml registry
wg model set-default <alias>   # Set default model for agent dispatch
wg model routing               # Show per-role model assignments
wg model set <role> <model>    # Set model for a dispatch role
```

#### `wg model list`

Wraps existing `show_registry`. Shows the effective registry (built-in + user):

```
  ID           PROVIDER     MODEL                          TIER       COST (in/out per MTok)
  -----------------------------------------------------------------------------------------
  haiku        anthropic    claude-haiku-4-latest      fast       $0.25/$1.25
  sonnet       anthropic    claude-sonnet-4-latest       standard   $3.00/$15.00
  opus         anthropic    claude-opus-4-latest                premium    $15.00/$75.00
* gpt-4o       openai       gpt-4o                         standard   $2.50/$10.00

  * = default model
```

| Flag | Type | Description |
|------|------|-------------|
| `--tier` | string | Filter by tier (fast, standard, premium) |
| `--json` | bool | JSON output |

#### `wg model add <alias>`

Wraps existing `add_registry_entry` with a simplified interface:

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--provider` | string | (required) | Provider: anthropic, openai, openrouter, local |
| `--model-id` | string | (alias) | Full API model identifier (defaults to alias if omitted) |
| `--tier` | string | `standard` | Quality tier: fast, standard, premium |
| `--endpoint` | string | (none) | Named endpoint to use for this model |
| `--context-window` | u64 | 0 | Context window in tokens |
| `--cost-in` | f64 | 0.0 | Cost per million input tokens (USD) |
| `--cost-out` | f64 | 0.0 | Cost per million output tokens (USD) |
| `--global` | bool | false | Write to global config |

**Examples:**
```
$ wg model add gpt-4o --provider openai --tier standard --cost-in 2.5 --cost-out 10
Added registry entry: gpt-4o
  gpt-4o / openai / gpt-4o (tier: standard)

$ wg model add claude-via-openrouter --provider openrouter \
    --model-id anthropic/claude-sonnet-4-latest --endpoint openrouter --tier standard
Added registry entry: claude-via-openrouter
  claude-via-openrouter / openrouter / anthropic/claude-sonnet-4-latest (tier: standard)
```

**Error messages:**
```
$ wg model remove haiku
Error: 'haiku' is a built-in registry entry and cannot be removed.
To override it, add a custom entry with the same ID.
```

#### `wg model set-default <alias>`

Sets `models.default.model` to the given alias:

```
$ wg model set-default gpt-4o
Set default model to: gpt-4o
```

This updates `[models.default].model` in config.toml. The alias must exist in the effective registry.

#### `wg model routing`

Wraps existing `show_model_routing`:

```
Model Routing
  ROLE            TIER       MODEL          PROVIDER       ENDPOINT   SOURCE
  default         premium    opus           anthropic                 [models.default]
  evaluator       standard   sonnet         anthropic                 [models.evaluator]
  triage          fast       haiku          anthropic                 [models.triage]
  ...

Use 'wg model set <role> <model>' to override a role.
```

#### `wg model set <role> <model>`

Shortcut for `wg config --set-model`:

```
$ wg model set evaluator gpt-4o
Set models.evaluator.model = "gpt-4o"
```

| Flag | Type | Description |
|------|------|-------------|
| `--provider` | string | Also set provider for this role |
| `--endpoint` | string | Also set endpoint for this role |
| `--tier` | string | Set tier override instead of direct model |

---

### `wg key` — API Key Management

New top-level command family for key operations.

```
wg key set <provider> [flags]   # Configure API key for a provider
wg key check [provider]         # Validate key + show status
wg key list                     # Show key status for all providers
```

#### `wg key set <provider>`

| Flag | Type | Description |
|------|------|-------------|
| `--env` | string | Reference an environment variable by name |
| `--file` | string | Path to a file containing the key |
| `--value` | string | Store key directly (writes to `api_key_file` in `~/.workgraph/keys/<provider>.key`, NOT to config) |
| `--global` | bool | Apply to global config |

**Behavior by flag:**

- `--env VAR_NAME` → Sets `api_key_env = "VAR_NAME"` on the provider's endpoint config
- `--file /path/to/key` → Sets `api_key_file = "/path/to/key"` on the provider's endpoint config
- `--value sk-xxx` → Writes key to `~/.workgraph/keys/<provider>.key` (chmod 600) and sets `api_key_file` pointing there. **Never writes the key value into config.toml.**

If no endpoint exists for the provider, one is auto-created.

**Examples:**
```
$ wg key set openrouter --env OPENROUTER_API_KEY
Set API key for 'openrouter': using env var OPENROUTER_API_KEY

$ wg key set anthropic --file ~/.secrets/anthropic.key
Set API key for 'anthropic': using key file ~/.secrets/anthropic.key

$ wg key set openrouter --value sk-or-v1-abc123...
Stored key securely in ~/.workgraph/keys/openrouter.key (mode 600)
Set API key for 'openrouter': using key file ~/.workgraph/keys/openrouter.key
```

#### `wg key check [provider]`

Validates key availability and (where API supports it) checks credit/usage status.

```
$ wg key check
API Key Status:
  anthropic       ✓ (from env: ANTHROPIC_API_KEY)
  openrouter      ✓ (from file: ~/.workgraph/keys/openrouter.key)
  openai          ✗ not configured

$ wg key check openrouter
Provider: openrouter
  Key source: file (~/.workgraph/keys/openrouter.key)
  Key status: ✓ valid
  Credits:    $12.34 remaining
  Rate limit: 200 req/min
```

For providers with credit/status APIs (currently OpenRouter), makes a live API call. For others, verifies the key resolves to a non-empty string.

| Flag | Type | Description |
|------|------|-------------|
| `--json` | bool | JSON output |

#### `wg key list`

Shows key configuration status for all endpoints, without revealing key values:

```
  PROVIDER       SOURCE                              STATUS
  anthropic      env: ANTHROPIC_API_KEY               ✓ present
  openrouter     file: ~/.workgraph/keys/openrouter   ✓ present
  openai         (not configured)                     ✗ missing
```

---

## TUI Settings Panel

The existing `ConfigPanelState` with its sections (Endpoints, ApiKeys, ModelTiers, ModelRouting, etc.) already covers most needs. The design extends it:

### Section: LLM Endpoints (existing, enhanced)

```
┌─ LLM Endpoints ──────────────────────────────────────────┐
│                                                           │
│  ▸ openrouter (default)                            [✓ OK] │
│      provider: openrouter                                 │
│      url:      https://openrouter.ai/api/v1               │
│      api_key:  sk-****...ab12                             │
│      key_env:  OPENROUTER_API_KEY                         │
│                                                           │
│  ▸ anthropic-direct                                [✓ OK] │
│      provider: anthropic                                  │
│      url:      https://api.anthropic.com                  │
│      api_key:  (from env) ✓                               │
│                                                           │
│  [+] Add endpoint    [t] Test    [d] Set default          │
│  [x] Remove          [Enter] Edit                         │
│                                                           │
└───────────────────────────────────────────────────────────┘
```

**Changes from current:**
- Add `key_env` field display
- Add key status indicator (`✓`/`✗`) inline with each endpoint
- Test result badges (`[✓ OK]`, `[✗ FAIL]`, `[? untested]`) — already partially implemented via `EndpointTestStatus`

### Section: API Keys (existing, enhanced)

```
┌─ API Keys ────────────────────────────────────────────────┐
│                                                           │
│  anthropic       env: ANTHROPIC_API_KEY           [✓]     │
│  openrouter      file: ~/.workgraph/keys/openro…  [✓]     │
│  openai          (not configured)                 [✗]     │
│                                                           │
│  [Enter] Edit source    [c] Check key (live)              │
│                                                           │
└───────────────────────────────────────────────────────────┘
```

**Changes from current:**
- Show resolved key source (env/file/inline)
- `[c]` keybinding triggers live key check (similar to `[t]` for endpoint test)
- Edit allows changing between env/file/value sources

### Section: Model Registry (new)

```
┌─ Model Registry ──────────────────────────────────────────┐
│                                                           │
│  ID           PROVIDER     MODEL                   TIER   │
│  ─────────────────────────────────────────────────────────│
│  haiku        anthropic    claude-haiku-4-latest        fast   │
│  sonnet       anthropic    claude-sonnet-4-latest       std    │
│  opus         anthropic    claude-opus-4-latest         prem   │
│▸ gpt-4o       openai       gpt-4o                  std   │
│                                                           │
│  [+] Add    [x] Remove    [Enter] Edit    [*] Default     │
│                                                           │
└───────────────────────────────────────────────────────────┘
```

This section replaces the current ModelTiers section with a more direct view. Inline editing opens a form similar to the existing new-endpoint form.

### Section: Model Routing (existing, enhanced)

```
┌─ Model Routing ───────────────────────────────────────────┐
│                                                           │
│  ROLE            MODEL      PROVIDER     SOURCE           │
│  ─────────────────────────────────────────────────────────│
│  default         opus       anthropic    [models.default] │
│  evaluator       sonnet     anthropic    [models.eval…]   │
│▸ triage          haiku      anthropic    [models.triage]  │
│  compactor       haiku      anthropic    [models.compa…]  │
│  ...                                                      │
│                                                           │
│  [Enter] Change model    [p] Change provider              │
│  [e] Change endpoint                                      │
│                                                           │
└───────────────────────────────────────────────────────────┘
```

Inline model editing presents a list of available models from the registry as a `Choice` selector.

### Navigation

Sections maintain the existing collapse/expand behavior (`[Tab]` to cycle, `[Space]` to collapse). The new Model Registry section is inserted between the existing API Keys and Model Routing sections in `ConfigSection`:

```
Endpoints → ApiKeys → ModelRegistry → ModelRouting → Service → ...
```

---

## Coordinator Conversation UX

The coordinator interprets natural language requests and maps them to `wg` CLI commands. These patterns should be recognized:

### Endpoint Management

| User says | Coordinator executes |
|-----------|---------------------|
| "Add OpenRouter with key sk-or-xxx" | `wg endpoint add openrouter --provider openrouter --api-key sk-or-xxx --default` |
| "Add my Anthropic endpoint" | `wg endpoint add anthropic --provider anthropic --default` |
| "Set up a local Ollama endpoint" | `wg endpoint add local-ollama --provider local --url http://localhost:11434/v1` |
| "List my endpoints" | `wg endpoint list` |
| "Test the openrouter endpoint" | `wg endpoint test openrouter` |
| "Remove the openai endpoint" | `wg endpoint remove openai` |
| "Make anthropic the default endpoint" | `wg endpoint set-default anthropic` |

### Model Management

| User says | Coordinator executes |
|-----------|---------------------|
| "Add gpt-4o as a standard tier model" | `wg model add gpt-4o --provider openai --tier standard` |
| "Use claude-3.5-sonnet as default" | `wg model set-default sonnet` (or add + set-default if not in registry) |
| "What models are available?" | `wg model list` |
| "Set triage to use haiku" | `wg model set triage haiku` |
| "Route evaluator through openrouter" | `wg model set evaluator sonnet --endpoint openrouter` |
| "Show model routing" | `wg model routing` |

### Key Management

| User says | Coordinator executes |
|-----------|---------------------|
| "Set my OpenRouter key to sk-or-xxx" | `wg key set openrouter --value sk-or-xxx` |
| "Use OPENROUTER_API_KEY env var for openrouter" | `wg key set openrouter --env OPENROUTER_API_KEY` |
| "Check if my API keys are working" | `wg key check` |
| "Check openrouter credits" | `wg key check openrouter` |

### Compound Operations

| User says | Coordinator executes |
|-----------|---------------------|
| "Add OpenRouter with my key sk-or-xxx, use claude-3.5-sonnet as default" | 1. `wg endpoint add openrouter --provider openrouter` |
| | 2. `wg key set openrouter --value sk-or-xxx` |
| | 3. `wg model add claude-3.5-sonnet --provider openrouter --model-id anthropic/claude-3.5-sonnet --endpoint openrouter --tier standard` |
| | 4. `wg model set-default claude-3.5-sonnet` |

### Safety Rules

- If user provides a raw API key in conversation, the coordinator **must** use `wg key set --value` (which stores securely) rather than `--api-key` (which could write inline to config).
- After key operations, coordinator should run `wg key check <provider>` to validate.
- Coordinator should confirm destructive actions ("Remove endpoint 'openrouter'? This will affect X roles that reference it.").

---

## Config Schema Changes

### `EndpointConfig` — add `api_key_env` field

```toml
# Before (unchanged):
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key_file = "~/.workgraph/keys/openrouter.key"

# After (new optional field):
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"   # NEW: explicit env var reference
```

**Rust struct change:**

```rust
pub struct EndpointConfig {
    pub name: String,
    pub provider: String,
    pub url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_file: Option<String>,
    pub api_key_env: Option<String>,      // NEW
    pub is_default: bool,
}
```

**Resolution priority update in `resolve_api_key()`:**

1. `api_key` (inline value)
2. `api_key_file` (read from file)
3. `api_key_env` (read named env var)
4. Provider-based env var fallback (`ANTHROPIC_API_KEY`, etc.)

### `ConfigSection` — add `ModelRegistry` variant

```rust
pub enum ConfigSection {
    Endpoints,
    ApiKeys,
    ModelRegistry,     // NEW — between ApiKeys and ModelRouting
    ModelRouting,
    Service,
    TuiSettings,
    AgentDefaults,
    Agency,
    Guardrails,
    Actions,
}
```

### No changes to `ModelRegistryEntry` or `ModelRoutingConfig`

These structures are already well-designed. The new CLI commands wrap existing config operations.

### Backward Compatibility

- All changes are additive (`api_key_env` is `Option<String>` with `#[serde(default)]`)
- Existing configs parse unchanged
- Old commands (`wg endpoints`, `wg models`, `wg config --registry-*`, `wg config --set-key`) continue to work
- The singular forms (`wg endpoint`, `wg model`) are new aliases, not replacements

---

## Security Model for Keys

### Threat Model

1. **Accidental commit** — `.workgraph/config.toml` is tracked by git; keys in this file get committed
2. **Log exposure** — keys appearing in command output, logs, or error messages
3. **Process exposure** — keys visible in `/proc/<pid>/cmdline`

### Defense Layers

#### Layer 1: Never store keys inline in config.toml (default)

`wg key set --value <key>` writes to `~/.workgraph/keys/<provider>.key` with mode `0600`, then sets `api_key_file` pointing there. The key value itself never appears in any `.workgraph/` file.

`~/.workgraph/keys/` is in the user's home directory, not the project directory, so it's never in a git repo.

#### Layer 2: Key file location hierarchy

| Method | Storage Location | Git Risk | Recommended For |
|--------|-----------------|----------|-----------------|
| `--env VAR` | Process environment | None | CI/CD, containers |
| `--file <path>` | User-specified file | User's responsibility | Advanced users |
| `--value <key>` | `~/.workgraph/keys/<provider>.key` | None (home dir) | Interactive use |
| `--api-key <key>` | `api_key` field in config.toml | **HIGH** | Emergency only, deprecated warning |

#### Layer 3: .gitignore enforcement

`wg init` adds `keys/` to `.workgraph/.gitignore` (defense in depth — keys are stored in `~/.workgraph/keys/`, not project `.workgraph/keys/`, but belt-and-suspenders).

#### Layer 4: Key masking in output

All display functions use the existing `masked_key()` method: `sk-****...ab12`. This already works for `wg endpoint list` and TUI.

#### Layer 5: Deprecation warning for inline keys

When `EndpointConfig` has `api_key` set (inline), emit a one-time warning:

```
Warning: Endpoint 'openrouter' has an inline API key in config.toml.
This key may be committed to git. Run 'wg key set openrouter --value <key>'
to move it to secure storage.
```

#### Future: Keyring Integration (out of scope for v1)

A future iteration could integrate with OS keyrings (macOS Keychain, GNOME Keyring, Windows Credential Manager) via the `keyring` crate. The `api_key_env` mechanism provides the same security level for most use cases, so this is not a v1 requirement.

---

## New User Onboarding Flow

### Path 1: Anthropic Direct (simplest — env var already set)

```bash
$ wg init
Created .workgraph/ directory.
# User has ANTHROPIC_API_KEY already set

$ wg service start
# Just works — Claude executor uses ANTHROPIC_API_KEY by default
```

**Time: ~30 seconds.** No model/endpoint/key configuration needed.

### Path 2: OpenRouter (most common new setup)

```bash
# Step 1: Add the endpoint
$ wg endpoint add openrouter --provider openrouter
Added endpoint 'openrouter' [openrouter] (set as default)

# Step 2: Set the API key
$ wg key set openrouter --value sk-or-v1-abc123def456
Stored key securely in ~/.workgraph/keys/openrouter.key (mode 600)
Set API key for 'openrouter': using key file ~/.workgraph/keys/openrouter.key

# Step 3: Validate
$ wg key check openrouter
Provider: openrouter
  Key source: file (~/.workgraph/keys/openrouter.key)
  Key status: ✓ valid
  Credits:    $12.34 remaining

# Step 4 (optional): Add specific models
$ wg model add claude-3.5-sonnet --provider openrouter \
    --model-id anthropic/claude-3.5-sonnet --endpoint openrouter --tier standard
Added registry entry: claude-3.5-sonnet

$ wg model set-default claude-3.5-sonnet
Set default model to: claude-3.5-sonnet

# Done! Start working:
$ wg service start
```

**Time: ~2 minutes.**

### Path 3: Local model (Ollama)

```bash
# Step 1: Add endpoint
$ wg endpoint add ollama --provider local --url http://localhost:11434/v1
Added endpoint 'ollama' [local] (set as default)

# Step 2: Add model
$ wg model add llama3 --provider local --model-id llama3:latest --endpoint ollama --tier fast
Added registry entry: llama3

# Step 3: Set as default
$ wg model set-default llama3
Set default model to: llama3

# Done:
$ wg service start
```

**Time: ~1 minute** (no key needed for local).

### Path 4: Via Coordinator Conversation

```
User: Set up OpenRouter with my key sk-or-v1-abc123 and use Claude Sonnet as default

Coordinator: Setting up OpenRouter...
  ✓ Added endpoint 'openrouter' [openrouter]
  ✓ Stored API key securely
  ✓ Key validated — $12.34 credits remaining
  ✓ Added model 'claude-sonnet' (anthropic/claude-sonnet-4-latest via openrouter)
  ✓ Set as default model

Ready to go! Your tasks will now use Claude Sonnet via OpenRouter.
```

**Time: ~30 seconds.**

### Quick Setup Command (optional enhancement)

For the absolute simplest path, a guided wizard:

```bash
$ wg setup
Welcome to workgraph setup!

? Select your LLM provider:
  > Anthropic (direct API)
    OpenRouter (multi-model gateway)
    OpenAI
    Local (Ollama/vLLM)

? Enter your API key: sk-or-v1-abc123def456
  Stored securely in ~/.workgraph/keys/openrouter.key

? Testing connection... ✓ Connected (12.34 credits)

? Select default model:
  > anthropic/claude-sonnet-4-latest ($3.00/$15.00 per MTok)
    anthropic/claude-opus-4-latest ($15.00/$75.00 per MTok)
    openai/gpt-4o ($2.50/$10.00 per MTok)

Setup complete! Run 'wg service start' to begin.
```

This is a nice-to-have for v2, not required for the initial implementation.

---

## Migration & Backward Compatibility

### No breaking changes

| Old command | Still works? | New equivalent |
|-------------|-------------|----------------|
| `wg endpoints add/list/remove/...` | ✓ (alias) | `wg endpoint add/list/remove/...` |
| `wg models list/search/remote/add/...` | ✓ (unchanged) | (separate catalog system) |
| `wg config --registry` | ✓ | `wg model list` |
| `wg config --registry-add --id X ...` | ✓ | `wg model add X ...` |
| `wg config --registry-remove X` | ✓ | `wg model remove X` |
| `wg config --set-key P --file F` | ✓ | `wg key set P --file F` |
| `wg config --check-key` | ✓ | `wg key check openrouter` |
| `wg config --models` | ✓ | `wg model routing` |
| `wg config --set-model R M` | ✓ | `wg model set R M` |

### Config file migration

None needed. The only schema change is the additive `api_key_env` field with `#[serde(default)]`.

---

## Implementation Tasks (for downstream consumers)

### `implement-wg-endpoint` scope:
- Add `wg endpoint` as alias for existing `wg endpoints`
- Add `--key-env` flag to `wg endpoint add`
- Add `api_key_env: Option<String>` to `EndpointConfig`
- Update `resolve_api_key()` to check `api_key_env` between file and provider fallback
- Add key status indicator to `wg endpoint list` output

### `implement-wg-model` scope:
- Add `wg model` command family (list, add, remove, set-default, routing, set)
- `wg model list` wraps `show_registry()`
- `wg model add` wraps `add_registry_entry()` with simplified flags
- `wg model remove` wraps `remove_registry_entry()`
- `wg model set-default` updates `models.default.model` in config
- `wg model routing` wraps `show_model_routing()`
- `wg model set` wraps `update_model_routing()`

### `implement-wg-key` scope:
- Add `wg key` command family (set, check, list)
- `wg key set --value` writes to `~/.workgraph/keys/<provider>.key`
- `wg key set --env` sets `api_key_env` on endpoint
- `wg key set --file` sets `api_key_file` on endpoint
- `wg key check` validates keys across all/specific providers
- `wg key list` shows key status summary
- Ensure `~/.workgraph/keys/` directory is created with mode 700
- Deprecation warning when inline `api_key` is detected
