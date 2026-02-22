# workgraph

A task graph for getting things done. Works for humans, works for AI agents, works for both at once.

## What is this?

You've got tasks. Some block others. Multiple people (or AIs) need to coordinate without stepping on each other. Workgraph handles that.

```bash
wg init
wg add "Design the API"
wg add "Build the backend" --blocked-by design-the-api
wg add "Write tests" --blocked-by build-the-backend

wg service start   # spawns agents on ready tasks automatically
wg agents          # who's working on what?
```

Tasks flow through `open → in-progress → done`. Dependencies are respected. The service handles claiming and spawning — no one works on the same thing twice.

## Install

From source:

```bash
git clone https://github.com/graphwork/workgraph
cd workgraph
cargo install --path .
```

Or directly via cargo:

```bash
cargo install --git https://github.com/graphwork/workgraph
```

Verify it works:

```bash
wg --help
```

## Setup

### 1. Initialize in your project

```bash
cd your-project
wg init
```

This creates `.workgraph/` with your task graph.

### 2. Add some tasks

```bash
# Simple task
wg add "Set up CI pipeline"

# Task with a blocker
wg add "Deploy to staging" --blocked-by set-up-ci-pipeline

# Task with metadata
wg add "Implement auth" \
  --hours 8 \
  --skill rust \
  --skill security \
  --deliverable src/auth.rs

# Task with per-task model override
wg add "Quick formatting fix" --model haiku

# Task requiring review before completion
wg add "Security audit" --verify "All findings documented with severity ratings"

# Task with visibility for cross-org sharing
wg add "Public API design" --visibility public
```

### 3. Edit tasks after creation

```bash
wg edit my-task --title "Better title"
wg edit my-task --add-blocked-by other-task
wg edit my-task --remove-tag stale --add-tag urgent
wg edit my-task --model opus
wg edit my-task --add-skill security --remove-skill docs
```

### 4. Register yourself (or your AI agent)

```bash
# Human
wg agent create "Erik" \
  --executor matrix \
  --contact "@erik:server" \
  --capabilities rust,python \
  --trust-level verified

# AI agent
wg agent create "Claude Coder" \
  --role <role-hash> \
  --motivation <motivation-hash> \
  --capabilities coding,testing,docs
```

### 5. Start working

```bash
# Service mode (recommended) — auto-spawns agents on ready tasks
wg service start

# Or manual mode — claim and work on tasks yourself
wg ready
wg claim set-up-ci-pipeline --actor erik
# ... do the work ...
wg done set-up-ci-pipeline       # unblocks deploy-to-staging
```

### 6. Verification workflow

Tasks created with `--verify` require human approval before completion:

```bash
# Create a task that needs review
wg add "Security audit" --verify "All findings documented with severity ratings"

# Agent works on it, then marks it done
wg done security-audit

# The verify field is recorded for human reviewers to check
wg show security-audit
```

The verify criteria are stored on the task for auditing and review purposes.

## Using with AI Coding Assistants

Workgraph includes a skill definition that teaches AI assistants to use the service as a coordinator.

### Claude Code

From the workgraph directory, install the skill:

```bash
# Personal (all your projects)
cp -r .claude/skills/wg ~/.claude/skills/

# Or project-specific
cp -r .claude/skills/wg /path/to/your-project/.claude/skills/
```

The skill has YAML frontmatter so Claude auto-detects when to use it. You can also invoke explicitly with `/wg`.

Add this to your project's `CLAUDE.md` (or `~/.claude/CLAUDE.md` for global):

```markdown
Use workgraph for task management.

At the start of each session, run `wg quickstart` in your terminal to orient yourself.
Use `wg service start` to dispatch work — do not manually claim tasks.
```

### OpenCode / Codex / Other Agents

Add the core instruction to your agent's system prompt or `AGENTS.md`:

```markdown
## Task Management

Use workgraph (`wg`) for task coordination. Run `wg quickstart` to orient yourself.

As a top-level agent, use service mode — do not manually claim tasks:
- `wg service start` to start the coordinator
- `wg add "Task" --blocked-by dep` to define work
- `wg list` / `wg agents` to monitor progress

The service automatically spawns agents and claims tasks.
See `wg --help` for all commands.
```

### What the skill teaches

