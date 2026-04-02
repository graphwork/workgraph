# Research: Current Config Structure and Setup Requirements

**Task:** research-current-config
**Date:** 2026-04-02

## 1. Config File Locations and Merge Behavior

| File | Path | Purpose |
|------|------|---------|
| Global config | `~/.workgraph/config.toml` | User-wide defaults |
| Local config | `.workgraph/config.toml` | Project-specific overrides |
| Matrix credentials | `~/.config/workgraph/matrix.toml` | Sensitive Matrix login (separate to avoid committing secrets) |

**Merge rule:** `Config::load_merged()` deep-merges global + local, with local values winning. `ConfigSource` tracks provenance (Global/Local/Default) for `wg config --list`.

## 2. Complete Config Schema (config.toml sections)

### `[agent]` — Agent Defaults
| Key | Type | Default | Required | Description |
|-----|------|---------|----------|-------------|
| `executor` | string | `"claude"` | Yes | Executor backend: "claude", "native", "amplifier", "shell" |
| `model` | string | `"claude:opus"` | Yes | Default model in provider:model format |
| `interval` | u64 | `10` | No | Sleep interval between agent iterations (seconds) |
| `command_template` | string | `'claude --model {model} --print "{prompt}"'` | No | Template for AI execution |
| `max_tasks` | Option<u32> | None | No | Max tasks per agent run |
| `heartbeat_timeout` | u64 | `5` | No | Heartbeat timeout in minutes |
| `reaper_grace_seconds` | u64 | `30` | No | Grace period before reaping dead PIDs |

### `[coordinator]` — Coordinator Settings
| Key | Type | Default | Required | Description |
|-----|------|---------|----------|-------------|
| `max_agents` | usize | `4` | **Setup wizard** | Max parallel agents |
| `interval` | u64 | `10` | No | Coordinator tick interval |
| `poll_interval` | u64 | `60` | No | Background safety-net poll interval |
| `executor` | Option<string> | None (auto-detect) | **Setup wizard** | Executor for spawned agents |
| `model` | Option<string> | None | **Setup wizard** | Model override for spawned agents |
| `provider` | Option<string> | None | Deprecated | Use provider:model format in model |
| `default_context_scope` | Option<string> | None | No | Default context scope |
| `agent_timeout` | string | `"30m"` | No | Hard timeout for agents |
| `settling_delay_ms` | u64 | `2000` | No | Event settling delay |
| `coordinator_agent` | bool | `true` | No | Spawn persistent LLM coordinator |
| `compactor_interval` | u32 | `5` | No | Compactor run frequency (ticks) |
| `compactor_ops_threshold` | usize | `100` | No | Ops threshold for compaction |
| `compaction_token_threshold` | u64 | `100000` | No | Token threshold for compaction |
| `compaction_threshold_ratio` | f64 | `0.8` | No | Context window fraction for compaction |
| `eval_frequency` | string | `"every_5"` | No | How often to evaluate coordinator turns |
| `worktree_isolation` | bool | `false` | No | Use git worktrees for isolation |
| `max_coordinators` | usize | `4` | No | Max simultaneous coordinators |
| `archive_retention_days` | u64 | `7` | No | Days to keep archived coordinators |
| `registry_refresh_interval` | u64 | `86400` | No | Model registry refresh interval (seconds) |
| `max_verify_failures` | u32 | `3` | No | Max verify failures before giving up |
| `max_spawn_failures` | u32 | `5` | No | Max spawn failures per task |

### `[project]` — Project Metadata
| Key | Type | Default | Required | Description |
|-----|------|---------|----------|-------------|
| `name` | Option<string> | None | No | Project name |
| `description` | Option<string> | None | No | Project description |
| `default_skills` | Vec<string> | `[]` | No | Default skills for new actors |

