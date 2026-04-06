"""
Terminal Bench Agent Adapter for Harbor Framework.

Supports two execution modes:
  1. Docker-aware: LLM agent loop in Python, routes commands through
     harbor's environment.exec() into Docker containers. (Default for harbor.)
  2. Host-native: Delegates to wg service start + native-exec.
     (Only works when verification runs on the host, e.g. run_full_a_prime_vs_f.py.)

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

import yaml

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
    # Strip env vars from the parent workgraph service that would override
    # the trial's own model/executor configuration.
    clean_env = {
        k: v for k, v in os.environ.items()
        if k not in (
            "WG_MODEL", "WG_EXECUTOR_TYPE", "WG_AGENT_ID", "WG_TASK_ID",
            "WG_LLM_PROVIDER", "WG_ENDPOINT", "WG_ENDPOINT_NAME",
            "WG_ENDPOINT_URL", "WG_API_KEY",
        )
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
# Docker-aware LLM agent loop
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

        # Federation: pull primitives from hub for agency conditions
        cfg = CONDITION_CONFIG[self.condition]
        hub_has_agency = False
        if (
            self.federation_hub
            and self.condition in FEDERATION_CONDITIONS
            and self.pull_primitives
        ):
            await _ensure_hub_initialized(self.federation_hub, wg_bin)
            await _write_trial_federation_config(self._wg_graph_dir, self.federation_hub)
            pull_result = await _federation_pull(self._wg_graph_dir, wg_bin, self.federation_hub)
            logger.info(f"Federation pull from hub: {pull_result.strip()}")
            hub_has_agency = True

        # Bootstrap agency for conditions D, E, F
        if cfg["agency"]:
            if not hub_has_agency:
                # No hub available — bootstrap from starters
                await _exec_wg_cmd_host(self._wg_graph_dir, wg_bin, ["agency", "init"])
            role, tradeoff = cfg["agency"]
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
        """Run LLM agent loop with commands routed through Docker."""
        root_task_id = f"tb-{uuid.uuid4().hex[:8]}"

        # Resolve max_turns and temperature from kwargs
        max_turns = getattr(self, "_max_turns", 9999)
        temperature = getattr(self, "_temperature", 0.0)

        # Initialize trial logger
        trial_log = TrialLogger(
            logs_dir=self.logs_dir,
            condition=self.condition,
            root_task_id=root_task_id,
            model=self._model,
        )

        trial_log.begin_turn(0)

        # Run Docker-aware agent loop
        metrics = await _run_docker_agent_loop(
            instruction=instruction,
            environment=environment,
            model=self._model,
            condition=self.condition,
            max_turns=max_turns,
            timeout_secs=self.timeout,
            temperature=temperature,
        )

        trial_log.end_turn(had_tool_calls=True)

        trial_log.total_input_tokens = metrics["total_input_tokens"]
        trial_log.total_output_tokens = metrics["total_output_tokens"]
        trial_log.total_cost = metrics.get("total_cost_usd", 0.0)
        trial_log.total_turns = metrics["total_turns"]
        trial_log.termination_type = metrics["termination_type"]

        elapsed = metrics["elapsed_s"]

        # Build metadata
        metadata: dict[str, Any] = {
            "condition": self.condition,
            "turns": metrics["total_turns"],
            "root_task_id": root_task_id,
            "model": self._model,
            "termination_type": metrics["termination_type"],
            "elapsed_s": elapsed,
            "docker_agent_loop": True,
        }

        cfg = CONDITION_CONFIG[self.condition]
        if cfg["agency"]:
            metadata["agent_identity"] = getattr(self, "_agent_identity", None)

        # Populate Harbor's AgentContext
        context.n_input_tokens = metrics["total_input_tokens"]
        context.n_output_tokens = metrics["total_output_tokens"]
        context.cost_usd = metrics.get("total_cost_usd", 0.0)
        context.metadata = metadata

        # Write trial summary
        trial_log.write_summary(extra_metadata={
            k: v for k, v in metadata.items()
            if k not in ("condition", "model", "root_task_id")
        })

        # Cleanup temp dir if it exists
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

    @staticmethod
    def _build_prompt() -> str:
        """Return the assembled system prompt for condition F (for testing)."""
        parts = ["You are a skilled software engineer. Complete the task below."]
        parts.append(CONDITION_F_MEMORY)
        return "\n\n".join(parts)
