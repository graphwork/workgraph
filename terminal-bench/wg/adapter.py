"""
Terminal Bench Agent Adapter for Harbor Framework.

Supports two execution modes:
  1. wg-native (default): Installs wg inside the Docker container and runs
     the native executor (wg service start + native-exec) entirely inside
     Docker via environment.exec(). No LiteLLM dependency.
  2. Docker-aware LiteLLM (legacy/deprecated): Python LLM agent loop that
     routes commands through environment.exec(). Kept as fallback.

Supports six conditions:
  Condition A (control): bash + file tools only, no graph, no resume
  Condition B (treatment): full wg tool access, graph awareness, journal/resume
  Condition C (treatment): wg tools + skill injection + planning phase
  Condition D (treatment): wg tools + autopoietic verification + agency identity
  Condition E (treatment): wg tools + organization generation + independent verification
  Condition F (treatment): wg tools + distilled context injection + empirical verification

Model routing end-to-end:
  wg-native path (inside Docker container):
    Harbor -m flag → ConditionXAgent.__init__(model_name=...) → setdefault(BENCHMARK_MODEL)
    → setup(): upload wg binary into container, wg init, write config.toml
    → run(): wg add "task" → wg service start → native-exec agent
    → native_exec.rs: create_provider_ext() parses "openrouter:model" → OpenAI-compat client
    → API calls go directly to OpenRouter (no LiteLLM in path)
    OPENROUTER_API_KEY exported in the shell before wg service start so the
    daemon and its child processes inherit it.

  Host-native path (standalone runners like run_full_a_prime_vs_f.py):
    Runner sets BENCHMARK_MODEL → writes config.toml [agent].model + [coordinator].model
    → wg add --model <model> → wg service start --model <model>
    → coordinator spawns agent via wg native-exec --model <model>
    → native_exec.rs: create_provider_ext() parses "openrouter:model" → OpenAI-compat client
    → API calls go directly to OpenRouter (no LiteLLM in path)

  Environment isolation:
    _exec_wg_cmd_host() strips ALL WG_* env vars + CLAUDECODE from subprocess.
    This prevents parent agent/service model/executor config from leaking into trials.
"""

import asyncio
import base64
import datetime
import json
import logging
import os
import shutil
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any

import yaml

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from wg.tb_logging import TrialLogger
from wg.tasks import lookup_verify_cmd

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Benchmark model — ALL conditions MUST use this for reproducibility
# Format: workgraph-style "provider:model"
# ---------------------------------------------------------------------------
BENCHMARK_MODEL = "openrouter:minimax/minimax-m2.7"

# Default poll interval for task completion checks (seconds)
DEFAULT_POLL_INTERVAL = 2.0

# Default timeout for a single trial (seconds)
DEFAULT_TRIAL_TIMEOUT = 1800  # 30 minutes

# Isolated trial working directory prefix inside the Docker container.
# Some container images share the host's /home/erik filesystem, so wg
# commands run in the default CWD would find the HOST .workgraph/ and
# corrupt it.  A unique directory (prefix + UUID) is created fresh in
# setup() and all wg commands are prefixed with `cd <dir> && ` to
# guarantee isolation.
_TRIAL_WORKDIR_PREFIX = "/var/tmp/tb-trial-"


# ---------------------------------------------------------------------------
# Condition → native wg config mapping
# ---------------------------------------------------------------------------

# Maps each condition to its native executor configuration.
# exec_mode: bare/light/full — controls tool bundle
# context_scope: clean/task/graph/full — controls prompt context assembly
# agency: None, or (role, tradeoff) for D/E
# max_agents: number of parallel agents allowed
CONDITION_CONFIG = {
    "A": {
        "exec_mode": "full",
        "context_scope": "clean",
        "agency": None,
        "exclude_wg_tools": True,
        "max_agents": 1,
    },
    "B": {
        "exec_mode": "full",
        "context_scope": "task",
        "agency": None,
        "exclude_wg_tools": False,
        "max_agents": 1,
    },
    "C": {
        "exec_mode": "full",
        "context_scope": "task",
        "agency": None,
        "exclude_wg_tools": False,
        "max_agents": 1,
    },
    "D": {
        "exec_mode": "full",
        "context_scope": "task",
        "agency": ("programmer", "careful"),
        "exclude_wg_tools": False,
        "max_agents": 1,
    },
    "E": {
        "exec_mode": "full",
        "context_scope": "graph",
        "agency": ("architect", "thorough"),
        "exclude_wg_tools": False,
        "max_agents": 1,
    },
    "F": {
        "exec_mode": "full",
        "context_scope": "graph",
        "agency": None,
        "exclude_wg_tools": False,
        "max_agents": 1,
    },
    "G": {
        "exec_mode": "full",
        "context_scope": "graph",
        "agency": None,
        "exclude_wg_tools": False,
        "max_agents": 8,
        "autopoietic": True,             # Inject architect meta-prompt so seed agent decomposes
        "coordinator_agent": True,        # Phase 3: persistent coordinator agent
        "heartbeat_interval": 30,         # Phase 3: 30s autonomous heartbeat
        # Note: coordinator_model removed — was dead config (not wired into config.toml)
    },
    "G-smart": {
        "exec_mode": "full",
        "context_scope": "graph",
        "agency": None,
        "exclude_wg_tools": False,
        "max_agents": 4,                  # Reduced from 8 — most trials use 1-2 agents
        "autopoietic": True,
        "smart_fanout": True,             # Use try-first smart meta-prompt
        "coordinator_agent": True,
        "heartbeat_interval": 30,
        "worktree_isolation": True,       # Prevent file conflicts between agents
    },
}


# ---------------------------------------------------------------------------
# Host-side wg CLI execution helper
# ---------------------------------------------------------------------------

async def _exec_wg_cmd_host(wg_dir: str, wg_bin: str, subcmd: list[str]) -> str:
    """Execute a wg command on the HOST (not in a container).

    The workgraph state lives on the host in a temp directory per trial.
    """
    cmd = [wg_bin, "--dir", wg_dir] + subcmd
    # Strip ALL WG_* env vars and CLAUDECODE from the parent agent/service
    # so the trial's own config.toml is the sole source of truth for model,
    # executor, and provider settings.
    clean_env = {
        k: v for k, v in os.environ.items()
        if not k.startswith("WG_") and k != "CLAUDECODE"
    }
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=clean_env,
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=60)
        output_parts = []
        if stdout:
            output_parts.append(stdout.decode(errors="replace"))
        if stderr:
            output_parts.append(stderr.decode(errors="replace"))
        if proc.returncode != 0:
            output_parts.append(f"[exit code: {proc.returncode}]")
        return "\n".join(output_parts) if output_parts else "(no output)"
    except asyncio.TimeoutError:
        return "[wg command timed out after 60s]"
    except Exception as e:
        return f"[wg command error: {e}]"


# ---------------------------------------------------------------------------
# Model format normalization
# ---------------------------------------------------------------------------

# Known LiteLLM provider prefixes that use "/" in their model format
# but should use ":" in workgraph format.
_KNOWN_PROVIDERS = {"openrouter", "openai", "anthropic", "together_ai", "groq", "ollama"}


