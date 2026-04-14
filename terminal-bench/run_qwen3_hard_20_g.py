#!/usr/bin/env python3
"""
TB Stress Test: Qwen3-Coder-30B, hardest tasks, Condition G (workgraph-assisted).

Runs all 18 available local TB tasks against Qwen3-Coder-30B on lambda01,
ordered hardest-first. Uses the SAME task set as run_qwen3_hard_20_a.py
for direct A vs G comparison.

Condition G: the seed agent decomposes the task into subtasks using `wg add`,
and the coordinator dispatches worker agents. Tests whether workgraph
decomposition helps on hard tasks — especially with 32k context pressure.

Hypothesis: On hard tasks, Condition G should outperform A because:
1. Decomposition gives each subtask a fresh 32k context window
2. Complex multi-file tasks can be broken into focused sub-problems
3. The graph coordination overhead is worth it when individual task
   complexity exceeds what fits in 32k

Usage:
    python run_qwen3_hard_20_g.py
    python run_qwen3_hard_20_g.py --smoke          # single task quick check
    python run_qwen3_hard_20_g.py --hard-only      # only hard-rated tasks (13)
    python run_qwen3_hard_20_g.py --tasks mailman,cobol-modernization
"""

import argparse
import asyncio
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

from wg.tasks import TASKS_BY_ID, ALL_TASKS

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

MODEL = "local:qwen3-coder-30b"
SGLANG_BASE_URL = "http://lambda01:30000/v1"
CONTEXT_WINDOW = 32768
MAX_AGENTS = 4  # Condition G: multiple agents for parallel subtask execution
DEFAULT_TIMEOUT = 3600  # 60 min per trial (longer for G due to coordination overhead)
DEFAULT_POLL_INTERVAL = 5.0

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
RUN_ID = "qwen3-hard-20-g"
RESULTS_DIR = os.path.join(SCRIPT_DIR, "results", RUN_ID)

WG_BIN = shutil.which("wg") or os.path.expanduser("~/.cargo/bin/wg")

# Same task ordering as Condition A for direct comparison.
# 10 hard-benchmark tasks (most challenging — multi-file, multi-step):
HARD_BENCHMARK = [
    "configure-git-webserver",     # pipeline: git + hooks + webserver
    "mailman",                     # pipeline: postfix + mailman3 + config
    "multi-source-data-merger",    # multi-file: 3 formats -> merge -> conflicts
    "financial-document-processor", # multi-file: classify -> extract -> summarize
    "cobol-modernization",         # multi-file: COBOL -> Python migration
    "build-cython-ext",            # pipeline: clone -> fix compat -> compile
    "fix-code-vulnerability",      # multi-file: analyze -> report -> fix
    "constraints-scheduling",      # algorithm: ICS parsing + constraint solving
    "multi-module-type-migration", # cascading: type change across 6 modules
    "iterative-test-fix",          # iterative: 6 bugs, 15 tests, fix all
]

# 3 hard calibration tasks:
HARD_CALIBRATION = [
    "algorithm",     # key-value store with transactions
    "ml",            # k-means clustering from scratch
    "sysadmin",      # rate-limited HTTP server
]

# 3 medium calibration tasks:
MEDIUM_CALIBRATION = [
    "debugging",         # fix merge sort bugs
    "shell-scripting",   # log file analyzer
    "data-processing",   # JSON to CSV department summary
]

# 2 easy calibration tasks (included for completeness):
EASY_CALIBRATION = [
    "file-ops",          # create project structure
    "text-processing",   # word frequency counter
]

# Ordered: hardest first (same as Condition A)
ALL_STRESS_TASKS = HARD_BENCHMARK + HARD_CALIBRATION + MEDIUM_CALIBRATION + EASY_CALIBRATION

# Per-difficulty time limits (seconds) — generous for Condition G coordination overhead
DIFFICULTY_TIMEOUTS = {
    "easy": 900,     # 15 min (G overhead on easy tasks)
    "medium": 1800,  # 30 min
    "hard": 3600,    # 60 min (complex decomposition + multiple agents)
}


# ---------------------------------------------------------------------------
# Condition G meta-prompt
# ---------------------------------------------------------------------------

CONDITION_G_META_PROMPT = """You are a graph architect. You do NOT implement solutions yourself.

Your job:
1. Read the task below and understand what needs to be done
2. Explore the working directory (`ls`, `cat`) to understand the codebase
3. Build a workgraph that solves the problem, then mark YOUR task done

DO NOT write code. DO NOT modify files. Only create wg tasks.

## Graph design

Create tasks using `wg add`, then wire them into a self-correcting cycle:

```bash
# 1. Work tasks (parallelize where possible — up to {max_agents} agents run concurrently)
wg add "Implement the solution" --no-place -d "Description of what to do..."

# 2. Verify task (runs after work, checks if tests pass)
wg add "Run tests and verify" --after implement-the-solution --no-place \\
  -d "Run the test suite: <test command>.
If tests PASS: wg done <your-task-id> --converged
If tests FAIL: wg log <your-task-id> 'what failed and why', then wg done <your-task-id>"

# 3. Close the loop: work task cycles back through verify
wg edit implement-the-solution --add-after run-tests-and-verify --max-iterations 5
```

The verify agent signals `--converged` when tests pass (stops the loop) or
plain `wg done` when tests fail (triggers another iteration with failure
context visible to the next work agent via `wg context`).

## Context management — CRITICAL

Your context window is only {context_window} tokens. This is SHORT.
Decomposition is your main tool for managing context pressure:
- Each subtask gets a FRESH context window
- Break complex multi-file tasks into focused sub-problems
- Don't try to fit everything into one agent's context

## Important details for sub-task descriptions

Worker agents don't see this prompt. They only see the description you write
in `wg add -d "..."`. So put ALL necessary context in each task's description:
- What files to read/modify
- What the expected output is
- How to verify (test command)
- IMPORTANT: Remind them their context window is only {context_window} tokens
- For the verify task: EXACTLY when to use `--converged` vs plain `wg done`

## After building the graph

Call `wg done {seed_task_id}` to mark this seed task complete. The
coordinator dispatches worker agents to your tasks automatically.

"""

