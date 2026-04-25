# Model/Provider Dispatch Audit

Research document for task `research-audit-all`. Comprehensive audit of every
model/provider dispatch point in the workgraph codebase.

---

## 1. Complete Inventory of Every Dispatch Point

### 1.1 Task Agent Spawning (coordinator -> spawn -> execution)

| Dispatch Point | File:Line | Description |
|---|---|---|
| Coordinator task spawn | `src/commands/service/coordinator.rs:1883` | `spawn::spawn_agent(dir, &task.id, &effective_executor, None, model)` — spawns agent for ready tasks |
| Coordinator model param | `src/commands/service/coordinator.rs:2224` | `spawn_agents_for_ready_tasks(dir, &graph, executor, model, slots_available)` — passes coordinator model down |
| spawn_agent_inner model resolution | `src/commands/spawn/execution.rs:117-119` | `task_model.or_else(\|\| executor_config.executor.model.clone()).or_else(\|\| model.map(…))` — hierarchy: task > executor > coordinator |
| Model passed to CLI via `--model` | `src/commands/spawn/execution.rs:399-401,439-441,473-475,502-504,528-536,571-573` | Each executor branch (claude resume/bare/light/full, amplifier, native) conditionally adds `--model` flag |
| `WG_MODEL` env var set on spawn | `src/commands/spawn/execution.rs:226-228` | `cmd.env("WG_MODEL", m)` — agents receive their model via env var |
| `WG_EXECUTOR_TYPE` env var set | `src/commands/spawn/execution.rs:225` | `cmd.env("WG_EXECUTOR_TYPE", &settings.executor_type)` |

### 1.2 Coordinator Agent (persistent LLM session)

| Dispatch Point | File:Line | Description |
|---|---|---|
| Coordinator agent spawn | `src/commands/service/coordinator_agent.rs:430` | `spawn_claude_process(dir, model, logger)` |
| Claude `--model` flag | `src/commands/service/coordinator_agent.rs:1101-1103` | `cmd.args(["--model", m])` when model is Some |
| Model resolved in daemon | `src/commands/service/mod.rs:697,1135` | `cli_model.or_else(\|\| config.coordinator.model.clone())` |
| Hardcoded to `claude` binary | `src/commands/service/coordinator_agent.rs:1087-1098` | `Command::new("claude")` — always spawns claude CLI |

### 1.3 Evaluation (Standard Evaluator)

| Dispatch Point | File:Line | Description |
|---|---|---|
| Evaluator model resolution | `src/commands/evaluate.rs:261-265` | `evaluator_model.or(config.agency.evaluator_model).or(task.model).unwrap_or(config.agent.model)` |
| Auto-eval task creation | `src/commands/service/coordinator.rs:1356` | `model: config.agency.evaluator_model.clone()` set on eval task |
| Eval task inline spawn | `src/commands/service/coordinator.rs:1851` | `spawn_eval_inline(dir, &task.id, eval_model)` — uses task's model field |

### 1.4 FLIP Evaluation (2-phase)

| Dispatch Point | File:Line | Description |
|---|---|---|
| FLIP inference model | `src/commands/evaluate.rs:594-597` | `evaluator_model.or(config.agency.flip_inference_model).unwrap_or("sonnet")` — **hardcoded fallback "sonnet"** |
| FLIP comparison model | `src/commands/evaluate.rs:599-603` | `config.agency.flip_comparison_model.unwrap_or("haiku")` — **hardcoded fallback "haiku"** |
| FLIP verification task model | `src/commands/service/coordinator.rs:1559` | `model: Some(verification_model.clone())` — uses `config.agency.flip_verification_model` |
| FLIP verification default | `src/config.rs:674-675` | `fn default_flip_verification_model() -> String { "opus".to_string() }` — **hardcoded "opus"** |

### 1.5 Triage (Dead Agent Summarization)

| Dispatch Point | File:Line | Description |
|---|---|---|
| Triage model | `src/commands/service/triage.rs:406` | `config.agency.triage_model.as_deref().unwrap_or("haiku")` — **hardcoded fallback "haiku"** |
| Checkpoint summary model | `src/commands/service/coordinator.rs:2056` | `config.agency.triage_model.as_deref().unwrap_or("haiku")` — same, **hardcoded "haiku"** |
| Both invoke `claude` binary | `src/commands/service/triage.rs:414`, `src/commands/service/coordinator.rs:2078` | `Command::new("timeout").arg("claude")` — **hardcoded to claude CLI** |