def _normalize_model(model: str) -> str:
    """Normalize a model string to workgraph format (provider:model).

    Harbor and LiteLLM use "/" separators ("openrouter/minimax/minimax-m2.7")
    while workgraph uses ":" ("openrouter:minimax/minimax-m2.7").

    If the model already uses ":" format, it is returned as-is.
    If the first path segment is a known provider, convert the first "/" to ":".
    Otherwise return unchanged.
    """
    if ":" in model:
        return model  # Already in wg format
    parts = model.split("/", 1)
    if len(parts) == 2 and parts[0] in _KNOWN_PROVIDERS:
        return f"{parts[0]}:{parts[1]}"
    return model


# ---------------------------------------------------------------------------
# Trial configuration writers
# ---------------------------------------------------------------------------

async def _write_trial_wg_config(
    trial_dir: str,
    wg_dir: str,
    condition: str,
    model: str,
) -> None:
    """Write .workgraph/config.toml for this trial.

    Configures the coordinator, executor, context scope, and model
    based on the condition.
    """
    cfg = CONDITION_CONFIG[condition]
    config_path = os.path.join(wg_dir, "config.toml")

    lines = [
        "[coordinator]",
        f'max_agents = {cfg["max_agents"]}',
        f'executor = "native"',
        f'model = "{model}"',
        f'worktree_isolation = false',
        "max_verify_failures = 0",
        "max_spawn_failures = 0",
        "",
        "[agent]",
        f'model = "{model}"',
        f'context_scope = "{cfg["context_scope"]}"',
        f'exec_mode = "{cfg["exec_mode"]}"',
        "",
        "[agency]",
        "auto_assign = false",
        "auto_evaluate = false",
        "",
    ]

    with open(config_path, "w") as f:
        f.write("\n".join(lines))


async def _write_trial_bundle(
    wg_dir: str,
    condition: str,
) -> None:
    """Write a custom bundle TOML for conditions that need tool filtering.

    For Condition A, creates a bundle that excludes wg tools entirely.
    For Condition G, creates an architect bundle (read-only + wg tools).
    """
    cfg = CONDITION_CONFIG[condition]

    bundles_dir = os.path.join(wg_dir, "bundles")
    bundle_path = os.path.join(bundles_dir, "implementer.toml")

    if cfg.get("exclude_wg_tools"):
        os.makedirs(bundles_dir, exist_ok=True)
        content = (
            'name = "implementer"\n'
            'description = "Full implementation agent without wg tools (Condition A baseline)."\n'
            'tools = ["bash", "read_file", "write_file", "edit_file", "glob", "grep"]\n'
            'context_scope = "clean"\n'
            'system_prompt_suffix = ""\n'
        )
        with open(bundle_path, "w") as f:
            f.write(content)
    elif cfg.get("smart_fanout"):
        # Smart fanout: agent needs implementation tools (may solve directly)
        os.makedirs(bundles_dir, exist_ok=True)
        with open(bundle_path, "w") as f:
            f.write(SMART_ARCHITECT_BUNDLE_TOML)
    elif cfg.get("autopoietic"):
        os.makedirs(bundles_dir, exist_ok=True)
        with open(bundle_path, "w") as f:
            f.write(ARCHITECT_BUNDLE_TOML)


# ---------------------------------------------------------------------------
# Conditions that use federation (agency conditions only)
# ---------------------------------------------------------------------------

FEDERATION_CONDITIONS = {"D", "E", "F"}


# ---------------------------------------------------------------------------
# Federation helpers
# ---------------------------------------------------------------------------

async def _ensure_hub_initialized(hub_path: str, wg_bin: str) -> None:
    """Initialize the federation hub if it doesn't exist."""
    wg_dir = os.path.join(hub_path, ".workgraph")
    if os.path.isdir(os.path.join(wg_dir, "agency")):
        return  # already initialized

    os.makedirs(hub_path, exist_ok=True)
    await _exec_wg_cmd_host(wg_dir, wg_bin, ["init"])
    await _exec_wg_cmd_host(wg_dir, wg_bin, ["agency", "init"])
    logger.info(f"Initialized federation hub at {hub_path}")


async def _write_trial_federation_config(wg_dir: str, hub_path: str) -> None:
    """Write .workgraph/federation.yaml pointing to the hub."""
    hub_agency = os.path.join(hub_path, ".workgraph", "agency")
    config = {
        "remotes": {
            "hub": {
                "path": hub_agency,
                "description": "TB evaluation hub for federation",
            }
        }
    }
    federation_path = os.path.join(wg_dir, "federation.yaml")
    with open(federation_path, "w") as f:
        yaml.dump(config, f, default_flow_style=False)


async def _federation_pull(wg_dir: str, wg_bin: str, hub_path: str) -> str:
    """Pull roles/tradeoffs/agents from the hub into a trial graph."""
    hub_agency = os.path.join(hub_path, ".workgraph", "agency")
    return await _exec_wg_cmd_host(
        wg_dir, wg_bin,
        ["agency", "pull", hub_agency, "--no-evaluations"],
    )


async def _federation_push(wg_dir: str, wg_bin: str, hub_path: str) -> str:
    """Push evaluations and performance data from a trial graph to the hub."""
    hub_agency = os.path.join(hub_path, ".workgraph", "agency")
    return await _exec_wg_cmd_host(
        wg_dir, wg_bin,
        ["agency", "push", hub_agency],
    )


# ---------------------------------------------------------------------------
# Task completion polling
# ---------------------------------------------------------------------------

async def _poll_task_completion(
    wg_dir: str,
    wg_bin: str,
    task_id: str,
    timeout_secs: float = DEFAULT_TRIAL_TIMEOUT,
    poll_interval: float = DEFAULT_POLL_INTERVAL,
) -> tuple[str, float]:
    """Poll `wg show` until the root task reaches a terminal status.

    Returns (status, elapsed_seconds).
    """
    start = time.monotonic()
    terminal_statuses = {"done", "failed", "abandoned"}

    while True:
        elapsed = time.monotonic() - start
        if elapsed > timeout_secs:
            return "timeout", elapsed

        result = await _exec_wg_cmd_host(wg_dir, wg_bin, ["show", task_id])

        # Parse status from `wg show` output (format: "Status: <status>")
        status = None
        for line in result.splitlines():
            stripped = line.strip()
            if stripped.startswith("Status:"):
                status = stripped.split(":", 1)[1].strip().lower()
                break

        if status and status in terminal_statuses:
            return status, elapsed

        await asyncio.sleep(poll_interval)


# ---------------------------------------------------------------------------
# Metric collection from native executor stream.jsonl
# ---------------------------------------------------------------------------

