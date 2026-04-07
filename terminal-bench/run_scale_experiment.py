#!/usr/bin/env python3
"""
Full-scale parallel experiment runner: Condition A vs F (optionally G).

Runs all 18 custom tasks across conditions A, F, and optionally G with
configurable replicas and concurrency. Supports resume after interruption,
adaptive concurrency ramp-up, and automatic retry of operational failures.

Condition A: bare agent (clean context, no wg tools).
Condition F: full wg context injection (graph context, WG Quick Guide, wg CLI).
Condition G: identical to F (historical ablation label, kept for compatibility).

Design: terminal-bench/docs/scale-experiment-design.md

Usage:
    python run_scale_experiment.py
    python run_scale_experiment.py --conditions A,F --replicas 5 --max-concurrent 8
    python run_scale_experiment.py --resume results/scale-run-001/
    python run_scale_experiment.py --smoke  # 3 tasks x 1 replica x 2 conditions
    python run_scale_experiment.py --conditions A,F,G --replicas 3

Architecture:
    1. Generate trial manifest (tasks x conditions x replicas, randomized)
    2. Execute trials with adaptive semaphore (ramp 4->8 after 20 stable trials)
    3. Per-task mutex for custom tasks sharing /tmp paths
    4. Retry operational failures (DNS, rate limits) up to 2x
    5. Write per-trial results to disk (crash-safe resume)
    6. Generate summary.json + comparison per condition
"""

import argparse
import asyncio
import json
import os
import random
import shutil
import sys
import tempfile
import time
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

DEFAULT_MODEL = "openrouter:minimax/minimax-m2.7"
DEFAULT_REPLICAS = 5
DEFAULT_MAX_CONCURRENT = 8
DEFAULT_INITIAL_CONCURRENT = 4
DEFAULT_RAMP_AFTER = 20
DEFAULT_TIMEOUT = 1800  # 30 min per trial
DEFAULT_POLL_INTERVAL = 5.0
DEFAULT_MAX_RETRIES = 2

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
DEFAULT_RESULTS_BASE = os.path.join(SCRIPT_DIR, "results")

WG_BIN = shutil.which("wg") or os.path.expanduser("~/.cargo/bin/wg")

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
# Task definitions — all 18 custom terminal-bench tasks
# ---------------------------------------------------------------------------

CALIBRATION_TASKS = [
    {
        "id": "file-ops",
        "title": "File Operations: create project structure",
        "instruction_file": "tasks/condition-a-calibration/01-file-ops-easy.txt",
        "verify_cmd": (
            "test -f /tmp/project/src/main.py && "
            "test -f /tmp/project/src/utils.py && "
            "test -f /tmp/project/src/tests/test_utils.py && "
            "test -f /tmp/project/data/config.json && "
            "test -f /tmp/project/README.md && "
            "test -f /tmp/project/.gitignore && "
            "python3 -c \"import json; json.load(open('/tmp/project/data/config.json'))\" && "
            "python3 -m pytest /tmp/project/src/tests/test_utils.py -v"
        ),
        "difficulty": "easy",
        "tmp_paths": ["/tmp/project"],
    },
    {
        "id": "text-processing",
        "title": "Text Processing: word frequency counter",
        "instruction_file": "tasks/condition-a-calibration/02-text-processing-easy.txt",
        "verify_cmd": (
            "test -f /tmp/wordfreq.py && "
            "echo 'the the the dog dog cat' | python3 /tmp/wordfreq.py | head -1 | grep -q 'the'"
        ),
        "difficulty": "easy",
        "tmp_paths": ["/tmp/wordfreq.py"],
    },
    {
        "id": "debugging",
        "title": "Debugging: fix merge sort bugs",
        "instruction_file": "tasks/condition-a-calibration/03-debugging-medium.txt",
        "verify_cmd": (
            "test -f /tmp/buggy_sort.py && "
            "python3 /tmp/buggy_sort.py 2>&1 | grep -v FAIL | grep -c PASS | "
            "python3 -c \"import sys; n=int(sys.stdin.read().strip()); sys.exit(0 if n>=6 else 1)\""
        ),
        "difficulty": "medium",
        "tmp_paths": ["/tmp/buggy_sort.py"],
    },
    {
        "id": "shell-scripting",
        "title": "Shell Scripting: log file analyzer",
        "instruction_file": "tasks/condition-a-calibration/04-shell-scripting-medium.txt",
        "verify_cmd": (
            "test -f /tmp/log_analyzer.sh && "
            "test -f /tmp/access.log && "
            "bash /tmp/log_analyzer.sh /tmp/access.log 2>&1 | grep -qE '[0-9]'"
        ),
        "difficulty": "medium",
        "tmp_paths": ["/tmp/log_analyzer.sh", "/tmp/access.log"],
    },
    {
        "id": "data-processing",
        "title": "Data Processing: JSON to CSV department summary",
        "instruction_file": "tasks/condition-a-calibration/05-data-processing-medium.txt",
        "verify_cmd": (
            "test -f /tmp/json_to_csv.py && "
            "test -f /tmp/employees.json && "
            "test -f /tmp/dept_summary.csv && "
            "python3 -c \"import csv; r=list(csv.DictReader(open('/tmp/dept_summary.csv'))); "
            "assert len(r)>=1\""
        ),
        "difficulty": "medium",
        "tmp_paths": ["/tmp/json_to_csv.py", "/tmp/employees.json", "/tmp/dept_summary.csv"],
    },
    {
        "id": "algorithm",
        "title": "Algorithm: key-value store with transactions",
        "instruction_file": "tasks/condition-a-calibration/06-algorithm-hard.txt",
        "verify_cmd": (
            "test -f /tmp/kvstore.py && test -f /tmp/kv_test.txt && "
            "python3 /tmp/kvstore.py < /tmp/kv_test.txt | head -1 | grep -q '10'"
        ),
        "difficulty": "hard",
        "tmp_paths": ["/tmp/kvstore.py", "/tmp/kv_test.txt"],
    },
    {
        "id": "ml",
        "title": "ML: k-means clustering from scratch",
        "instruction_file": "tasks/condition-a-calibration/07-ml-hard.txt",
        "verify_cmd": (
            "test -f /tmp/kmeans.py && "
            "python3 /tmp/kmeans.py 2>&1 | "
            "python3 -c \"import sys; o=sys.stdin.read().lower(); "
            "sys.exit(0 if 'centroid' in o or 'cluster' in o else 1)\""
        ),
        "difficulty": "hard",
        "tmp_paths": ["/tmp/kmeans.py"],
    },
    {
        "id": "sysadmin",
        "title": "Sysadmin: rate-limited HTTP server",
        "instruction_file": "tasks/condition-a-calibration/08-sysadmin-hard.txt",
        "verify_cmd": (
            "test -f /tmp/ratelimit_server.py && "
            "python3 -c \"import ast; ast.parse(open('/tmp/ratelimit_server.py').read())\" && "
            "grep -q '8765' /tmp/ratelimit_server.py && "
            "grep -q '429' /tmp/ratelimit_server.py && "
            "grep -qi 'rate' /tmp/ratelimit_server.py"
        ),
        "difficulty": "hard",
        "tmp_paths": ["/tmp/ratelimit_server.py"],
    },
]