### 1.6 Assigner

| Dispatch Point | File:Line | Description |
|---|---|---|
| Assigner task model | `src/commands/service/coordinator.rs:1133` | `model: config.agency.assigner_model.clone()` on auto-assign task |

### 1.7 Evolver

| Dispatch Point | File:Line | Description |
|---|---|---|
| Evolver model resolution | `src/commands/evolve/mod.rs:102-105` | `model.or(config.agency.evolver_model).unwrap_or(config.agent.model)` |

### 1.8 Agent Creator

| Dispatch Point | File:Line | Description |
|---|---|---|
| Creator model resolution | `src/commands/agency_create.rs:363-366` | `model.or(config.agency.creator_model).unwrap_or(config.agent.model)` |

### 1.9 Native Executor

| Dispatch Point | File:Line | Description |
|---|---|---|
| Default model constant | `src/commands/native_exec.rs:23` | `const DEFAULT_MODEL: &str = "claude-sonnet-4-latest-20250514"` — **hardcoded Anthropic model ID** |
| Model resolution | `src/commands/native_exec.rs:118-121` | `model.or(WG_MODEL env).unwrap_or(DEFAULT_MODEL)` |
| Provider resolution | `src/commands/native_exec.rs:32-54` | Resolves provider from: config `[native_executor].provider` > `WG_LLM_PROVIDER` env > heuristic (model contains `/` → openai, else anthropic) |
| API base URL | `src/commands/native_exec.rs:57-60` | `[native_executor].api_base` config — supports OpenAI-compatible endpoints |

### 1.10 Command Template (Legacy)

| Dispatch Point | File:Line | Description |
|---|---|---|
| Template default | `src/config.rs:1045-1046` | `"claude --model {model} --print \"{prompt}\""` — **hardcoded "claude" binary** |
| Template `{model}` substitution | `src/config.rs:1419` | `self.agent.command_template.replace("{model}", &self.agent.model)` |

### 1.11 Task-Level Model/Provider Fields

| Dispatch Point | File:Line | Description |
|---|---|---|
| Task.model field | `src/graph.rs:258` | `pub model: Option<String>` — per-task model override |
| Task.provider field | `src/graph.rs:261` | `pub provider: Option<String>` — per-task provider override (currently unused in dispatch) |
| `wg edit --model` | `src/commands/edit.rs:128` | `task.model = Some(new_model.to_string())` |
| `wg add --model` | `src/commands/add.rs` | Sets task.model from CLI flag |

### 1.12 Model Registry (Metadata Catalog)

| Dispatch Point | File:Line | Description |
|---|---|---|
| Registry with defaults | `src/models.rs:93-226` | 13 models hardcoded: Anthropic (3), OpenAI (3), Google (2), DeepSeek (2), Meta (2), Qwen (1) |
| Default provider | `src/models.rs:72-74` | `"openrouter"` — **hardcoded default provider for registry entries** |
| Registry load path | `src/models.rs:230` | `.workgraph/models.yaml` |

### 1.13 TUI Config Editor

| Dispatch Point | File:Line | Description |
|---|---|---|
| Model choice lists | `src/tui/viz_viewer/state.rs:4839,4982,5047-5113` | Hardcoded `vec!["opus", "sonnet", "haiku"]` for all model config fields in TUI |

---

## 2. Current Configuration Surface

### 2.1 Configurable Fields (in `.workgraph/config.toml`)

