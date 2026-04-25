# Research: WG Executor Invocation Path & LiteLLM Fallback Analysis

## Executive Summary

Agents running terminal-bench (TB) tasks were falling back to litellm/harbor instead of using wg's intended executor. **The root cause is architectural**: there are two completely separate execution paths, and which one fires depends on whether TB uses harbor's Docker runner or wg's native executor. The recent commit `fab4ae74` fixed a secondary issue — env var leakage from parent wg services — but the fundamental duality remains.

---

## 1. The Intended Model Execution Path

### 1.1 Config → Coordinator → Agent Spawn → Model Call

```
User config (.workgraph/config.toml)
  ├── coordinator.executor = "claude" | "amplifier" | "native" | "shell"
  ├── coordinator.model = "..."
  ├── models.task_agent.model = "..."
  └── llm_endpoints.* = { provider, url, api_key }
        │
        ▼
wg service start (daemon loop)
  └── coordinator_tick()                    [src/commands/service/coordinator.rs:3428]
        │
        ├── Phase 1-4.7: Graph maintenance, agency scaffolding
        │
        └── Phase 6: spawn_agents_for_ready_tasks()  [coordinator.rs:3019]
              │
              ├── Resolve executor per task:
              │   task.exec → "shell"
              │   agent_entity.effective_executor() → agent config
              │   coordinator.executor → default         [coordinator.rs:3159-3173]
              │
              ├── Resolve model per task:
              │   task.model → per-task override
              │   config.resolve_model_for_role(TaskAgent) → role cascade
              │   coordinator model CLI arg → fallback   [coordinator.rs:3590-3594]
              │
              ├── Auto-detect native executor for non-Anthropic models [coordinator.rs:3191-3208]
              │
              └── spawn::spawn_agent()                   [src/commands/spawn/mod.rs:145]
                    │
                    └── execution::spawn_agent_inner()   [src/commands/spawn/execution.rs:29]
                          │
                          ├── resolve_model_and_provider()  [execution.rs:1240]
                          │   Cascade: task → agent → executor → role → coordinator
                          │
                          ├── resolve_model_via_registry()  [execution.rs:1283]
                          │   Registry alias → full API model ID + provider + endpoint
                          │
                          ├── build_inner_command()          [execution.rs:741]
                          │   Constructs CLI invocation per executor type:
                          │   "claude" → `claude --print --model <m> ...`
                          │   "native" → `wg native-exec --model <m> --provider <p> ...`
                          │   "amplifier" → `amplifier run -m <m> ...`
                          │   "shell" → `bash -c <exec_cmd>`
                          │
                          ├── write_wrapper_script()         [execution.rs:985]
                          │   run.sh wraps command with timeout, output capture, auto-done/fail
                          │
                          └── cmd.spawn()                    [execution.rs:635]
                                Environment variables set on child process:
                                  WG_TASK_ID, WG_AGENT_ID, WG_EXECUTOR_TYPE,
                                  WG_MODEL, WG_ENDPOINT, WG_ENDPOINT_NAME,
                                  WG_ENDPOINT_URL, WG_API_KEY, WG_LLM_PROVIDER,
                                  WG_USER, WG_WORKTREE_PATH, WG_BRANCH
                                Env vars stripped:
                                  CLAUDECODE, CLAUDE_CODE_ENTRYPOINT (in wrapper)
```

### 1.2 Model Resolution Cascade (Full Priority Order)

Defined in `resolve_model_and_provider()` at `execution.rs:1240`:

| Priority | Source | Field |
|----------|--------|-------|
| 1 (highest) | Task | `task.model` + `task.provider` |
| 2 | Agent identity | `agent.preferred_model` + `agent.preferred_provider` |
| 3 | Executor config | `executor.model` (from `.workgraph/executors/<name>.toml`) |
| 4 | Role config | `config.models.task_agent.model` + `.provider` |
| 5 (lowest) | Coordinator | `coordinator.model` + `coordinator.provider` |

After cascade resolution, the model string passes through `resolve_model_via_registry()` which:
- Handles `provider:model` prefix format (e.g., `openrouter:minimax/minimax-m2.7`)
- Looks up aliases in the model registry
- Resolves endpoints from the registry entry

### 1.3 Executor Types and Their Model Invocation

