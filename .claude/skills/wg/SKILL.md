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
wg add "Implement backend" --after design-the-api
wg add "Write tests" --after implement-backend
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
wg watch                 # Stream events as JSON lines (live tail)
wg viz                   # ASCII dependency graph
wg tui                   # Interactive TUI dashboard
wg chat "How is task X?" # Ask the coordinator a question
wg chat -i               # Interactive chat with the coordinator
wg chat --history        # Review past coordinator conversations
```

### What you do NOT do as coordinator

- **Don't `wg claim`** — the service claims tasks automatically
- **Don't `wg spawn`** — the service spawns agents automatically
- **Don't work on tasks yourself** — spawned agents do the work

Always use `wg done` to complete tasks. Tasks with `--verify` enter a `pending-validation` state and need `wg approve` or `wg reject` to finalize.

### Reviewing completed work

Tasks created with `--verify` land in `pending-validation` when agents mark them done. As coordinator, review and finalize:

```bash
wg list --status pending-validation   # See tasks awaiting review
wg show <task-id>                     # Inspect work and artifacts
wg approve <task-id>                  # Accept — transitions to done
wg reject <task-id> --reason "why"    # Reject — reopens for retry (or fails after max rejections)
```

## If you ARE a spawned agent working on a task

You were spawned by the service to work on a specific task. Your workflow:

```bash
wg show <task-id>        # Understand what to do
wg context <task-id>     # See inputs from dependencies
wg log <task-id> "msg"   # Log progress as you work
wg done <task-id>        # Mark complete when finished
```

### Checking and sending messages

Other agents or the coordinator may send you messages with updated requirements or feedback. Check for messages periodically and always reply:

```bash
wg msg read <task-id> --agent $WG_AGENT_ID   # Read unread messages (marks as read)
wg msg send <task-id> "Acknowledged — working on it"  # Reply to messages
wg msg poll <task-id> --agent $WG_AGENT_ID   # Poll without blocking (exit 0 = new, 1 = none)
```

If you discover new work while working:

```bash
wg add "New task" --after <current-task>
```

## Task decomposition

When working on a task, you may discover that it's larger than expected or has independent parts. Rather than doing everything in one shot, decompose into subtasks and let the coordinator dispatch them.

### When to decompose

- **3+ independent parts** that touch disjoint files — parallelize with a diamond
- **Discovered bugs or issues** unrelated to the current task — spin off a fix task
- **Missing prerequisites** — create a blocking task for the prerequisite

### When NOT to decompose

- **Small tasks** — if the total work is under ~200 lines of changes, just do it
- **Shared files** — if the subtasks would all edit the same files, keep them sequential or do them yourself. Parallel agents editing the same file will overwrite each other
- **High coordination overhead** — if explaining the decomposition is harder than doing the work, just do the work

### Diamond pattern for parallel decomposition

Fan out independent work, then join with an integrator:

```bash
# Fan out: each subtask depends on the current task
wg add "Implement module A" --after <current-task> -d "File scope: src/a.rs"
wg add "Implement module B" --after <current-task> -d "File scope: src/b.rs"
wg add "Implement module C" --after <current-task> -d "File scope: src/c.rs"

# Always add an integrator at the join point
wg add "Integrate modules A, B, C" --after implement-module-a,implement-module-b,implement-module-c
```

**Always include an integrator task at join points.** Without one, parallel outputs never get merged and downstream tasks see inconsistent state.

### Guardrails

Two guardrails prevent runaway decomposition:

- **`max_child_tasks_per_agent`** (default: 10) — max tasks a single agent can create via `wg add`. If you hit this limit, the system tells you. Use `wg fail` or `wg log` to explain why more decomposition is needed.
- **`max_task_depth`** (default: 8) — max depth of `--after` chains from root tasks. Prevents infinite decomposition chains. If you hit this, create tasks at the current level instead of nesting deeper.

Configure with:
```bash
wg config --max-child-tasks 15
wg config --max-task-depth 10
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
                              → [done --verify] → pending-validation → [approve] → done
                                                                     → [reject] → open (retry)
                              → [fail] → failed → [retry] → open
                              → [abandon] → abandoned
                              → [wait] → waiting → [condition met] → in-progress
