---
name: wg
description: Use this skill for task coordination with workgraph (wg). Triggers include "workgraph", "wg", task graphs, multi-step projects, tracking dependencies, coordinating agents, or when you see a .workgraph directory.
---

# workgraph

## First: run `wg quickstart`

Always run `wg quickstart` in your terminal at the start of a session. It prints
the current workflow cheat sheet and tells you whether the service is already running.

```bash
wg quickstart
```

## Your role as a top-level agent

You are a **coordinator**. Your job is to define work and let the service dispatch it.

### Start the service if it's not running

```bash
wg service start --max-agents 5
```

### Define tasks with dependencies

```bash
wg add "Design the API" -d "Description of what to do"
wg add "Implement backend" --blocked-by design-the-api
wg add "Write tests" --blocked-by implement-backend
```

### Monitor progress

```bash
wg list                  # All tasks with status
wg agents                # Who's working on what
wg service status        # Service health
```

### What you do NOT do as coordinator

- **Don't `wg claim`** — the service claims tasks automatically
- **Don't `wg spawn`** — the service spawns agents automatically
- **Don't work on tasks yourself** — spawned agents do the work

If `wg done` fails, the task may require verification — use `wg submit`.

## If you ARE a spawned agent working on a task

You were spawned by the service to work on a specific task. Your workflow:

```bash
wg show <task-id>        # Understand what to do
wg context <task-id>     # See inputs from dependencies
wg log <task-id> "msg"   # Log progress as you work
wg done <task-id>        # Mark complete when finished
```

If you discover new work while working:

```bash
wg add "New task" --blocked-by <current-task>
```

## Manual mode (no service running)

Only use this if you're working alone without the service:

```bash
wg ready                 # See available tasks
wg claim <task-id>       # Claim a task
wg log <task-id> "msg"   # Log progress
wg done <task-id>        # Mark complete
```

## Quick reference

| Command | Purpose |
|---------|---------|
| `wg quickstart` | Orient yourself at session start |
| `wg service start` | Start the coordinator service |
| `wg service status` | Check if service is running |
| `wg add "Title" -d "Desc"` | Create a task |
| `wg add "X" --blocked-by Y` | Create task with dependency |
| `wg list` | All tasks with status |
| `wg ready` | Tasks available to work on |
| `wg show <id>` | Full task details |
| `wg context <id>` | Inputs from dependencies |
| `wg agents` | Running agents |
| `wg done <id>` | Complete a task |
| `wg submit <id>` | Submit verified task for review |
| `wg fail <id> --reason "why"` | Mark task failed |
| `wg log <id> "msg"` | Log progress |
| `wg artifact <id> <path>` | Record output file |
| `wg impact <id>` | What depends on this? |
| `wg analyze` | Full health report |

All commands support `--json` for structured output. Run `wg --help` for the full command list.
