# Workgraph Agent Guide

How to operate AI agents with workgraph: the service daemon, spawning, identity injection, evaluation, and manual operation.

## Table of Contents

- [Overview](#overview)
- [Service Mode (recommended)](#service-mode-recommended)
- [Agent Identity](#agent-identity)
- [Manual Agent Operation](#manual-agent-operation)
- [The Autonomous Agent Loop](#the-autonomous-agent-loop)
- [Task Selection](#task-selection)
- [Multi-Agent Coordination](#multi-agent-coordination)
- [Context and Trajectories](#context-and-trajectories)
- [Model Selection](#model-selection)
- [Monitoring](#monitoring)
- [Best Practices](#best-practices)

---

## Overview

Workgraph supports three ways to run agents:

1. **Service mode** (recommended): `wg service start` runs a daemon that automatically spawns agents on ready tasks, monitors them, and cleans up dead ones. This is the primary workflow.
2. **Autonomous loop**: `wg agent run` runs a continuous wake/check/work/sleep cycle for a single agent.
3. **Manual mode**: Agents use `wg ready`, `wg claim`, `wg done` for step-by-step control.

In all modes, the agency system can inject an identity (role + motivation) into the agent's prompt when it starts working on a task.

---

## Service Mode (recommended)

The service daemon handles everything: finding ready tasks, spawning agents, reaping dead ones, and picking up newly unblocked work.

### Starting the service

```bash
wg service start
```

That's it. The daemon auto-spawns agents on ready tasks (up to `max_agents` in parallel). When a task completes and unblocks new ones, the daemon picks those up on the next tick.

### How the coordinator tick works

Each tick, the coordinator:

1. **Reaps zombie child processes** — agents that exited
2. **Cleans up dead agents** — process exited or heartbeat stale
3. **Counts alive agents** — if `>= max_agents`, skips spawning
4. **Gets ready tasks** — open tasks with all dependencies done
5. **[If auto_assign enabled]** Creates `assign-{task}` blocker tasks for unassigned ready tasks, dispatched with the assigner model/agent
6. **[If auto_evaluate enabled]** Creates `evaluate-{task}` tasks blocked by completed tasks, dispatched with the evaluator model/agent
7. **Spawns agents** on remaining ready tasks, respecting per-task model overrides

The coordinator runs on two triggers:
- **IPC-driven immediate ticks**: Any graph change (done, add, edit, fail) sends a `graph_changed` notification over the Unix socket, triggering an immediate tick
- **Safety-net poll**: A background tick every `poll_interval` seconds (default: 60s) catches manual edits or missed events

### Pausing and resuming

```bash
wg service pause    # running agents continue, no new spawns
wg service resume   # resume coordinator, immediate tick
```

Pause is useful when you want running agents to finish but don't want new work dispatched (e.g., during a deploy).

### Configuration

```toml
# .workgraph/config.toml

[coordinator]
max_agents = 4         # max parallel agents
poll_interval = 60     # safety-net tick interval (seconds)
executor = "claude"    # executor for spawned agents
model = "opus"         # model override (optional)

[agent]
executor = "claude"
model = "opus"         # default model
heartbeat_timeout = 5  # minutes before agent is considered dead

[agency]
auto_evaluate = false  # auto-create evaluation tasks
auto_assign = false    # auto-create identity assignment tasks
assigner_model = "haiku"
evaluator_model = "opus"
evolver_model = "opus"
```

### What happens when a task is spawned

1. The coordinator claims the task (sets status to `in-progress`)
2. It resolves the effective model: task's `model` field > coordinator `model` > agent `model`
3. If the task has an `agent` field (identity assignment), the agent's role and motivation are loaded and injected into the prompt
4. A wrapper script is generated in `.workgraph/agents/agent-N/run.sh` that:
   - Runs the executor (claude, shell, etc.)
   - Captures output to `output.log`
   - On exit: marks the task as done, submitted (if verified), or failed
5. The process is detached with `setsid()` so it survives daemon restarts
6. The agent is registered in the agent registry

---

## Agent Identity

The agency system lets you assign composable identities to agents. When a task has an agent assignment, the spawned agent receives an identity section in its prompt covering:

- **Role**: skills, desired outcome, description
- **Motivation**: acceptable trade-offs, hard constraints, description

### Manual assignment

```bash
# Create roles and motivations
wg role add "Programmer" --outcome "Working, tested code" --skill rust --skill testing
wg motivation add "Careful" --accept "Slow" --reject "Untested"

# Pair them into an agent
wg agent create "Careful Coder" --role <role-hash> --motivation <motivation-hash>

# Assign to a task
wg assign my-task <agent-hash>
```

### Automatic assignment

Enable auto-assign and the coordinator creates `assign-{task}` meta-tasks for unassigned ready tasks. The assigner agent picks from available agents based on the task's requirements.

```bash
wg config --auto-assign true
wg config --assigner-model haiku   # cheap model is fine for assignment
```

### Automatic evaluation

Enable auto-evaluate and the coordinator creates `evaluate-{task}` meta-tasks for completed tasks. The evaluator scores the work across four dimensions and updates performance records.

```bash
wg config --auto-evaluate true
wg config --evaluator-model opus   # strong model for quality evaluation
```

See [AGENCY.md](AGENCY.md) for the full agency system documentation.

---

## Manual Agent Operation

For AI assistants (like Claude Code) working interactively on a claimed task:

### Protocol

1. **Check for work**
   ```bash
   wg ready
   ```

2. **Select and claim a task**
   ```bash
   wg next --actor claude
   wg claim <task-id> --actor claude
   ```

3. **View task details and context**
   ```bash
   wg show <task-id>
   wg context <task-id>
   ```

4. **Do the work** (coding, documentation, etc.)

5. **Log progress**
   ```bash
   wg log <task-id> "Completed implementation" --actor claude
   ```

6. **Record artifacts**
   ```bash
   wg artifact <task-id> src/feature.rs
   ```

7. **Mark complete or failed**
   ```bash
   wg done <task-id>
   # or for verified tasks:
   wg submit <task-id>
   # or if blocked:
   wg fail <task-id> --reason "Missing dependency"
   ```

### Integrating with Claude Code

Add to `CLAUDE.md`:

```markdown
Use workgraph for task management.

At the start of each session, run `wg quickstart` to orient yourself.
Use `wg service start` to dispatch work — do not manually claim tasks.
```

For spawned agents (subagents), the service injects task context and completion instructions into the prompt automatically.

---

## The Autonomous Agent Loop

### Running the loop

```bash
# Run continuously
wg agent run --actor claude-main

# Run single iteration
wg agent run --actor claude-main --once

# Custom interval and task limit
wg agent run --actor claude-main --interval 30 --max-tasks 10
```

### The wake/check/work/sleep cycle

```
     ┌──────────────────────────────────────┐
     │                                      │
     v                                      │
   WAKE                                     │
     │  Record heartbeat                    │
     v                                      │
   CHECK                                    │
     │  Find ready tasks                    │
     │  Match to agent capabilities         │
     │  Select best task                    │
     ├──── No work? ───────────────────────>│
     v                                      │
   WORK                                     │
     │  Claim task                          │
     │  Execute (if exec command set)       │
     │  Mark done or failed                 │
     v                                      │
   SLEEP ──────────────────────────────────>┘
```

### Agent registration

Each autonomous agent session needs an agent identity:

```bash
# AI agent with role + motivation
wg agent create "Claude Coder" \
  --role <role-hash> \
  --motivation <motivation-hash> \
  --capabilities coding,documentation,testing \
  --trust-level provisional

# Or a minimal agent for simple autonomous loops
wg agent create "General Worker" \
  --role <role-hash> \
  --motivation <motivation-hash> \
  --capabilities coding,testing
```

### Tasks with exec commands

For fully automated execution, attach shell commands to tasks:

```bash
wg exec run-tests --set "cargo test"
wg exec build --set "cargo build --release"
```

The agent loop runs these automatically. Tasks without exec commands are claimed but left for external completion (e.g., by an AI coding assistant).

---

## Task Selection

### How tasks are scored

The `wg next` and agent loop commands score ready tasks by:

| Factor | Score |
|--------|-------|
| Each matched skill | +10 |
| Each missing required skill | -5 |
| All required skills matched | +20 |
| No skill requirements (general task) | +5 |
| Task has exec command | +15 |
| Verified trust level | +5 |

```bash
wg next --actor claude-main
# Next task for claude-main:
#   implement-api - Implement API endpoints
#   Skills: rust, api-design (all matched)
```

---

## Multi-Agent Coordination

### Parallel execution

Multiple agents work simultaneously on independent tasks. The service handles this automatically:

```bash
wg service start --max-agents 4
wg agents    # see who's working on what
```

### Claim atomicity

Claims are atomic — if two agents try to claim the same task, only one succeeds. The graph is protected by flock-based file locking.

### Heartbeats and dead agent detection

Agents send heartbeats while working. If an agent's process exits or its heartbeat goes stale (default: 5 minutes), the coordinator marks it dead and unclaims its task so another agent can pick it up.

```bash
wg dead-agents --check       # check without modifying
wg dead-agents --cleanup     # mark dead and unclaim tasks
```

---

## Context and Trajectories

### Context inheritance

Tasks can specify inputs and deliverables. When dependencies complete, their artifacts and deliverables become available context:

```bash
wg context implement-api
# Context for implement-api:
#   From design-api (done):
#     Artifacts: docs/api-spec.md
```

### Trajectory planning

For AI agents with limited context windows, trajectories minimize context switching:

```bash
wg trajectory implement-api --actor claude-main
# Groups related tasks that share context (files, directories)
```

---

## Model Selection

Models are selected in priority order:

1. `--model` flag on `wg spawn` (highest priority)
2. Task's `model` field (set with `wg add --model` or `wg edit --model`)
3. `coordinator.model` in config.toml
4. `agent.model` in config.toml (lowest priority)

For agency meta-tasks, separate model settings apply:
- `agency.assigner_model` for assignment tasks
- `agency.evaluator_model` for evaluation tasks
- `agency.evolver_model` for evolution

```bash
# Per-task model at creation
wg add "Simple fix" --model haiku

# Change model on existing task
wg edit my-task --model sonnet

# Set coordinator default
wg config --model sonnet
wg service reload
```

---

## Monitoring

### Check service status

```bash
wg service status    # daemon info, coordinator state
wg agents            # all agents with status
wg agents --alive    # running only
wg agents --dead     # dead only
```

### View agent work

```bash
wg list --status in-progress   # tasks being worked on
wg show <task-id>              # full task details
wg log <task-id> --list        # progress log
```

### Interactive dashboard

```bash
wg tui    # split-pane view of tasks, agents, and logs
```

### Quick project overview

```bash
wg status     # one-screen summary
wg analyze    # comprehensive health report
```

---

## Best Practices

### Task design

- **Use `--model` for cost control**: haiku for simple tasks, opus for complex ones
- **Use `--verify` for critical tasks**: require human approval before completion
- **Specify skills**: helps task selection match agents to appropriate work
- **Specify inputs and deliverables**: enables context inheritance

### Service operation

- **Start with a low `max_agents`**: 2-4 is usually enough. More agents means more concurrent API costs.
- **Use `wg service pause`**: when deploying or making manual changes
- **Monitor with `wg tui`**: see what's happening in real time
- **Check `wg service status`**: after any issues to see coordinator state

### Agency

- **Start without auto-assign/auto-evaluate**: manually assign and evaluate first to understand the system
- **Use cheap models for assignment**: haiku is fine for picking which agent works on what
- **Use strong models for evaluation**: opus gives more accurate quality scores
- **Run `wg evolve --dry-run` first**: preview evolution proposals before applying them