# ---------------------------------------------------------------------------
# Condition G-smart: try-first meta-prompt (smart fanout calculus)
# ---------------------------------------------------------------------------

CONDITION_G_SMART_META_PROMPT = """You are solving a programming task. You have two strategies available:

**Strategy 1 — Direct Implementation (default)**
Implement the solution yourself. This is fastest for most tasks.

**Strategy 2 — Decomposition (only when needed)**
Break the task into subtasks and let other agents implement them in parallel.
Only use this if direct implementation won't work.

## Step 1: Triage (spend < 2 minutes here)

Read the task. Scan the working directory (`ls`, `ls tests/`). Then decide:

**Use DIRECT IMPLEMENTATION if ANY of these are true:**
- The instruction is under ~300 words
- You need to modify 2 or fewer files
- The test suite has 5 or fewer tests
- The task is a single logical unit of work (even if complex)
- You're not sure → default to direct implementation

**Use DECOMPOSITION only if ALL of these are true:**
- The instruction is over ~500 words
- You need to modify 3+ distinct files
- The work splits into 2-4 independent sub-problems (different files, no ordering)
- Each sub-problem is substantial enough to benefit from a fresh context window

**Log your decision:**
```bash
wg log {seed_task_id} "FANOUT_DECISION: <direct|decompose> — <reason>"
```

## Context management — CRITICAL

Your context window is only {context_window} tokens. This is SHORT.
If you choose direct implementation, be aware of context pressure:
- If you start re-reading files you already read, or losing track of earlier edits
- Switch to decomposition for the REMAINING work only
```bash
wg log {seed_task_id} "FANOUT_SWITCH: direct→decompose — context pressure after N turns"
```

## If Direct Implementation

Implement the solution. Write code, modify files, run tests.

If tests pass → `wg done {seed_task_id}`

## If Decomposition

1. **Serialize your exploration** — everything you learned during triage goes
   into the subtask descriptions. File paths, test commands, patterns, edge cases.
   Workers only see what you write in `wg add -d "..."`.

2. **Create 2-4 focused subtasks** (NEVER more than 4):
```bash
wg add "Part 1: <specific scope>" --no-place -d "## What to do
<concrete instructions>

## Files to modify
- path/to/file1.py

## How to verify
Run: <test command>

## Context
Your context window is only {context_window} tokens.

## IMPORTANT
Implement directly. Do NOT create subtasks. Do NOT decompose further."
```

3. **Wire in a verify task**:
```bash
wg add "Verify: run full test suite" --after part-1,part-2 --no-place \\
  -d "Run the test suite: <test command>.
If ALL tests pass: wg done <your-task-id> --converged
If tests fail: wg log <your-task-id> 'what failed' then wg done <your-task-id>"
```

4. **Create the retry loop** (if the task warrants iteration):
```bash
wg edit part-1 --add-after verify --max-iterations 3
```

5. **Mark your seed task done**:
```bash
wg done {seed_task_id}
```

## Hard constraints
- NEVER create more than 4 subtasks
- Subtasks must NOT create their own subtasks (1 level max)
- If two subtasks would modify the same file, merge them or serialize with --after
- Always include a verify task at the end

"""


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

async def exec_wg(wg_dir: str, subcmd: list[str], timeout: float = 120,
                  extra_env: dict | None = None) -> str:
    """Execute a wg command against a specific graph directory."""
    cmd = [WG_BIN, "--dir", wg_dir] + subcmd
    env = {k: v for k, v in os.environ.items()
           if not k.startswith("WG_") and k != "CLAUDECODE"}
    if extra_env:
        env.update(extra_env)
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=env,
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        parts = []
        if stdout:
            parts.append(stdout.decode(errors="replace"))
        if stderr:
            parts.append(stderr.decode(errors="replace"))
        if proc.returncode != 0:
            parts.append(f"[exit code: {proc.returncode}]")
        return "\n".join(parts) if parts else "(no output)"
    except asyncio.TimeoutError:
        return f"[wg command timed out after {timeout}s]"
    except Exception as e:
        return f"[wg command error: {e}]"


def load_instruction(task_def: dict) -> str:
    """Load task instruction from file."""
    path = os.path.join(SCRIPT_DIR, task_def["instruction_file"])
    with open(path) as f:
        return f.read().strip()


