"""
Terminal Bench Agent Adapter for Harbor Framework.

Bridges Harbor's agent protocol to the workgraph native executor concept.
Supports two conditions:
  Condition A (control): bash + file tools only, no graph, no resume
  Condition B (treatment): full wg tool access, graph awareness, journal/resume
"""

import asyncio
import json
import logging
import os
import shlex
import shutil
import uuid
from pathlib import Path
from typing import Any

import litellm
from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Tool definitions (OpenAI function-calling schema)
# ---------------------------------------------------------------------------

BASH_TOOL = {
    "type": "function",
    "function": {
        "name": "bash",
        "description": (
            "Execute a shell command and return stdout + stderr. "
            "Use this for running programs, installing packages, and system operations."
        ),
        "parameters": {
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute.",
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 120).",
                },
            },
        },
    },
}

READ_FILE_TOOL = {
    "type": "function",
    "function": {
        "name": "read_file",
        "description": "Read the contents of a file.",
        "parameters": {
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "description": "Path to the file."},
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (0-indexed).",
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read.",
                },
            },
        },
    },
}

WRITE_FILE_TOOL = {
    "type": "function",
    "function": {
        "name": "write_file",
        "description": "Write content to a file (creates or overwrites).",
        "parameters": {
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {"type": "string", "description": "Path to the file."},
                "content": {
                    "type": "string",
                    "description": "Content to write.",
                },
            },
        },
    },
}

EDIT_FILE_TOOL = {
    "type": "function",
    "function": {
        "name": "edit_file",
        "description": (
            "Make a targeted edit to an existing file by replacing an exact string match."
        ),
        "parameters": {
            "type": "object",
            "required": ["path", "old_string", "new_string"],
            "properties": {
                "path": {"type": "string", "description": "Path to the file."},
                "old_string": {
                    "type": "string",
                    "description": "Exact text to find.",
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text.",
                },
            },
        },
    },
}