The skill teaches agents to:
- Run `wg quickstart` at session start to orient themselves
- Act as a coordinator: start the service, define tasks, monitor progress
- Let the service handle claiming and spawning — not do it manually
- Use manual mode only as a fallback when working alone without the service

## Agentic workflows

### Pattern 1: Service mode (recommended)

Start the service and let it handle everything:

```bash
# Define the work
wg add "Refactor auth module" --skill rust
wg add "Update tests" --blocked-by refactor-auth-module --skill testing
wg add "Update docs" --blocked-by refactor-auth-module --skill docs

# Start the service — it spawns agents on ready tasks automatically
wg service start --max-agents 4

# Monitor
wg agents    # who's working on what
wg list      # task status
wg tui       # interactive dashboard
```

The service claims tasks, spawns agents, detects dead agents, and picks up newly unblocked work — all automatically.

### Pattern 2: Agent plans, service executes

Let a top-level agent define the work, then the service dispatches it:

```markdown
# In CLAUDE.md or your prompt:

Break down this goal into tasks using workgraph:
1. Analyze what needs to be done
2. Create tasks with `wg add`, linking dependencies with --blocked-by
3. Start `wg service start` to dispatch work automatically
4. Monitor with `wg list` and `wg agents`
5. If you discover more work, add it to the graph — the service picks it up
```

### Pattern 3: Mixed human + AI

```bash
# Human claims the design work
wg claim design-api --actor erik

# Service handles implementation once design is done
wg service start
```

The service waits for your work to complete before spawning agents on dependent tasks.

### Pattern 4: Manual mode (single agent, no service)

For simple cases where you don't need parallel execution:

```bash
wg ready                         # see what's available
wg claim set-up-ci-pipeline --actor claude
# ... do the work ...
wg done set-up-ci-pipeline       # unblocks dependents
```

## Service

The service daemon automates agent spawning and lifecycle management. Start it once and it continuously picks up ready tasks, spawns agents, and cleans up dead ones.

### Quick start

```bash
wg service start
```

That's it. The daemon watches your task graph and auto-spawns agents on ready tasks (up to `max_agents` in parallel). When a task completes and unblocks new ones, the daemon picks those up too.

Monitor what's happening:

```bash
wg service status    # daemon info, agent summary, coordinator state
wg agents            # list all agents
wg tui               # interactive dashboard
```

Stop the daemon when you're done:

```bash
wg service stop              # stop daemon (agents keep running)
wg service stop --kill-agents  # stop daemon and all agents
```

### Configuration

The service reads from `.workgraph/config.toml`:

```toml
[coordinator]
max_agents = 4         # max parallel agents (default: 4)
poll_interval = 60     # seconds between safety-net ticks (default: 60)
executor = "claude"    # executor for spawned agents (default: "claude")
model = "opus"         # model override for all spawned agents (optional)

[agent]
executor = "claude"
model = "opus"         # default model (default: "opus")
heartbeat_timeout = 5  # minutes before agent is considered dead (default: 5)

[agency]
auto_evaluate = false    # auto-create evaluation tasks on completion
auto_assign = false      # auto-create identity assignment tasks
auto_triage = false      # auto-triage dead agents using LLM
assigner_model = "haiku" # model for assigner agents
evaluator_model = "opus" # model for evaluator agents
evolver_model = "opus"   # model for evolver agents
```

Set config values with:

```bash
wg config --max-agents 8
wg config --model sonnet
wg config --poll-interval 120
wg config --executor shell

# Agency settings
wg config --auto-evaluate true
wg config --auto-assign true
wg config --assigner-model haiku
wg config --evaluator-model opus
wg config --evolver-model opus

# Triage settings
wg config --auto-triage true
wg config --triage-model haiku
```

CLI flags on `wg service start` override config.toml:

```bash
wg service start --max-agents 8 --executor shell --interval 120 --model haiku
```

### Managing the service

| Command | What it does |
|---------|-------------|
| `wg service start` | Start the background daemon |
| `wg service stop` | Stop daemon (agents continue independently) |
| `wg service stop --kill-agents` | Stop daemon and kill all running agents |
| `wg service stop --force` | Immediately SIGKILL the daemon |
| `wg service status` | Show daemon PID, uptime, agent summary, coordinator state |
| `wg service reload` | Re-read config.toml without restarting |
| `wg service pause` | Pause coordinator (running agents continue, no new spawns) |
| `wg service resume` | Resume coordinator (immediate tick) |
| `wg service install` | Generate a systemd user service file |