| Executor | Command | Model Passed Via | File |
|----------|---------|-----------------|------|
| `claude` | `claude --print --model <m>` | CLI `--model` flag | execution.rs:859-887 |
| `native` | `wg native-exec --model <m> --provider <p>` | CLI args + env vars | execution.rs:920-961 |
| `amplifier` | `amplifier run -m <m> [-p <provider>]` | CLI `-m`/`-p` flags | execution.rs:888-919 |
| `shell` | `bash -c <task.exec>` | Not passed (user command) | execution.rs:963-971 |

---

## 2. Where LiteLLM/Harbor Fallback Happens

### 2.1 The Two Execution Paths

There are **two completely independent code paths** for running TB tasks:

#### Path A: Docker-aware LLM agent loop (litellm)
**File:** `terminal-bench/wg/adapter.py:453-622`

```python
async def _run_docker_agent_loop(...):
    import litellm                           # ← DIRECT litellm import
    litellm_model = model.replace(":", "/", 1)  # "openrouter:minimax/minimax-m2.7" → "openrouter/minimax/minimax-m2.7"
    response = await litellm.acompletion(
        model=litellm_model,
        messages=messages,
        tools=AGENT_TOOLS,
        ...
    )
```

This path:
- Is used by `WorkgraphAgent.run()` (the harbor `BaseAgent` implementation)
- Calls litellm directly from Python — **completely bypasses wg's executor**
- Routes tool calls through `environment.exec()` into Docker containers
- Uses litellm's own model routing (e.g., `openrouter/minimax/...` → OpenRouter API)

#### Path B: Native wg executor (wg service)
**File:** `terminal-bench/run_full_a_prime_vs_f.py` and `terminal-bench/run_hard_benchmarks.py`

These scripts:
1. Create a temp `.workgraph/` directory per trial
2. Write `config.toml` with `executor = "native"` and the benchmark model
3. Add a task via `wg add`
4. Start `wg service start`
5. Poll for completion
6. The wg service spawns agents through the standard path (Section 1.1)

### 2.2 The Exact Interception Point

**LiteLLM intercepts at `adapter.py:466`** — `import litellm` inside `_run_docker_agent_loop()`.

When harbor's runner (`harbor run`) invokes `WorkgraphAgent`, it calls:
```
WorkgraphAgent.run() → _run_docker_agent_loop() → litellm.acompletion()
```

This path **never touches wg's executor pipeline at all**. It's a self-contained Python LLM loop that:
1. Sends prompts to litellm (which routes to OpenRouter/OpenAI/etc.)
2. Gets tool call responses
3. Executes tools via harbor's `environment.exec()` (Docker)
4. Loops until done

### 2.3 Why Both Paths Exist

- **Path A (litellm/Docker)**: Required when TB uses Docker containers for environment isolation. The LLM runs on the host, but tool executions happen inside Docker. This is the standard harbor agent pattern.
- **Path B (native wg)**: Used by `run_full_a_prime_vs_f.py` for host-native execution. Gives wg full control over the executor, model routing, and agent lifecycle.

---

## 3. Environment Isolation Issues

### 3.1 The WG_MODEL Leak (Fixed in fab4ae74)

When the TB adapter runs **inside** a wg agent (i.e., wg spawns an agent that runs the TB benchmark), the parent service sets env vars:
```
WG_MODEL=claude-opus-4-latest    # parent service's model
WG_EXECUTOR_TYPE=claude       # parent's executor
WG_AGENT_ID=agent-NNN        # parent's agent ID
```

These leak into the TB adapter's subprocess calls. The native executor in the trial's `config.toml` specifies `model = "openrouter:minimax/minimax-m2.7"`, but `WG_MODEL` in the environment takes precedence at certain resolution points.

**Fix (fab4ae74):** `_exec_wg_cmd_host()` now strips these env vars:
```python
clean_env = {
    k: v for k, v in os.environ.items()
    if k not in (
        "WG_MODEL", "WG_EXECUTOR_TYPE", "WG_AGENT_ID", "WG_TASK_ID",
        "WG_LLM_PROVIDER", "WG_ENDPOINT", "WG_ENDPOINT_NAME",
        "WG_ENDPOINT_URL", "WG_API_KEY",
    )
}
```

### 3.2 Env Vars That Control Model/Endpoint

Set by `spawn_agent_inner()` at `execution.rs:464-497`:

| Env Var | Purpose | Set When |
|---------|---------|----------|
| `WG_MODEL` | Effective model for this agent | Model resolved |
| `WG_EXECUTOR_TYPE` | Executor type (claude/native/etc.) | Always |
| `WG_LLM_PROVIDER` | Provider name (anthropic/openrouter/etc.) | Provider resolved |
| `WG_ENDPOINT` | Named endpoint from config | Endpoint resolved |
| `WG_ENDPOINT_URL` | API base URL | URL resolved |
| `WG_API_KEY` | API key for the endpoint | Key resolved |
| `WG_AGENT_ID` | Agent identifier | Always |
| `WG_TASK_ID` | Task identifier | Always |

Additionally, provider-specific env vars are set (`OPENROUTER_API_KEY`, etc.) at `execution.rs:490-496`.

### 3.3 Config Propagation to Spawned Agents

**Does config reach the spawned agent?** Yes, through two mechanisms:

1. **CLI arguments**: `build_inner_command()` passes `--model`, `--provider`, etc. directly to the executor command line.
2. **Environment variables**: The env vars above are set on the `Command` before `.spawn()`.
3. **Config file**: The agent runs in the workgraph directory (or worktree), so it reads `.workgraph/config.toml` at startup.

**Potential gap**: If a parent wg service's env vars leak into a nested trial's subprocess (the bug fixed in fab4ae74), the env vars can override the trial's config.toml settings.

---

## 4. Recommendations

### 4.1 For Enforcing the Correct Model Path in TB

**Problem**: The Docker-based harbor runner uses litellm directly, bypassing wg's executor entirely.

**Recommendation**: This is **by design** for Docker-isolated trials. The litellm path is correct when:
- Verification runs in Docker containers
- Tool calls need to be routed through `environment.exec()`

For host-native trials (run_full_a_prime_vs_f.py), the wg native executor path is already correct.

### 4.2 For Preventing Env Var Leakage

The fix in `fab4ae74` handles the immediate issue. To make this more robust:

1. **Expand the strip list**: Consider stripping all `WG_*` env vars in `_exec_wg_cmd_host()`, not just the known ones. New env vars added to wg could leak without updating the strip list.

2. **Defensive config.toml**: The trial's config.toml should be authoritative. The native executor's model resolution (`create_provider_ext` at `provider.rs:77`) checks env vars like `WG_LLM_PROVIDER` and `WG_ENDPOINT_URL` — these must not leak from the parent.

### 4.3 For TB Isolation Per Problem

For the downstream `tb-isolation-design` task: each TB problem should get its own isolated `.workgraph/` directory (which `run_full_a_prime_vs_f.py` already does via `tempfile.mkdtemp`). The key requirements are:
- Clean env (no parent WG_* vars)
- Own config.toml with the benchmark model
- Own agent registry (no cross-contamination)
- Own service lifecycle (`wg service start` / `wg service stop`)

---

## 5. File Reference

| Component | File | Key Lines |
|-----------|------|-----------|
| Coordinator tick | `src/commands/service/coordinator.rs` | 3428-3610 |
| Agent spawn orchestration | `src/commands/service/coordinator.rs` | 3019-3239 |
| Spawn inner (claim + exec) | `src/commands/spawn/execution.rs` | 29-737 |
| Model resolution cascade | `src/commands/spawn/execution.rs` | 1240-1266 |
| Model registry resolution | `src/commands/spawn/execution.rs` | 1283-1370 |
| Command construction | `src/commands/spawn/execution.rs` | 741-981 |
| Env var propagation | `src/commands/spawn/execution.rs` | 464-497 |
| Wrapper script generation | `src/commands/spawn/execution.rs` | 985-1165 |
| Executor defaults | `src/service/executor.rs` | 1184-1260 |
| Native provider creation | `src/executor/native/provider.rs` | 77-200 |
| Coordinator agent spawn | `src/commands/service/coordinator_agent.rs` | 1974-2062 |
| TB adapter (litellm path) | `terminal-bench/wg/adapter.py` | 453-622 |
| TB adapter (env stripping) | `terminal-bench/wg/adapter.py` | 110-145 |
| TB host runner (native path) | `terminal-bench/run_full_a_prime_vs_f.py` | 0-150 |
| WG_MODEL fix commit | `fab4ae74` | — |
