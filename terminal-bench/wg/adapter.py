"""
Terminal Bench Agent Adapter for Harbor Framework.

Delegates execution to workgraph's native executor via `wg service start`.
The adapter is a thin orchestrator: it creates a per-trial graph, starts
the native service, polls for task completion, and collects metrics.

Supports six conditions:
  Condition A (control): bash + file tools only, no graph, no resume
  Condition B (treatment): full wg tool access, graph awareness, journal/resume
  Condition C (treatment): wg tools + skill injection + planning phase
  Condition D (treatment): wg tools + autopoietic verification + agency identity
  Condition E (treatment): wg tools + organization generation + independent verification
  Condition F (treatment): wg tools + distilled context injection + empirical verification
"""

import asyncio
import json
import logging
import os
import shutil
import time
import uuid
from pathlib import Path
from typing import Any

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from wg.tb_logging import TrialLogger

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
}


# ---------------------------------------------------------------------------
# Host-side wg CLI execution helper
# ---------------------------------------------------------------------------

async def _exec_wg_cmd_host(wg_dir: str, wg_bin: str, subcmd: list[str]) -> str:
    """Execute a wg command on the HOST (not in a container).

    The workgraph state lives on the host in a temp directory per trial.
    """
    cmd = [wg_bin, "--dir", wg_dir] + subcmd
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
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
        "",
        "[agent]",
        f'context_scope = "{cfg["context_scope"]}"',
        f'exec_mode = "{cfg["exec_mode"]}"',
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
    """
    cfg = CONDITION_CONFIG[condition]
    if not cfg.get("exclude_wg_tools"):
        return

    bundles_dir = os.path.join(wg_dir, "bundles")
    os.makedirs(bundles_dir, exist_ok=True)

    # Override the implementer bundle (used by exec_mode=full) to exclude wg tools
    bundle_path = os.path.join(bundles_dir, "implementer.toml")
    content = (
        'name = "implementer"\n'
        'description = "Full implementation agent without wg tools (Condition A baseline)."\n'
        'tools = ["bash", "read_file", "write_file", "edit_file", "glob", "grep"]\n'
        'context_scope = "clean"\n'
        'system_prompt_suffix = ""\n'
    )
    with open(bundle_path, "w") as f:
        f.write(content)


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
# WorkgraphAgent — the Harbor BaseAgent implementation
# ---------------------------------------------------------------------------

class WorkgraphAgent(BaseAgent):
    """
    Harbor agent adapter for Terminal Bench evaluation.

    Delegates execution to workgraph's native executor via `wg service start`.

    Supports six experimental conditions:
      condition="A" — bare agent (bash + file tools, no graph)
      condition="B" — agent + workgraph (full tools, journal/resume)
      condition="C" — agent + workgraph + skill injection + planning phase
      condition="D" — agent + workgraph + autopoietic verification + agency identity
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

    def _find_wg_binary(self) -> str:
        """Locate the wg binary on the host."""
        candidates = [
            os.path.expanduser("~/.cargo/bin/wg"),
            "/home/erik/workgraph/target/release/wg",
            "/home/erik/workgraph/target/debug/wg",
        ]
        for p in candidates:
            if os.path.isfile(p):
                return p
        return shutil.which("wg") or "wg"

    async def setup(self, environment: BaseEnvironment) -> None:
        """Create per-trial graph directory and configure the native executor."""
        import tempfile

        self._wg_temp_dir = tempfile.mkdtemp(prefix="tb-wg-")
        self._wg_graph_dir = os.path.join(self._wg_temp_dir, ".workgraph")
        wg_bin = self._wg_binary_host_path

        # Initialize workgraph
        await _exec_wg_cmd_host(self._wg_graph_dir, wg_bin, ["init"])
        logger.info(f"Initialized trial workgraph at {self._wg_graph_dir}")

        # Determine model
        model_raw = self.model_name or BENCHMARK_MODEL
        self._model = model_raw

        # Write wg config for the trial
        await _write_trial_wg_config(
            self._wg_temp_dir, self._wg_graph_dir,
            self.condition, self._model,
        )

        # Write custom bundle if needed (e.g. Condition A: no wg tools)
        await _write_trial_bundle(self._wg_graph_dir, self.condition)

        # Bootstrap agency for conditions D and E
        cfg = CONDITION_CONFIG[self.condition]
        if cfg["agency"]:
            role, tradeoff = cfg["agency"]
            await _exec_wg_cmd_host(self._wg_graph_dir, wg_bin, ["agency", "init"])
            agent_name = "solver" if self.condition == "D" else "orchestrator"
            await _exec_wg_cmd_host(self._wg_graph_dir, wg_bin, [
                "agent", "create", agent_name,
                "--role", role,
                "--tradeoff", tradeoff,
            ])
            self._agent_identity = {
                "name": agent_name,
                "role": role,
                "tradeoff": tradeoff,
            }
            logger.info(f"Condition {self.condition}: agency bootstrapped, {agent_name} created")

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Start the native wg service, poll for completion, and collect metrics."""
        wg_dir = self._wg_graph_dir
        wg_bin = self._wg_binary_host_path

        # Create root task
        root_task_id = f"tb-{uuid.uuid4().hex[:8]}"
        title = instruction[:100] + ("..." if len(instruction) > 100 else "")
        add_cmd = ["add", title, "--id", root_task_id, "-d", instruction]

        # Condition F: add --verify if task has verification criteria
        if self.condition == "F" and "test" in instruction.lower():
            add_cmd += ["--verify", "true"]

        await _exec_wg_cmd_host(wg_dir, wg_bin, add_cmd)

        # Assign agent identity for D/E
        cfg = CONDITION_CONFIG[self.condition]
        if cfg["agency"]:
            agent_name = "solver" if self.condition == "D" else "orchestrator"
            await _exec_wg_cmd_host(wg_dir, wg_bin, ["assign", root_task_id, agent_name])

        # Initialize trial logger
        trial_log = TrialLogger(
            logs_dir=self.logs_dir,
            condition=self.condition,
            root_task_id=root_task_id,
            model=self._model,
        )

        # Snapshot initial graph state
        snapshot = await _exec_wg_cmd_host(wg_dir, wg_bin, ["list"])
        trial_log.record_wg_snapshot("after_init", snapshot)

        # Start the native wg service
        service_cmd = [
            "service", "start",
            "--max-agents", str(cfg["max_agents"]),
            "--executor", "native",
            "--model", self._model,
            "--no-coordinator-agent",
            "--force",
        ]
        service_result = await _exec_wg_cmd_host(wg_dir, wg_bin, service_cmd)
        logger.info(f"Service started: {service_result.strip()}")

        trial_log.begin_turn(0)

        try:
            # Poll for task completion
            status, elapsed = await _poll_task_completion(
                wg_dir, wg_bin, root_task_id,
                timeout_secs=self.timeout,
                poll_interval=self.poll_interval,
            )
            logger.info(f"Root task {root_task_id} reached status: {status} in {elapsed:.1f}s")

            if status == "done":
                trial_log.termination_type = "wg_done"
            elif status == "failed":
                trial_log.termination_type = "wg_fail"
            elif status == "timeout":
                trial_log.termination_type = "timeout"
            else:
                trial_log.termination_type = status

        finally:
            # Stop the service
            stop_result = await _exec_wg_cmd_host(wg_dir, wg_bin, ["service", "stop", "--kill-agents"])
            logger.info(f"Service stopped: {stop_result.strip()}")

        trial_log.end_turn(had_tool_calls=True)

        # Snapshot final graph state
        final_snapshot = await _exec_wg_cmd_host(wg_dir, wg_bin, ["list"])
        trial_log.record_wg_snapshot("before_done", final_snapshot)

        # Collect metrics from native executor stream.jsonl files
        metrics = await _collect_agent_metrics(wg_dir)

        trial_log.total_input_tokens = metrics["total_input_tokens"]
        trial_log.total_output_tokens = metrics["total_output_tokens"]
        trial_log.total_cost = metrics["total_cost_usd"]
        trial_log.total_turns = metrics["total_turns"]

        # Build metadata
        metadata: dict[str, Any] = {
            "condition": self.condition,
            "turns": metrics["total_turns"],
            "root_task_id": root_task_id,
            "model": self._model,
            "termination_type": trial_log.termination_type,
            "elapsed_s": elapsed,
            "native_executor": True,
        }

        if cfg["agency"]:
            metadata["agent_identity"] = getattr(self, "_agent_identity", None)

        # Populate Harbor's AgentContext
        context.n_input_tokens = metrics["total_input_tokens"]
        context.n_output_tokens = metrics["total_output_tokens"]
        context.cost_usd = metrics["total_cost_usd"]
        context.metadata = metadata

        # Write trial summary
        trial_log.write_summary(extra_metadata={
            k: v for k, v in metadata.items()
            if k not in ("condition", "model", "root_task_id")
        })

        # Save workgraph state for analysis
        wg_state_dst = self.logs_dir / "workgraph_state"
        try:
            shutil.copytree(wg_dir, str(wg_state_dst))
            logger.info(f"Saved workgraph state to {wg_state_dst}")
        except Exception as e:
            logger.warning(f"Failed to save workgraph state: {e}")

        # Cleanup temp dir
        if hasattr(self, "_wg_temp_dir"):
            shutil.rmtree(self._wg_temp_dir, ignore_errors=True)


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
        kwargs["model_name"] = BENCHMARK_MODEL
        super().__init__(*args, **kwargs)


class ConditionBAgent(WorkgraphAgent):
    """Condition B (treatment): full workgraph tools + journal/resume."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-b"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "B"
        kwargs["model_name"] = BENCHMARK_MODEL
        super().__init__(*args, **kwargs)


class ConditionCAgent(WorkgraphAgent):
    """Condition C (treatment): wg tools + skill injection + planning phase."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-c"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "C"
        kwargs["model_name"] = BENCHMARK_MODEL
        super().__init__(*args, **kwargs)


class ConditionDAgent(WorkgraphAgent):
    """Condition D (treatment): wg tools + autopoietic verification + agency."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-d"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "D"
        kwargs["model_name"] = BENCHMARK_MODEL
        super().__init__(*args, **kwargs)


class ConditionEAgent(WorkgraphAgent):
    """Condition E (treatment): organization generation + independent verification."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-e"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "E"
        kwargs["model_name"] = BENCHMARK_MODEL
        super().__init__(*args, **kwargs)


class ConditionFAgent(WorkgraphAgent):
    """Condition F (treatment): wg-native agent with full context parity."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-f"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "F"
        kwargs["model_name"] = BENCHMARK_MODEL
        super().__init__(*args, **kwargs)