async def _collect_agent_metrics(wg_dir: str) -> dict[str, Any]:
    """Read agent stream.jsonl files to extract token counts and cost.

    Scans .workgraph/agents/*/stream.jsonl for Result events and
    aggregates usage data.
    """
    agents_dir = os.path.join(wg_dir, "agents")
    metrics: dict[str, Any] = {
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cost_usd": 0.0,
        "total_turns": 0,
        "tool_calls": [],
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

                    event_type = event.get("type")

                    if event_type == "turn":
                        metrics["total_turns"] += 1
                        usage = event.get("usage")
                        if usage:
                            metrics["total_input_tokens"] += usage.get("input_tokens", 0)
                            metrics["total_output_tokens"] += usage.get("output_tokens", 0)
                        tools = event.get("tools_used", [])
                        metrics["tool_calls"].extend(tools)

                    elif event_type == "result":
                        usage = event.get("usage", {})
                        cost = usage.get("cost_usd")
                        if cost:
                            metrics["total_cost_usd"] += cost

        except Exception as e:
            logger.warning(f"Failed to read stream.jsonl for {agent_id}: {e}")

    return metrics


# ---------------------------------------------------------------------------
# Container-based artifact and metric collection (for wg-native path)
# ---------------------------------------------------------------------------

async def _download_wg_artifacts(
    environment: BaseEnvironment,
    target_dir: Path | str,
    trial_workdir: str = "",
) -> Path:
    """Download the entire .workgraph/ directory from the container.

    Tars the directory inside the container, downloads the tarball,
    and extracts it into target_dir/wg-artifacts/.

    Returns the path to the extracted artifacts directory.
    """
    target = Path(target_dir)
    artifacts_dir = target / "wg-artifacts"
    artifacts_dir.mkdir(parents=True, exist_ok=True)

    try:
        # Tar up .workgraph/ inside the container from the isolated trial dir
        wdir = trial_workdir or "."
        tar_result = await environment.exec(
            command=f"cd {wdir} && tar czf /tmp/wg-artifacts.tar.gz .workgraph/ 2>/dev/null || true",
            timeout_sec=60,
        )

        # Download the tarball
        local_tar = str(artifacts_dir / "wg-artifacts.tar.gz")
        await environment.download_file("/tmp/wg-artifacts.tar.gz", local_tar)

        # Extract locally
        import tarfile
        with tarfile.open(local_tar, "r:gz") as tf:
            tf.extractall(path=str(artifacts_dir))

        # Remove the tarball (keep only extracted content)
        os.remove(local_tar)

        logger.info(f"Downloaded wg artifacts to {artifacts_dir}")
    except Exception as e:
        logger.warning(f"Failed to download wg artifacts: {e}")

    return artifacts_dir


# ---------------------------------------------------------------------------
# Condition G: autopoietic meta-prompt
# ---------------------------------------------------------------------------

CONDITION_G_META_PROMPT = """You are a graph architect. You do NOT implement solutions yourself.

Your job:
1. Read the task below and understand what needs to be done
2. Explore the working directory (`ls`, `cat`) to understand the codebase
3. Check `ls tests/` to find the test scripts that verify success
4. Build a workgraph that solves the problem, then mark YOUR task done

DO NOT write code. DO NOT modify files. Only create wg tasks.

## Graph design

Create tasks using `wg add`, then wire them into a self-correcting cycle:

```bash
# 1. Work tasks (parallelize where possible — up to 8 agents run concurrently)
wg add "Implement the solution" --no-place -d "Description of what to do..."

# 2. Verify task (runs after work, checks if tests pass)
wg add "Run tests and verify" --after implement-the-solution --no-place \
  -d "Run the test suite: bash tests/test.sh (or python3 tests/test_outputs.py).
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

## The task to solve

"""


# ---------------------------------------------------------------------------
# Condition G: architect bundle TOML (written into container)
# ---------------------------------------------------------------------------

ARCHITECT_BUNDLE_TOML = """\
name = "bare"
description = "Graph architect agent: reads the problem, designs the workgraph, delegates all implementation."
tools = ["bash", "read_file", "glob", "grep", "wg_show", "wg_list", "wg_add", "wg_done", "wg_fail", "wg_log", "wg_artifact"]
context_scope = "clean"
system_prompt_suffix = ""
"""


# ---------------------------------------------------------------------------
# Condition G: smart fanout meta-prompt (try-first, decompose-if-needed)
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

## If Direct Implementation

Implement the solution. Write code, modify files, run tests.

If tests pass → `wg done {seed_task_id}`

If you notice context pressure during implementation (re-reading files you've
already read, losing track of earlier changes, tool outputs getting truncated),
you may switch to decomposition for the REMAINING work. Log:
```bash
wg log {seed_task_id} "FANOUT_SWITCH: direct→decompose — context pressure after N turns"
```
Then create subtasks for the unfinished portions only.

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
# Condition G: smart architect bundle TOML
# The smart fanout agent needs write_file and edit_file since it may
# implement directly (Strategy 1), unlike the original architect-only bundle.
# ---------------------------------------------------------------------------

SMART_ARCHITECT_BUNDLE_TOML = """\
name = "bare"
description = "Smart fanout agent: tries direct implementation first, decomposes only when needed."
tools = ["bash", "read_file", "write_file", "edit_file", "glob", "grep", "wg_show", "wg_list", "wg_add", "wg_done", "wg_fail", "wg_log", "wg_artifact", "wg_edit"]
context_scope = "clean"
system_prompt_suffix = ""
"""


async def _collect_agent_metrics_from_container(
    environment: BaseEnvironment,
    artifacts_dir: Path | str | None = None,
    trial_workdir: str = "",
) -> dict[str, Any]:
    """Collect metrics from stream.jsonl files.

    If artifacts_dir is provided (already downloaded), reads from there.
    Otherwise downloads from the container.
    """
    # If we already have the artifacts locally, use them directly
    if artifacts_dir is not None:
        wg_dir = os.path.join(str(artifacts_dir), ".workgraph")
        if os.path.isdir(os.path.join(wg_dir, "agents")):
            return await _collect_agent_metrics(wg_dir)

    # Fallback: download just the agents dir from the isolated trial dir
    local_tmp = tempfile.mkdtemp(prefix="tb-metrics-")
    try:
        local_agents = os.path.join(local_tmp, "agents")
        wdir = trial_workdir or "."
        await environment.download_dir(f"{wdir}/.workgraph/agents/", local_agents)
        return await _collect_agent_metrics(local_tmp)
    except Exception as e:
        logger.warning(f"download_dir failed, falling back to exec cat: {e}")
        wdir = trial_workdir or "."
        result = await environment.exec(
            command=f"cat {wdir}/.workgraph/agents/*/stream.jsonl 2>/dev/null || true"
        )
        return _parse_stream_jsonl_text(result.stdout or "")
    finally:
        shutil.rmtree(local_tmp, ignore_errors=True)


def _parse_stream_jsonl_text(text: str) -> dict[str, Any]:
    """Parse stream.jsonl content from a raw text blob (fallback path)."""
    metrics: dict[str, Any] = {
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cost_usd": 0.0,
        "total_turns": 0,
        "tool_calls": [],
    }
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        event_type = event.get("type")
        if event_type == "turn":
            metrics["total_turns"] += 1
            usage = event.get("usage")
            if usage:
                metrics["total_input_tokens"] += usage.get("input_tokens", 0)
                metrics["total_output_tokens"] += usage.get("output_tokens", 0)
            metrics["tool_calls"].extend(event.get("tools_used", []))
        elif event_type == "result":
            usage = event.get("usage", {})
            cost = usage.get("cost_usd")
            if cost:
                metrics["total_cost_usd"] += cost
    return metrics


# ---------------------------------------------------------------------------
# Native executor: run wg inside Docker container
# ---------------------------------------------------------------------------

async def _run_native_executor(
    environment: BaseEnvironment,
    task_instruction: str,
    model: str,
    condition: str,
    trial_workdir: str,
    timeout_secs: float = DEFAULT_TRIAL_TIMEOUT,
    poll_interval: float = DEFAULT_POLL_INTERVAL,
    verify_cmd: str | None = None,
) -> dict[str, Any]:
    """Run the wg native executor entirely inside the Docker container.

    Adds a task to the in-container graph, starts the wg service (which
    forks a daemon that spawns native-exec), and polls until the task
    reaches a terminal status.

    Returns a metrics dict (status, task_id, elapsed_s, plus token/cost
    data from stream.jsonl).
    """
    task_id = f"tb-{uuid.uuid4().hex[:8]}"
    cfg = CONDITION_CONFIG[condition]

    # For Condition G, prepend the autopoietic meta-prompt to the instruction
    # so the agent knows to build a self-correcting workgraph.
    # Also inject the verify command into the meta-prompt so the architect
    # can include it in subtask descriptions (the verify gate on the seed
    # task is auto-deferred by wg done when children are detected, but the
    # architect still needs to know what tests to tell subtasks to run).
    if cfg.get("autopoietic"):
        if cfg.get("smart_fanout"):
            meta = CONDITION_G_SMART_META_PROMPT.replace("{seed_task_id}", task_id)
        else:
            meta = CONDITION_G_META_PROMPT.replace("{seed_task_id}", task_id)
        if verify_cmd:
            meta += (
                f"\n## Test command\n"
                f"The test command that determines pass/fail is:\n"
                f"```\n{verify_cmd}\n```\n"
                f"Include this command in your verify task's description "
                f"so it knows exactly what to run.\n\n"
            )
        full_instruction = meta + task_instruction
    else:
        full_instruction = task_instruction

    # Write the task instruction to a file inside the container using base64
    # encoding to avoid shell quoting and heredoc issues.
    # (Harbor's exec() pipes commands to bash via stdin, which breaks heredocs.)
    b64_instruction = base64.b64encode(
        full_instruction.encode()
    ).decode()
    write_instruction_cmd = (
        f"echo '{b64_instruction}' | base64 -d > /tmp/tb-instruction.txt"
    )
    await environment.exec(command=write_instruction_cmd)

    # Add the task to the graph using the instruction file.
    # --no-place skips the placement pipeline and makes the task immediately
    # available for dispatch (otherwise interactive default is paused/draft).
    # When a verify_cmd is provided, write it to a file and pass --verify so
    # `wg done` automatically gates completion on the test command passing.
    exec_mode_flag = ''
    verify_flag = ''
    if verify_cmd:
        b64_verify = base64.b64encode(verify_cmd.encode()).decode()
        await environment.exec(
            command=f"echo '{b64_verify}' | base64 -d > /tmp/tb-verify-cmd.txt"
        )
        verify_flag = ' --verify "$(cat /tmp/tb-verify-cmd.txt)"'
    add_cmd = (
        f'cd {trial_workdir} && '
        f'wg add "TB task" --id {task_id} --no-place{exec_mode_flag}{verify_flag} '
        f'-d "$(cat /tmp/tb-instruction.txt)"'
    )
    add_result = await environment.exec(command=add_cmd)
    if add_result.return_code != 0:
        logger.error(f"wg add failed: {add_result.stderr}")
        return {
            "status": "setup_error",
            "task_id": task_id,
            "elapsed_s": 0.0,
            "error": f"wg add failed: {add_result.stderr}",
        }

    # Build the env export line.  OPENROUTER_API_KEY must be inherited by
    # the daemon process that wg service start forks.
    api_key = os.environ.get("OPENROUTER_API_KEY", "")
    env_exports = f'export OPENROUTER_API_KEY="{api_key}"'

    # Start the service.  For Condition G (Phase 3 heartbeat), the coordinator
    # agent is always enabled — it orchestrates via heartbeat prompts.
    # For other conditions, --no-coordinator-agent avoids overhead.
    needs_coordinator = cfg.get("coordinator_agent") or cfg.get("autopoietic")
    no_coord = "" if needs_coordinator else " --no-coordinator-agent"
    start_cmd = (
        f'{env_exports} && '
        f'cd {trial_workdir} && '
        f'wg service start --model "{model}"{no_coord}'
    )
    start_result = await environment.exec(command=start_cmd, timeout_sec=30)
    if start_result.return_code != 0:
        logger.error(f"wg service start failed: {start_result.stderr}")
        return {
            "status": "setup_error",
            "task_id": task_id,
            "elapsed_s": 0.0,
            "error": f"wg service start failed: {start_result.stderr}",
        }

    # Poll for task completion.
    # For multi-agent conditions (G), the coordinator creates sub-tasks.
    # We poll until the entire graph is quiescent — no open or in-progress
    # tasks remain. For other conditions, we just poll the single root task.
    start_time = time.monotonic()
    status = "timeout"
    is_autopoietic = cfg.get("autopoietic", False) or cfg.get("coordinator_agent", False)

    while True:
        elapsed = time.monotonic() - start_time
        if elapsed > timeout_secs:
            # Kill the service to avoid burning API credits
            await environment.exec(command=f"cd {trial_workdir} && wg service stop")
            break

        if is_autopoietic:
            # Check if any non-internal tasks are still active (non-terminal).
            # Internal daemon tasks (.coordinator-0, .compact-0, etc.) are
            # perpetually open and must be excluded from the quiescence check.
            # wg list --status only accepts a single value, so query each.
            # Must check ALL non-terminal statuses to avoid premature
            # completion when tasks are blocked, pending-validation, or waiting.
            has_active = False
            for check_status in ("open", "in-progress", "blocked", "pending-validation", "waiting"):
                list_result = await environment.exec(
                    command=f"cd {trial_workdir} && wg list --status {check_status}"
                )
                if list_result.return_code == 0 and list_result.stdout:
                    for line in list_result.stdout.strip().splitlines():
                        stripped = line.strip()
                        if not stripped:
                            continue
                        # Output format: "[ ] task-id - title [tags]"
                        # or table format: "task-id  status  title"
                        # Extract task ID: after checkbox "[ ] " or "[x] "
                        # or as first token if no checkbox prefix.
                        parts = stripped.split()
                        if len(parts) >= 3 and parts[0] in ("[", "[x]"):
                            # Checkbox format: [ ] task-id ...
                            task_id_col = parts[2] if parts[0] == "[" else parts[1]
                        elif parts:
                            task_id_col = parts[0]
                        else:
                            continue
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
                done_result = await environment.exec(
                    command=f"cd {trial_workdir} && wg list --status done"
                )
                if done_result.return_code == 0 and done_result.stdout and done_result.stdout.strip():
                    status = "done"
                else:
                    status = "failed"
                break
        else:
            show_result = await environment.exec(command=f"cd {trial_workdir} && wg show {task_id}")
            if show_result.return_code == 0 and show_result.stdout:
                for line in show_result.stdout.splitlines():
                    stripped = line.strip()
                    if stripped.startswith("Status:"):
                        parsed = stripped.split(":", 1)[1].strip().lower()
                        if parsed in ("done", "failed", "abandoned"):
                            status = parsed
                            break
            if status != "timeout":
                break

        await asyncio.sleep(poll_interval)

    final_elapsed = time.monotonic() - start_time

    # Stop the service cleanly
    await environment.exec(command=f"cd {trial_workdir} && wg service stop")

    # Collect metrics from stream.jsonl inside the container.
    # Caller may provide artifacts_dir if they already downloaded artifacts.
    metrics = await _collect_agent_metrics_from_container(
        environment, trial_workdir=trial_workdir,
    )
    metrics["status"] = status
    metrics["task_id"] = task_id
    metrics["elapsed_s"] = final_elapsed

    return metrics


# ---------------------------------------------------------------------------
# Build config.toml content as a string (for in-container writing)
# ---------------------------------------------------------------------------

def _build_config_toml_content(condition: str, model: str) -> str:
    """Return config.toml content string for the given condition."""
    cfg = CONDITION_CONFIG[condition]
    worktree_iso = "true" if cfg.get("worktree_isolation") else "false"
    lines = [
        "[coordinator]",
        f'max_agents = {cfg["max_agents"]}',
        f'executor = "native"',
        f'model = "{model}"',
        f'worktree_isolation = {worktree_iso}',
        "max_verify_failures = 0",
        "max_spawn_failures = 0",
    ]
    # Phase 3 heartbeat: coordinator_agent + heartbeat_interval
    if cfg.get("coordinator_agent"):
        lines.append("coordinator_agent = true")
    if cfg.get("heartbeat_interval"):
        lines.append(f'heartbeat_interval = {cfg["heartbeat_interval"]}')
    # Graceful completion: inject trial budget so heartbeat shifts to wind-down
    if cfg.get("coordinator_agent"):
        lines.append(f"trial_budget_secs = {int(DEFAULT_TRIAL_TIMEOUT)}")
    lines += [
        "",
        "[agent]",
        f'model = "{model}"',
        f'context_scope = "{cfg["context_scope"]}"',
        f'exec_mode = "{cfg["exec_mode"]}"',
        "",
        "[agency]",
        "auto_assign = false",
        "auto_evaluate = false",
        "",
    ]
    return "\n".join(lines)


def _build_bundle_toml_content() -> str:
    """Return bundle TOML content for Condition A (no wg tools)."""
    return (
        'name = "implementer"\n'
        'description = "Full implementation agent without wg tools (Condition A baseline)."\n'
        'tools = ["bash", "read_file", "write_file", "edit_file", "glob", "grep"]\n'
        'context_scope = "clean"\n'
        'system_prompt_suffix = ""\n'
    )


# ---------------------------------------------------------------------------
# Distilled context guide for Condition F
# ---------------------------------------------------------------------------

WG_QUICK_GUIDE = """## WG Quick Reference (Distilled)

You are working inside a task environment. Complete the task described below.

### Guidelines
- Read the task instructions carefully
- Write code and create files as requested
- Test your work before considering it done
- Focus on correctness and completeness
"""

# ---------------------------------------------------------------------------
# Canonical memory for Condition F — distilled from MEMORY.md
# Gives open models equivalent project knowledge to what Claude gets natively.
# ---------------------------------------------------------------------------

CONDITION_F_MEMORY = """## Workgraph Project Memory (Distilled)

### Architecture
- **Graph storage**: `.workgraph/graph.jsonl` — one JSON object per line, append-only
- **Task lifecycle**: open → in-progress → done | failed | abandoned | blocked | waiting
  - Tasks with `--verify` gates pass through `pending-validation` before `done`
- **Dependencies**: Directed graph. Use `--after <task-id>` to declare edges
- **Service model**: `wg service start` spawns agents on ready tasks
- **Agent isolation**: Each concurrent agent gets its own git worktree

### Key Conventions
- **Task IDs**: kebab-case, auto-generated from title
- **TDD pattern**: Write failing test first, implement until it passes
- **Dependency edges are mandatory**: Use `--after` for every dependent step
- **Verification gates**: `--verify "command"` — must exit 0 for task to complete
- **Same files = sequential edges**: NEVER parallelize tasks modifying the same files

### Project Structure
.workgraph/graph.jsonl — task graph (source of truth)
.workgraph/config.toml — coordinator/agent/model config
.workgraph/agency/ — roles, tradeoffs, agents, evaluations

### Build & Test
- Build: `cargo build`
- Test: `cargo test`
- Install after changes: `cargo install --path .`

### Essential Commands
- `wg add "title" --after dep --verify "cmd"` — create task
- `wg done <id>` / `wg fail <id> --reason "why"` — complete or fail
- `wg log <id> "msg"` — journal progress
- `wg show <id>` / `wg list` / `wg ready` — inspect state

### Common Pitfalls
1. Forgetting `--after` creates race conditions
2. Not running `cargo install --path .` after code changes
3. Flat task lists without dependency edges fail unpredictably
4. Always run `cargo build && cargo test` before marking done
"""


# ---------------------------------------------------------------------------
# DEPRECATED: Docker-aware LLM agent loop (replaced by _run_native_executor)
# Kept as fallback for conditions that haven't been migrated yet.
# ---------------------------------------------------------------------------

# Tool definitions for the LLM (OpenAI function-calling format)
AGENT_TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "bash",
            "description": "Execute a shell command and return stdout + stderr.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute",
                    },
                },
                "required": ["command"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Write content to a file (creates or overwrites).",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to write",
                    },
                    "content": {
                        "type": "string",
                        "description": "File content",
                    },
                },
                "required": ["path", "content"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read the contents of a file.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to read",
                    },
                },
                "required": ["path"],
            },
        },
    },
]


