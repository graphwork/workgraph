"""
Terminal Bench Agent Adapter for Harbor Framework.

Bridges Harbor's agent protocol to the workgraph native executor concept.
Supports five conditions:
  Condition A (control): bash + file tools only, no graph, no resume
  Condition B (treatment): full wg tool access, graph awareness, journal/resume
  Condition C (treatment): wg tools + skill injection + planning phase
  Condition D (treatment): wg tools + autopoietic verification + agency identity
  Condition E (treatment): wg tools + organization generation + independent verification
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

# Condition C uses the same tools as B — the variable is the prompt, not the tools
CONDITION_C_TOOLS = CONDITION_B_TOOLS

# Condition D uses the same tools as B — the variable is the prompt, setup, and tracking
CONDITION_D_TOOLS = CONDITION_B_TOOLS

# Condition E uses the same tools as B — the variable is the prompt, tracking, and org generation
CONDITION_E_TOOLS = CONDITION_B_TOOLS


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


async def _exec_wg_cmd_host(wg_dir: str, wg_bin: str, subcmd: list[str]) -> str:
    """Execute a wg command on the HOST (not in the container).

    This avoids injecting the wg binary into Docker containers, which
    can break Harbor's verifier. The workgraph state lives on the host
    in a temp directory.
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


async def execute_tool(
    env: BaseEnvironment,
    tool_name: str,
    args: dict,
    wg_dir: str | None = None,
    wg_bin: str | None = None,
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
        return await _exec_wg_cmd_host(wg_dir, wg_bin, ["show", args["task_id"]])
    elif tool_name == "wg_list":
        cmd = ["list"]
        if args.get("status"):
            cmd += ["--status", args["status"]]
        return await _exec_wg_cmd_host(wg_dir, wg_bin, cmd)
    elif tool_name == "wg_add":
        cmd = ["add", args["title"]]
        if args.get("after"):
            cmd += ["--after", args["after"]]
        if args.get("description"):
            cmd += ["-d", args["description"]]
        return await _exec_wg_cmd_host(wg_dir, wg_bin, cmd)
    elif tool_name == "wg_done":
        cmd = ["done", args["task_id"]]
        if args.get("converged"):
            cmd.append("--converged")
        return await _exec_wg_cmd_host(wg_dir, wg_bin, cmd)
    elif tool_name == "wg_fail":
        return await _exec_wg_cmd_host(wg_dir, wg_bin, ["fail", args["task_id"], "--reason", args["reason"]])
    elif tool_name == "wg_log":
        return await _exec_wg_cmd_host(wg_dir, wg_bin, ["log", args["task_id"], args["message"]])
    elif tool_name == "wg_artifact":
        return await _exec_wg_cmd_host(wg_dir, wg_bin, ["artifact", args["task_id"], args["path"]])
    elif tool_name == "wg_msg_send":
        return await _exec_wg_cmd_host(wg_dir, wg_bin, ["msg", "send", args["task_id"], args["message"]])
    elif tool_name == "wg_msg_read":
        return await _exec_wg_cmd_host(wg_dir, wg_bin, ["msg", "read", args["task_id"]])
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


def build_condition_c_prompt(instruction: str, root_task_id: str) -> str:
    """Condition C: wg tools + skill injection + planning phase.

    Same tools as B but with a skill prompt that teaches WHEN and HOW to use
    workgraph for decomposition, plus a mandatory planning phase instruction.
    """
    return (
        "# Task Assignment\n\n"
        "You are an AI agent completing a Terminal Bench task.\n"
        f"Your root task ID is: **{root_task_id}**\n\n"
        "## Workgraph: Your External Memory\n\n"
        "You have a workgraph — a persistent task graph that acts as external memory.\n"
        "It survives even if your context fills up. Use it.\n\n"
        "### Always do this\n"
        f'- `wg_log("{root_task_id}", "Starting: <plan>")` before your first action\n'
        f'- `wg_log("{root_task_id}", "Done: <result>")` after completing a step\n'
        f'- `wg_done("{root_task_id}")` when the task is complete\n'
        f'- `wg_fail("{root_task_id}", "reason")` if you cannot complete the task\n\n'
        "### Decompose when needed\n"
        "If the task has 3+ distinct phases or might exhaust your context:\n"
        '- `wg_add("Step 1: <title>")` to create subtasks\n'
        "- Solve each subtask, then `wg_done` each one\n"
        f'- Finally `wg_done("{root_task_id}")`\n\n'
        "If the task is simple (< 10 steps), skip decomposition and solve directly.\n\n"
        "### Record outputs\n"
        f'- `wg_artifact("{root_task_id}", "/path/to/file")` for files you create\n\n'
        "## Planning Phase\n\n"
        "Before writing code or running commands, analyze the task in ONE response:\n"
        "1. What does the task require?\n"
        "2. How many steps? Simple (< 10) or complex (10+)?\n"
        "3. Plan: decompose or solve directly?\n"
        "4. First action?\n\n"
        "Then execute your plan.\n\n"
        "## Tools\n"
        "- bash, read_file, write_file, edit_file, glob, grep — for working in the environment\n"
        "- wg_log, wg_add, wg_done, wg_fail, wg_show, wg_list, wg_artifact, "
        "wg_msg_send, wg_msg_read — for task coordination\n\n"
        "Begin by analyzing the task below, then execute.\n"
    )


def build_condition_d_prompt(instruction: str, root_task_id: str, agent_identity: dict) -> str:
    """Condition D: autopoietic verification loop + agency identity + wg tools."""
    return (
        "# Task Assignment\n\n"
        "You are an AI agent completing a Terminal Bench task.\n"
        f"Your root task ID is: **{root_task_id}**\n\n"
        "## Your Identity\n\n"
        f"You are **{agent_identity['name']}** (role: {agent_identity['role']}, "
        f"approach: {agent_identity['tradeoff']}). "
        "This means you prioritize correctness over speed. "
        "Verify your work before declaring it done.\n\n"
        "## Core Loop: Attempt → Verify → Iterate → Declare\n\n"
        "You MUST follow this loop for every task:\n\n"
        "1. **Understand**: Read the task. Identify what success looks like. "
        "Find any existing tests or verification criteria.\n"
        "2. **Attempt**: Implement your solution.\n"
        "3. **Verify**: Run the task's tests, check command, or verify output independently. "
        "Do NOT rely on your own reading of the code — execute something that proves correctness.\n"
        "4. **Iterate**: If verification fails, diagnose the failure, fix it, and go back to step 3. "
        "You may iterate as many times as needed.\n"
        "5. **Declare**:\n"
        f'   - If verification passes: `wg_done("{root_task_id}")`\n'
        f'   - If you are stuck after 3+ failed iterations and cannot make progress: '
        f'`wg_fail("{root_task_id}", "reason: what failed and what you tried")`\n\n'
        "**CRITICAL**: Never call `wg_done` without first running a verification step "
        "that succeeded. Never spin indefinitely — if 3 consecutive fix attempts fail "
        "on the same issue, call `wg_fail` with diagnostics.\n\n"
        "## Workgraph Tools\n\n"
        f'- `wg_log("{root_task_id}", "message")` — Record progress (do this at each step)\n'
        f'- `wg_done("{root_task_id}")` — Task complete (ONLY after verification passes)\n'
        f'- `wg_fail("{root_task_id}", "reason")` — Cannot complete (with diagnostics)\n'
        f'- `wg_add("title")` — Decompose into subtasks if needed\n'
        f'- `wg_artifact("{root_task_id}", "/path")` — Record output files\n\n'
        "Use `wg_log` at every major step. This is your external memory — "
        "if your context fills up, a resumed agent can read your log.\n\n"
        "## When to Decompose\n\n"
        "If the task has 3+ independent phases that could fail independently, "
        "decompose with `wg_add`. Otherwise, solve directly. "
        "Most tasks are single-phase — just use the core loop.\n\n"
        "## Tools Available\n"
        "- `bash` — Run commands (compile, test, install packages)\n"
        "- `read_file`, `write_file`, `edit_file` — File operations\n"
        "- `glob`, `grep` — Search the codebase\n"
        "- `wg_*` tools — Task coordination (see above)\n\n"
        "Begin by reading the task, identifying verification criteria, then implementing.\n"
    )


def build_condition_e_prompt(instruction: str, root_task_id: str, agent_identity: dict) -> str:
    """Condition E: autopoietic organization generation."""
    return (
        "# Task Assignment: Organization Generation Mode\n\n"
        "You are an AI agent completing a Terminal Bench task.\n"
        f"Your root task ID is: **{root_task_id}**\n\n"
        "## Your Identity\n\n"
        f"You are **{agent_identity['name']}** (role: {agent_identity['role']}, "
        f"approach: {agent_identity['tradeoff']}). "
        "You are an ORCHESTRATOR, not a direct implementer. "
        "Your job is to create and manage an organization of tasks "
        "that solves the problem.\n\n"
        "## Core Protocol: Organize → Implement → Verify → Triage\n\n"
        "You MUST follow this protocol:\n\n"
        "### Phase 1: Analyze & Decompose\n"
        "1. Read the task instruction carefully.\n"
        "2. Identify what success looks like (test criteria, expected outputs).\n"
        "3. Break the task into implementation steps.\n"
        "4. Create tasks for each step using `wg_add`.\n\n"
        "### Phase 2: Implement\n"
        "For each implementation task you created:\n"
        "1. Log that you're starting: "
        f'`wg_log("{root_task_id}", "Implementing: <task-name>")`\n'
        "2. Do the implementation work (write code, run commands, etc.)\n"
        "3. Log the result: "
        f'`wg_log("{root_task_id}", "Completed: <task-name>")`\n'
        "4. Mark the subtask done: `wg_done(\"<subtask-id>\")`\n\n"
        "### Phase 3: Independent Verification\n"
        "After ALL implementation tasks are done:\n"
        "1. **STOP and shift perspective.** You are now a REVIEWER, not the implementer.\n"
        "2. **Do NOT rely on your memory of writing the code.** "
        "Instead, read the files fresh as if seeing them for the first time.\n"
        "3. Run the task's test suite or verification command.\n"
        "4. Independently check that outputs match the task specification.\n"
        "5. Record a structured verdict:\n"
        f'   - PASS: `wg_log("{root_task_id}", "VERIFY: PASS — <evidence>")`\n'
        f'   - FAIL: `wg_log("{root_task_id}", "VERIFY: FAIL — <specific issue>")`\n\n'
        "### Phase 4: Triage (on FAIL only)\n"
        "If verification fails:\n"
        "1. Diagnose the root cause from the verification evidence.\n"
        "2. Create a new fix task: "
        '`wg_add("Fix: <diagnosis>", description="Previous attempt failed because: '
        '<reason>. Fix: <specific fix>")`\n'
        "3. Implement the fix (Phase 2 again).\n"
        "4. Re-verify (Phase 3 again).\n"
        "5. Repeat until verification passes OR you've done "
        "6 iterations without progress.\n\n"
        "### Phase 5: Declare\n"
        f'- Verification passed: `wg_done("{root_task_id}")`\n'
        "- Stuck after 6 iterations: "
        f'`wg_fail("{root_task_id}", "reason: <what failed across N iterations>")`\n\n'
        "## CRITICAL Rules\n\n"
        f"1. **NEVER call `wg_done(\"{root_task_id}\")` without a PASS verdict.** "
        "The root task represents the TB benchmark task — it can only be done "
        "when verification confirms success.\n"
        "2. **Verification must be INDEPENDENT.** When verifying, read files from disk. "
        "Do not trust your memory of what you wrote. Run tests. Check outputs.\n"
        "3. **Triage creates NEW tasks.** Don't just edit the same code in place — "
        "create a `wg_add(\"Fix: ...\")` task so the fix is tracked. Then implement it.\n"
        "4. **Log everything.** Every phase transition, every verification result, "
        "every triage decision. Your log is the organization's memory.\n"
        "5. **Iterate, don't spin.** Each fix attempt must be DIFFERENT from the last. "
        "If you're trying the same thing twice, step back and reconsider.\n\n"
        "## Workgraph Tools\n\n"
        f'- `wg_log("{root_task_id}", "message")` — Record progress (every phase)\n'
        f'- `wg_done("{root_task_id}")` — Root task complete (ONLY after PASS verdict)\n'
        f'- `wg_fail("{root_task_id}", "reason")` — Cannot complete (with full diagnostics)\n'
        '- `wg_add("title", description="details")` — Create subtasks\n'
        '- `wg_done("<subtask-id>")` — Mark a subtask complete\n'
        f'- `wg_artifact("{root_task_id}", "/path")` — Record output files\n'
        '- `wg_list()` — See all tasks and their status\n'
        '- `wg_show("<task-id>")` — Inspect a task\'s details\n\n'
        "## File Tools\n"
        "- `bash` — Run commands (compile, test, install packages)\n"
        "- `read_file`, `write_file`, `edit_file` — File operations\n"
        "- `glob`, `grep` — Search the codebase\n\n"
        "Begin by reading the task, analyzing what needs to be done, "
        "then creating your implementation plan as wg tasks.\n"
    )


# ---------------------------------------------------------------------------
# WorkgraphAgent — the Harbor BaseAgent implementation
# ---------------------------------------------------------------------------

class WorkgraphAgent(BaseAgent):
    """
    Harbor agent adapter for Terminal Bench evaluation.

    Supports five experimental conditions:
      condition="A" — bare agent (bash + file tools, no graph)
      condition="B" — agent + workgraph (full tools, journal/resume)
      condition="C" — agent + workgraph + skill injection + planning phase
      condition="D" — agent + workgraph + autopoietic verification + agency identity
      condition="E" — agent + workgraph + organization generation + independent verification

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
            # Prefer statically-linked binary for Docker container compatibility
            "/home/erik/workgraph/target/x86_64-unknown-linux-gnu/release/wg",
            os.path.expanduser("~/.cargo/bin/wg"),
            "/home/erik/workgraph/target/release/wg",
            "/home/erik/workgraph/target/debug/wg",
        ]
        for p in candidates:
            if os.path.isfile(p):
                return p
        return shutil.which("wg") or "wg"

    async def setup(self, environment: BaseEnvironment) -> None:
        """Set up host-side workgraph for Condition B/C/D/E (no container injection)."""
        if self.condition in ("B", "C", "D", "E"):
            import tempfile
            self._wg_temp_dir = tempfile.mkdtemp(prefix="tb-wg-")
            self._wg_graph_dir = os.path.join(self._wg_temp_dir, ".workgraph")
            wg_bin = self._wg_binary_host_path
            # Initialize workgraph on host
            proc = await asyncio.create_subprocess_exec(
                wg_bin, "--dir", self._wg_graph_dir, "init",
                env={"HOME": self._wg_temp_dir, **os.environ},
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            logger.info(f"Initialized host-side workgraph at {self._wg_graph_dir}")

            if self.condition == "D":
                wg_dir = self._wg_graph_dir
                # Bootstrap agency (seed starter roles/tradeoffs)
                await _exec_wg_cmd_host(wg_dir, wg_bin, ["agency", "init"])
                # Create agent identity
                await _exec_wg_cmd_host(wg_dir, wg_bin, [
                    "agent", "create", "solver",
                    "--role", "programmer",
                    "--tradeoff", "careful",
                ])
                self._agent_identity = {
                    "name": "solver",
                    "role": "programmer",
                    "tradeoff": "careful",
                }
                logger.info("Condition D: agency bootstrapped, solver agent created")

            elif self.condition == "E":
                wg_dir = self._wg_graph_dir
                # Bootstrap agency (seed starter roles/tradeoffs)
                await _exec_wg_cmd_host(wg_dir, wg_bin, ["agency", "init"])
                # Create orchestrator agent (architect role, thorough tradeoff)
                await _exec_wg_cmd_host(wg_dir, wg_bin, [
                    "agent", "create", "orchestrator",
                    "--role", "architect",
                    "--tradeoff", "thorough",
                ])
                self._agent_identity = {
                    "name": "orchestrator",
                    "role": "architect",
                    "tradeoff": "thorough",
                }
                logger.info("Condition E: agency bootstrapped, orchestrator agent created")

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Execute the agent loop: LLM calls + tool execution via Harbor environment."""

        # Determine tools and prompt based on condition
        root_task_id = None
        wg_dir = getattr(self, "_wg_graph_dir", None)
        wg_bin = self._wg_binary_host_path if self.condition in ("B", "C", "D", "E") else None
        if self.condition == "E":
            tools = CONDITION_E_TOOLS
            # Create root task in host-side workgraph
            root_task_id = f"tb-{uuid.uuid4().hex[:8]}"
            title = instruction[:100] + ("..." if len(instruction) > 100 else "")
            await _exec_wg_cmd_host(
                wg_dir, wg_bin,
                ["add", title, "--id", root_task_id],
            )
            # Assign orchestrator agent to root task
            await _exec_wg_cmd_host(wg_dir, wg_bin, ["assign", root_task_id, "orchestrator"])
            agent_identity = getattr(self, "_agent_identity", {
                "name": "orchestrator", "role": "architect", "tradeoff": "thorough",
            })
            system_prompt = build_condition_e_prompt(instruction, root_task_id, agent_identity)
        elif self.condition == "D":
            tools = CONDITION_D_TOOLS
            # Create root task in host-side workgraph
            root_task_id = f"tb-{uuid.uuid4().hex[:8]}"
            title = instruction[:100] + ("..." if len(instruction) > 100 else "")
            await _exec_wg_cmd_host(
                wg_dir, wg_bin,
                ["add", title, "--id", root_task_id],
            )
            # Assign agent identity to root task
            await _exec_wg_cmd_host(wg_dir, wg_bin, ["assign", root_task_id, "solver"])
            agent_identity = getattr(self, "_agent_identity", {
                "name": "solver", "role": "programmer", "tradeoff": "careful",
            })
            system_prompt = build_condition_d_prompt(instruction, root_task_id, agent_identity)
        elif self.condition == "C":
            tools = CONDITION_C_TOOLS
            # Create root task in host-side workgraph
            root_task_id = f"tb-{uuid.uuid4().hex[:8]}"
            title = instruction[:100] + ("..." if len(instruction) > 100 else "")
            await _exec_wg_cmd_host(
                wg_dir, wg_bin,
                ["add", title, "--id", root_task_id],
            )
            system_prompt = build_condition_c_prompt(instruction, root_task_id)
        elif self.condition == "B":
            tools = CONDITION_B_TOOLS
            # Create root task in host-side workgraph
            root_task_id = f"tb-{uuid.uuid4().hex[:8]}"
            title = instruction[:100] + ("..." if len(instruction) > 100 else "")
            await _exec_wg_cmd_host(
                wg_dir, wg_bin,
                ["add", title, "--id", root_task_id],
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

        model = self.model_name or "minimax/minimax-m2.7"
        total_input_tokens = 0
        total_output_tokens = 0
        total_cost = 0.0

        log_path = self.logs_dir / "agent_loop.ndjson"

        # Condition D/E: verification and termination tracking
        verification_count = 0
        wg_tool_call_count = 0
        termination_type = "max_turns"
        verification_commands: list[str] = []

        # Condition E: organization-specific tracking
        decomposition_tasks: list[str] = []
        verification_verdicts: list[tuple[int, str, str]] = []
        triage_count = 0

        for turn in range(self.max_turns):
            try:
                response = await litellm.acompletion(
                    model=model,
                    messages=messages,
                    tools=tools,
                    tool_choice="auto",
                    temperature=self.temperature,
                    max_tokens=16384,
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
                if termination_type == "max_turns":
                    termination_type = "no_tool_calls"
                break

            # Execute each tool call
            done_or_failed = False
            for tc in message.tool_calls:
                fn_name = tc.function.name
                try:
                    fn_args = json.loads(tc.function.arguments)
                except json.JSONDecodeError:
                    fn_args = {}
                    logger.warning(
                        f"Failed to parse tool args for {fn_name}: {tc.function.arguments}"
                    )

                # Condition D: track wg tool calls and verification commands
                if fn_name.startswith("wg_"):
                    wg_tool_call_count += 1
                if fn_name == "bash":
                    cmd = fn_args.get("command", "")
                    if any(kw in cmd.lower() for kw in [
                        "test", "pytest", "make test", "cargo test",
                        "npm test", "check", "verify", "./verify",
                    ]):
                        verification_count += 1
                        verification_commands.append(cmd[:200])
                if fn_name == "wg_done" and fn_args.get("task_id") == root_task_id:
                    termination_type = "wg_done"
                    done_or_failed = True
                elif fn_name == "wg_fail" and fn_args.get("task_id") == root_task_id:
                    termination_type = "wg_fail"
                    done_or_failed = True

                # Condition E: track decomposition, verification verdicts, triage
                if self.condition == "E":
                    if fn_name == "wg_add":
                        task_title = fn_args.get("title", "")
                        decomposition_tasks.append(task_title)
                        if task_title.startswith("Fix:"):
                            triage_count += 1
                    if fn_name == "wg_log":
                        msg = fn_args.get("message", "")
                        if "VERIFY:" in msg:
                            if "PASS" in msg:
                                verification_verdicts.append((turn, "PASS", msg))
                            elif "FAIL" in msg:
                                verification_verdicts.append((turn, "FAIL", msg))

                try:
                    result = await execute_tool(
                        environment, fn_name, fn_args,
                        wg_dir=wg_dir, wg_bin=wg_bin,
                    )
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

            # Condition D/E: stop loop after agent declares done/failed on root task
            if self.condition in ("D", "E") and done_or_failed:
                break

        # Populate Harbor's AgentContext with results
        context.n_input_tokens = total_input_tokens
        context.n_output_tokens = total_output_tokens
        context.cost_usd = total_cost
        metadata = {
            "condition": self.condition,
            "turns": turn + 1 if turn < self.max_turns else self.max_turns,
            "root_task_id": root_task_id,
            "model": model,
        }
        if self.condition == "D":
            metadata.update({
                "agent_identity": getattr(self, "_agent_identity", None),
                "verification_iterations": verification_count,
                "self_termination_type": termination_type,
                "wg_tool_calls": wg_tool_call_count,
                "verification_commands": verification_commands,
            })
        elif self.condition == "E":
            metadata.update({
                "agent_identity": getattr(self, "_agent_identity", None),
                # D-compatible metrics
                "verification_iterations": verification_count,
                "self_termination_type": termination_type,
                "wg_tool_calls": wg_tool_call_count,
                "verification_commands": verification_commands,
                # E-specific metrics
                "decomposition_task_count": len(decomposition_tasks),
                "decomposition_tasks": decomposition_tasks[:20],
                "verification_verdicts": [
                    {"turn": v[0], "verdict": v[1], "message": v[2]}
                    for v in verification_verdicts
                ],
                "triage_count": triage_count,
                "organization_phases": {
                    "decompose": bool(decomposition_tasks),
                    "verify_independent": any(v[1] for v in verification_verdicts),
                    "triage_on_fail": triage_count > 0,
                },
            })
        context.metadata = metadata

        self._log_event(log_path, {
            "type": "result",
            "total_input_tokens": total_input_tokens,
            "total_output_tokens": total_output_tokens,
            "total_cost_usd": total_cost,
            "turns": context.metadata["turns"],
            "condition": self.condition,
        })

        # Save workgraph state for analysis (Condition B, C, D, and E)
        if self.condition in ("B", "C", "D", "E") and wg_dir:
            wg_state_dst = self.logs_dir / "workgraph_state"
            try:
                shutil.copytree(wg_dir, str(wg_state_dst))
                logger.info(f"Saved workgraph state to {wg_state_dst}")
            except Exception as e:
                logger.warning(f"Failed to save workgraph state: {e}")
            # Cleanup temp dir
            wg_temp = getattr(self, "_wg_temp_dir", None)
            if wg_temp:
                shutil.rmtree(wg_temp, ignore_errors=True)

        # Extract planning turn for Condition C analysis
        if self.condition == "C":
            self._extract_planning_turn(log_path)

    def _log_event(self, path: Path, event: dict) -> None:
        """Append an NDJSON event to the log file."""
        try:
            with open(path, "a") as f:
                f.write(json.dumps(event, default=str) + "\n")
        except Exception as e:
            logger.warning(f"Failed to write log event: {e}")

    def _extract_planning_turn(self, log_path: Path) -> None:
        """Extract the first assistant turn and save as planning_turn.json.

        This enables analysis of whether the agent planned before acting,
        correctly classified task complexity, and followed the planning phase.
        """
        planning_path = self.logs_dir / "planning_turn.json"
        try:
            with open(log_path, "r") as f:
                for line in f:
                    event = json.loads(line.strip())
                    if event.get("type") == "turn" and event.get("turn") == 0:
                        with open(planning_path, "w") as pf:
                            json.dump(event, pf, indent=2, default=str)
                        logger.info(f"Extracted planning turn to {planning_path}")
                        return
            logger.warning("No turn-0 event found in agent loop log")
        except Exception as e:
            logger.warning(f"Failed to extract planning turn: {e}")


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


class ConditionCAgent(WorkgraphAgent):
    """Condition C (treatment): wg tools + skill injection + planning phase."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-c"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "C"
        super().__init__(*args, **kwargs)


class ConditionDAgent(WorkgraphAgent):
    """Condition D (treatment): wg tools + autopoietic verification + agency + no turn cap."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-d"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "D"
        kwargs.setdefault("max_turns", 200)
        super().__init__(*args, **kwargs)


class ConditionEAgent(WorkgraphAgent):
    """Condition E (treatment): organization generation + independent verification + triage."""

    @staticmethod
    def name() -> str:
        return "workgraph-condition-e"

    def __init__(self, *args, **kwargs):
        kwargs["condition"] = "E"
        kwargs.setdefault("max_turns", 300)
        super().__init__(*args, **kwargs)
