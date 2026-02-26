# Agent Service Architecture

The agent service is a background daemon that automatically spawns agents on ready tasks, monitors their health, and manages their lifecycle. Start it once and it handles everything.

## Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    Service Daemon (wg service start)        │
│                                                             │
│  Unix socket listener  ←──── IPC: graph_changed, spawn,    │
│  Coordinator loop            kill, pause, resume, status    │
│  Agent reaper                                               │
│  Dead agent detector                                        │
└──────┬──────────────┬──────────────┬───────────────────────┘
       │              │              │
       ▼              ▼              ▼
   ┌───────┐     ┌───────┐     ┌───────┐
   │Agent 1│     │Agent 2│     │Agent 3│
   │task-a │     │task-b │     │task-c │
   │(claude)│    │(claude)│    │(shell)│
   └───────┘     └───────┘     └───────┘
   (detached via setsid — survives daemon restart)
```

## Quick Start

```bash
wg service start                  # start daemon
wg service status                 # check it's running
wg agents                         # see spawned agents
wg service stop                   # stop daemon (agents keep running)
wg service stop --kill-agents     # stop daemon and agents
```

## The Coordinator Tick

The daemon runs a coordinator tick on two triggers:

1. **IPC-driven**: Any command that modifies the graph (done, add, edit, fail, etc.) sends a `graph_changed` notification over the Unix socket, triggering an immediate tick
2. **Safety-net poll**: A background tick every `poll_interval` seconds (default: 60s) catches manual graph.jsonl edits or missed events

Each tick does:

```
1. Reap zombie child processes (waitpid for exited agents)
2. Clean up dead agents (process exited or heartbeat stale)
3. Count alive agents → if >= max_agents, stop here
4. Compute cycle analysis (Tarjan SCC) for back-edge exemption
5. Get ready tasks (open, all blockers done, not_before passed)
   - Cycle headers get back-edge exemption: predecessors within the
     same cycle that form back-edges are exempt from readiness checks
     (only when the header has a CycleConfig)

6. [IF auto_assign enabled]
   For each unassigned ready task (no agent field):
     Skip meta-tasks (tagged assignment/evaluation/evolution)
     Create assign-{task-id} blocker task
     Set assigner_model and assigner_agent on the new task
     The assigner runs: wg agent list, wg role list, then wg assign <task> <agent-hash>

7. [IF auto_evaluate enabled]
   For each completed task without an existing evaluate-{task-id}:
     Skip meta-tasks (tagged evaluation/assignment/evolution)
     Create evaluate-{task-id} blocked by the original task
     Set evaluator_model and evaluator_agent on the new task
     Unblock eval tasks whose source task is Failed (so failures get evaluated too)

8. Spawn agents on ready tasks:
     Resolve effective model: task.model > executor.model > coordinator.model
     Register agent in AgentRegistry
     Detach with setsid()

9. Evaluate cycle iteration on completed tasks:
     If all members of a cycle are Done and cycle hasn't converged
     or hit max_iterations → re-open all members for next iteration