### `[agency]` — Agency (Evolutionary Identity) Settings
| Key | Type | Default | Required | Description |
|-----|------|---------|----------|-------------|
| `auto_evaluate` | bool | `false` | **Setup wizard** | Auto-evaluate on task completion |
| `auto_assign` | bool | `false` | **Setup wizard** | Auto-assign agent identity |
| `auto_place` | bool | `false` | No | Include placement in assignment |
| `auto_create` | bool | `false` | No | Auto-invoke creator agent |
| `auto_create_threshold` | u32 | `20` | No | Tasks since last creator invocation |
| `auto_triage` | bool | `false` | No | Triage dead agents before respawn |
| `auto_evolve` | bool | `false` | No | Enable evolution cycle |
| `flip_enabled` | bool | `false` | No | Enable FLIP scoring |
| `flip_verification_threshold` | Option<f64> | None | No | FLIP threshold for verification |
| `eval_gate_threshold` | Option<f64> | None | No | Eval gate rejection threshold |
| `eval_gate_all` | bool | `false` | No | Apply eval gate to all tasks |
| `triage_timeout` | Option<u64> | None | No | Triage call timeout (seconds) |
| `triage_max_log_bytes` | Option<usize> | None | No | Max log bytes for triage |
| `exploration_interval` | u32 | `20` | No | Force learning assignment every N tasks |
| `cache_population_threshold` | f64 | `0.8` | No | Score threshold for cache population |
| `ucb_exploration_constant` | f64 | `sqrt(2)` | No | UCB exploration constant |
| `novelty_bonus_multiplier` | f64 | `1.5` | No | Novelty bonus for UCB |
| `bizarre_ideation_interval` | u32 | `10` | No | Bizarre ideation frequency |
| `auto_assign_grace_seconds` | u64 | `10` | No | Grace period before auto-assign |
| `evolution_interval` | u64 | `7200` | No | Evolution cycle interval (seconds) |
| `evolution_threshold` | u32 | `10` | No | Min evaluations before evolving |
| `evolution_budget` | u32 | `5` | No | Max new primitives per evolution |
| `evolution_reactive_threshold` | f64 | `0.4` | No | Reactive evolution trigger threshold |
| `assigner_agent` | Option<string> | None | No | Fixed agent hash for assigner role |
| `evaluator_agent` | Option<string> | None | No | Fixed agent hash for evaluator role |
| `evolver_agent` | Option<string> | None | No | Fixed agent hash for evolver role |
| `creator_agent` | Option<string> | None | No | Fixed agent hash for creator role |
| `placer_agent` | Option<string> | None | No | Fixed agent hash for placer role |
| `retention_heuristics` | Option<string> | None | No | Prose policy for retention |

### `[llm_endpoints]` — LLM API Endpoints
```toml
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"          # "anthropic", "openai", "openrouter", "gemini", "ollama", "llamacpp", "vllm", "local"
url = "https://openrouter.ai/api/v1"
api_key = "sk-..."               # Direct key (avoid committing!)
api_key_file = "~/.openrouter.key"  # Read from file (supports ~ expansion)
api_key_env = "OPENROUTER_API_KEY"  # Read from named env var
is_default = true
```

**API key resolution priority:** `api_key` > `api_key_file` > `api_key_env` > provider-based env var fallback.

### `[models]` — Per-Role Model Routing
14 dispatch roles: `default`, `task_agent`, `evaluator`, `flip_inference`, `flip_comparison`, `assigner`, `evolver`, `verification`, `triage`, `creator`, `compactor`, `coordinator_eval`, `placer`, `chat_compactor`.

```toml
[models.default]
model = "claude:opus"

[models.task_agent]
model = "claude:opus"

[models.evaluator]
model = "claude:sonnet"
```

### `[tiers]` — Quality Tier Defaults
| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `fast` | Option<string> | None | Model ID for fast tier |
| `standard` | Option<string> | None | Model ID for standard tier |
| `premium` | Option<string> | None | Model ID for premium tier |

### `[[model_registry]]` — Model Registry Entries
```toml
[[model_registry]]
id = "deepseek-v3.2"
provider = "openrouter"
model = "deepseek/deepseek-v3.2"
tier = "fast"
endpoint = "openrouter"
context_window = 163840
max_output_tokens = 0
cost_per_input_mtok = 0.26
cost_per_output_mtok = 0.38
prompt_caching = false
cache_read_discount = 0.0
cache_write_premium = 0.0
descriptors = []
```

### `profile` — Provider Profile
Top-level string, e.g., `"anthropic"`, `"openrouter"`. Supplies tier defaults.

