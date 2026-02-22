---
name: wg
description: Use this skill for task coordination with workgraph (wg). Triggers include "workgraph", "wg", task graphs, multi-step projects, tracking dependencies, coordinating agents, or when you see a .workgraph directory.
---

# workgraph

## First: orient and start the service

At the start of every session, run these two commands:

```bash
wg quickstart              # Orient yourself — prints cheat sheet and service status
wg service start           # Start the coordinator (no-op if already running)
```

If the service is already running, `wg service start` will tell you. Always ensure the service is up before defining work — it's what dispatches tasks to agents.

## Your role as a top-level agent

You are a **coordinator**. Your job is to define work and let the service dispatch it.

### Start the service if it's not running

```bash
wg service start --max-agents 5
```

### Define tasks with dependencies

```bash
wg add "Design the API" --description "Description of what to do"
wg add "Implement backend" --blocked-by design-the-api
wg add "Write tests" --blocked-by implement-backend
```

### Monitor progress

```bash
wg list                  # All tasks with status
wg list --status open    # Filter by status (open, in-progress, done, failed)
wg agents                # Who's working on what
wg agents --alive        # Only alive agents
wg agents --working      # Only working agents
wg service status        # Service health
wg status                # Quick one-screen overview
wg viz                   # ASCII dependency graph
wg tui                   # Interactive TUI dashboard
```

### What you do NOT do as coordinator

- **Don't `wg claim`** — the service claims tasks automatically
- **Don't `wg spawn`** — the service spawns agents automatically
- **Don't work on tasks yourself** — spawned agents do the work

Always use `wg done` to complete tasks. Do NOT use `wg submit` (deprecated).

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

Record output files so downstream tasks can find them:

```bash
wg artifact <task-id> path/to/output
```

## Manual mode (no service running)

Only use this if you're working alone without the service:

```bash
wg ready                 # See available tasks
wg claim <task-id>       # Claim a task
wg log <task-id> "msg"   # Log progress
wg done <task-id>        # Mark complete
```

## Task lifecycle

```
open → [claim] → in-progress → [done] → done
                              → [fail] → failed → [retry] → open
                              → [abandon] → abandoned
```

**Note:** `wg submit`, `wg approve`, and `wg reject` are deprecated. Always use `wg done`.

## Cycles (repeating workflows)

Some workflows repeat. Workgraph models these as **structural cycles** — `blocked_by` back-edges with a `CycleConfig` that controls iteration limits. When a cycle iteration completes, the cycle header task is reset to `open` with its `loop_iteration` incremented, and intermediate tasks are re-opened automatically.

```bash
# Create a write/review cycle, max 3 iterations
wg add "Write" --id write --blocked-by review --max-iterations 3
wg add "Review" --blocked-by write --id review

# Inspect cycles
wg cycles
```

As a spawned agent on a task inside a cycle, check `wg show <task-id>` for `loop_iteration` to know which pass you're on. Review previous logs and artifacts to build on prior work. If the work has converged and no more iterations are needed, use `wg done <task-id> --converged` to signal early termination — the cycle will not iterate again.

```bash
wg cycles                   # List detected cycles and status
wg show <task-id>           # See loop_iteration and cycle membership
```

### Pausing and resuming cycles

To temporarily stop a cycling task without losing its iteration count:

```bash
wg pause <task-id>          # Coordinator skips this task until resumed
wg resume <task-id>         # Task becomes dispatchable again
```

Paused tasks keep their status and iteration count intact. `wg show` displays "(PAUSED)" and `wg list` shows "[PAUSED]".

To pause/resume the entire coordinator (all dispatching stops, running agents continue):

```bash
wg service pause            # No new agents spawned
wg service resume           # Resume dispatching
```

## Full command reference

### Task creation & editing

| Command | Purpose |
|---------|---------|
| `wg add "Title" --description "Desc"` | Create a task (`-d` alias for `--description`) |
| `wg add "X" --blocked-by Y` | Create task with dependency |
| `wg add "X" --blocked-by a,b,c` | Multiple dependencies (comma-separated) |
| `wg add "X" --skill rust --input src/foo.rs --deliverable docs/out.md` | Task with skills, inputs, deliverables |
| `wg add "X" --model haiku` | Task with preferred model |
| `wg add "X" --verify "Tests pass"` | Task requiring review before completion |
| `wg add "X" --tag important --hours 2` | Tags and estimates |
| `wg add "X" --blocked-by Y --max-iterations 3` | Create cycle header with max 3 iterations |
| `wg edit <id> --title "New" --description "New"` | Edit task fields |
| `wg edit <id> --add-blocked-by X --remove-blocked-by Y` | Modify dependencies |
| `wg edit <id> --add-blocked-by X --max-iterations 3` | Add cycle back-edge |
| `wg edit <id> --add-tag T --remove-tag T` | Modify tags |
| `wg edit <id> --add-skill S --remove-skill S` | Modify skills |
| `wg edit <id> --model sonnet` | Change preferred model |

