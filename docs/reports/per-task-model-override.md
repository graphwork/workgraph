# Per-Task and Per-Coordinator Model Override: Investigation Report

Research report for task `investigate-per-task`.

---

## 1. Per-Task Model Override

### Status: WORKS (with fix applied)

**How it works:**

1. `wg add "task" --model <model>` stores the model on the `Task` struct (`src/graph.rs`, field `model: Option<String>`)
2. At spawn time, `resolve_model()` (`src/commands/spawn/execution.rs:1028-1038`) applies the priority hierarchy:
   ```
   task.model > agent.preferred_model > executor.model > coordinator.model
   ```
3. The resolved model then passes through `resolve_model_via_registry()` (`src/commands/spawn/execution.rs:1067-1119`) for alias resolution

### Model Resolution Paths

| Input form | Example | Behavior |
|---|---|---|
| Built-in tier alias | `--model opus` | Kept as-is for Claude CLI compatibility |
| Registered custom alias | `--model minimax-m2.5` | Resolved to full API model ID via `[[model_registry]]` |
| Full provider/model ID (registered) | `--model minimax/minimax-m2.5` | Matched by registry `model` field, gets provider/endpoint info |
| Full provider/model ID (unregistered) | `--model deepseek/deepseek-chat` | **NEW:** Passed through; `create_provider_ext()` auto-detects provider from `/` |
| Unknown short alias | `--model unknown-alias` | Error: must register first |

### Bug Fixed

**Before:** Task-specified models containing `/` (like `deepseek/deepseek-chat`) that weren't in the config registry caused a hard error at spawn time, even though the downstream native executor's `create_provider_ext()` (`src/executor/native/provider.rs:84-92`) handles them correctly via auto-detection.

**After:** Full model IDs (containing `/`) are allowed to pass through without registry registration. The auto-detection heuristic in `create_provider_ext()` routes `anthropic/*` to the anthropic provider and all other `*/` patterns to the openai provider (which works with OpenRouter).

**Additionally:** Registry lookup now also matches by the `model` field (not just the short `id`), so `--model minimax/minimax-m2.5` will find a registry entry with `model = "minimax/minimax-m2.5"` even if its `id` is `minimax-m2.5`.

### Code path (end-to-end)

```
wg add --model "deepseek/deepseek-chat"
  → task.model = "deepseek/deepseek-chat"              (graph.rs)
  → resolve_model() returns task.model (highest priority) (execution.rs:1028-1038)
  → resolve_model_via_registry():                        (execution.rs:1067-1119)
    → registry_lookup("deepseek/deepseek-chat") → None
    → model_field_lookup → None (if not registered)
    → contains('/') → pass through unchanged
  → build_inner_command() for native executor:           (execution.rs:767-808)
    → --model "deepseek/deepseek-chat" passed to wg native-exec
  → create_provider_ext():                               (provider.rs:84-92)
    → model.contains('/') → provider = "openai"
    → routes to OpenRouter endpoint
```

---

## 2. Per-Coordinator Model Override

### Status: WORKS (already implemented)

**Mechanism 1: `wg service start --model X`** (CLI flag, `src/cli.rs:2946-2948`)
- Overrides `config.coordinator.model` for the daemon session
- Passed through to every coordinator tick and used as the fallback model for all spawned agents
- The model flows: CLI → `run_service_start()` → daemon → `coordinator_tick()` → `spawn_agents_for_ready_tasks()` → `spawn_agent()`

**Mechanism 2: `wg service reload --model X`** (CLI flag, `src/cli.rs:2990-2992`)
- Updates the running daemon's model without restart
- Sent as IPC `Reconfigure` request

**Mechanism 3: `config.coordinator.model`** (config.toml)
- Persistent default: `[coordinator] model = "opus"`
- Used when no CLI override is provided

### Per-Multi-Coordinator Model Override: NOT SUPPORTED

`wg service create-coordinator` only accepts `--name`. There is no mechanism to set a different model for individual coordinator instances within a daemon. All coordinators share the daemon's model setting.

**Recommendation:** If per-coordinator model override is needed, it would require adding a `--model` flag to `CreateCoordinator` IPC and storing the model on the coordinator state. This is a follow-up task, not critical for the current goal.

---