Reload lets you change settings at runtime:

```bash
wg service reload                              # re-read config.toml
wg service reload --max-agents 8 --model haiku # apply specific overrides
```

### Agent management

List and filter agents:

```bash
wg agents              # all agents
wg agents --alive      # running agents only
wg agents --dead       # dead agents only
wg agents --working    # actively working on a task
wg agents --idle       # waiting for work
wg agents --json       # JSON output for scripting
```

Kill agents:

```bash
wg kill agent-7          # graceful: SIGTERM → wait → SIGKILL
wg kill agent-7 --force  # immediate SIGKILL
wg kill --all            # kill all running agents
```

Killing an agent automatically unclaims its task so another agent can pick it up.

**Dead agent detection:** Agents send heartbeats while working. If an agent's process exits or its heartbeat goes stale (default: 5 minutes), the coordinator marks it dead and unclaims its task. You can also check manually:

```bash
wg dead-agents --check     # check for dead agents (read-only)
wg dead-agents --cleanup   # mark dead and unclaim their tasks
wg dead-agents --remove    # remove dead agents from registry
```

**Smart triage:** When a dead agent is detected, the coordinator can automatically triage the situation using an LLM. Triage reads the agent's output log and decides whether the task was actually completed (mark done), still running (leave alone), or needs to be restarted (re-spawn). Enable it with:

```bash
wg config --auto-triage true
wg config --triage-model haiku      # cheap model is usually sufficient
wg config --triage-timeout 30       # seconds
wg config --triage-max-log-bytes 50000
```

### Model selection

Models are selected in priority order:

1. `--model` flag on `wg spawn` (highest priority)
2. Task's `model` property (set with `wg add --model` or `wg edit --model`)
3. Coordinator config (`coordinator.model` in config.toml)
4. Agent config default (`agent.model` in config.toml)

```bash
# Set model per-task at creation
wg add "Simple fix" --model haiku
wg add "Complex design" --model opus

# Change model on an existing task
wg edit my-task --model sonnet

# Override at spawn time
wg spawn my-task --executor claude --model haiku

# Set coordinator default (applies to all auto-spawned agents)
wg config --model sonnet
wg service reload
```

**Cost tips:** Use **haiku** for simple formatting/linting, **sonnet** for typical coding, **opus** for complex reasoning and architecture.

### The TUI

Launch the interactive terminal dashboard:

```bash
wg tui [--refresh-rate 2000]  # default: 2000ms refresh
```

The TUI has three views:

**Dashboard** — split-pane showing tasks (left) and agents (right) with status bars.

**Graph Explorer** — tree view of the dependency graph with task status and active agent indicators.

**Log Viewer** — real-time tailing of agent output with auto-scroll.

#### Keybindings

**Global:**

| Key | Action |
|-----|--------|
| `q` | Quit |
| `?` | Show help overlay |
| `Esc` | Back / close overlay |

**Dashboard:**

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Switch panel (Tasks ↔ Agents) |
| `j` / `k` or `↑` / `↓` | Scroll up / down |
| `Enter` | Drill into selected item |
| `g` | Open graph explorer |
| `r` | Refresh data |

**Graph Explorer:**

| Key | Action |
|-----|--------|
| `j` / `k` or `↑` / `↓` | Navigate up / down |
| `h` / `l` or `←` / `→` | Collapse / expand subtree |
| `d` | Toggle between tree and graph view |
| `Enter` | View task details or jump to agent log |
| `a` | Cycle to next task with active agents |
| `r` | Refresh graph |

**Log Viewer:**

| Key | Action |
|-----|--------|
| `j` / `k` or `↑` / `↓` | Scroll one line |
| `PageDown` / `PageUp` | Scroll half viewport |
| `g` | Jump to top (disable auto-scroll) |
| `G` | Jump to bottom (enable auto-scroll) |

### Troubleshooting

**Daemon logs:** Check `.workgraph/service/daemon.log` for errors. The daemon logs with timestamps and rotates at 10 MB (keeps one backup at `daemon.log.1`).