```

## Service Commands

### `wg service start`

Start the background daemon.

```bash
wg service start [--max-agents <N>] [--executor <NAME>] [--interval <SECS>] [--model <MODEL>]
```

CLI flags override config.toml values for the daemon's lifetime. The daemon forks into the background and writes its PID to `.workgraph/service/state.json`.

### `wg service stop`

Stop the daemon.

```bash
wg service stop                   # graceful SIGTERM
wg service stop --force           # immediate SIGKILL
wg service stop --kill-agents     # stop daemon and kill all agents
```

By default, detached agents continue running after the daemon stops. Use `--kill-agents` to clean up everything.

### `wg service status`

Show daemon status, uptime, coordinator state, and agent summary.

```bash
wg service status
```

### `wg service reload`

Re-read config.toml or apply specific overrides without restarting.

```bash
wg service reload                              # re-read config.toml
wg service reload --max-agents 8 --model haiku # apply overrides
```

Sends a `reconfigure` IPC message to the running daemon.

### `wg service pause`

Pause the coordinator. Running agents continue working, but no new agents are spawned.

```bash
wg service pause
```

The paused state is persisted in `coordinator-state.json` and survives daemon restarts.

### `wg service resume`

Resume the coordinator and trigger an immediate tick.

```bash
wg service resume
```

### `wg service tick`

Run a single coordinator tick and exit. Useful for debugging.

```bash
wg service tick [--max-agents <N>] [--executor <NAME>] [--model <MODEL>]
```

### `wg service install`

Generate a systemd user service file.

```bash
wg service install
```

## Executor Types

The coordinator spawns agents via a configurable executor. Built-in executors:

| Executor | Command | Use case |
|----------|---------|----------|
| **claude** | `claude --print --model <M>` | Default — Anthropic Claude CLI agents |
| **amplifier** | `amplifier run --mode single -m <M>` | OpenRouter-backed models, supports `provider:model` syntax (e.g., `provider-openai:gpt-4o`) |
| **shell** | Custom command from task `exec` field | Non-LLM tasks, scripts, builds |

```bash
wg config --coordinator-executor claude      # default
wg config --coordinator-executor amplifier   # switch to amplifier
```

Custom executors can be defined in `.workgraph/executors/<name>.toml`.

### Environment variables injected into spawned agents

Every spawned agent receives these environment variables:

| Variable | Description |
|----------|-------------|
| `WG_TASK_ID` | The task ID being worked on |
| `WG_AGENT_ID` | The agent registry ID (e.g., `agent-7`) |
| `WG_EXECUTOR_TYPE` | The executor type (e.g., `claude`, `amplifier`) |
| `WG_MODEL` | The effective model selected for this agent (set only when a model is resolved) |

Agents can read these to adapt behavior based on their runtime context.

## Spawning

When the coordinator spawns an agent for a task:

1. **Claim**: The task is claimed (status → `in-progress`)
2. **Model resolution**: task.model > executor.model > coordinator.model/CLI --model
3. **Identity injection**: If the task has an `agent` field, the agent's role and motivation are loaded from `.workgraph/agency/` and rendered into an identity prompt section
4. **Context scope resolution**: The task's `context_scope` determines how much context is assembled into the prompt:
   - `clean` — task description only (no dependency context)
   - `task` — task description + direct predecessor artifacts/logs (default)
   - `graph` — task + transitive dependency chain
   - `full` — everything: full graph state, all logs, all artifacts
   If no scope is set on the task, the assigned role's default scope is used; otherwise `task` is the implicit default.
5. **Cycle context injection**: If the task is part of a structural cycle, the prompt includes:
   - The current `loop_iteration` (which pass this is)
   - A note about `--converged`: the agent can signal `wg done <task-id> --converged` to stop the cycle when work has stabilized
   - The `"converged"` tag is placed on the cycle header regardless of which member the agent completes
6. **Wrapper script**: A bash script is generated at `.workgraph/agents/agent-N/run.sh`:
   - Runs the executor command (e.g., `claude --model opus --print "..."`)
   - Captures stdout/stderr to `output.log`
   - Sends heartbeats periodically
   - On exit: checks task status, marks done/failed based on exit code
7. **Detach**: Process is launched with `setsid()` so it survives daemon restarts
8. **Register**: Agent is added to the registry with PID, task_id, executor, model, and start time

### Manual spawning

Outside the service, you can spawn agents directly:

```bash
wg spawn my-task --executor claude --model haiku --timeout 30m
```

## Agent Registry

Lives at `.workgraph/service/registry.json`. Protected by flock-based locking for concurrent access.

Each entry tracks:
- `id`: agent-N (incrementing counter)
- `task_id`: the task being worked on
- `executor`: claude, shell, etc.
- `pid`: OS process ID
- `status`: Starting, Working, Idle, Dead
- `started_at`: ISO 8601 timestamp
- `last_heartbeat`: ISO 8601 timestamp
- `model`: effective model used

## Agent Lifecycle

```
spawned → working → [heartbeat...] → done|failed|dead
                                        │
                                        ▼
                                  task unclaimed
                                  (available for retry)
```

### Heartbeats

Spawned agents send heartbeats via the wrapper script. Heartbeats are recorded in the agent registry for monitoring purposes.

### Dead agent detection

The coordinator detects dead agents on each tick by checking whether the agent's process is still running (via PID liveness check). Dead agents are cleaned up automatically before spawning new agents.

### Dead agent triage

When `auto_triage` is enabled, dead agents are triaged using an LLM to assess how much progress was made before the agent died. The triage produces one of three verdicts:

| Verdict | Behavior |
|---------|----------|
| `done` | Task is marked complete |
| `continue` | Task is unclaimed and reopened with a recovery context appended to the description, so the next agent can pick up where the previous one left off |
| `restart` | Task is unclaimed and reopened for a fresh attempt |

When `auto_triage` is disabled (the default), dead agents simply have their tasks unclaimed and reopened.

### Manual dead agent commands

```bash
wg dead-agents               # read-only check (default)
wg dead-agents --cleanup     # mark dead and unclaim tasks
wg dead-agents --remove      # remove dead entries from registry
wg dead-agents --processes   # check if agent PIDs are still running
wg dead-agents --purge       # purge dead/done/failed agents from registry
wg dead-agents --purge --delete-dirs   # also delete agent work directories
wg dead-agents --threshold 10         # override heartbeat timeout (minutes)
```

These commands are useful for manual intervention when the service is not running. `--purge` cleans up finished entries from the registry; combine with `--delete-dirs` to reclaim disk space by removing `.workgraph/agents/<id>/` directories.

## Configuration

View merged configuration with source annotations:

```bash
wg config --list                # show merged config (global/local/default)
wg config --global --show       # show only global config
wg config --local --show        # show only local config
```

Writes target local config by default. Use `--global` to write to `~/.workgraph/config.toml`:

```bash
wg config --global --model opus   # set default model globally
```

```toml
# .workgraph/config.toml