| Config Key | Section | Default | User-Settable | How |
|---|---|---|---|---|
| `agent.model` | `[agent]` | `"opus"` | Yes | `wg config --model X`, `wg setup`, edit config.toml |
| `agent.command_template` | `[agent]` | `"claude --model {model} ..."` | Yes | Edit config.toml |
| `agent.executor` | `[agent]` | `"claude"` | Yes | `wg config --executor X` |
| `coordinator.model` | `[coordinator]` | None | Yes | `wg config --coordinator-model X`, CLI `--model` |
| `coordinator.executor` | `[coordinator]` | `"claude"` | Yes | `wg config --coordinator-executor X` |
| `agency.assigner_model` | `[agency]` | None | Yes | `wg config --assigner-model X`, `wg setup` |
| `agency.evaluator_model` | `[agency]` | None | Yes | `wg config --evaluator-model X`, `wg setup` |
| `agency.evolver_model` | `[agency]` | None | Yes | `wg config --evolver-model X` |
| `agency.creator_model` | `[agency]` | None | Yes | `wg config --creator-model X` |
| `agency.triage_model` | `[agency]` | None (fallback: `"haiku"`) | Yes | `wg config --triage-model X` |
| `agency.flip_inference_model` | `[agency]` | None (fallback: `"sonnet"`) | Yes | Config file edit |
| `agency.flip_comparison_model` | `[agency]` | None (fallback: `"haiku"`) | Yes | Config file edit |
| `agency.flip_verification_model` | `[agency]` | `"opus"` | Yes | Config file edit |
| `models.default` | `[models]` | None | Yes | Config file / `wg config` |
| `models.<role>.model` | `[models.<role>]` | None | Yes | Config file / `wg config --role-model` |
| `models.<role>.provider` | `[models.<role>]` | None | Yes | Config file / `wg config --role-provider` |
| `native_executor.provider` | `[native_executor]` | None (heuristic) | Yes | Config file edit |
| `native_executor.api_base` | `[native_executor]` | None | Yes | Config file edit |

### 2.2 Per-Task Overrides

| Source | Scope | Set Via |
|---|---|---|
| `task.model` | Single task | `wg add --model`, `wg edit --model` |
| `task.provider` | Single task | `wg add --provider`, `wg edit --provider` |

### 2.3 Per-Executor Overrides

| Source | Scope | Set Via |
|---|---|---|
| `executor.model` | All tasks using that executor | `.workgraph/executors/<name>.toml` |

### 2.4 Environment Variables

| Env Var | Used By | Description |
|---|---|---|
| `WG_MODEL` | Spawned agents, native_exec | Set by spawn; native-exec reads as fallback |
| `WG_EXECUTOR_TYPE` | Spawned agents | Indicates executor type |
| `WG_LLM_PROVIDER` | native_exec | Override provider for native executor |

### 2.5 Hardcoded Values

| Value | Location | Description |
|---|---|---|
| `"opus"` | `src/config.rs:1034` | Default `agent.model` |
| `"opus"` | `src/config.rs:675` | Default `flip_verification_model` |
| `"sonnet"` | `src/commands/evaluate.rs:597` | FLIP inference fallback |
| `"haiku"` | `src/commands/evaluate.rs:603` | FLIP comparison fallback |
| `"haiku"` | `src/commands/service/triage.rs:406` | Triage fallback |
| `"haiku"` | `src/commands/service/coordinator.rs:2056` | Checkpoint summary fallback |
| `"claude-sonnet-4-latest-20250514"` | `src/commands/native_exec.rs:23` | Native executor default model |
| `"claude"` binary | `src/commands/service/triage.rs:414`, `coordinator.rs:2078`, `coordinator_agent.rs` | Hardcoded to claude CLI in triage, checkpoints, coordinator agent |
| `"openrouter"` | `src/models.rs:73` | Default provider for model registry entries |
| `"claude --model {model} ..."` | `src/config.rs:1046` | Default command template |
| `"opus","sonnet","haiku"` | `src/tui/viz_viewer/state.rs` (5 places) | TUI config editor model choices |

---

## 3. Provider Coupling

### 3.1 Hard Anthropic/Claude Coupling Points

1. **Triage calls**: `src/commands/service/triage.rs:412-420` and `src/commands/service/coordinator.rs:2076-2084` — both use `Command::new("timeout").arg("claude")`. The binary name `claude` is hardcoded. To support other providers, these would need to route through an executor abstraction.

2. **Coordinator agent**: `src/commands/service/coordinator_agent.rs:1087-1098` — spawns `Command::new("claude")` with Claude-specific flags (`--input-format stream-json`, `--output-format stream-json`). This is deeply coupled to Claude Code's streaming protocol.

3. **Spawn execution (claude branches)**: `src/commands/spawn/execution.rs:384-517` — all `"claude"` executor branches construct Claude-specific CLI flags (`--print`, `--verbose`, `--output-format stream-json`, `--dangerously-skip-permissions`, `--disallowedTools Agent`, `--allowedTools`, `--system-prompt`, `--resume`). These are all Claude Code API specifics.

4. **Default command template**: `src/config.rs:1046` — `"claude --model {model} --print \"{prompt}\""` assumes claude binary.