### Other Sections
- `[help]` — ordering: "usage"/"alphabetical"/"curated"
- `[log]` — rotation_threshold (bytes, default 10MB)
- `[replay]` — keep_done_threshold (0.0-1.0), snapshot_agent_output
- `[guardrails]` — max_child_tasks_per_agent, max_task_depth, max_triage_attempts
- `[viz]` — edge_color, animations
- `[tui]` — 15+ UI settings (layout, theme, timestamps, panel ratios, etc.)
- `[checkpoint]` — auto_interval_turns, auto_interval_mins, max_checkpoints, retry_context_tokens
- `[chat]` — max_file_size, max_messages, retention_days, compact_threshold

## 3. Environment Variables

### Set by coordinator/spawn for agents:
| Env Var | Set Where | Purpose |
|---------|-----------|---------|
| `WG_TASK_ID` | spawn/execution.rs:420 | Current task ID |
| `WG_AGENT_ID` | spawn/execution.rs:421 | Agent identity hash |
| `WG_EXECUTOR_TYPE` | spawn/execution.rs:422 | Executor type being used |
| `WG_MODEL` | spawn/execution.rs:426 | Model being used |
| `WG_ENDPOINT` | spawn/execution.rs:429 | Endpoint name |
| `WG_ENDPOINT_NAME` | spawn/execution.rs:430 | Endpoint name (duplicate) |
| `WG_LLM_PROVIDER` | spawn/execution.rs:433 | Provider name |
| `WG_ENDPOINT_URL` | spawn/execution.rs:436 | Endpoint API URL |
| `WG_API_KEY` | spawn/execution.rs:439 | Resolved API key |
| `WG_USER` | spawn/execution.rs:424 | User identity |
| `WG_MSG_FILE` | messages.rs:604 | Notification file path |

### Read from environment (user/system):
| Env Var | Read Where | Purpose |
|---------|------------|---------|
| `ANTHROPIC_API_KEY` | config.rs:432 | Anthropic API key fallback |
| `OPENROUTER_API_KEY` | config.rs:430 | OpenRouter API key fallback |
| `OPENAI_API_KEY` | config.rs:430-431 | OpenAI API key fallback (also OpenRouter fallback) |
| `WG_USER` | lib.rs:66 | User identity (fallback to `$USER`) |
| `WG_MODEL` | native_exec.rs:46 | Model override for native exec |
| `WG_LLM_PROVIDER` | native_exec.rs:94, provider.rs:109 | Provider override |
| `WG_ENDPOINT_URL` | native_exec.rs:101, provider.rs:141 | Endpoint URL override |
| `WG_API_KEY` | provider.rs:167 | API key override |
| `HOME` | Various | Home directory |
| `USER` | lib.rs:67 | Fallback for user identity |

## 4. External Tools That Can Be Auto-Detected

| Tool | Detection Method | Purpose |
|------|-----------------|---------|
| `claude` CLI | `which claude` | Default executor for Anthropic |
| `amplifier` | `which amplifier` | Alternative executor with delegation |
| `ollama` | `which ollama` + check `http://localhost:11434` | Local LLM inference |
| `tmux` | `which tmux` | Required for `wg server` |
| API key files | Check `~/.openrouter.key`, etc. | Pre-configured key files |
| Env vars | Check `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY` | API key availability |
| Git | `which git` | Required for worktree isolation |
| vLLM/llamacpp | Check `http://localhost:8000`, `http://localhost:8080` | Local inference servers |

## 5. Current `wg config` Subcommands

The `wg config` command supports these operations (from `config_cmd.rs`):

1. **`wg config --show`** / **`wg config`** — Display current merged configuration
2. **`wg config --init`** — Create default config.toml
3. **`wg config --list`** — Show all values with source annotations (global/local/default)
4. **`wg config --json`** — JSON output for show/list

