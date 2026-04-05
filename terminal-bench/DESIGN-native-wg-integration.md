# Design: Native WG Executor Integration for Terminal Bench

**Task:** design-tb-integration
**Date:** 2026-04-05
**Status:** Proposed
**Depends on:** [AUDIT-adapter-bypass-points.md](wg/AUDIT-adapter-bypass-points.md)

---

## 1. Problem Statement

The current TB adapter (`terminal-bench/wg/adapter.py`, 1730 lines) reimplements workgraph's entire agent execution loop using litellm. This means TB benchmarks measure the adapter's custom loop, not workgraph's actual execution machinery. Specifically:

1. **Agent loop** is a hand-rolled `for turn in range(max_turns)` loop with `litellm.acompletion()` — native wg uses coordinator → executor dispatch
2. **Tool schemas** are reimplemented as OpenAI function-call JSON — native wg provides tools via Claude Code / amplifier bundles
3. **Prompts** are six hand-crafted functions — native wg uses `src/agency/prompt.rs` composition
4. **Graph state** lives in `/tmp/tb-wg-XXXX/` and is destroyed — native wg uses persistent `.workgraph/`
5. **`wg service start`** is never called — the adapter *is* the agent loop

## 2. Decision: Host-with-Bridge Architecture

### Options Evaluated

| Option | Description | Pros | Cons |
|--------|-------------|------|------|
| **A: WG inside container** | Install `wg` binary + run service inside Harbor Docker containers | Full isolation per trial | Requires wg binary injection into Docker; breaks Harbor verifier; complex networking; service/coordinator doesn't work without git repo |
| **B: Host-with-bridge** | Run `wg service` on host; bridge Harbor's `env.exec()` to wg agents | Tests real wg execution; minimal Harbor changes; reuses existing wg infrastructure | Less isolation; need trial namespace; adapter becomes thinner shim |
| **C: Replace Harbor entirely** | Run TB tasks through `wg service` directly, skip Harbor | Tests exactly what ships; simplest wg integration | Loses Harbor's verification, scoring, container isolation; can't compare with non-wg agents |

**Decision: Option B — Host-with-Bridge**

Reasoning:
- Harbor provides irreplaceable value: container isolation for task work, automated verification/scoring, reproducible environments, comparison framework
- Running wg on host is what users actually do — wg manages worktrees, not containers
- The bridge is thin: Harbor only needs to call `wg` CLI commands; wg agents do the actual work
- The current adapter already runs wg commands on the host (`_exec_wg_cmd_host()`); we're extending this pattern rather than fighting it

## 3. Architecture

### 3.1 Control Flow Diagram

```
Harbor Runner
    │
    ▼
┌─────────────────────────────────────────────┐
│  WorkgraphAgent (adapter.py — THINNED)      │
│                                             │
│  setup():                                   │
│    1. Create trial graph dir (wg init)      │
│    2. Configure executor in trial graph     │
│    3. Create root task with instruction     │
│    4. Bootstrap agency (D/E conditions)     │
│                                             │
│  run():                                     │
│    1. wg service start --max-agents 1       │
│    2. Poll: wg show <root> until done/fail  │
│    3. Collect metrics from graph + logs     │
│    4. Populate AgentContext                 │
│                                             │
│  teardown():                                │
│    1. wg service stop                       │
│    2. Copy graph state for analysis         │
│    3. Cleanup temp dir                      │
└──────────────────┬──────────────────────────┘
                   │
        wg service start
                   │
                   ▼
┌─────────────────────────────────────────────┐
│  WG Coordinator (native, on host)           │
│                                             │
│  Dispatches ready tasks → executor          │
│  Monitors agent health                      │
│  Runs verify gates                          │
└──────────────────┬──────────────────────────┘
                   │
            spawn executor
                   │
                   ▼
┌─────────────────────────────────────────────┐
│  WG Agent (executor-dependent)              │
│                                             │
│  For claude executor:                       │
│    Claude Code CLI with full tool access    │
│    Works in host filesystem or worktree     │
│                                             │
│  For shell executor:                        │
│    bash -c <task.exec>                      │
│    Can invoke Harbor's env.exec() via       │
│    a bridge script                          │
│                                             │
│  For native executor:                       │
│    wg native-exec with OpenAI-compatible    │
│    API — can target the BENCHMARK_MODEL     │
└──────────────────┬──────────────────────────┘
                   │
         file/bash operations
                   │
                   ▼
┌─────────────────────────────────────────────┐
│  Work Environment                           │
│                                             │
│  Option 1 (claude executor):                │
│    Host filesystem / wg worktree            │
│    Claude Code's native Bash/Read/Write/etc │
│                                             │
│  Option 2 (native executor + Harbor env):   │
│    Harbor container via bridge script        │
│    env.exec() called from host side         │
└─────────────────────────────────────────────┘
```

