# Workgraph Command Reference

Complete reference for all `wg` commands. All commands support `--json` for machine-readable output and `--dir <path>` to specify a custom workgraph directory.

## Table of Contents

- [Task Management](#task-management)
- [Query Commands](#query-commands)
- [Analysis Commands](#analysis-commands)
- [Agent and Resource Management](#agent-and-resource-management)
- [Agency Commands](#agency-commands)
- [Agent Commands](#agent-commands)
- [Service Commands](#service-commands)
- [Utility Commands](#utility-commands)

---

## Task Management

### `wg add`

Add a new task to the graph.

```bash
wg add <TITLE> [OPTIONS]
```

**Arguments:**
- `TITLE` - Task title (required)

**Options:**
| Option | Description |
|--------|-------------|
| `--id <ID>` | Custom task ID (auto-generated from title if not provided) |
| `-d, --description <TEXT>` | Detailed description, acceptance criteria |
| `--blocked-by <ID>` | Add dependency on another task (repeatable, comma-separated) |
| `--assign <AGENT>` | Assign to an agent |
| `--hours <N>` | Estimated hours |
| `--cost <N>` | Estimated cost |
| `-t, --tag <TAG>` | Add tag (repeatable) |
| `--skill <SKILL>` | Required skill (repeatable) |
| `--input <PATH>` | Input file/context needed (repeatable) |
| `--deliverable <PATH>` | Expected output (repeatable) |
| `--max-retries <N>` | Maximum retry attempts |
| `--model <MODEL>` | Preferred model for this task (haiku, sonnet, opus) |
| `--verify <CRITERIA>` | Verification criteria — task requires review before done |
| `--max-iterations <N>` | Maximum cycle iterations — sets `CycleConfig` on this task, making it a cycle header |
| `--cycle-guard <EXPR>` | Guard condition for cycle iteration: `task:<id>=<status>` or `always` |
| `--cycle-delay <DUR>` | Delay between cycle iterations (e.g., `30s`, `5m`, `1h`) |

**Examples:**

```bash
# Simple task
wg add "Fix login bug"

# Task with dependencies and metadata
wg add "Implement user auth" \
  --id user-auth \
  --blocked-by design-api \
  --hours 8 \
  --skill rust \
  --skill security \
  --deliverable src/auth.rs

# Task with model override
wg add "Quick formatting fix" --model haiku

# Task requiring review
wg add "Security audit" --verify "All findings documented with severity ratings"

# Cycle header — creates a structural cycle with review
wg add "Write draft" --id write --after review --max-iterations 3
wg add "Review draft" --after write --id review

# Cycle header with guard and delay
wg add "Write" --after review --max-iterations 5 \
  --cycle-guard "task:review=failed" --cycle-delay "5m"
```

---

### `wg edit`

Modify an existing task's fields without replacing it.

```bash
wg edit <ID> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--title <TEXT>` | Update task title |
| `-d, --description <TEXT>` | Update task description |
| `--add-blocked-by <ID>` | Add a blocked-by dependency (repeatable) |
| `--remove-blocked-by <ID>` | Remove a blocked-by dependency (repeatable) |
| `--add-tag <TAG>` | Add a tag (repeatable) |
| `--remove-tag <TAG>` | Remove a tag (repeatable) |
| `--add-skill <SKILL>` | Add a required skill (repeatable) |
| `--remove-skill <SKILL>` | Remove a required skill (repeatable) |
| `--model <MODEL>` | Update preferred model |
| `--max-iterations <N>` | Set maximum cycle iterations (creates or updates `CycleConfig`) |
| `--cycle-guard <EXPR>` | Set guard condition for cycle iteration |
| `--cycle-delay <DUR>` | Set delay between cycle iterations |

Triggers a `graph_changed` IPC notification to the service daemon, so the coordinator picks up changes immediately.

**Examples:**

```bash
# Change title
wg edit my-task --title "Better title"

# Add a dependency
wg edit my-task --add-blocked-by other-task

# Swap tags
wg edit my-task --remove-tag stale --add-tag urgent

# Change model
wg edit my-task --model opus

# Set cycle configuration (makes this task a cycle header)
wg edit my-task --max-iterations 5
wg edit my-task --cycle-guard "task:review=failed"
wg edit my-task --cycle-delay "5m"
```

---

### `wg done`

Mark a task as completed.

```bash
wg done <ID> [--converged]
```

Sets status to `done`, records `completed_at` timestamp, and unblocks dependent tasks. If the task is part of a structural cycle, completing the last member triggers cycle iteration (re-opening all members for the next pass).

**Options:**
| Option | Description |
|--------|-------------|
| `--converged` | Stop the cycle — adds a `"converged"` tag to the cycle header, preventing further iterations even if `max_iterations` hasn't been reached |

**Examples:**
```bash
wg done design-api
# Automatically unblocks tasks that were waiting on design-api

# In a cycle: allow next iteration
wg done review-task

# In a cycle: signal convergence (stops the cycle)
wg done review-task --converged
```

---

### `wg submit`

Submit a verified task for review.

```bash
wg submit <ID> [--actor <ACTOR>]
```

**DEPRECATED**: Behaves like `wg done`. Use `wg done` instead.

---

### `wg approve`

**DEPRECATED**: Behaves like `wg done`. Use `wg done` instead.

```bash
wg approve <ID> [--actor <ACTOR>]
```

---

### `wg reject`

**DEPRECATED**: Sends a done/in-progress task back to open for rework.

```bash
wg reject <ID> [--reason <TEXT>] [--actor <ACTOR>]
```

---

### `wg fail`

Mark a task as failed (can be retried later).

```bash
wg fail <ID> [--reason <TEXT>]
```

**Example:**
```bash
wg fail deploy-prod --reason "AWS credentials expired"
```

---

### `wg abandon`

Mark a task as abandoned (will not be completed).

```bash
wg abandon <ID> [--reason <TEXT>]
```

Abandoned is a terminal state — the task will not be retried.

**Example:**
```bash
wg abandon legacy-migration --reason "Feature deprecated"
```

---

### `wg retry`

Reset a failed task back to open status for another attempt.

```bash
wg retry <ID>
```

Increments the retry counter and sets status back to `open`.

**Example:**
```bash
wg retry deploy-prod
# Resets deploy-prod to open status with incremented retry count
```

---

### `wg claim`

Claim a task for work (sets status to in-progress).

```bash
wg claim <ID> [--actor <ACTOR>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--actor <ACTOR>` | Who is claiming the task (recorded in logs) |

Claiming sets `started_at` timestamp and assigns the task. Prevents double-work in multi-agent scenarios.

**Example:**
```bash
wg claim implement-api --actor claude
```

---

### `wg unclaim`

Release a claimed task back to open status.

```bash
wg unclaim <ID>
```

**Example:**
```bash
wg unclaim implement-api
# Returns the task to open status so another agent can pick it up
```

---

### `wg reclaim`

Reclaim a task from a dead/unresponsive agent.

```bash
wg reclaim <ID> --from <ACTOR> --to <ACTOR>
```

**Options:**
| Option | Description |
|--------|-------------|
| `--from <ACTOR>` | The agent currently holding the task (required) |
| `--to <ACTOR>` | The new agent to assign the task to (required) |

**Example:**
```bash
wg reclaim implement-api --from agent-1 --to agent-2
```

---

### `wg log`

Add progress notes to a task or view existing logs.

```bash
# Add a log entry
wg log <ID> <MESSAGE> [--actor <ACTOR>]

# View log entries
wg log <ID> --list
```

**Examples:**
```bash
wg log implement-api "Completed endpoint handlers" --actor erik
wg log implement-api --list
```

---

### `wg assign`

Assign an agent identity to a task (or clear the assignment).

```bash
wg assign <TASK> <AGENT-HASH>    # Assign agent to task
wg assign <TASK> --clear         # Remove assignment
```

When the service spawns that task, the agent's role and motivation are injected into the prompt. The agent hash can be a prefix (minimum 4 characters).

**Example:**
```bash
wg assign my-task a3f7c21d
wg assign my-task --clear
```

---

### `wg show`

Display detailed information about a single task.

```bash
wg show <ID>
```

Shows all task fields including description, logs, timestamps, dependencies, model, and agent assignment.

---

## Query Commands

### `wg list`

List all tasks in the graph.

```bash
wg list [--status <STATUS>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--status <STATUS>` | Filter by status (open, in-progress, done, failed, abandoned) |

---

### `wg ready`

List tasks ready to work on (no incomplete blockers).

```bash
wg ready
```

Shows only open tasks where all dependencies are done and any `not_before` timestamp has passed.

**Example:**
```bash
wg ready
# Shows tasks you can start working on right now
```

---

### `wg blocked`

Show direct blockers of a task.

```bash
wg blocked <ID>
```

**Example:**
```bash
wg blocked deploy-prod
# Lists the immediate dependencies preventing deploy-prod from being ready
```

---

### `wg why-blocked`

Show the full transitive chain explaining why a task is blocked.

```bash
wg why-blocked <ID>
```

Traces through the entire dependency graph to show the root cause of a blocked task.

**Example:**
```bash
wg why-blocked deploy-prod
# Shows: deploy-prod ← run-tests ← fix-auth-bug (in-progress)
```

---

### `wg impact`

Show what tasks depend on a given task (forward analysis).

```bash
wg impact <ID>
```

**Example:**
```bash
wg impact design-api
# Shows all downstream tasks that will be unblocked when design-api completes
```

---

### `wg context`

Show available context for a task from its completed dependencies.

```bash
wg context <TASK> [--dependents]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--dependents` | Also show tasks that will consume this task's outputs |

**Example:**
```bash
wg context implement-api
# Shows artifacts and logs from completed dependencies

wg context implement-api --dependents
# Also shows what downstream tasks expect from this task
```

---

### `wg status`

Quick one-screen status overview of the project.

```bash
wg status
```

**Example:**
```bash
wg status
# Shows task counts by status, recent activity, and overall progress
```

---

## Analysis Commands

### `wg bottlenecks`

Find tasks blocking the most downstream work.

```bash
wg bottlenecks
```

**Example:**
```bash
wg bottlenecks
# Shows tasks ranked by how many downstream tasks they block
```

---

### `wg critical-path`

Show the longest dependency chain (determines minimum project duration).

```bash
wg critical-path
```

**Example:**
```bash
wg critical-path
# Shows the chain of tasks that determines the earliest possible completion
```

---

### `wg forecast`

Estimate project completion based on velocity and remaining work.

```bash
wg forecast
```

**Example:**
```bash
wg forecast
# Projects completion date based on recent task throughput
```

---

### `wg velocity`

Show task completion velocity over time.

```bash
wg velocity [--weeks <N>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--weeks <N>` | Number of weeks to show (default: 4) |

**Example:**
```bash
wg velocity --weeks 8
```

---

### `wg aging`

Show task age distribution — how long tasks have been open.

```bash
wg aging
```

**Example:**
```bash
wg aging
# Shows histogram of task ages to identify stale work
```

---

### `wg structure`

Analyze graph structure — entry points, dead ends, high-impact roots.

```bash
wg structure
```

**Example:**
```bash
wg structure
# Reports orphan tasks, entry points, leaf nodes, and connectivity
```

---

### `wg cycles`

Detect and display structural cycles in the task graph.

```bash
wg cycles [--json]
```

Uses Tarjan's SCC algorithm to find cycles formed by `after` edges. Shows cycle members, header, iteration status, and configuration.

**Example output:**

```
Detected cycles: 2

  1. write → review → write  [ACTIVE]
     Header: write (iteration 1/5)
     Guard: task:review=failed
     Delay: 5m
     Status: review is in-progress

  2. spec → implement → test → spec  [CONVERGED]
     Header: spec (iteration 2/3, converged)
     Guard: none (always)
     Status: all done

Summary: 1 active, 1 converged
```

**Options:**
| Option | Description |
|--------|-------------|
| `--json` | Output cycle data as JSON |

---

### `wg loops`

**DEPRECATED**: Redirects to `wg cycles` with a deprecation warning. Use `wg cycles` instead.

```bash
wg loops    # prints deprecation warning and runs wg cycles
```

---

### `wg migrate-loops`

Migrate legacy `loops_to` edges to structural cycles (`after` edges + `CycleConfig`).

```bash
wg migrate-loops [--dry-run]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--dry-run` | Show what would be migrated without making changes |

For each `loops_to` edge on a source task targeting a target task:
1. Adds the source task to the target's `after` list (creating the back-edge)
2. Sets `CycleConfig` on the target task (preserving `max_iterations`, `guard`, and `delay`)
3. Removes the `loops_to` edge from the source

**Example:**
```bash
wg migrate-loops --dry-run
# Found 2 loops_to edge(s) to migrate:
#   review --loops_to--> write (max: 3, guard: None, delay: None)
#   test --loops_to--> spec (max: 5, guard: Always, delay: Some("5m"))
# Dry run — no changes made.

wg migrate-loops
# Migrated 2 loops_to edge(s) to structural cycles.
```

---

### `wg workload`

Show agent workload balance and assignment distribution.

```bash
wg workload
```

**Example:**
```bash
wg workload
# Shows task counts and hours per agent
```

---

### `wg analyze`

Comprehensive health report combining all analyses.

```bash
wg analyze
```

Runs bottlenecks, structure, cycles, aging, and other analyses together.

**Example:**
```bash
wg analyze
# Full project health report in one command
```

---

### `wg cost`

Calculate total cost of a task including all dependencies.

```bash
wg cost <ID>
```

**Example:**
```bash
wg cost deploy-prod
# Shows total cost including all transitive dependency costs
```

---

### `wg plan`

Plan what can be accomplished with given resources.

```bash
wg plan [--budget <N>] [--hours <N>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--budget <N>` | Available budget in dollars |
| `--hours <N>` | Available work hours |

**Example:**
```bash
wg plan --budget 5000 --hours 40
# Shows which tasks fit within the given constraints
```

---

### `wg coordinate`

Show ready tasks for parallel execution dispatch.

```bash
wg coordinate [--max-parallel <N>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--max-parallel <N>` | Maximum number of parallel tasks to show |

**Example:**
```bash
wg coordinate --max-parallel 3
# Shows up to 3 tasks that can be worked on simultaneously
```

---

---

## Agent and Resource Management

Agent creation is covered in the [Agency Commands](#agency-commands) section under `wg agent create`.

---

### `wg resource add`

Add a new resource.

```bash
wg resource add <ID> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--name <NAME>` | Display name |
| `--type <TYPE>` | Resource type (money, compute, time, etc.) |
| `--available <N>` | Available amount |
| `--unit <UNIT>` | Unit (usd, hours, gpu-hours, etc.) |

**Example:**
```bash
wg resource add gpu-cluster --name "GPU Cluster" --type compute --available 4 --unit gpu-hours
```

---

### `wg resource list`

List all resources.

```bash
wg resource list
```

---

### `wg resources`

Show resource utilization (committed vs available).

```bash
wg resources
```

**Example:**
```bash
wg resources
# Shows resource usage summary: committed vs available capacity
```

---

### `wg skill`

List and find skills across tasks.

```bash
wg skill list           # list all skills in use
wg skill task <ID>      # show skills for a specific task
wg skill find <SKILL>   # find tasks requiring a specific skill
wg skill install        # install the wg Claude Code skill to ~/.claude/skills/wg/
```

**Examples:**
```bash
wg skill list
# Shows all skills referenced across the graph

wg skill find rust
# Lists tasks that require the "rust" skill

wg skill task implement-api
# Shows which skills implement-api requires

wg skill install
# Installs the wg skill for Claude Code into ~/.claude/skills/wg/
```

---

### `wg match`

Find agents capable of performing a task based on required skills.

```bash
wg match <TASK>
```

**Example:**
```bash
wg match implement-api
# Shows agents whose capabilities match the task's required skills
```

---

### `wg matrix`

Matrix integration commands for task management and notifications.

```bash
wg matrix <SUBCOMMAND> [OPTIONS]
```

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `listen` | Listen for Matrix messages |
| `send` | Send a message to Matrix |
| `status` | Check Matrix connection status |
| `login` | Authenticate with Matrix server |
| `logout` | Disconnect from Matrix server |

---

### `wg notify`

Send task notification to Matrix room.

```bash
wg notify <TASK> [OPTIONS]
```

Notifies configured Matrix room(s) about task status changes.

---

## Agency Commands

The agency system manages composable agent identities (roles + motivations). See [AGENCY.md](AGENCY.md) for the full design.

### `wg agency init`

Seed the agency with starter roles (Programmer, Reviewer, Documenter, Architect) and motivations (Careful, Fast, Thorough, Balanced).

```bash
wg agency init
```

**Example:**
```bash
wg agency init
# Creates default roles and motivations to get started with agent identities
```

---

### `wg agency stats`

Display aggregated performance statistics across the agency.

```bash
wg agency stats [--min-evals <N>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--min-evals <N>` | Minimum evaluations to consider a pair "explored" (default: 3) |

Shows role leaderboard, motivation leaderboard, synergy matrix, tag breakdown, and under-explored combinations.

---

### `wg role`

Manage roles — the "what" of agent identity.

| Command | Description |
|---------|-------------|
| `wg role add <name> --outcome <text> [--skill <spec>] [-d <text>]` | Create a new role |
| `wg role list` | List all roles |
| `wg role show <id>` | Show details of a role |
| `wg role edit <id>` | Edit a role in `$EDITOR` (re-hashes on save) |
| `wg role rm <id>` | Delete a role |
| `wg role lineage <id>` | Show evolutionary ancestry |

**Skill specifications:**
- `rust` — simple name tag
- `coding:file:///path/to/style.md` — load content from file
- `review:https://example.com/checklist.md` — fetch from URL
- `tone:inline:Write in a clear, technical style` — inline content

---

### `wg motivation`

Manage motivations — the "why" of agent identity. Also aliased as `wg mot`.

| Command | Description |
|---------|-------------|
| `wg motivation add <name> --accept <text> --reject <text> [-d <text>]` | Create a new motivation |
| `wg motivation list` | List all motivations |
| `wg motivation show <id>` | Show details |
| `wg motivation edit <id>` | Edit in `$EDITOR` (re-hashes on save) |
| `wg motivation rm <id>` | Delete a motivation |
| `wg motivation lineage <id>` | Show evolutionary ancestry |

---

### `wg agent create`

Create a new agent. Agents can represent AI workers or humans.

```bash
wg agent create <NAME> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--role <ROLE-ID>` | Role ID or prefix (required for AI agents, optional for human) |
| `--motivation <MOTIVATION-ID>` | Motivation ID or prefix (required for AI agents, optional for human) |
| `--capabilities <SKILLS>` | Comma-separated skills for task matching |
| `--rate <FLOAT>` | Hourly rate for cost tracking |
| `--capacity <FLOAT>` | Maximum concurrent task capacity |
| `--trust-level <LEVEL>` | `verified`, `provisional` (default), or `unknown` |
| `--contact <STRING>` | Contact info (email, Matrix ID, etc.) |
| `--executor <NAME>` | Executor backend: `claude` (default), `matrix`, `email`, `shell` |

IDs can be prefixes (minimum unique match).

**Examples:**
```bash
# AI agent (role + motivation required)
wg agent create "Careful Coder" --role programmer --motivation careful

# AI agent with operational fields
wg agent create "Rust Expert" \
  --role programmer \
  --motivation careful \
  --capabilities rust,testing \
  --rate 50.0

# Human agent (role + motivation optional)
wg agent create "Erik" \
  --executor matrix \
  --contact "@erik:server" \
  --capabilities rust,python,architecture \
  --trust-level verified
```

---

### `wg agent list|show|rm|lineage|performance`

| Command | Description |
|---------|-------------|
| `wg agent list` | List all agents |
| `wg agent show <id>` | Show agent details with resolved role/motivation |
| `wg agent rm <id>` | Remove an agent |
| `wg agent lineage <id>` | Show agent + role + motivation ancestry |
| `wg agent performance <id>` | Show evaluation history for an agent |

---

### `wg evaluate`

Trigger evaluation of a completed task.

```bash
wg evaluate <TASK> [--evaluator-model <MODEL>] [--dry-run]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--evaluator-model <MODEL>` | Model for the evaluator (overrides config) |
| `--dry-run` | Show the evaluator prompt without executing |

The task must be done or failed. Spawns an evaluator agent that scores the task across four dimensions:
- **correctness** (40%) — output matches desired outcome
- **completeness** (30%) — all aspects addressed
- **efficiency** (15%) — no unnecessary steps
- **style_adherence** (15%) — project conventions and constraints followed

Scores propagate to the agent, role, and motivation performance records.

---

### `wg evolve`

Trigger an evolution cycle to improve roles and motivations based on performance data.

```bash
wg evolve [--strategy <STRATEGY>] [--budget <N>] [--model <MODEL>] [--dry-run]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--strategy <name>` | Evolution strategy (default: `all`) |
| `--budget <N>` | Maximum number of operations to apply |
| `--model <MODEL>` | LLM model for the evolver agent |
| `--dry-run` | Show the evolver prompt without executing |

**Strategies:**
| Strategy | Description |
|----------|-------------|
| `mutation` | Modify a single existing role to improve weak dimensions |
| `crossover` | Combine traits from two high-performing roles |
| `gap-analysis` | Create entirely new roles/motivations for unmet needs |
| `retirement` | Remove consistently poor-performing entities |
| `motivation-tuning` | Adjust trade-offs on existing motivations |
| `all` | Use all strategies as appropriate (default) |

---

## Agent Commands

### `wg agent run`

Run the autonomous agent loop (wake/check/work/sleep cycle).

```bash
wg agent run --actor <ACTOR> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--actor <ACTOR>` | Agent session ID for the autonomous loop (required) |
| `--once` | Run only one iteration then exit |
| `--interval <SECONDS>` | Sleep interval between iterations (default from config, fallback: 10) |
| `--max-tasks <N>` | Stop after completing N tasks |
| `--reset-state` | Reset agent state (discard saved statistics and task history) |

**Example:**
```bash
wg agent run --actor claude --once
# Run one iteration: find a task, work on it, then exit

wg agent run --actor claude --interval 30 --max-tasks 5
# Run agent loop, check every 30s, stop after 5 tasks
```

---

### `wg spawn`

Spawn an agent to work on a specific task.

```bash
wg spawn <TASK> --executor <NAME> [--model <MODEL>] [--timeout <DURATION>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--executor <NAME>` | Executor to use: claude, shell, or custom config name (required) |
| `--model <MODEL>` | Model override (haiku, sonnet, opus) |
| `--timeout <DURATION>` | Timeout (e.g., 30m, 1h, 90s) |

Model selection priority: CLI `--model` > task's `.model` > `coordinator.model` > `agent.model`.

**Example:**
```bash
wg spawn fix-bug --executor claude --model sonnet --timeout 30m
# Spawn a Claude agent to work on fix-bug with a 30 minute timeout
```

---

### `wg next`

Find the best next task for an agent.

```bash
wg next --actor <ACTOR>
```

**Options:**
| Option | Description |
|--------|-------------|
| `--actor <ACTOR>` | Agent session ID to find tasks for (required) |

**Example:**
```bash
wg next --actor claude
# Returns the highest-priority ready task matching the agent's capabilities
```

---

### `wg exec`

Execute a task's shell command (claim + run + done/fail).

```bash
wg exec <TASK> [--actor <ACTOR>] [--dry-run]
wg exec <TASK> --set <CMD>     # set the exec command
wg exec <TASK> --clear         # clear the exec command
```

**Options:**
| Option | Description |
|--------|-------------|
| `--actor <ACTOR>` | Agent performing the execution |
| `--dry-run` | Show what would be executed without running |
| `--set <CMD>` | Set the exec command for a task |
| `--clear` | Clear the exec command for a task |

**Example:**
```bash
# Set a command for a task
wg exec run-tests --set "cargo test"

# Execute it (claims the task, runs the command, marks done or failed)
wg exec run-tests --actor claude

# Preview without running
wg exec run-tests --dry-run
```

---

### `wg trajectory`

Show context-efficient task trajectory (optimal claim order).

```bash
wg trajectory <TASK> [--actor <ACTOR>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--actor <ACTOR>` | Suggest trajectories based on agent's capabilities |

**Example:**
```bash
wg trajectory deploy-prod
# Shows the optimal order to complete deploy-prod and its dependencies
```

---

### `wg heartbeat`

Record agent heartbeat or check for stale agents.

```bash
wg heartbeat [AGENT]                           # record heartbeat
wg heartbeat --check [--threshold <MINUTES>]   # check for stale agents
```

**Options:**
| Option | Description |
|--------|-------------|
| `--check` | Check for stale agents instead of recording a heartbeat |
| `--threshold <MINUTES>` | Minutes without heartbeat before considered stale (default: 5) |

**Examples:**
```bash
wg heartbeat claude
# Record a heartbeat for agent "claude"

wg heartbeat --check --threshold 10
# Find agents with no heartbeat in the last 10 minutes
```

---

### `wg agents`

List running agents (from the service registry).

```bash
wg agents [--alive] [--dead] [--working] [--idle]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--alive` | Only show alive agents (starting, working, idle) |
| `--dead` | Only show dead agents |
| `--working` | Only show working agents |
| `--idle` | Only show idle agents |

**Examples:**
```bash
wg agents
# List all registered agents

wg agents --alive
# Show only agents that are currently running

wg agents --working
# Show agents actively working on tasks
```

---

### `wg kill`

Terminate running agent(s).

```bash
wg kill <AGENT-ID> [--force]   # kill single agent
wg kill --all [--force]         # kill all agents
```

**Options:**
| Option | Description |
|--------|-------------|
| `--force` | Force kill (SIGKILL immediately instead of graceful shutdown) |
| `--all` | Kill all running agents |

**Examples:**
```bash
wg kill agent-1
# Gracefully terminate agent-1

wg kill agent-1 --force
# Force kill agent-1 immediately

wg kill --all
# Terminate all running agents
```

---

### `wg dead-agents`

Detect and clean up dead agents.

```bash
wg dead-agents --check [--threshold <MINUTES>]  # check without modifying
wg dead-agents --cleanup [--threshold <MINUTES>] # mark dead and unclaim tasks
wg dead-agents --remove                          # remove dead agents from registry
wg dead-agents --processes                       # check if agent processes are still running
```

**Options:**
| Option | Description |
|--------|-------------|
| `--check` | Check for dead agents without modifying state |
| `--cleanup` | Mark dead agents and unclaim their tasks |
| `--remove` | Remove dead agents from the registry entirely |
| `--processes` | Check if agent processes are still running at the OS level |
| `--threshold <MINUTES>` | Override heartbeat timeout threshold in minutes |

**Examples:**
```bash
wg dead-agents --check
# List agents that appear to be dead

wg dead-agents --cleanup --threshold 10
# Mark agents dead if no heartbeat for 10 minutes, unclaim their tasks

wg dead-agents --processes
# Check if agent PIDs are still alive in the OS

wg dead-agents --remove
# Remove all dead agents from the registry
```

---

## Service Commands

### `wg service start`

Start the agent service daemon.

```bash
wg service start [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--port <PORT>` | Port for HTTP API (optional) |
| `--socket <PATH>` | Unix socket path (default: /tmp/wg-{project}.sock) |
| `--max-agents <N>` | Max parallel agents (overrides config) |
| `--executor <NAME>` | Executor for spawned agents (overrides config) |
| `--interval <SECS>` | Background poll interval in seconds (overrides config) |
| `--model <MODEL>` | Model for spawned agents (overrides config) |

**Example:**
```bash
wg service start --max-agents 3 --executor claude --model sonnet
# Start the daemon with up to 3 parallel Claude agents using Sonnet
```

---

### `wg service stop`

Stop the agent service daemon.

```bash
wg service stop [--force] [--kill-agents]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--force` | SIGKILL the daemon immediately |
| `--kill-agents` | Also kill running agents (by default they continue) |

**Example:**
```bash
wg service stop --kill-agents
# Stop daemon and terminate all running agents
```

---

### `wg service status`

Show daemon PID, uptime, agent summary, and coordinator state.

```bash
wg service status
```

**Example:**
```bash
wg service status
# Shows PID, uptime, running agents, and coordinator state (active/paused)
```

---

### `wg service reload`

Re-read config.toml without restarting (or apply specific overrides).

```bash
wg service reload [--max-agents <N>] [--executor <NAME>] [--interval <SECS>] [--model <MODEL>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--max-agents <N>` | Maximum parallel agents |
| `--executor <NAME>` | Executor for spawned agents |
| `--interval <SECS>` | Background poll interval |
| `--model <MODEL>` | Model for spawned agents |

Without flags, re-reads config.toml from disk.

**Example:**
```bash
wg service reload
# Re-read config.toml from disk

wg service reload --max-agents 5
# Hot-update max parallel agents without restarting
```

---

### `wg service pause`

Pause the coordinator. Running agents continue, but no new agents are spawned.

```bash
wg service pause
```

**Example:**
```bash
wg service pause
# Pause agent spawning (existing agents continue working)
```

---

### `wg service resume`

Resume the coordinator. Triggers an immediate tick.

```bash
wg service resume
```

**Example:**
```bash
wg service resume
# Resume spawning new agents and trigger an immediate coordinator tick
```

---

### `wg service tick`

Run a single coordinator tick and exit (debug mode).

```bash
wg service tick [--max-agents <N>] [--executor <NAME>] [--model <MODEL>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--max-agents <N>` | Maximum parallel agents (overrides config) |
| `--executor <NAME>` | Executor for spawned agents (overrides config) |
| `--model <MODEL>` | Model for spawned agents (overrides config) |

**Example:**
```bash
wg service tick --executor claude --model haiku
# Run one coordinator tick: check ready tasks, spawn agents, then exit
```

---

### `wg service install`

Generate a systemd user service file for the wg service daemon.

```bash
wg service install
```

**Example:**
```bash
wg service install
# Outputs a systemd unit file; follow instructions to enable auto-start
```

---

## Utility Commands

### `wg init`

Initialize a new workgraph in the current directory.

```bash
wg init
```

Creates `.workgraph/` directory with `graph.jsonl`.

**Example:**
```bash
cd my-project && wg init
# Creates .workgraph/ directory ready for task management
```

---

### `wg check`

Check the graph for issues (cycles, orphan references).

```bash
wg check
```

**Example:**
```bash
wg check
# Reports any dependency cycles or references to non-existent tasks
```

---

### `wg viz`

Visualize the dependency graph (ASCII tree by default).

```bash
wg viz [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--all` | Include done tasks (default: only open tasks) |
| `--status <STATUS>` | Filter by status (open, in-progress, done, blocked) |
| `--critical-path` | Highlight the critical path in red |
| `--dot` | Output Graphviz DOT format |
| `--mermaid` | Output Mermaid diagram format |
| `-o, --output <FILE>` | Render directly to file (requires graphviz) |

**Example:**
```bash
wg viz
# ASCII dependency tree of active tasks

wg viz --all
# Include completed tasks

wg viz --dot
# Graphviz DOT output

wg viz --mermaid
# Mermaid diagram output

wg viz --dot -o graph.png
# Render to PNG file (requires graphviz)

wg viz --critical-path
# Highlight the longest dependency chain
```

---

### `wg archive`

Archive completed tasks to a separate file.

```bash
wg archive [--dry-run] [--older <DURATION>] [--list]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--dry-run` | Show what would be archived without archiving |
| `--older <DURATION>` | Only archive tasks older than this (e.g., 30d, 7d, 1w) |
| `--list` | List already-archived tasks instead of archiving |

**Example:**
```bash
wg archive --dry-run
# Preview which tasks would be archived

wg archive --older 30d
# Archive tasks completed more than 30 days ago

wg archive --list
# Show previously archived tasks
```

---

### `wg reschedule`

Reschedule a task (set `not_before` timestamp).

```bash
wg reschedule <ID> [--after <HOURS>] [--at <TIMESTAMP>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--after <HOURS>` | Hours from now until task is ready |
| `--at <TIMESTAMP>` | Specific ISO 8601 timestamp |

**Example:**
```bash
wg reschedule deploy-prod --after 24
# Delay deploy-prod for 24 hours

wg reschedule deploy-prod --at "2025-06-01T09:00:00Z"
# Schedule deploy-prod for a specific date/time
```

---

### `wg artifact`

Manage task artifacts (produced outputs).

```bash
wg artifact <TASK> [<PATH>] [--remove]
```

Without a path, lists artifacts. With a path, adds it (or removes with `--remove`).

---

### `wg config`

View or modify project configuration.

```bash
wg config [OPTIONS]
```

With no options (or `--show`), displays current configuration.

**Options:**
| Option | Description |
|--------|-------------|
| `--show` | Display current configuration |
| `--init` | Create default config file |
| `--executor <NAME>` | Set agent executor (claude, opencode, codex, shell) |
| `--model <MODEL>` | Set agent model |
| `--set-interval <SECS>` | Set agent sleep interval |
| `--max-agents <N>` | Set coordinator max agents |
| `--coordinator-interval <SECS>` | Set coordinator tick interval |
| `--poll-interval <SECS>` | Set service daemon background poll interval |
| `--coordinator-executor <NAME>` | Set coordinator executor |
| `--auto-evaluate <BOOL>` | Enable/disable automatic evaluation |
| `--auto-assign <BOOL>` | Enable/disable automatic identity assignment |
| `--assigner-model <MODEL>` | Set model for assigner agents |
| `--evaluator-model <MODEL>` | Set model for evaluator agents |
| `--evolver-model <MODEL>` | Set model for evolver agents |
| `--assigner-agent <HASH>` | Set assigner agent (content-hash) |
| `--evaluator-agent <HASH>` | Set evaluator agent (content-hash) |
| `--evolver-agent <HASH>` | Set evolver agent (content-hash) |
| `--retention-heuristics <TEXT>` | Set retention heuristics (prose policy for evolver) |
| `--auto-triage <BOOL>` | Enable/disable automatic triage of dead agents |
| `--triage-model <MODEL>` | Set model for triage (default: haiku) |
| `--triage-timeout <SECS>` | Set timeout for triage calls (default: 30) |
| `--triage-max-log-bytes <N>` | Set max bytes for triage log reading (default: 50000) |

**Examples:**

```bash
# View config
wg config

# Set executor and model
wg config --executor claude --model opus

# Enable the full agency automation loop
wg config --auto-evaluate true --auto-assign true

# Set per-role model overrides
wg config --assigner-model haiku --evaluator-model opus --evolver-model opus
```

---

### `wg quickstart`

Print a concise cheat sheet for agent onboarding — shows project status and commonly-used commands.

```bash
wg quickstart
```

**Example:**
```bash
wg quickstart
# Prints current project status and a quick-reference command list
```

---

### `wg tui`

Launch the interactive terminal dashboard.

```bash
wg tui [--refresh-rate <MS>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--refresh-rate <MS>` | Data refresh rate in milliseconds (default: 2000) |

**Example:**
```bash
wg tui
# Opens the interactive TUI with default 2s refresh

wg tui --refresh-rate 500
# Open TUI with faster 500ms refresh rate
```

---

## Global Options

All commands support these options:

| Option | Description |
|--------|-------------|
| `--dir <PATH>` | Workgraph directory (default: .workgraph) |
| `--json` | Output as JSON for machine consumption |
| `-h, --help` | Show help (use `--help-all` for full command list) |
| `--help-all` | Show all commands in help output (including less common ones) |
| `-a, --alphabetical` | Sort help output alphabetically |
| `-V, --version` | Show version |

**Example:**
```bash
wg --help-all --alphabetical
# Show all commands sorted alphabetically

wg list --json
# Output task list as JSON
```