async def _run_docker_agent_loop(
    instruction: str,
    environment: BaseEnvironment,
    model: str,
    condition: str,
    max_turns: int = 9999,
    timeout_secs: float = DEFAULT_TRIAL_TIMEOUT,
    temperature: float = 0.0,
) -> dict[str, Any]:
    """Run an LLM agent loop with commands routed through Docker via environment.exec().

    Model routing:
      1. The ``model`` parameter uses wg format ("openrouter:minimax/minimax-m2.7").
      2. It is converted to LiteLLM format ("openrouter/minimax/minimax-m2.7").
      3. LiteLLM routes to the correct provider using the prefix and the
         matching API key env var (e.g. OPENROUTER_API_KEY).

    Returns metrics dict with turns, tokens, termination info.
    """
    import litellm

    metrics = {
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cost_usd": 0.0,
        "total_turns": 0,
        "termination_type": "max_turns",
        "elapsed_s": 0.0,
        "tool_calls": [],
    }

    # Build system prompt
    cfg = CONDITION_CONFIG[condition]
    system_parts = ["You are a skilled software engineer. Complete the task below."]
    if condition == "F":
        system_parts.append(CONDITION_F_MEMORY)
    elif not cfg.get("exclude_wg_tools"):
        system_parts.append(WG_QUICK_GUIDE)
    system_prompt = "\n\n".join(system_parts)

    # Resolve litellm model name: "openrouter:minimax/minimax-m2.7" → "openrouter/minimax/minimax-m2.7"
    litellm_model = model.replace(":", "/", 1) if ":" in model else model

    # --- Model routing validation ---
    # Verify that the required API key is present for the target provider.
    # This catches misconfigurations early instead of silently falling back
    # to a different model/provider.
    if litellm_model.startswith("openrouter/"):
        api_key = os.environ.get("OPENROUTER_API_KEY", "")
        if not api_key:
            raise RuntimeError(
                f"Model '{litellm_model}' requires OPENROUTER_API_KEY but it is not set. "
                "Set it in the environment before running the trial."
            )
        logger.info(
            f"[model-routing] Using OpenRouter model '{litellm_model}' "
            f"(API key: ...{api_key[-4:]})"
        )
    else:
        logger.info(f"[model-routing] Using LiteLLM model '{litellm_model}'")

    messages = [
        {"role": "system", "content": system_prompt},
        {"role": "user", "content": instruction},
    ]

    start = time.monotonic()

    for turn in range(1, max_turns + 1):
        elapsed = time.monotonic() - start
        if elapsed > timeout_secs:
            metrics["termination_type"] = "timeout"
            break

        metrics["total_turns"] = turn

        try:
            response = await asyncio.wait_for(
                litellm.acompletion(
                    model=litellm_model,
                    messages=messages,
                    tools=AGENT_TOOLS,
                    tool_choice="auto",
                    temperature=temperature,
                    max_tokens=4096,
                ),
                timeout=min(120, timeout_secs - elapsed),
            )
        except asyncio.TimeoutError:
            metrics["termination_type"] = "timeout"
            logger.warning(f"LLM call timed out at turn {turn}")
            break
        except Exception as e:
            logger.error(f"LLM call failed at turn {turn}: {e}")
            metrics["termination_type"] = "llm_error"
            break

        # Verify model routing on first turn — log the model the API actually used
        if turn == 1:
            resp_model = getattr(response, "model", None)
            logger.info(
                f"[model-routing] Turn 1 response model: '{resp_model}' "
                f"(requested: '{litellm_model}')"
            )
            if resp_model and "claude" in str(resp_model).lower() and "minimax" in litellm_model:
                logger.error(
                    f"[model-routing] MODEL MISMATCH: requested '{litellm_model}' "
                    f"but got '{resp_model}' — trial results are invalid!"
                )
                metrics["termination_type"] = "model_mismatch"
                metrics["actual_model"] = resp_model
                break

        # Track tokens
        usage = response.usage
        if usage:
            metrics["total_input_tokens"] += getattr(usage, "prompt_tokens", 0)
            metrics["total_output_tokens"] += getattr(usage, "completion_tokens", 0)

        choice = response.choices[0]
        message = choice.message

        # Add assistant message to history
        messages.append(message.model_dump())

        # Check if the model wants to call tools
        if not message.tool_calls:
            # No tool calls — model is done
            if message.content:
                logger.info(f"Turn {turn}: model finished with text response")
            metrics["termination_type"] = "natural_stop"
            break

        # Execute each tool call
        for tool_call in message.tool_calls:
            fn = tool_call.function
            tool_name = fn.name
            try:
                args = json.loads(fn.arguments)
            except json.JSONDecodeError:
                args = {}

            metrics["tool_calls"].append(tool_name)
            result_text = ""

            try:
                if tool_name == "bash":
                    cmd = args.get("command", "echo 'no command'")
                    exec_result = await asyncio.wait_for(
                        environment.exec(
                            command=cmd,
                            timeout_sec=120,
                            env={"DEBIAN_FRONTEND": "noninteractive"},
                        ),
                        timeout=130,
                    )
                    parts = []
                    if exec_result.stdout:
                        parts.append(exec_result.stdout)
                    if exec_result.stderr:
                        parts.append(exec_result.stderr)
                    if exec_result.return_code != 0:
                        parts.append(f"Exit code: {exec_result.return_code}")
                    result_text = "\n".join(parts) if parts else "(no output)"
                    # Truncate large outputs
                    if len(result_text) > 16000:
                        result_text = result_text[:8000] + "\n...[truncated]...\n" + result_text[-8000:]

                elif tool_name == "write_file":
                    path = args.get("path", "/tmp/output.txt")
                    content = args.get("content", "")
                    # Use heredoc to avoid escaping issues
                    eof_marker = f"WGEOF{uuid.uuid4().hex[:6]}"
                    write_cmd = f"mkdir -p $(dirname '{path}') && cat > '{path}' <<'{eof_marker}'\n{content}\n{eof_marker}"
                    exec_result = await asyncio.wait_for(
                        environment.exec(command=write_cmd, timeout_sec=30),
                        timeout=35,
                    )
                    if exec_result.return_code == 0:
                        result_text = f"Successfully wrote {len(content)} bytes to {path}"
                    else:
                        result_text = f"Error writing {path}: {exec_result.stderr or 'unknown error'}"

                elif tool_name == "read_file":
                    path = args.get("path", "")
                    exec_result = await asyncio.wait_for(
                        environment.exec(command=f"cat '{path}'", timeout_sec=30),
                        timeout=35,
                    )
                    if exec_result.return_code == 0:
                        result_text = exec_result.stdout or "(empty file)"
                    else:
                        result_text = f"Error reading {path}: {exec_result.stderr or 'file not found'}"
                    if len(result_text) > 16000:
                        result_text = result_text[:8000] + "\n...[truncated]...\n" + result_text[-8000:]

                else:
                    result_text = f"Unknown tool: {tool_name}"

            except asyncio.TimeoutError:
                result_text = f"Tool execution timed out"
            except Exception as e:
                result_text = f"Tool error: {e}"

            messages.append({
                "role": "tool",
                "tool_call_id": tool_call.id,
                "content": result_text,
            })

    metrics["elapsed_s"] = time.monotonic() - start
    return metrics


