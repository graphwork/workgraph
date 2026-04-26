# workgraph

A task graph for getting things done. Works for humans, works for AI agents, works for both at once.

## What is this?

You've got tasks. Some block others. Multiple people (or AIs) need to coordinate without stepping on each other. Workgraph handles that.

```bash
wg init
wg add "Design the API"
wg add "Build the backend" --after design-the-api
wg add "Write tests" --after build-the-backend

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

### 1. Global config (once, after install)

```bash
wg setup    # interactive wizard — executor, model, agency defaults
```

Writes `~/.workgraph/config.toml`. Configures your executor (claude/amplifier), default model, whether to auto-assign agents and auto-evaluate, and which lightweight model to use for assignment/evaluation (haiku recommended).

### 2. Initialize a project

```bash
cd your-project
wg init
```

Creates `.workgraph/` with your task graph. Inherits global config; override per-project with `wg config --local`.

### 3. Add some tasks

```bash
# Simple task
wg add "Set up CI pipeline"

# Task with a blocker
wg add "Deploy to staging" --after set-up-ci-pipeline

# Task with metadata
wg add "Implement auth" \
  --hours 8 \
  --skill rust \
  --skill security \
  --deliverable src/auth.rs

# Task with per-task model override (use provider:model for non-default providers)
wg add "Quick formatting fix" --model haiku
wg add "Use GPT for this" --model openai:gpt-4o

# Execution weight controls what the agent can do
wg add "Quick lint fix" --exec-mode shell       # no LLM, just runs shell command
wg add "Research task" --exec-mode light         # read-only tools
wg add "Full implementation" --exec-mode full    # default: all tools

# Task requiring review before completion
wg add "Security audit" --verify "All findings documented with severity ratings"

# Scheduling: delay or absolute time gate
wg add "Follow-up check" --delay 1h             # becomes ready 1h after deps complete
wg add "Deploy window" --not-before 2026-03-20T09:00:00Z  # ISO 8601

# Placement hints and paused creation
wg add "Related work" --place-near auth-task    # hint: place near related task
wg add "Urgent fix" --place-before deploy-task  # hint: place before this task
wg add "Standalone" --no-place                  # skip automatic placement
wg add "Draft idea" --paused                    # created but not dispatched

# Task with visibility for cross-org sharing
wg add "Public API design" --visibility public

# Control how much context the agent receives at dispatch
wg add "Quick lint fix" --context-scope clean   # minimal: just the task
wg add "Complex refactor" --context-scope full  # everything: full graph + logs
```

### 4. Edit tasks after creation

```bash
wg edit my-task --title "Better title"
wg edit my-task --add-after other-task
wg edit my-task --remove-tag stale --add-tag urgent
wg edit my-task --model opus
wg edit my-task --exec-mode light
wg edit my-task --verify "cargo test passes"
wg edit my-task --delay 30m --not-before 2026-03-20T09:00:00Z
wg edit my-task --add-skill security --remove-skill docs
```

### 5. Register yourself (or your AI agent)

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
  --tradeoff <tradeoff-hash> \
  --capabilities coding,testing,docs
```

### 6. Start working

```bash
# Service mode (recommended) — auto-spawns agents on ready tasks
wg service start

# Or manual mode — claim and work on tasks yourself
wg ready
wg claim set-up-ci-pipeline --actor erik
# ... do the work ...
wg done set-up-ci-pipeline       # unblocks deploy-to-staging
```

### 7. Verification workflow

Tasks created with `--verify` go through a validation gate before completion. When an agent calls `wg done`, the task transitions to `PendingValidation` instead of `Done`:

```bash
# Create a task that needs review
wg add "Security audit" --verify "All findings documented with severity ratings"

# Agent works on it, then marks it done — status becomes PendingValidation
wg done security-audit

# Reviewer approves or rejects
wg approve security-audit                          # transitions to Done
wg reject security-audit --reason "Missing CVE references"  # reopens for rework
```

Rejected tasks reopen for the agent to address feedback. After too many rejections (default: 3), the task is failed automatically. See [docs/COMMANDS.md](docs/COMMANDS.md) for full details.

## Using with AI Coding Assistants

Workgraph includes a skill definition that teaches AI assistants to use the service as a coordinator.

