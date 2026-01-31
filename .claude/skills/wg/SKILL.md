---
name: wg
description: Use this skill for task coordination with workgraph (wg). Triggers include "workgraph", "wg", task graphs, multi-step projects, tracking dependencies, coordinating agents, or when you see a .workgraph directory.
---

# workgraph

Structured task coordination for complex work. Use this when you need to break down, track, and execute multi-step projects.

## When to use workgraph

- Projects with multiple dependent tasks
- Work that spans multiple sessions
- Coordinating between humans and AI agents
- Anything where you need to track "what's done, what's next, what's blocked"

## Quick check

```bash
wg ready    # what can I work on?
wg list     # all tasks
wg analyze  # project health
```

## The protocol

### Starting work

1. Check what's available:
   ```bash
   wg ready
   ```

2. Claim a task:
   ```bash
   wg claim <task-id> --actor claude
   ```

3. Understand the task:
   ```bash
   wg show <task-id>      # full details
   wg context <task-id>   # inputs from dependencies
   ```

### While working

Log progress (helps with context recovery if interrupted):
```bash
wg log <task-id> "Completed X, now working on Y"
```

If you produce output files:
```bash
wg artifact <task-id> path/to/output
```

### Finishing

Success:
```bash
wg done <task-id>
```

Failed (can retry later):
```bash
wg fail <task-id> --reason "why it failed"
```

Need to stop mid-task:
```bash
wg unclaim <task-id>
```

### Discovering new work

Add tasks as you discover them:
```bash
wg add "New task title" --blocked-by current-task
```

Check impact:
```bash
wg impact <task-id>  # what depends on this?
```

## Planning work

Break down a goal:
```bash
wg add "Goal: Ship the feature"
wg add "Design the API"
wg add "Implement backend" --blocked-by design-the-api
wg add "Write tests" --blocked-by implement-backend
wg add "Update docs" --blocked-by implement-backend
```

Add metadata:
```bash
wg add "Complex task" \
  --hours 4 \
  --skill rust \
  --deliverable src/feature.rs \
  --blocked-by prerequisite-task \
  --model sonnet
```

Check the plan:
```bash
wg critical-path  # longest chain
wg bottlenecks    # what to prioritize
wg forecast       # when will it be done?
```

## Analysis commands

| Command | What it tells you |
|---------|-------------------|
| `wg ready` | Tasks you can work on now |
| `wg list` | All tasks with status |
| `wg show <id>` | Full task details |
| `wg why-blocked <id>` | Why can't this start? |
| `wg impact <id>` | What depends on this? |
| `wg bottlenecks` | Highest-impact tasks |
| `wg critical-path` | Longest dependency chain |
| `wg forecast` | Completion estimate |
| `wg analyze` | Full health report |
| `wg context <id>` | Available inputs |
| `wg trajectory <id>` | Optimal claim order |

## Key behaviors

1. **Always claim before working** - prevents conflicts with other agents
2. **Log as you go** - helps recovery if interrupted
3. **Mark done immediately** - unblocks dependent tasks
4. **Add tasks as you discover them** - keep the graph current
5. **Check `wg ready` after completing** - see what's unblocked

## Multi-agent coordination

If multiple agents are working:
- Claims are atomic (no double-work)
- Use `wg coordinate` to see parallel opportunities
- Each agent should have a unique actor ID

## Agent Service

Automated agent spawning and management for parallel task execution.

### When to use spawn vs manual claim

Use `wg spawn` when:
- Running multiple agents in parallel
- Need automatic output capture
- Want heartbeat monitoring and dead agent detection
- Automating a coordinator pattern

Use manual `wg claim` when:
- Working interactively on a single task
- Don't need process management
- Testing or debugging

### Spawning agents

```bash
wg spawn <task-id> --executor <name> [--timeout <duration>] [--model <model>]
```