HARD_BENCHMARK_TASKS = [
    {
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
        "tmp_paths": ["/tmp/git-server", "/tmp/web", "/tmp/git-client"],
    },
    {
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
        "tmp_paths": ["/tmp/mailman"],
    },
    {
        "id": "multi-source-data-merger",
        "title": "Multi-Source Data Merger: 3 formats -> merge -> conflict report",
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
        "tmp_paths": ["/tmp/merger"],
    },
    {
        "id": "financial-document-processor",
        "title": "Financial Document Processor: classify -> extract -> summarize",
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
        "tmp_paths": ["/tmp/finproc"],
    },
    {
        "id": "cobol-modernization",
        "title": "COBOL Modernization: payroll COBOL -> Python with identical output",
        "instruction_file": "tasks/hard-benchmarks/05-cobol-modernization.txt",
        "verify_cmd": (
            "test -f /tmp/cobol-modern/python/payroll.py && "
            "test -f /tmp/cobol-modern/python/test_payroll.py && "
            "cd /tmp/cobol-modern && python3 python/payroll.py && "
            "cd /tmp/cobol-modern && python3 -m pytest python/test_payroll.py -v"
        ),
        "difficulty": "hard",
        "tmp_paths": ["/tmp/cobol-modern"],
    },
    {
        "id": "build-cython-ext",
        "title": "Build Cython Extension: numpy integration, build, test",
        "instruction_file": "tasks/hard-benchmarks/06-build-cython-ext.txt",
        "verify_cmd": (
            "cd /tmp/cython-ext && "
            "python3 -c 'from fastmath import dot_product, matrix_multiply, moving_average, euclidean_distance; print(\"OK\")' && "
            "python3 -m pytest tests/ -v"
        ),
        "difficulty": "hard",
        "tmp_paths": ["/tmp/cython-ext"],
    },
    {
        "id": "fix-code-vulnerability",
        "title": "Fix Code Vulnerabilities: analyze -> report -> fix -> test",
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
        "tmp_paths": ["/tmp/vuln-app"],
    },
    {
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
        "tmp_paths": ["/tmp/scheduler"],
    },
    {
        "id": "multi-module-type-migration",
        "title": "Multi-Module Type Migration: UserId str -> dataclass across 6 modules",
        "instruction_file": "tasks/hard-benchmarks/09-multi-module-type-migration.txt",
        "verify_cmd": (
            "cd /tmp/type_migration && "
            "python3 -c 'from core.types import UserId; assert not isinstance(UserId, type(str))' && "
            "python3 -m pytest tests/ -v && "
            "python3 main.py"
        ),
        "difficulty": "hard",
        "tmp_paths": ["/tmp/type_migration"],
    },
    {
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
        "tmp_paths": ["/tmp/iterative_fix"],
    },
]

ALL_TASKS = CALIBRATION_TASKS + HARD_BENCHMARK_TASKS
TASK_BY_ID = {t["id"]: t for t in ALL_TASKS}


# ---------------------------------------------------------------------------
# Helpers (reused from run_condition_a.py and run_pilot_f_89.py)
# ---------------------------------------------------------------------------

async def exec_wg(wg_dir: str, subcmd: list[str], timeout: float = 120) -> str:
    """Execute a wg command against a specific graph directory.

    Strips all WG_* and CLAUDECODE env vars to prevent parent-service leakage.
    """
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
            try:
                os.remove(p)
            except OSError:
                pass


