# Design: Isolated WG Service Per TerminalBench Problem (Condition A)

## Overview

Condition A runs each terminalbench problem inside its own isolated `wg` instance
with up to 8 parallel agents, using wg's native executor. This document specifies
the directory structure, service lifecycle, config, verification, and results
collection — all as runnable code.

---

## 1. Directory Structure

Each problem gets a fresh temporary directory with its own `.workgraph/`:

```
/tmp/tb-condA-{timestamp}/
├── {problem-id}-r{replica}/           # one per trial
│   ├── .workgraph/
│   │   ├── graph.jsonl
│   │   ├── config.toml                # locked to native executor + target model
│   │   ├── service/
│   │   │   ├── state.json
│   │   │   └── daemon.log
│   │   └── agents/
│   │       ├── agent-{N}/
│   │       │   ├── stream.jsonl       # ← model verification data lives here
│   │       │   └── output.log
│   │       └── ...
│   └── workspace/                     # agent working area (if needed)
└── results/
    ├── summary.json                   # combined report
    └── {problem-id}-r{replica}.json   # per-trial result
```

**Key property:** Each trial's `.workgraph/` is independent — no shared state,
no shared service socket, no shared graph. Parallel trials cannot interfere.

The `tempfile.mkdtemp()` approach from `run_full_a_prime_vs_f.py` is proven and
correct. For condition A with 8 agents, we change `max_agents` from 1 to 8 —
nothing else about the isolation model changes.

---

## 2. Service Lifecycle

### Starting N independent services

Each trial gets its own `wg service start`. The services are independent because
they operate on separate `.workgraph/` directories (separate daemon sockets,
separate PID files).

```python
# Start service for one trial
async def start_trial_service(wg_dir: str, model: str, max_agents: int = 8) -> str:
    return await exec_wg(wg_dir, [
        "service", "start",
        "--max-agents", str(max_agents),
        "--executor", "native",
        "--model", model,
        "--no-coordinator-agent",  # no LLM coordinator — just dispatch tasks
        "--force",                 # clean start
    ])
```

### Concurrency control

Running N trials fully in parallel would spawn N * 8 = N*8 agents simultaneously.
To avoid overwhelming API rate limits or the host machine:

```python
MAX_CONCURRENT_TRIALS = 4  # 4 trials * 8 agents = 32 concurrent agents max

semaphore = asyncio.Semaphore(MAX_CONCURRENT_TRIALS)

async def run_trial_with_limit(trial_fn, *args):
    async with semaphore:
        return await trial_fn(*args)

# Launch all trials, semaphore gates concurrency
tasks = [run_trial_with_limit(run_trial, ...) for trial in trial_list]
results = await asyncio.gather(*tasks)
```

### Stopping services

Stop each service after its trial completes (or times out):

```python
async def stop_trial_service(wg_dir: str) -> str:
    return await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])
```

### Monitoring

Poll task status via `wg show` against each trial's directory:

```python
async def poll_all_trials(active_trials: dict[str, str]) -> dict[str, str]:
    """Poll completion status for all active trials in parallel."""
    results = {}
    for trial_id, wg_dir in active_trials.items():
        result = await exec_wg(wg_dir, ["show", root_task_id(trial_id)])
        status = parse_status(result)
        results[trial_id] = status
    return results
```

For real-time monitoring of all N services at once:

```bash
# Watch all running services (from runner script)
for dir in /tmp/tb-condA-*/*/; do
  echo "=== $(basename $dir) ==="
  wg --dir "$dir/.workgraph" list --status in-progress 2>/dev/null
  echo
done
```

---

## 3. Config Template

The config.toml locks the executor to `native` and specifies the exact model:

```toml
[coordinator]
max_agents = 8
executor = "native"
model = "{MODEL}"              # e.g., "openrouter:minimax/minimax-m2.7"
worktree_isolation = false     # all agents share the trial's workspace
agent_timeout = "30m"

[agent]
model = "{MODEL}"
context_scope = "clean"        # condition A: bare agent, no graph context
exec_mode = "full"

[agency]
auto_assign = false
auto_evaluate = false
```