5. **`wg setup`**: `src/commands/setup.rs:224-226` — model choices limited to `["opus", "sonnet", "haiku"]`.

6. **TUI config editor**: `src/tui/viz_viewer/state.rs` (5 places) — model choice dropdowns hardcoded to `["opus", "sonnet", "haiku"]`.

7. **Native executor DEFAULT_MODEL**: `src/commands/native_exec.rs:23` — `"claude-sonnet-4-latest-20250514"` is a full Anthropic model ID.

### 3.2 Already Abstracted

1. **Executor registry**: `.workgraph/executors/<name>.toml` — supports custom executors with arbitrary commands. The `amplifier`, `native`, and `shell` executor types already provide non-claude paths.

2. **Native executor provider heuristic**: `src/commands/native_exec.rs:32-54` — supports `"anthropic"` and `"openai"` providers, plus custom `api_base`. The OpenAI-compatible client can hit any compatible API.

3. **Model registry**: `src/models.rs` — already includes models from multiple providers (Anthropic, OpenAI, Google, DeepSeek, Meta, Qwen) though all default to `"openrouter"` provider.

4. **`[models]` routing config**: `src/config.rs:377-641` — `DispatchRole` enum with `ModelRoutingConfig` and `resolve_model_for_role()` already exists. Supports per-role model+provider.

5. **Task.provider field**: `src/graph.rs:261` — exists on Task struct but is **not yet used** in any dispatch logic.

### 3.3 Gaps to Close

| Gap | Current State | Needed |
|---|---|---|
| Triage/checkpoint calls | Hardcoded `Command::new("claude")` | Route through executor or native client |
| Coordinator agent | Hardcoded Claude Code session | Abstract to support other providers |
| `task.provider` unused | Field exists but dispatch ignores it | Wire into `resolve_model_for_role` or spawn |
| TUI model choices | Hardcoded Anthropic trio | Dynamic from model registry |
| Setup wizard | Hardcoded Anthropic trio | Dynamic from model registry |
| FLIP/triage fallbacks | Hardcoded "sonnet"/"haiku" | Route through `resolve_model_for_role()` |

---

## 4. Proposed Dispatch Roles

The existing `DispatchRole` enum already covers the needed roles:

| Role | Purpose | Current Config Path | Default |
|---|---|---|---|
| `default` | Fallback for all roles | `[models.default]` | `agent.model` ("opus") |
| `task_agent` | Main work agents | `[models.task_agent]` | Falls to default |
| `evaluator` | Post-task scoring | `[models.evaluator]` / `agency.evaluator_model` | Falls to default |
| `flip_inference` | FLIP prompt reconstruction | `[models.flip_inference]` / `agency.flip_inference_model` | "sonnet" hardcoded |
| `flip_comparison` | FLIP similarity scoring | `[models.flip_comparison]` / `agency.flip_comparison_model` | "haiku" hardcoded |
| `assigner` | Agent assignment | `[models.assigner]` / `agency.assigner_model` | Falls to default |
| `evolver` | Agency evolution | `[models.evolver]` / `agency.evolver_model` | Falls to default |
| `verification` | FLIP-triggered re-verification | `[models.verification]` / `agency.flip_verification_model` | "opus" hardcoded |
| `triage` | Dead agent summarization | `[models.triage]` / `agency.triage_model` | "haiku" hardcoded |
| `creator` | Agent creation | `[models.creator]` / `agency.creator_model` | Falls to default |

**Recommended additions (none)**. The existing 10 roles cover all dispatch points. No new roles needed.

**Recommended changes:**
- Make `triage` and `checkpoint_summary` use `resolve_model_for_role(DispatchRole::Triage)` instead of reading `agency.triage_model` directly
- Make FLIP inference/comparison use `resolve_model_for_role()` instead of hardcoded fallbacks
- Set sensible tier-based defaults: triage/comparison → Budget tier, inference → Mid, verification → Frontier

---

## 5. Recommended Config Schema

The existing `[models]` section is already well-designed. The key change is to:
1. Make ALL dispatch points use `resolve_model_for_role()` consistently
2. Deprecate the legacy `agency.*_model` fields (keep for backward compat)
3. Wire `provider` through to all dispatch points

### Current schema (already in place, config.toml):