```

**Note:** The `wg approve` and `wg reject` commands handle tasks in `pending-validation` state (tasks created with `--verify`).

## Cycles (repeating workflows)

Some workflows repeat. Workgraph models these as **structural cycles** — `after` back-edges with a `CycleConfig` that controls iteration limits. When a cycle iteration completes, the cycle header task is reset to `open` with its `loop_iteration` incremented, and intermediate tasks are re-opened automatically.

```bash
# Create a write/review cycle, max 3 iterations
wg add "Write" --id write --after review --max-iterations 3
wg add "Review" --after write --id review

# Inspect cycles
wg cycles
```

As a spawned agent on a task inside a cycle, check `wg show <task-id>` for `loop_iteration` to know which pass you're on. Review previous logs and artifacts to build on prior work. If the work has converged and no more iterations are needed, use `wg done <task-id> --converged` to signal early termination — the cycle will not iterate again.

```bash
wg cycles                   # List detected cycles and status
wg show <task-id>           # See loop_iteration and cycle membership
```

### Cycle configuration flags

Fine-tune cycle behavior when creating or editing tasks:

```bash
wg add "Task" --after dep --max-iterations 5 \
  --cycle-guard "task:check=done"    # Guard: only iterate when check is done
  --cycle-delay 5m                   # Wait 5 minutes between iterations
  --no-converge                      # Force all iterations (agents can't signal --converged)
  --no-restart-on-failure            # Don't auto-restart the cycle on failure
  --max-failure-restarts 2           # Cap failure-triggered restarts (default: 3)
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
| `wg add "X" --after Y` | Create task with dependency |
| `wg add "X" --after a,b,c` | Multiple dependencies (comma-separated) |
| `wg add "X" --skill rust --input src/foo.rs --deliverable docs/out.md` | Task with skills, inputs, deliverables |
| `wg add "X" --model haiku` | Task with preferred model |
| `wg add "X" --provider openrouter` | Task with preferred provider |
| `wg add "X" --context-scope clean` | Set prompt context scope (clean/task/graph/full) |
| `wg add "X" --exec-mode light` | Set execution weight (full/light/bare/shell) |
| `wg add "X" --verify "Tests pass"` | Task requiring review before completion |
| `wg add "X" --tag important --hours 2` | Tags and estimates |
| `wg add "X" --after Y --max-iterations 3` | Create cycle header with max 3 iterations |
| `wg add "X" --paused` | Create task in paused state |
| `wg add "X" --delay 1h` | Task becomes ready after delay (30s, 5m, 1h, 1d) |
| `wg add "X" --not-before "2026-01-15T09:00:00Z"` | Schedule task for specific time (ISO 8601) |
| `wg add "X" --no-place` | Skip automatic placement analysis |
| `wg add "X" --place-near a,b` | Placement hint: near these tasks |
| `wg add "X" --place-before a,b` | Placement hint: before these tasks |
| `wg add "X" --repo peer-name` | Create task in a peer workgraph (by name or path) |
| `wg edit <id> --title "New" --description "New"` | Edit task fields |
| `wg edit <id> --add-after X --remove-after Y` | Modify dependencies |
| `wg edit <id> --add-after X --max-iterations 3` | Add cycle back-edge |
| `wg edit <id> --add-tag T --remove-tag T` | Modify tags |
| `wg edit <id> --add-skill S --remove-skill S` | Modify skills |
| `wg edit <id> --model sonnet` | Change preferred model |
| `wg edit <id> --provider anthropic` | Change preferred provider |
| `wg edit <id> --context-scope graph` | Change context scope |
| `wg edit <id> --exec-mode bare` | Change execution weight |
| `wg edit <id> --verify "cargo test passes"` | Set or update verification criteria |
| `wg edit <id> --delay 30m` | Set scheduling delay |
| `wg edit <id> --not-before "2026-03-20T00:00:00Z"` | Set absolute schedule |
| `wg edit <id> --no-converge` | Force all cycle iterations |
| `wg edit <id> --no-restart-on-failure` | Disable cycle restart on failure |
| `wg edit <id> --max-failure-restarts 2` | Cap failure-triggered restarts |
| `wg add-dep <task> <dep>` | Add a dependency edge between two existing tasks |
| `wg rm-dep <task> <dep>` | Remove a dependency edge between two tasks |