[coordinator]
max_agents = 4           # max parallel agents (default: 4)
interval = 30            # standalone coordinator tick interval
poll_interval = 60       # daemon safety-net poll interval (default: 60)
executor = "claude"      # executor for spawned agents
model = "opus"           # model override for all spawns (optional)

[agent]
executor = "claude"      # default executor
model = "opus"           # default model
heartbeat_timeout = 5    # minutes before stale (default: 5)

[agency]
auto_evaluate = false    # auto-create evaluation tasks
auto_assign = false      # auto-create assignment tasks
auto_triage = false      # triage dead agents with LLM before respawning
triage_model = "haiku"   # model for triage (default: haiku)
triage_timeout = 30      # seconds before triage call times out (default: 30)
triage_max_log_bytes = 50000  # max bytes of agent output to send to triage (default: 50000)
assigner_model = "haiku" # model for assigner agents (default via wg agency init)
evaluator_model = "haiku" # model for evaluator agents (default via wg agency init)
evolver_model = "opus"   # model for evolver agents
assigner_agent = ""      # content-hash of assigner agent identity
evaluator_agent = ""     # content-hash of evaluator agent identity
evolver_agent = ""       # content-hash of evolver agent identity
creator_agent = ""       # content-hash of creator agent identity
creator_model = ""       # model for creator agents
```

Set creator-agent/model via CLI:

```bash
wg config --creator-agent <content-hash>
wg config --creator-model haiku
```

### Model hierarchy

For regular tasks (resolution order, highest priority wins):
1. `task.model` — per-task override (highest)
2. Executor config model — model field in the executor's config file
3. CLI `--model` on `wg spawn` / `coordinator.model` in service mode
4. Executor default — if no model is resolved, no `--model` flag is passed and the executor uses its own default

For agency meta-tasks:
- Assignment: `agency.assigner_model` (defaults to `haiku` after `wg agency init`)
- Evaluation: `agency.evaluator_model` (defaults to `haiku` after `wg agency init`)
- Evolution: `agency.evolver_model`

## IPC Protocol

The daemon listens on a Unix socket at `/tmp/wg-{project}.sock`.

| Command | Description |
|---------|-------------|
| `graph_changed` | Trigger immediate coordinator tick |
| `spawn` | Spawn agent for a task |
| `agents` | List agents |
| `kill` | Kill an agent |
| `heartbeat` | Record agent heartbeat |
| `status` | Get daemon status |
| `shutdown` | Graceful shutdown |
| `pause` | Pause coordinator |
| `resume` | Resume coordinator |
| `reconfigure` | Update config at runtime |

Commands that modify the graph (`wg done`, `wg add`, `wg edit`, `wg fail`, etc.) automatically send `graph_changed` to trigger an immediate tick.

## State Files

```
.workgraph/service/
├── state.json              # Daemon PID, socket path, start time
├── daemon.log              # Timestamped daemon logs (10MB rotation)
├── daemon.log.1            # Rotated backup
├── coordinator-state.json  # Coordinator metrics: paused, ticks, agents_alive, etc.
└── registry.json           # Agent registry (flock-protected)

.workgraph/agents/
└── agent-N/
    ├── run.sh              # Wrapper script
    ├── output.log          # Agent stdout/stderr
    ├── prompt.txt          # Rendered prompt (claude executor)
    └── metadata.json       # Agent metadata (timing, exit code)
```

## Troubleshooting

**Daemon logs**: `.workgraph/service/daemon.log`

```bash
wg service status    # shows recent errors
```

**Common issues:**

| Problem | Fix |
|---------|-----|
| "Socket already exists" | `wg service stop` or delete stale socket |
| Agents not spawning | Check `wg service status`, verify `max_agents` not reached with `wg agents --alive`, ensure `wg ready` has tasks |
| Agent marked dead prematurely | Increase `heartbeat_timeout` in config.toml |
| Config changes not taking effect | `wg service reload` |
| Daemon won't start | Check for existing daemon with `wg service status` |
| Agents not picking up identity | Ensure task has `agent` field set via `wg assign` or auto-assign |