async def poll_completion(
    wg_dir: str,
    task_id: str,
    timeout_secs: float,
    poll_interval: float = DEFAULT_POLL_INTERVAL,
) -> tuple[str, float]:
    """Poll task status until terminal or timeout."""
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
    """Collect token/turn/cost metrics from agent stream.jsonl files."""
    agents_dir = os.path.join(wg_dir, "agents")
    metrics = {
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cost_usd": 0.0,
        "total_turns": 0,
        "num_agents_spawned": 0,
        "model_used": None,
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
                    if event.get("type") == "init":
                        model = event.get("model")
                        if model:
                            metrics["model_used"] = model
                    elif event.get("type") == "turn":
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


async def run_verify_command(verify_cmd: str, timeout: float = 60) -> tuple[bool, str]:
    """Run a verify command and return (passed, output)."""
    try:
        proc = await asyncio.create_subprocess_shell(
            verify_cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        output = (stdout.decode(errors="replace") + stderr.decode(errors="replace"))[:500]
        return proc.returncode == 0, output
    except Exception as e:
        return False, f"Error running verify: {e}"


# ---------------------------------------------------------------------------
# Network health check
# ---------------------------------------------------------------------------

async def check_api_health() -> bool:
    """Quick probe to OpenRouter API."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
            "https://openrouter.ai/api/v1/models",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
        return stdout.decode().strip() == "200"
    except Exception:
        return False


async def wait_for_api(max_wait: float = 600, check_interval: float = 30) -> bool:
    """Wait for API to become available. Returns False if timeout."""
    start = time.monotonic()
    while time.monotonic() - start < max_wait:
        if await check_api_health():
            return True
        print(f"  API unreachable, retrying in {check_interval}s...", flush=True)
        await asyncio.sleep(check_interval)
    return False


# ---------------------------------------------------------------------------
# Adaptive semaphore
# ---------------------------------------------------------------------------

class AdaptiveSemaphore:
    """Start conservative, increase after confirmed stability."""

    def __init__(self, initial: int = 4, target: int = 8, ramp_after: int = 20):
        self._sem = asyncio.Semaphore(initial)
        self._initial = initial
        self._target = target
        self._ramp_after = ramp_after
        self._completed = 0
        self._errors = 0
        self._ramped = False
        self._lock = asyncio.Lock()

    async def acquire(self):
        await self._sem.acquire()

    async def release(self, success: bool):
        async with self._lock:
            self._completed += 1
            if not success:
                self._errors += 1
            self._sem.release()
            # Ramp up after initial phase if error rate is low
            if (not self._ramped
                    and self._completed >= self._ramp_after
                    and self._errors / self._completed < 0.1
                    and self._initial < self._target):
                self._ramped = True
                extra = self._target - self._initial
                for _ in range(extra):
                    self._sem.release()
                print(f"  [Semaphore] Ramped up: {self._initial} -> {self._target} "
                      f"(after {self._completed} trials, {self._errors} errors)", flush=True)

    @property
    def completed(self) -> int:
        return self._completed

    @property
    def errors(self) -> int:
        return self._errors


# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------

def preflight_checks(model: str) -> bool:
    """Verify all prerequisites before launching experiment."""
    import subprocess

    checks = [
        ("OPENROUTER_API_KEY set", bool(os.environ.get("OPENROUTER_API_KEY"))),
        ("wg binary found", shutil.which("wg") is not None or os.path.isfile(os.path.expanduser("~/.cargo/bin/wg"))),
        ("Python 3.10+", sys.version_info >= (3, 10)),
        ("Disk space > 10GB", shutil.disk_usage("/tmp").free > 10 * 1024**3),
    ]

    # Optional checks (warn but don't block)
    optional = []
    try:
        docker_ok = subprocess.run(
            ["docker", "info"], capture_output=True, timeout=10
        ).returncode == 0
        optional.append(("Docker running", docker_ok))
    except Exception:
        optional.append(("Docker running", False))

    print("Pre-flight checks:")
    all_ok = True
    for name, ok in checks:
        status = "OK" if ok else "FAIL"
        print(f"  [{status}] {name}")
        if not ok:
            all_ok = False

    for name, ok in optional:
        status = "OK" if ok else "WARN"
        print(f"  [{status}] {name} (optional)")

    return all_ok


# ---------------------------------------------------------------------------
# Manifest management
# ---------------------------------------------------------------------------

def generate_manifest(
    conditions: list[str],
    tasks: list[dict],
    replicas: int,
    model: str,
    seed: int | None = None,
) -> dict:
    """Generate a randomized trial manifest."""
    trials = {}
    trial_order = []

    for condition in conditions:
        for task_def in tasks:
            for replica in range(replicas):
                trial_id = f"cond{condition}-{task_def['id']}-r{replica}"
                trials[trial_id] = {
                    "trial_id": trial_id,
                    "condition": condition,
                    "task_id": task_def["id"],
                    "difficulty": task_def["difficulty"],
                    "replica": replica,
                    "status": "pending",
                    "attempts": 0,
                }
                trial_order.append(trial_id)

    # Randomize order
    if seed is not None:
        random.seed(seed)
    random.shuffle(trial_order)

    return {
        "version": 1,
        "created": datetime.now(timezone.utc).isoformat(),
        "conditions": conditions,
        "replicas": replicas,
        "model": model,
        "total_trials": len(trials),
        "seed": seed,
        "trial_order": trial_order,
        "trials": trials,
    }


def load_manifest(path: str) -> dict:
    """Load an existing manifest for resume."""
    with open(path) as f:
        return json.load(f)


def save_manifest(manifest: dict, path: str) -> None:
    """Atomically save manifest to disk."""
    tmp_path = path + ".tmp"
    with open(tmp_path, "w") as f:
        json.dump(manifest, f, indent=2)
    os.replace(tmp_path, path)


def get_pending_trials(manifest: dict) -> list[str]:
    """Return trial IDs that haven't completed successfully."""
    pending = []
    for trial_id in manifest["trial_order"]:
        trial = manifest["trials"][trial_id]
        if trial["status"] not in ("done", "failed_permanent"):
            pending.append(trial_id)
    return pending


# ---------------------------------------------------------------------------
# Condition A trial runner
# ---------------------------------------------------------------------------

async def run_trial_condition_a(
    task_def: dict,
    replica: int,
    model: str,
    trial_id: str,
    results_dir: str,
    timeout: float = DEFAULT_TIMEOUT,
) -> dict:
    """Run a single Condition A trial: bare agent, clean context, no wg tools."""
    result = {
        "trial_id": trial_id,
        "condition": "A",
        "task_id": task_def["id"],
        "difficulty": task_def["difficulty"],
        "replica": replica,
        "model": model,
        "status": "not_started",
        "elapsed_s": 0.0,
        "metrics": None,
        "verify_output": None,
        "surveillance": None,
        "error": None,
    }

    cleanup_tmp_paths(task_def.get("tmp_paths", []))
    tmpdir = tempfile.mkdtemp(prefix=f"tb-{trial_id}-")
    wg_dir = os.path.join(tmpdir, ".workgraph")
    start = time.monotonic()

    try:
        # 1. Init graph
        init_out = await exec_wg(wg_dir, ["init"])
        if "error" in init_out.lower() and "already" not in init_out.lower():
            raise RuntimeError(f"Init failed: {init_out}")

        # 2. Write locked config (condition A: clean context, native executor)
        config = (
            "[coordinator]\n"
            "max_agents = 1\n"
            'executor = "native"\n'
            f'model = "{model}"\n'
            "worktree_isolation = false\n"
            'agent_timeout = "30m"\n'
            "max_verify_failures = 0\n"
            "max_spawn_failures = 0\n"
            "\n"
            "[agent]\n"
            f'model = "{model}"\n'
            'context_scope = "clean"\n'
            'exec_mode = "full"\n'
            "\n"
            "[agency]\n"
            "auto_assign = false\n"
            "auto_evaluate = false\n"
        )
        with open(os.path.join(wg_dir, "config.toml"), "w") as f:
            f.write(config)

        # 3. Create root task
        instruction = load_instruction(task_def)
        root_task_id = f"tb-{trial_id}"
        description = (
            f"## Terminal Bench Trial (Condition A)\n\n"
            f"**Task:** {task_def['id']} ({task_def['difficulty']})\n"
            f"**Replica:** {replica}\n\n"
            f"## Instructions\n\n{instruction}\n"
        )
        add_out = await exec_wg(wg_dir, [
            "add", f"A: {task_def['title']} (rep {replica})",
            "--id", root_task_id,
            "-d", description,
            "--verify", task_def["verify_cmd"],
            "--exec-mode", "full",
            "--context-scope", "clean",
            "--model", model,
            "--no-place",
        ])
        if "[exit code:" in add_out and root_task_id not in add_out:
            raise RuntimeError(f"Task creation failed: {add_out}")

        # 4. Start isolated service
        await exec_wg(wg_dir, [
            "service", "start",
            "--max-agents", "1",
            "--executor", "native",
            "--model", model,
            "--no-coordinator-agent",
            "--force",
        ])

        # 5. Poll for completion
        status, elapsed = await poll_completion(wg_dir, root_task_id, timeout)
        result["status"] = status
        result["elapsed_s"] = round(elapsed, 2)

        # 6. Stop service
        await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])

        # 7. Collect metrics
        result["metrics"] = await collect_metrics(wg_dir)

        # 8. External verify
        passed, output = await run_verify_command(task_def["verify_cmd"])
        result["verify_output"] = output
        if passed and result["status"] != "done":
            result["status"] = "done"

    except Exception as e:
        result["status"] = "error"
        result["error"] = str(e)
        try:
            await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])
        except Exception:
            pass
    finally:
        result["elapsed_s"] = round(time.monotonic() - start, 2)
        # Preserve graph state
        state_dst = os.path.join(results_dir, trial_id, "workgraph_state")
        try:
            os.makedirs(os.path.dirname(state_dst), exist_ok=True)
            if os.path.isdir(wg_dir):
                shutil.copytree(wg_dir, state_dst)
        except Exception:
            pass
        shutil.rmtree(tmpdir, ignore_errors=True)

    return result