### 3.2 Executor Selection: `native` for Benchmark, `claude` for Production Comparison

**Primary executor for TB: `native`**

The `native` executor (`wg native-exec`) is the right choice because:

1. **Model flexibility**: It uses the OpenAI-compatible API path, so it can target `openrouter:minimax/minimax-m2.7` (BENCHMARK_MODEL) directly via wg's endpoint/provider system. The `claude` executor is hardwired to Claude Code CLI which only supports Anthropic models.

2. **Benchmark model requirement**: TB requires all conditions use the same model for fair comparison. The native executor respects wg's model resolution hierarchy (`task.model > executor.model > coordinator.model`) and works with any OpenAI-compatible provider.

3. **Prompt composition**: The native executor receives prompts via `build_prompt()` in `src/service/executor.rs`, which already handles scope-based context assembly. The TB conditions' prompt variations map to wg's existing mechanisms:
   - Condition A: `context_scope=clean`, no agency, bare exec_mode
   - Condition B: `context_scope=task`, standard prompt
   - Condition C: `context_scope=task`, skill injection via role
   - Condition D: `context_scope=task`, agency identity (programmer/careful)
   - Condition E: `context_scope=graph`, agency identity (architect/thorough)
   - Condition F: `context_scope=graph`, full native features

4. **Tool parity**: The native executor provides tool bundles (via exec_mode) that map to the adapter's tool sets — bash, file ops, glob, grep, web search/fetch, and wg CLI access.

**Secondary comparison: `claude` executor**

For a separate "production parity" benchmark track, use the `claude` executor (Claude Code CLI) with the model override. This tests what real users experience but restricts the model to Anthropic's offerings.

### 3.3 File/Bash Tool Routing: Host Filesystem (Not Container Bridge)

**Decision: Tasks execute on the host filesystem, not inside Harbor containers.**

Rationale:
- Native wg agents work on the host (or in git worktrees). This is the production execution model.
- Harbor's `env.exec()` container routing exists to isolate agents from each other. WG already provides this via worktree isolation (`config.coordinator.worktree_isolation = true`).
- Trying to bridge native executor tools → Harbor container adds complexity with no benchmark benefit. We'd be measuring the bridge latency, not wg's actual tool performance.
- Harbor's *verification* (scoring the result) can still run in a container — only the agent's work happens on the host.

**Implication for task definitions:**
- TB task instructions must specify host-accessible paths (not container paths like `/tmp/project/`)
- Each trial gets its own working directory (wg worktree or temp dir) to prevent cross-trial contamination
- Harbor's verifier runs `verify_cmd` against the agent's working directory

### 3.4 Trial Isolation Strategy

Each trial gets an isolated graph + working directory:

```
/tmp/tb-trials/<run-id>/
├── <condition>-<task>-r<replica>/
│   ├── .workgraph/           # Trial-specific graph
│   │   ├── graph.jsonl
│   │   ├── config.toml       # Pre-configured executor + model
│   │   ├── executors/
│   │   │   └── native.toml   # Native executor with BENCHMARK_MODEL
│   │   └── service/
│   ├── work/                  # Working directory for agent
│   │   └── (task files created here)
│   └── logs/                  # Trial logs for analysis
│       ├── agent-output/
│       └── workgraph_state/   # Copied from .workgraph/ after trial
```

This preserves per-trial isolation without Docker containers for the wg layer.

## 4. Adapter Modification Plan

### 4.1 What adapter.py Becomes

The adapter shrinks from ~1730 lines to ~300 lines. It becomes a thin orchestrator:

```python
class WorkgraphAgent(BaseAgent):
    """Harbor adapter — delegates execution to native wg service."""

    async def setup(self, environment):
        # 1. Create trial directory
        # 2. wg init
        # 3. Write executor config (native.toml with BENCHMARK_MODEL)
        # 4. Write wg config (model, context_scope per condition)
        # 5. Bootstrap agency (conditions D, E)
        # 6. Create root task

    async def run(self, instruction, environment, context):
        # 1. wg service start --max-agents 1 --dir <trial-dir>
        # 2. Poll root task status every 2s
        # 3. On done/failed: wg service stop
        # 4. Read agent logs → populate context.n_input_tokens etc.
        # 5. Copy graph state for analysis
        # 6. Record structured trial log (keep TrialLogger)
```

### 4.2 Functions to Remove (Replaced by Native WG)

| Function | Lines | Replacement |
|----------|-------|-------------|
| `BASH_TOOL` through `WG_MSG_READ_TOOL` (all tool schemas) | 44–503 | Native executor provides tools via exec_mode bundles |
| `_exec_bash()`, `_exec_read_file()`, etc. | 510–663 | Native executor's built-in tools |
| `_exec_web_search()`, `_exec_web_fetch()` | 666–785 | Native executor's WebSearch/WebFetch |
| `_exec_wg_cmd_host()` (tool routing) | 788–814 | Keep for setup/teardown wg commands; remove from tool dispatch |
| `execute_tool()` | 817–875 | Entirely removed — native executor handles tools |
| `build_condition_*_prompt()` (all six) | 882–1229 | Native prompt composition via config + agency |
| Agent loop in `run()` | 1450–1541 | Replaced by `wg service start` + poll |

### 4.3 Functions to Keep/Adapt

| Function | Lines | Adaptation |
|----------|-------|------------|
| `_exec_wg_cmd_host()` | 788–814 | Keep — used for setup/teardown wg CLI calls (init, add, service start/stop) |
| `WorkgraphAgent.__init__()` | 1262–1294 | Simplify — remove litellm-specific params, keep condition + model |
| `WorkgraphAgent.setup()` | 1296–1349 | Rewrite — configure executor, write config, bootstrap agency |
| `WorkgraphAgent.run()` | 1351–1603 | Rewrite — start service, poll, collect metrics |
| `TrialLogger` | tb_logging.py | Keep — adapt to read metrics from wg agent logs instead of litellm responses |
| `ConditionXAgent` classes | 1642–1729 | Keep — they just set condition + model |

### 4.4 New Functions to Add

```python
async def _write_trial_executor_config(trial_dir, model, condition):
    """Write .workgraph/executors/native.toml for this trial."""
    # Configures native executor with BENCHMARK_MODEL
    # Sets exec_mode based on condition (bare/light/full)

async def _write_trial_wg_config(trial_dir, condition):
    """Write .workgraph/config.toml for this trial."""
    # Sets context_scope, model, disable auto_assign for A
    # Configures endpoint for OpenRouter

async def _poll_task_completion(trial_dir, task_id, timeout_secs, poll_interval=2.0):
    """Poll wg show until root task reaches terminal status."""
    # Returns (status, elapsed_secs)

async def _collect_agent_metrics(trial_dir, task_id):
    """Read agent logs to extract token counts, cost, tool calls."""
    # Parses stream.jsonl from agent output dir
    # Returns dict compatible with AgentContext fields
```

## 5. Condition Mapping to Native WG Mechanisms

| Condition | Current Adapter | Native WG Equivalent |
|-----------|----------------|----------------------|
| **A** (control) | Bare tools, no wg | `exec_mode=bare`, `context_scope=clean`, no agency, no wg tools. Agent gets only Bash + file tools. |
| **B** (treatment) | Full wg tools | `exec_mode=full`, `context_scope=task`, standard prompt with wg CLI access |
| **C** (treatment) | + skill injection | `exec_mode=full`, `context_scope=task`, role with skill component that includes planning phase guidance |
| **D** (treatment) | + agency identity | `exec_mode=full`, `context_scope=task`, agency bootstrap: role=programmer, tradeoff=careful, agent assigned |
| **E** (treatment) | + orchestrator | `exec_mode=full`, `context_scope=graph`, agency bootstrap: role=architect, tradeoff=thorough, agent assigned |
| **F** (treatment) | + full native | `exec_mode=full`, `context_scope=graph`, full native features, `--verify` on tasks, auto test discovery |