### Task state transitions

| Command | Purpose |
|---------|---------|
| `wg claim <id>` | Claim task (in-progress) |
| `wg unclaim <id>` | Release claimed task (back to open) |
| `wg done <id>` | Complete task |
| `wg pause <id>` | Pause task (coordinator skips it) |
| `wg resume <id>` | Resume a paused task |
| `wg fail <id> --reason "why"` | Mark task failed |
| `wg retry <id>` | Retry failed task |
| `wg abandon <id> --reason "why"` | Abandon permanently |
| `wg reclaim <id> --from old --to new` | Reassign from dead agent |

### Querying & viewing

| Command | Purpose |
|---------|---------|
| `wg list` | All tasks with status |
| `wg list --status open` | Filter: open, in-progress, done, failed, abandoned |
| `wg ready` | Tasks available to work on |
| `wg show <id>` | Full task details |
| `wg blocked <id>` | What's blocking a task |
| `wg why-blocked <id>` | Full transitive blocking chain |
| `wg context <id>` | Inputs from dependencies |
| `wg context <id> --dependents` | Tasks depending on this one's outputs |
| `wg log <id> --list` | View task log entries |
| `wg impact <id>` | What depends on this task |
| `wg status` | Quick one-screen overview |

### Visualization

| Command | Purpose |
|---------|---------|
| `wg viz` | ASCII dependency graph of open tasks |
| `wg viz --all` | Include done tasks |
| `wg viz --status done` | Filter by status |
| `wg viz --dot` | Graphviz DOT output |
| `wg viz --mermaid` | Mermaid diagram |
| `wg viz --critical-path` | Highlight critical path |
| `wg viz --dot -o graph.png` | Render to file |
| `wg tui` | Interactive TUI dashboard |

### Analysis & metrics

| Command | Purpose |
|---------|---------|
| `wg analyze` | Comprehensive health report |
| `wg check` | Graph validation (cycles, orphans) |
| `wg structure` | Entry points, dead ends, high-impact roots |
| `wg bottlenecks` | Tasks blocking the most work |
| `wg critical-path` | Longest dependency chain |
| `wg cycles` | Cycle detection and classification |
| `wg velocity --weeks 8` | Completion velocity over time |
| `wg aging` | Task age distribution |
| `wg forecast` | Completion forecast from velocity |
| `wg workload` | Agent workload balance |
| `wg resources` | Resource utilization |
| `wg cost <id>` | Cost including dependencies |
| `wg coordinate` | Ready tasks for parallel execution |
| `wg trajectory <id>` | Optimal claim order for context |
| `wg next --agent <id>` | Best next task for an agent |

### Service & agents

| Command | Purpose |
|---------|---------|
| `wg service start` | Start coordinator daemon |
| `wg service start --max-agents 5` | Start with parallelism limit |
| `wg service stop` | Stop daemon |
| `wg service pause` | Pause coordinator (running agents continue, no new spawns) |
| `wg service resume` | Resume coordinator dispatching |
| `wg service status` | Check daemon health |
| `wg agents` | List all agents |
| `wg agents --alive` | Only alive agents |
| `wg agents --working` | Only working agents |
| `wg agents --dead` | Only dead agents |
| `wg spawn <id> --executor claude` | Manually spawn agent |
| `wg spawn <id> --executor claude --model haiku` | Spawn with model override |
| `wg kill <agent-id>` | Kill an agent |
| `wg kill --all` | Kill all agents |
| `wg kill <id> --force` | Force kill (SIGKILL) |
| `wg dead-agents --cleanup` | Unclaim dead agents' tasks |
| `wg dead-agents --remove` | Remove from registry |

### Agency (roles, motivations, agents)