# ---------------------------------------------------------------------------
# Condition F trial runner
# ---------------------------------------------------------------------------

async def run_trial_condition_f(
    task_def: dict,
    replica: int,
    model: str,
    trial_id: str,
    results_dir: str,
    timeout: float = DEFAULT_TIMEOUT,
    condition_label: str = "F",
) -> dict:
    """Run a single Condition F/G trial: wg-native + graph context, no surveillance."""
    task_id = task_def["id"]
    work_task_id = f"work-{task_id}"

    result = {
        "trial_id": trial_id,
        "condition": condition_label,
        "task_id": task_id,
        "difficulty": task_def["difficulty"],
        "replica": replica,
        "model": model,
        "status": "not_started",
        "elapsed_s": 0.0,
        "metrics": None,
        "verify_output": None,
        "surveillance": None,
        "error": None,
    }

    cleanup_tmp_paths(task_def.get("tmp_paths", []))
    tmpdir = tempfile.mkdtemp(prefix=f"tb-{trial_id}-")
    wg_dir = os.path.join(tmpdir, ".workgraph")
    start = time.monotonic()

    try:
        # 1. Init graph
        init_out = await exec_wg(wg_dir, ["init"])
        if "error" in init_out.lower() and "already" not in init_out.lower():
            raise RuntimeError(f"Init failed: {init_out}")

        # 2. Write config (graph context, native executor, single agent)
        config = (
            "[coordinator]\n"
            "max_agents = 1\n"
            'executor = "native"\n'
            f'model = "{model}"\n'
            "worktree_isolation = false\n"
            "\n"
            "[agent]\n"
            f'model = "{model}"\n'
            'context_scope = "graph"\n'
            'exec_mode = "full"\n'
            "\n"
            "[agency]\n"
            "auto_assign = false\n"
            "auto_evaluate = false\n"
        )
        with open(os.path.join(wg_dir, "config.toml"), "w") as f:
            f.write(config)

        # 3. Create WORK task with full wg context
        instruction = load_instruction(task_def)
        work_description = (
            f"## Terminal Bench Trial (Condition {condition_label} — wg-native)\n\n"
            f"**Task:** {task_id} ({task_def['difficulty']})\n\n"
            f"{WG_QUICK_GUIDE}\n\n"
            f"## Instructions\n\n{instruction}\n"
        )
        add_work = await exec_wg(wg_dir, [
            "add", f"Work: {task_def['title']}",
            "--id", work_task_id,
            "-d", work_description,
            "--verify", task_def["verify_cmd"],
            "--exec-mode", "full",
            "--context-scope", "graph",
            "--model", model,
            "--no-place",
        ])
        if "[exit code:" in add_work and work_task_id not in add_work:
            raise RuntimeError(f"Work task creation failed: {add_work}")

        # 4. Start wg service
        await exec_wg(wg_dir, [
            "service", "start",
            "--max-agents", "1",
            "--executor", "native",
            "--model", model,
            "--no-coordinator-agent",
            "--force",
        ])

        # 5. Poll for completion
        status, elapsed = await poll_completion(wg_dir, work_task_id, timeout)
        result["status"] = status
        result["elapsed_s"] = round(elapsed, 2)

        # 6. Stop service
        await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])

        # 7. Collect metrics
        result["metrics"] = await collect_metrics(wg_dir)

        # 8. External verify
        passed, output = await run_verify_command(task_def["verify_cmd"])
        result["verify_output"] = output
        if passed and result["status"] != "done":
            result["status"] = "done"

    except Exception as e:
        result["status"] = "error"
        result["error"] = str(e)
        try:
            await exec_wg(wg_dir, ["service", "stop", "--kill-agents"])
        except Exception:
            pass
    finally:
        result["elapsed_s"] = round(time.monotonic() - start, 2)
        # Preserve graph state
        state_dst = os.path.join(results_dir, trial_id, "workgraph_state")
        try:
            os.makedirs(os.path.dirname(state_dst), exist_ok=True)
            if os.path.isdir(wg_dir):
                shutil.copytree(wg_dir, state_dst)
        except Exception:
            pass
        shutil.rmtree(tmpdir, ignore_errors=True)

    return result