### Claude Code

Install the skill from the workgraph directory:

```bash
wg skill install           # installs to ~/.claude/skills/ (all your projects)
```

You can also discover and inspect available skills:

```bash
wg skill list              # list available skills
wg skill find <query>      # search skills by keyword
wg skill task <task-id>    # show skills relevant to a task
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
- `wg service start` to start the dispatcher
- `wg add "Task" --after dep` to define work
- `wg list` / `wg agents` to monitor progress

The service automatically spawns agents and claims tasks.
See `wg --help` for all commands.
```

### What the skill teaches

The skill teaches agents to:
- Run `wg quickstart` at session start to orient themselves
- Act as a dispatcher: start the service, define tasks, monitor progress
- Let the service handle claiming and spawning — not do it manually
- Use manual mode only as a fallback when working alone without the service

## Agentic workflows

### Pattern 1: Service mode (recommended)

Start the service and let it handle everything:

```bash
# Define the work
wg add "Refactor auth module" --skill rust
wg add "Update tests" --after refactor-auth-module --skill testing
wg add "Update docs" --after refactor-auth-module --skill docs

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
2. Create tasks with `wg add`, linking dependencies with --after
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
wg service status    # daemon info, agent summary, dispatcher state
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
[dispatcher]           # legacy alias [coordinator] still accepted
max_agents = 4         # max parallel agents (default: 4)
poll_interval = 60     # seconds between safety-net ticks (default: 60)
executor = "claude"    # executor: "claude" (default), "amplifier", or "shell"
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
evaluator_model = "haiku" # model for evaluator agents
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
wg config --auto-place true           # automatic task placement
wg config --auto-create true          # automatic task creation
wg config --assigner-model haiku
wg config --evaluator-model opus
wg config --evolver-model opus

# Creator tracking (recorded on tasks created by the dispatcher)
wg config --creator-agent <agent-hash>
wg config --creator-model opus

# Triage settings
wg config --auto-triage true
wg config --triage-model haiku

# Eval gate and FLIP settings
wg config --eval-gate-threshold 0.7
wg config --flip-enabled true

# Model registry and routing
wg config --registry                  # show model registry
wg config --registry-add --id my-model --provider openrouter --reg-model my-model --reg-tier standard  # add to registry
wg config --set-model default sonnet  # set default dispatch model
wg config --set-model evaluator opus  # per-role model routing

# Multi-chat
wg config --max-coordinators 3

# Inspect merged config (shows source: global, local, or default)
wg config --list
wg config --global          # show/set global config only (~/.workgraph/config.toml)
wg config --local           # show/set project config only (.workgraph/config.toml)
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
| `wg service status` | Show daemon PID, uptime, agent summary, dispatcher state |
| `wg service reload` | Re-read config.toml without restarting |
| `wg service restart` | Graceful stop then start |
| `wg service pause` | Pause dispatcher (running agents continue, no new spawns) |
| `wg service resume` | Resume coordinator (immediate tick) |
| `wg service freeze` | SIGSTOP all running agents and pause coordinator |
| `wg service thaw` | SIGCONT all frozen agents and resume coordinator |
| `wg service install` | Generate a systemd user service file |
| `wg service tick` | Run a single coordinator tick (debug) |
| `wg service create-coordinator` | Create a new coordinator session |
| `wg service stop-coordinator` | Stop a running coordinator session |
| `wg service archive-coordinator` | Archive a coordinator session |
| `wg service delete-coordinator` | Delete a coordinator session |
| `wg service interrupt-coordinator` | Interrupt a coordinator's current generation |

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
wg dead-agents             # check for dead agents (read-only, default)
wg dead-agents --cleanup   # mark dead and unclaim their tasks
wg dead-agents --remove    # remove dead agents from registry
wg dead-agents --purge     # remove all dead agents and clean up
wg dead-agents --delete-dirs  # also delete agent working directories
wg dead-agents --threshold 10   # custom staleness threshold (minutes)
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

1. Task's `model` property (set with `wg add --model` or `wg edit --model`) — highest priority
2. Executor config model (model field in the executor's config file)
3. `coordinator.model` in config.toml (or `--model` on `wg spawn` / `wg service start`)
4. Executor default (if no model is resolved, no `--model` flag is passed)

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

**Alternative providers:** Workgraph supports [OpenRouter](https://openrouter.ai/) and any OpenAI-compatible API. Configure an endpoint with `wg endpoints add` and use full model IDs like `deepseek/deepseek-chat-v3`. See [docs/guides/openrouter-setup.md](docs/guides/openrouter-setup.md) for details.

### Model registry

Manage the model registry and per-role routing:

```bash
wg model list                      # show all models (built-in + user-defined)
wg model add my-model --provider openrouter --model-id deepseek/deepseek-chat-v3
wg model remove my-model
wg model set-default sonnet        # set default dispatch model
wg model routing                   # show per-role model routing
wg model set --role evaluator opus # set model for a specific dispatch role
```

### API key management

```bash
wg key set anthropic               # configure a provider's API key
wg key check                       # validate key availability
wg key list                        # show key status for all providers
```

### The TUI

Launch the interactive terminal dashboard:

```bash
wg tui [--refresh-rate 2000]  # default: 2000ms refresh
```

The TUI has three main views plus a rich inspector panel:

**Dashboard** — split-pane showing tasks (left) and agents (right) with status bars.

**Graph Explorer** — tree view of the dependency graph with task status and active agent indicators. Touch drag-to-pan is supported for mobile terminals (Termux).

**Log Viewer** — real-time tailing of agent output with auto-scroll.

**Inspector panel** — nine tabbed views accessible via `Alt+Left`/`Alt+Right` (with slide animation): Chat, Detail, Log, Messages, Agency, Config, Files, Coordinator Log, and Firehose. The Firehose tab is a combined live stream of all agent activity. Resize the inspector with `i` (cycle through 1/3 → 1/2 → 2/3 → full) and `I` (shrink back).

**Status bar features:**
- Service health badge — colored dot (green/yellow/red) with tap-to-inspect showing service state, stuck tasks, and control actions
- Token display — shows novel vs cached input split per task
- Lifecycle indicators — Unicode symbols for agency phases (⊳ assigning, ∴ evaluating, validating, verifying) rendered in pink
- Markdown rendering with syntax highlighting (pulldown-cmark + syntect) in detail views

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
| `/` | Open search |
| `n` / `N` | Next / previous match |
| `Tab` / `Shift+Tab` | Next / previous match (in search mode) |
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
- **Agents not spawning** — Check `wg service status` for dispatcher state. Verify `max_agents` isn't already reached with `wg agents --alive`. Ensure there are tasks in `wg ready`.
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

The agency system gives agents composable identities — a **role** (what it does) paired with a **tradeoff** (why it acts that way). Instead of every spawned agent being a generic assistant, the agency system lets you define specialized agents that are evaluated and evolved over time.

### Quick start

```bash
# Seed built-in starter roles and tradeoffs
wg agency init

