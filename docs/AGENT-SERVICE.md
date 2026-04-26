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
wg service start --force          # kill stale daemon first, then start
wg service status                 # check it's running
wg agents                         # see spawned agents
wg agents --alive                 # only alive agents (starting, working, idle)
wg agents --working               # only working agents
wg agents --dead                  # only dead agents
wg service stop                   # stop daemon (agents keep running)
wg service stop --kill-agents     # stop daemon and agents
```

## The Coordinator Tick

The daemon runs a coordinator tick on two triggers:

1. **IPC-driven**: Any command that modifies the graph (done, add, edit, fail, etc.) sends a `graph_changed` notification over the Unix socket, triggering an immediate tick
2. **Safety-net poll**: A background tick every `poll_interval` seconds (default: 60s) catches manual graph.jsonl edits or missed events

Each tick does:

```
 0. Process chat inbox (user-facing, runs before capacity checks)

 1. Clean up dead agents (process exited) and count alive
    → If alive >= max_agents, stop here (early return)

 1.3 Zero-output agent detection
     Kill agents alive 5+ min with 0 bytes written to stream files
     Three-layer circuit breaker:
       a) Agent-level: kill the zombie agent, unclaim its task
       b) Per-task: after 2 consecutive zero-output spawns, fail the task
       c) Global API-down: if ≥50% of alive agents are zero-output,
          pause all spawning with exponential backoff (60s → 15min max)

 1.5 Auto-checkpoint alive agents
     Saves a checkpoint for agents that have exceeded the configured
     turn count or elapsed time thresholds. This preserves context
     for recovery if the agent is later killed or dies unexpectedly.

 2. Load graph

 2.5 Cycle iteration evaluation
     If all members of a cycle are Done and the cycle hasn't converged
     or hit max_iterations → re-open all members for the next iteration

 2.6 Cycle failure restart
     If a cycle member is Failed and restart_on_failure is true (default)
     → re-activate the cycle for another attempt

 2.7 Wait/resume evaluation
     Check Waiting tasks for satisfied conditions (task status, timer,
     human input, message) and transition them back to Open.
     Detect and fail circular waits.

 2.8 Message-triggered resurrection
     Scan Done tasks for unread messages from whitelisted senders
     (user, coordinator, dependent-task agents). Reopen the task so
     the next agent can address the message. Rate-limited: max 3
     resurrections per task with cooldown.

 3. [IF auto_assign enabled]
    For each unassigned ready task (no agent field):
      Skip meta-tasks (tagged assignment/evaluation/evolution)
      Create assign-{task-id} blocker task
      Set assigner_model and assigner_agent on the new task

 4. [IF auto_evaluate enabled]
    For each completed task without an existing evaluate-{task-id}:
      Skip meta-tasks (tagged evaluation/assignment/evolution)
      Create evaluate-{task-id} blocked by the original task
      Set evaluator_model and evaluator_agent on the new task

 4.5 FLIP verification (if flip_verification_threshold is set)
     For tasks with FLIP scores below threshold, create an independent
     .verify-flip-{task-id} verification task dispatched to a stronger
     model (Opus) to confirm or reject the result.

 4.6 [IF auto_evolve enabled]
     Trigger agent evolution when evaluation data warrants it.

 4.7 [IF auto_create enabled]
     Invoke the creator agent to expand the primitive store (roles,
     tradeoffs) when enough tasks have completed since the last
     invocation (threshold: auto_create_threshold, default 20).

 5. Check for ready tasks (after agency phases may have created new ones)
    → If no ready tasks, stop here (early return)
    Cycle headers get back-edge exemption: predecessors within the
    same cycle that form back-edges are exempt from readiness checks

 5.5 Check global API-down backoff
     → If zero-output backoff is active, skip spawning (early return)

 6. Spawn agents on ready tasks
    Resolve effective model: task.model > executor.model > coordinator.model
    Register agent in AgentRegistry
    Detach with setsid()