# ---------------------------------------------------------------------------
# Failure classification
# ---------------------------------------------------------------------------

def is_operational_failure(result: dict) -> bool:
    """Distinguish operational failures from model failures."""
    error = (result.get("error") or "").lower()
    status = result.get("status", "")
    return status in ("error", "timeout") and any(keyword in error for keyword in [
        "dns", "connection", "rate_limit", "429", "503", "timeout",
        "network", "socket", "ssl", "init failed",
    ])


# ---------------------------------------------------------------------------
# Trial dispatcher
# ---------------------------------------------------------------------------

async def run_trial_dispatched(
    trial_info: dict,
    model: str,
    results_dir: str,
    timeout: float,
) -> dict:
    """Dispatch a trial to the appropriate condition runner."""
    condition = trial_info["condition"]
    task_def = TASK_BY_ID[trial_info["task_id"]]
    replica = trial_info["replica"]
    trial_id = trial_info["trial_id"]

    if condition == "A":
        return await run_trial_condition_a(
            task_def, replica, model, trial_id, results_dir, timeout
        )
    elif condition in ("F", "G"):
        return await run_trial_condition_f(
            task_def, replica, model, trial_id, results_dir, timeout,
            condition_label=condition,
        )
    else:
        raise ValueError(f"Unknown condition: {condition}")


async def run_trial_with_retry(
    trial_info: dict,
    model: str,
    results_dir: str,
    timeout: float,
    max_retries: int = DEFAULT_MAX_RETRIES,
) -> dict:
    """Run a trial with retry on operational failures."""
    for attempt in range(max_retries + 1):
        result = await run_trial_dispatched(trial_info, model, results_dir, timeout)

        if result["status"] == "done":
            return result

        if is_operational_failure(result) and attempt < max_retries:
            backoff = 30 * (2 ** attempt)
            print(f"  [{trial_info['trial_id']}] Operational failure (attempt {attempt + 1}), "
                  f"retrying in {backoff}s: {result.get('error', 'unknown')}", flush=True)

            # Health check before retry
            if not await check_api_health():
                print(f"  [{trial_info['trial_id']}] API unhealthy, waiting...", flush=True)
                if not await wait_for_api(max_wait=300):
                    result["error"] = f"API unreachable after retry wait. Original: {result.get('error')}"
                    return result

            await asyncio.sleep(backoff)
            continue

        # Model failure or exhausted retries
        return result

    return result


# ---------------------------------------------------------------------------
# Progress reporting
# ---------------------------------------------------------------------------

class ProgressReporter:
    """Track and display experiment progress."""

    def __init__(self, total_trials: int, conditions: list[str]):
        self._total = total_trials
        self._conditions = conditions
        self._completed = 0
        self._results: list[dict] = []
        self._start = time.monotonic()
        self._lock = asyncio.Lock()

    async def record(self, result: dict):
        async with self._lock:
            self._completed += 1
            self._results.append(result)
            self._print_progress(result)

    def _print_progress(self, latest: dict):
        elapsed = time.monotonic() - self._start
        rate = elapsed / self._completed if self._completed > 0 else 0
        remaining = (self._total - self._completed) * rate

        # Per-condition stats
        cond_parts = []
        for cond in self._conditions:
            cond_results = [r for r in self._results if r["condition"] == cond]
            passed = sum(1 for r in cond_results if r["status"] == "done")
            total = len(cond_results)
            if total > 0:
                cond_parts.append(f"{cond}:{passed}/{total}")

        overall_passed = sum(1 for r in self._results if r["status"] == "done")
        overall_rate = overall_passed / self._completed if self._completed > 0 else 0

        status_str = "PASS" if latest["status"] == "done" else latest["status"].upper()
        tokens = 0
        if latest.get("metrics"):
            tokens = (latest["metrics"].get("total_input_tokens", 0)
                      + latest["metrics"].get("total_output_tokens", 0))

        eta_str = f"{remaining/3600:.1f}h" if remaining > 3600 else f"{remaining/60:.0f}m"

        print(
            f"[{self._completed:>4}/{self._total}] {self._completed/self._total:.1%} "
            f"| {' '.join(cond_parts)} "
            f"| Pass: {overall_rate:.1%} "
            f"| ETA: {eta_str} "
            f"| {latest['trial_id']} {status_str} ({latest['elapsed_s']:.0f}s, {tokens:,} tok)",
            flush=True,
        )

    @property
    def results(self) -> list[dict]:
        return list(self._results)


# ---------------------------------------------------------------------------
# Results collection and analysis
# ---------------------------------------------------------------------------