Spawns an agent process to work on a task:
1. Claims the task (fails if already claimed)
2. Loads executor config from `.workgraph/executors/<name>.toml`
3. Starts the executor process with task context
4. Registers agent in the registry
5. Returns immediately (doesn't wait for completion)

```bash
wg spawn implement-feature --executor claude
# Spawned agent-7 for task 'implement-feature'
#   Executor: claude
#   PID: 54321
#   Output: .workgraph/agents/agent-7/output.log

# With model override:
wg spawn simple-task --executor claude --model haiku
```

### Model selection for cost optimization

Use `--model` to select haiku (fast/cheap), sonnet (balanced), or opus (most capable):

```bash
# Set model when creating task (becomes default for that task)
wg add "Simple formatting fix" --model haiku
wg add "Complex architecture design" --model opus

# Override at spawn time
wg spawn my-task --executor claude --model haiku
```

Model priority: `--model` flag > task's model > executor default

**Cost optimization tips:**
- Use **haiku** for simple tasks: formatting, linting, file updates
- Use **sonnet** for typical coding tasks
- Use **opus** for complex reasoning, proofs, architecture decisions

### Monitoring agents

```bash
wg agents                # List all agents
wg agents --alive        # Only running agents
wg agents --dead         # Only dead agents
wg agents --working      # Actively working
wg agents --idle         # Idle agents
wg agents --json         # JSON for scripting
```

Output shows agent ID, task, executor, PID, uptime, and status.

### Terminating agents

```bash
wg kill <agent-id>       # Graceful (SIGTERM, wait, SIGKILL)
wg kill <agent-id> --force  # Immediate SIGKILL
wg kill --all            # Kill all running agents
```

Killing an agent automatically unclaims its task.

### Dead agent detection

Agents send heartbeats. When heartbeats stop, agents are marked dead.

```bash
wg dead-agents --check              # Check for dead agents (read-only)
wg dead-agents --cleanup            # Mark dead and unclaim their tasks
wg dead-agents --remove             # Remove dead agents from registry
wg dead-agents --threshold 10       # Custom timeout in minutes
```

### Service daemon

Optional background daemon for centralized agent management:

```bash
wg service start         # Start daemon
wg service stop          # Stop daemon
wg service stop --force  # Stop and kill all agents
wg service status        # Show daemon status and agent summary
```

The daemon:
- Accepts spawn requests via Unix socket
- Monitors agent health periodically
- Automatically detects dead agents

### Executor configuration

Executors define how agents run. Place configs in `.workgraph/executors/`:

**Claude executor** (`claude.toml`):
```toml
[executor]
type = "claude"
command = "claude"
args = ["--print", "--dangerously-skip-permissions"]

[executor.prompt_template]
template = """
Working on: {{task_id}} - {{task_title}}
{{task_description}}

Context: {{task_context}}

When done: wg done {{task_id}}
"""
```

**Shell executor** (`shell.toml`) - uses task's `exec` field:
```toml
[executor]
type = "shell"
command = "bash"
```

### Coordinator

Automate parallel task execution with `wg coordinator`:

```bash
wg coordinator                    # Run continuous loop
wg coordinator --once             # Spawn once and exit
wg coordinator --max-agents 3     # Limit parallel agents
wg coordinator --interval 10      # Check every 10 seconds
wg coordinator --install-service  # Generate systemd service
```

The coordinator:
1. Detects finished agents (checks if process exited)
2. Cleans up completed agents from registry
3. Spawns new agents for ready tasks (up to max)
4. Repeats on interval until all tasks done

**Example session:**
```bash
# Add some tasks
wg add "Task A" --model haiku
wg add "Task B" --model haiku
wg add "Task C" --blocked-by task-a --model haiku

# Run coordinator (spawns A and B in parallel, then C when A done)
wg coordinator --max-agents 2
```

**Configure defaults** in `.workgraph/config.toml`:
```toml
[coordinator]
interval = 30        # seconds between checks
max_agents = 4       # max parallel agents
executor = "claude"  # default executor
```

Or set via CLI: `wg config --max-agents 3 --interval 15`

## All commands

```
wg init              # start a workgraph
wg add <title>       # create task
wg done <id>         # complete task
wg fail <id>         # mark failed
wg abandon <id>      # give up on task
wg retry <id>        # retry failed task
wg claim <id>        # take a task
wg unclaim <id>      # release a task
wg reclaim <id>      # take from dead agent
wg log <id> <msg>    # add progress note
wg show <id>         # task details
wg list              # all tasks
wg ready             # available tasks
wg blocked <id>      # direct blockers
wg why-blocked <id>  # full blocker chain
wg impact <id>       # dependents
wg context <id>      # available inputs
wg trajectory <id>   # optimal claim order
wg bottlenecks       # high-impact tasks
wg critical-path     # longest chain
wg forecast          # completion estimate
wg velocity          # completion rate
wg aging             # task age distribution
wg workload          # actor assignments
wg analyze           # health report
wg actor add <id>    # register actor
wg actor list        # list actors
wg artifact <id> <p> # record output
wg exec <id>         # run task command
wg agent --actor <x> # autonomous loop
wg config            # view/set config
wg spawn <id>        # spawn agent for task
wg agents            # list running agents
wg kill <agent-id>   # terminate agent
wg coordinator       # auto-spawn agents on ready tasks
wg service start     # start service daemon
wg service stop      # stop service daemon
wg service status    # daemon status
wg dead-agents       # dead agent detection
```

All commands support `--json` for structured output.