# ---------------------------------------------------------------------------
# WorkgraphAgent — the Harbor BaseAgent implementation
# ---------------------------------------------------------------------------

class WorkgraphAgent(BaseAgent):
    """
    Harbor agent adapter for Terminal Bench evaluation.

    Uses a Docker-aware LLM agent loop: calls the LLM and routes tool
    executions through harbor's environment.exec() into Docker containers.

    Supports seven experimental conditions:
      condition="A" — bare agent (bash + file tools, no graph)
      condition="B" — agent + workgraph (full tools, journal/resume)
      condition="C" — agent + workgraph + skill injection + planning phase
      condition="D" — agent + workgraph + autopoietic verification + agency identity
      condition="G" — autopoietic: agent builds its own self-correcting workgraph
      condition="E" — agent + workgraph + organization generation + independent verification
      condition="F" — agent + workgraph + distilled context injection + empirical verification

    Usage:
        harbor run \\
            --agent-import-path wg.adapter:WorkgraphAgent \\
            -m minimax/minimax-m2.7 \\
            --task-ids task-1 -k 1
    """

    @staticmethod
    def name() -> str:
        return "workgraph"

    def version(self) -> str | None:
        return "0.2.0"

    def __init__(
        self,
        logs_dir: Path | None = None,
        model_name: str | None = None,
        condition: str = "B",
        timeout: float = DEFAULT_TRIAL_TIMEOUT,
        poll_interval: float = DEFAULT_POLL_INTERVAL,
        wg_binary_host_path: str | None = None,
        federation_hub: str | None = None,
        pull_primitives: bool = True,
        push_evaluations: bool = True,
        evolve_after_n: int = 0,
        max_turns: int = 9999,
        temperature: float = 0.0,
        *args,
        **kwargs,
    ):
        if logs_dir is None:
            logs_dir = Path("/tmp/wg-harbor-logs")
            logs_dir.mkdir(parents=True, exist_ok=True)
        super().__init__(logs_dir=logs_dir, model_name=model_name, *args, **kwargs)
        self.condition = condition.upper()
        self.timeout = timeout
        self.poll_interval = poll_interval
        self._wg_binary_host_path = wg_binary_host_path or self._find_wg_binary()
        self.federation_hub = federation_hub
        self.pull_primitives = pull_primitives
        self.push_evaluations = push_evaluations
        self.evolve_after_n = evolve_after_n
        self._max_turns = max_turns
        self._temperature = temperature

    def _find_wg_binary(self) -> str:
        """Locate the wg binary on the host.

        Prefers the bookworm-out build which is compiled against glibc 2.36
        (Debian bookworm) for compatibility with TB Docker containers.
        The host-native binary may require a newer glibc than containers have.
        """
        # Look for bookworm-out build relative to the repo root (works for any user)
        # adapter.py is at terminal-bench/wg/adapter.py, so .parent.parent.parent = repo root
        repo_root = Path(__file__).resolve().parent.parent.parent
        candidates = [
            str(repo_root / "target" / "bookworm-out" / "wg"),
            os.path.expanduser("~/.cargo/bin/wg"),
            str(repo_root / "target" / "release" / "wg"),
            str(repo_root / "target" / "debug" / "wg"),
        ]
        for p in candidates:
            if os.path.isfile(p):
                return p
        return shutil.which("wg") or "wg"

    async def setup(self, environment: BaseEnvironment) -> None:
        """Install wg inside the Docker container and configure the trial graph.

        Steps:
          1. Upload the host wg binary into the container
          2. Run ``wg init`` inside the container
          3. Write config.toml (condition-specific) inside the container
          4. Write custom bundle if needed (Condition A: no wg tools)
          5. Bootstrap agency for conditions D/E (inside the container)
        """
        # Determine model — normalize Harbor format to wg format
        model_raw = self.model_name or BENCHMARK_MODEL
        self._model = _normalize_model(model_raw)
        logger.info(f"[model-routing] Trial model: '{self._model}' (raw from Harbor: '{model_raw}')")

        # Store the environment for use in run() / teardown
        self._environment = environment

        # 1. Upload wg binary into the container
        wg_bin = self._wg_binary_host_path
        # Log binary metadata for diagnosing stale-binary issues (see smoke-test
        # iteration 2: container had Apr 7 binary missing unblock_stuck_tasks).
        try:
            stat = os.stat(wg_bin)
            mtime = datetime.datetime.fromtimestamp(stat.st_mtime, tz=datetime.timezone.utc)
            logger.info(
                f"[binary] Uploading wg binary: {wg_bin} "
                f"(size={stat.st_size}, mtime={mtime.isoformat()})"
            )
        except OSError as e:
            logger.warning(f"[binary] Could not stat wg binary {wg_bin}: {e}")
        await environment.upload_file(wg_bin, "/usr/local/bin/wg")
        await environment.exec(command="chmod +x /usr/local/bin/wg")

        # Verify wg is functional inside the container
        check = await environment.exec(command="wg --version")
        if check.return_code != 0:
            raise RuntimeError(
                f"wg binary not functional inside container: {check.stderr}"
            )
        logger.info(f"wg installed in container: {(check.stdout or '').strip()}")

        # Ensure git is available (wg init requires it)
        git_check = await environment.exec(command="which git || apt-get install -y git 2>/dev/null")
        if git_check.return_code != 0:
            logger.warning("git may not be available in container")

        # 2. Create isolated trial directory and initialize workgraph there.
        #    Some containers share the host filesystem (/home/erik), so the
        #    default CWD may already contain a .workgraph/ from the host.
        #    Using a fresh unique directory guarantees isolation.
        self._trial_workdir = f"{_TRIAL_WORKDIR_PREFIX}{uuid.uuid4().hex[:12]}"
        await environment.exec(command=f"mkdir -p {self._trial_workdir}")
        init_result = await environment.exec(
            command=f"cd {self._trial_workdir} && wg init"
        )
        if init_result.return_code != 0:
            raise RuntimeError(
                f"wg init failed inside container: {init_result.stderr}"
            )
        logger.info(f"Initialized trial workgraph at {self._trial_workdir}")

        # 3. Write config.toml inside the container via base64 encoding.
        #    Heredocs fail because Harbor's exec() pipes commands to bash
        #    via stdin, and heredocs also read from stdin — conflict.
        config_content = _build_config_toml_content(self.condition, self._model)
        b64_config = base64.b64encode(config_content.encode()).decode()
        cfg_write = await environment.exec(
            command=f"echo '{b64_config}' | base64 -d > {self._trial_workdir}/.workgraph/config.toml"
        )
        if cfg_write.return_code != 0:
            raise RuntimeError(
                f"Failed to write config.toml: {cfg_write.stderr}"
            )

        # 4. Write custom bundle if needed
        cfg = CONDITION_CONFIG[self.condition]
        if cfg.get("exclude_wg_tools"):
            # Condition A: bundle without wg tools
            bundle_content = _build_bundle_toml_content()
            b64_bundle = base64.b64encode(bundle_content.encode()).decode()
            await environment.exec(
                command=f"mkdir -p {self._trial_workdir}/.workgraph/bundles"
            )
            bun_write = await environment.exec(
                command=f"echo '{b64_bundle}' | base64 -d > {self._trial_workdir}/.workgraph/bundles/implementer.toml"
            )
            if bun_write.return_code != 0:
                raise RuntimeError(
                    f"Failed to write bundle: {bun_write.stderr}"
                )
        elif cfg.get("autopoietic"):
            # Condition G: architect bundle restricts seed agent to read-only
            # + wg tools (no write_file/edit_file), forcing decomposition
            bundle_content = ARCHITECT_BUNDLE_TOML
            b64_bundle = base64.b64encode(bundle_content.encode()).decode()
            await environment.exec(
                command=f"mkdir -p {self._trial_workdir}/.workgraph/bundles"
            )
            bun_write = await environment.exec(
                command=f"echo '{b64_bundle}' | base64 -d > {self._trial_workdir}/.workgraph/bundles/implementer.toml"
            )
            if bun_write.return_code != 0:
                raise RuntimeError(
                    f"Failed to write architect bundle: {bun_write.stderr}"
                )
            logger.info("Condition G: architect bundle written (tools restricted)")

        # 5. Bootstrap agency for conditions D/E (inside the container)
        if cfg["agency"]:
            await environment.exec(
                command=f"cd {self._trial_workdir} && wg agency init"
            )
            role, tradeoff = cfg["agency"]
            agent_name = "solver" if self.condition == "D" else "orchestrator"
            await environment.exec(
                command=f'cd {self._trial_workdir} && wg agent create {agent_name} --role {role} --tradeoff {tradeoff}'
            )
            self._agent_identity = {
                "name": agent_name,
                "role": role,
                "tradeoff": tradeoff,
            }
            logger.info(f"Condition {self.condition}: agency bootstrapped in container")

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Run wg native executor inside the Docker container."""
        root_task_id = f"tb-{uuid.uuid4().hex[:8]}"

        # Initialize trial logger
        trial_log = TrialLogger(
            logs_dir=self.logs_dir,
            condition=self.condition,
            root_task_id=root_task_id,
            model=self._model,
        )

        trial_log.begin_turn(0)

        # Look up the verify command for this TB task so the wg task gets a
        # --verify gate.  When present, `wg done` will automatically run the
        # test command and keep the task open if it fails — forcing the agent
        # to iterate until tests pass (the pilot F pattern).
        verify_cmd = lookup_verify_cmd(instruction)
        if verify_cmd:
            logger.info(f"Verify gate found for trial: {verify_cmd[:80]}...")
        else:
            logger.warning("No verify gate found for this instruction — task will complete without test gate")

        # Run the native executor inside the container
        trial_workdir = getattr(self, '_trial_workdir', '')
        metrics = await _run_native_executor(
            environment=environment,
            task_instruction=instruction,
            model=self._model,
            condition=self.condition,
            trial_workdir=trial_workdir,
            timeout_secs=self.timeout,
            poll_interval=self.poll_interval,
            verify_cmd=verify_cmd,
        )

        # Download the entire .workgraph/ directory from the container
        # for paper analysis (graph structure, agent logs, service logs, etc.)
        artifacts_dir = await _download_wg_artifacts(
            environment, self.logs_dir, trial_workdir=trial_workdir,
        )

        # If _run_native_executor returned empty metrics (e.g. download_dir
        # failed inside it), try again using the downloaded artifacts.
        if metrics.get("total_turns", 0) == 0 and artifacts_dir is not None:
            artifact_metrics = await _collect_agent_metrics_from_container(
                environment, artifacts_dir=artifacts_dir,
                trial_workdir=trial_workdir,
            )
            # Merge artifact metrics into the main metrics dict (don't
            # overwrite status/task_id/elapsed_s)
            for k in ("total_input_tokens", "total_output_tokens",
                       "total_cost_usd", "total_turns", "tool_calls"):
                if artifact_metrics.get(k):
                    metrics[k] = artifact_metrics[k]

        trial_log.end_turn(had_tool_calls=True)

        trial_log.total_input_tokens = metrics.get("total_input_tokens", 0)
        trial_log.total_output_tokens = metrics.get("total_output_tokens", 0)
        trial_log.total_cost = metrics.get("total_cost_usd", 0.0)
        trial_log.total_turns = metrics.get("total_turns", 0)

        # Map native executor status to termination type
        status = metrics.get("status", "unknown")
        if status == "done":
            trial_log.termination_type = "natural_stop"
        elif status == "failed":
            trial_log.termination_type = "wg_fail"
        elif status == "timeout":
            trial_log.termination_type = "timeout"
        elif status == "setup_error":
            trial_log.termination_type = "llm_error"
        else:
            trial_log.termination_type = status

        elapsed = metrics.get("elapsed_s", 0.0)

        # Build metadata
        metadata: dict[str, Any] = {
            "condition": self.condition,
            "turns": metrics.get("total_turns", 0),
            "root_task_id": root_task_id,
            "wg_task_id": metrics.get("task_id"),
            "model": self._model,
            "termination_type": trial_log.termination_type,
            "elapsed_s": elapsed,
            "native_executor": True,
        }

        cfg = CONDITION_CONFIG[self.condition]
        if cfg["agency"]:
            metadata["agent_identity"] = getattr(self, "_agent_identity", None)

        if metrics.get("error"):
            metadata["error"] = metrics["error"]

        # Populate Harbor's AgentContext
        context.n_input_tokens = metrics.get("total_input_tokens", 0)
        context.n_output_tokens = metrics.get("total_output_tokens", 0)
        context.cost_usd = metrics.get("total_cost_usd", 0.0)
        context.metadata = metadata

        # Write trial summary
        trial_log.write_summary(extra_metadata={
            k: v for k, v in metadata.items()
            if k not in ("condition", "model", "root_task_id")
        })


# ---------------------------------------------------------------------------
# Convenience aliases for condition-specific imports
# ---------------------------------------------------------------------------

class ConditionAAgent(WorkgraphAgent):
    """Condition A (control): bare agent, no workgraph tools."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-a"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "A"
        kwargs.setdefault("model_name", BENCHMARK_MODEL)
        super().__init__(*args, **kwargs)


