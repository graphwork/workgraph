#!/usr/bin/env python3
"""
TB Pilot: Qwen3-Coder-30B local (SGLang on lambda01), 10 tasks, Condition G.

Condition G (workgraph-assisted): the seed agent decomposes the task into
subtasks using `wg add`, and the coordinator dispatches worker agents. This
tests whether workgraph decomposition helps (or hurts) compared to Condition A
where a single agent tackles the task directly.

Uses the SAME 10-task subset as run_pilot_qwen3_local_10.py for direct A/G
comparison.

Usage:
    python run_pilot_qwen3_local_10_g.py
    python run_pilot_qwen3_local_10_g.py --smoke    # single task quick check
"""

import argparse
import asyncio
import json
import os
import shutil
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
DEFAULT_TIMEOUT = 2400  # 40 min per trial (matching Condition A for fair comparison)
DEFAULT_POLL_INTERVAL = 5.0

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
RUN_ID = "pilot-qwen3-local-10-g"
RESULTS_DIR = os.path.join(SCRIPT_DIR, "results", RUN_ID)

WG_BIN = shutil.which("wg") or os.path.expanduser("~/.cargo/bin/wg")

# Same 10 tasks as Condition A pilot for direct comparison
PILOT_TASKS = [
    # Easy (2)
    "file-ops",
    "text-processing",
    # Medium (3)
    "debugging",
    "shell-scripting",
    "data-processing",
    # Hard (5)
    "algorithm",
    "ml",
    "sysadmin",
    "configure-git-webserver",
    "mailman",
]

# Per-difficulty time limits (seconds)
# Condition G has significant decomposition overhead, so easy tasks take much
# longer than in Condition A. Use a flat 2400s default for fair comparison.
# These can be overridden per-class if TB metadata specifies limits.
DIFFICULTY_TIMEOUTS = {
    "easy": 2400,    # 40 min (Condition G overhead makes easy tasks slow)
    "medium": 2400,  # 40 min
    "hard": 2400,    # 40 min
}

# ---------------------------------------------------------------------------
# Condition G meta-prompt (from adapter.py)
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

## Important details for sub-task descriptions

Worker agents don't see this prompt. They only see the description you write
in `wg add -d "..."`. So put ALL necessary context in each task's description:
- What files to read/modify
- What the expected output is
- How to verify (test command)
- For the verify task: EXACTLY when to use `--converged` vs plain `wg done`

## After building the graph

Call `wg done {seed_task_id}` to mark this seed task complete. The
coordinator dispatches worker agents to your tasks automatically.

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


def cleanup_tmp_paths(paths: list[str]) -> None:
    """Remove /tmp files from a previous trial to ensure isolation."""
    for p in paths:
        if os.path.isdir(p):
            shutil.rmtree(p, ignore_errors=True)
        elif os.path.isfile(p):
            os.remove(p)


def _parse_task_ids_from_wg_list(output: str) -> list[str]:
    """Extract task IDs from `wg list` output, skipping internal tasks.

    Output format is: [x] task-id - title [tags]
    or: [ ] task-id - title [tags]
    Lines like "No tasks found" are skipped (no checkbox prefix).
    """
    task_ids = []
    for line in output.strip().splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        # Only parse lines with checkbox prefix [x] or [ ]
        if not stripped.startswith("["):
            continue
        parts = stripped.split()
        if len(parts) < 3:
            continue
        # Format: [x] task-id ... or [ ] task-id ...
        if parts[0] == "[x]":
            task_id = parts[1]
        elif parts[0] == "[" and len(parts) >= 3 and parts[1] == "]":
            task_id = parts[2]
        else:
            continue
        # Skip header/border lines
        if task_id.startswith("─") or task_id == "ID":
            continue
        # Skip internal daemon tasks (IDs starting with '.')
        if task_id.startswith("."):
            continue
        task_ids.append(task_id)
    return task_ids


async def poll_graph_quiescence(
    wg_dir: str,
    timeout_secs: float,
    poll_interval: float = DEFAULT_POLL_INTERVAL,
    min_wait_secs: float = 30.0,
) -> tuple[str, float]:
    """Poll until all non-internal tasks reach terminal status.

    For Condition G, we don't track a single task — the architect creates
    subtasks and marks the seed task done. We wait until ALL user tasks
    (excluding internal .coordinator-0, .compact-0, etc.) are terminal.

    min_wait_secs: don't check for quiescence until this many seconds have
    passed, giving the service time to dispatch the first agent.
    """
    start = time.monotonic()
    saw_active = False  # Track if we ever saw an active task

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
            active_ids = _parse_task_ids_from_wg_list(result)
            if active_ids:
                has_active = True
                saw_active = True
                break

        if not has_active:
            # Don't declare quiescence until we've waited at least min_wait_secs
            # (gives the service time to dispatch the first agent) AND we've seen
            # at least one active task before (prevents instant false completion).
            if elapsed < min_wait_secs or not saw_active:
                await asyncio.sleep(poll_interval)
                continue

            # All user tasks are terminal — check if any succeeded
            done_result = await exec_wg(wg_dir, ["list", "--status", "done"])
            if "[exit code:" not in done_result:
                done_ids = _parse_task_ids_from_wg_list(done_result)
                if done_ids:
                    return "done", elapsed
            return "failed", elapsed

        await asyncio.sleep(poll_interval)