GLOB_TOOL = {
    "type": "function",
    "function": {
        "name": "glob",
        "description": "Find files matching a glob pattern.",
        "parameters": {
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern (e.g., **/*.py).",
                },
                "path": {
                    "type": "string",
                    "description": "Base directory (default: working directory).",
                },
            },
        },
    },
}

GREP_TOOL = {
    "type": "function",
    "function": {
        "name": "grep",
        "description": "Search file contents using regex.",
        "parameters": {
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for.",
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in.",
                },
            },
        },
    },
}

# Condition A: bash + file tools
CONDITION_A_TOOLS = [
    BASH_TOOL,
    READ_FILE_TOOL,
    WRITE_FILE_TOOL,
    EDIT_FILE_TOOL,
    GLOB_TOOL,
    GREP_TOOL,
]

# Condition B adds workgraph tools
WG_SHOW_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_show",
        "description": "Show details of a workgraph task.",
        "parameters": {
            "type": "object",
            "required": ["task_id"],
            "properties": {
                "task_id": {"type": "string", "description": "The task ID."},
            },
        },
    },
}

WG_LIST_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_list",
        "description": "List tasks in the workgraph, optionally filtered by status.",
        "parameters": {
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Filter: open, done, failed, in-progress, blocked.",
                },
            },
        },
    },
}

WG_ADD_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_add",
        "description": "Create a new task in the workgraph.",
        "parameters": {
            "type": "object",
            "required": ["title"],
            "properties": {
                "title": {"type": "string", "description": "Task title."},
                "after": {
                    "type": "string",
                    "description": "Comma-separated dependency task IDs.",
                },
                "description": {
                    "type": "string",
                    "description": "Detailed description.",
                },
            },
        },
    },
}

WG_DONE_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_done",
        "description": "Mark a task as done.",
        "parameters": {
            "type": "object",
            "required": ["task_id"],
            "properties": {
                "task_id": {"type": "string", "description": "The task ID."},
                "converged": {
                    "type": "boolean",
                    "description": "True if cycle has converged.",
                },
            },
        },
    },
}

WG_FAIL_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_fail",
        "description": "Mark a task as failed.",
        "parameters": {
            "type": "object",
            "required": ["task_id", "reason"],
            "properties": {
                "task_id": {"type": "string", "description": "The task ID."},
                "reason": {"type": "string", "description": "Failure reason."},
            },
        },
    },
}

WG_LOG_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_log",
        "description": "Append a log entry to a task.",
        "parameters": {
            "type": "object",
            "required": ["task_id", "message"],
            "properties": {
                "task_id": {"type": "string", "description": "The task ID."},
                "message": {"type": "string", "description": "Log message."},
            },
        },
    },
}

WG_ARTIFACT_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_artifact",
        "description": "Record an artifact (file path) for a task.",
        "parameters": {
            "type": "object",
            "required": ["task_id", "path"],
            "properties": {
                "task_id": {"type": "string", "description": "The task ID."},
                "path": {"type": "string", "description": "Artifact path."},
            },
        },
    },
}

WG_MSG_SEND_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_msg_send",
        "description": "Send a message to a task's message queue.",
        "parameters": {
            "type": "object",
            "required": ["task_id", "message"],
            "properties": {
                "task_id": {"type": "string", "description": "The task ID."},
                "message": {"type": "string", "description": "Message content."},
            },
        },
    },
}

WG_MSG_READ_TOOL = {
    "type": "function",
    "function": {
        "name": "wg_msg_read",
        "description": "Read messages for a task.",
        "parameters": {
            "type": "object",
            "required": ["task_id"],
            "properties": {
                "task_id": {"type": "string", "description": "The task ID."},
            },
        },
    },
}

CONDITION_B_TOOLS = CONDITION_A_TOOLS + [
    WG_SHOW_TOOL,
    WG_LIST_TOOL,
    WG_ADD_TOOL,
    WG_DONE_TOOL,
    WG_FAIL_TOOL,
    WG_LOG_TOOL,
    WG_ARTIFACT_TOOL,
    WG_MSG_SEND_TOOL,
    WG_MSG_READ_TOOL,
]


# ---------------------------------------------------------------------------
# Tool execution helpers
# ---------------------------------------------------------------------------

async def _exec_bash(
    env: BaseEnvironment, args: dict, timeout: int = 120
) -> str:
    """Execute a bash command inside the Harbor environment."""
    command = args.get("command", "")
    timeout_sec = args.get("timeout", timeout)
    result = await env.exec(command=command, timeout_sec=timeout_sec)
    output_parts = []
    if result.stdout:
        output_parts.append(result.stdout)
    if result.stderr:
        output_parts.append(f"[stderr]\n{result.stderr}")
    if result.return_code != 0:
        output_parts.append(f"[exit code: {result.return_code}]")
    return "\n".join(output_parts) if output_parts else "(no output)"


async def _exec_read_file(env: BaseEnvironment, args: dict) -> str:
    """Read a file inside the Harbor environment."""
    path = shlex.quote(args["path"])
    offset = args.get("offset")
    limit = args.get("limit")
    if offset is not None and limit is not None:
        cmd = f"sed -n '{offset + 1},{offset + limit}p' {path}"
    elif offset is not None:
        cmd = f"tail -n +{offset + 1} {path}"
    elif limit is not None:
        cmd = f"head -n {limit} {path}"
    else:
        cmd = f"cat {path}"
    result = await env.exec(command=cmd, timeout_sec=30)
    if result.return_code != 0:
        return f"Error reading {args['path']}: {result.stderr or 'file not found'}"
    return result.stdout or "(empty file)"


async def _exec_write_file(env: BaseEnvironment, args: dict) -> str:
    """Write a file inside the Harbor environment using base64 to avoid escaping."""
    import base64

    path = args["path"]
    content = args["content"]
    b64 = base64.b64encode(content.encode()).decode()
    cmd = (
        f"mkdir -p $(dirname {shlex.quote(path)}) && "
        f"echo {shlex.quote(b64)} | base64 -d > {shlex.quote(path)}"
    )
    result = await env.exec(command=cmd, timeout_sec=30)
    if result.return_code != 0:
        return f"Error writing {path}: {result.stderr}"
    return f"Wrote {path}"


async def _exec_edit_file(env: BaseEnvironment, args: dict) -> str:
    """Edit a file inside the Harbor environment using python + base64."""
    import base64

    path = args["path"]
    b64_old = base64.b64encode(args["old_string"].encode()).decode()
    b64_new = base64.b64encode(args["new_string"].encode()).decode()
    # Use python3 inside the container with base64-encoded strings to avoid escaping
    cmd = (
        f"python3 -c \""
        f"import base64;"
        f"p={shlex.quote(path)};"
        f"old=base64.b64decode('{b64_old}').decode();"
        f"new=base64.b64decode('{b64_new}').decode();"
        f"t=open(p).read();"
        f"c=t.count(old);"
        f"exit('old_string not found') if c==0 else None;"
        f"exit(f'old_string matches {{c}} times, must be unique') if c>1 else None;"
        f"open(p,'w').write(t.replace(old,new,1));"
        f"print('Edited '+p)\""
    )
    result = await env.exec(command=cmd, timeout_sec=30)
    if result.return_code != 0:
        return f"Error editing {path}: {result.stderr or result.stdout}"
    return result.stdout or f"Edited {path}"


async def _exec_glob(env: BaseEnvironment, args: dict) -> str:
    """Find files matching a glob pattern."""
    pattern = shlex.quote(args["pattern"])
    base = shlex.quote(args.get("path", "."))
    cmd = f"find {base} -path {pattern} -type f 2>/dev/null | head -200"
    result = await env.exec(command=cmd, timeout_sec=30)
    if not result.stdout or not result.stdout.strip():
        # Try with shell glob via bash
        cmd2 = f"ls -1 {base}/{args['pattern']} 2>/dev/null | head -200"
        result = await env.exec(command=cmd2, timeout_sec=30)
    return result.stdout or "(no matches)"


async def _exec_grep(env: BaseEnvironment, args: dict) -> str:
    """Search file contents using regex."""
    pattern = shlex.quote(args["pattern"])
    path = shlex.quote(args.get("path", "."))
    cmd = f"grep -rn {pattern} {path} 2>/dev/null | head -200"
    result = await env.exec(command=cmd, timeout_sec=30)
    return result.stdout or "(no matches)"


async def _exec_wg_cmd(env: BaseEnvironment, subcmd: list[str]) -> str:
    """Execute a wg command inside the container."""
    cmd = "wg " + " ".join(shlex.quote(s) for s in subcmd)
    result = await env.exec(command=cmd, timeout_sec=60)
    output_parts = []
    if result.stdout:
        output_parts.append(result.stdout)
    if result.stderr:
        output_parts.append(result.stderr)
    if result.return_code != 0:
        output_parts.append(f"[exit code: {result.return_code}]")
    return "\n".join(output_parts) if output_parts else "(no output)"


async def execute_tool(
    env: BaseEnvironment,
    tool_name: str,
    args: dict,
) -> str:
    """Dispatch a tool call to the appropriate handler."""
    if tool_name == "bash":
        return await _exec_bash(env, args)
    elif tool_name == "read_file":
        return await _exec_read_file(env, args)
    elif tool_name == "write_file":
        return await _exec_write_file(env, args)
    elif tool_name == "edit_file":
        return await _exec_edit_file(env, args)
    elif tool_name == "glob":
        return await _exec_glob(env, args)
    elif tool_name == "grep":
        return await _exec_grep(env, args)
    elif tool_name == "wg_show":
        return await _exec_wg_cmd(env, ["show", args["task_id"]])
    elif tool_name == "wg_list":
        cmd = ["list"]
        if args.get("status"):
            cmd += ["--status", args["status"]]
        return await _exec_wg_cmd(env, cmd)
    elif tool_name == "wg_add":
        cmd = ["add", args["title"]]
        if args.get("after"):
            cmd += ["--after", args["after"]]
        if args.get("description"):
            cmd += ["-d", args["description"]]
        return await _exec_wg_cmd(env, cmd)
    elif tool_name == "wg_done":
        cmd = ["done", args["task_id"]]
        if args.get("converged"):
            cmd.append("--converged")
        return await _exec_wg_cmd(env, cmd)
    elif tool_name == "wg_fail":
        return await _exec_wg_cmd(env, ["fail", args["task_id"], "--reason", args["reason"]])
    elif tool_name == "wg_log":
        return await _exec_wg_cmd(env, ["log", args["task_id"], args["message"]])
    elif tool_name == "wg_artifact":
        return await _exec_wg_cmd(env, ["artifact", args["task_id"], args["path"]])
    elif tool_name == "wg_msg_send":
        return await _exec_wg_cmd(env, ["msg", "send", args["task_id"], args["message"]])
    elif tool_name == "wg_msg_read":
        return await _exec_wg_cmd(env, ["msg", "read", args["task_id"]])
    else:
        return f"Unknown tool: {tool_name}"


# ---------------------------------------------------------------------------
# System prompt builders
# ---------------------------------------------------------------------------

def build_condition_a_prompt(instruction: str) -> str:
    """Condition A: bare agent, minimal scaffolding."""
    return (
        "You are a coding agent completing a Terminal Bench task.\n"
        "You have access to bash and file tools.\n"
        "Focus on completing the task efficiently and correctly.\n"
        "Do not ask for clarification - proceed with your best judgment.\n"
        "\n"
        "## Guidelines\n"
        "- Use bash to run commands, install packages, compile code, etc.\n"
        "- Use read_file, write_file, edit_file for file operations.\n"
        "- Use glob and grep to explore the codebase.\n"
        "- Always prefer precise edits over full file rewrites.\n"
        "- Keep output concise.\n"
    )


def build_condition_b_prompt(instruction: str, root_task_id: str) -> str:
    """Condition B: full workgraph integration."""
    return (
        "# Task Assignment\n\n"
        "You are an AI agent working on a task in a workgraph project.\n"
        f"Your root task ID is: **{root_task_id}**\n\n"
        "## Guidelines\n"
        "- Use bash, file tools, and wg tools to complete the task.\n"
        "- Use `wg_log` to record progress (enables crash recovery).\n"
        "- Use `wg_add` to decompose complex work into subtasks.\n"
        "- Use `wg_done` when finished, `wg_fail` if blocked.\n"
        "- Always prefer precise edits over full file rewrites.\n"
        "- Keep output concise.\n\n"
        "## Workgraph Patterns\n"
        "- **Pipeline**: A -> B -> C (sequential steps)\n"
        "- **Diamond**: A -> [B,C,D] -> E (fan-out/fan-in)\n"
        "- **Loop**: A -> B -> C -> A with --max-iterations\n"
        "- **Golden rule**: same files = sequential edges (never parallelize)\n\n"
        "## Journal/Resume\n"
        "Your progress is persisted via wg_log. If the session is interrupted,\n"
        "a resumed agent will see your log entries and continue from there.\n"
        "Log frequently so progress is not lost.\n\n"
        "Begin working on the task now.\n"
    )


# ---------------------------------------------------------------------------
# WorkgraphAgent — the Harbor BaseAgent implementation
# ---------------------------------------------------------------------------

class WorkgraphAgent(BaseAgent):
    """
    Harbor agent adapter for Terminal Bench evaluation.

    Supports two experimental conditions:
      condition="A" — bare agent (bash + file tools, no graph)
      condition="B" — agent + workgraph (full tools, journal/resume)

    Usage:
        harbor run \\
            --agent-import-path wg.adapter:WorkgraphAgent \\
            -m openrouter/qwen/qwen3-32b \\
            --task-ids task-1 -k 1
    """

    @staticmethod
    def name() -> str:
        return "workgraph"

    def version(self) -> str | None:
        return "0.1.0"

    def __init__(
        self,
        logs_dir: Path | None = None,
        model_name: str | None = None,
        condition: str = "B",
        max_turns: int = 100,
        temperature: float = 0.0,
        wg_binary_host_path: str | None = None,
        *args,
        **kwargs,
    ):
        if logs_dir is None:
            logs_dir = Path("/tmp/wg-harbor-logs")
            logs_dir.mkdir(parents=True, exist_ok=True)
        super().__init__(logs_dir=logs_dir, model_name=model_name, *args, **kwargs)
        self.condition = condition.upper()
        self.max_turns = max_turns
        self.temperature = temperature
        self._wg_binary_host_path = wg_binary_host_path or self._find_wg_binary()

    def _find_wg_binary(self) -> str:
        """Locate the wg binary on the host for injection into containers."""
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
        """Inject wg binary for Condition B."""
        if self.condition == "B":
            wg_path = self._wg_binary_host_path
            if not os.path.isfile(wg_path):
                logger.warning(f"wg binary not found at {wg_path}, skipping injection")
                return
            await environment.upload_file(
                source_path=wg_path, target_path="/usr/local/bin/wg"
            )
            await environment.exec(command="chmod +x /usr/local/bin/wg")
            await environment.exec(command="wg init", cwd="/root")
            logger.info("Injected wg binary and initialized workgraph in container")

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Execute the agent loop: LLM calls + tool execution via Harbor environment."""

        # Determine tools and prompt based on condition
        root_task_id = None
        if self.condition == "B":
            tools = CONDITION_B_TOOLS
            # Create root task in container's workgraph
            root_task_id = f"tb-{uuid.uuid4().hex[:8]}"
            title = instruction[:100] + ("..." if len(instruction) > 100 else "")
            await environment.exec(
                command=f"wg add {shlex.quote(title)} --id {shlex.quote(root_task_id)}",
                cwd="/root",
            )
            system_prompt = build_condition_b_prompt(instruction, root_task_id)
        else:
            tools = CONDITION_A_TOOLS
            system_prompt = build_condition_a_prompt(instruction)

        # Build initial messages
        messages: list[dict[str, Any]] = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": f"## Task\n\n{instruction}"},
        ]

        model = self.model_name or "openrouter/qwen/qwen3-32b"
        total_input_tokens = 0
        total_output_tokens = 0
        total_cost = 0.0

        log_path = self.logs_dir / "agent_loop.ndjson"

        for turn in range(self.max_turns):
            try:
                response = await litellm.acompletion(
                    model=model,
                    messages=messages,
                    tools=tools,
                    tool_choice="auto",
                    temperature=self.temperature,
                    max_tokens=4096,
                )
            except Exception as e:
                logger.error(f"LLM call failed on turn {turn}: {e}")
                self._log_event(log_path, {
                    "type": "error",
                    "turn": turn,
                    "error": str(e),
                })
                break

            # Track token usage
            usage = response.usage
            if usage:
                total_input_tokens += getattr(usage, "prompt_tokens", 0)
                total_output_tokens += getattr(usage, "completion_tokens", 0)
                if hasattr(usage, "cost_usd"):
                    total_cost += usage.cost_usd or 0.0

            choice = response.choices[0]
            message = choice.message

            # Log the turn
            self._log_event(log_path, {
                "type": "turn",
                "turn": turn,
                "finish_reason": choice.finish_reason,
                "content": message.content,
                "tool_calls": (
                    [
                        {"name": tc.function.name, "arguments": tc.function.arguments}
                        for tc in message.tool_calls
                    ]
                    if message.tool_calls
                    else None
                ),
            })

            # Add assistant message to history
            messages.append(message.model_dump())

            # If no tool calls, the agent is done
            if not message.tool_calls:
                break

            # Execute each tool call
            for tc in message.tool_calls:
                fn_name = tc.function.name
                try:
                    fn_args = json.loads(tc.function.arguments)
                except json.JSONDecodeError:
                    fn_args = {}
                    logger.warning(
                        f"Failed to parse tool args for {fn_name}: {tc.function.arguments}"
                    )

                try:
                    result = await execute_tool(environment, fn_name, fn_args)
                except Exception as e:
                    result = f"Tool execution error: {e}"
                    logger.error(f"Tool {fn_name} failed: {e}")

                # Truncate very long outputs
                if len(result) > 50000:
                    truncated = len(result) - 50000
                    result = result[:50000] + f"\n\n[truncated {truncated} characters]"

                messages.append({
                    "role": "tool",
                    "tool_call_id": tc.id,
                    "content": result,
                })

                self._log_event(log_path, {
                    "type": "tool_result",
                    "turn": turn,
                    "tool": fn_name,
                    "result_length": len(result),
                })

        # Populate Harbor's AgentContext with results
        context.n_input_tokens = total_input_tokens
        context.n_output_tokens = total_output_tokens
        context.cost_usd = total_cost
        context.metadata = {
            "condition": self.condition,
            "turns": turn + 1 if turn < self.max_turns else self.max_turns,
            "root_task_id": root_task_id,
            "model": model,
        }

        self._log_event(log_path, {
            "type": "result",
            "total_input_tokens": total_input_tokens,
            "total_output_tokens": total_output_tokens,
            "total_cost_usd": total_cost,
            "turns": context.metadata["turns"],
            "condition": self.condition,
        })

    def _log_event(self, path: Path, event: dict) -> None:
        """Append an NDJSON event to the log file."""
        try:
            with open(path, "a") as f:
                f.write(json.dumps(event, default=str) + "\n")
        except Exception as e:
            logger.warning(f"Failed to write log event: {e}")


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
        super().__init__(*args, **kwargs)


class ConditionBAgent(WorkgraphAgent):
    """Condition B (treatment): full workgraph tools + journal/resume."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-b"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "B"
        super().__init__(*args, **kwargs)
