# Agent Service Architecture

The agent service layer enables seamless subagent dispatch - a coordinator agent can spawn, monitor, and manage worker agents without manual intervention.

## Overview

```
┌─────────────────────────────────────────────────────────────┐
│                     Coordinator Agent                        │
│  (checks wg ready, decides what to spawn, monitors progress) │
└─────────────────────────────┬───────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                      wg service                              │
│  - Agent registry (PIDs, status, current task)              │
│  - Executor plugins (claude, shell, custom)                  │
│  - Output routing (capture → task artifacts)                 │
│  - Health monitoring (heartbeats, dead agent detection)      │
└──────┬──────────────┬──────────────┬───────────────────────┘
       │              │              │
       ▼              ▼              ▼
   ┌───────┐     ┌───────┐     ┌───────┐
   │Agent 1│     │Agent 2│     │Agent 3│
   │task-a │     │task-b │     │task-c │
   └───────┘     └───────┘     └───────┘
```

## Commands

### `wg service start`

Starts the agent service daemon.

```bash
wg service start [--port 9746] [--socket /tmp/wg.sock]
```

The service:
- Listens for spawn requests
- Maintains agent registry
- Monitors agent health
- Routes output to task artifacts

### `wg service stop`

Gracefully stops the service (waits for agents to finish or kills them).

```bash
wg service stop [--force]  # --force kills running agents
```

### `wg service status`

Shows service status.

```bash
wg service status
# Service: running (pid 12345)
# Agents: 3 active, 2 idle
# Uptime: 4h 23m
```

### `wg spawn <task-id>`

Spawns an agent to work on a specific task.

```bash
wg spawn task-123 --executor claude [--model opus-4] [--timeout 30m]
wg spawn task-456 --executor shell --command "./scripts/build.sh"
wg spawn task-789 --executor custom --config executors/my-agent.toml
```

What happens:
1. Claims the task (fails if already claimed)
2. Starts executor with task context
3. Registers agent in registry
4. Returns agent ID

```bash
$ wg spawn implement-feature --executor claude
Spawned agent-7 for task 'implement-feature'
  Executor: claude (opus-4)
  PID: 54321
  Output: .workgraph/agents/agent-7/output.log
```

### `wg agents`

Lists running agents.

```bash
$ wg agents
ID       TASK                 EXECUTOR  PID    UPTIME  STATUS
agent-1  implement-feature    claude    54321  12m     working
agent-2  write-tests          claude    54322  8m      working
agent-3  update-docs          shell     54323  2m      idle

$ wg agents --json  # for scripting
```

### `wg kill <agent-id>`

Terminates an agent.

```bash
wg kill agent-1              # graceful (SIGTERM, wait, SIGKILL)
wg kill agent-1 --force      # immediate (SIGKILL)
wg kill --all                # kill all agents
```

The task is automatically unclaimed when the agent is killed.

## Agent Registry

Lives at `.workgraph/service/registry.json`:

```json
{
  "agents": {
    "agent-1": {
      "id": "agent-1",
      "pid": 54321,
      "task_id": "implement-feature",
      "executor": "claude",
      "started_at": "2026-01-27T10:00:00Z",
      "last_heartbeat": "2026-01-27T10:12:00Z",
      "status": "working",
      "output_file": ".workgraph/agents/agent-1/output.log"
    }
  },
  "next_agent_id": 8
}
```

## Executor Plugins

Executors define how to run agents. Built-in executors:

### Claude Executor

Spawns Claude Code to work on a task.

```toml
# .workgraph/executors/claude.toml
[executor]
type = "claude"
command = "claude"
args = ["--print", "--dangerously-skip-permissions"]

[executor.env]
CLAUDE_MODEL = "opus-4"

[executor.prompt_template]
# Injected as system context
template = """
You are working on task: {{task_id}}
Title: {{task_title}}
Description: {{task_description}}

Context from dependencies:
{{task_context}}

When done, run: wg done {{task_id}}
If blocked, run: wg fail {{task_id}} --reason "..."
"""
```

### Shell Executor

Runs a shell command.

```toml
# .workgraph/executors/shell.toml
[executor]
type = "shell"
command = "bash"
args = ["-c", "{{task_command}}"]

[executor.env]
TASK_ID = "{{task_id}}"
```