**Why `native` executor?** The native executor is wg's built-in LLM client
(`src/executor/native/`). It calls OpenRouter/Anthropic APIs directly via
`openai_client.rs`. There is no litellm in this path — the Rust code makes
HTTP requests to the API endpoint configured in the model registry or endpoint
config.

**Why `auto_assign = false` and `auto_evaluate = false`?** These prevent the
agency system from spawning its own LLM calls (assigner, evaluator) that would
consume tokens and muddy the benchmark.

### Config generation (Python)

```python
def write_trial_config(wg_dir: str, model: str, max_agents: int = 8,
                       context_scope: str = "clean") -> None:
    """Write config.toml that locks executor to native + target model."""
    config = f"""[coordinator]
max_agents = {max_agents}
executor = "native"
model = "{model}"
worktree_isolation = false
agent_timeout = "30m"

[agent]
model = "{model}"
context_scope = "{context_scope}"
exec_mode = "full"

[agency]
auto_assign = false
auto_evaluate = false
"""
    with open(os.path.join(wg_dir, "config.toml"), "w") as f:
        f.write(config)
```

---

## 4. Verification: Confirming the Correct Executor Was Used

Three layers of verification, each independently sufficient:

### Layer 1: Environment sanitization (preventive)

Strip all `WG_*` env vars from the subprocess environment before invoking `wg`.
This is already done in `exec_wg()` from the existing runners. It prevents env
var leakage from any parent wg service:

```python
async def exec_wg(wg_dir: str, subcmd: list[str], timeout: float = 120) -> str:
    cmd = [WG_BIN, "--dir", wg_dir] + subcmd
    # Strip ALL WG_* env vars + CLAUDECODE — prevents any parent-service leakage
    env = {k: v for k, v in os.environ.items()
           if not k.startswith("WG_") and k != "CLAUDECODE"}
    proc = await asyncio.create_subprocess_exec(
        *cmd, stdout=PIPE, stderr=PIPE, env=env)
    ...
```

### Layer 2: stream.jsonl model verification (detective)

Every agent writes `stream.jsonl` with an `Init` event that records the
executor type and model. After the trial completes, parse these:

```python
def verify_executor_path(wg_dir: str, expected_model: str) -> dict:
    """Verify every agent in this trial used the expected model via native executor."""
    agents_dir = os.path.join(wg_dir, "agents")
    verification = {
        "all_native": True,
        "all_correct_model": True,
        "agents": [],
    }
    if not os.path.isdir(agents_dir):
        verification["error"] = "No agents directory"
        return verification

    for agent_id in os.listdir(agents_dir):
        stream_path = os.path.join(agents_dir, agent_id, "stream.jsonl")
        if not os.path.isfile(stream_path):
            continue
        agent_info = {"agent_id": agent_id, "executor": None, "model": None}
        with open(stream_path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if event.get("type") == "init":
                    agent_info["executor"] = event.get("executor_type")
                    agent_info["model"] = event.get("model")
                    break
        verification["agents"].append(agent_info)
        if agent_info["executor"] != "native":
            verification["all_native"] = False
        # Model comparison: strip provider prefix for matching
        actual = (agent_info.get("model") or "").replace(":", "/", 1)
        expected = expected_model.replace(":", "/", 1)
        if actual != expected and agent_info.get("model") != expected_model:
            verification["all_correct_model"] = False

    return verification
```

### Layer 3: config.toml + service flags audit (structural)

After the trial, copy the entire `.workgraph/` directory to results. The
downstream consumer can independently verify:

1. `config.toml` says `executor = "native"` and `model = "{expected}"`
2. `service/state.json` shows the service was started with native executor
3. Every `agents/*/stream.jsonl` Init event confirms native executor + model
4. `daemon.log` shows native executor spawn lines (not claude/amplifier)