### Task state transitions

| Command | Purpose |
|---------|---------|
| `wg claim <id>` | Claim task (in-progress) |
| `wg unclaim <id>` | Release claimed task (back to open) |
| `wg done <id>` | Complete task |
| `wg done <id> --converged` | Complete task and signal cycle convergence |
| `wg approve <id>` | Approve a task in pending-validation (transitions to done) |
| `wg reject <id> --reason "why"` | Reject a task in pending-validation (reopens for retry) |
| `wg publish <id>` | Publish a draft task (validates deps, resumes subgraph) |
| `wg publish <id> --only` | Publish single task only (skip subgraph propagation) |
| `wg pause <id>` | Pause task (coordinator skips it) |
| `wg resume <id>` | Resume a paused task |
| `wg wait <id> --until "condition"` | Park task as Waiting until condition is met |
| `wg wait <id> --until "task:dep=done"` | Wait for another task to complete |
| `wg wait <id> --until "timer:5m"` | Wait for a timer duration |
| `wg wait <id> --until "message"` | Wait for a message |
| `wg wait <id> --until "human-input"` | Wait for a human message |
| `wg wait <id> --until "file:path"` | Wait for a file to change |
| `wg wait <id> --checkpoint "summary"` | Save progress checkpoint when parking |
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
| `wg discover` | Show recently completed tasks and artifacts (last 24h) |
| `wg discover --since 7d` | Custom time window (e.g. 30m, 24h, 7d) |
| `wg discover --with-artifacts` | Include artifact paths in output |

### Visualization

| Command | Purpose |
|---------|---------|
| `wg viz` | ASCII dependency graph of open tasks |
| `wg viz [TASK_ID]...` | Show only subgraphs containing specified tasks |
| `wg viz --all` | Include done tasks |
| `wg viz --status done` | Filter by status |
| `wg viz --dot` | Graphviz DOT output |
| `wg viz --mermaid` | Mermaid diagram |
| `wg viz --critical-path` | Highlight critical path |
| `wg viz --dot -o graph.png` | Render to file |
| `wg viz --show-internal` | Show internal tasks (assign-*, evaluate-*) normally hidden |
| `wg viz --no-tui` | Force static output even when stdout is interactive |
| `wg tui` | Interactive TUI dashboard |

### Monitoring & event streaming

| Command | Purpose |
|---------|---------|
| `wg watch` | Stream workgraph events as JSON lines |
| `wg watch --event task_state` | Filter by event type (task_state, evaluation, agent, all) |
| `wg watch --task <id>` | Filter events to a specific task (prefix match) |
| `wg watch --replay 10` | Include N recent historical events before streaming |

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
| `wg next --actor <id>` | Best next task for an agent |

### Service & agents

| Command | Purpose |
|---------|---------|
| `wg service start` | Start coordinator daemon |
| `wg service start --max-agents 5` | Start with parallelism limit |
| `wg service stop` | Stop daemon |
| `wg service restart` | Restart daemon (graceful stop then start) |
| `wg service pause` | Pause coordinator (running agents continue, no new spawns) |
| `wg service resume` | Resume coordinator dispatching |
| `wg service status` | Check daemon health |
| `wg service reload` | Reload daemon configuration without restarting |
| `wg service tick` | Run a single coordinator tick (debug mode) |
| `wg service create-coordinator` | Create a new coordinator session |
| `wg service delete-coordinator` | Delete a coordinator session |
| `wg service archive-coordinator` | Archive a coordinator session (mark as Done) |
| `wg service stop-coordinator` | Stop a coordinator session (kill agent, reset to Open) |
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
| `wg dead-agents --purge` | Purge dead/done/failed agents from registry |
| `wg dead-agents --purge --delete-dirs` | Also delete agent work directories when purging |
| `wg dead-agents --threshold 30` | Override heartbeat timeout threshold (minutes) |
| `wg heartbeat <agent-id>` | Record agent heartbeat |
| `wg heartbeat --check` | Check for stale agents (no heartbeat within threshold) |
| `wg heartbeat --threshold 10` | Override stale threshold in minutes (default: 5) |