# Create an agent pairing
wg agent create "Careful Coder" --role <role-hash> --tradeoff <tradeoff-hash>

# Assign the agent identity to a task
wg assign my-task <agent-hash>

# When the service spawns that task, the agent's identity is injected into its prompt
```

### What it does

1. **Roles** define skills and desired outcomes ("Programmer" → working, tested code)
2. **Tradeoffs** define trade-offs and constraints ("Careful" → prioritizes reliability, rejects untested code)
3. **Agents** pair one role + one tradeoff into a named identity
4. **Assignment** binds an agent to a task — its identity is injected at spawn time
5. **Evaluation** scores completed tasks across four dimensions:
   - `wg evaluate run <task>` — trigger LLM-based evaluation
   - `wg evaluate record --task <id> --score <n> --source <tag>` — record external signals (CI, peer review)
   - `wg evaluate show` — view evaluation history
6. **Evolution** uses performance data to create new roles/tradeoffs and retire weak ones

### FLIP pipeline

FLIP (Fidelity via Latent Intent Probing) is an independent second-opinion scoring system. After a task completes, an LLM reconstructs what the task must have been from only the agent's output, then a comparison scores how well the output matched the actual task description. Low FLIP scores (below threshold) automatically trigger verification tasks where a stronger model independently checks the work.

The full agency loop: **eval → FLIP → verify → evolve**. Evaluation grades quality, FLIP grades fidelity, verification catches low-confidence results, and evolution uses performance data to improve agent identities.

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
wg evolve run                              # full evolution cycle
wg evolve run --strategy mutation --budget 3  # targeted changes
wg evolve run --dry-run                    # preview without applying
```