def cleanup_tmp_paths(task_def: dict) -> None:
    """Remove /tmp files from a previous trial to ensure isolation."""
    for p in task_def.get("tmp_paths", []):
        if os.path.isdir(p):
            shutil.rmtree(p, ignore_errors=True)
        elif os.path.isfile(p):
            os.remove(p)


async def poll_graph_quiescence(
    wg_dir: str,
    timeout_secs: float,
    poll_interval: float = DEFAULT_POLL_INTERVAL,
) -> tuple[str, float]:
    """Poll until all non-internal tasks reach terminal status.

    For Condition G, the architect creates subtasks and marks the seed task done.
    We wait until ALL user tasks (excluding internal .coordinator-0, .compact-0, etc.)
    are terminal.
    """
    start = time.monotonic()

    while True:
        elapsed = time.monotonic() - start
        if elapsed > timeout_secs:
            return "timeout", elapsed

        # Check for any active (non-terminal) non-internal tasks
        has_active = False
        for check_status in ("open", "in-progress", "blocked"):
            result = await exec_wg(wg_dir, ["list", "--status", check_status])
            if "[exit code:" in result:
                continue
            for line in result.strip().splitlines():
                stripped = line.strip()
                if not stripped:
                    continue
                # Skip "No tasks found" and similar non-task lines
                if "no tasks" in stripped.lower() or "tasks found" in stripped.lower():
                    continue
                parts = stripped.split()
                if not parts:
                    continue
                # Extract task ID from various output formats
                # Format: [~] task-id - Title  or  [x] task-id - Title  or  [F] task-id - ...
                if len(parts) >= 2 and parts[0].startswith("["):
                    task_id_col = parts[1]
                else:
                    task_id_col = parts[0]
                # Skip header/border lines
                if task_id_col.startswith("─") or task_id_col == "ID":
                    continue
                # Skip internal daemon tasks (IDs starting with '.')
                if task_id_col.startswith("."):
                    continue
                has_active = True
                break
            if has_active:
                break

        if not has_active:
            # All user tasks are terminal — check if any succeeded
            done_result = await exec_wg(wg_dir, ["list", "--status", "done"])
            if "[exit code:" not in done_result and done_result.strip():
                for line in done_result.strip().splitlines():
                    stripped = line.strip()
                    if not stripped or "no tasks" in stripped.lower():
                        continue
                    parts = stripped.split()
                    if not parts:
                        continue
                    if len(parts) >= 2 and parts[0].startswith("["):
                        task_id_col = parts[1]
                    else:
                        task_id_col = parts[0]
                    if task_id_col.startswith("─") or task_id_col == "ID":
                        continue
                    if not task_id_col.startswith("."):
                        return "done", elapsed
            return "failed", elapsed

        await asyncio.sleep(poll_interval)


async def collect_metrics(wg_dir: str) -> dict:
    """Read agent stream.jsonl files to extract token counts and context events."""
    agents_dir = os.path.join(wg_dir, "agents")
    metrics = {
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cost_usd": 0.0,
        "total_turns": 0,
        "num_agents_spawned": 0,
        "max_input_tokens_single_turn": 0,
        "context_truncation_events": 0,
        "token_counts_per_turn": [],
    }

    if not os.path.isdir(agents_dir):
        return metrics

    for agent_id in os.listdir(agents_dir):
        agent_dir = os.path.join(agents_dir, agent_id)
        if not os.path.isdir(agent_dir):
            continue
        metrics["num_agents_spawned"] += 1

        stream_path = os.path.join(agent_dir, "stream.jsonl")
        if not os.path.isfile(stream_path):
            continue

        try:
            with open(stream_path) as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        event = json.loads(line)
                    except json.JSONDecodeError:
                        continue

                    if event.get("type") == "turn":
                        metrics["total_turns"] += 1
                        usage = event.get("usage")
                        if usage:
                            in_tok = usage.get("input_tokens", 0)
                            out_tok = usage.get("output_tokens", 0)
                            metrics["total_input_tokens"] += in_tok
                            metrics["total_output_tokens"] += out_tok
                            metrics["token_counts_per_turn"].append({
                                "agent": agent_id,
                                "turn": metrics["total_turns"],
                                "input_tokens": in_tok,
                                "output_tokens": out_tok,
                            })
                            if in_tok > metrics["max_input_tokens_single_turn"]:
                                metrics["max_input_tokens_single_turn"] = in_tok
                            # Context pressure: approaching 32k window
                            if in_tok > CONTEXT_WINDOW * 0.8:
                                metrics["context_truncation_events"] += 1
                    elif event.get("type") == "result":
                        usage = event.get("usage", {})
                        cost = usage.get("cost_usd")
                        if cost:
                            metrics["total_cost_usd"] += cost
        except Exception:
            pass

    return metrics