### Messaging

| Command | Purpose |
|---------|---------|
| `wg msg send <task> "message"` | Send a message to a task/agent |
| `wg msg list <task>` | List all messages for a task |
| `wg msg read <task> --agent <id>` | Read unread messages (marks as read) |
| `wg msg poll <task> --agent <id>` | Poll for new messages (exit code 0 = new, 1 = none) |

### Chat (coordinator interaction)

| Command | Purpose |
|---------|---------|
| `wg chat "message"` | Send a message to the coordinator |
| `wg chat -i` | Interactive REPL mode |
| `wg chat --history` | Show chat history |
| `wg chat --clear` | Clear chat history |
| `wg chat --attachment path/to/file` | Attach a file to the message |
| `wg chat --coordinator 1` | Target a specific coordinator (multi-coordinator) |

### Notifications & integrations

| Command | Purpose |
|---------|---------|
| `wg notify <task>` | Send task notification to Matrix room |
| `wg notify <task> --room "#room"` | Target specific Matrix room |
| `wg notify <task> -m "message"` | Include custom message with notification |
| `wg matrix listen` | Start Matrix message listener |
| `wg matrix send "message"` | Send a message to a Matrix room |
| `wg matrix status` | Show Matrix connection status |
| `wg matrix login` | Login with password (caches access token) |
| `wg matrix logout` | Logout and clear cached credentials |
| `wg telegram listen` | Start Telegram bot listener |
| `wg telegram send "message"` | Send a message to configured Telegram chat |
| `wg telegram status` | Show Telegram configuration status |

### Housekeeping & maintenance

| Command | Purpose |
|---------|---------|
| `wg compact` | Distill graph state into context.md |
| `wg sweep` | Detect and recover orphaned in-progress tasks with dead agents |
| `wg sweep --dry-run` | Preview orphaned tasks without fixing |
| `wg checkpoint <task> -s "summary"` | Save checkpoint for context preservation |
| `wg checkpoint <task> --list` | List checkpoints for a task |
| `wg stats` | Show time counters and agent statistics |
| `wg gc` | Remove terminal tasks (done/abandoned/failed) from the graph |
| `wg archive` | Archive completed tasks |
| `wg archive --dry-run` | Preview what would be archived |
| `wg archive --older 30d` | Only archive old completions |
| `wg archive --list` | List archived tasks |
| `wg reschedule <id> --after 24` | Delay task 24 hours |
| `wg reschedule <id> --at "2025-01-15T09:00:00Z"` | Schedule at specific time |
| `wg plan --budget 500 --hours 20` | Plan within constraints |
| `wg exec <task>` | Execute a task's shell command (claim + run + done/fail) |
| `wg exec <task> --set "cargo test"` | Set the exec command for a task |
| `wg exec <task> --clear` | Clear the exec command |
| `wg exec <task> --dry-run` | Show what would be executed |
| `wg replay` | Snapshot graph, selectively reset tasks, re-execute with different model |
| `wg replay --failed-only` | Only reset failed/abandoned tasks |
| `wg replay --below-score 0.7` | Only reset tasks with evaluation score below threshold |
| `wg replay --tasks a,b,c` | Reset specific tasks plus transitive dependents |
| `wg replay --subgraph <id>` | Only replay tasks in subgraph rooted at given task |
| `wg replay --keep-done 0.9` | Preserve done tasks scoring above threshold (default: 0.9) |
| `wg replay --plan-only` | Dry run: show what would be reset |

### Agency (roles, tradeoffs, agents)