| Command | Purpose |
|---------|---------|
| `wg agency init` | Bootstrap agency with starter roles, motivations, and agents |
| `wg agency stats` | Performance analytics |
| `wg agency stats --by-model` | Per-model score breakdown |
| `wg models` | List known models and usage stats |
| `wg role add <id>` | Create a role |
| `wg role list` | List roles |
| `wg role show <id>` | Show role details |
| `wg role edit <id>` | Edit a role |
| `wg role rm <id>` | Remove a role |
| `wg motivation add <id>` | Create a motivation |
| `wg motivation list` | List motivations |
| `wg motivation show <id>` | Show motivation details |
| `wg motivation edit <id>` | Edit a motivation |
| `wg motivation rm <id>` | Remove a motivation |
| `wg agent create` | Create agent (role+motivation pairing) |
| `wg agent list` | List agents |
| `wg agent show <hash>` | Show agent details |
| `wg agent rm <hash>` | Remove an agent |
| `wg agent lineage <hash>` | Show agent ancestry |
| `wg agent performance <hash>` | Show agent performance |
| `wg assign <task> <agent-hash>` | Assign agent to task |
| `wg assign <task> --clear` | Clear assignment |
| `wg evaluate <task>` | Trigger task evaluation |
| `wg evolve` | Trigger evolution cycle |
| `wg evolve --strategy mutation --budget 3` | Targeted evolution |

### Artifacts & resources

| Command | Purpose |
|---------|---------|
| `wg artifact <task> <path>` | Record output file |
| `wg artifact <task>` | List task artifacts |
| `wg artifact <task> <path> --remove` | Remove artifact |
| `wg resource add <id> --type money --available 1000 --unit usd` | Add resource |
| `wg resource list` | List resources |
| `wg match <task>` | Find capable agents |

### Housekeeping

| Command | Purpose |
|---------|---------|
| `wg gc` | Remove terminal tasks (done/abandoned/failed) from the graph |
| `wg archive` | Archive completed tasks |
| `wg archive --dry-run` | Preview what would be archived |
| `wg archive --older 30d` | Only archive old completions |
| `wg archive --list` | List archived tasks |
| `wg reschedule <id> --after 24` | Delay task 24 hours |
| `wg reschedule <id> --at "2025-01-15T09:00:00Z"` | Schedule at specific time |
| `wg plan --budget 500 --hours 20` | Plan within constraints |

### Configuration

| Command | Purpose |
|---------|---------|
| `wg config --show` | Show current config |
| `wg config --init` | Create default config |
| `wg config --executor claude` | Set executor |
| `wg config --model opus` | Set default model |
| `wg config --max-agents 5` | Set agent limit |
| `wg config --auto-evaluate true` | Enable auto-evaluation |
| `wg config --auto-assign true` | Enable auto-assignment |

### Output options

All commands support `--json` for structured output. Run `wg --help` for the quick list or `wg --help-all` for every command.

## Executor and model awareness

### Environment variables

Every spawned agent receives these environment variables:

| Variable | Description |
|----------|-------------|
| `WG_TASK_ID` | The task ID you're working on |
| `WG_AGENT_ID` | Your agent ID |
| `WG_EXECUTOR_TYPE` | Executor type: `claude`, `amplifier`, or `shell` |
| `WG_MODEL` | Model you're running on (e.g. `opus`, `sonnet`, `haiku`) — set when a model is configured |

### Multi-executor awareness

You may be running under different executors:

- **claude** — Claude Code CLI (`claude --print`). You have full access to Claude Code tools (file editing, bash, etc).
- **amplifier** — Amplifier multi-agent runtime. You have access to installed amplifier bundles and can delegate to sub-agents.
- **shell** — Direct shell execution for scripted tasks.

Check `$WG_EXECUTOR_TYPE` to know which executor you're running under if your behavior should differ.

### Model awareness

The `$WG_MODEL` variable tells you what model you're running on. Different tasks may use different models based on complexity (model hierarchy: task.model > executor.model > coordinator.model).

Calibrate your approach to your model tier:
- **Frontier models** (opus): Tackle complex multi-file refactors, architectural decisions, nuanced trade-offs.
- **Mid-tier models** (sonnet): Good for standard implementation, bug fixes, well-scoped features.
- **Fast models** (haiku): Best for simple, well-defined tasks — lookups, single-file edits, formatting.

If a task feels beyond your model's capability, use `wg fail` with a clear reason rather than producing low-quality output.

### Amplifier bundles (amplifier executor only)

When running under the amplifier executor (`WG_EXECUTOR_TYPE=amplifier`), you can use installed amplifier bundles for specialized capabilities. Delegate to sub-agents when:

- The subtask is independent and well-scoped (e.g. "write tests for module X")
- Parallel execution would speed things up
- The subtask needs a different skill set than your current context

Do the work yourself when:
- The task is simple and sequential
- Context from prior steps is critical and hard to transfer
- Coordination overhead would exceed the work itself
