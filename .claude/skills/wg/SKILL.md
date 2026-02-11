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
wg dag                   # ASCII DAG of dependencies
wg tui                   # Interactive TUI dashboard
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
                             → [submit] → pending-review → [approve] → done
                                                         → [reject] → open
                             → [fail] → failed → [retry] → open
                             → [abandon] → abandoned
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
| `wg edit <id> --title "New" --description "New"` | Edit task fields |
| `wg edit <id> --add-blocked-by X --remove-blocked-by Y` | Modify dependencies |
| `wg edit <id> --add-tag T --remove-tag T` | Modify tags |
| `wg edit <id> --add-skill S --remove-skill S` | Modify skills |
| `wg edit <id> --model sonnet` | Change preferred model |

### Task state transitions

| Command | Purpose |
|---------|---------|
| `wg claim <id>` | Claim task (in-progress) |
| `wg unclaim <id>` | Release claimed task (back to open) |
| `wg done <id>` | Complete task |
| `wg submit <id>` | Submit verified task for review |
| `wg approve <id>` | Approve reviewed task |
| `wg reject <id> --reason "why"` | Reject reviewed task |
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
| `wg dag` | ASCII DAG of open tasks |
| `wg dag --all` | Include done tasks |
| `wg dag --status done` | Filter by status |
| `wg viz --format dot` | Graphviz DOT output |
| `wg viz --format mermaid` | Mermaid diagram |
| `wg viz --format ascii` | ASCII visualization |
| `wg viz --critical-path` | Highlight critical path |
| `wg viz -o graph.png` | Render to file |
| `wg tui` | Interactive TUI dashboard |

### Analysis & metrics

| Command | Purpose |
|---------|---------|
| `wg analyze` | Comprehensive health report |
| `wg check` | Graph validation (cycles, orphans) |
| `wg structure` | Entry points, dead ends, high-impact roots |
| `wg bottlenecks` | Tasks blocking the most work |
| `wg critical-path` | Longest dependency chain |
| `wg loops` | Cycle detection and classification |
| `wg velocity --weeks 8` | Completion velocity over time |
| `wg aging` | Task age distribution |
| `wg forecast` | Completion forecast from velocity |
| `wg workload` | Actor workload balance |
| `wg resources` | Resource utilization |
| `wg cost <id>` | Cost including dependencies |
| `wg coordinate` | Ready tasks for parallel execution |
| `wg trajectory <id>` | Optimal claim order for context |
| `wg next --actor <id>` | Best next task for an actor |

### Service & agents

| Command | Purpose |
|---------|---------|
| `wg service start` | Start coordinator daemon |
| `wg service start --max-agents 5` | Start with parallelism limit |
| `wg service stop` | Stop daemon |
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
| `wg dead-agents --check` | Detect dead agents |
| `wg dead-agents --cleanup` | Unclaim dead agents' tasks |
| `wg dead-agents --remove` | Remove from registry |

### Agency (roles, motivations, agents)

| Command | Purpose |
|---------|---------|
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
| `wg agency stats` | Performance analytics |

### Artifacts & resources

| Command | Purpose |
|---------|---------|
| `wg artifact <task> <path>` | Record output file |
| `wg artifact <task>` | List task artifacts |
| `wg artifact <task> <path> --remove` | Remove artifact |
| `wg resource add <id> --type money --available 1000 --unit usd` | Add resource |
| `wg resource list` | List resources |
| `wg actor add <id> --role engineer --capability rust` | Add actor |
| `wg match <task>` | Find capable actors |

### Housekeeping

| Command | Purpose |
|---------|---------|
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
