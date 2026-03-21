# Workgraph Command Reference

Complete reference for all `wg` commands. Most query commands support `--json` for machine-readable output. All commands support `--dir <path>` to specify a custom workgraph directory.

## Table of Contents

- [Task Management](#task-management)
- [Query Commands](#query-commands)
- [Analysis Commands](#analysis-commands)
- [Function Commands](#function-commands)
- [Trace Commands](#trace-commands)
- [Agent and Resource Management](#agent-and-resource-management)
- [Agency Commands](#agency-commands)
- [Agent Commands](#agent-commands)
- [Peer Commands](#peer-commands)
- [Service Commands](#service-commands)
- [Monitoring Commands](#monitoring-commands)
- [Communication Commands](#communication-commands)
- [Model and Endpoint Management](#model-and-endpoint-management)
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
| `--after <ID>` | This task comes after another task (repeatable) |
| `--repo <REPO>` | Create the task in a peer workgraph (by name or path) |
| `--assign <AGENT>` | Assign to an agent |
| `--hours <N>` | Estimated hours |
| `--cost <N>` | Estimated cost |
| `-t, --tag <TAG>` | Add tag (repeatable) |
| `--skill <SKILL>` | Required skill (repeatable) |
| `--input <PATH>` | Input file/context needed (repeatable) |
| `--deliverable <PATH>` | Expected output (repeatable) |
| `--max-retries <N>` | Maximum retry attempts |
| `--visibility <LEVEL>` | Task visibility zone for trace exports: `internal` (default), `peer`, `public` |
| `--model <MODEL>` | Preferred model for this task (haiku, sonnet, opus) |
| `--verify <CRITERIA>` | Verification criteria — task requires review before done |
| `--max-iterations <N>` | Maximum cycle iterations — sets `CycleConfig` on this task, making it a cycle header |
| `--cycle-guard <EXPR>` | Guard condition for cycle iteration: `task:<id>=<status>` or `always` |
| `--cycle-delay <DUR>` | Delay between cycle iterations (e.g., `30s`, `5m`, `1h`) |
| `--exec-mode <MODE>` | Execution weight: `full` (default), `light` (read-only tools), `bare` (wg CLI only), `shell` (no LLM) |
| `--provider <PROVIDER>` | Provider for this task: `anthropic`, `openai`, `openrouter`, `local` |
| `--paused` | Create the task in paused state (default for interactive use) |
| `--no-place` | Skip automatic placement — make task immediately available for dispatch |
| `--place-near <IDS>` | Placement hint: place near these tasks (comma-separated IDs) |
| `--place-before <IDS>` | Placement hint: place before these tasks (comma-separated IDs) |
| `--delay <DUR>` | Delay before task becomes ready (e.g., `30s`, `5m`, `1h`, `1d`) |
| `--not-before <TIMESTAMP>` | Absolute timestamp before which task won't be dispatched (ISO 8601) |
| `--no-converge` | Force all cycle iterations to run (agents cannot signal convergence) |
| `--no-restart-on-failure` | Disable automatic cycle restart on failure (restart is on by default) |
| `--max-failure-restarts <N>` | Maximum failure-triggered cycle restarts (default: 3) |
| `--context-scope <SCOPE>` | Context scope for prompt assembly: `clean`, `task`, `graph`, `full` (see below) |

**Context scopes** control how much context the coordinator assembles into the agent's prompt. Each level includes everything from the previous level:

| Scope | Includes |
|-------|----------|
| `clean` | Core task info only (title, description, dependency context) |
| `task` | + workflow sections, tags/skills, downstream awareness |
| `graph` | + project description, subgraph summary (1-hop neighborhood) |
| `full` | + system awareness preamble, full graph summary, CLAUDE.md content |

**Examples:**

```bash
# Simple task
wg add "Fix login bug"

# Task with dependencies and metadata
wg add "Implement user auth" \
  --id user-auth \
  --after design-api \
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

# Minimal prompt for a focused, low-context task
wg add "Format config file" --context-scope clean

# Full context for a task that needs project-wide awareness
wg add "Architect new module" --context-scope full
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
| `--add-after <ID>` | Add an after dependency (repeatable) |
| `--remove-after <ID>` | Remove an after dependency (repeatable) |
| `--add-tag <TAG>` | Add a tag (repeatable) |
| `--remove-tag <TAG>` | Remove a tag (repeatable) |
| `--add-skill <SKILL>` | Add a required skill (repeatable) |
| `--remove-skill <SKILL>` | Remove a required skill (repeatable) |
| `--model <MODEL>` | Update preferred model |
| `--max-iterations <N>` | Set maximum cycle iterations (creates or updates `CycleConfig`) |
| `--cycle-guard <EXPR>` | Set guard condition for cycle iteration |
| `--cycle-delay <DUR>` | Set delay between cycle iterations |
| `--visibility <LEVEL>` | Set task visibility zone: `internal`, `peer`, `public` |
| `--context-scope <SCOPE>` | Set context scope for prompt assembly: `clean`, `task`, `graph`, `full` |
| `--exec-mode <MODE>` | Set execution weight: `full` (default), `light` (read-only tools), `bare` (wg CLI only), `shell` (no LLM) |
| `--provider <PROVIDER>` | Update provider for this task (`anthropic`, `openai`, `openrouter`, `local`) |
| `--verify <CRITERIA>` | Set or update verification criteria (shell command that must pass before done) |
| `--delay <DUR>` | Delay before task becomes ready (e.g., `30s`, `5m`, `1h`, `1d`) |
| `--not-before <TIMESTAMP>` | Absolute timestamp before which task won't be dispatched (ISO 8601) |
| `--no-converge` | Force all cycle iterations to run (agents cannot signal convergence) |
| `--no-restart-on-failure` | Disable automatic cycle restart on failure |
| `--max-failure-restarts <N>` | Maximum failure-triggered cycle restarts (default: 3) |

Triggers a `graph_changed` IPC notification to the service daemon, so the coordinator picks up changes immediately.

**Examples:**

```bash
# Change title
wg edit my-task --title "Better title"

# Add a dependency
wg edit my-task --add-after other-task

# Swap tags
wg edit my-task --remove-tag stale --add-tag urgent

# Change model
wg edit my-task --model opus

# Set cycle configuration (makes this task a cycle header)
wg edit my-task --max-iterations 5
wg edit my-task --cycle-guard "task:review=failed"
wg edit my-task --cycle-delay "5m"

# Reduce context for a simple task
wg edit my-task --context-scope clean

# Use bare execution mode (wg CLI only)
wg edit my-task --exec-mode bare

# Set or update verification criteria
wg edit my-task --verify "cargo test test_feature passes"

# Set a provider
wg edit my-task --provider openrouter

# Schedule a delay
wg edit my-task --delay 1h
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
| `--skip-verify` | Skip the verify command gate (human escape hatch, blocked when `WG_AGENT_ID` is set) |

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

### `wg fail`

Mark a task as failed (can be retried later).

```bash
wg fail <ID> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--reason <REASON>` | Reason for failure |
| `--eval-reject` | Reject a done task via evaluation gate — allows failing a task that is already Done because the evaluator determined the work is unacceptable. The task transitions to Failed and its dependents become blocked |

**Example:**
```bash
wg fail deploy-prod --reason "AWS credentials expired"
```

---

### `wg abandon`

Mark a task as abandoned (will not be completed).

```bash
wg abandon <ID> [OPTIONS]
```

Abandoned is a terminal state — the task will not be retried.

**Options:**
| Option | Description |
|--------|-------------|
| `--reason <REASON>` | Reason for abandonment |
| `--superseded-by <IDS>` | Task IDs that supersede/replace this task (comma-separated) |

**Example:**
```bash
wg abandon legacy-migration --reason "Feature deprecated"
wg abandon old-approach --superseded-by new-approach-a,new-approach-b
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

# View agent prompts and outputs
wg log <ID> --agent

# View the operations log
wg log --operations
```

**Options:**
| Option | Description |
|--------|-------------|
| `--actor <ACTOR>` | Actor adding the log entry |
| `--list` | List log entries instead of adding |
| `--agent` | Show archived agent prompts and outputs for a task |
| `--operations` | Show the operations log (reads current and rotated files) |

**Examples:**
```bash
wg log implement-api "Completed endpoint handlers" --actor erik
wg log implement-api --list
wg log implement-api --agent
wg log --operations
```

---

### `wg assign`

Assign an agent identity to a task (or clear the assignment).

```bash
wg assign <TASK> <AGENT-HASH>    # Assign agent to task
wg assign <TASK> --clear         # Remove assignment
wg assign <TASK> --auto          # Auto-select agent via LLM
```

When the service spawns that task, the agent's role and tradeoff are injected into the prompt. The agent hash can be a prefix (minimum 4 characters).

**Options:**
| Option | Description |
|--------|-------------|
| `--clear` | Clear the agent assignment from the task |
| `--auto` | Automatically select an agent using LLM |

**Example:**
```bash
wg assign my-task a3f7c21d
wg assign my-task --clear
wg assign my-task --auto
```

---

### `wg show`

Display detailed information about a single task.

```bash
wg show <ID>
```

Shows all task fields including description, logs, timestamps, dependencies, model, and agent assignment.

---

### `wg pause`

Pause a task so the coordinator skips it until resumed.

```bash
wg pause <ID>
```

**Example:**
```bash
wg pause implement-api
# Coordinator will skip this task until it is resumed
```

---

### `wg resume`

Resume a paused task (propagates to downstream subgraph by default).

```bash
wg resume <ID> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--only` | Only resume this single task (skip subgraph propagation) |

**Example:**
```bash
wg resume implement-api
# Task and downstream subgraph are eligible for coordinator dispatch again

wg resume implement-api --only
# Only resume this task, not its dependents
```

---

### `wg approve`

Approve a task pending validation (transitions to Done).

```bash
wg approve <TASK>
```

**Arguments:**
- `TASK` - Task ID to approve (required)

**Example:**
```bash
wg approve security-audit
# Transitions the task from pending-validation to done
```

---

### `wg reject`

Reject a task pending validation (reopens with feedback, or fails after max rejections).

```bash
wg reject <TASK> --reason <REASON>
```

**Arguments:**
- `TASK` - Task ID to reject (required)

**Options:**
| Option | Description |
|--------|-------------|
| `--reason <REASON>` | Reason for rejection (required) |

**Example:**
```bash
wg reject security-audit --reason "Missing severity ratings for 3 findings"
# Reopens the task with feedback so the agent can address issues
```

---

### `wg publish`

Publish a draft task (validates dependencies, then resumes entire subgraph).

```bash
wg publish <TASK> [OPTIONS]
```

**Arguments:**
- `TASK` - Task ID to publish (required)

**Options:**
| Option | Description |
|--------|-------------|
| `--only` | Only publish this single task (skip subgraph propagation) |

**Examples:**
```bash
wg publish my-draft-task
# Validates dependencies and resumes the entire subgraph

wg publish my-draft-task --only
# Publish just this task without propagating to the subgraph
```

---

### `wg add-dep`

Add a dependency edge between two tasks.

```bash
wg add-dep <TASK> <DEPENDENCY>
```

**Arguments:**
- `TASK` - The task that will depend on the dependency (required)
- `DEPENDENCY` - The dependency (blocker) task (required)

**Example:**
```bash
wg add-dep deploy-prod run-tests
# deploy-prod now waits for run-tests to complete
```

---

### `wg rm-dep`

Remove a dependency edge between two tasks.

```bash
wg rm-dep <TASK> <DEPENDENCY>
```

**Arguments:**
- `TASK` - The task to remove the dependency from (required)
- `DEPENDENCY` - The dependency to remove (required)

**Example:**
```bash
wg rm-dep deploy-prod run-tests
# deploy-prod no longer waits for run-tests
```

---

### `wg wait`

Park a task and exit — sets status to Waiting until a condition is met.

```bash
wg wait <TASK> --until <UNTIL> [OPTIONS]
```

**Arguments:**
- `TASK` - Task ID to park (required)

**Options:**
| Option | Description |
|--------|-------------|
| `--until <UNTIL>` | Condition to wait for: `task:<id>=<status>`, `timer:<duration>`, `message`, `human-input`, `file:<path>` (required) |
| `--checkpoint <CHECKPOINT>` | Checkpoint summary of progress so far |

**Examples:**
```bash
wg wait my-task --until "task:dep-a=done"
# Park until dep-a completes

wg wait my-task --until "timer:5m"
# Park for 5 minutes

wg wait my-task --until "message" --checkpoint "Completed phase 1, waiting for review feedback"
# Park until a message arrives, saving a checkpoint of progress

wg wait my-task --until "human-input"
# Park until a human sends a message

wg wait my-task --until "file:path/to/file"
# Park until a file changes
```

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
| `--paused` | Only show paused tasks |
| `--tag <TAG>` | Filter by tag (repeatable, multiple `--tag` flags use AND semantics) |

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

### `wg discover`

Show recently completed tasks and their artifacts (stigmergic discovery).

```bash
wg discover [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--since <SINCE>` | Time window (e.g. `24h`, `7d`, `30m`). Default: `24h` |
| `--with-artifacts` | Include artifact paths in output |

**Examples:**
```bash
wg discover
# Show tasks completed in the last 24 hours

wg discover --since 7d
# Show tasks completed in the last 7 days

wg discover --since 1h --with-artifacts
# Show recently completed tasks with their artifact paths
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

## Function Commands

Function commands manage workflow templates — extracting reusable patterns from completed work, listing and inspecting them, and applying them as new task graphs. All function commands are subcommands of `wg func`.

> **Note:** These commands were previously under `wg trace`. The old names (`wg trace extract`, `wg trace instantiate`, `wg trace list-functions`, `wg trace show-function`, `wg trace bootstrap`, `wg trace make-adaptive`) still work as hidden aliases but print a deprecation warning. Use the `wg func` forms going forward.

### `wg func list`

List available functions (workflow templates).

```bash
wg func list [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--verbose` | Show input parameters and task templates |
| `--include-peers` | Include functions from federated peer workgraphs |
| `--visibility <LEVEL>` | Filter by visibility level: `internal`, `peer`, `public` |

**Examples:**
```bash
wg func list
# List all local functions

wg func list --include-peers --visibility peer
# Include peer functions, show only peer-visible or higher
```

---

### `wg func show`

Show details of a function.

```bash
wg func show <ID>
```

The ID supports prefix matching. Displays version, visibility, planning config, constraints, memory config, inputs, task templates, outputs, and run history.

**Example:**
```bash
wg func show impl-feat
# Shows full details of the impl-feature function (prefix match)
```

---

### `wg func extract`

Extract a function from completed task(s).

```bash
wg func extract <TASK-ID>... [OPTIONS]
```

Supports two modes:
- **Static extraction** (single task): Extracts a version 1 function with fixed topology from one completed task.
- **Generative extraction** (`--generative`, multiple tasks): Compares multiple completed traces to produce a version 2 function with a planning node and structural constraints.

**Options:**
| Option | Description |
|--------|-------------|
| `--name <NAME>` | Function name/ID (default: derived from task ID) |
| `--subgraph` | Include all subtasks (tasks blocked by this one) |
| `--recursive` | Alias for `--subgraph` |
| `--generalize` | Use LLM to generalize descriptions (calls executor) |
| `--generative` | Multi-trace mode: compare multiple traces to produce a version 2 (generative) function |
| `--output <PATH>` | Write to specific path instead of `.workgraph/functions/` |
| `--force` | Overwrite existing function with same name |
| `--include-evaluations` | Include coordinator-generated evaluation and assignment tasks (`evaluate-*`, `assign-*`) that are normally filtered out |

**Examples:**
```bash
# Static extraction from a completed task
wg func extract impl-auth --name impl-feature --subgraph
# Extracts the full subgraph as a reusable template

# Generative extraction from multiple traces
wg func extract impl-auth impl-caching impl-logging --generative --name impl-feature
# Compares three traces, produces a version 2 function with planning node

# With LLM generalization
wg func extract fix-login-bug --name bug-fix --generalize
# LLM replaces instance-specific values with {{input.<name>}} placeholders
```

---

### `wg func apply`

Create tasks from a function with provided inputs.

```bash
wg func apply <FUNCTION-ID> [OPTIONS]
```

The function ID supports prefix matching. For version 2+ (generative) functions, application first runs the planner task; when the planner completes and produces YAML output, re-running apply parses it and creates the planned tasks. For version 3 (adaptive) functions, past run summaries are injected into the planner prompt via `{{memory.run_summaries}}`.

**Options:**
| Option | Description |
|--------|-------------|
| `--from <SOURCE>` | Load function from a peer (`peer:function-id`), file (`.yaml`), or peer name |
| `--input <KEY=VALUE>` | Set an input parameter (repeatable) |
| `--input-file <PATH>` | Read inputs from a YAML/JSON file |
| `--prefix <PREFIX>` | Override the task ID prefix (default: from `feature_name` input or function ID) |
| `--dry-run` | Show what tasks would be created without creating them |
| `--after <ID>` | Make root tasks depend on this task (repeatable; alias: `--blocked-by`) |
| `--model <MODEL>` | Set model for all created tasks |

**Examples:**
```bash
# Instantiate a static function
wg func apply impl-feature \
  --input feature_name=auth --input description="Add OAuth support"
# Creates tasks: auth-plan, auth-implement, auth-validate, etc.

# Instantiate from a peer
wg func apply impl-feature --from alice:impl-feature \
  --input feature_name=caching

# Instantiate with dependency
wg func apply bug-fix --input bug_name=login-crash \
  --after design-phase --model sonnet

# Preview without creating
wg func apply impl-feature --input feature_name=auth --dry-run
```

---

### `wg func bootstrap`

Bootstrap the `extract-function` meta-function — a built-in version 2 (generative) function that describes the trace extraction process itself as a workgraph workflow.

```bash
wg func bootstrap [OPTIONS]
```

Creates `.workgraph/functions/extract-function.yaml` with a planning node, structural constraints, and a static fallback (analyze → draft → validate → export).

**Options:**
| Option | Description |
|--------|-------------|
| `--force` | Overwrite if `extract-function` already exists |

**Examples:**
```bash
# Bootstrap the meta-function
wg func bootstrap

# Use it to extract a new function via a managed workflow
wg func apply extract-function \
  --input source_task_id=impl-auth \
  --input function_name=impl-feature

# Later, make it adaptive (learns from past extractions)
wg func make-adaptive extract-function
```

---

### `wg func make-adaptive`

Upgrade a generative (version 2) function to adaptive (version 3) by adding trace memory.

```bash
wg func make-adaptive <FUNCTION-ID> [OPTIONS]
```

Scans provenance for past applications of the function, builds run summaries from graph state, stores them, injects `{{memory.run_summaries}}` into the planner template, and bumps the version to 3. Version 1 (static) functions are rejected — extract with `--generative` first.

**Options:**
| Option | Description |
|--------|-------------|
| `--max-runs <N>` | Maximum number of past runs to include in planner memory (default: 10) |

**Examples:**
```bash
# Upgrade a generative function to adaptive
wg func make-adaptive impl-feature

# With custom memory depth
wg func make-adaptive deploy-pipeline --max-runs 20
```

---

---

## Trace Commands

Trace commands cover execution history and trace data export/import. All trace commands are subcommands of `wg trace`.

> **Note:** Function management commands (`extract`, `list-functions`, `show-function`, `instantiate`, `bootstrap`, `make-adaptive`) have moved to `wg func`. The old `wg trace` names still work as hidden aliases but print a deprecation warning.

### `wg trace show`

Show the execution history of a task.

```bash
wg trace show <ID> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--full` | Show complete agent conversation output |
| `--ops-only` | Show only provenance log entries |
| `--recursive` | Show full execution tree (all descendant tasks) |
| `--timeline` | Chronological timeline with parallel lanes (requires `--recursive`) |
| `--graph` | Render the trace subgraph as a 2D box layout |
| `--animate` | Replay graph evolution over time in the terminal |
| `--speed <N>` | Playback speed multiplier for `--animate` (default: 10.0) |

**Examples:**
```bash
wg trace show deploy-prod
# Summary of task execution history

wg trace show deploy-prod --recursive --timeline
# Timeline view of deploy-prod and all descendant tasks

wg trace show deploy-prod --animate --speed 5
# Animated replay of graph evolution at 5x speed
```

---

### `wg trace export`

Export trace data filtered by visibility zone.

```bash
wg trace export [OPTIONS]
```

Produces a JSON bundle containing tasks, evaluations, operations, and functions, filtered and redacted according to the visibility level.

**Options:**
| Option | Description |
|--------|-------------|
| `--root <ID>` | Scope export to a task and all its descendants |
| `--visibility <LEVEL>` | Visibility filter: `internal` (everything), `peer` (richer for trusted peers), `public` (sanitized). Default: `internal` |
| `-o, --output <PATH>` | Output file path (default: stdout) |

**Visibility behavior:**
| Data | Internal | Peer | Public |
|------|----------|------|--------|
| Tasks | All | Public + peer visibility | Public only |
| Agent/log | Full | Agent shown, log stripped | Both stripped |
| Evaluations | Full | Included (notes stripped) | Omitted |
| Operations | All ops, full detail | All ops for included tasks | Structural ops only, detail stripped |
| Functions | All | Peer/public visible, redacted | Public only, fully redacted |

**Examples:**
```bash
wg trace export --visibility public -o public-trace.json
# Sanitized export safe for open sharing

wg trace export --visibility peer --root deploy-prod -o peer-export.json
# Richer export scoped to deploy-prod subtree, for trusted peers

wg trace import peer-export.json --source "peer:alice"
# Import a peer's export as read-only context
```

---

### `wg trace import`

Import a trace export file as read-only context.

```bash
wg trace import <FILE> [OPTIONS]
```

Tasks are namespaced under `imported/<source>/` to avoid ID collisions.

**Options:**
| Option | Description |
|--------|-------------|
| `--source <TAG>` | Source tag for imported data (e.g., `peer:alice`, `team:platform`) |
| `--dry-run` | Show what would be imported without making changes |

**Example:**
```bash
wg trace import peer-export.json --source "peer:alice" --dry-run
# Preview what would be imported
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
| `listen` | Start the Matrix message listener |
| `send` | Send a message to a Matrix room |
| `status` | Show Matrix connection status |
| `login` | Login with password (caches access token) |
| `logout` | Logout and clear cached credentials |

---

### `wg notify`

Send task notification to Matrix room.

```bash
wg notify <TASK> [OPTIONS]
```

Notifies configured Matrix room(s) about task status changes.

---

## Agency Commands

The agency system manages composable agent identities (roles + tradeoffs). See [AGENCY.md](AGENCY.md) for the full design.

### `wg agency init`

Seed the agency with starter roles (Programmer, Reviewer, Documenter, Architect) and tradeoffs (Careful, Fast, Thorough, Balanced).

```bash
wg agency init
```

**Example:**
```bash
wg agency init
# Creates default roles and tradeoffs to get started with agent identities
```

---

### `wg agency migrate`

Migrate old-format agency store (`roles/`, `motivations/`, `agents/`) to the `primitives/` + `cache/` format.

```bash
wg agency migrate [--dry-run]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--dry-run` | Show what would be migrated without writing |

---

### `wg agency scan`

Scan the filesystem for agency stores.

```bash
wg agency scan <ROOT> [--max-depth <N>]
```

**Arguments:**
- `ROOT` — Root directory to scan (required)

**Options:**
| Option | Description |
|--------|-------------|
| `--max-depth <N>` | Maximum recursion depth (default: 10) |

**Example:**
```bash
wg agency scan /home/erik --max-depth 5
# Find all agency stores under the given root
```

---

### `wg agency pull`

Pull entities from another agency store into the local project.

```bash
wg agency pull <SOURCE> [OPTIONS]
```

**Arguments:**
- `SOURCE` — Source store (path, named remote, or directory)

**Options:**
| Option | Description |
|--------|-------------|
| `--entity <IDS>` | Only pull specific entity IDs (prefix match) |
| `--type <TYPE>` | Only pull entities of this type (`role`, `tradeoff`, `agent`) |
| `--dry-run` | Show what would be pulled without writing |
| `--no-performance` | Skip merging performance data (copy definitions only) |
| `--no-evaluations` | Skip copying evaluation JSON files |
| `--force` | Overwrite local metadata instead of merging |
| `--global` | Pull into `~/.workgraph/agency/` instead of local project |

**Example:**
```bash
wg agency pull /home/alice/project
# Pull all entities from Alice's agency store

wg agency pull my-remote --type role --dry-run
# Preview pulling only roles from a named remote
```

---

### `wg agency push`

Push local entities to another agency store.

```bash
wg agency push <TARGET> [OPTIONS]
```

**Arguments:**
- `TARGET` — Target store (path, named remote, or directory)

**Options:**
| Option | Description |
|--------|-------------|
| `--entity <IDS>` | Only push specific entity IDs |
| `--type <TYPE>` | Only push entities of this type (`role`, `tradeoff`, `agent`) |
| `--dry-run` | Show what would be pushed without writing |
| `--no-performance` | Skip merging performance data (copy definitions only) |
| `--no-evaluations` | Skip copying evaluation JSON files |
| `--force` | Overwrite target metadata instead of merging |
| `--global` | Push from `~/.workgraph/agency/` instead of local project |

**Example:**
```bash
wg agency push /home/alice/project --type role
# Push only roles to Alice's agency store
```

---

### `wg agency merge`

Merge entities from multiple agency stores.

```bash
wg agency merge [SOURCES]... [OPTIONS]
```

**Arguments:**
- `[SOURCES]...` — Source stores (paths, named remotes, or directories)

**Options:**
| Option | Description |
|--------|-------------|
| `--into <PATH>` | Merge into a specific target path instead of local project |
| `--dry-run` | Show what would be merged without writing |

**Example:**
```bash
wg agency merge /home/alice/project /home/bob/project
# Merge entities from two stores into local

wg agency merge store-a store-b --into /shared/agency --dry-run
# Preview merging into a shared target
```

---

### `wg agency remote`

Manage named references to other agency stores.

```bash
wg agency remote <COMMAND>
```

| Command | Description |
|---------|-------------|
| `add <NAME> <PATH> [-d <TEXT>]` | Add a named remote agency store |
| `remove <NAME>` | Remove a named remote |
| `list` | List all configured remotes |
| `show <NAME>` | Show details of a remote including entity counts |

**Example:**
```bash
wg agency remote add alice /home/alice/project -d "Alice's agency"
wg agency remote list
wg agency remote show alice
wg agency remote remove alice
```

---

### `wg agency deferred`

List pending deferred evolver operations awaiting human review.

```bash
wg agency deferred
```

---

### `wg agency approve`

Approve a deferred evolver operation.

```bash
wg agency approve <ID> [-n <NOTE>]
```

**Arguments:**
- `ID` — Deferred operation ID (required)

**Options:**
| Option | Description |
|--------|-------------|
| `-n, --note <NOTE>` | Optional note explaining approval |

---

### `wg agency reject`

Reject a deferred evolver operation.

```bash
wg agency reject <ID> [-n <NOTE>]
```

**Arguments:**
- `ID` — Deferred operation ID (required)

**Options:**
| Option | Description |
|--------|-------------|
| `-n, --note <NOTE>` | Optional note explaining rejection |

---

### `wg agency create`

Invoke the creator agent to discover and add new primitives.

```bash
wg agency create [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--model <MODEL>` | Model to use for the creator agent |
| `--dry-run` | Show what would be created without writing |

**Example:**
```bash
wg agency create --model opus
# Use LLM to discover and add new roles/tradeoffs

wg agency create --dry-run
# Preview what would be created
```

---

### `wg agency import`

Import Agency's starter.csv primitives into WorkGraph.

```bash
wg agency import <CSV_PATH> [OPTIONS]
```

**Arguments:**
- `CSV_PATH` — Path to the CSV file to import (required)

**Options:**
| Option | Description |
|--------|-------------|
| `--dry-run` | Show what would be imported without writing files |
| `--tag <TAG>` | Provenance tag (default: `agency-import`) |

**Example:**
```bash
wg agency import starter.csv --dry-run
# Preview what would be imported

wg agency import starter.csv --tag "external-v2"
# Import with a custom provenance tag
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
| `--by-model` | Group stats by model (shows per-model score breakdown) |

Shows role leaderboard, tradeoff leaderboard, synergy matrix, tag breakdown, and under-explored combinations.

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

### `wg tradeoff`

Manage tradeoffs — acceptable and unacceptable constraints for agent identity.

| Command | Description |
|---------|-------------|
| `wg tradeoff add <name> --accept <text> --reject <text> [-d <text>]` | Create a new tradeoff |
| `wg tradeoff list` | List all tradeoffs |
| `wg tradeoff show <id>` | Show details |
| `wg tradeoff edit <id>` | Edit in `$EDITOR` (re-hashes on save) |
| `wg tradeoff rm <id>` | Delete a tradeoff |
| `wg tradeoff lineage <id>` | Show evolutionary ancestry |

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
| `--tradeoff <TRADEOFF-ID>` | Tradeoff ID or prefix (required for AI agents, optional for human) |
| `--capabilities <SKILLS>` | Comma-separated skills for task matching |
| `--rate <FLOAT>` | Hourly rate for cost tracking |
| `--capacity <FLOAT>` | Maximum concurrent task capacity |
| `--trust-level <LEVEL>` | `verified`, `provisional` (default), or `unknown` |
| `--contact <STRING>` | Contact info (email, Matrix ID, etc.) |
| `--executor <NAME>` | Executor backend: `claude` (default), `matrix`, `email`, `shell` |
| `--model <MODEL>` | Preferred model (e.g., opus, sonnet, haiku, or full model ID) |
| `--provider <PROVIDER>` | Preferred provider (e.g., anthropic, openrouter) |

IDs can be prefixes (minimum unique match).

**Examples:**
```bash
# AI agent (role + tradeoff required)
wg agent create "Careful Coder" --role programmer --tradeoff careful

# AI agent with operational fields
wg agent create "Rust Expert" \
  --role programmer \
  --tradeoff careful \
  --capabilities rust,testing \
  --rate 50.0

# Human agent (role + tradeoff optional)
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
| `wg agent show <id>` | Show agent details with resolved role/tradeoff |
| `wg agent rm <id>` | Remove an agent |
| `wg agent lineage <id>` | Show agent + role + tradeoff ancestry |
| `wg agent performance <id>` | Show evaluation history for an agent |

---

### `wg evaluate`

Evaluate tasks: trigger LLM-based evaluation, record external scores, or view evaluation history.

```bash
wg evaluate <SUBCOMMAND>
```

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `run` | Trigger LLM-based evaluation of a completed task |
| `record` | Record an evaluation from an external source |
| `show` | Show evaluation history |

---

#### `wg evaluate run`

Trigger LLM-based evaluation of a completed task.

```bash
wg evaluate run <TASK> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--evaluator-model <MODEL>` | Model for the evaluator (overrides config) |
| `--dry-run` | Show what would be evaluated without spawning the evaluator |
| `--flip` | Run FLIP (roundtrip intent fidelity) evaluation instead of direct evaluation |

The task must be done or failed. Spawns an evaluator agent that scores the task across four dimensions:
- **correctness** (40%) — output matches desired outcome
- **completeness** (30%) — all aspects addressed
- **efficiency** (15%) — no unnecessary steps
- **style_adherence** (15%) — project conventions and constraints followed

Scores propagate to the agent, role, and tradeoff performance records.

**Example:**
```bash
wg evaluate run my-task
wg evaluate run my-task --evaluator-model opus --dry-run
```

---

#### `wg evaluate record`

Record an evaluation from an external source (human review, CI metrics, outcome signals).

```bash
wg evaluate record --task <TASK> --score <SCORE> --source <SOURCE> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--task <TASK>` | Task ID (required) |
| `--score <SCORE>` | Overall score, 0.0–1.0 (required) |
| `--source <SOURCE>` | Source identifier, e.g. `"outcome:sharpe"`, `"manual"` (required) |
| `--notes <NOTES>` | Optional notes |
| `--dim <DIM=SCORE>` | Optional dimensional scores (repeatable, format: `dimension=score`) |

**Example:**
```bash
wg evaluate record --task deploy-prod --score 0.85 --source "manual" \
  --notes "Clean deploy" --dim correctness=0.9 --dim efficiency=0.8
```

---

#### `wg evaluate show`

Show evaluation history with optional filters. When a positional `TASK` is given, shows both task-level and org-level scores side by side.

```bash
wg evaluate show [TASK] [OPTIONS]
```

**Arguments:**
- `[TASK]` - Show both task-level and org-level scores side by side for this task (optional)

**Options:**
| Option | Description |
|--------|-------------|
| `--task <TASK>` | Filter by task ID (prefix match, when no positional TASK arg) |
| `--agent <AGENT>` | Filter by agent ID (prefix match) |
| `--source <SOURCE>` | Filter by source (exact match or glob, e.g. `"outcome:*"`) |
| `--limit <N>` | Show only the N most recent evaluations |

**Example:**
```bash
wg evaluate show
wg evaluate show --task deploy --limit 10
wg evaluate show --source "outcome:*"
```

---

### `wg evolve`

Trigger an evolution cycle, or review deferred operations.

```bash
wg evolve <SUBCOMMAND>
```

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `run` | Trigger an evolution cycle on agency roles and tradeoffs |
| `apply` | Apply a `synthesis-result.json` from a fan-out evolution run |
| `review` | Review deferred evolver operations (list, approve, reject) |

#### `wg evolve run`

```bash
wg evolve run [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--strategy <name>` | Evolution strategy (default: `all`) |
| `--budget <N>` | Maximum number of operations to apply |
| `--model <MODEL>` | LLM model for the evolver agent |
| `--dry-run` | Show proposed changes without applying them |
| `--autopoietic` | Enable autopoietic cycle mode (back-edge from evaluate to partition) |
| `--max-iterations <N>` | Max cycle iterations (default: 3, requires `--autopoietic`) |
| `--cycle-delay <SECS>` | Seconds between cycle iterations (default: 3600, requires `--autopoietic`) |
| `--force-fanout` | Force fan-out mode even with <50 evaluations |
| `--single-shot` | Force legacy single-shot mode even with ≥50 evaluations |

**Strategies:**
| Strategy | Description |
|----------|-------------|
| `mutation` | Modify a single existing role to improve weak dimensions |
| `crossover` | Combine traits from two high-performing roles |
| `gap-analysis` | Create entirely new roles/tradeoffs for unmet needs |
| `retirement` | Remove consistently poor-performing entities |
| `tradeoff-tuning` | Adjust constraints on existing tradeoffs |
| `all` | Use all strategies as appropriate (default) |

#### `wg evolve apply`

Apply a synthesis-result.json from a fan-out evolution run.

```bash
wg evolve apply <SYNTHESIS_FILE> [OPTIONS]
```

**Arguments:**
- `SYNTHESIS_FILE` — Path to `synthesis-result.json` (required)

**Options:**
| Option | Description |
|--------|-------------|
| `-o, --output <PATH>` | Output path for `apply-results.json` (default: auto-derived from synthesis file path) |

**Example:**
```bash
wg evolve apply .workgraph/agency/synthesis-result.json
# Apply the synthesized evolution operations

wg evolve apply synthesis-result.json -o results.json
# Apply with a custom output path
```

---

#### `wg evolve review`

```bash
wg evolve review <SUBCOMMAND>
```

| Subcommand | Description |
|------------|-------------|
| `list` | List pending deferred operations awaiting human review |
| `approve <ID>` | Approve a deferred evolver operation and apply it |
| `reject <ID>` | Reject a deferred evolver operation |

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
| `--executor <NAME>` | Executor to use: claude, amplifier, shell, or custom config name (required) |
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
wg dead-agents [--threshold <MINUTES>]           # check for dead agents (default)
wg dead-agents --cleanup [--threshold <MINUTES>] # mark dead and unclaim tasks
wg dead-agents --remove                          # remove dead agents from registry
wg dead-agents --processes                       # check if agent processes are still running
wg dead-agents --purge [--delete-dirs]           # purge dead/done/failed agents from registry
```

**Options:**
| Option | Description |
|--------|-------------|
| `--cleanup` | Mark dead agents and unclaim their tasks |
| `--remove` | Remove dead agents from the registry entirely |
| `--processes` | Check if agent processes are still running at the OS level |
| `--purge` | Purge dead/done/failed agents from registry (and optionally delete dirs) |
| `--delete-dirs` | Also delete agent work directories (`.workgraph/agents/<id>/`) when purging |
| `--threshold <MINUTES>` | Override heartbeat timeout threshold in minutes |

**Examples:**
```bash
wg dead-agents
# Check for dead agents (default behavior)

wg dead-agents --cleanup --threshold 10
# Mark agents dead if no heartbeat for 10 minutes, unclaim their tasks

wg dead-agents --processes
# Check if agent PIDs are still alive in the OS

wg dead-agents --remove
# Remove all dead agents from the registry

wg dead-agents --purge
# Purge dead/done/failed agents from registry

wg dead-agents --purge --delete-dirs
# Purge agents and also delete their work directories
```

---

### `wg checkpoint`

Save a checkpoint for context preservation during long-running tasks.

```bash
wg checkpoint <TASK> --summary <SUMMARY> [OPTIONS]
```

**Arguments:**
- `TASK` - Task ID (required)

**Options:**
| Option | Description |
|--------|-------------|
| `-s, --summary <SUMMARY>` | Summary of progress (~500 tokens) (required) |
| `--agent <AGENT>` | Agent ID (default: `WG_AGENT_ID` env var or task assignee) |
| `-f, --file <FILES>` | Files modified since last checkpoint (repeatable) |
| `--stream-offset <OFFSET>` | Stream byte offset |
| `--turn-count <N>` | Conversation turn count |
| `--token-input <N>` | Input tokens used |
| `--token-output <N>` | Output tokens used |
| `--checkpoint-type <TYPE>` | Checkpoint type: `explicit` (default) or `auto` |
| `--list` | List checkpoints instead of creating one |

**Examples:**
```bash
wg checkpoint my-task --summary "Completed auth module, starting API routes"
# Save a progress checkpoint

wg checkpoint my-task --summary "Phase 2 done" -f src/api.rs -f src/auth.rs
# Checkpoint with modified file tracking

wg checkpoint my-task --summary "Midway" --token-input 50000 --token-output 8000
# Checkpoint with token usage metrics

wg checkpoint my-task --list
# List all checkpoints for a task
```

---

## Peer Commands

Manage peer workgraph instances for cross-repo communication and function sharing.

### `wg peer add`

Register a peer workgraph instance.

```bash
wg peer add <NAME> <PATH> [-d <DESCRIPTION>]
```

**Arguments:**
- `NAME` — Peer name (used as shorthand reference)
- `PATH` — Path to the peer project (containing `.workgraph/`)

**Options:**
| Option | Description |
|--------|-------------|
| `-d, --description <TEXT>` | Description of this peer |

**Example:**
```bash
wg peer add alice /home/alice/project -d "Alice's frontend repo"
```

---

### `wg peer remove`

Remove a registered peer.

```bash
wg peer remove <NAME>
```

**Example:**
```bash
wg peer remove alice
```

---

### `wg peer list`

List all configured peers with service status.

```bash
wg peer list
```

---

### `wg peer show`

Show detailed info about a peer.

```bash
wg peer show <NAME>
```

---

### `wg peer status`

Quick health check of all peers.

```bash
wg peer status
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
| `--socket <PATH>` | Unix socket path (default: `.workgraph/service/daemon.sock`) |
| `--max-agents <N>` | Max parallel agents (overrides config) |
| `--executor <NAME>` | Executor for spawned agents (overrides config) |
| `--interval <SECS>` | Background poll interval in seconds (overrides config) |
| `--model <MODEL>` | Model for spawned agents (overrides config) |
| `--force` | Kill existing daemon before starting (prevents stacked daemons) |
| `--no-coordinator-agent` | Disable the persistent coordinator agent (LLM chat session) |

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

### `wg service restart`

Restart the service daemon (graceful stop then start).

```bash
wg service restart
```

**Example:**
```bash
wg service restart
# Gracefully stops the daemon and starts it again with the same config
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

### `wg service create-coordinator`

Create a new coordinator session.

```bash
wg service create-coordinator [--name <NAME>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--name <NAME>` | Optional name for the coordinator |

**Example:**
```bash
wg service create-coordinator --name "release-v2"
# Creates a new coordinator session with the given name
```

---

### `wg service delete-coordinator`

Delete a coordinator session.

```bash
wg service delete-coordinator <ID>
```

**Arguments:**
- `ID` — Coordinator ID to delete (required)

**Example:**
```bash
wg service delete-coordinator 2
# Permanently removes coordinator session 2
```

---

### `wg service archive-coordinator`

Archive a coordinator session (mark as Done).

```bash
wg service archive-coordinator <ID>
```

**Arguments:**
- `ID` — Coordinator ID to archive (required)

**Example:**
```bash
wg service archive-coordinator 1
# Marks coordinator 1 as Done — preserved in history but no longer active
```

---

### `wg service stop-coordinator`

Stop a coordinator session (kill agent, reset to Open).

```bash
wg service stop-coordinator <ID>
```

**Arguments:**
- `ID` — Coordinator ID to stop (required)

**Example:**
```bash
wg service stop-coordinator 1
# Kills the coordinator agent and resets the session to Open
```

---

## Monitoring Commands

### `wg watch`

Stream workgraph events as JSON lines. Useful for live monitoring, external dashboards, or piping into other tools.

```bash
wg watch [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--event <TYPE>` | Filter by event type (repeatable): `task_state`, `evaluation`, `agent`, `all` (default: `all`) |
| `--task <TASK>` | Filter events to a specific task ID (prefix match) |
| `--replay <N>` | Include N most recent historical events before streaming (default: 0) |

**Examples:**
```bash
wg watch
# Stream all events as JSON lines

wg watch --event task_state --event evaluation
# Only task state changes and evaluations

wg watch --task deploy --replay 20
# Stream events for tasks matching "deploy", including 20 historical events
```

---

### `wg stats`

Show time counters and agent statistics.

```bash
wg stats
```

**Example:**
```bash
wg stats
# Displays agent time counters, task throughput, and resource usage
```

---

## Communication Commands

### `wg msg`

Send and receive messages to/from tasks and agents.

```bash
wg msg <COMMAND>
```

**Subcommands:**

#### `wg msg send`

Send a message to a task/agent.

```bash
wg msg send <TASK_ID> [MESSAGE] [OPTIONS]
```

**Arguments:**
- `TASK_ID` - Task ID (required)
- `MESSAGE` - Message body (optional if `--stdin` is used)

**Options:**
| Option | Description |
|--------|-------------|
| `--from <FROM>` | Sender identifier (default: `user`) |
| `--priority <PRIORITY>` | Message priority: `normal` or `urgent` (default: `normal`) |
| `--stdin` | Read message body from stdin |

**Examples:**
```bash
wg msg send my-task "Please also update the README"
# Send a message to a task

wg msg send my-task "Urgent: API key rotated" --priority urgent
# Send an urgent message

echo "Long feedback..." | wg msg send my-task --stdin --from reviewer
# Pipe message from stdin
```

---

#### `wg msg list`

List all messages for a task.

```bash
wg msg list <TASK_ID>
```

**Example:**
```bash
wg msg list my-task
# Show all messages associated with the task
```

---

#### `wg msg read`

Read unread messages (marks as read, advances cursor).

```bash
wg msg read <TASK_ID> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--agent <AGENT>` | Agent ID (default: from `WG_AGENT_ID` env var, or `user`) |

**Example:**
```bash
wg msg read my-task --agent agent-1234
# Read unread messages for this agent on the task
```

---

#### `wg msg poll`

Poll for new messages (exit code 0 = new messages, 1 = none).

```bash
wg msg poll <TASK_ID> [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--agent <AGENT>` | Agent ID (default: from `WG_AGENT_ID` env var, or `user`) |

**Example:**
```bash
wg msg poll my-task --agent agent-1234
# Check if new messages exist (useful in scripts)
```

---

### `wg chat`

Chat with the coordinator agent.

```bash
wg chat [OPTIONS] [MESSAGE]
```

**Arguments:**
- `MESSAGE` - Message to send (omit for interactive mode)

**Options:**
| Option | Description |
|--------|-------------|
| `-i, --interactive` | Interactive REPL mode |
| `--history` | Show chat history |
| `--clear` | Clear chat history |
| `--timeout <TIMEOUT>` | Timeout in seconds waiting for response (default: 120) |
| `--attachment <ATTACHMENT>` | Attach a file (copied to `.workgraph/attachments/`) |
| `--coordinator <ID>` | Target coordinator ID (default: 0) — for multi-coordinator setups |

**Examples:**
```bash
wg chat "What tasks are blocked?"
# Send a one-shot message to the coordinator

wg chat -i
# Start an interactive chat session

wg chat --history
# View previous chat messages

wg chat "Review this file" --attachment src/main.rs
# Send a message with a file attachment
```

---

## Model and Endpoint Management

See [docs/models.md](models.md) for the full guide including architecture, security model, and common configurations.

### `wg model`

Model registry and routing management.

```bash
wg model <SUBCOMMAND>
```

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `list` | Show all models in the registry (built-in + user-defined) |
| `add <ALIAS>` | Add or update a model in the config registry |
| `remove <ALIAS>` | Remove a model from the config registry |
| `set-default <ALIAS>` | Set the default model for agent dispatch |
| `routing` | Show per-role model routing configuration |
| `set <ROLE> <MODEL>` | Set the model for a specific dispatch role |

#### `wg model list`

```bash
wg model list [--tier <TIER>]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--tier <TIER>` | Filter by tier: `fast`, `standard`, `premium` |

#### `wg model add`

```bash
wg model add <ALIAS> --provider <PROVIDER> [OPTIONS]
```

**Arguments:**
- `ALIAS` — Short alias for the model (e.g., `gpt-4o`, `claude-via-openrouter`)

**Options:**
| Option | Description |
|--------|-------------|
| `--provider <PROVIDER>` | Provider: `anthropic`, `openai`, `openrouter`, `local` (required) |
| `--model-id <ID>` | Full API model identifier (defaults to alias if omitted) |
| `--tier <TIER>` | Quality tier: `fast`, `standard`, `premium` (default: `standard`) |
| `--endpoint <NAME>` | Named endpoint to use for this model |
| `--context-window <N>` | Context window in tokens |
| `--cost-in <N>` | Cost per million input tokens (USD) |
| `--cost-out <N>` | Cost per million output tokens (USD) |
| `--global` | Write to global config (`~/.workgraph/config.toml`) |

#### `wg model remove`

```bash
wg model remove <ALIAS> [--force] [--global]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--force` | Skip confirmation for entries referenced by roles |
| `--global` | Write to global config |

#### `wg model set-default`

```bash
wg model set-default <ALIAS> [--global]
```

Sets the default model for agent dispatch. The alias must exist in the registry.

#### `wg model routing`

```bash
wg model routing
```

Show per-role model routing configuration.

#### `wg model set`

```bash
wg model set <ROLE> <MODEL> [OPTIONS]
```

**Arguments:**
- `ROLE` — Role name (e.g., `default`, `evaluator`, `triage`, `compactor`)
- `MODEL` — Model alias or ID

**Options:**
| Option | Description |
|--------|-------------|
| `--provider <PROVIDER>` | Also set provider for this role |
| `--endpoint <ENDPOINT>` | Also set endpoint for this role |
| `--tier <TIER>` | Set tier override instead of direct model |
| `--global` | Write to global config |

**Examples:**
```bash
# List all models
wg model list

# Filter by tier
wg model list --tier premium

# Add a model
wg model add gpt-4o --provider openai --tier standard --cost-in 2.5 --cost-out 10.0

# Set the default model
wg model set-default sonnet

# Show routing
wg model routing

# Set evaluator to use opus
wg model set evaluator opus

# Set triage to use haiku via openrouter
wg model set triage haiku --provider openrouter
```

---

### `wg key`

Manage API keys for LLM providers.

```bash
wg key <SUBCOMMAND>
```

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `set <PROVIDER>` | Configure an API key for a provider |
| `check [PROVIDER]` | Validate API key availability and status |
| `list` | Show key configuration status for all providers |

#### `wg key set`

```bash
wg key set <PROVIDER> [OPTIONS]
```

**Arguments:**
- `PROVIDER` — Provider name (e.g., `openrouter`, `anthropic`, `openai`)

**Options:**
| Option | Description |
|--------|-------------|
| `--env <VAR>` | Reference an environment variable by name |
| `--file <PATH>` | Path to a file containing the key |
| `--value <VALUE>` | Store key value directly (written to `~/.workgraph/keys/<provider>.key`, NOT to config) |
| `--global` | Apply to global config (`~/.workgraph/config.toml`) |

#### `wg key check`

```bash
wg key check [PROVIDER]
```

Validate API key availability and status. Omit provider to check all.

#### `wg key list`

```bash
wg key list
```

Show key configuration status for all providers.

**Examples:**
```bash
# Set a key from a file
wg key set openrouter --file ~/.secrets/openrouter.key

# Set a key from an environment variable
wg key set anthropic --env ANTHROPIC_API_KEY

# Store a key value directly
wg key set openai --value sk-abc123...

# Check all keys
wg key check

# Check a specific provider
wg key check openrouter

# Show key status
wg key list
```

---

### `wg models`

Browse and search available models.

```bash
wg models <SUBCOMMAND>
```

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `list` | List models from the local registry |
| `search <QUERY>` | Search models from OpenRouter by name, ID, or description |
| `remote` | List all models available on OpenRouter |
| `add <ID>` | Add a custom model to the local registry |
| `set-default <ID>` | Set the default model |
| `init` | Initialize models.yaml with defaults |

**Examples:**

```bash
# List all local models
wg models list

# Filter by tier
wg models list --tier frontier

# Search OpenRouter for Claude models
wg models search claude

# Search for tool-capable models only
wg models search gemini --tools

# Add a custom model
wg models add "custom/my-model" --cost-in 1.0 --cost-out 5.0 --tier mid

# Set default
wg models set-default "anthropic/claude-sonnet-4-6"
```

---

### `wg endpoints`

Manage LLM endpoints (connection targets with URL + auth).

```bash
wg endpoints <SUBCOMMAND>
```

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `add <NAME>` | Add a new endpoint |
| `list` | List all configured endpoints |
| `remove <NAME>` | Remove an endpoint by name |
| `set-default <NAME>` | Set an endpoint as the default |
| `test <NAME>` | Test endpoint connectivity |

**Examples:**

```bash
# Add an OpenRouter endpoint
wg endpoints add openrouter --provider openrouter --default

# Add with a key file
wg endpoints add anthropic --provider anthropic --api-key-file ~/.secrets/anthropic.key

# Add a local Ollama endpoint
wg endpoints add ollama --provider local --url http://localhost:11434/v1

# List endpoints
wg endpoints list

# Test connectivity
wg endpoints test openrouter

# Remove an endpoint
wg endpoints remove openai

# Add to global config
wg endpoints add openrouter --provider openrouter --global
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
wg viz [OPTIONS] [TASK_ID]...
```

**Arguments:**
- `[TASK_ID]...` - Task IDs to focus on — shows only their containing subgraphs

**Options:**
| Option | Description |
|--------|-------------|
| `--all` | Include done tasks (default: only open tasks) |
| `--status <STATUS>` | Filter by status (open, in-progress, done, blocked) |
| `--critical-path` | Highlight the critical path in red |
| `--dot` | Output Graphviz DOT format |
| `--mermaid` | Output Mermaid diagram format |
| `--graph` | Output 2D spatial graph with box-drawing characters |
| `-o, --output <FILE>` | Render directly to file (requires graphviz) |
| `--show-internal` | Show internal tasks (`assign-*`, `evaluate-*`) normally hidden |
| `--tui` | Launch interactive TUI mode instead of static output |
| `--no-tui` | Force static output even when stdout is an interactive terminal |
| `--no-mouse` | Disable mouse capture in TUI mode (useful in tmux) |
| `--layout <LAYOUT>` | Layout strategy: `diamond` (default, fan-in nodes under common ancestor) or `tree` (classic DFS order) |
| `--tag <TAG>` | Filter by tag (repeatable, multiple `--tag` flags use AND semantics) |
| `--edge-color <STYLE>` | Edge color style: `gray` (default), `white`, or `mixed` (tree=white, arcs=gray) |

**Examples:**
```bash
wg viz
# ASCII dependency tree of active tasks

wg viz --all
# Include completed tasks

wg viz my-task other-task
# Show only subgraphs containing these tasks

wg viz --dot
# Graphviz DOT output

wg viz --mermaid
# Mermaid diagram output

wg viz --dot -o graph.png
# Render to PNG file (requires graphviz)

wg viz --critical-path
# Highlight the longest dependency chain

wg viz --no-tui
# Force static output (useful in scripts or when piping)
```

---

### `wg archive`

Archive completed tasks to a separate file.

```bash
wg archive [OPTIONS] [IDS]...
```

**Arguments:**
- `[IDS]...` - Specific task IDs to archive (optional; archives all eligible tasks if omitted)

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `search` | Search archived tasks by title, description, and tags |
| `restore` | Restore an archived task back into the active graph |

**Options:**
| Option | Description |
|--------|-------------|
| `--dry-run` | Show what would be archived without archiving |
| `--older <DURATION>` | Only archive tasks older than this (e.g., 30d, 7d, 1w) |
| `--list` | List already-archived tasks instead of archiving |
| `-y, --yes` | Skip confirmation prompt for bulk archive operations |
| `--undo` | Undo the last archive operation (restore all tasks from the last batch) |

**Examples:**
```bash
wg archive --dry-run
# Preview which tasks would be archived

wg archive --older 30d
# Archive tasks completed more than 30 days ago

wg archive --list
# Show previously archived tasks

wg archive my-task-1 my-task-2
# Archive specific tasks

wg archive --undo
# Undo the last archive operation

wg archive search "auth"
# Search archived tasks

wg archive restore my-old-task
# Restore an archived task back into the active graph
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
| `--global` | Target global config (`~/.workgraph/config.toml`) instead of local |
| `--local` | Explicitly target local config (default for writes) |
| `--list` | Show merged config with source annotations (global/local/default) |
| `--executor <NAME>` | Set executor (claude, amplifier, shell, or custom config name) |
| `--model <MODEL>` | Set agent model |
| `--set-interval <SECS>` | Set agent sleep interval |
| `--max-agents <N>` | Set coordinator max agents |
| `--coordinator-interval <SECS>` | Set coordinator tick interval |
| `--poll-interval <SECS>` | Set service daemon background poll interval |
| `--coordinator-executor <NAME>` | Set coordinator executor |
| `--max-coordinators <N>` | Set max concurrent coordinator agents (LLM sessions). Default: 4 |
| `--auto-evaluate <BOOL>` | Enable/disable automatic evaluation |
| `--auto-assign <BOOL>` | Enable/disable automatic identity assignment |
| `--auto-place <BOOL>` | Enable/disable automatic placement analysis on new tasks |
| `--auto-create <BOOL>` | Enable/disable automatic creator agent invocation |
| `--assigner-model <MODEL>` | Set model for assigner agents |
| `--evaluator-model <MODEL>` | Set model for evaluator agents |
| `--evolver-model <MODEL>` | Set model for evolver agents |
| `--assigner-agent <HASH>` | Set assigner agent (content-hash) |
| `--evaluator-agent <HASH>` | Set evaluator agent (content-hash) |
| `--evolver-agent <HASH>` | Set evolver agent (content-hash) |
| `--creator-agent <HASH>` | Set creator agent (content-hash) |
| `--creator-model <MODEL>` | Set model for creator agents |
| `--retention-heuristics <TEXT>` | Set retention heuristics (prose policy for evolver) |
| `--max-child-tasks <N>` | Max tasks a single agent can create per execution (default: 10) |
| `--max-task-depth <N>` | Max depth of task dependency chains from root (default: 8) |
| `--auto-triage <BOOL>` | Enable/disable automatic triage of dead agents |
| `--triage-model <MODEL>` | Set model for triage (default: haiku) |
| `--triage-timeout <SECS>` | Set timeout for triage calls (default: 30) |
| `--triage-max-log-bytes <N>` | Set max bytes for triage log reading (default: 50000) |
| `--eval-gate-threshold <N>` | Set evaluation gate threshold (0.0–1.0). Evaluations below this score reject the original task. Only applies to tasks tagged `eval-gate` unless `--eval-gate-all` is set |
| `--eval-gate-all <BOOL>` | Apply eval gate to ALL evaluated tasks, not just those tagged `eval-gate` |
| `--flip-enabled <BOOL>` | Enable or disable FLIP (roundtrip intent fidelity) evaluation |
| `--flip-inference-model <MODEL>` | Model for FLIP inference phase (reconstructing prompt from output) |
| `--flip-comparison-model <MODEL>` | Model for FLIP comparison phase (scoring similarity) |
| `--flip-verification-threshold <N>` | FLIP score threshold for triggering verification (default: 0.7) |
| `--flip-verification-model <MODEL>` | Model for FLIP-triggered verification agents (default: opus) |
| `--chat-history <BOOL>` | Enable/disable chat history persistence across TUI restarts |
| `--chat-history-max <N>` | Maximum number of chat messages to persist (default: 1000) |
| `--tui-counters <LIST>` | TUI time counters (comma-separated: `uptime`, `cumulative`, `active`, `session`) |
| `--retry-context-tokens <N>` | Max tokens of previous-attempt context to inject on retry (default: 2000, 0 = disabled) |
| `--viz-edge-color <STYLE>` | Viz edge color style: `gray` (default), `white`, or `mixed` |
| `--install-global` | Install project config as global default (`~/.workgraph/config.toml`) |
| `--force` | Skip confirmation when overwriting existing global config |
| `--homeserver <URL>` | Set Matrix homeserver URL |
| `--username <USER>` | Set Matrix username |
| `--password <PASS>` | Set Matrix password |
| `--access-token <TOKEN>` | Set Matrix access token |
| `--room <ROOM>` | Set Matrix default room |
| `--models` | Show all model routing assignments (per-role model+provider) |
| `--set-model <ROLE> <MODEL>` | Set model for a dispatch role |
| `--set-provider <ROLE> <PROVIDER>` | Set provider for a dispatch role |
| `--set-endpoint <ROLE> <ENDPOINT>` | Bind a named endpoint to a dispatch role |
| `--role-model <ROLE=MODEL>` | Set model for a role (key=value syntax) |
| `--role-provider <ROLE=PROVIDER>` | Set provider for a role (key=value syntax) |
| `--registry` | Show all model registry entries (built-in + user-defined) |
| `--registry-add` | Add a model to the registry (use with `--id`, `--provider`, `--reg-model`, `--reg-tier`, `--endpoint`, `--context-window`, `--cost-input`, `--cost-output`) |
| `--registry-remove <ID>` | Remove a model from the registry |
| `--tiers` | Show current tier→model assignments |
| `--tier <TIER=MODEL_ID>` | Set which model a tier uses (e.g., `--tier standard=gpt-4o`) |
| `--set-key <PROVIDER>` | Set API key file for a provider (use with `--file`) |
| `--check-key` | Check OpenRouter API key validity and credit status |

**Examples:**

```bash
# View config
wg config

# Show merged config with source annotations
wg config --list

# Set executor and model
wg config --executor claude --model opus

# Set a global default (applies to all projects)
wg config --global --model sonnet

# Enable the full agency automation loop
wg config --auto-evaluate true --auto-assign true

# Set per-role model overrides
wg config --assigner-model haiku --evaluator-model opus --evolver-model opus

# Model routing: show and set per-role model assignments
wg config --models
wg config --set-model evaluator sonnet
wg config --set-model triage haiku
wg config --role-model evaluator=sonnet

# Tier management
wg config --tiers
wg config --tier fast=haiku
wg config --tier standard=sonnet

# Model registry
wg config --registry
wg config --registry-add --id gpt-4o --provider openai --reg-model gpt-4o --reg-tier standard

# API key management
wg config --set-key openrouter --file ~/.secrets/openrouter.key
wg config --check-key

# Eval gate and FLIP
wg config --eval-gate-threshold 0.7 --eval-gate-all true
wg config --flip-enabled true --flip-verification-threshold 0.7

# Automation flags
wg config --auto-place true --auto-create true

# Multi-coordinator
wg config --max-coordinators 6

# Chat history
wg config --chat-history true --chat-history-max 500

# Install project config as global default
wg config --install-global

# Matrix integration
wg config --homeserver https://matrix.example.com --username bot --room '#ops:example.com'
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

### `wg setup`

Interactive configuration wizard for first-time setup. Walks through executor, model, agency, and service configuration.

```bash
wg setup
```

**Example:**
```bash
wg setup
# Launches interactive prompts to configure your workgraph project
```

---

### `wg replay`

Replay tasks: snapshot the current graph, selectively reset tasks, and re-execute with a different model.

```bash
wg replay [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--model <MODEL>` | Model to use for replayed tasks |
| `--failed-only` | Only reset failed/abandoned tasks |
| `--below-score <SCORE>` | Only reset tasks with evaluation score below this threshold |
| `--tasks <IDS>` | Reset specific tasks (comma-separated) plus their transitive dependents |
| `--keep-done <SCORE>` | Preserve done tasks scoring above this threshold (default: 0.9) |
| `--plan-only` | Dry run: show what would be reset without making changes |
| `--subgraph <TASK>` | Only replay tasks in this subgraph (rooted at given task) |

**Examples:**
```bash
wg replay --failed-only --model opus
# Re-run all failed tasks with Opus

wg replay --below-score 0.7 --model sonnet
# Reset tasks scoring below 0.7 and replay with Sonnet

wg replay --tasks auth-impl,auth-test --plan-only
# Preview which tasks would be reset

wg replay --subgraph deploy-pipeline --failed-only
# Only replay failed tasks under the deploy-pipeline subtree
```

---

### `wg runs`

Manage run snapshots — saved states of the graph for comparison and rollback.

```bash
wg runs <SUBCOMMAND>
```

**Subcommands:**
| Subcommand | Description |
|------------|-------------|
| `list` | List all run snapshots |
| `show <ID>` | Show details of a specific run |
| `restore <ID>` | Restore graph from a run snapshot |
| `diff <ID>` | Diff current graph against a run snapshot |

**Examples:**
```bash
wg runs list
# List all saved snapshots

wg runs show run-001
# Show details of a specific snapshot

wg runs diff run-001
# Compare current graph state against the snapshot

wg runs restore run-001
# Restore the graph to the snapshot state
```

---

### `wg gc`

Garbage collect terminal tasks (failed, abandoned) from the graph.

```bash
wg gc [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--dry-run` | Show what would be removed without actually removing |
| `--include-done` | Also remove done tasks (by default only failed + abandoned) |
| `--older <DURATION>` | Only remove tasks older than this duration (e.g., `30d`, `7d`, `1w`, `24h`) |

**Examples:**
```bash
wg gc --dry-run
# Preview which tasks would be removed

wg gc
# Remove all failed and abandoned tasks

wg gc --include-done
# Also remove completed tasks

wg gc --older 30d
# Only remove tasks older than 30 days
```

---

### `wg compact`

Compact: distill graph state into context.md.

```bash
wg compact
```

**Example:**
```bash
wg compact
# Distills current graph state into .workgraph/context.md for context preservation
```

---

### `wg sweep`

Detect and recover orphaned in-progress tasks with dead agents.

```bash
wg sweep [--dry-run]
```

Sweep detects in-progress tasks whose assigned agent has died, been marked Dead, or is missing from the registry. It resets them to Open so the coordinator can re-dispatch. Safe to run anytime — it is idempotent.

**Options:**
| Option | Description |
|--------|-------------|
| `--dry-run` | Only report orphaned tasks, don't fix them |

**Examples:**
```bash
wg sweep --dry-run
# Preview orphaned tasks without fixing them

wg sweep
# Reset all orphaned in-progress tasks to Open
```

---

### `wg telegram`

Telegram integration commands.

```bash
wg telegram <COMMAND>
```

**Subcommands:**

#### `wg telegram listen`

Start the Telegram bot listener.

```bash
wg telegram listen [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--chat-id <CHAT_ID>` | Telegram chat ID to listen in (uses configured chat_id if not specified) |

**Example:**
```bash
wg telegram listen
# Start listening for Telegram messages using configured chat ID

wg telegram listen --chat-id 123456789
# Listen on a specific chat
```

---

#### `wg telegram send`

Send a message to the configured Telegram chat.

```bash
wg telegram send <MESSAGE> [OPTIONS]
```

**Arguments:**
- `MESSAGE` - Message to send (required)

**Options:**
| Option | Description |
|--------|-------------|
| `--chat-id <CHAT_ID>` | Target chat ID (uses configured chat_id if not specified) |

**Example:**
```bash
wg telegram send "Deploy complete — all tests passing"
# Send a notification to the configured Telegram chat

wg telegram send "Alert: build failed" --chat-id 123456789
# Send to a specific chat
```

---

#### `wg telegram status`

Show Telegram configuration status.

```bash
wg telegram status
```

**Example:**
```bash
wg telegram status
# Shows whether Telegram is configured and the current chat ID
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