| Command | Purpose |
|---------|---------|
| `wg agency init` | Bootstrap agency with starter roles, tradeoffs, and agents |
| `wg agency stats` | Performance analytics |
| `wg agency stats --by-model` | Per-model score breakdown |
| `wg role add <id>` | Create a role |
| `wg role list` | List roles |
| `wg role show <id>` | Show role details |
| `wg role edit <id>` | Edit a role |
| `wg role rm <id>` | Remove a role |
| `wg tradeoff add <id>` | Create a tradeoff |
| `wg tradeoff list` | List tradeoffs |
| `wg tradeoff show <id>` | Show tradeoff details |
| `wg tradeoff edit <id>` | Edit a tradeoff |
| `wg tradeoff rm <id>` | Remove a tradeoff |
| `wg agent create` | Create agent (role+tradeoff pairing) |
| `wg agent list` | List agents |
| `wg agent show <hash>` | Show agent details |
| `wg agent rm <hash>` | Remove an agent |
| `wg agent lineage <hash>` | Show agent ancestry |
| `wg agent performance <hash>` | Show agent performance |
| `wg assign <task> <agent-hash>` | Assign agent to task |
| `wg assign <task> --clear` | Clear assignment |
| `wg evaluate run <task>` | Trigger LLM-based evaluation of a completed task |
| `wg evaluate record --task <id> --score <n> --source <tag>` | Record evaluation from external source |
| `wg evaluate show` | Show evaluation history (filter by `--task`, `--agent`, `--source`) |
| `wg evolve` | Trigger evolution cycle |
| `wg evolve --strategy mutation --budget 3` | Targeted evolution |

### Federation (cross-repo agency sharing)

| Command | Purpose |
|---------|---------|
| `wg agency remote add <name> <path>` | Add a named remote agency store |
| `wg agency remote remove <name>` | Remove a named remote |
| `wg agency remote list` | List configured remotes |
| `wg agency remote show <name>` | Show remote details and entity counts |
| `wg agency scan <root>` | Scan filesystem for agency stores |
| `wg agency pull <source>` | Pull entities from another agency store |
| `wg agency push <target>` | Push local entities to another agency store |
| `wg agency merge <sources...>` | Merge entities from multiple stores |

### Peer workgraphs (cross-repo communication)

| Command | Purpose |
|---------|---------|
| `wg peer add <name> <path>` | Register a peer workgraph instance |
| `wg peer remove <name>` | Remove a registered peer |
| `wg peer list` | List all configured peers with service status |
| `wg peer show <name>` | Show detailed info about a peer |
| `wg peer status` | Quick health check of all peers |

### Run snapshots

| Command | Purpose |
|---------|---------|
| `wg runs list` | List all run snapshots |
| `wg runs show <id>` | Show details of a specific run |
| `wg runs restore <id>` | Restore graph from a run snapshot |
| `wg runs diff <id>` | Diff current graph against a run snapshot |

### Functions (reusable workflow patterns)

| Command | Purpose |
|---------|---------|
| `wg func list` | List available functions |
| `wg func show <id>` | Show function details and required inputs |
| `wg func extract <task-id>...` | Extract a function from completed task(s) |
| `wg func apply <id> --input key=value` | Create tasks from a function with provided inputs |
| `wg func bootstrap` | Bootstrap the extract-function meta-function |
| `wg func make-adaptive <id>` | Upgrade a generative function to adaptive (adds run memory) |

### Traces (execution history & sharing)

| Command | Purpose |
|---------|---------|
| `wg trace show <id>` | Show the execution history of a task |
| `wg trace export --visibility <zone>` | Export trace data filtered by visibility zone |
| `wg trace import <file>` | Import a trace export file as read-only context |

### Model & provider management

| Command | Purpose |
|---------|---------|
| `wg model list` | Show all models in the registry (built-in + user-defined) |
| `wg model add` | Add or update a model in the config registry |
| `wg model remove <id>` | Remove a model from the config registry |
| `wg model set-default <id>` | Set the default model for agent dispatch |
| `wg model routing` | Show per-role model routing configuration |
| `wg model set <role> <model>` | Set the model for a specific dispatch role |
| `wg models list` | List models from the local registry |
| `wg models search <query>` | Search models from OpenRouter by name/ID/description |
| `wg models remote` | List all models available on OpenRouter |
| `wg models add` | Add a custom model to the local registry |
| `wg models set-default <id>` | Set the default model |
| `wg models init` | Initialize models.yaml with defaults |
| `wg endpoints list` | List all configured LLM endpoints |
| `wg endpoints add` | Add a new LLM endpoint |
| `wg endpoints remove <name>` | Remove an endpoint by name |
| `wg endpoints set-default <name>` | Set an endpoint as the default |
| `wg endpoints test` | Test endpoint connectivity (hits /models API) |
| `wg key set <provider>` | Configure an API key for a provider |
| `wg key check` | Validate API key availability and status |
| `wg key list` | Show key configuration status for all providers |