class ConditionBAgent(WorkgraphAgent):
    """Condition B (treatment): full workgraph tools + journal/resume."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-b"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "B"
        kwargs.setdefault("model_name", BENCHMARK_MODEL)
        super().__init__(*args, **kwargs)


class ConditionCAgent(WorkgraphAgent):
    """Condition C (treatment): wg tools + skill injection + planning phase."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-c"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "C"
        kwargs.setdefault("model_name", BENCHMARK_MODEL)
        super().__init__(*args, **kwargs)


class ConditionDAgent(WorkgraphAgent):
    """Condition D (treatment): wg tools + autopoietic verification + agency."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-d"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "D"
        kwargs.setdefault("model_name", BENCHMARK_MODEL)
        super().__init__(*args, **kwargs)


class ConditionEAgent(WorkgraphAgent):
    """Condition E (treatment): organization generation + independent verification."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-e"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "E"
        kwargs.setdefault("model_name", BENCHMARK_MODEL)
        super().__init__(*args, **kwargs)


class ConditionFAgent(WorkgraphAgent):
    """Condition F (treatment): wg-native agent with full context parity."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-f"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "F"
        kwargs.setdefault("model_name", BENCHMARK_MODEL)
        super().__init__(*args, **kwargs)

    @staticmethod
    def _build_prompt() -> str:
        """Return the assembled system prompt for condition F (for testing)."""
        parts = ["You are a skilled software engineer. Complete the task below."]
        parts.append(CONDITION_F_MEMORY)
        return "\n\n".join(parts)


class ConditionGAgent(WorkgraphAgent):
    """Condition G (treatment): autopoietic — agent builds self-correcting workgraph."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-g"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "G"
        kwargs.setdefault("model_name", BENCHMARK_MODEL)
        super().__init__(*args, **kwargs)


class ConditionGSmartAgent(WorkgraphAgent):
    """Condition G-smart: try-first smart fanout — implements directly, decomposes only when needed."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-g-smart"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "G-smart"
        kwargs.setdefault("model_name", BENCHMARK_MODEL)
        super().__init__(*args, **kwargs)
