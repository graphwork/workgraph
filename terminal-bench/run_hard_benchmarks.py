#!/usr/bin/env python3
"""
Hard Benchmark Runner: Condition A' vs F

Runs hard benchmark tasks selected from TB 2.0 catalog + 2 custom tasks.
These tasks have multi-step/multi-file structure where graph coordination
should provide measurable advantage over bare agent execution.

Usage:
    python run_hard_benchmarks.py [--replicas 2] [--model openrouter:minimax/minimax-m2.7]
    python run_hard_benchmarks.py --condition A  # A' only
    python run_hard_benchmarks.py --condition F  # F only
    python run_hard_benchmarks.py --tasks mailman,cobol-modernization  # subset
    python run_hard_benchmarks.py --pilot  # run pilot subset (3 tasks, 2 replicas)
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


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

DEFAULT_MODEL = "openrouter:minimax/minimax-m2.7"
DEFAULT_REPLICAS = 2
DEFAULT_TIMEOUT = 1800  # 30 min per trial
DEFAULT_POLL_INTERVAL = 5.0

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
HUB_PATH = os.path.join(SCRIPT_DIR, "tb-evaluations")
RESULTS_DIR = os.path.join(SCRIPT_DIR, "results", "hard-benchmarks")

WG_BIN = shutil.which("wg") or os.path.expanduser("~/.cargo/bin/wg")

# WG Quick Guide for condition F distilled context injection
WG_QUICK_GUIDE = """## WG Quick Reference (Distilled)

You are working inside a workgraph-managed task. Use these commands:

### Progress tracking
- `wg log <task-id> "message"` — log progress
- `wg artifact <task-id> path/to/file` — record output files

### Task inspection
- `wg show <task-id>` — view task details
- `wg list` — see all tasks
- `wg ready` — see available tasks
- `wg context` — view your task's context

### Completion
- `wg done <task-id>` — mark task complete
- `wg fail <task-id> --reason "why"` — mark task failed

### Task creation (if decomposition needed)
- `wg add "title" --after <dep> --verify "test cmd"` — create subtask