```bash
# Recent errors are also shown in status
wg service status
```

**Common issues:**

- **"Socket already exists"** — A previous daemon didn't clean up. Check if it's still running with `wg service status`, then `wg service stop` or manually remove the stale socket.
- **Agents not spawning** — Check `wg service status` for coordinator state. Verify `max_agents` isn't already reached with `wg agents --alive`. Ensure there are tasks in `wg ready`.
- **Agent marked dead prematurely** — Increase `heartbeat_timeout` in config.toml if agents do long-running work without heartbeating.
- **Config changes not taking effect** — Run `wg service reload` after editing `config.toml`. CLI flag overrides on `wg service start` take precedence over the file.
- **Daemon won't start** — Check if another daemon is already running. Look at `.workgraph/service/state.json` for stale PID info.

**State files:** The service stores runtime state in `.workgraph/service/`:

| File | Purpose |
|------|---------|
| `state.json` | Daemon PID, socket path, start time |
| `daemon.log` | Persistent daemon logs |
| `coordinator-state.json` | Effective config and runtime metrics |
| `registry.json` | Agent registry (IDs, PIDs, tasks, status) |

## Agency system

The agency system gives agents composable identities — a **role** (what it does) paired with a **motivation** (why it acts that way). Instead of every spawned agent being a generic assistant, the agency system lets you define specialized agents that are evaluated and evolved over time.

### Quick start

```bash
# Seed built-in starter roles and motivations
wg agency init

# Create an agent pairing
wg agent create "Careful Coder" --role <role-hash> --motivation <motivation-hash>

# Assign the agent identity to a task
wg assign my-task <agent-hash>

# When the service spawns that task, the agent's identity is injected into its prompt
```

### What it does

1. **Roles** define skills and desired outcomes ("Programmer" → working, tested code)
2. **Motivations** define trade-offs and constraints ("Careful" → prioritizes reliability, rejects untested code)
3. **Agents** pair one role + one motivation into a named identity
4. **Assignment** binds an agent to a task — its identity is injected at spawn time
5. **Evaluation** scores completed tasks across four dimensions, with a `source` field distinguishing internal LLM assessments from external signals (CI results, outcome data, peer reviews)
6. **Evolution** uses performance data to create new roles/motivations and retire weak ones

### Automation

Enable auto-assign and auto-evaluate to run the full loop without manual intervention:

```bash
wg config --auto-assign true     # auto-creates assignment tasks for ready work
wg config --auto-evaluate true   # auto-creates evaluation tasks on completion
wg config --assigner-model haiku # cheap model for assignment decisions
wg config --evaluator-model opus # strong model for quality evaluation
wg config --evolver-model opus   # strong model for evolution decisions
```

When the coordinator ticks, it automatically creates `assign-{task}` and `evaluate-{task}` meta-tasks that are dispatched like any other work.

### Evolution

```bash
wg evolve                              # full evolution cycle
wg evolve --strategy mutation --budget 3  # targeted changes
wg evolve --dry-run                    # preview without applying
```

### Federation

Share agency entities across projects:

```bash
wg agency remote add partner /path/to/other/project/.workgraph/agency
wg agency scan partner              # see what they have
wg agency pull partner              # import their roles, motivations, agents
wg agency push partner              # export yours to them
```

Performance records merge during transfer — evaluations are deduplicated and averages recalculated. Content-hash IDs make this natural: the same entity has the same ID everywhere.

See [docs/AGENCY.md](docs/AGENCY.md) for the full agency system documentation.

## Graph locking

Workgraph uses `flock`-based file locking to prevent concurrent modifications when multiple agents or the coordinator are writing to the graph simultaneously. This is automatic — no user action required. The lock is acquired for each write operation and released immediately after.

## The recommended flow

For most projects:

1. **Plan first**: Sketch out the major tasks and dependencies
   ```bash
   wg add "Goal task"
   wg add "Step 1"
   wg add "Step 2" --blocked-by step-1
   wg add "Step 3" --blocked-by step-2
   ```

2. **Check the structure**:
   ```bash
   wg analyze        # health check
   wg critical-path  # what's the longest chain?
   wg bottlenecks    # what should we prioritize?
   ```

3. **Execute**: Start the service and let it dispatch
   ```bash
   wg service start --max-agents 4
   wg tui            # watch progress in the dashboard
   ```