### Federation

Share agency entities across projects:

```bash
wg agency remote add partner /path/to/other/project/.workgraph/agency
wg agency scan partner              # see what they have
wg agency pull partner              # import their roles, tradeoffs, agents
wg agency push partner              # export yours to them
```

Performance records merge during transfer — evaluations are deduplicated and averages recalculated. Content-hash IDs make this natural: the same entity has the same ID everywhere.

### Peer workgraphs

For cross-repo task coordination (separate from agency federation):

```bash
wg peer add partner /path/to/other/project
wg peer list                        # list configured peers with status
wg peer status                      # quick health check of all peers
```

See [docs/AGENCY.md](docs/AGENCY.md) for the full agency system documentation.

## Communication

Agents and humans can exchange messages on tasks using `wg msg`:

```bash
# Send a message to a task (any agent working on it will see it)
wg msg send my-task "The API schema changed — use v2 endpoints"

# Read messages as an agent
wg msg read my-task --agent $WG_AGENT_ID
```

For interactive conversation with the coordinator agent, use `wg chat`:

```bash
wg chat "What's the status of the auth refactor?"
wg chat -i                                       # interactive REPL mode
wg chat "Here's the spec" --attachment spec.pdf   # attach a file
wg chat --coordinator 2 "Status?"                 # target a specific coordinator
wg chat --history                                 # show chat history
wg chat --clear                                   # clear chat history
```

See [docs/COMMANDS.md](docs/COMMANDS.md) for full messaging options.

## Agent isolation