```

## Service Commands

### `wg service start`

Start the background daemon.

```bash
wg service start [--max-agents <N>] [--executor <NAME>] [--interval <SECS>] [--model <MODEL>]
```

Additional flags:

| Flag | Description |
|------|-------------|
| `--port <PORT>` | Enable HTTP API on this port (optional) |
| `--socket <SOCKET>` | Unix socket path (default: `.workgraph/service/daemon.sock`) |
| `--force` | Kill any existing daemon before starting (prevents stacked daemons) |
| `--no-coordinator-agent` | Disable the persistent coordinator agent (LLM chat session) |

CLI flags override config.toml values for the daemon's lifetime. The daemon forks into the background and writes its PID to `.workgraph/service/state.json`.

### `wg service stop`

Stop the daemon.

```bash
wg service stop                   # graceful SIGTERM
wg service stop --force           # immediate SIGKILL
wg service stop --kill-agents     # stop daemon and kill all agents
```

By default, detached agents continue running after the daemon stops. Use `--kill-agents` to clean up everything.

### `wg service restart`

Graceful stop then start. Equivalent to `wg service stop && wg service start`.

```bash
wg service restart
```

### `wg service status`

Show daemon status, uptime, coordinator state, and agent summary.

```bash
wg service status
```

### `wg service freeze`

Freeze all running agents (sends SIGSTOP) and pause the coordinator. Useful for temporarily halting all work without killing agents.

```bash
wg service freeze
```

### `wg service thaw`

Thaw all frozen agents (sends SIGCONT) and resume the coordinator.

```bash
wg service thaw
```

### `wg service reload`

Re-read config.toml or apply specific overrides without restarting.

```bash
wg service reload                              # re-read config.toml
wg service reload --max-agents 8 --model haiku # apply overrides
wg service reload --interval 120               # change poll interval
wg service reload --executor amplifier         # switch executor at runtime
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

### Multi-Coordinator Commands

The service supports multiple concurrent coordinator sessions. Manage them with:

```bash
wg service create-coordinator              # create a new coordinator session
wg service create-coordinator --name foo   # create a named coordinator session
wg service stop-coordinator <ID>           # stop a coordinator session (kill agent, reset to Open)
wg service archive-coordinator <ID>        # archive a coordinator session (mark as Done)
wg service delete-coordinator <ID>         # delete a coordinator session
wg service interrupt-coordinator <ID>      # interrupt current generation (SIGINT, preserves context)
```

`interrupt-coordinator` sends SIGINT to the coordinator's active LLM generation without killing the agent process. This is useful for redirecting a coordinator that's producing unwanted output — the agent preserves its context and can be given new instructions via `wg chat`.

Target a specific coordinator from `wg chat`:

```bash
wg chat --coordinator my-coord "Instructions for this coordinator"
```

Configure the maximum concurrent coordinators:

```bash
wg config --max-coordinators 2
```

### Coordinator Persistence

Coordinator tasks are preserved across service restarts. When the daemon stops and restarts, existing coordinator tasks (tagged `coordinator-loop`) are discovered and reused rather than creating new ones. This means:

- The TUI continues to show the same coordinator sessions after a restart
- Coordinator chat history and state are retained
- No duplicate coordinator tasks accumulate over time

Previously, service restarts would create fresh coordinator tasks each time, leaving orphaned old ones. The fix (commit `cd8b3c07`) ensures only truly legacy tasks (`.archive-*`, `.registry-refresh-*`, `.user-*`) are cleaned up on startup.

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
| `WG_USER` | The current user identity |
| `WG_ENDPOINT` / `WG_ENDPOINT_NAME` | The endpoint name (set when an endpoint is configured) |
| `WG_LLM_PROVIDER` | The LLM provider (e.g., `anthropic`, `openrouter`) |
| `WG_ENDPOINT_URL` | The endpoint URL (set when an endpoint is configured) |
| `WG_API_KEY` | The API key for the endpoint (set when an endpoint is configured) |
| `WG_WORKTREE_PATH` | Path to the agent's isolated git worktree (set when worktree isolation is active) |
| `WG_BRANCH` | The worktree branch name (set when worktree isolation is active) |
| `WG_PROJECT_ROOT` | Path to the main project root (set when worktree isolation is active) |

Agents can read these to adapt behavior based on their runtime context.

## Spawning

When the coordinator spawns an agent for a task:

1. **Claim**: The task is claimed (status → `in-progress`)
2. **Model resolution**: task.model > executor.model > coordinator.model/CLI --model
3. **Identity injection**: If the task has an `agent` field, the agent's role and tradeoff are loaded from `.workgraph/agency/` and rendered into an identity prompt section
4. **Provider resolution**: If the task has a `provider` field (set via the `provider:model` format in `--model`, e.g., `--model openai:gpt-4o`), the executor uses that provider. Supported providers: `anthropic`, `openai`, `openrouter`, `local`. The standalone `--provider` flag on `wg add`/`wg edit` still works but is deprecated.
5. **Exec-mode resolution**: The task's `exec_mode` determines the agent's toolset:
   - `full` — all tools (default)
   - `light` — read-only tools (research, review)
   - `bare` — only `wg` CLI (graph orchestration)
   - `shell` — no LLM; runs the task's `exec` command directly