### Artifacts & resources

| Command | Purpose |
|---------|---------|
| `wg artifact <task> <path>` | Record output file |
| `wg artifact <task>` | List task artifacts |
| `wg artifact <task> <path> --remove` | Remove artifact |
| `wg resource add <id> --type money --available 1000 --unit usd` | Add resource |
| `wg resource list` | List resources |
| `wg match <task>` | Find capable agents |

### Skills

| Command | Purpose |
|---------|---------|
| `wg skill list` | List all skills used across tasks |
| `wg skill task <id>` | Show skills for a specific task |
| `wg skill find <name>` | Find tasks requiring a specific skill |
| `wg skill install` | Install the wg Claude Code skill to ~/.claude/skills/wg/ |

### Setup & configuration

| Command | Purpose |
|---------|---------|
| `wg setup` | Interactive configuration wizard for first-time setup |
| `wg config --show` | Show current config |
| `wg config --list` | Show merged config with source annotations (global/local/default) |
| `wg config --init` | Create default config |
| `wg config --global --executor claude` | Write to global config (~/.workgraph/config.toml) |
| `wg config --local --model opus` | Write to local config (default for writes) |
| `wg config --executor claude` | Set executor |
| `wg config --model opus` | Set default model |
| `wg config --max-agents 5` | Set agent limit |
| `wg config --max-coordinators 2` | Set max concurrent coordinator sessions |
| `wg config --coordinator-interval 30` | Set coordinator poll interval in seconds |
| `wg config --poll-interval 60` | Set service daemon background poll interval in seconds |
| `wg config --coordinator-executor claude` | Set coordinator executor |
| `wg config --auto-evaluate true` | Enable auto-evaluation |
| `wg config --auto-assign true` | Enable auto-assignment |
| `wg config --auto-place true` | Enable automatic placement analysis on new tasks |
| `wg config --auto-create true` | Enable automatic creator agent invocation |
| `wg config --auto-triage true` | Enable automatic triage of dead agents |
| `wg config --creator-agent <hash>` | Set creator agent (content-hash) |
| `wg config --creator-model <model>` | Set model for creator agents |
| `wg config --assigner-model <model>` | Set model for assigner agents |
| `wg config --evaluator-model <model>` | Set model for evaluator agents |
| `wg config --evolver-model <model>` | Set model for evolver agents |
| `wg config --assigner-agent <hash>` | Set assigner agent (content-hash) |
| `wg config --evaluator-agent <hash>` | Set evaluator agent (content-hash) |
| `wg config --evolver-agent <hash>` | Set evolver agent (content-hash) |
| `wg config --triage-model haiku` | Set model for triage (default: haiku) |
| `wg config --triage-timeout 30` | Set timeout in seconds for triage calls |
| `wg config --triage-max-log-bytes 50000` | Max bytes to read from agent output log for triage |
| `wg config --retention-heuristics "policy"` | Set retention heuristics (prose policy for evolver) |
| `wg config --eval-gate-threshold 0.7` | Set eval gate threshold (0.0-1.0) |
| `wg config --eval-gate-all true` | Apply eval gate to all tasks (not just eval-gate tagged) |
| `wg config --flip-enabled true` | Enable FLIP (roundtrip intent fidelity) evaluation |
| `wg config --flip-inference-model <m>` | Model for FLIP inference phase |
| `wg config --flip-comparison-model <m>` | Model for FLIP comparison phase |
| `wg config --flip-verification-threshold 0.7` | FLIP score threshold for Opus verification |
| `wg config --flip-verification-model opus` | Model for FLIP-triggered verification |
| `wg config --chat-history true` | Enable chat history persistence |
| `wg config --chat-history-max 100` | Max chat history entries |
| `wg config --retry-context-tokens 2000` | Max tokens of previous-attempt context on retry |
| `wg config --install-global` | Install project config as global default |
| `wg config --viz-edge-color mixed` | Viz edge color style (gray/white/mixed) |
| `wg config --tui-counters uptime,active` | TUI time counters (uptime, cumulative, active, session) |
| `wg config --tiers` | Show current tier-to-model assignments |
| `wg config --tier standard=gpt-4o` | Set which model a tier uses |
| `wg config --models` | Show all model routing assignments (per-role) |
| `wg config --set-model <role> <model>` | Set model for a dispatch role |
| `wg config --set-provider <role> <provider>` | Set provider for a dispatch role |
| `wg config --set-endpoint <role> <endpoint>` | Bind a named endpoint to a dispatch role |
| `wg config --role-model <role>=<model>` | Set model for dispatch role (key=value syntax) |
| `wg config --role-provider <role>=<provider>` | Set provider for dispatch role (key=value syntax) |
| `wg config --set-key <provider> --file <path>` | Set API key file for a provider |
| `wg config --check-key` | Check OpenRouter API key validity |
| `wg config --registry` | Show all model registry entries |
| `wg config --registry-add --id <id> --provider <p> --reg-model <m> --reg-tier <t>` | Add model to registry |
| `wg config --registry-remove <id>` | Remove model from registry |

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