### Custom Executor

Any command that follows the protocol:
1. Receives task info via env vars or stdin
2. Does work
3. Calls `wg done` or `wg fail` when finished

## Output Routing

Agent output is captured and linked to tasks:

```
.workgraph/
├── agents/
│   ├── agent-1/
│   │   ├── output.log      # stdout/stderr
│   │   ├── metadata.json   # timing, exit code
│   │   └── artifacts/      # files produced
│   └── agent-2/
│       └── ...
```

When an agent completes:
1. Output log is linked as task artifact
2. Any files in `artifacts/` are added to task
3. Metadata recorded (duration, exit code, etc.)

## Health Monitoring

### Heartbeats

Agents send heartbeats every 30s (configurable):

```bash
# Agent does this automatically, or manually:
wg heartbeat agent-1
```

### Dead Agent Detection

Service checks for dead agents every 60s:

```
if now - last_heartbeat > threshold:
    mark agent as dead
    unclaim task (or reclaim for retry)
    notify coordinator
```

### Coordinator Notification

When interesting events happen, the service can notify:

```toml
# .workgraph/config.toml
[service.notifications]
on_agent_done = "wg ready"           # run command
on_agent_failed = "notify-send 'Agent failed'"
on_agent_dead = "wg reclaim {{task_id}}"
```

## Coordinator Pattern

With the service layer, the coordinator pattern becomes:

```python
# Pseudocode for coordinator agent

while True:
    ready = wg_ready()
    if not ready:
        if all_done():
            break
        sleep(10)
        continue

    for task in ready[:max_parallel]:
        wg_spawn(task.id, executor="claude")

    # Service handles the rest:
    # - Monitors agents
    # - Captures output
    # - Detects failures
    # - Unclaims on death

    sleep(30)
```

Or as a simple bash script:

```bash
#!/bin/bash
# coordinator.sh - spawn agents for all ready tasks

while true; do
    READY=$(wg ready --json | jq -r '.[].id')

    if [ -z "$READY" ]; then
        OPEN=$(wg list --json | jq '[.[] | select(.status == "open")] | length')
        if [ "$OPEN" -eq 0 ]; then
            echo "All done!"
            break
        fi
        sleep 10
        continue
    fi

    for TASK in $READY; do
        RUNNING=$(wg agents --json | jq -r '.[].task_id' | grep -c "^$TASK$")
        if [ "$RUNNING" -eq 0 ]; then
            wg spawn "$TASK" --executor claude &
        fi
    done

    sleep 30
done
```

## Configuration

```toml
# .workgraph/config.toml

[service]
socket = "/tmp/wg-{{project}}.sock"
max_agents = 10
heartbeat_interval = 30
dead_threshold = 120

[service.defaults]
executor = "claude"
timeout = "1h"

[service.notifications]
on_agent_done = ""
on_agent_failed = ""
on_agent_dead = "wg reclaim {{task_id}}"
```

## IPC Protocol

Service uses Unix socket for IPC:

```
Request: {"cmd": "spawn", "task_id": "foo", "executor": "claude"}
Response: {"ok": true, "agent_id": "agent-7", "pid": 54321}

Request: {"cmd": "agents"}
Response: {"ok": true, "agents": [...]}

Request: {"cmd": "kill", "agent_id": "agent-7"}
Response: {"ok": true}
```

This allows `wg spawn` to work whether called from CLI or programmatically.

## Task Graph After Service Implementation

```
design-agent-service
├── implement-agent-registry
│   ├── implement-wg-agents
│   ├── implement-wg-kill
│   ├── add-agent-heartbeat
│   │   └── implement-dead-agent
│   └── implement-wg-service ←─┐
├── implement-executor-plugin  │
│   ├── implement-claude-executor
│   │   └── integration-coordinator-spawns ←─┐
│   ├── implement-shell-executor              │
│   └── implement-wg-service ─────────────────┘
└── implement-wg-spawn
    └── implement-output-capture
        └── link-agent-outputs

update-documentation-for (← after integration)
```

## Future Extensions

- **Web UI**: Real-time agent dashboard
- **Remote agents**: Spawn on different machines
- **Resource limits**: CPU/memory caps per agent
- **Priority queue**: High-priority tasks get agents first
- **Agent pools**: Pre-warmed agents for faster spawn