```python
def audit_trial_config(wg_dir: str, expected_model: str) -> dict:
    """Structural audit: verify config matches intent."""
    config_path = os.path.join(wg_dir, "config.toml")
    audit = {"config_exists": False, "executor_native": False,
             "model_matches": False}
    if not os.path.isfile(config_path):
        return audit
    audit["config_exists"] = True
    with open(config_path) as f:
        content = f.read()
    audit["executor_native"] = 'executor = "native"' in content
    audit["model_matches"] = f'model = "{expected_model}"' in content
    return audit
```

### Verification in the trial result

Each trial result dict includes a `verification` key:

```python
result["verification"] = {
    "env_sanitized": True,  # always true by construction
    "executor_audit": audit_trial_config(wg_dir, model),
    "stream_verification": verify_executor_path(wg_dir, model),
}
```

---

## 5. Results Collection

### Per-trial result structure

```python
trial_result = {
    "trial_id": "configure-git-webserver-r0",
    "problem_id": "configure-git-webserver",
    "condition": "A",
    "replica": 0,
    "model": "openrouter:minimax/minimax-m2.7",
    "max_agents": 8,
    "status": "done",         # done | failed | timeout | error
    "elapsed_s": 423.7,
    "metrics": {
        "total_input_tokens": 145000,
        "total_output_tokens": 12340,
        "total_cost_usd": 0.0312,
        "total_turns": 15,
        "num_agents_spawned": 3,
    },
    "verification": {
        "env_sanitized": True,
        "executor_audit": {"config_exists": True, "executor_native": True, "model_matches": True},
        "stream_verification": {"all_native": True, "all_correct_model": True, "agents": [...]},
    },
    "verify_output": "...",   # wg evaluate output
    "error": None,
}
```

### Aggregation

```python
def aggregate_results(results: list[dict]) -> dict:
    """Aggregate per-trial results into a summary report."""
    passed = [r for r in results if r["status"] == "done"]
    failed = [r for r in results if r["status"] in ("failed", "error")]
    timed_out = [r for r in results if r["status"] == "timeout"]

    # Verification rollup
    all_verified = all(
        r.get("verification", {}).get("stream_verification", {}).get("all_native", False)
        and r.get("verification", {}).get("stream_verification", {}).get("all_correct_model", False)
        for r in results
    )

    times = [r["elapsed_s"] for r in results if r["elapsed_s"] > 0]
    total_tokens = sum(
        (r.get("metrics") or {}).get("total_input_tokens", 0)
        + (r.get("metrics") or {}).get("total_output_tokens", 0)
        for r in results
    )

    return {
        "condition": "A",
        "total_trials": len(results),
        "passed": len(passed),
        "failed": len(failed),
        "timed_out": len(timed_out),
        "pass_rate": len(passed) / len(results) if results else 0,
        "mean_time_s": sum(times) / len(times) if times else 0,
        "total_tokens": total_tokens,
        "all_executor_verified": all_verified,
        "per_problem": group_by_problem(results),
    }
```

### Result preservation

After each trial completes, copy the entire `.workgraph/` to the results
directory for post-hoc analysis:

```python
state_dst = os.path.join(results_dir, trial_id, "workgraph_state")
if os.path.isdir(wg_dir):
    shutil.copytree(wg_dir, state_dst)
```

Write incremental JSON after each trial (crash-safe progress):

```python
with open(os.path.join(results_dir, "incremental.json"), "w") as f:
    json.dump({"timestamp": now_iso(), "results": all_results}, f, indent=2)
```

---

## 6. Complete Trial Runner Function

This combines all 5 design decisions into one function:

```python
async def run_condition_a_trial(
    problem: dict,
    replica: int,
    model: str,
    max_agents: int = 8,
    timeout: float = 1800,
    results_dir: str = "results/condition-a",
) -> dict:
    """Run a single condition A trial with full isolation and verification."""
    trial_id = f"{problem['id']}-r{replica}"
    result = {
        "trial_id": trial_id,
        "problem_id": problem["id"],
        "condition": "A",
        "replica": replica,
        "model": model,
        "max_agents": max_agents,
        "status": "not_started",
        "elapsed_s": 0.0,
        "metrics": None,
        "verification": None,
        "error": None,
    }

    # Clean up any leftover tmp paths from prior runs
    cleanup_tmp_paths(problem.get("tmp_paths", []))

    # 1. Create isolated directory
    tmpdir = tempfile.mkdtemp(prefix=f"tb-condA-{trial_id}-")
    wg_dir = os.path.join(tmpdir, ".workgraph")
    start = time.monotonic()

    try:
        # 2. Initialize graph
        await exec_wg(wg_dir, ["init"])

        # 3. Write locked config
        write_trial_config(wg_dir, model, max_agents, context_scope="clean")

        # 4. Create the root task
        instruction = load_instruction(problem)
        root_task_id = f"tb-{trial_id}"
        await exec_wg(wg_dir, [
            "add", problem["title"],
            "--id", root_task_id,
            "-d", f"## Instructions\n\n{instruction}",
            "--verify", problem["verify_cmd"],
            "--exec-mode", "full",
            "--context-scope", "clean",
            "--model", model,
            "--no-place",
        ])

        # 5. Start isolated service
        await start_trial_service(wg_dir, model, max_agents)

        # 6. Poll for completion
        status, elapsed = await poll_completion(wg_dir, root_task_id, timeout)
        result["status"] = status
        result["elapsed_s"] = round(elapsed, 2)

        # 7. Stop service
        await stop_trial_service(wg_dir)

        # 8. Verify executor path
        result["verification"] = {
            "env_sanitized": True,
            "executor_audit": audit_trial_config(wg_dir, model),
            "stream_verification": verify_executor_path(wg_dir, model),
        }

        # 9. Collect metrics
        result["metrics"] = await collect_metrics(wg_dir)

    except Exception as e:
        result["status"] = "error"
        result["error"] = str(e)
        try:
            await stop_trial_service(wg_dir)
        except Exception:
            pass
    finally:
        result["elapsed_s"] = round(time.monotonic() - start, 2)
        # 10. Preserve graph state
        state_dst = os.path.join(results_dir, trial_id, "workgraph_state")
        try:
            os.makedirs(os.path.dirname(state_dst), exist_ok=True)
            if os.path.isdir(wg_dir):
                shutil.copytree(wg_dir, state_dst)
        except Exception:
            pass
        shutil.rmtree(tmpdir, ignore_errors=True)

    return result
```

---

## 7. Why Executor Fallback Cannot Happen

The design prevents litellm/harbor fallback at every level:

| Layer | Mechanism | Prevents |
|-------|-----------|----------|
| **Config** | `executor = "native"` in config.toml | Claude CLI executor, amplifier, shell |
| **CLI flags** | `--executor native` on `wg service start` | Config override by service |
| **Env sanitization** | Strip all `WG_*` from subprocess env | Parent service env leaking |
| **No litellm in path** | Native executor is Rust → HTTP → API | No Python litellm in the call chain |
| **No Docker** | Trial runs host-native, no harbor | Harbor's litellm agent loop never invoked |
| **Post-hoc verification** | `stream.jsonl` Init event audit | Detect if any agent deviated |

The litellm fallback (documented in the research) only occurs when using
harbor's `WorkgraphAgent._run_docker_agent_loop()` — which imports litellm
directly in Python. The native executor path is entirely in Rust:
`coordinator.rs → spawn/execution.rs → native/agent.rs → native/openai_client.rs → HTTP`.
There is no litellm import anywhere in this chain.

---

## 8. Differences from Existing Runners

| Aspect | Existing (`run_full_a_prime_vs_f.py`) | Condition A Design |
|--------|--------------------------------------|-------------------|
| `max_agents` | 1 | 8 |
| Concurrency | Sequential trials | Parallel trials (semaphore-gated) |
| Verification | None | 3-layer (env, stream, audit) |
| Federation | Yes (hub pull/push) | Optional (not needed for condition A) |
| Agency | Initialized but disabled | Not initialized (overhead reduction) |

The core isolation mechanism is identical: `tempfile.mkdtemp()` → `wg init` →
write config → `wg add` → `wg service start` → poll → `wg service stop`.
The changes are: more agents, parallel execution, and verification instrumentation.