## 3. OpenRouter Model IDs with `/`

### Status: WORKS (with fix applied)

**The `/` character passes through correctly in all layers:**

1. **CLI parsing:** clap accepts strings with `/` in `--model` arguments (no escaping needed)
2. **Graph storage:** JSON serialization handles `/` without issue
3. **Registry resolution:** **Fixed** — full model IDs with `/` now pass through even when task-specified
4. **Native executor command:** Shell escaping via `shell_escape()` handles `/` correctly when building the `wg native-exec --model "deepseek/deepseek-chat"` command
5. **Provider auto-detection** (`provider.rs:84-92`):
   - `anthropic/` prefix → anthropic provider (strips prefix)
   - Any other `/` → openai provider (kept as-is, sent to OpenRouter)
6. **OpenAI client:** Sends model as-is in the API request body, OpenRouter resolves it

### Resolution flow for `deepseek/deepseek-chat`:

```
task.model = "deepseek/deepseek-chat"
→ provider auto-detected as "openai" (contains '/' but not 'anthropic/')
→ model kept as "deepseek/deepseek-chat" (not stripped)
→ endpoint resolved from llm_endpoints config (OpenRouter)
→ sent to https://openrouter.ai/api/v1/chat/completions with model: "deepseek/deepseek-chat"
```

---

## 4. Model Resolution Order (Verified)

### Complete resolution hierarchy

```
1. task.model                    # wg add --model X, wg edit --model X
2. agent.preferred_model         # agency agent identity (wg assign)
3. executor.model                # executor config file
4. coordinator.model             # wg service start --model X, or config.coordinator.model
```

After raw resolution, the result goes through **registry alias resolution**:

```
5. registry_lookup(model) by id  # [[model_registry]] entry matching by id
6. registry_lookup(model) by model field  # [[model_registry]] entry matching by model
7. If task-specified + contains '/': pass through  # full provider/model IDs
8. If task-specified + no '/': ERROR (register first)
9. If not task-specified: pass through unchanged
```

### Test coverage

10 unit tests in `src/commands/spawn/execution.rs` cover this hierarchy:
- `test_resolve_model_task_overrides_agent` — task wins over agent
- `test_resolve_model_agent_preferred_when_no_task_model` — agent wins over executor
- `test_resolve_model_executor_when_no_agent` — executor wins over coordinator
- `test_resolve_model_coordinator_fallback` — coordinator as last resort
- `test_resolve_model_none_when_all_empty` — all None returns None
- `test_registry_resolves_custom_alias_to_model_id` — custom alias → full model
- `test_registry_keeps_builtin_alias_unchanged` — haiku/sonnet/opus preserved
- `test_registry_errors_on_unknown_task_model` — short unknown alias errors
- `test_registry_full_model_id_passthrough_for_task` — `/` model IDs pass through
- `test_registry_lookup_by_model_field` — match by model field, not just id
- `test_registry_short_alias_still_errors_when_unknown` — short unknowns error
- `test_registry_truly_unknown_non_task_model_passes_through` — non-task unknowns pass

---

## Changes Made

### `src/commands/spawn/execution.rs`

1. **Registry lookup by model field** (line ~1081-1087): Added fallback lookup that searches registry entries by their `model` field (not just `id`), so `--model minimax/minimax-m2.5` finds an entry with `model = "minimax/minimax-m2.5"`.

2. **Full model ID passthrough** (line ~1099-1106): When a task specifies a model containing `/` that's not in the registry, it passes through instead of erroring. The native executor's provider auto-detection handles these correctly.

3. **Updated test** `test_registry_passes_through_non_task_model`: Updated expectations since `claude-opus-4-latest` now correctly matches the builtin opus entry's model field.

4. **New tests**: Added 4 new tests covering the new behaviors.

---

## Recommendations for Follow-up

1. **Per-multi-coordinator model**: Add `--model` to `wg service create-coordinator` if users need different models for different coordinator sessions.

2. **Config validation**: Add a warning at config validation time when `executor = "claude"` but model is a non-Anthropic OpenRouter model (which requires `executor = "native"`).

3. **Registry unification**: The dual-registry system (config.toml `[[model_registry]]` + `models.yaml`) is confusing. Consider merging them or auto-syncing.