### Provider awareness

Tasks can specify a `--provider` (anthropic, openai, openrouter, local) to route to a specific LLM provider. The provider resolves at dispatch time: task.provider > config default.

### Execution modes

Tasks can specify `--exec-mode` to control what tools the agent receives:

| Mode | Description | Use Case |
|------|-------------|----------|
| `full` | Full Claude Code tools (file editing, bash, etc.) — default | Standard implementation, debugging |
| `light` | Read-only tools (no file writes, no destructive commands) | Code review, analysis, audit |
| `bare` | Only wg CLI instructions, no extra tools | Meta-tasks, workflow management |
| `shell` | No LLM — direct shell execution of `exec` command | Scripted automation, CI/CD |

```bash
wg add "Review code" --exec-mode light
wg add "Run tests" --exec-mode shell
wg exec run-tests --set "cargo test"
```

### Context scopes

Tasks can specify a `--context-scope` that controls how much context the agent receives in its prompt:

| Scope | Description | Use Case |
|-------|-------------|----------|
| `clean` | Bare executor — no wg CLI instructions | Pure computation, translation, writing |
| `task` | Task-aware with wg workflow instructions (default) | Standard implementation, bug fixes |
| `graph` | Adds project description, 1-hop neighborhood, status summary | Integration, cross-component review |
| `full` | Adds full graph summary, CLAUDE.md, system preamble | Meta-tasks, workflow design |

Set scope when creating or editing tasks:
```bash
wg add "Translate document" --context-scope clean
wg edit <task-id> --context-scope graph
```

Resolution priority: task > role default > coordinator config > "task" (default).

### Amplifier bundles (amplifier executor only)

When running under the amplifier executor (`WG_EXECUTOR_TYPE=amplifier`), you can use installed amplifier bundles for specialized capabilities. Delegate to sub-agents when:

- The subtask is independent and well-scoped (e.g. "write tests for module X")
- Parallel execution would speed things up
- The subtask needs a different skill set than your current context

Do the work yourself when:
- The task is simple and sequential
- Context from prior steps is critical and hard to transfer
- Coordination overhead would exceed the work itself

## Multi-coordinator support

The service supports multiple concurrent coordinator sessions. Each coordinator manages its own scope of tasks and agents.

```bash
wg service create-coordinator        # Create a new coordinator session
wg service stop-coordinator <id>     # Stop a coordinator (kill agent, reset to Open)
wg service archive-coordinator <id>  # Archive a coordinator (mark as Done)
wg service delete-coordinator <id>   # Delete a coordinator session
wg config --max-coordinators 4       # Set max concurrent coordinators (default: 4)
wg chat --coordinator 1 "message"    # Target a specific coordinator
```