6. **Context scope resolution**: The task's `context_scope` determines how much context is assembled into the prompt:
   - `clean` — core task info only (title, description, dependency context) — no workflow instructions
   - `task` — + workflow sections, tags/skills, downstream awareness (default)
   - `graph` — + project description, subgraph summary (1-hop neighborhood)
   - `full` — + system awareness preamble, full graph summary, CLAUDE.md content
   If no scope is set on the task, the assigned role's default scope is used; otherwise `task` is the implicit default.
7. **Cycle context injection**: If the task is part of a structural cycle, the prompt includes:
   - The current `loop_iteration` (which pass this is)
   - A note about `--converged`: the agent can signal `wg done <task-id> --converged` to stop the cycle when work has stabilized
   - The `"converged"` tag is placed on the cycle header regardless of which member the agent completes
8. **Wrapper script**: A bash script is generated at `.workgraph/agents/agent-N/run.sh`:
   - Runs the executor command (e.g., `claude --model opus --print "..."`)
   - Captures stdout/stderr to `output.log`
   - Sends heartbeats periodically
   - On exit: checks task status, marks done/failed based on exit code
9. **Detach**: Process is launched with `setsid()` so it survives daemon restarts
10. **Register**: Agent is added to the registry with PID, task_id, executor, model, and start time

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

### Task reclaim

Transfer a task from a dead or unresponsive agent to a new one without waiting for automatic cleanup:

```bash
wg reclaim <task-id> --from <old-agent> --to <new-agent>
```

### Waiting tasks

Agents can park a task in `Waiting` status until a condition is met. The coordinator evaluates waiting conditions each tick (step 2.7) and resumes satisfied tasks automatically.

```bash
wg wait <task-id> --until "task:dep-a=done"       # wait for another task to reach a status
wg wait <task-id> --until "timer:5m"              # wait for a duration (e.g. 5m, 2h, 30s)
wg wait <task-id> --until "message"               # wait for any message on the task
wg wait <task-id> --until "human-input"           # wait specifically for a human message
wg wait <task-id> --until "file:path/to/file"     # wait for a file to change
wg wait <task-id> --until "task:dep-a=done" --checkpoint "Progress so far"
```

The coordinator detects and fails circular waits (task A waiting on task B waiting on task A).

## Configuration

View merged configuration with source annotations:

```bash
wg config --merged              # show effective config (global + local merged)
wg config --list                # show merged config with source annotations
wg config --global --show       # show only global config
wg config --local --show        # show only local config
```

### Endpoint inheritance (opt-in)

`[[llm_endpoints.endpoints]]` entries do **not** cascade from global to local
by default. If your global config declares an endpoint (e.g. an `openrouter`
entry with `is_default = true`) and your local config has no `[llm_endpoints]`
section, **no global endpoints will be visible to the project**. The local
config defines the complete set of available endpoints.

This is opt-in inheritance: if you want the legacy "global cascades into
local" behavior, set the knob explicitly in local config:

```toml
[llm_endpoints]
inherit_global = true
# (and any local-only endpoints below; they still take precedence)
```

Use `wg config --merged` to confirm what's actually in effect — that view
prints the current `inherit_global` value and the effective endpoints list.
This is the cleanest way to debug "why is openrouter still being inherited
from global?": if `inherit_global = false (default — local endpoints fully
replace global)` is shown, no global endpoints are merged in.

If a local endpoint shares the same `name` as a global one, local always
wins (the local list fully replaces global's, regardless of `inherit_global`
when local declares its own entries — `inherit_global` only matters when
local has no endpoints of its own).

Writes target local config by default. Use `--global` to write to `~/.workgraph/config.toml`:

```bash
wg config --global --model opus   # set default model globally
```

```toml
# .workgraph/config.toml

[coordinator]
max_agents = 4           # max parallel agents (default: 4)
max_coordinators = 4     # max concurrent coordinator sessions (default: 4)
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
auto_place = false       # auto-placement analysis merged into assignment step (see AGENT-GUIDE.md §1b)
auto_create = false      # auto-invoke creator agent for primitive store expansion
auto_create_threshold = 20  # completed tasks before next creator invocation (default: 20)
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