4. **Adapt**: As you learn more, update the graph — the service picks up changes
   ```bash
   wg add "New thing we discovered" --blocked-by whatever
   wg edit stuck-task --add-tag needs-rethink
   wg fail stuck-task --reason "Need to rethink this"
   wg retry stuck-task  # when ready to try again
   ```

5. **Ship**: When `wg ready` is empty and everything important is done, you're there.

## Cycles (repeating workflows)

Some workflows repeat: write → review → revise → write again. Workgraph models these as **structural cycles** — `after` back-edges with a `CycleConfig` that controls iteration limits and behavior. Cycles are detected automatically from the graph structure using Tarjan's SCC algorithm. Use `wg cycles` to inspect detected cycles.

### Creating cycles

```bash
# write → review cycle, max 3 iterations
wg add "Write draft" --id write --after review --max-iterations 3
wg add "Review draft" --after write --id review

# Inspect detected cycles
wg cycles
```

The `--max-iterations` flag sets a `CycleConfig` on the task, making it the **cycle header** — the entry point that controls iteration. The cycle is detected automatically from the `after` edges. Without `--max-iterations`, a cycle in the graph is treated as an unconfigured deadlock (flagged by `wg check`).

Optional cycle configuration:

```bash
# Guard: only iterate if review failed
wg add "Write draft" --id write --after review \
  --max-iterations 5 --cycle-guard "task:review=failed"

# Delay between iterations
wg edit write --cycle-delay "5m"
```

When a cycle completes an iteration (all members reach `done`), the cycle header and all members are reset to `open` with `loop_iteration` incremented.

### Convergence

Any agent working on a cycle member can signal early termination when the work has converged:

```bash
wg done <task-id> --converged   # stops the cycle even if iterations remain
```

The `--converged` flag adds a `"converged"` tag to the **cycle header** (regardless of which member you complete). This stops the cycle from iterating further. Using plain `wg done` allows the next iteration to proceed. Use `--converged` when no more iterations are needed.

### How agents see cycles

When an agent is spawned on a task inside a cycle, it can read `loop_iteration` from `wg show` to know which pass it's on. Previous iterations' logs and artifacts are preserved, so the agent can review what happened before and build on it rather than starting from scratch.

### Inspecting cycles

```bash
wg cycles              # List detected cycles, their status, and iteration counts
wg cycles --json       # Machine-readable cycle information
wg show <task-id>      # Shows cycle membership and current iteration on a task
wg viz                 # Cycle edges appear as dashed lines in graph output
```

### Migrating from loops_to

If you have existing graphs using the old `loops_to` edge system, migrate them:

```bash
wg migrate-loops --dry-run   # Preview what would change
wg migrate-loops             # Convert loops_to edges to structural cycles
```

## Trace & sharing

Workgraph records every operation in a trace log — the project's organizational memory. Use it for introspection, sharing, and workflow reuse.

### Watching events

```bash
wg watch                             # stream events to terminal
wg watch --event task_state          # only task state changes
wg watch --event evaluation          # only evaluations
wg watch --task my-task              # events for a specific task
wg watch --replay 20                 # include 20 most recent historical events
```

The event stream enables external adapters — a CI integration, a Slack bot, or a monitoring tool can observe workgraph events and react without polling.

### Exporting and importing traces

Tasks carry a `visibility` field (`internal`, `public`, or `peer`) that controls what crosses organizational boundaries:

```bash
wg trace export --visibility public   # sanitized for open sharing (structure only)
wg trace export --visibility peer     # richer detail for trusted peers
wg trace import peer-export.json      # import a peer's trace as read-only context
```

### Trace functions (workflow templates)

Extract proven workflows into reusable templates:

```bash
wg trace extract impl-feature --name impl-feature  # extract pattern from completed work
wg trace instantiate impl-feature \
  --input feature_name=auth --input description="Add OAuth"  # create tasks from pattern
wg trace list-functions              # list available templates
wg trace show-function impl-feature  # inspect a template
```

### Trace visualization

```bash
wg trace show <task-id>              # execution history of a task
wg trace show <task-id> --animate    # animated replay of execution over time
```

## Key concepts