Config setters via flags (all apply to `wg config` directly):
- `--executor`, `--model`, `--interval` (agent)
- `--max-agents`, `--max-coordinators`, `--coordinator-interval`, `--poll-interval` (coordinator)
- `--coordinator-executor`, `--coordinator-model`, `--coordinator-provider` (deprecated)
- `--auto-evaluate`, `--auto-assign`, `--auto-triage`, `--auto-place`, `--auto-create` (agency)
- `--assigner-agent`, `--evaluator-agent`, `--evolver-agent`, `--creator-agent` (agency agents)
- `--retention-heuristics`, `--triage-timeout`, `--triage-max-log-bytes` (agency)
- `--max-child-tasks`, `--max-task-depth` (guardrails)
- `--viz-edge-color` (viz)
- `--eval-gate-threshold`, `--eval-gate-all` (eval gate)
- `--flip-enabled`, `--flip-verification-threshold` (FLIP)
- `--chat-history`, `--chat-history-max`, `--tui-counters` (TUI)
- `--retry-context-tokens` (checkpoint)

Scope: `--global` / `--local` (default: local).

## 6. Current `wg setup` Wizard

Already exists at `src/commands/setup.rs`. Interactive wizard that:
1. Asks provider: Anthropic, OpenRouter, OpenAI, Local, Custom
2. Auto-sets executor based on provider
3. Provider-specific config (endpoint URL, API key file/env, model)
4. Validates API key by hitting `/models` endpoint
5. Offers to auto-discover and register models from API response
6. Asks about agency (auto_assign, auto_evaluate)
7. Asks about max_agents
8. Writes to global config (`~/.workgraph/config.toml`)
9. Configures `~/.claude/CLAUDE.md` with workgraph directives
10. Also configures project-level `CLAUDE.md`

Non-interactive mode via `--provider` flag with `--api-key-file`, `--api-key-env`, `--url`, `--model`, `--skip-validation`.

## 7. Values That Should Be Part of a Setup Wizard

### Essential (already in wizard):
- Provider selection (anthropic/openrouter/openai/local/custom)
- API key configuration (file, env var, or direct)
- API key validation
- Model selection (with auto-discovery)
- Executor selection (auto-detected from provider)
- Max parallel agents
- Agency enable/disable

### Missing from wizard but should be included:
- **`profile`** — Provider profile name (currently set but not prominently)
- **`[tiers]`** mapping — Only auto-set from discovered models, not explicitly prompted
- **`agent_timeout`** — Important operational setting, currently only settable via `wg config`
- **`worktree_isolation`** — Critical for multi-agent safety, not in wizard
- **`coordinator_agent`** — Whether to use persistent LLM coordinator
- **`default_context_scope`** — Affects agent context quality

### Gaps: Values set via env vars but should be in config:
- `WG_USER` — User identity. Falls back to `$USER` but has no config.toml equivalent. Could be added to `[project]` section.
- `WG_MODEL` / `WG_LLM_PROVIDER` / `WG_ENDPOINT_URL` / `WG_API_KEY` — These are set by the coordinator for spawned agents and read by `wg native-exec`. They serve as runtime overrides and don't need config equivalents, but there's no way to set per-agent model overrides in config outside the `[models]` routing system.

### Auto-detectable prerequisites:
| Prerequisite | Detection | Required? |
|-------------|-----------|-----------|
| `claude` CLI installed | `which claude` | Required if executor=claude |
| `ANTHROPIC_API_KEY` set | `env::var("ANTHROPIC_API_KEY")` | Required if using Anthropic directly |
| `OPENROUTER_API_KEY` set | `env::var("OPENROUTER_API_KEY")` | Required if using OpenRouter |
| API key file exists | `Path::new(file).exists()` | If api_key_file configured |
| API key validates | `validate_api_key()` | Recommended |
| `git` installed | `which git` | Required for worktree isolation |
| `tmux` installed | `which tmux` | Required for `wg server` |
| Local inference server | HTTP check `localhost:11434/8000/8080` | Required if executor=local |
| `.workgraph/` initialized | `dir.exists()` | Required for most operations |

## 8. Config Health Check / Validation

`Config::validate_config()` (config.rs:2933) performs these checks:
1. **executor-model-auto-route**: claude executor with non-Anthropic model → warning
2. **provider-model-mismatch**: non-Anthropic provider with Anthropic-only model alias → error
3. **unresolved-model-id**: model ID doesn't match registry and lacks `/` → warning
4. **registry-model-format**: registry entry model lacks `/` for non-Anthropic providers → warning
5. **missing-api-key-file**: referenced api_key_file doesn't exist → error
6. **empty-api-key-file**: referenced api_key_file is empty → error

Returns `ConfigValidation` with errors (block service start) and warnings (display but allow).