### Eval gate configuration

Control the evaluation gate that blocks task completion pending a minimum score:

```bash
wg config --eval-gate-threshold 0.7    # tasks scoring below 0.7 are rejected
wg config --eval-gate-all true         # gate ALL tasks, not just --verify tasks
```

### FLIP verification

When `flip_verification_threshold` is set, tasks with FLIP scores below the threshold automatically get a `.verify-flip-<task-id>` verification task dispatched to a stronger model (Opus):

```toml
[agency]
flip_enabled = true
flip_verification_threshold = 0.5
flip_inference_model = "sonnet"
flip_comparison_model = "sonnet"
flip_verification_model = "opus"
```

### Retry context injection

When a task is retried, the coordinator injects context from the previous attempt into the new agent's prompt. Control how much:

```bash
wg config --retry-context-tokens 2000   # max tokens of prior-attempt context (default: 2000, 0 = disabled)
```

### Compaction cycle

The coordinator drives a `.compact-0` cycle task that periodically distills graph state into a `context.md` summary. This is the coordinator's self-introspection loop — it runs automatically as a structural cycle within the service.

```bash
wg compact              # manually trigger compaction
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

The daemon listens on a Unix socket at `.workgraph/service/daemon.sock` (overridable via `wg service start --socket <path>`).

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
| `freeze` | SIGSTOP all running agents and pause coordinator |
| `thaw` | SIGCONT all frozen agents and resume coordinator |
| `reconfigure` | Update config at runtime |
| `add_task` | Create a task (cross-repo dispatch) |
| `query_task` | Query a task's status (cross-repo query) |
| `send_message` | Send a message to a task's message queue |
| `user_chat` | Send a chat message to the coordinator agent |
| `create_coordinator` | Create a new coordinator instance |
| `delete_coordinator` | Delete a coordinator instance |
| `archive_coordinator` | Archive a coordinator (mark as Done) |
| `stop_coordinator` | Stop a coordinator (kill agent, reset to Open) |
| `interrupt_coordinator` | Interrupt a coordinator's current generation (SIGINT, does not kill) |
| `list_coordinators` | List all active coordinators |

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
└── agent-N/                # Every spawn path (full and inline) emits these 4 files:
    ├── metadata.json       # Agent metadata: agent_id, task_id, executor, model, started_at, worktree info
    ├── output.log          # Agent stdout/stderr (always present, may be empty initially)
    ├── prompt.txt          # Rendered LLM prompt (full spawns) or task description (inline spawns)
    └── run.sh              # Executable wrapper script (full spawns) or inline script (inline spawns)
```

### Agent Directory Contract

Every agent directory (`.workgraph/agents/agent-N/`) is guaranteed to contain these four files regardless of spawn path:

| File | Content | Purpose |
|------|---------|---------|
| `metadata.json` | JSON: `agent_id`, `task_id`, `executor`, `model`, `started_at`, optional `worktree_path`/`worktree_branch`, `inline` flag for inline spawns | Identification, debugging, worktree cleanup |
| `output.log` | Agent stdout/stderr stream | Live monitoring, TUI display, token usage extraction |
| `prompt.txt` | Full LLM prompt (standard spawns) or task description (inline spawns) | Replay, debugging, prompt forensics |
| `run.sh` | Executable bash script that launched the agent | Replay, debugging, understanding what ran |

**Full spawns** (via `spawn/execution.rs`): Used for regular task agents (claude, native, shell, amplifier executors). The prompt.txt contains the rendered LLM prompt and run.sh is a full wrapper with heartbeats, timeout handling, and completion logic.

**Inline spawns** (via `coordinator.rs`): Used for system tasks (evaluation, assignment, FLIP). These run simple CLI commands (`wg evaluate`, `wg assign`). The prompt.txt notes that no LLM prompt was assembled, and run.sh contains the inline script.

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

## Manual Cleanup Commands

For edge-case recovery when automatic cleanup isn't sufficient:

```bash
wg cleanup orphaned              # clean up orphaned worktrees with no agent metadata
wg cleanup recovery-branches     # clean up old recovery branches (recover/<agent>/<task>)
wg cleanup nightly               # comprehensive nightly cleanup (task hygiene + maintenance)
```

These commands complement the automatic cleanup performed by the coordinator on each tick. Use them when:
- The service was killed without graceful shutdown
- Worktrees accumulate from interrupted development sessions
- Recovery branches pile up after many agent deaths