### 5.1 Condition A: The "No WG" Baseline

Condition A is the hardest to map because the current adapter gives the agent zero wg access. Options:

1. **`exec_mode=bare`**: The native executor's bare mode restricts tools to only `Bash(wg:*)` — but we want *no* wg tools at all.
2. **`exec_mode=bare` with no wg in PATH**: Set `PATH` in executor env to exclude wg binary. The agent gets Bash + file tools but `wg` commands fail.
3. **Shell executor with Harbor bridge**: Use `shell` executor that calls a Python bridge script which delegates to `env.exec()` — but this reintroduces the bridge complexity.
4. **Separate non-wg native executor config**: Configure native executor without wg guide injection and with a system prompt that doesn't mention wg.

**Recommendation: Option 4** — Create a separate executor config (`native-bare.toml`) that:
- Uses `exec_mode=bare` with custom allowedTools that exclude wg
- Injects a minimal system prompt matching Condition A's current prompt
- Does NOT inject wg guide content

The native executor (`wg native-exec`) can be configured to exclude wg tools from the bundle, making this a config-level change, not a code change. If the current `bare` exec_mode includes `Bash(wg:*)`, we need a new exec_mode `bare-no-wg` or a config flag `exclude_wg_tools = true`.

## 6. Trial Lifecycle

### 6.1 Setup Phase (in adapter `setup()`)

```
1. Create temp directory: /tmp/tb-trials/<run-id>/<condition>-<task>-r<replica>/
2. Initialize: wg init --dir <trial-dir>/.workgraph
3. Write executor config: <trial-dir>/.workgraph/executors/native.toml
   - type = "native"
   - model = BENCHMARK_MODEL
   - endpoint/provider for OpenRouter
4. Write wg config: <trial-dir>/.workgraph/config.toml
   - coordinator.model = BENCHMARK_MODEL
   - coordinator.max_agents = 1
   - coordinator.worktree_isolation = false  (trial is already isolated)
   - coordinator.auto_test_discovery = (true for F, false otherwise)
   - coordinator.context_scope = (per condition)
5. Bootstrap agency (conditions D, E):
   - wg agency init --dir <trial-dir>/.workgraph
   - wg agent create <name> --role <role> --tradeoff <tradeoff>
6. Create root task:
   - wg add "<title>" --id <root-task-id> --dir <trial-dir>/.workgraph
   - (D, E) wg assign <root-task-id> <agent-name>
   - (F) wg add with --verify
```

### 6.2 Execute Phase (in adapter `run()`)

```
1. Start service:
   wg service start --dir <trial-dir>/.workgraph --max-agents 1

2. Poll for completion:
   loop every 2s for up to <timeout>:
     status = wg show <root-task-id> --dir <trial-dir>/.workgraph --json
     if status in (done, failed, abandoned): break

3. Stop service:
   wg service stop --dir <trial-dir>/.workgraph
```

### 6.3 Teardown Phase (in adapter `run()`, after execution)

```
1. Collect metrics:
   - Read stream.jsonl from agent output dir → token counts, cost
   - Read graph.jsonl → task statuses, subtask count, tool call counts
   - Read agent logs → verification results, decomposition data

2. Populate AgentContext:
   context.n_input_tokens = <from stream.jsonl>
   context.n_output_tokens = <from stream.jsonl>
   context.cost_usd = <from stream.jsonl>
   context.metadata = { condition, turns, decomposition_tasks, ... }

3. Write structured trial log (TrialLogger)

4. Archive:
   shutil.copytree(<trial-dir>/.workgraph, <logs-dir>/workgraph_state)

5. Cleanup:
   shutil.rmtree(<trial-dir>)
```

## 7. Metric Collection from Native Executor

The native executor writes `stream.jsonl` with structured events. Key event types:

| Event Type | Fields | Maps to |
|------------|--------|---------|
| `type=init` | `task_id`, `model`, `exec_mode` | Trial metadata |
| `type=turn` | `usage.input_tokens`, `usage.output_tokens`, `usage.cost` | `context.n_input_tokens`, `context.n_output_tokens`, `context.cost_usd` |
| `type=tool_use` | `tool_name`, `tool_input`, `duration_ms` | Tool call counts, timing |
| `type=result` | `status`, `message` | Termination type |

The `TrialLogger` class (`tb_logging.py`) needs adaptation to parse these events instead of litellm response objects. The core interface stays the same.

## 8. Implementation Plan

### Phase 1: Infrastructure (implement-native-wg)
1. **Add `bare-no-wg` exec_mode** (or `exclude_wg_tools` config flag) — needed for Condition A
2. **Verify native executor works with OpenRouter** — ensure BENCHMARK_MODEL routes correctly
3. **Test trial-dir isolation** — `wg init` + `wg service start` in a temp directory

### Phase 2: Adapter Rewrite
1. **Strip adapter.py** — remove tool schemas, tool execution, litellm loop, prompt builders
2. **Add setup/teardown** — write executor config, wg config, create trial graph
3. **Add service orchestration** — start, poll, stop, collect metrics
4. **Adapt TrialLogger** — parse stream.jsonl instead of litellm responses

### Phase 3: Condition Parity
1. **Map each condition** to native wg config (exec_mode, context_scope, agency)
2. **Create role/tradeoff definitions** matching adapter's condition prompts
3. **Validate** — run each condition on a calibration task, compare with adapter results

### Phase 4: Integration Testing
1. **Run full TB suite** with native adapter on calibration tasks
2. **Compare metrics** — ensure token counts, tool usage patterns, pass rates are comparable
3. **Performance baseline** — native vs adapter execution time

## 9. Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Native executor doesn't support BENCHMARK_MODEL provider | Blocks all benchmarking | Verify OpenRouter endpoint config before starting. Native executor uses standard OpenAI-compatible client. |
| Prompt composition differences change benchmark results | Invalidates comparisons | Phase 3 calibration run. Accept that prompt differences are part of what we're measuring. |
| `wg service start` overhead (daemon startup) per trial | Slow benchmarks | Service starts in <1s. 2s poll interval is acceptable for TB's typical 30s–30min tasks. |
| Condition A tool parity | Wrong baseline | Test `bare-no-wg` mode to ensure agent has exactly bash + file tools. |
| Agent timeout/hang detection | Stuck trials | Use `agent_timeout` in wg config. Service already handles heartbeat timeouts. |
| Stream.jsonl format varies by executor type | Wrong metrics | Parse logic handles both claude and native formats (already handled in `src/graph.rs:596`). |

## 10. What This Design Does NOT Change

- **Harbor framework**: Still used for trial orchestration, verification, scoring
- **Task definitions**: Same calibration tasks (easy/medium/hard)
- **TrialLogger**: Kept for structured benchmark logging (adapted, not replaced)
- **Condition classes**: `ConditionAAgent` through `ConditionFAgent` remain as Harbor entry points
- **`tb_trial_runner.py`**: Separate path for project-graph trials — unchanged
- **`tb_collect_results.py`**: Fan-in analysis — may need minor updates to read native executor metrics

## 11. Open Questions for Implementation

1. **`bare-no-wg` implementation**: Should this be a new `exec_mode` value, a flag on the executor config, or achieved by manipulating the agent's PATH? The cleanest option is a new exec_mode that builds tool bundles without wg access.

2. **Token tracking from native executor**: Does the native executor's stream.jsonl reliably include cost data from OpenRouter? Need to verify that `openrouter:minimax/minimax-m2.7` response metadata includes usage/cost fields.

3. **Multi-agent trials**: Conditions E and F can spawn subtasks. With `max_agents=1`, subtasks queue. Should we allow `max_agents>1` for conditions E/F to test actual multi-agent coordination? Yes — this is a key wg feature being benchmarked.

4. **Graph persistence for analysis**: Currently the adapter copies the graph and destroys it. The native approach should do the same (isolate per trial, archive, cleanup). Should we also capture the full agent output logs?

---

*This design document is an artifact of task `design-tb-integration`. Implementation tasks should be created as subtasks.*