```toml
[agent]
model = "opus"                    # Global fallback model
executor = "claude"               # Global fallback executor

[coordinator]
model = "opus"                    # Override for coordinator-spawned agents
executor = "claude"

[models]
# Default model+provider for all roles
[models.default]
model = "opus"
provider = "anthropic"            # Optional

# Per-role overrides
[models.task_agent]
model = "sonnet"

[models.evaluator]
model = "haiku"
provider = "openrouter"

[models.flip_inference]
model = "sonnet"

[models.flip_comparison]
model = "haiku"

[models.triage]
model = "haiku"

[models.verification]
model = "opus"

[models.assigner]
model = "haiku"

[models.evolver]
model = "opus"

[models.creator]
model = "opus"

# Legacy (deprecated, overridden by [models.*])
[agency]
evaluator_model = "haiku"         # → use [models.evaluator] instead
assigner_model = "haiku"          # → use [models.assigner] instead
```

### Proposed additions:

```toml
[native_executor]
provider = "anthropic"            # Already exists
api_base = "https://..."          # Already exists
default_model = "claude-sonnet-4-latest-20250514"  # Replace hardcoded DEFAULT_MODEL
```

### Resolution order (already implemented, no change needed):
1. `models.<role>.model` — role-specific in [models]
2. Legacy `agency.<role>_model` — backward compatibility
3. `models.default.model` — default in [models]
4. `agent.model` — global fallback

### Provider resolution (needs implementation):
1. `models.<role>.provider` — role-specific
2. `models.default.provider` — default
3. `native_executor.provider` / heuristic — for native executor
4. Implicit "anthropic" — when using claude CLI executor

---

## 6. Implementation Plan

The implementation is broken into 4 subtasks with clear boundaries. All subtasks modify different files or aspects, enabling a clean pipeline.

### Subtask 1: Unify hardcoded model fallbacks through `resolve_model_for_role()`

**Scope:** Eliminate hardcoded model fallbacks in dispatch points.

Files touched:
- `src/commands/evaluate.rs` — FLIP inference/comparison fallbacks
- `src/commands/service/triage.rs` — triage model fallback
- `src/commands/service/coordinator.rs` — checkpoint summary, verification model
- `src/config.rs` — add default values for roles that currently have hardcoded fallbacks

Changes:
- Replace `config.agency.triage_model.as_deref().unwrap_or("haiku")` with `config.resolve_model_for_role(DispatchRole::Triage).model`
- Replace FLIP inference/comparison hardcoded fallbacks with `resolve_model_for_role()`
- Add default model values in `resolve_model_for_role()` for roles that need tier-based defaults (triage→budget, flip_inference→mid, flip_comparison→budget, verification→frontier)

### Subtask 2: Wire provider through to triage/checkpoint dispatch

**Scope:** Make triage and checkpoint calls route through the executor/provider abstraction instead of hardcoding `claude` binary.

Files touched:
- `src/commands/service/triage.rs` — replace hardcoded claude CLI call
- `src/commands/service/coordinator.rs` — replace hardcoded claude CLI checkpoint call

Changes:
- Create a utility function that dispatches a simple prompt to the appropriate CLI based on provider config
- Replace `Command::new("timeout").arg("claude")` with provider-aware dispatch
- Support at minimum: "claude" (claude CLI) and "native" (in-process) providers for these lightweight calls

### Subtask 3: Dynamic model choices in TUI and setup wizard

**Scope:** Replace hardcoded `["opus", "sonnet", "haiku"]` with dynamic lists from model registry.

Files touched:
- `src/tui/viz_viewer/state.rs` — TUI config editor dropdowns
- `src/commands/setup.rs` — setup wizard model choices

Changes:
- Load model registry and extract model names for choice lists
- Group by tier for the setup wizard (frontier/mid/budget)
- Fallback to hardcoded list if registry can't be loaded

### Subtask 4: Deprecation warnings for legacy `agency.*_model` fields

**Scope:** Add deprecation notices and migration path for legacy config.

Files touched:
- `src/config.rs` — add deprecation warnings on load when legacy fields are set
- `src/commands/config_cmd.rs` — add `--role-model` and `--role-provider` flags that set `[models.<role>]`

Changes:
- When `agency.evaluator_model` etc. are set, emit a one-time warning suggesting `[models.evaluator]`
- Add `wg config --role-model evaluator=haiku` as the preferred way to set per-role models
- Update `wg config show` to display the unified `[models]` view