async def collect_metrics(wg_dir: str) -> dict:
    """Read agent stream.jsonl files to extract token counts."""
    agents_dir = os.path.join(wg_dir, "agents")
    metrics = {
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cost_usd": 0.0,
        "total_turns": 0,
        "num_agents_spawned": 0,
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
                            metrics["total_input_tokens"] += usage.get("input_tokens", 0)
                            metrics["total_output_tokens"] += usage.get("output_tokens", 0)
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
                    if status == "done":
                        counts["done_tasks"] += 1
                    elif status in ("failed", "abandoned"):
                        counts["failed_tasks"] += 1
    except Exception:
        pass

    return counts


def write_trial_config(wg_dir: str) -> None:
    """Write config.toml for a Condition G trial against lambda01 SGLang.

    Key differences from Condition A:
    - max_agents = 4 (multi-agent for subtask parallelism)
    - context_scope = "graph" (agents see the full graph)
    - coordinator_agent = true (needed for Condition G dispatch)
    - heartbeat_interval = 30 (autonomous coordination)
    """
    config = f"""[coordinator]
max_agents = {MAX_AGENTS}
executor = "native"
model = "{MODEL}"
worktree_isolation = false
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
        "status": "not_started",
        "elapsed_s": 0.0,
        "reward": 0.0,
        "failure_mode": None,
        "metrics": None,
        "subtask_counts": None,
        "error": None,
    }

    # Clean up /tmp paths from previous runs of same task
    cleanup_tmp_paths(task_def.get("tmp_paths", []))

    tmpdir = tempfile.mkdtemp(prefix=f"tb-qwen3-g-{task_id}-")
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
        write_trial_config(wg_dir)

        # 3. Load instruction and build Condition G description
        instruction = load_instruction(task_def)
        root_task_id = f"tb-{task_id}"

        # Build the Condition G meta-prompt with seed task ID and verify cmd
        meta = CONDITION_G_META_PROMPT.replace("{seed_task_id}", root_task_id)
        meta = meta.replace("{max_agents}", str(MAX_AGENTS))

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
            f"## Terminal Bench Trial — Condition G (Qwen3-Coder-30B Local)\n\n"
            f"**Task:** {task_id} ({task_def['difficulty']})\n"
            f"**Model:** {MODEL}\n"
            f"**Endpoint:** {SGLANG_BASE_URL}\n"
            f"**Context Window:** {CONTEXT_WINDOW}\n"
            f"**Condition:** G (workgraph-assisted, autopoietic)\n\n"
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
        import subprocess
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
              f"(reward={result['reward']})")

        # 6. Stop service
        await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])

        # 7. Collect metrics
        result["metrics"] = await collect_metrics(wg_dir)

        # 8. Count subtasks (key Condition G metric)
        result["subtask_counts"] = await count_subtasks(wg_dir)

        # 9. Check daemon log for errors
        daemon_log = os.path.join(wg_dir, "service", "daemon.log")
        if os.path.isfile(daemon_log):
            try:
                with open(daemon_log) as f:
                    log_content = f.read()
                for pattern in ["OOM", "out of memory", "CUDA", "connection refused",
                                "Connection refused"]:
                    if pattern.lower() in log_content.lower():
                        result["failure_mode"] = f"endpoint_error:{pattern}"
                        result["error"] = f"{pattern} detected in daemon log"
                        break
                result["daemon_log_tail"] = log_content[-3000:]
            except Exception:
                pass

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

async def main(timeout: float, tasks: list[str] | None = None):
    task_names = tasks or PILOT_TASKS
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

    print(f"\nTB Pilot: Qwen3-Coder-30B Local (SGLang) — Condition G")
    print(f"  Model: {MODEL}")
    print(f"  Endpoint: {SGLANG_BASE_URL}")
    print(f"  Context window: {CONTEXT_WINDOW}")
    print(f"  Max agents: {MAX_AGENTS}")
    print(f"  Run ID: {RUN_ID}")
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

        print(f"\n--- [{i}/{total}] Task: {task_name} ({task_def['difficulty']}, "
              f"timeout={timeout}s) ---")

        result = await run_trial(task_def, timeout)
        results.append(result)

        # Print running tally
        passed_so_far = sum(1 for r in results if r["reward"] > 0)
        total_subtasks = sum(
            (r.get("subtask_counts") or {}).get("user_tasks", 0) for r in results
        )
        print(f"  Running: {passed_so_far}/{len(results)} passed, "
              f"{total_subtasks} total subtasks created")

    total_time = time.monotonic() - start_time

    # Compute statistics
    passed = sum(1 for r in results if r["reward"] > 0)
    total_trials = len(results)
    times = [r["elapsed_s"] for r in results if r["elapsed_s"] > 0]
    mean_time = sum(times) / len(times) if times else 0
    median_time = sorted(times)[len(times) // 2] if times else 0

    # Token throughput
    total_input = sum(
        r.get("metrics", {}).get("total_input_tokens", 0)
        for r in results if r.get("metrics")
    )
    total_output = sum(
        r.get("metrics", {}).get("total_output_tokens", 0)
        for r in results if r.get("metrics")
    )
    total_turns = sum(
        r.get("metrics", {}).get("total_turns", 0)
        for r in results if r.get("metrics")
    )
    total_agents = sum(
        r.get("metrics", {}).get("num_agents_spawned", 0)
        for r in results if r.get("metrics")
    )

    # Subtask metrics
    total_user_tasks = sum(
        (r.get("subtask_counts") or {}).get("user_tasks", 0)
        for r in results
    )
    avg_subtasks_per_trial = total_user_tasks / total_trials if total_trials else 0

    # Failure mode breakdown
    failure_modes = {}
    for r in results:
        mode = r.get("failure_mode") or "success"
        failure_modes[mode] = failure_modes.get(mode, 0) + 1

    summary = {
        "run_id": RUN_ID,
        "model": MODEL,
        "endpoint": SGLANG_BASE_URL,
        "context_window": CONTEXT_WINDOW,
        "serving_engine": "SGLang",
        "gpu": "RTX 6000 Ada 48GB (lambda01)",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "condition": "G (workgraph-assisted, autopoietic)",
        "max_agents": MAX_AGENTS,
        "timeout_per_trial_s": timeout,
        "tasks": task_names,
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
        "failure_modes": failure_modes,
        "trials": results,
    }

    # Write results
    os.makedirs(RESULTS_DIR, exist_ok=True)

    json_path = os.path.join(RESULTS_DIR, "summary.json")
    with open(json_path, "w") as f:
        json.dump(summary, f, indent=2)

    # Print summary
    print(f"\n{'='*70}")
    print(f"SUMMARY: {RUN_ID} — Condition G (workgraph-assisted)")
    print(f"{'='*70}")
    print(f"  Model: {MODEL}")
    print(f"  Endpoint: {SGLANG_BASE_URL}")
    print(f"  Context window: {CONTEXT_WINDOW}")
    print(f"  Max agents: {MAX_AGENTS}")
    print(f"  Pass rate: {passed}/{total_trials} ({passed/total_trials:.0%})"
          if total_trials else "  No trials")
    print(f"  Mean time per trial: {mean_time:.1f}s")
    print(f"  Median time per trial: {median_time:.1f}s")
    print(f"  Total wall clock: {total_time:.1f}s ({total_time/60:.1f}m)")
    print(f"  Total tokens: {total_input} in + {total_output} out = {total_input + total_output}")
    print(f"  Total turns: {total_turns}")
    print(f"  Total agents spawned: {total_agents}")
    print(f"  Total user tasks created: {total_user_tasks} "
          f"(avg {avg_subtasks_per_trial:.1f}/trial)")
    print(f"  Effective throughput: {summary['tokens_per_second']:.1f} tok/s (output)")
    print(f"  Failure modes: {failure_modes}")
    print()
    print(f"  Per-task results:")
    for r in results:
        turns = r.get("metrics", {}).get("total_turns", 0) if r.get("metrics") else 0
        agents = r.get("metrics", {}).get("num_agents_spawned", 0) if r.get("metrics") else 0
        subtasks = (r.get("subtask_counts") or {}).get("user_tasks", 0)
        print(f"    {r['task']:30s} reward={r['reward']:.1f}  time={r['elapsed_s']:7.1f}s  "
              f"turns={turns:2d}  agents={agents:2d}  subtasks={subtasks:2d}  "
              f"status={r['status']:8s}  "
              f"failure={r.get('failure_mode', 'none')}")
    print()
    print(f"  Results written to: {json_path}")

    return summary


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="TB Pilot: Qwen3-Coder-30B Local — Condition G (workgraph)"
    )
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT,
                        help=f"Per-trial timeout in seconds (default: {DEFAULT_TIMEOUT})")
    parser.add_argument("--tasks", nargs="*", help="Override task list")
    parser.add_argument("--smoke", action="store_true",
                        help="Run single task for quick validation")
    args = parser.parse_args()

    tasks = args.tasks
    if args.smoke:
        tasks = ["text-processing"]

    summary = asyncio.run(main(
        timeout=args.timeout,
        tasks=tasks,
    ))
    sys.exit(0 if summary["passed"] > 0 else 1)