def compute_condition_stats(results: list[dict], condition: str) -> dict:
    """Compute aggregate stats for a single condition."""
    cond_results = [r for r in results if r["condition"] == condition]
    if not cond_results:
        return {}

    passed = sum(1 for r in cond_results if r["status"] == "done")
    total = len(cond_results)
    times = [r["elapsed_s"] for r in cond_results if r["elapsed_s"] > 0]

    total_input = sum((r.get("metrics") or {}).get("total_input_tokens", 0) for r in cond_results)
    total_output = sum((r.get("metrics") or {}).get("total_output_tokens", 0) for r in cond_results)
    total_cost = sum((r.get("metrics") or {}).get("total_cost_usd", 0.0) for r in cond_results)
    total_turns = sum((r.get("metrics") or {}).get("total_turns", 0) for r in cond_results)
    total_agents = sum((r.get("metrics") or {}).get("num_agents_spawned", 0) for r in cond_results)

    # Per-difficulty
    difficulty_stats = {}
    for diff in ("easy", "medium", "hard"):
        diff_results = [r for r in cond_results if r.get("difficulty") == diff]
        if diff_results:
            d_passed = sum(1 for r in diff_results if r["status"] == "done")
            d_times = [r["elapsed_s"] for r in diff_results if r["elapsed_s"] > 0]
            difficulty_stats[diff] = {
                "total": len(diff_results),
                "passed": d_passed,
                "pass_rate": d_passed / len(diff_results),
                "mean_time_s": round(sum(d_times) / len(d_times), 2) if d_times else 0,
            }

    # Per-task
    task_stats = {}
    for task_def in ALL_TASKS:
        task_results = [r for r in cond_results if r.get("task_id") == task_def["id"]]
        if task_results:
            t_passed = sum(1 for r in task_results if r["status"] == "done")
            t_times = [r["elapsed_s"] for r in task_results if r["elapsed_s"] > 0]
            task_stats[task_def["id"]] = {
                "total": len(task_results),
                "passed": t_passed,
                "pass_rate": t_passed / len(task_results),
                "mean_time_s": round(sum(t_times) / len(t_times), 2) if t_times else 0,
            }

    return {
        "condition": condition,
        "total": total,
        "passed": passed,
        "failed": total - passed,
        "pass_rate": passed / total if total > 0 else 0,
        "mean_time_s": round(sum(times) / len(times), 2) if times else 0,
        "token_stats": {
            "total_input_tokens": total_input,
            "total_output_tokens": total_output,
            "total_tokens": total_input + total_output,
            "total_cost_usd": round(total_cost, 4),
            "total_turns": total_turns,
            "total_agents_spawned": total_agents,
            "mean_tokens_per_trial": round(
                (total_input + total_output) / total, 0
            ) if total else 0,
        },
        "difficulty_stats": difficulty_stats,
        "task_stats": task_stats,
    }


def write_summary(
    results: list[dict],
    conditions: list[str],
    model: str,
    replicas: int,
    wall_clock_s: float,
    results_dir: str,
) -> dict:
    """Write unified summary.json and per-condition summaries."""
    condition_stats = {
        cond: compute_condition_stats(results, cond)
        for cond in conditions
    }

    summary = {
        "run_id": os.path.basename(results_dir),
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "model": model,
        "replicas": replicas,
        "conditions": conditions,
        "unique_tasks": len(ALL_TASKS),
        "total_trials": len(results),
        "total_wall_clock_s": round(wall_clock_s, 2),
        "condition_stats": condition_stats,
        "trials": results,
    }

    # Write unified summary
    summary_path = os.path.join(results_dir, "summary.json")
    with open(summary_path, "w") as f:
        json.dump(summary, f, indent=2)

    # Write per-condition summaries
    for cond in conditions:
        cond_dir = os.path.join(results_dir, f"condition-{cond}")
        os.makedirs(cond_dir, exist_ok=True)
        cond_summary = {
            "condition": cond,
            "timestamp": summary["timestamp"],
            "model": model,
            "replicas": replicas,
            "stats": condition_stats.get(cond, {}),
            "trials": [r for r in results if r["condition"] == cond],
        }
        with open(os.path.join(cond_dir, "summary.json"), "w") as f:
            json.dump(cond_summary, f, indent=2)

    return summary


def write_comparison_report(summary: dict, results_dir: str) -> str:
    """Write a markdown comparison report."""
    path = os.path.join(results_dir, "comparison.md")
    conditions = summary["conditions"]
    cstats = summary["condition_stats"]

    with open(path, "w") as out:
        out.write("# Scale Experiment: Condition Comparison\n\n")
        out.write(f"**Date:** {summary['timestamp']}\n")
        out.write(f"**Model:** {summary['model']}\n")
        out.write(f"**Replicas:** {summary['replicas']}\n")
        out.write(f"**Tasks:** {summary['unique_tasks']}\n")
        out.write(f"**Total trials:** {summary['total_trials']}\n")
        out.write(f"**Wall clock:** {summary['total_wall_clock_s']:.0f}s "
                  f"({summary['total_wall_clock_s']/3600:.1f}h)\n\n")

        # Overall comparison table
        out.write("## Overall Results\n\n")
        out.write("| Metric |")
        for c in conditions:
            out.write(f" {c} |")
        out.write("\n|--------|")
        for _ in conditions:
            out.write("------|")
        out.write("\n")

        for metric, fmt in [
            ("pass_rate", lambda s: f"{s.get('pass_rate', 0):.1%} ({s.get('passed', 0)}/{s.get('total', 0)})"),
            ("mean_time_s", lambda s: f"{s.get('mean_time_s', 0):.1f}s"),
            ("tokens/trial", lambda s: f"{s.get('token_stats', {}).get('mean_tokens_per_trial', 0):,.0f}"),
            ("total_cost", lambda s: f"${s.get('token_stats', {}).get('total_cost_usd', 0):.4f}"),
        ]:
            out.write(f"| {metric} |")
            for c in conditions:
                out.write(f" {fmt(cstats.get(c, {}))} |")
            out.write("\n")

        # Per-difficulty comparison
        out.write("\n## Per-Difficulty Results\n\n")
        for diff in ("easy", "medium", "hard"):
            out.write(f"\n### {diff.title()}\n\n")
            out.write("| Condition | Pass Rate | Mean Time |\n")
            out.write("|-----------|-----------|----------|\n")
            for c in conditions:
                d = cstats.get(c, {}).get("difficulty_stats", {}).get(diff, {})
                if d:
                    out.write(f"| {c} | {d['passed']}/{d['total']} ({d['pass_rate']:.0%}) "
                              f"| {d['mean_time_s']:.1f}s |\n")
                else:
                    out.write(f"| {c} | N/A | N/A |\n")

        # Per-task comparison
        out.write("\n## Per-Task Results\n\n")
        out.write("| Task | Difficulty |")
        for c in conditions:
            out.write(f" {c} Pass Rate |")
        out.write("\n|------|-----------|")
        for _ in conditions:
            out.write("------------|")
        out.write("\n")

        for task_def in ALL_TASKS:
            tid = task_def["id"]
            out.write(f"| {tid} | {task_def['difficulty']} |")
            for c in conditions:
                t = cstats.get(c, {}).get("task_stats", {}).get(tid, {})
                if t:
                    out.write(f" {t['passed']}/{t['total']} ({t['pass_rate']:.0%}) |")
                else:
                    out.write(" N/A |")
            out.write("\n")

    return path