async def count_subtasks(wg_dir: str) -> dict:
    """Count tasks in the graph to measure decomposition behavior."""
    graph_path = os.path.join(wg_dir, "graph.jsonl")
    counts = {
        "total_tasks": 0,
        "user_tasks": 0,
        "internal_tasks": 0,
        "done_tasks": 0,
        "failed_tasks": 0,
        "task_ids": [],
    }

    if not os.path.isfile(graph_path):
        return counts

    try:
        with open(graph_path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    task = json.loads(line)
                except json.JSONDecodeError:
                    continue

                task_id = task.get("id", "")
                status = task.get("status", "")
                counts["total_tasks"] += 1

                if task_id.startswith("."):
                    counts["internal_tasks"] += 1
                else:
                    counts["user_tasks"] += 1
                    counts["task_ids"].append(task_id)
                    if status == "done":
                        counts["done_tasks"] += 1
                    elif status in ("failed", "abandoned"):
                        counts["failed_tasks"] += 1
    except Exception:
        pass

    return counts


def analyze_daemon_log(wg_dir: str) -> dict:
    """Parse daemon log for context management events and errors."""
    daemon_log = os.path.join(wg_dir, "service", "daemon.log")
    analysis = {
        "error_patterns": [],
        "context_events": [],
        "log_tail": "",
    }

    if not os.path.isfile(daemon_log):
        return analysis

    try:
        with open(daemon_log) as f:
            log_content = f.read()

        # Check for known error patterns
        error_patterns = [
            ("OOM", "out_of_memory"),
            ("out of memory", "out_of_memory"),
            ("CUDA", "cuda_error"),
            ("connection refused", "endpoint_down"),
            ("Connection refused", "endpoint_down"),
            ("context length", "context_overflow"),
            ("maximum context", "context_overflow"),
            ("token limit", "token_limit"),
            ("truncat", "truncation"),
            ("rate limit", "rate_limit"),
            ("429", "rate_limit"),
        ]
        for pattern, category in error_patterns:
            if pattern.lower() in log_content.lower():
                analysis["error_patterns"].append(category)

        # Look for context management events
        for line in log_content.splitlines():
            lower = line.lower()
            if any(kw in lower for kw in ["truncat", "context", "token", "overflow", "sliding"]):
                if len(analysis["context_events"]) < 50:
                    analysis["context_events"].append(line.strip()[:200])

        # Save last 5000 chars
        analysis["log_tail"] = log_content[-5000:]
    except Exception:
        pass

    return analysis


def write_trial_config(wg_dir: str, worktree_isolation: bool = False) -> None:
    """Write config.toml for a Condition G trial against lambda01 SGLang.

    Key differences from Condition A:
    - max_agents = 4 (multi-agent for subtask parallelism)
    - context_scope = "graph" (agents see the full graph)
    - coordinator_agent = true (needed for Condition G dispatch)
    - heartbeat_interval = 30 (autonomous coordination)
    """
    worktree_val = "true" if worktree_isolation else "false"
    config = f"""[coordinator]
max_agents = {MAX_AGENTS}
executor = "native"
model = "{MODEL}"
worktree_isolation = {worktree_val}
agent_timeout = "40m"
max_verify_failures = 0
max_spawn_failures = 0
coordinator_agent = true
heartbeat_interval = 30

[agent]
model = "{MODEL}"
context_scope = "graph"
exec_mode = "full"

[agency]
auto_assign = false
auto_evaluate = false

[native_executor]
api_base = "{SGLANG_BASE_URL}"
context_window = {CONTEXT_WINDOW}
"""
    with open(os.path.join(wg_dir, "config.toml"), "w") as f:
        f.write(config)


# ---------------------------------------------------------------------------
# Trial runner
# ---------------------------------------------------------------------------

async def run_trial(
    task_def: dict,
    timeout: float,
    smart: bool = False,
) -> dict:
    """Run a single Condition G trial with per-trial isolation."""
    task_id = task_def["id"]
    trial_id = f"{RUN_ID}-{task_id}"
    result = {
        "trial_id": trial_id,
        "task": task_id,
        "difficulty": task_def["difficulty"],
        "condition": "G",
        "model": MODEL,
        "endpoint": SGLANG_BASE_URL,
        "context_window": CONTEXT_WINDOW,
        "max_agents": MAX_AGENTS,
        "status": "not_started",
        "elapsed_s": 0.0,
        "reward": 0.0,
        "failure_mode": None,
        "metrics": None,
        "subtask_counts": None,
        "context_analysis": None,
        "error": None,
    }

    # Clean up /tmp paths from previous runs of same task
    cleanup_tmp_paths(task_def)

    tmpdir = tempfile.mkdtemp(prefix=f"tb-qwen3-g-hard-{task_id}-")
    wg_dir = os.path.join(tmpdir, ".workgraph")
    start = time.monotonic()

    print(f"  [{trial_id}] Starting Condition G trial in {tmpdir}...")

    try:
        # 1. Init graph
        init_out = await exec_wg(wg_dir, ["init"])
        if "error" in init_out.lower() and "already" not in init_out.lower():
            result["error"] = f"Init failed: {init_out}"
            result["status"] = "failed"
            result["failure_mode"] = "init_error"
            return result

        # 2. Write config (Condition G specific)
        write_trial_config(wg_dir, worktree_isolation=smart)

        # 3. Load instruction and build Condition G description
        instruction = load_instruction(task_def)
        root_task_id = f"tb-{task_id}"

        # Build the Condition G meta-prompt with seed task ID, verify cmd, and context info
        base_prompt = CONDITION_G_SMART_META_PROMPT if smart else CONDITION_G_META_PROMPT
        meta = base_prompt.replace("{seed_task_id}", root_task_id)
        meta = meta.replace("{max_agents}", str(MAX_AGENTS))
        meta = meta.replace("{context_window}", str(CONTEXT_WINDOW))

        if task_def.get("verify_cmd"):
            meta += (
                f"\n## Test command\n"
                f"The test command that determines pass/fail is:\n"
                f"```\n{task_def['verify_cmd']}\n```\n"
                f"Include this command in your verify task's description "
                f"so it knows exactly what to run.\n\n"
            )

        full_instruction = meta + instruction

        description = (
            f"## Terminal Bench Trial: Qwen3-Coder-30B Stress Test — Condition G\n\n"
            f"**Task:** {task_id} ({task_def['difficulty']})\n"
            f"**Model:** {MODEL}\n"
            f"**Endpoint:** {SGLANG_BASE_URL}\n"
            f"**Context Window:** {CONTEXT_WINDOW} tokens\n"
            f"**Condition:** G (workgraph-assisted, decomposition enabled)\n"
            f"**Max Parallel Agents:** {MAX_AGENTS}\n\n"
            f"## Instructions\n\n{full_instruction}\n"
        )

        add_out = await exec_wg(wg_dir, [
            "add", f"TB-G: {task_def['title']}",
            "--id", root_task_id,
            "-d", description,
            "--exec-mode", "full",
            "--context-scope", "graph",
            "--model", MODEL,
            "--no-place",
        ])
        if "[exit code:" in add_out and root_task_id not in add_out:
            result["error"] = f"Task creation failed: {add_out}"
            result["status"] = "failed"
            result["failure_mode"] = "task_create_error"
            return result

        # 4. Start wg service (WITH coordinator agent for Condition G)
        service_out = await exec_wg(wg_dir, [
            "service", "start",
            "--max-agents", str(MAX_AGENTS),
            "--executor", "native",
            "--model", MODEL,
            "--force",
        ])
        print(f"  [{trial_id}] Service started (Condition G, max_agents={MAX_AGENTS}), "
              f"polling for graph quiescence...")

        # 5. Poll for graph quiescence (all user tasks terminal)
        status, elapsed = await poll_graph_quiescence(wg_dir, timeout)
        result["status"] = status
        result["elapsed_s"] = round(elapsed, 2)

        # Always run the verify command — even after timeout, the agents
        # may have actually completed the work (race between poll and timeout).
        try:
            verify_result = subprocess.run(
                ["bash", "-c", task_def["verify_cmd"]],
                capture_output=True, text=True, timeout=60,
            )
            verify_passed = verify_result.returncode == 0
        except (subprocess.TimeoutExpired, Exception):
            verify_passed = False

        if verify_passed:
            result["reward"] = 1.0
            result["failure_mode"] = None
            if status == "timeout":
                result["status"] = "done_after_timeout"
            elif status != "done":
                result["status"] = "done"
        elif status == "timeout":
            result["failure_mode"] = "timeout"
        elif status == "failed":
            result["failure_mode"] = "wrong_answer"
        else:
            result["failure_mode"] = f"status_{status}"

        print(f"  [{trial_id}] Completed: {status} in {elapsed:.1f}s "
              f"(reward={result['reward']}, verify={'PASS' if verify_passed else 'FAIL'})")

        # 6. Stop service
        await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])

        # 7. Collect metrics (token counts, context events)
        result["metrics"] = await collect_metrics(wg_dir)

        # 8. Count subtasks (key Condition G metric)
        result["subtask_counts"] = await count_subtasks(wg_dir)

        # 9. Analyze daemon log for context management and errors
        result["context_analysis"] = analyze_daemon_log(wg_dir)

        # Classify failure mode based on analysis
        if result["failure_mode"] and result["context_analysis"]:
            patterns = result["context_analysis"].get("error_patterns", [])
            if "context_overflow" in patterns:
                result["failure_mode"] = "context_overflow"
            elif "out_of_memory" in patterns:
                result["failure_mode"] = "oom"
            elif "endpoint_down" in patterns:
                result["failure_mode"] = "endpoint_error"
            elif "rate_limit" in patterns:
                result["failure_mode"] = "rate_limit"

    except Exception as e:
        result["status"] = "error"
        result["error"] = str(e)
        result["failure_mode"] = "exception"
        print(f"  [{trial_id}] Error: {e}")
        try:
            await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])
        except Exception:
            pass
    finally:
        result["elapsed_s"] = round(time.monotonic() - start, 2)
        # Save workgraph state before cleanup
        state_dst = os.path.join(RESULTS_DIR, trial_id, "workgraph_state")
        try:
            os.makedirs(os.path.dirname(state_dst), exist_ok=True)
            if os.path.isdir(wg_dir):
                shutil.copytree(wg_dir, state_dst)
        except Exception:
            pass

        shutil.rmtree(tmpdir, ignore_errors=True)

    return result


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main(timeout: float, tasks: list[str] | None = None, smart: bool = False):
    global RUN_ID, RESULTS_DIR
    if smart:
        RUN_ID = "qwen3-hard-20-g-smart"
        RESULTS_DIR = os.path.join(SCRIPT_DIR, "results", RUN_ID)
    task_names = tasks or ALL_STRESS_TASKS
    total = len(task_names)

    # Verify endpoint is reachable
    import urllib.request
    try:
        with urllib.request.urlopen(f"{SGLANG_BASE_URL}/models", timeout=10) as resp:
            models_data = json.loads(resp.read())
            model_ids = [m["id"] for m in models_data.get("data", [])]
            if "qwen3-coder-30b" not in model_ids:
                print(f"ERROR: qwen3-coder-30b not found at {SGLANG_BASE_URL}")
                print(f"  Available models: {model_ids}")
                sys.exit(1)
            print(f"Endpoint OK: qwen3-coder-30b available at {SGLANG_BASE_URL}")
    except Exception as e:
        print(f"ERROR: Cannot reach {SGLANG_BASE_URL}: {e}")
        sys.exit(1)

    variant = "G-smart (try-first smart fanout)" if smart else "G (always-decompose)"
    print(f"\nTB Stress Test: Qwen3-Coder-30B — Hardest Tasks — Condition {variant}")
    print(f"  Model: {MODEL}")
    print(f"  Endpoint: {SGLANG_BASE_URL}")
    print(f"  Context window: {CONTEXT_WINDOW} tokens")
    print(f"  Max agents: {MAX_AGENTS}")
    print(f"  Run ID: {RUN_ID}")
    print(f"  Smart fanout: {smart}")
    print(f"  Tasks ({total}): {task_names}")
    print(f"  Timeout: {timeout}s per trial")
    print(f"  wg binary: {WG_BIN}")
    print()

    results = []
    start_time = time.monotonic()

    for i, task_name in enumerate(task_names, 1):
        if task_name not in TASKS_BY_ID:
            print(f"  WARNING: Unknown task '{task_name}', skipping")
            continue

        task_def = TASKS_BY_ID[task_name]
        # Use difficulty-based timeout if available
        task_timeout = DIFFICULTY_TIMEOUTS.get(task_def["difficulty"], timeout)

        print(f"\n--- [{i}/{total}] Task: {task_name} ({task_def['difficulty']}, "
              f"timeout={task_timeout}s) ---")

        result = await run_trial(task_def, task_timeout, smart=smart)
        results.append(result)

        # Print running tally with G-specific metrics
        passed_so_far = sum(1 for r in results if r["reward"] > 0)
        metrics = result.get("metrics") or {}
        subtasks = (result.get("subtask_counts") or {}).get("user_tasks", 0)
        agents = metrics.get("num_agents_spawned", 0)
        max_tok = metrics.get("max_input_tokens_single_turn", 0)
        ctx_events = metrics.get("context_truncation_events", 0)
        turns = metrics.get("total_turns", 0)
        print(f"  Running: {passed_so_far}/{len(results)} passed | "
              f"subtasks={subtasks} | agents={agents} | "
              f"turns={turns} | max_input_tok={max_tok} | "
              f"ctx_pressure={ctx_events} | "
              f"failure={result.get('failure_mode', 'none')}")

    total_time = time.monotonic() - start_time

    # Compute statistics
    passed = sum(1 for r in results if r["reward"] > 0)
    total_trials = len(results)
    times = [r["elapsed_s"] for r in results if r["elapsed_s"] > 0]
    mean_time = sum(times) / len(times) if times else 0
    median_time = sorted(times)[len(times) // 2] if times else 0

    # Token throughput
    total_input = sum(r.get("metrics", {}).get("total_input_tokens", 0)
                      for r in results if r.get("metrics"))
    total_output = sum(r.get("metrics", {}).get("total_output_tokens", 0)
                       for r in results if r.get("metrics"))
    total_turns = sum(r.get("metrics", {}).get("total_turns", 0)
                      for r in results if r.get("metrics"))
    total_agents = sum(r.get("metrics", {}).get("num_agents_spawned", 0)
                       for r in results if r.get("metrics"))

    # Context stress analysis
    ctx_truncations = sum(r.get("metrics", {}).get("context_truncation_events", 0)
                          for r in results if r.get("metrics"))
    max_input_any = max((r.get("metrics", {}).get("max_input_tokens_single_turn", 0)
                         for r in results if r.get("metrics")), default=0)

    # Subtask metrics (key Condition G analysis)
    total_user_tasks = sum(
        (r.get("subtask_counts") or {}).get("user_tasks", 0)
        for r in results
    )
    avg_subtasks_per_trial = total_user_tasks / total_trials if total_trials else 0

    # Decomposition analysis: did decomposition correlate with success?
    decomposed_trials = [r for r in results
                         if (r.get("subtask_counts") or {}).get("user_tasks", 0) > 1]
    decomposed_passed = sum(1 for r in decomposed_trials if r["reward"] > 0)
    undecomposed_trials = [r for r in results
                           if (r.get("subtask_counts") or {}).get("user_tasks", 0) <= 1]
    undecomposed_passed = sum(1 for r in undecomposed_trials if r["reward"] > 0)

    # Failure mode breakdown
    failure_modes = {}
    for r in results:
        mode = r.get("failure_mode") or "success"
        failure_modes[mode] = failure_modes.get(mode, 0) + 1

    # By difficulty breakdown
    by_difficulty = {}
    for r in results:
        diff = r["difficulty"]
        if diff not in by_difficulty:
            by_difficulty[diff] = {"passed": 0, "total": 0}
        by_difficulty[diff]["total"] += 1
        if r["reward"] > 0:
            by_difficulty[diff]["passed"] += 1
    for d in by_difficulty.values():
        d["pass_rate"] = d["passed"] / d["total"] if d["total"] > 0 else 0

    summary = {
        "run_id": RUN_ID,
        "model": MODEL,
        "endpoint": SGLANG_BASE_URL,
        "context_window": CONTEXT_WINDOW,
        "serving_engine": "SGLang",
        "gpu": "RTX 6000 Ada 48GB (lambda01)",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "condition": f"G-smart (try-first smart fanout)" if smart else "G (workgraph-assisted, decomposition enabled)",
        "smart_fanout": smart,
        "max_agents": MAX_AGENTS,
        "task_selection": "All 18 available local TB tasks, ordered hardest-first (same as Condition A)",
        "total_trials": total_trials,
        "passed": passed,
        "pass_rate": passed / total_trials if total_trials > 0 else 0,
        "mean_time_s": round(mean_time, 2),
        "median_time_s": round(median_time, 2),
        "total_wall_clock_s": round(total_time, 2),
        "total_input_tokens": total_input,
        "total_output_tokens": total_output,
        "total_turns": total_turns,
        "total_agents_spawned": total_agents,
        "total_user_tasks_created": total_user_tasks,
        "avg_subtasks_per_trial": round(avg_subtasks_per_trial, 2),
        "tokens_per_second": round(total_output / total_time, 2) if total_time > 0 else 0,
        "context_stress": {
            "total_truncation_events": ctx_truncations,
            "max_input_tokens_any_turn": max_input_any,
            "context_window": CONTEXT_WINDOW,
            "utilization_pct": round(max_input_any / CONTEXT_WINDOW * 100, 1) if CONTEXT_WINDOW > 0 else 0,
        },
        "decomposition_analysis": {
            "trials_that_decomposed": len(decomposed_trials),
            "decomposed_pass_rate": decomposed_passed / len(decomposed_trials) if decomposed_trials else 0,
            "trials_no_decomposition": len(undecomposed_trials),
            "undecomposed_pass_rate": undecomposed_passed / len(undecomposed_trials) if undecomposed_trials else 0,
        },
        "failure_modes": failure_modes,
        "by_difficulty": by_difficulty,
        "trials": results,
    }

    # Write results
    os.makedirs(RESULTS_DIR, exist_ok=True)

    json_path = os.path.join(RESULTS_DIR, "summary.json")
    with open(json_path, "w") as f:
        json.dump(summary, f, indent=2)

    # Print summary
    print(f"\n{'='*80}")
    print(f"SUMMARY: {RUN_ID} — Qwen3-Coder-30B Stress Test (Condition G)")
    print(f"{'='*80}")
    print(f"  Model: {MODEL}")
    print(f"  Endpoint: {SGLANG_BASE_URL}")
    print(f"  Context window: {CONTEXT_WINDOW} tokens")
    print(f"  Max agents: {MAX_AGENTS}")
    print(f"  Pass rate: {passed}/{total_trials} ({passed/total_trials:.0%})"
          if total_trials else "  No trials")
    print(f"  Mean time per trial: {mean_time:.1f}s")
    print(f"  Median time per trial: {median_time:.1f}s")
    print(f"  Total wall clock: {total_time:.1f}s ({total_time/60:.1f}m)")
    print(f"  Total tokens: {total_input:,} in + {total_output:,} out")
    print(f"  Total turns: {total_turns}")
    print(f"  Total agents spawned: {total_agents}")
    print(f"  Total user tasks created: {total_user_tasks} "
          f"(avg {avg_subtasks_per_trial:.1f}/trial)")
    print(f"  Effective throughput: {summary['tokens_per_second']:.1f} tok/s (output)")
    print(f"\n  Context stress:")
    print(f"    Truncation events (>80% ctx): {ctx_truncations}")
    print(f"    Max input tokens any turn: {max_input_any:,}")
    print(f"    Context utilization: {summary['context_stress']['utilization_pct']:.1f}%")
    print(f"\n  Decomposition analysis:")
    print(f"    Trials that decomposed: {len(decomposed_trials)}/{total_trials}")
    da = summary["decomposition_analysis"]
    print(f"    Decomposed pass rate: {da['decomposed_pass_rate']:.0%}" if decomposed_trials else "    (none decomposed)")
    print(f"    Undecomposed pass rate: {da['undecomposed_pass_rate']:.0%}" if undecomposed_trials else "    (all decomposed)")
    print(f"\n  Failure modes: {failure_modes}")
    print(f"\n  By difficulty:")
    for diff, stats in by_difficulty.items():
        print(f"    {diff}: {stats['passed']}/{stats['total']} ({stats['pass_rate']:.0%})")
    print()
    print(f"  Per-task results:")
    for r in results:
        metrics = r.get("metrics") or {}
        turns = metrics.get("total_turns", 0)
        agents = metrics.get("num_agents_spawned", 0)
        subtasks = (r.get("subtask_counts") or {}).get("user_tasks", 0)
        max_tok = metrics.get("max_input_tokens_single_turn", 0)
        ctx_ev = metrics.get("context_truncation_events", 0)
        print(f"    {r['task']:35s} {r['difficulty']:8s} "
              f"reward={r['reward']:.1f}  time={r['elapsed_s']:7.1f}s  "
              f"turns={turns:3d}  agents={agents:2d}  subtasks={subtasks:2d}  "
              f"max_tok={max_tok:6d}  ctx_press={ctx_ev:2d}  "
              f"failure={r.get('failure_mode', 'none')}")
    print()

    # Head-to-head comparison with Condition A
    a_path = os.path.join(SCRIPT_DIR, "results", "qwen3-hard-20-a", "summary.json")
    if os.path.isfile(a_path):
        with open(a_path) as f:
            a_data = json.load(f)
        a_trials = {t["task"]: t for t in a_data.get("trials", [])}
        print(f"\n  {'='*80}")
        print(f"  HEAD-TO-HEAD: Condition A vs G")
        print(f"  {'='*80}")
        print(f"  {'Task':35s} {'A':>6s} {'G':>6s} {'A_time':>8s} {'G_time':>8s} "
              f"{'G_subs':>7s} {'Verdict':>10s}")
        print(f"  {'-'*35} {'-'*6} {'-'*6} {'-'*8} {'-'*8} {'-'*7} {'-'*10}")

        a_wins, g_wins, ties = 0, 0, 0
        for r in results:
            task_name = r["task"]
            a = a_trials.get(task_name)
            if not a:
                continue
            a_reward = a.get("reward", 0)
            g_reward = r["reward"]
            a_time = a.get("elapsed_s", 0)
            g_time = r["elapsed_s"]
            g_subs = (r.get("subtask_counts") or {}).get("user_tasks", 0)

            if g_reward > a_reward:
                verdict = "G WINS"
                g_wins += 1
            elif a_reward > g_reward:
                verdict = "A WINS"
                a_wins += 1
            else:
                verdict = "TIE"
                ties += 1
                if g_reward > 0 and g_time < a_time:
                    verdict = "TIE(G-)"
                elif g_reward > 0 and g_time > a_time:
                    verdict = "TIE(A-)"

            print(f"  {task_name:35s} {a_reward:6.1f} {g_reward:6.1f} "
                  f"{a_time:7.1f}s {g_time:7.1f}s "
                  f"{g_subs:7d} {verdict:>10s}")

        print(f"\n  Overall: A wins {a_wins}, G wins {g_wins}, Ties {ties}")
        a_pass = a_data.get("pass_rate", 0)
        g_pass = summary["pass_rate"]
        print(f"  A pass rate: {a_pass:.0%}  |  G pass rate: {g_pass:.0%}  |  "
              f"Delta: {(g_pass - a_pass):+.0%}")
    else:
        print(f"\n  Condition A results not yet available at {a_path}")
        print(f"  Run comparison after both A and G complete.")

    # Also compare with pilot (10-task subset)
    pilot_path = os.path.join(SCRIPT_DIR, "results", "pilot-qwen3-local-10", "summary.json")
    if os.path.isfile(pilot_path):
        with open(pilot_path) as f:
            pilot = json.load(f)
        pilot_tasks = {t["task"]: t for t in pilot.get("trials", [])}
        print(f"\n  Comparison with Condition A pilot (10-task, all passed):")
        for r in results:
            if r["task"] in pilot_tasks:
                pt = pilot_tasks[r["task"]]
                delta_time = r["elapsed_s"] - pt.get("elapsed_s", 0)
                subtasks = (r.get("subtask_counts") or {}).get("user_tasks", 0)
                print(f"    {r['task']:35s} "
                      f"pilot(A): {pt.get('reward', 0):.0f} in {pt.get('elapsed_s', 0):.0f}s / "
                      f"stress(G): {r['reward']:.0f} in {r['elapsed_s']:.0f}s "
                      f"(Δ{delta_time:+.0f}s, {subtasks} subtasks)")

    print(f"\n  Results written to: {json_path}")
    return summary


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="TB Stress Test: Qwen3-Coder-30B, hardest tasks, Condition G (workgraph)")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT,
                        help=f"Per-trial timeout in seconds (default: {DEFAULT_TIMEOUT})")
    parser.add_argument("--tasks", nargs="*", help="Override task list")
    parser.add_argument("--smoke", action="store_true",
                        help="Run single hard task for quick validation")
    parser.add_argument("--hard-only", action="store_true",
                        help="Only run hard-rated tasks (13 tasks)")
    parser.add_argument("--smart", action="store_true",
                        help="Use smart fanout meta-prompt (try-first, decompose-if-needed)")
    args = parser.parse_args()

    tasks = args.tasks
    if args.smoke:
        tasks = ["iterative-test-fix"]
    elif args.hard_only:
        tasks = HARD_BENCHMARK + HARD_CALIBRATION

    summary = asyncio.run(main(
        timeout=args.timeout,
        tasks=tasks,
        smart=args.smart,
    ))
    sys.exit(0 if summary["passed"] > 0 else 1)