When the service spawns multiple agents concurrently, each agent operates in its own [git worktree](https://git-scm.com/docs/git-worktree) to avoid file conflicts. Each worktree has an independent working tree and index while sharing the same repository, so agents can build, test, and commit without interfering with each other.

See [docs/WORKTREE-ISOLATION.md](docs/WORKTREE-ISOLATION.md) for the full design and implementation details.

## Graph locking

Workgraph uses `flock`-based file locking to prevent concurrent modifications when multiple agents or the coordinator are writing to the graph simultaneously. This is automatic — no user action required. The lock is acquired for each write operation and released immediately after.

## The recommended flow

For most projects:

1. **Plan first**: Sketch out the major tasks and dependencies
   ```bash
   wg add "Goal task"
   wg add "Step 1"
   wg add "Step 2" --after step-1
   wg add "Step 3" --after step-2
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
   wg add "New thing we discovered" --after whatever
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

# Force all iterations (agents cannot signal --converged)
wg edit write --no-converge

# Control failure behavior
wg edit write --no-restart-on-failure            # don't restart cycle on failure
wg edit write --max-failure-restarts 5           # cap failure-triggered restarts
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

### Functions (workflow templates)

Extract proven workflows into reusable templates. Three layers of increasing sophistication:

- **Static** (version 1): Fixed task topology with `{{input.X}}` substitution
- **Generative** (version 2): A planning node decides the task graph at apply time, within structural constraints
- **Adaptive** (version 3): Generative + trace memory from past runs, so the planner learns over time

```bash
# Extract a static function from completed work
wg func extract impl-auth --name impl-feature --subgraph

# Extract a generative function by comparing multiple traces
wg func extract impl-auth impl-caching impl-logging \
  --generative --name impl-feature

# Apply a function (creates tasks from template)
wg func apply impl-feature \
  --input feature_name=auth --input description="Add OAuth"

# Upgrade to adaptive (adds learning from past runs)
wg func make-adaptive impl-feature

# Bootstrap the meta-function (extraction as a workflow)
wg func bootstrap

# List and inspect functions
wg func list              # list available templates
wg func show impl-feature  # inspect a template
```

### Trace visualization

```bash
wg trace show <task-id>              # execution history of a task
wg trace show <task-id> --animate    # animated replay of execution over time
```

## Key concepts

**Tasks** have a status (`open`, `in-progress`, `done`, `failed`, `abandoned`, `blocked`, `pending-validation`, `waiting`) and can block other tasks. Tasks can carry a per-task `model` override (with optional `provider`), an `agent` identity assignment, a `visibility` field (`internal`, `public`, `peer`) controlling what information is shared during trace exports, a `context_scope` (`clean`, `task`, `graph`, `full`) controlling how much context the agent receives at dispatch, and an `exec_mode` (`full`, `light`, `bare`, `shell`) controlling the agent's tool access.

**Agents** are humans or AIs that do work. They can be AI agents (with a role and tradeoff that shape their behavior) or human agents (with contact info and a human executor like Matrix or email). All agents share the same identity model: capabilities, trust levels, rate, and capacity.

**The graph** is tasks connected by dependency edges (the `after` field). A task is waiting until all its dependencies reach a terminal status. Concurrent writes are protected by flock-based file locking.

**Context flow**: Tasks can declare inputs (what they need) and deliverables (what they produce). Use `wg context <task>` to see what's available.

**Trajectories**: For AI agents, `wg trajectory <task>` suggests the best order to claim related tasks, minimizing context switches.

**Agency**: Composable agent identities (role + tradeoff) that are assigned to tasks, evaluated after completion, and evolved over time based on performance data.

## Query and analysis

```bash
wg ready              # what can be worked on now?
wg list               # all tasks (--status to filter)
wg show <id>          # full task details
wg status             # quick one-screen overview
wg viz                # ASCII dependency graph (--all to include done)
wg viz --graph        # 2D spatial layout with box-drawing characters
wg viz task-a task-b  # focus on subgraphs containing specific tasks
wg viz --show-internal # include assign-*/evaluate-* meta-tasks
wg viz --no-tui       # force static output (skip interactive TUI)

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
```

See [docs/COMMANDS.md](docs/COMMANDS.md) for the full command reference including `viz`, `plan`, `coordinate`, `archive`, `reschedule`, and more.

## Utilities

```bash
wg log <id> "message"     # add progress notes to a task
wg artifact <id> path     # record a file produced by a task
wg compact                # distill graph state into context.md
wg sweep                  # detect and recover orphaned in-progress tasks
wg checkpoint <id> -s "progress summary"  # save checkpoint for long tasks
wg stats                  # show time counters and agent statistics
wg exec <id>              # execute a task's shell command (claim + run + done/fail)
wg model list             # model registry management (see Model registry section)
wg key list               # API key status (see API key management section)
wg viz --mermaid          # generate Mermaid flowchart output
wg viz --graph            # 2D spatial layout with box-drawing characters
wg archive                # archive completed tasks
wg screencast             # render TUI event traces into asciinema screencasts
wg server                 # multi-user server setup automation
wg tui-dump               # dump current TUI screen contents (requires running tui)
wg check                  # check graph for cycles and issues
wg trajectory <id>        # optimal task claim order for agents
wg runs list              # list run snapshots
wg runs diff <snapshot>   # diff current graph against a snapshot
wg runs restore <snapshot> # restore graph from a snapshot
wg replay --failed-only   # re-execute failed tasks (optionally with --model)
wg replay --below-score 0.5  # re-execute poorly-scored tasks
wg replay --subgraph task-id # replay a specific subgraph
wg replay --keep-done        # don't reset done tasks when replaying
```

## Storage

Everything lives in `.workgraph/graph.jsonl`. One JSON object per line. Human-readable, git-friendly, easy to hack on.

```jsonl
{"kind":"task","id":"design-api","title":"Design the API","status":"done"}
{"kind":"task","id":"build-backend","title":"Build the backend","status":"open","after":["design-api"],"model":"sonnet"}
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

See [Service > Configuration](#configuration) for the full set of options including agency automation, FLIP, eval gates, model routing, and multi-coordinator settings.

Agency data lives in `.workgraph/agency/`, with federation config and functions alongside:

```
.workgraph/
  graph.jsonl              # Task graph (operations log / trace)
  config.toml              # Configuration
  federation.yaml          # Named remotes for agency federation
  functions/               # Trace functions (workflow templates)
    <name>.yaml
  agency/
    primitives/
      components/          # Skill components (atomic capabilities)
      outcomes/            # Desired outcomes
      tradeoffs/           # Tradeoff definitions
    cache/
      roles/               # Composed roles (component_ids + outcome_id)
      agents/              # Agent definitions (role + tradeoff pairs)
    assignments/           # Task-to-agent assignment records
    evaluations/           # Evaluation records (JSON)
    org-evaluations/       # Organization-level evaluation records
    evolution_runs/        # Evolution run history
    evolver-skills/        # Strategy-specific guidance documents
    coordinator-prompt/    # Coordinator prompt files
    deferred/              # Deferred evolution operations
    creator_state.json     # Creator agent state
```

## Terminal-Bench evaluation

We evaluated workgraph's impact on agent performance using [Terminal-Bench 2.0](https://terminal-bench.org), a benchmark of 89 real-world terminal tasks with binary pass/fail verification.

**Model:** Minimax M2.7 via OpenRouter | **Tasks:** 89 | **Trials:** 3 per condition

| Condition | Description | Pass Rate | 95% CI |
|-----------|-------------|-----------|--------|
| **A** (control) | Bare agent: bash + file tools | 52.3% | [43.4, 61.6] |
| **B** (stigmergic) | A + workgraph tools + graph context | 51.4% | [42.0, 60.4] |
| **C** (enhanced) | B + skill injection + planning | 49.0% | [39.4, 58.2] |

**Result:** No statistically significant difference between conditions (all pairwise p > 0.3). Workgraph showed modest gains on medium-difficulty tasks (+9pp) offset by losses on easy tasks (-16pp) where bookkeeping overhead introduced friction. Hard tasks remained beyond the model's reach regardless of scaffolding.

Full analysis: [`terminal-bench/results/analysis.md`](terminal-bench/results/analysis.md) | Blog post: [`terminal-bench/BLOG.md`](terminal-bench/BLOG.md)

To reproduce:
```bash
# Install the adapter
pip install -e terminal-bench/

# Run all conditions (requires OPENROUTER_API_KEY)
bash terminal-bench/reproduce.sh

# Analyze results
python3 terminal-bench/results/analyze.py
```

## Testing

Run the wave-1 integration smoke test after any wave-1 task lands.

**This MUST be run live against real endpoints — no stubs, no mocks, no
special bypass.** The earlier version of this smoke silently passed because
it relied on a fake LLM and ran the daemon with `--no-coordinator-agent`,
which is exactly how the `wg nex` 404 reached the user on the first 'hi' in
TUI chat. Live scenarios cover the user's literal reproduction:

```bash
# Full suite — runs scenarios 1-7 (offline + live)
bash scripts/smoke/wave-1-smoke.sh

# Skip slow daemon/TUI scenarios (and the live ones)
bash scripts/smoke/wave-1-smoke.sh --quick

# Skip live scenarios (6, 7) but keep offline ones — for sandboxed CI
bash scripts/smoke/wave-1-smoke.sh --offline
```

If a live endpoint is unreachable, scenario 6/7 print a LOUD banner —
`*** NEX SMOKE SKIPPED — endpoint unreachable ***` — that is greppable in
output and impossible to miss. Set `WG_SMOKE_FAIL_ON_SKIP=1` to promote
loud skips to fail in CI. Set `WG_SMOKE_KEEP_SCRATCH=1` to preserve the
per-scenario scratch dirs for post-mortem inspection.

Live scenarios point at `https://lambda01.tail334fe6.ts.net:30000` with
model `qwen3-coder` by default; override via `WG_LIVE_NEX_ENDPOINT` and
`WG_LIVE_NEX_MODEL`.

## More docs

- [docs/COMMANDS.md](docs/COMMANDS.md) - Complete command reference
- [docs/AGENT-GUIDE.md](docs/AGENT-GUIDE.md) - Deep dive on agent operation
- [docs/AGENT-SERVICE.md](docs/AGENT-SERVICE.md) - Service architecture and coordinator lifecycle
- [docs/AGENCY.md](docs/AGENCY.md) - Agency system: roles, tradeoffs, evaluation, evolution
- [docs/LOGGING.md](docs/LOGGING.md) - Provenance logging and the operations log
- [docs/DEV.md](docs/DEV.md) - Developer notes
- [docs/KEY_DOCS.md](docs/KEY_DOCS.md) - Documentation inventory and status

## License

MIT