# ---------------------------------------------------------------------------
# Main experiment loop
# ---------------------------------------------------------------------------

async def run_experiment(
    conditions: list[str],
    tasks: list[dict],
    replicas: int,
    model: str,
    max_concurrent: int,
    initial_concurrent: int,
    ramp_after: int,
    timeout: float,
    max_retries: int,
    results_dir: str,
    resume_manifest: dict | None = None,
    seed: int | None = None,
):
    """Run the full-scale experiment."""
    os.makedirs(results_dir, exist_ok=True)
    manifest_path = os.path.join(results_dir, "manifest.json")

    # Generate or load manifest
    if resume_manifest:
        manifest = resume_manifest
        print(f"Resuming from {manifest_path}")
        print(f"  Completed: {sum(1 for t in manifest['trials'].values() if t['status'] in ('done', 'failed_permanent'))}"
              f"/{manifest['total_trials']}")
    else:
        manifest = generate_manifest(conditions, tasks, replicas, model, seed)
        save_manifest(manifest, manifest_path)

    # Save config
    config = {
        "conditions": conditions,
        "replicas": replicas,
        "model": model,
        "max_concurrent": max_concurrent,
        "initial_concurrent": initial_concurrent,
        "ramp_after": ramp_after,
        "timeout": timeout,
        "max_retries": max_retries,
        "unique_tasks": len(tasks),
        "total_trials": manifest["total_trials"],
        "seed": seed,
        "started": datetime.now(timezone.utc).isoformat(),
    }
    with open(os.path.join(results_dir, "config.json"), "w") as f:
        json.dump(config, f, indent=2)

    # Get pending trials
    pending_ids = get_pending_trials(manifest)
    total_pending = len(pending_ids)

    if total_pending == 0:
        print("All trials already completed. Nothing to do.")
        # Load existing results
        all_results = []
        for trial_id, trial in manifest["trials"].items():
            result_path = os.path.join(results_dir, f"{trial_id}.json")
            if os.path.isfile(result_path):
                with open(result_path) as f:
                    all_results.append(json.load(f))
        return all_results

    print(f"\nScale Experiment")
    print(f"  Conditions: {conditions}")
    print(f"  Tasks: {len(tasks)}")
    print(f"  Replicas: {replicas}")
    print(f"  Total trials: {manifest['total_trials']}")
    print(f"  Pending: {total_pending}")
    print(f"  Concurrency: {initial_concurrent} -> {max_concurrent} (after {ramp_after} trials)")
    print(f"  Model: {model}")
    print(f"  Timeout: {timeout}s per trial")
    print(f"  Max retries: {max_retries}")
    print(f"  Results: {results_dir}")
    print()

    # Setup adaptive semaphore
    semaphore = AdaptiveSemaphore(
        initial=min(initial_concurrent, max_concurrent),
        target=max_concurrent,
        ramp_after=ramp_after,
    )

    # Per-task mutex for /tmp isolation (custom tasks share /tmp paths)
    task_locks = defaultdict(asyncio.Lock)

    # Progress reporter
    reporter = ProgressReporter(total_pending, conditions)

    overall_start = time.monotonic()

    # Load already-completed results
    all_results = []
    for trial_id, trial in manifest["trials"].items():
        if trial["status"] in ("done", "failed_permanent"):
            result_path = os.path.join(results_dir, f"{trial_id}.json")
            if os.path.isfile(result_path):
                with open(result_path) as f:
                    all_results.append(json.load(f))

    async def run_one(trial_id: str):
        trial_info = manifest["trials"][trial_id]
        task_id = trial_info["task_id"]

        await semaphore.acquire()
        try:
            # Per-task mutex for custom tasks (shared /tmp paths)
            async with task_locks[task_id]:
                result = await run_trial_with_retry(
                    trial_info, model, results_dir, timeout, max_retries
                )
        finally:
            success = result["status"] == "done"
            await semaphore.release(success)

        # Save per-trial result to disk (crash-safe)
        result_path = os.path.join(results_dir, f"{trial_id}.json")
        with open(result_path, "w") as f:
            json.dump(result, f, indent=2)

        # Update manifest
        manifest["trials"][trial_id]["status"] = (
            "done" if result["status"] == "done"
            else "failed_permanent" if not is_operational_failure(result)
            else "failed_operational"
        )
        manifest["trials"][trial_id]["attempts"] = (
            manifest["trials"][trial_id].get("attempts", 0) + 1
        )
        save_manifest(manifest, manifest_path)

        all_results.append(result)
        await reporter.record(result)

        return result

    # Launch all pending trials
    tasks_coros = [run_one(tid) for tid in pending_ids]
    await asyncio.gather(*tasks_coros)

    wall_clock = time.monotonic() - overall_start

    # Write summaries
    summary = write_summary(
        all_results, conditions, model, replicas, wall_clock, results_dir
    )
    report_path = write_comparison_report(summary, results_dir)

    # Print final report
    print(f"\n{'='*70}")
    print(f"  EXPERIMENT COMPLETE")
    print(f"{'='*70}")
    print(f"  Wall clock: {wall_clock:.1f}s ({wall_clock/3600:.1f}h)")
    print(f"  Total trials: {len(all_results)}")
    for cond in conditions:
        cs = summary["condition_stats"].get(cond, {})
        print(f"  Condition {cond}: {cs.get('passed', 0)}/{cs.get('total', 0)} "
              f"({cs.get('pass_rate', 0):.1%}) "
              f"| mean {cs.get('mean_time_s', 0):.1f}s "
              f"| {cs.get('token_stats', {}).get('total_tokens', 0):,} tokens")

    print(f"\n  Results:    {os.path.join(results_dir, 'summary.json')}")
    print(f"  Report:     {report_path}")
    print(f"  Manifest:   {manifest_path}")
    print(f"{'='*70}")

    return all_results


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Full-scale parallel experiment: Condition A vs F (vs G)"
    )
    parser.add_argument("--conditions", type=str, default="A,F",
                        help="Comma-separated conditions to test (default: A,F)")
    parser.add_argument("--replicas", type=int, default=DEFAULT_REPLICAS,
                        help=f"Replicas per task per condition (default: {DEFAULT_REPLICAS})")
    parser.add_argument("--tasks", type=str, default=None,
                        help="Comma-separated task IDs (default: all 18)")
    parser.add_argument("--max-concurrent", type=int, default=DEFAULT_MAX_CONCURRENT,
                        help=f"Max concurrent trials (default: {DEFAULT_MAX_CONCURRENT})")
    parser.add_argument("--initial-concurrent", type=int, default=DEFAULT_INITIAL_CONCURRENT,
                        help=f"Initial concurrent trials before ramp-up (default: {DEFAULT_INITIAL_CONCURRENT})")
    parser.add_argument("--ramp-after", type=int, default=DEFAULT_RAMP_AFTER,
                        help=f"Ramp up concurrency after N trials (default: {DEFAULT_RAMP_AFTER})")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT,
                        help=f"Timeout per trial in seconds (default: {DEFAULT_TIMEOUT})")
    parser.add_argument("--max-retries", type=int, default=DEFAULT_MAX_RETRIES,
                        help=f"Max retries for operational failures (default: {DEFAULT_MAX_RETRIES})")
    parser.add_argument("--model", type=str, default=DEFAULT_MODEL,
                        help=f"Model to use (default: {DEFAULT_MODEL})")
    parser.add_argument("--results-dir", type=str, default=None,
                        help="Results directory (default: auto-generated)")
    parser.add_argument("--resume", type=str, default=None,
                        help="Resume from results directory (loads manifest.json)")
    parser.add_argument("--seed", type=int, default=None,
                        help="Random seed for trial order (default: random)")
    parser.add_argument("--smoke", action="store_true",
                        help="Smoke test: 3 tasks x 1 replica x 2 conditions")
    parser.add_argument("--skip-preflight", action="store_true",
                        help="Skip preflight checks")
    args = parser.parse_args()

    # Smoke test overrides
    if args.smoke:
        conditions = ["A", "F"]
        task_names = ["file-ops", "text-processing", "debugging"]
        replicas = 1
        max_concurrent = 2
        initial_concurrent = 2
        ramp_after = 999
    else:
        conditions = [c.strip() for c in args.conditions.split(",")]
        task_names = [t.strip() for t in args.tasks.split(",")] if args.tasks else None
        replicas = args.replicas
        max_concurrent = args.max_concurrent
        initial_concurrent = args.initial_concurrent
        ramp_after = args.ramp_after

    # Validate conditions
    valid_conditions = {"A", "F", "G"}
    for c in conditions:
        if c not in valid_conditions:
            sys.exit(f"Invalid condition: {c}. Must be one of {valid_conditions}")

    # Filter tasks
    if task_names:
        tasks = []
        for name in task_names:
            if name not in TASK_BY_ID:
                sys.exit(f"Unknown task: {name}. Available: {list(TASK_BY_ID.keys())}")
            tasks.append(TASK_BY_ID[name])
    else:
        tasks = ALL_TASKS

    # Preflight checks
    if not args.skip_preflight and not args.resume:
        if not preflight_checks(args.model):
            sys.exit("Pre-flight checks failed. Fix the issues above and retry, "
                     "or use --skip-preflight to skip.")
        print()

    # Results directory
    if args.resume:
        results_dir = args.resume.rstrip("/")
        manifest_path = os.path.join(results_dir, "manifest.json")
        if not os.path.isfile(manifest_path):
            sys.exit(f"No manifest.json found in {results_dir}")
        resume_manifest = load_manifest(manifest_path)
    else:
        if args.results_dir:
            results_dir = args.results_dir
        else:
            run_num = 1
            while True:
                results_dir = os.path.join(
                    DEFAULT_RESULTS_BASE,
                    f"scale-run-{run_num:03d}"
                )
                if not os.path.exists(results_dir):
                    break
                run_num += 1
        resume_manifest = None

    # Run experiment
    all_results = asyncio.run(run_experiment(
        conditions=conditions,
        tasks=tasks,
        replicas=replicas,
        model=args.model,
        max_concurrent=max_concurrent,
        initial_concurrent=initial_concurrent,
        ramp_after=ramp_after,
        timeout=args.timeout,
        max_retries=args.max_retries,
        results_dir=results_dir,
        resume_manifest=resume_manifest,
        seed=args.seed,
    ))

    # Exit code based on overall success
    if not all_results:
        sys.exit(1)
    passed = sum(1 for r in all_results if r["status"] == "done")
    sys.exit(0 if passed / len(all_results) >= 0.3 else 1)


if __name__ == "__main__":
    main()