**Tasks** have a status (`open`, `in-progress`, `done`, `failed`, `abandoned`, `blocked`) and can block other tasks. Tasks can carry a per-task `model` override, an `agent` identity assignment, and a `visibility` field (`internal`, `public`, `peer`) controlling what information is shared during trace exports.

**Agents** are humans or AIs that do work. They can be AI agents (with a role and motivation that shape their behavior) or human agents (with contact info and a human executor like Matrix or email). All agents share the same identity model: capabilities, trust levels, rate, and capacity.

**The graph** is tasks connected by "blocked-by" relationships. A task is blocked until all its blockers are done. Concurrent writes are protected by flock-based file locking.

**Context flow**: Tasks can declare inputs (what they need) and deliverables (what they produce). Use `wg context <task>` to see what's available.

**Trajectories**: For AI agents, `wg trajectory <task>` suggests the best order to claim related tasks, minimizing context switches.

**Agency**: Composable agent identities (role + motivation) that are assigned to tasks, evaluated after completion, and evolved over time based on performance data.

## Query and analysis

```bash
wg ready              # what can be worked on now?
wg list               # all tasks (--status to filter)
wg show <id>          # full task details
wg status             # quick one-screen overview
wg viz                # ASCII dependency graph (--all to include done)
wg viz --graph        # 2D spatial layout with box-drawing characters

wg why-blocked <id>   # trace the blocker chain
wg impact <id>        # what depends on this?
wg context <id>       # available context from completed dependencies
wg bottlenecks        # tasks blocking the most work
wg critical-path      # longest dependency chain

wg forecast           # project completion estimate
wg velocity           # task completion rate over time
wg aging              # how long tasks have been open
wg workload           # agent assignment distribution
wg structure          # entry points, dead ends, high-impact roots
wg analyze            # comprehensive health report (all of the above)

wg watch              # real-time event stream (for external adapters)
wg trace show <id>    # execution history of a task
wg trace export       # export trace data for sharing
wg trace extract <id> # extract workflow pattern into reusable template
```

See [docs/COMMANDS.md](docs/COMMANDS.md) for the full command reference including `viz`, `plan`, `coordinate`, `archive`, `reschedule`, and more.

## Utilities

```bash
wg log <id> "message"     # add progress notes to a task
wg artifact <id> path     # record a file produced by a task
wg viz --mermaid          # generate DOT/mermaid/ASCII graph
wg viz --graph            # 2D spatial layout with box-drawing characters
wg archive                # archive completed tasks
wg check                  # check graph for cycles and issues
wg trajectory <id>        # optimal task claim order for agents
```

## Storage

Everything lives in `.workgraph/graph.jsonl`. One JSON object per line. Human-readable, git-friendly, easy to hack on.

```jsonl
{"kind":"task","id":"design-api","title":"Design the API","status":"done"}
{"kind":"task","id":"build-backend","title":"Build the backend","status":"open","blocked_by":["design-api"],"model":"sonnet"}
```

Configuration is in `.workgraph/config.toml`:

```toml
[agent]
executor = "claude"
model = "opus"
interval = 10

[coordinator]
max_agents = 4
poll_interval = 60

[agency]
auto_evaluate = false
auto_assign = false

[project]
name = "My Project"
```

Agency data lives in `.workgraph/agency/`, with federation config and trace functions alongside:

```
.workgraph/
  graph.jsonl              # Task graph (operations log / trace)
  config.toml              # Configuration
  federation.yaml          # Named remotes for agency federation
  functions/               # Trace functions (workflow templates)
    <name>.yaml
  agency/
    roles/                 # Role YAML files (keyed by content-hash)
    motivations/           # Motivation YAML files
    agents/                # Agent YAML files (role+motivation pairings)
    evaluations/           # Evaluation records (JSON)
    evolver-skills/        # Strategy-specific skill documents for evolution
```

## More docs

- [docs/COMMANDS.md](docs/COMMANDS.md) - Complete command reference
- [docs/AGENT-GUIDE.md](docs/AGENT-GUIDE.md) - Deep dive on agent operation
- [docs/AGENT-SERVICE.md](docs/AGENT-SERVICE.md) - Service architecture and coordinator lifecycle
- [docs/AGENCY.md](docs/AGENCY.md) - Agency system: roles, motivations, evaluation, evolution

## License

MIT