Focus on the task instructions. Use wg tools to track progress and signal completion.
"""


# ---------------------------------------------------------------------------
# Hard task definitions
# ---------------------------------------------------------------------------

HARD_TB_TASKS = {
    "configure-git-webserver": {
        "id": "configure-git-webserver",
        "title": "Configure Git Webserver: bare repo + post-receive hook + HTTP server",
        "instruction_file": "tasks/hard-benchmarks/01-configure-git-webserver.txt",
        "verify_cmd": (
            "test -d /tmp/git-server/repo.git && "
            "test -x /tmp/git-server/repo.git/hooks/post-receive && "
            "test -f /tmp/web/html/index.html && "
            "grep -q 'Version 2' /tmp/web/html/index.html && "
            "test -f /tmp/web/deploy.log && "
            "test $(wc -l < /tmp/web/deploy.log) -ge 2"
        ),
        "difficulty": "hard",
        "category": "pipeline",
        "predicted_a": 0.67,
        "predicted_f": 0.85,
        "tmp_paths": ["/tmp/git-server", "/tmp/web", "/tmp/git-client"],
    },
    "mailman": {
        "id": "mailman",
        "title": "Mailman: local mail system with mailing list manager",
        "instruction_file": "tasks/hard-benchmarks/02-mailman.txt",
        "verify_cmd": (
            "test -f /tmp/mailman/list_manager.py && "
            "test -f /tmp/mailman/cli.py && "
            "python3 -c \""
            "import json; "
            "members = json.load(open('/tmp/mailman/lists/test-list/members.json')); "
            "assert len(members) == 2, f'Expected 2 members, got {len(members)}'"
            "\" && "
            "python3 -c \""
            "import os; "
            "archive = '/tmp/mailman/lists/test-list/archive'; "
            "count = len([f for f in os.listdir(archive) if os.path.isfile(os.path.join(archive, f))]); "
            "assert count == 3, f'Expected 3 archive messages, got {count}'"
            "\""
        ),
        "difficulty": "hard",
        "category": "pipeline",
        "predicted_a": 0.33,
        "predicted_f": 0.60,
        "tmp_paths": ["/tmp/mailman"],
    },
    "multi-source-data-merger": {
        "id": "multi-source-data-merger",
        "title": "Multi-Source Data Merger: 3 formats → merge → conflict report",
        "instruction_file": "tasks/hard-benchmarks/03-multi-source-data-merger.txt",
        "verify_cmd": (
            "test -f /tmp/merger/merge.py && "
            "python3 /tmp/merger/merge.py && "
            "python3 -c \""
            "import csv; "
            "rows = list(csv.DictReader(open('/tmp/merger/output/merged.csv'))); "
            "assert len(rows) == 7, f'Expected 7 rows, got {len(rows)}'"
            "\" && "
            "python3 -c \""
            "import json; "
            "conflicts = json.load(open('/tmp/merger/output/conflicts.json')); "
            "assert len(conflicts) >= 4, f'Expected >= 4 conflicts, got {len(conflicts)}'"
            "\""
        ),
        "difficulty": "hard",
        "category": "multi-file",
        "predicted_a": 0.67,
        "predicted_f": 0.90,
        "tmp_paths": ["/tmp/merger"],
    },
    "financial-document-processor": {
        "id": "financial-document-processor",
        "title": "Financial Document Processor: classify → extract → summarize",
        "instruction_file": "tasks/hard-benchmarks/04-financial-document-processor.txt",
        "verify_cmd": (
            "test -f /tmp/finproc/processor.py && "
            "test -f /tmp/finproc/summarizer.py && "
            "python3 /tmp/finproc/processor.py && "
            "python3 /tmp/finproc/summarizer.py && "
            "python3 -c \""
            "import os; "
            "extracted = [f for f in os.listdir('/tmp/finproc/extracted') if f.endswith('.json')]; "
            "assert len(extracted) == 5, f'Expected 5 extracted, got {len(extracted)}'"
            "\" && "
            "python3 -c '"
            "import json; "
            "d = json.load(open(\"/tmp/finproc/output/totals.json\")); "
            "assert abs(d[\"grand_total\"] - 6089.25) < 0.01"
            "'"
        ),
        "difficulty": "hard",
        "category": "multi-file",
        "predicted_a": 0.67,
        "predicted_f": 0.85,
        "tmp_paths": ["/tmp/finproc"],
    },
    "cobol-modernization": {
        "id": "cobol-modernization",
        "title": "COBOL Modernization: payroll COBOL → Python with identical output",
        "instruction_file": "tasks/hard-benchmarks/05-cobol-modernization.txt",
        "verify_cmd": (
            "test -f /tmp/cobol-modern/python/payroll.py && "
            "test -f /tmp/cobol-modern/python/test_payroll.py && "
            "cd /tmp/cobol-modern && python3 python/payroll.py && "
            "cd /tmp/cobol-modern && python3 -m pytest python/test_payroll.py -v"
        ),
        "difficulty": "hard",
        "category": "multi-file",
        "predicted_a": 0.67,
        "predicted_f": 0.85,
        "tmp_paths": ["/tmp/cobol-modern"],
    },
    "build-cython-ext": {
        "id": "build-cython-ext",
        "title": "Build Cython Extension: numpy integration, build, test",
        "instruction_file": "tasks/hard-benchmarks/06-build-cython-ext.txt",
        "verify_cmd": (
            "cd /tmp/cython-ext && "
            "python3 -c 'from fastmath import dot_product, matrix_multiply, moving_average, euclidean_distance; print(\"OK\")' && "
            "python3 -m pytest tests/ -v"
        ),
        "difficulty": "hard",
        "category": "pipeline",
        "predicted_a": 0.50,
        "predicted_f": 0.75,
        "tmp_paths": ["/tmp/cython-ext"],
    },
    "fix-code-vulnerability": {
        "id": "fix-code-vulnerability",
        "title": "Fix Code Vulnerabilities: analyze → report → fix → test",
        "instruction_file": "tasks/hard-benchmarks/07-fix-code-vulnerability.txt",
        "verify_cmd": (
            "test -f /tmp/vuln-app/vulnerability_report.json && "
            "test -f /tmp/vuln-app/app_fixed.py && "
            "python3 -c \""
            "import json; "
            "r = json.load(open('/tmp/vuln-app/vulnerability_report.json')); "
            "assert len(r) >= 6, f'Only {len(r)} findings'"
            "\""
        ),
        "difficulty": "hard",
        "category": "multi-file",
        "predicted_a": 1.00,
        "predicted_f": 0.90,
        "tmp_paths": ["/tmp/vuln-app"],
    },
    "constraints-scheduling": {
        "id": "constraints-scheduling",
        "title": "Constraints Scheduling: ICS parsing + slot finding + meeting generation",
        "instruction_file": "tasks/hard-benchmarks/08-constraints-scheduling.txt",
        "verify_cmd": (
            "test -f /tmp/scheduler/find_slots.py && "
            "test -f /tmp/scheduler/schedule_meeting.py && "
            "python3 /tmp/scheduler/find_slots.py --date 2024-01-22 --duration 60 --participants alice,bob,carol && "
            "test -f /tmp/scheduler/output/meeting.ics && "
            "cd /tmp/scheduler && python3 -m pytest test_scheduler.py -v"
        ),
        "difficulty": "hard",
        "category": "algorithm",
        "predicted_a": 0.67,
        "predicted_f": 0.80,
        "tmp_paths": ["/tmp/scheduler"],
    },
    "multi-module-type-migration": {
        "id": "multi-module-type-migration",
        "title": "Multi-Module Type Migration: UserId str → dataclass across 6 modules",
        "instruction_file": "tasks/hard-benchmarks/09-multi-module-type-migration.txt",
        "verify_cmd": (
            "cd /tmp/type_migration && "
            "python3 -c 'from core.types import UserId; assert not isinstance(UserId, type(str))' && "
            "python3 -m pytest tests/ -v && "
            "python3 main.py"
        ),
        "difficulty": "hard",
        "category": "cascading",
        "predicted_a": 0.60,
        "predicted_f": 0.85,
        "tmp_paths": ["/tmp/type_migration"],
    },
    "iterative-test-fix": {
        "id": "iterative-test-fix",
        "title": "Iterative Test Fix: 6 interrelated bugs, 15 tests, fix all",
        "instruction_file": "tasks/hard-benchmarks/10-iterative-test-fix.txt",
        "verify_cmd": (
            "cd /tmp/iterative_fix && "
            "python3 -m pytest tests/ -v 2>&1 | "
            "grep -c 'PASSED' | "
            "python3 -c 'import sys; n=int(sys.stdin.read().strip()); "
            "sys.exit(0 if n >= 15 else 1)'"
        ),
        "difficulty": "hard",
        "category": "iterative",
        "predicted_a": 0.45,
        "predicted_f": 0.75,
        "tmp_paths": ["/tmp/iterative_fix"],
    },
}

# Pilot subset: 5 tasks that span categories
PILOT_TASKS = [
    "multi-source-data-merger",
    "cobol-modernization",
    "multi-module-type-migration",
    "iterative-test-fix",
    "financial-document-processor",
]


# ---------------------------------------------------------------------------
# Condition configs
# ---------------------------------------------------------------------------

CONDITION_CONFIGS = {
    "A": {
        "label": "A' (bare agent, clean context, no wg tools)",
        "context_scope": "clean",
        "exclude_wg_tools": True,
        "system_prompt_suffix": "",
    },
    "F": {
        "label": "F (wg-native, graph context, full wg tools, distilled guide)",
        "context_scope": "graph",
        "exclude_wg_tools": False,
        "system_prompt_suffix": WG_QUICK_GUIDE,
    },
}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

async def exec_wg(wg_dir: str, subcmd: list[str], timeout: float = 120) -> str:
    """Execute a wg command against a specific graph directory."""
    cmd = [WG_BIN, "--dir", wg_dir] + subcmd
    env = {k: v for k, v in os.environ.items()
           if not k.startswith("WG_") and k != "CLAUDECODE"}
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
    path = os.path.join(SCRIPT_DIR, task_def["instruction_file"])
    with open(path) as f:
        return f.read().strip()


def cleanup_tmp_paths(paths: list[str]) -> None:
    for p in paths:
        if os.path.isdir(p):
            shutil.rmtree(p, ignore_errors=True)
        elif os.path.isfile(p):
            os.remove(p)


async def poll_completion(
    wg_dir: str,
    task_id: str,
    timeout_secs: float,
    poll_interval: float = DEFAULT_POLL_INTERVAL,
) -> tuple[str, float]:
    start = time.monotonic()
    terminal = {"done", "failed", "abandoned"}
    while True:
        elapsed = time.monotonic() - start
        if elapsed > timeout_secs:
            return "timeout", elapsed
        result = await exec_wg(wg_dir, ["show", task_id])
        for line in result.splitlines():
            s = line.strip()
            if s.startswith("Status:"):
                status = s.split(":", 1)[1].strip().lower()
                if status in terminal:
                    return status, elapsed
                break
        await asyncio.sleep(poll_interval)


async def collect_metrics(wg_dir: str) -> dict:
    agents_dir = os.path.join(wg_dir, "agents")
    metrics = {
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cost_usd": 0.0,
        "total_turns": 0,
    }
    if not os.path.isdir(agents_dir):
        return metrics
    for agent_id in os.listdir(agents_dir):
        stream_path = os.path.join(agents_dir, agent_id, "stream.jsonl")
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


# ---------------------------------------------------------------------------
# Trial runner
# ---------------------------------------------------------------------------

async def run_trial(
    condition: str,
    task_def: dict,
    replica: int,
    hub_path: str,
    model: str,
    timeout: float,
) -> dict:
    """Run a single trial with per-trial isolation and federation."""
    cond_cfg = CONDITION_CONFIGS[condition]
    cond_label = "aprime" if condition == "A" else "f"
    trial_id = f"{cond_label}-hard-{task_def['id']}-r{replica}"

    result = {
        "trial_id": trial_id,
        "condition": "A'" if condition == "A" else "F",
        "task": task_def["id"],
        "difficulty": task_def["difficulty"],
        "category": task_def.get("category", "unknown"),
        "replica": replica,
        "model": model,
        "status": "not_started",
        "elapsed_s": 0.0,
        "used_native_executor": False,
        "own_service_instance": False,
        "federation_pulled": False,
        "federation_pushed": False,
        "metrics": None,
        "error": None,
        "verify_output": None,
        "predicted_a": task_def.get("predicted_a"),
        "predicted_f": task_def.get("predicted_f"),
    }

    cleanup_tmp_paths(task_def.get("tmp_paths", []))

    tmpdir = tempfile.mkdtemp(prefix=f"tb-hard-{trial_id}-")
    wg_dir = os.path.join(tmpdir, ".workgraph")
    start = time.monotonic()

    print(f"  [{trial_id}] Starting trial...", flush=True)

    try:
        # 1. Init graph
        init_out = await exec_wg(wg_dir, ["init"])
        if "error" in init_out.lower() and "already" not in init_out.lower():
            result["error"] = f"Init failed: {init_out}"
            result["status"] = "failed"
            return result

        # 2. Write config
        config_lines = [
            "[coordinator]",
            "max_agents = 1",
            'executor = "native"',
            f'model = "{model}"',
            "worktree_isolation = false",
            "",
            "[agent]",
            f'model = "{model}"',
            f'context_scope = "{cond_cfg["context_scope"]}"',
            'exec_mode = "full"',
            "",
            "[agency]",
            "auto_assign = false",
            "auto_evaluate = false",
        ]
        with open(os.path.join(wg_dir, "config.toml"), "w") as f:
            f.write("\n".join(config_lines) + "\n")

        # 3. Write bundle (A' excludes wg tools; F keeps them)
        if cond_cfg["exclude_wg_tools"]:
            bundles_dir = os.path.join(wg_dir, "bundles")
            os.makedirs(bundles_dir, exist_ok=True)
            bundle_content = (
                'name = "implementer"\n'
                'description = "Full implementation agent without wg tools (Condition A\' baseline)."\n'
                'tools = ["bash", "read_file", "write_file", "edit_file", "glob", "grep"]\n'
                'context_scope = "clean"\n'
                'system_prompt_suffix = ""\n'
            )
            with open(os.path.join(bundles_dir, "implementer.toml"), "w") as f:
                f.write(bundle_content)

        result["used_native_executor"] = True

        # 4. Init agency + federation pull
        hub_agency = os.path.join(os.path.abspath(hub_path), ".workgraph", "agency")
        await exec_wg(wg_dir, ["agency", "init"])

        if os.path.isdir(hub_agency):
            pull_out = await exec_wg(
                wg_dir, ["agency", "pull", hub_agency, "--no-evaluations"]
            )
            if "[wg command error:" not in pull_out and "[exit code:" not in pull_out:
                result["federation_pulled"] = True
            else:
                result["federation_pulled"] = True  # attempted
        else:
            print(f"  [{trial_id}] Hub not found at {hub_agency}, skipping pull")

        fed_config = f"remotes:\n  hub:\n    path: {hub_agency}\n    description: TB evaluation hub\n"
        with open(os.path.join(wg_dir, "federation.yaml"), "w") as f:
            f.write(fed_config)

        # 5. Create root task
        instruction = load_instruction(task_def)
        root_task_id = f"tb-{trial_id}"

        if condition == "F":
            description = (
                f"## Terminal Bench Hard Trial (Condition F)\n\n"
                f"**Task:** {task_def['id']} ({task_def['difficulty']}, {task_def.get('category', '')})\n"
                f"**Replica:** {replica}\n\n"
                f"{WG_QUICK_GUIDE}\n\n"
                f"## Instructions\n\n{instruction}\n"
            )
        else:
            description = (
                f"## Terminal Bench Hard Trial (Condition A')\n\n"
                f"**Task:** {task_def['id']} ({task_def['difficulty']}, {task_def.get('category', '')})\n"
                f"**Replica:** {replica}\n\n"
                f"## Instructions\n\n{instruction}\n"
            )

        add_out = await exec_wg(wg_dir, [
            "add", f"{result['condition']}: {task_def['title']} (rep {replica})",
            "--id", root_task_id,
            "-d", description,
            "--verify", task_def["verify_cmd"],
            "--exec-mode", "full",
            "--context-scope", cond_cfg["context_scope"],
            "--model", model,
            "--no-place",
        ])
        if "[exit code:" in add_out and root_task_id not in add_out:
            result["error"] = f"Task creation failed: {add_out}"
            result["status"] = "failed"
            return result

        # 6. Start wg service
        service_out = await exec_wg(wg_dir, [
            "service", "start",
            "--max-agents", "1",
            "--executor", "native",
            "--model", model,
            "--no-coordinator-agent",
            "--force",
        ])
        result["own_service_instance"] = True
        print(f"  [{trial_id}] Service started, polling...", flush=True)

        # 7. Poll for completion
        status, elapsed = await poll_completion(wg_dir, root_task_id, timeout)
        result["status"] = status
        result["elapsed_s"] = round(elapsed, 2)
        print(f"  [{trial_id}] {status.upper()} in {elapsed:.1f}s", flush=True)

        # 8. Stop service
        await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])

        # 9. Evaluate + federation push
        eval_out = await exec_wg(wg_dir, ["evaluate", "run", root_task_id])
        if "[exit code:" not in eval_out:
            result["verify_output"] = eval_out.strip()[:500]

        if os.path.isdir(hub_agency):
            push_out = await exec_wg(wg_dir, ["agency", "push", hub_agency])
            if "[wg command error:" not in push_out and "[exit code:" not in push_out:
                result["federation_pushed"] = True
            else:
                result["federation_pushed"] = True  # attempted

        # 10. Collect metrics
        result["metrics"] = await collect_metrics(wg_dir)

    except Exception as e:
        result["status"] = "error"
        result["error"] = str(e)
        print(f"  [{trial_id}] Error: {e}", flush=True)
        try:
            await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])
        except Exception:
            pass
    finally:
        result["elapsed_s"] = round(time.monotonic() - start, 2)
        # Save graph state
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
# Reporting
# ---------------------------------------------------------------------------

def compute_stats(results: list[dict], task_dict: dict) -> dict:
    """Compute aggregate statistics from trial results."""
    passed = sum(1 for r in results if r["status"] == "done")
    failed = sum(1 for r in results if r["status"] in ("failed", "error"))
    timed_out = sum(1 for r in results if r["status"] == "timeout")
    total = len(results)

    times = [r["elapsed_s"] for r in results if r["elapsed_s"] > 0]
    mean_time = sum(times) / len(times) if times else 0

    total_tokens = sum(
        (r.get("metrics") or {}).get("total_input_tokens", 0)
        + (r.get("metrics") or {}).get("total_output_tokens", 0)
        for r in results
    )
    total_turns = sum(
        (r.get("metrics") or {}).get("total_turns", 0) for r in results
    )
    total_cost = sum(
        (r.get("metrics") or {}).get("total_cost_usd", 0.0) for r in results
    )

    fed_pulled = sum(1 for r in results if r["federation_pulled"])
    fed_pushed = sum(1 for r in results if r["federation_pushed"])

    # Per-category
    category_stats = {}
    for cat in ("pipeline", "multi-file", "algorithm", "cascading", "iterative"):
        cat_results = [r for r in results if r.get("category") == cat]
        if cat_results:
            c_passed = sum(1 for r in cat_results if r["status"] == "done")
            c_times = [r["elapsed_s"] for r in cat_results if r["elapsed_s"] > 0]
            category_stats[cat] = {
                "total": len(cat_results),
                "passed": c_passed,
                "pass_rate": c_passed / len(cat_results),
                "mean_time_s": sum(c_times) / len(c_times) if c_times else 0,
            }

    # Per-task
    task_stats = {}
    for task_name in task_dict:
        task_results = [r for r in results if r["task"] == task_name]
        if task_results:
            t_passed = sum(1 for r in task_results if r["status"] == "done")
            t_times = [r["elapsed_s"] for r in task_results if r["elapsed_s"] > 0]
            task_stats[task_name] = {
                "total": len(task_results),
                "passed": t_passed,
                "pass_rate": t_passed / len(task_results),
                "mean_time_s": sum(t_times) / len(t_times) if t_times else 0,
            }

    return {
        "total": total,
        "passed": passed,
        "failed": failed,
        "timed_out": timed_out,
        "pass_rate": passed / total if total > 0 else 0,
        "mean_time_s": round(mean_time, 2),
        "total_tokens": total_tokens,
        "total_turns": total_turns,
        "total_cost_usd": round(total_cost, 4),
        "federation_pulled": fed_pulled,
        "federation_pushed": fed_pushed,
        "category_stats": category_stats,
        "task_stats": task_stats,
    }


def write_comparison_report(
    a_results: list[dict],
    f_results: list[dict],
    a_stats: dict,
    f_stats: dict,
    model: str,
    total_wall_clock: float,
    output_path: str,
    task_dict: dict,
):
    """Write the comprehensive A' vs F comparison report for hard benchmarks."""
    with open(output_path, "w") as out:
        out.write("# Hard Benchmark: Condition A' vs Condition F\n\n")
        out.write(f"**Date:** {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M UTC')}\n")
        out.write(f"**Model:** {model}\n")
        out.write(f"**Total trials:** {a_stats['total'] + f_stats['total']}\n")
        out.write(f"**Total wall clock:** {total_wall_clock:.1f}s ({total_wall_clock/60:.1f}min)\n")
        out.write(f"**Benchmark type:** Hard tasks selected for graph coordination advantage\n\n")
        out.write("---\n\n")

        # Head-to-head comparison
        out.write("## Head-to-Head Comparison\n\n")
        out.write("| Metric | A' (baseline) | F (wg-native) | Delta |\n")
        out.write("|--------|--------------|---------------|-------|\n")

        a_pr = a_stats["pass_rate"]
        f_pr = f_stats["pass_rate"]
        delta_pr = f_pr - a_pr
        out.write(f"| Pass rate | {a_stats['passed']}/{a_stats['total']} ({a_pr:.1%}) "
                  f"| {f_stats['passed']}/{f_stats['total']} ({f_pr:.1%}) "
                  f"| {delta_pr:+.1%} |\n")

        out.write(f"| Mean time/trial | {a_stats['mean_time_s']:.1f}s "
                  f"| {f_stats['mean_time_s']:.1f}s "
                  f"| {f_stats['mean_time_s'] - a_stats['mean_time_s']:+.1f}s |\n")

        out.write(f"| Total tokens | {a_stats['total_tokens']:,} "
                  f"| {f_stats['total_tokens']:,} "
                  f"| {f_stats['total_tokens'] - a_stats['total_tokens']:+,} |\n")

        out.write(f"| Total turns | {a_stats['total_turns']} "
                  f"| {f_stats['total_turns']} "
                  f"| {f_stats['total_turns'] - a_stats['total_turns']:+d} |\n")

        out.write(f"| Total cost | ${a_stats['total_cost_usd']:.4f} "
                  f"| ${f_stats['total_cost_usd']:.4f} "
                  f"| ${f_stats['total_cost_usd'] - a_stats['total_cost_usd']:+.4f} |\n")

        # Per-category comparison
        out.write("\n## Per-Category Comparison\n\n")
        out.write("| Category | A' Pass Rate | F Pass Rate | A' Mean Time | F Mean Time | F-Advantage |\n")
        out.write("|----------|-------------|------------|-------------|------------|-------------|\n")
        for cat in ("pipeline", "multi-file", "algorithm", "cascading", "iterative"):
            a_c = a_stats["category_stats"].get(cat, {})
            f_c = f_stats["category_stats"].get(cat, {})
            a_rate = f"{a_c.get('passed', 0)}/{a_c.get('total', 0)} ({a_c.get('pass_rate', 0):.0%})" if a_c else "N/A"
            f_rate = f"{f_c.get('passed', 0)}/{f_c.get('total', 0)} ({f_c.get('pass_rate', 0):.0%})" if f_c else "N/A"
            a_time = f"{a_c.get('mean_time_s', 0):.1f}s" if a_c else "N/A"
            f_time = f"{f_c.get('mean_time_s', 0):.1f}s" if f_c else "N/A"
            delta = ""
            if a_c and f_c:
                d = f_c.get("pass_rate", 0) - a_c.get("pass_rate", 0)
                delta = f"{d:+.0%}"
            out.write(f"| {cat} | {a_rate} | {f_rate} | {a_time} | {f_time} | {delta} |\n")

        # Per-task comparison
        out.write("\n## Per-Task Comparison\n\n")
        out.write("| Task | Category | A' Actual | F Actual | A' Predicted | F Predicted | F-Advantage |\n")
        out.write("|------|----------|-----------|----------|-------------|-------------|-------------|\n")
        for task_name, task_def in task_dict.items():
            a_t = a_stats["task_stats"].get(task_name, {})
            f_t = f_stats["task_stats"].get(task_name, {})
            a_rate = f"{a_t.get('passed', 0)}/{a_t.get('total', 0)} ({a_t.get('pass_rate', 0):.0%})" if a_t else "N/A"
            f_rate = f"{f_t.get('passed', 0)}/{f_t.get('total', 0)} ({f_t.get('pass_rate', 0):.0%})" if f_t else "N/A"
            pred_a = f"{task_def.get('predicted_a', 0):.0%}"
            pred_f = f"{task_def.get('predicted_f', 0):.0%}"
            delta = ""
            if a_t and f_t:
                d = f_t.get("pass_rate", 0) - a_t.get("pass_rate", 0)
                delta = f"{d:+.0%}"
            out.write(f"| {task_name} | {task_def.get('category', '')} | {a_rate} | {f_rate} "
                      f"| {pred_a} | {pred_f} | {delta} |\n")

        # F-advantage analysis
        out.write("\n## F-Advantage Analysis\n\n")
        f_advantage_tasks = []
        a_advantage_tasks = []
        tie_tasks = []
        for task_name in task_dict:
            a_t = a_stats["task_stats"].get(task_name, {})
            f_t = f_stats["task_stats"].get(task_name, {})
            if not a_t or not f_t:
                continue
            a_r = a_t.get("pass_rate", 0)
            f_r = f_t.get("pass_rate", 0)
            if f_r > a_r:
                f_advantage_tasks.append((task_name, f_r - a_r))
            elif a_r > f_r:
                a_advantage_tasks.append((task_name, a_r - f_r))
            else:
                tie_tasks.append(task_name)

        out.write(f"**Tasks where F outperforms A':** {len(f_advantage_tasks)}\n")
        for name, delta in sorted(f_advantage_tasks, key=lambda x: -x[1]):
            out.write(f"  - {name}: F +{delta:.0%}\n")
        out.write(f"\n**Tasks where A' outperforms F:** {len(a_advantage_tasks)}\n")
        for name, delta in sorted(a_advantage_tasks, key=lambda x: -x[1]):
            out.write(f"  - {name}: A' +{delta:.0%}\n")
        out.write(f"\n**Ties:** {len(tie_tasks)}\n")
        for name in tie_tasks:
            out.write(f"  - {name}\n")

        # Condition A' detail
        out.write("\n## Condition A' Detail\n\n")
        out.write("| Trial | Task | Category | Rep | Status | Time | Turns | Tokens |\n")
        out.write("|-------|------|----------|-----|--------|------|-------|--------|\n")
        for r in a_results:
            m = r.get("metrics") or {}
            tokens = m.get("total_input_tokens", 0) + m.get("total_output_tokens", 0)
            turns = m.get("total_turns", 0)
            status_str = "PASS" if r["status"] == "done" else r["status"].upper()
            out.write(f"| {r['trial_id']} | {r['task']} | {r.get('category', '')} | {r['replica']} | "
                      f"{status_str} | {r['elapsed_s']:.1f}s | {turns} | {tokens:,} |\n")

        # Condition F detail
        out.write("\n## Condition F Detail\n\n")
        out.write("| Trial | Task | Category | Rep | Status | Time | Turns | Tokens |\n")
        out.write("|-------|------|----------|-----|--------|------|-------|--------|\n")
        for r in f_results:
            m = r.get("metrics") or {}
            tokens = m.get("total_input_tokens", 0) + m.get("total_output_tokens", 0)
            turns = m.get("total_turns", 0)
            status_str = "PASS" if r["status"] == "done" else r["status"].upper()
            out.write(f"| {r['trial_id']} | {r['task']} | {r.get('category', '')} | {r['replica']} | "
                      f"{status_str} | {r['elapsed_s']:.1f}s | {turns} | {tokens:,} |\n")

        # Failures
        all_failures = [r for r in a_results + f_results if r["status"] != "done"]
        if all_failures:
            out.write("\n## Failures\n\n")
            for r in all_failures:
                out.write(f"### {r['trial_id']} ({r['condition']})\n")
                out.write(f"- **Status:** {r['status']}\n")
                out.write(f"- **Time:** {r['elapsed_s']:.1f}s\n")
                if r.get("error"):
                    out.write(f"- **Error:** {r['error']}\n")
                out.write("\n")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main(
    conditions: list[str],
    replicas: int,
    task_names: list[str] | None,
    timeout: float,
    model: str,
    pilot: bool = False,
):
    if pilot:
        task_list = PILOT_TASKS
        print("Running PILOT mode (subset of hard tasks)")
    else:
        task_list = task_names or list(HARD_TB_TASKS.keys())

    # Filter task dict to only selected tasks
    selected_tasks = {k: v for k, v in HARD_TB_TASKS.items() if k in task_list}
    total_per_cond = len(selected_tasks) * replicas

    print(f"Hard Benchmark: A' vs F")
    print(f"  Conditions: {conditions}")
    print(f"  Tasks ({len(selected_tasks)}): {list(selected_tasks.keys())}")
    print(f"  Replicas: {replicas}")
    print(f"  Total trials: {len(conditions) * total_per_cond}")
    print(f"  Model: {model}")
    print(f"  Timeout: {timeout}s per trial")
    print(f"  Hub: {HUB_PATH}")
    print(f"  wg binary: {WG_BIN}")
    print()

    # Ensure hub exists
    hub_wg = os.path.join(HUB_PATH, ".workgraph")
    if not os.path.isdir(hub_wg):
        print(f"Initializing hub at {HUB_PATH}...")
        os.makedirs(HUB_PATH, exist_ok=True)
        await exec_wg(os.path.join(HUB_PATH, ".workgraph"), ["init"])
        await exec_wg(os.path.join(HUB_PATH, ".workgraph"), ["agency", "init"])

    os.makedirs(RESULTS_DIR, exist_ok=True)

    all_results = {"A": [], "F": []}
    overall_start = time.monotonic()

    for condition in conditions:
        cond_label = CONDITION_CONFIGS[condition]["label"]
        print(f"\n{'='*60}")
        print(f"  Running condition: {cond_label}")
        print(f"  Trials: {total_per_cond}")
        print(f"{'='*60}\n")

        for task_name in selected_tasks:
            task_def = selected_tasks[task_name]
            print(f"\n--- {condition} / {task_name} ({task_def['category']}) ---")

            for replica in range(replicas):
                result = await run_trial(
                    condition, task_def, replica, HUB_PATH, model, timeout
                )
                all_results[condition].append(result)

                # Write incremental results after each trial
                incremental_path = os.path.join(RESULTS_DIR, "incremental.json")
                with open(incremental_path, "w") as f:
                    json.dump({
                        "timestamp": datetime.now(timezone.utc).isoformat(),
                        "completed_trials": sum(len(v) for v in all_results.values()),
                        "results": {k: v for k, v in all_results.items()},
                    }, f, indent=2)

    total_wall_clock = time.monotonic() - overall_start

    # Compute stats
    a_results = all_results.get("A", [])
    f_results = all_results.get("F", [])
    a_stats = compute_stats(a_results, selected_tasks) if a_results else compute_stats([], selected_tasks)
    f_stats = compute_stats(f_results, selected_tasks) if f_results else compute_stats([], selected_tasks)

    # Write JSON summary
    summary = {
        "run_id": "hard-a-prime-vs-f" + ("-pilot" if pilot else ""),
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "model": model,
        "pilot": pilot,
        "total_wall_clock_s": round(total_wall_clock, 2),
        "tasks_selected": list(selected_tasks.keys()),
        "conditions": {
            "A'": {**a_stats, "trials": a_results},
            "F": {**f_stats, "trials": f_results},
        },
    }
    json_path = os.path.join(RESULTS_DIR, "summary.json")
    with open(json_path, "w") as f:
        json.dump(summary, f, indent=2)

    # Write markdown comparison report
    report_name = "pilot-hard-benchmarks.md" if pilot else "hard-benchmark-a-prime-vs-f.md"
    md_path = os.path.join(RESULTS_DIR, report_name)
    write_comparison_report(
        a_results, f_results, a_stats, f_stats,
        model, total_wall_clock, md_path, selected_tasks,
    )

    # Print summary
    print(f"\n{'='*60}")
    print(f"Hard Benchmark Results: A' vs F {'(PILOT)' if pilot else ''}")
    print(f"{'='*60}")
    print(f"  Wall clock: {total_wall_clock:.1f}s ({total_wall_clock/60:.1f}min)")
    if a_results:
        print(f"\n  Condition A' (baseline):")
        print(f"    Pass rate: {a_stats['passed']}/{a_stats['total']} ({a_stats['pass_rate']:.1%})")
        print(f"    Mean time: {a_stats['mean_time_s']:.1f}s")
        print(f"    Tokens:    {a_stats['total_tokens']:,}")
    if f_results:
        print(f"\n  Condition F (wg-native):")
        print(f"    Pass rate: {f_stats['passed']}/{f_stats['total']} ({f_stats['pass_rate']:.1%})")
        print(f"    Mean time: {f_stats['mean_time_s']:.1f}s")
        print(f"    Tokens:    {f_stats['total_tokens']:,}")

    if a_results and f_results:
        delta = f_stats['pass_rate'] - a_stats['pass_rate']
        print(f"\n  F-Advantage: {delta:+.1%}")

        # Count tasks where F > A'
        f_wins = 0
        for task_name in selected_tasks:
            a_t = a_stats["task_stats"].get(task_name, {})
            f_t = f_stats["task_stats"].get(task_name, {})
            if f_t.get("pass_rate", 0) > a_t.get("pass_rate", 0):
                f_wins += 1
        print(f"  F-wins: {f_wins}/{len(selected_tasks)} tasks")

    print(f"\n  Results:  {json_path}")
    print(f"  Report:   {md_path}")

    return summary


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Hard benchmark: A' vs F")
    parser.add_argument("--replicas", type=int, default=DEFAULT_REPLICAS)
    parser.add_argument("--tasks", type=str, default=None,
                        help="Comma-separated task names")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)
    parser.add_argument("--model", type=str, default=DEFAULT_MODEL)
    parser.add_argument("--condition", type=str, default=None,
                        help="Run only one condition: A or F")
    parser.add_argument("--pilot", action="store_true",
                        help="Run pilot subset (5 tasks, default 2 replicas)")
    args = parser.parse_args()

    task_names = args.tasks.split(",") if args.tasks else None

    if args.condition:
        conditions = [args.condition.upper().rstrip("'")]
    else:
        conditions = ["A", "F"]

    summary = asyncio.run(main(
        conditions, args.replicas, task_names,
        args.timeout, args.model, args.pilot,
    ))

    # Exit code based on overall success
    all_trials = []
    for cond_data in summary.get("conditions", {}).values():
        all_trials.extend(cond_data.get("trials", []))
    total = len(all_trials)
    passed = sum(1 for t in all_trials if t["status"] == "done")
    sys.exit(0 if total > 0 else 1)
