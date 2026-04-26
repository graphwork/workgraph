# Workgraph Agent Guide

How spawned agents should think about task graphs: recognizing patterns, building structures, staffing work, and staying in control.

**Reference:** The canonical pattern vocabulary is defined in [spec-patterns-vocab](design/spec-patterns-vocab.md). This guide teaches you how to apply it.

---

## 1. Pattern Recognition

When a human (or a planner task) says one of these words, you should know exactly what graph shape to build.

| Word | Graph shape | Key property |
|------|-------------|--------------|
| **pipeline** | `A → B → C → D` | Sequential handoffs; throughput = slowest stage |
| **diamond** | `A → [B,C,D] → E` | Fork-join; parallel workers, single integrator |
| **scatter-gather** | Like diamond, but workers assess the *same* input from different perspectives | Heterogeneous reviewers |
| **loop** | `A → B → C → A` (back-edge) | Iterate until `--converged` or `--max-iterations` hit |
| **map-reduce** | Data-parallel diamond: planner → N workers → reducer | Workers do the same kind of work on different data |
| **scaffold** | Platform task → [stream tasks…] | Shared infrastructure first, then parallel features |
| **diamond with specialists** | Planner forks to role-matched workers, synthesizer joins | The power pattern — combines structure + agency |
| **chatroom** | `prep → [perspectives…] → discussion → synthesis` | Deliberation via messages; perspectives interact |

If the request doesn't map to one of these, it's probably a **composition** — see §6.

---

## 1b. Task Lifecycle

Every task moves through a state machine. Understanding the states and transitions is essential for working effectively with the graph.

### Status states

| Status | Meaning |
|--------|---------|
| **Open** | Ready to be claimed/dispatched (all `--after` deps are done) |
| **Blocked** | Waiting for upstream dependencies to complete |
| **InProgress** | An agent has claimed the task and is working on it |
| **PendingValidation** | Agent called `wg done`, but the task has a `--verify` criterion requiring external validation |
| **Done** | Completed successfully |
| **Failed** | Agent called `wg fail` or validation failed |
| **Abandoned** | Manually abandoned via `wg abandon` |
| **Waiting** | Paused — waiting for an external event or manual intervention |

### Validation flow (PendingValidation)

Tasks created with `--verify` go through an extra validation gate:

```
InProgress → wg done → PendingValidation → wg approve → Done
                                          → wg reject  → Open (re-dispatched)
```

- `wg approve <task-id>` — transitions PendingValidation → Done
- `wg reject <task-id> --reason "..."` — reopens the task for re-dispatch (clears assignment)
- After `max_rejections` (default: 3), `wg reject` transitions the task to Failed instead of Open

### Retry workflow

Failed tasks can be retried:

```bash
wg retry <task-id>    # Failed → Open (clears assignment, preserves logs)
```

- Retry count is tracked (`retry_count`). Set `max_retries` to limit attempts.
- Previous failure reasons and logs are preserved for the next agent to learn from.
- The dispatcher re-dispatches the task automatically after retry.

### Cascade abandon

When a task is abandoned (`wg abandon`), its **system tasks** (IDs starting with `.`, such as `.evaluate-*` and `.verify-*`) are automatically cascade-abandoned. Normal dependent tasks are NOT cascade-abandoned — they remain blocked until the situation is resolved.

### Task supersession

When abandoning a task that's been replaced by a better approach:

```bash
wg abandon old-task --superseded-by new-task-a,new-task-b --reason "Replaced with better approach"
```

The `superseded_by` field creates a traceable link from the old task to its replacements.

### Placement flow

When `auto_place` is enabled (`wg config --auto-place true`), placement analysis is merged into the assignment step. Rather than creating separate `.place-*` tasks, the dispatcher performs placement analysis inline when building `.assign-*` tasks for ready unassigned work:

```
wg add "New task" → Open state → dispatcher creates .assign-<task-id>
                                → assignment agent analyzes graph context
                                  (including placement when auto_place is on)
                                → determines optimal dependencies, wiring,
                                  and agent assignment
                                → task is assigned and ready for dispatch
```

The assignment agent examines the current graph structure and the task's description to decide:
- Which existing tasks should be `--after` dependencies (when auto_place is on)
- The best agent identity to assign
- Whether the task needs specific context scope or exec mode

If `auto_place` is disabled, tasks are added directly in Open state and the user must wire dependencies manually with `--after`.

You can also control placement via CLI flags on `wg add`:
- `--no-place` — skip automatic placement entirely; make the task immediately available
- `--place-near <IDS>` — hint: place near these tasks (comma-separated)
- `--place-before <IDS>` — hint: place before these tasks (comma-separated)

---

## 2. Structure Rules

Structure patterns describe how to arrange tasks in the graph.

### 2.1 Pipeline — sequential stages

Use when work requires ordered transformations: design → build → test → ship.

```bash
wg add "Design API" --id design
wg add "Implement API" --id implement --after design
wg add "Review API" --id review --after implement
wg add "Deploy API" --id deploy --after review
```

**Rule:** If stages share files, they *must* be sequential. A pipeline is the safe default when you're unsure about file overlap.

### 2.2 Diamond — parallel workers with integrator

Use when work decomposes into independent units that touch **disjoint files**.

```bash
wg add "Plan the work" --id planner
wg add "Implement module A" --id worker-a --after planner
wg add "Implement module B" --id worker-b --after planner
wg add "Implement module C" --id worker-c --after planner
wg add "Integrate results" --id synthesizer --after worker-a,worker-b,worker-c
```

**Rules:**
- Each worker must own distinct files. If worker-a and worker-b both edit `src/main.rs`, one will overwrite the other. Serialize them instead.
- **Always have an integrator task at the join point.** The synthesizer resolves conflicts and produces a coherent whole. Forgetting this is an anti-pattern (§5).
- The planner should explicitly list each worker's file scope in its task description.

### 2.3 Scatter-Gather — multiple perspectives

Like a diamond, but workers examine the *same* artifact from different angles rather than building parts of a whole.

```bash
wg add "Produce artifact" --id artifact
wg add "Security review" --id sec-review --after artifact
wg add "Performance review" --id perf-review --after artifact
wg add "UX review" --id ux-review --after artifact
wg add "Summarize reviews" --id summary --after sec-review,perf-review,ux-review
```

### 2.4 Chat Room — deliberation via messages

Use when a question needs multiple perspectives that interact with each other, not just independent reviews. Agents discuss via `wg msg` on a shared task.

**Three phases:**

**1. Preparation** — research tasks run in parallel:
```bash
wg add "Research: prior art" --id prep-prior-art
wg add "Research: user impact" --id prep-user-impact
wg add "Research: technical constraints" --id prep-tech
```

**2. Discussion** — a single task where agents post positions via messages:
```bash
wg add "Discussion: which approach to use?" --id discuss-approach \
  --after prep-prior-art,prep-user-impact,prep-tech \
  -d "Chat room discussion. Participants read prep artifacts via wg context,
then post positions via: wg msg send discuss-approach 'My position: ...'
Read and respond to others' messages before marking done."
```

**3. Synthesis** — extract a decision from the discussion:
```bash
wg add "Synthesize decision" --id synth-approach \
  --after discuss-approach \
  -d "Read discussion messages (wg msg list discuss-approach).
Produce: decision + rationale + dissenting views + action items."
```

**Rules:**
- **Preparation is not optional.** Agents without research produce shallow positions. Each prep task should produce a structured findings document.
- **5-8 participants** is the sweet spot. Fewer loses diversity; more creates noise.
- **The synthesis task is mandatory.** Without it, the discussion is interesting but not actionable.
- **Messages, not artifacts.** Unlike scatter-gather (independent artifacts), chat room agents interact through `wg msg send` / `wg msg list` on the discussion task.

**How participants interact:**
```bash
# Read preparation context
wg context discuss-approach

# Read what others have said
wg msg list discuss-approach

# Post your position
wg msg send discuss-approach "Position: X because Y. Re @agent-3's concern about Z: ..."
```

**When to use chatroom vs. scatter-gather:**
- **Scatter-gather:** independent assessments that don't need to interact (security review + perf review + UX review)
- **Chat room:** positions that should respond to each other (design decisions, strategy, prioritization)

### 2.5 Loop — iterate until convergence

Use for iterative refinement: draft → review → revise, repeat.

```bash
# The back-edge (--after revise) creates the cycle
wg add "Write draft" --id write --after revise --max-iterations 5
wg add "Review draft" --id review --after write
wg add "Revise draft" --id revise --after review
```

**Rules:**
- Always set `--max-iterations` on the cycle header. Unbounded loops are an anti-pattern.
- **Always have a refine task that can loop back.** The revise node is what feeds improvements back into the next iteration.
- Use `wg done review --converged` to stop the loop when work has stabilized.
- Use plain `wg done review` to let the cycle continue iterating.
- Any cycle member can signal convergence — it stops the entire cycle.

**Additional cycle flags:**
- `--no-converge` — force all iterations to run (agents cannot signal convergence early)
- `--no-restart-on-failure` — disable automatic cycle restart when a member fails (restart is on by default)
- `--max-failure-restarts <N>` — cap the number of failure-triggered cycle restarts (default: 3)

**Checking iteration state:**
```bash
wg show <task-id>    # shows loop_iteration and cycle membership
wg cycles            # see all cycles and their current state
```

Previous iterations' logs and artifacts are preserved. Review them with `wg log <task-id> --list` and `wg context <task-id>` to build on prior work rather than starting fresh.

### 2.6 The Critical Structural Rule

> **Same files = sequential edges. NEVER parallelize shared-file mutations.**

When deciding between pipeline and diamond, check file overlap:
- **Disjoint files** → safe to fork-join (diamond)
- **Shared files** → must serialize (pipeline)
- **Unsure** → default to pipeline; you can always parallelize later

The synthesizer/integrator is the *only* task that may touch all files, because it runs after all workers finish.

---

## 3. Agency Rules

Agency patterns describe how to staff work — which roles to create and how to match them to tasks.

### 3.1 Planner-Workers-Synthesizer (default for complex work)

One thinker decomposes the problem, many doers execute in parallel, one integrator combines results. This is the default staffing for any diamond structure.

```bash
# Define roles
wg role add "Architect" --outcome "Clean decomposition with non-overlapping boundaries" --skill architecture
wg role add "Implementer" --outcome "Working, tested code for assigned module" --skill coding --skill testing
wg role add "Integrator" --outcome "Cohesive merged output with conflicts resolved" --skill integration

# Create agents from roles
wg agent create "Planner" --role <architect-hash> --tradeoff <tradeoff-hash>
wg agent create "Worker" --role <implementer-hash> --tradeoff <tradeoff-hash>
wg agent create "Synthesizer" --role <integrator-hash> --tradeoff <tradeoff-hash>

# Assign agents to tasks
wg assign planner <planner-agent>
wg assign worker-a <worker-agent>
wg assign synthesizer <synthesizer-agent>
```

### 3.2 Diamond with Specialists — the power pattern

Combine a diamond structure with role-matched workers. Each worker is a specialist in its domain.

```bash
# Structure
wg add "Plan decomposition" --id plan
wg add "Implement auth module" --id auth --after plan
wg add "Implement data layer" --id data --after plan
wg add "Implement UI" --id ui --after plan
wg add "Integrate" --id integrate --after auth,data,ui

# Specialist roles
wg role add "Security Dev" --outcome "Secure auth" --skill security --skill coding
wg role add "Data Dev" --outcome "Efficient data layer" --skill database --skill coding
wg role add "Frontend Dev" --outcome "Responsive UI" --skill frontend --skill coding
wg role add "Integrator" --outcome "Working integrated system" --skill integration

# Route specialists to matching tasks
wg assign auth <security-agent>
wg assign data <data-agent>
wg assign ui <frontend-agent>
wg assign integrate <integrator-agent>
```

### 3.3 Requisite Variety — match roles to task types

**Ashby's Law: you need at least as many distinct roles as you have distinct task types.**

| Violation | Symptom | Fix |
|-----------|---------|-----|
| Too few roles | Low scores on specialized tasks | `wg evolve run --strategy gap-analysis` |
| Too many roles | Roles with zero assignments | `wg evolve run --strategy retirement` |

Check coverage:
```bash
wg agency stats    # role coverage and utilization
wg skill list      # all skill types in the graph
```

### 3.4 Stream-Aligned vs. Specialist

- **Stream-aligned role**: owns an end-to-end flow (auth feature, user onboarding). Minimizes handoffs.
- **Specialist role**: owns a domain across all flows (security, database). Maximizes depth.

Default to stream-aligned. Use specialists when deep domain expertise is required.

### 3.5 Platform (Scaffold)

A platform role produces shared infrastructure that other roles depend on. Platform tasks appear as `--after` dependencies for stream-aligned work.

```bash
wg add "Set up CI pipeline" --id setup-ci
wg add "Implement feature A" --id feature-a --after setup-ci
wg add "Implement feature B" --id feature-b --after setup-ci
```

---

## 4. Control Rules

Control patterns describe how you stay coordinated through the graph as a shared medium.

### 4.1 Stigmergic Coordination — the graph is truth

**Read `wg show` and `wg context`. Don't assume — the graph is the single source of truth.**

Agents coordinate indirectly through the shared graph. Every `wg done`, `wg log`, `wg artifact`, and `wg msg send` call modifies the graph, which stimulates downstream agents and makes your work discoverable.

```bash
# You (Agent A) complete work, leaving traces
wg log implement-api "Implemented REST endpoints in src/api.rs"
wg artifact implement-api src/api.rs
wg done implement-api

# Agent B (on a downstream task) reads the traces
wg context write-tests
# → Shows: From implement-api (done): Artifacts: src/api.rs
```

**Key practices:**
- Write descriptive task titles and descriptions — they are pheromone trails for downstream agents
- Use `wg log` to leave progress traces — they become context for dependent tasks
- Use `wg artifact` to mark outputs — they appear in `wg context` for successors
- Always check `wg context <your-task>` before starting work — it shows what predecessors produced
- **You are expected to create tasks** when you discover work. Bugs, missing docs, needed refactors — create them with `wg add`. The dispatcher dispatches automatically.

### 4.1.1 Discovery — see what's new

At session start, run `wg discover` to see what other agents have recently completed. This gives you awareness of the broader system state beyond your direct dependencies:

```bash
wg discover                          # Last 24h of completions
wg discover --since 7d               # Wider window
wg discover --with-artifacts         # Show artifact paths
wg discover --since 24h --json       # Machine-readable output
```

Output is grouped by tag, showing task ID, title, completion time, artifacts, and the last log entry. Use this to:
- Avoid duplicating work another agent already did
- Find artifacts that may be relevant to your task
- Understand the pace and direction of the project

### 4.1.2 Breadcrumbs — leave trails for future agents

Every log entry and artifact you create is a breadcrumb for future agents. Write them with your successors in mind:

```bash
# Good: specific, actionable, references files
wg log my-task "Implemented auth middleware in src/middleware/auth.rs using JWT validation"
wg log my-task "Design decision: chose HMAC-SHA256 over RSA because tokens are short-lived"
wg artifact my-task src/middleware/auth.rs
wg artifact my-task docs/auth-design.md

# Bad: vague, no context
wg log my-task "Done with auth"
```

Detailed breadcrumbs compound — each agent builds on the last, and the graph accumulates institutional knowledge.

### 4.1.3 Agent-to-Agent Messages

Send messages to tasks being worked on by other agents. This enables real-time coordination for related or overlapping work:

```bash
# Send a message to another task's agent
wg msg send <task-id> "Hey, I found this is related to your work: ..."
wg msg send <task-id> "FYI: I changed the API signature in src/api.rs"
wg msg send <task-id> --from agent-3 "Message with explicit sender"
wg msg send <task-id> --priority urgent "Blocking issue found"

# Check for messages on your task
wg msg list <task-id>

# Read unread messages (advances cursor)
wg msg read <task-id> --agent $WG_AGENT_ID

# Poll without advancing cursor
wg msg poll <task-id> --agent $WG_AGENT_ID
```

**The `--agent` flag and cursor semantics:**

`wg msg read` uses a per-agent read cursor. Each agent has an independent position in the message stream — reading advances *that agent's* cursor without affecting other readers. The `--agent` flag defaults to the `WG_AGENT_ID` environment variable (set automatically for spawned agents), or `"user"` when unset. This means:

- Multiple agents can read the same task's messages independently
- Each agent only sees messages posted *after* its last read
- `wg msg poll` checks for new messages without advancing the cursor (useful for conditional logic)

**When to send messages:**
- You discover your work affects another in-progress task
- You find a bug or pattern relevant to another agent's work
- You need to coordinate shared resource access (e.g., a shared config file)

**Messages are persistent** — they survive agent restarts and are visible to future agents who work on the same task.

**Workflow expectation:** Agents should check for messages at task start, at natural breakpoints during work, and before marking a task done. Unreplied messages at completion time indicate incomplete work.

### 4.1.4 Human-in-the-Loop via User Boards

For scenarios requiring human input or approval, use **user boards** (`wg user`) — per-user conversation spaces that persist across sessions:

```bash
# Initialize a user board (creates .user-<NAME> task)
wg user init                        # defaults to current user ($WG_USER)
wg user init --name alice           # explicit user name

# List all user boards
wg user list

# Archive and rotate a board
wg user archive                     # archives current board, creates successor
```

User boards provide a dedicated channel for human-agent communication. Agents can send messages to the user's board, and the user can respond asynchronously.

**Alternative: `wg wait --until human-input`**

When an agent needs to pause until a human responds, use `wg wait` instead of polling:

```bash
# Park the task until a human posts a message
wg wait <task-id> --until "human-input" --checkpoint "Waiting for approval on design"
```

The dispatcher evaluates waiting conditions each tick and automatically resumes the task when a human message arrives. This is more efficient than polling `wg msg read` in a loop and frees the agent slot for other work while waiting.

### 4.2 After Code Changes: Rebuild

When you modify source code in a Rust project that uses `cargo install`:

```bash
cargo install --path .
```

This updates the global `wg` binary. Forgetting this step is a common source of "why isn't this working" bugs. Do it after every code change.

### 4.3 Worktree Isolation

When the dispatcher spawns agents, each agent works in an isolated [git worktree](WORKTREE-ISOLATION.md). This means:

- Each agent has its own working tree — no shared file conflicts
- Agents can build, test, and commit independently
- Worktrees share the same `.git` object store, so branches are visible across agents
- Worktrees are created under `.wg-worktrees/` and cleaned up after task completion

This eliminates the "parallel file conflict" problem at the git level. However, you should still serialize tasks that modify the same logical files (via `--after`) to avoid merge conflicts at commit time.

### 4.4 Convergence Signaling

Use `wg done --converged` to stop a loop. Use plain `wg done` to iterate.

```bash
# Work is good enough — stop the loop
wg done review --converged

# Work needs another pass — let the cycle continue
wg done review
```

The `--converged` flag tags the cycle header. This prevents further iterations even if `--max-iterations` hasn't been reached. Any cycle member can signal convergence for the entire cycle.

### 4.5 Evolve — the feedback loop

After work accumulates, evaluate and evolve roles:

```bash
# Evaluate completed work
wg evaluate run my-task

# Record an external evaluation (e.g., from a human reviewer)
wg evaluate record --task my-task --score 0.85 --source "manual"

# View evaluation history
wg evaluate show --task my-task

# Preview evolution proposals
wg evolve run --dry-run

# Apply evolution
wg evolve run --strategy all
```

**Single-loop learning:** task failed → retry with different agent (`wg retry`).
**Double-loop learning:** task type keeps failing → change the role itself (`wg evolve run --strategy mutation`).

Escalate from single to double loop when the same task type fails repeatedly.

---

## 5. Anti-Patterns

| Anti-pattern | What goes wrong | Fix |
|--------------|----------------|-----|
| **Parallel file conflict** | Two concurrent tasks edit the same file → one overwrites the other | Serialize with `--after` or decompose so each task owns distinct files |
| **Using built-in TaskCreate** | Built-in task tools are a separate system that does NOT interact with workgraph | Always use `wg` CLI commands (`wg add`, `wg done`, etc.) |
| **Missing integrator** | Diamond with no join point → parallel outputs never get merged | Always add a synthesizer task with `--after worker-a,worker-b,...` |
| **Missing loop-back on refine** | Review identifies issues but there's no path back to fix them | Add a revise task in the cycle with a back-edge to the cycle header |
| **Unbounded loop** | Cycle without `--max-iterations` → runs forever | Always set `--max-iterations` on cycle headers |
| **Monolithic task** | One giant task with no decomposition → no parallelism, no feedback | Break into diamond or pipeline |
| **Over-specialization** | Too many roles → coordination overhead exceeds benefit | `wg evolve run --strategy retirement` |
| **Under-specialization** | Generalist role for all tasks → poor quality | `wg evolve run --strategy gap-analysis` |
| **Skipping evaluation** | No feedback signal → no evolution → performance plateau | Enable `--auto-evaluate` or run `wg evaluate run` manually |

---

## 6. Pattern Composition

Real workflows combine patterns. Common compositions:

| Composition | Structure | Example |
|-------------|-----------|---------|
| **Pipeline of diamonds** | Sequential phases, each a fork-join | Design → [impl-A, impl-B, impl-C] → integrate → [test-unit, test-e2e] → deploy |
| **Diamond with pipeline workers** | Fork to workers, each a mini-pipeline | Plan → [design-A → impl-A, design-B → impl-B] → integrate |
| **Loop around a diamond** | Iterate a fork-join until convergence | [write-spec, write-impl, write-tests] → review → (back-edge) |
| **Scaffold then stream** | Platform first, then parallel features | setup-ci → [feature-A, feature-B, feature-C] |

---

## 7. Service Operation

The service daemon handles spawning, monitoring, and cleanup automatically.

### Starting

```bash
wg service start                  # default settings
wg service start --max-agents 4   # limit parallel agents
```

### Coordinator tick

Each tick runs these phases in order:

1. Process chat inbox → clean dead agents → zero-output detection → auto-checkpoint
2. Graph maintenance: cycle iteration, cycle failure restart, waiting task evaluation, message-triggered resurrection
3. Agency scaffolding: auto-assign, auto-evaluate, FLIP verification, auto-evolve, auto-create
4. Find ready tasks → spawn agents

Ticks happen on two triggers:
- **Immediate:** any graph change (`wg done`, `wg add`, etc.) triggers a tick via IPC
- **Poll:** safety-net every 60 seconds catches missed events

See [AGENT-SERVICE.md](AGENT-SERVICE.md) for the full step-by-step tick breakdown.

### Pause/resume/freeze

```bash
wg service pause    # running agents continue, no new spawns
wg service resume   # resume + immediate tick
wg service freeze   # SIGSTOP all agents + pause dispatcher
wg service thaw     # SIGCONT all agents + resume dispatcher
```

### Monitoring

```bash
wg service status              # daemon and dispatcher state
wg agents                      # all agents with status
wg agents --alive              # only alive agents
wg agents --working            # only working agents
wg agents --dead               # only dead agents
wg list --status in-progress   # tasks being worked on
wg tui                         # interactive dashboard (equiv. to wg viz --all --tui)
wg tui --no-mouse              # TUI without mouse capture (useful in tmux)
wg status                      # one-screen summary
wg analyze                     # comprehensive health report
wg watch                       # stream workgraph events as JSON lines
```

#### TUI views and keybindings

`wg tui` launches a full-screen terminal dashboard with task list, detail pane, and log viewer. Key bindings:

| Key | Action |
|-----|--------|
| `j`/`k` or `↑`/`↓` | Navigate tasks |
| `Enter` | View task detail |
| `/` | Search tasks |
| `n`/`N` | Next/previous search match |
| `Tab`/`Shift-Tab` | Next/previous match (in search mode) |
| `q` | Quit |

`wg viz` renders the graph as an ASCII diagram. It accepts optional task IDs to focus on specific subgraphs:

```bash
wg viz                          # active trees only (default)
wg viz --all                    # all tasks including fully-done trees
wg viz my-task                  # only the subgraph containing my-task
wg viz --show-internal          # include assign-*/evaluate-* meta-tasks
wg viz --no-tui                 # static output (no interactive TUI)
wg viz --status open            # filter by status
wg viz --tag my-tag             # filter by tag (AND semantics with multiple --tag)
wg viz --critical-path          # highlight the critical path in red
wg viz --layout tree            # classic DFS layout (default: diamond)
wg viz --dot                    # output Graphviz DOT format
wg viz --mermaid                # output Mermaid diagram format
wg viz --graph                  # 2D spatial graph with box-drawing characters
```

### Executor types

The dispatcher spawns agents via an executor. Three built-in executors:

| Executor | What it does | When to use |
|----------|-------------|-------------|
| **claude** | Pipes prompt into `claude --print` (Anthropic CLI) | Default — Claude Code agents |
| **amplifier** | Pipes prompt into `amplifier run --mode single` | OpenRouter-backed models, multi-provider setups |
| **shell** | Runs the task's `exec` command directly (no LLM) | Scripts, builds, non-AI work |

```bash
wg config --dispatcher-executor claude      # default
wg config --dispatcher-executor amplifier   # switch to amplifier
```

Spawned agents receive environment variables indicating their runtime context:
- `WG_TASK_ID` — the task ID being worked on
- `WG_AGENT_ID` — the agent registry ID (e.g., `agent-7`)
- `WG_EXECUTOR_TYPE` — the executor that spawned them (e.g., `claude`, `amplifier`)
- `WG_MODEL` — the effective model selected for this agent (set only when a model is resolved)
- `WG_USER` — the current user identity
- `WG_WORKTREE_PATH` / `WG_BRANCH` / `WG_PROJECT_ROOT` — worktree isolation paths (set when worktree isolation is active)

### Configuration

```toml
# .workgraph/config.toml
[dispatcher]
max_agents = 4
poll_interval = 60
executor = "claude"
model = "opus"

[agent]
heartbeat_timeout = 5   # minutes before agent is considered dead

[agency]
auto_evaluate = false
auto_assign = false
auto_place = false          # placement analysis merged into assignment step
auto_create = false         # auto-invoke creator agent for primitive store expansion
assigner_model = "haiku"    # lightweight model for assignment (default via wg agency init)
evaluator_model = "haiku"   # lightweight model for evaluation (default via wg agency init)
```

### Model selection

Model resolution follows a priority chain (highest wins):

1. `task.model` — per-task override set via `wg add --model` or `wg edit --model`
2. Executor config model — model field in the executor's config
3. `dispatcher.model` — from `[dispatcher]` in config.toml or CLI `--model` (legacy `[coordinator]` accepted)
4. Executor default — if no model is resolved, no model flag is passed and the executor uses its own default

For agency meta-tasks (assignment, evaluation, evolution), dedicated model settings apply:
- Assignment: `agency.assigner_model` (defaults to `haiku` after `wg agency init`)
- Evaluation: `agency.evaluator_model` (defaults to `haiku` after `wg agency init`)
- Evolution: `agency.evolver_model`

```bash
wg add "Simple fix" --model haiku      # cheap model for simple work
wg add "Complex design" --model opus   # strong model for hard work
```

### Provider selection

Use the `provider:model` format in `--model` to route tasks to a specific provider:

```bash
wg add "My task" --model openrouter:haiku
wg edit my-task --model anthropic:opus
```

Supported providers: `anthropic`, `openai`, `openrouter`, `local`. The legacy `--provider` flag still works but is deprecated.

### Execution modes

Control the agent's toolset with `--exec-mode`:

| Mode | What the agent gets | When to use |
|------|-------------------|-------------|
| `full` | All tools (default) | Implementation, integration |
| `light` | Read-only tools | Research, review, analysis |
| `bare` | Only `wg` CLI | Graph-only orchestration |
| `shell` | No LLM — runs task `exec` command | Scripts, builds, non-AI work |

```bash
wg add "Research X" --exec-mode light
wg add "Run tests" --exec-mode shell
wg edit my-task --exec-mode bare
```

### Task scheduling

Delay a task's availability with `--delay` or `--not-before`:

```bash
wg add "Follow-up check" --delay 1h        # available 1 hour from now
wg add "Deploy" --not-before 2026-03-20T09:00:00Z  # ISO 8601 timestamp
wg edit my-task --delay 30m
```

### Context scopes

Control how much context is assembled into an agent's prompt with `--context-scope`:

| Scope | What's included | When to use |
|-------|----------------|-------------|
| `clean` | Core task info only (title, description, dependency context) — no workflow instructions | Independent tasks, fresh starts |
| `task` | + workflow sections, tags/skills, downstream awareness | Most work — default |
| `graph` | + project description, subgraph summary (1-hop neighborhood) | Integration, cross-component review |
| `full` | + system awareness preamble, full graph summary, CLAUDE.md content | Meta-tasks, workflow design |

```bash
wg add "Leaf task" --context-scope clean    # minimal prompt
wg add "Integration" --context-scope graph  # needs full chain
wg edit my-task --context-scope full        # override for debugging
```

Scope is resolved at dispatch time by the dispatcher. If not set on the task, the role's default scope is used (if the task has an assigned agent with a role that specifies a default scope), otherwise `task` is the implicit default.

---

## 7b. Operational Commands

These commands support ongoing project health and agent lifecycle management.

### Compaction

`wg compact` distills the current graph state into a `context.md` summary. When running as a service, the dispatcher drives compaction via a `.compact-0` cycle task — this is the chat agent's self-introspection loop.

```bash
wg compact              # generate context.md from graph state
```

### Sweep

`wg sweep` detects and recovers orphaned in-progress tasks — tasks claimed by agents that are no longer running. By default it fixes them (unclaims and reopens). Use `--dry-run` to preview without changes.

```bash
wg sweep                # detect AND fix orphaned tasks (idempotent)
wg sweep --dry-run      # report only, don't fix
```

### Checkpoint

`wg checkpoint` saves a checkpoint for context preservation during long-running tasks. The dispatcher also auto-checkpoints alive agents when turn/time thresholds are met.

```bash
wg checkpoint <task-id> --summary "Completed auth module, starting tests"
wg checkpoint <task-id> --summary "Progress so far" --file src/auth.rs --file src/tests.rs
wg checkpoint <task-id> --list   # list existing checkpoints
```

The `--summary` flag is required (a ~500-token summary of progress). Optionally list `--file` for files modified since the last checkpoint.

### Stats

`wg stats` shows time counters and agent statistics.

```bash
wg stats                # project-wide stats
```

### Wait

`wg wait` parks a task in `Waiting` status until a condition is met. The dispatcher checks waiting conditions each tick (step 2.7) and automatically resumes satisfied tasks.

```bash
wg wait <task-id> --until "task:dep-a=done"       # wait for another task to reach a status
wg wait <task-id> --until "timer:5m"              # wait for a duration (e.g. 5m, 2h, 30s)
wg wait <task-id> --until "message"               # wait for any message on the task
wg wait <task-id> --until "human-input"           # wait specifically for a human message
wg wait <task-id> --until "file:path/to/file"     # wait for a file to change
wg wait <task-id> --until "task:dep-a=done" --checkpoint "Progress so far"
```

### Reclaim

`wg reclaim` transfers a task from a dead or unresponsive agent to a new one:

```bash
wg reclaim <task-id> --from <old-agent> --to <new-agent>
```

### Why-Blocked

`wg why-blocked` shows the full transitive dependency chain explaining why a task is blocked:

```bash
wg why-blocked <task-id>
```

### Match

`wg match` finds agents capable of performing a task based on skill requirements:

```bash
wg match <task-id>
```

### Chat

Send messages to a chat agent with `wg chat`. Useful for multi-chat setups:

```bash
wg chat "Deploy when ready"
wg chat --attachment report.md "Review this"      # attach a file
wg chat --chat my-coord "Target a specific chat agent"
```

## 8. Manual Operation

For interactive sessions (e.g., Claude Code working on a claimed task):

```bash
wg quickstart                    # orient yourself
wg ready                         # find available work
wg show <task-id>                # read task details
wg context <task-id>             # see what predecessors produced
# ... do the work ...
wg log <task-id> "What I did"    # leave traces for successors
wg artifact <task-id> path/to   # record outputs
wg done <task-id>                # complete
```

---

## 9. Functions (Workflow Templates)

Functions let you extract proven workflows into reusable templates and apply them with new inputs. All function commands are under `wg func`. There are three layers, each building on the previous one.

### 9.1 The Three Layers

| Layer | Version | What it does | When to use |
|-------|---------|-------------|-------------|
| **Static** | 1 | Fixed task topology, `{{input.X}}` substitution | Routine workflows where structure never varies |
| **Generative** | 2 | Planning node decides topology at apply time | Workflows where structure depends on inputs |
| **Adaptive** | 3 | Generative + trace memory from past runs | Workflows that benefit from learning over time |

### 9.2 Extracting Functions

Extract a template from completed work:

```bash
# Static: extract from one completed task
wg func extract impl-auth --name impl-feature --subgraph

# Generative: compare multiple completed traces
wg func extract impl-auth impl-caching impl-logging \
  --generative --name impl-feature

# With LLM generalization (replaces specific values with placeholders)
wg func extract fix-login --name bug-fix --generalize
```

Static extraction captures the exact task graph. Generative extraction compares multiple traces, identifies variable topology, and produces a planning node + constraints.

### 9.3 Applying Functions

Create tasks from a template:

```bash
# Basic application
wg func apply impl-feature \
  --input feature_name=auth \
  --input description="Add OAuth support"

# With dependency on existing work
wg func apply impl-feature \
  --input feature_name=auth \
  --after design-phase

# Preview without creating tasks
wg func apply impl-feature \
  --input feature_name=auth --dry-run

# From a federated peer
wg func apply impl-feature --from alice:impl-feature \
  --input feature_name=caching
```

For **generative functions** (version 2+), the first application creates a planner task. Once the planner completes (producing YAML output in its logs or artifacts), re-running apply parses the plan, validates it against constraints, and creates the planned tasks.

For **adaptive functions** (version 3), past run summaries are automatically injected into the planner prompt via `{{memory.run_summaries}}`, so the planner can learn from previous applications.

### 9.4 Making Functions Adaptive

Upgrade a generative function to learn from past runs:

```bash
wg func make-adaptive impl-feature
# Scans provenance for past applications, builds run summaries,
# injects {{memory.run_summaries}} into planner template, bumps to v3
```

The function must be version 2 (generative) first. If you have a static function, re-extract with `--generative` from multiple traces.

### 9.5 The Meta-Function (Self-Bootstrapping)

The extraction process itself can be expressed as a function:

```bash
# Bootstrap the built-in extract-function meta-function
wg func bootstrap

# Use it to extract a new function via a managed workflow
wg func apply extract-function \
  --input source_task_id=impl-auth \
  --input function_name=impl-feature-v2

# Make the extractor adaptive (learns from past extractions)
wg func make-adaptive extract-function
```

### 9.6 Discovering and Sharing Functions

```bash
# List local functions
wg func list

# Include federated peer functions
wg func list --include-peers

# Filter by visibility
wg func list --visibility peer

# Inspect a function
wg func show impl-feature
```

Functions carry a visibility field (`internal`, `peer`, `public`) that controls what crosses organizational boundaries during export.

### 9.7 Function Anti-Patterns

| Anti-pattern | What goes wrong | Fix |
|--------------|----------------|-----|
| **Static when generative needed** | Fixed topology doesn't fit varying inputs | Re-extract with `--generative` from multiple traces |
| **Skipping constraints** | Planner generates invalid task graphs | Set `constraints` with min/max tasks, required skills, etc. |
| **No static fallback** | Planner failure = total failure | Set `static_fallback: true` in planning config |
| **Unbounded memory** | Too many past runs slow the planner | Set `--max-runs` to a reasonable limit (10-20) |

---

## 10. Experimentation & Introspection

### 10.1 Replay — re-execute with a different model

`wg replay` snapshots the current graph, selectively resets tasks, and re-runs them (optionally with a different model):

```bash
wg replay --failed-only                    # retry all failed tasks
wg replay --failed-only --model haiku      # retry with a cheaper model
wg replay --below-score 0.7               # reset tasks scoring below 0.7
wg replay --subgraph my-task              # only replay this subgraph
wg replay --keep-done 0.9                 # preserve high-scoring done tasks
wg replay --plan-only                     # dry run: show what would reset
```

### 10.2 Run snapshots

`wg runs` manages graph snapshots for comparing experiments:

```bash
wg runs list                    # list all snapshots
wg runs show <run-id>           # inspect a specific snapshot
wg runs diff <run-id>           # compare current graph to a snapshot
wg runs restore <run-id>        # restore graph from a snapshot
```

### 10.3 Trace — execution history

`wg trace` provides provenance for understanding how work was done:

```bash
wg trace show <task-id>         # execution history of a task
wg trace export --visibility peer  # export trace data for sharing
wg trace import <file>          # import a peer's trace as read-only context
```

---

## 11. Cross-Repo Collaboration

### 11.1 Peer workgraphs

Connect workgraphs across repositories for cross-project coordination:

```bash
wg peer add alice /path/to/alice/repo    # register a peer
wg peer list                              # show all peers + status
wg peer status                            # quick health check
```

Create tasks in a peer's graph directly:

```bash
wg add "Fix shared API" --repo alice -d "Needs update for new auth flow"
```

### 11.2 Agency federation

Share roles, agents, and functions across organizational boundaries:

```bash
wg agency remote add upstream /path/to/upstream
wg agency pull upstream                    # pull roles/agents from upstream
wg agency push upstream                    # push local roles/agents
wg agency merge upstream                   # merge federated agency data
```

Functions also federate:

```bash
wg func list --include-peers               # discover peer functions
wg func apply impl-feature --from alice:impl-feature --input feature_name=auth
```

---

## 12. Autopoietic Task Generation

Agents should leave the system better than they found it. Beyond completing your assigned task, you can — and should — create follow-up work when you discover it. This is **autopoietic task generation**: the graph grows organically as agents encounter reality.

### 12.1 The Philosophy

A task description is a hypothesis. When you start working, you discover truth: missing prerequisites, unexpected complexity, bugs in adjacent code, documentation gaps. Rather than ignoring these discoveries or trying to fix everything yourself, encode them as new tasks. The dispatcher will dispatch them to the right agent at the right time.

```bash
# Found a bug while implementing a feature
wg add "Fix: race condition in connection pool" --after <current-task> \
  -d "Found while working on <current-task>: connection pool doesn't handle concurrent close()"

# Documentation is wrong
wg add "Update API docs for auth endpoint" --after <current-task> \
  -d "Discovered stale docs during implementation — endpoint signature changed"

# Missing prerequisite
wg add "Add retry logic to HTTP client" --after <current-task> \
  -d "Current task needs reliable HTTP calls but client has no retry support"
```

### 12.2 Decomposition Patterns

#### Simple fan-out (diamond)

When your task has 3+ independent parts touching **disjoint files**:

```bash
# You're working on current-task. Decompose:
wg add "Implement parser" --after <current-task> -d "File scope: src/parser.rs"
wg add "Implement formatter" --after <current-task> -d "File scope: src/formatter.rs"
wg add "Implement validator" --after <current-task> -d "File scope: src/validator.rs"

# Always add an integrator at the join point
wg add "Integrate parser + formatter + validator" \
  --after implement-parser,implement-formatter,implement-validator \
  -d "Wire modules together in src/lib.rs, run full test suite"
```

#### Pipeline decomposition

When parts must be sequential (shared files or ordering constraints):

```bash
wg add "Define data model" --after <current-task> -d "Creates src/models.rs"
wg add "Implement storage layer" --after define-data-model -d "Uses models in src/storage.rs"
wg add "Add API endpoints" --after implement-storage-layer -d "Exposes storage via src/api.rs"
```

#### Discovered work (not decomposition)

When you find issues unrelated to your current task:

```bash
# Spin off independent work — no --after needed if it's truly independent
wg add "Fix: typo in error messages" -d "Found across src/errors.rs"

# Or chain it after your task if it should happen later
wg add "Refactor: extract shared validation logic" --after <current-task> \
  -d "Three modules duplicate the same validation — extract to src/validate.rs"
```

### 12.3 Guardrails

Two guardrails prevent runaway task creation:

| Guardrail | Default | What it prevents | Error message |
|-----------|---------|-----------------|---------------|
| `max_child_tasks_per_agent` | 10 | A single agent creating unbounded tasks | "Agent {id} has already created {n}/{max} tasks" |
| `max_task_depth` | 8 | Infinite decomposition chains (tasks creating subtasks creating sub-subtasks...) | "Task would be at depth {d} (max: {max})" |

**Rationale:**
- **Per-agent limit** catches agents stuck in a decomposition loop. If an agent genuinely needs more than 10 subtasks, it should `wg fail` with an explanation — the human can raise the limit.
- **Depth limit** prevents vertical explosion. Real work rarely needs more than 8 levels of nesting. If you hit this, create tasks at the current level instead of nesting deeper.

Configure via:
```bash
wg config --max-child-tasks 15    # raise per-agent limit
wg config --max-task-depth 10     # raise depth limit
```

Guardrails only apply when `WG_AGENT_ID` is set (agent context). Human-initiated `wg add` commands bypass the per-agent limit.

### 12.4 Decision Tree: Should I Decompose?

```
Is the remaining work small (< ~200 lines of changes)?
├─ YES → Just do it. Don't decompose.
└─ NO → Does the work have 3+ independent parts?
         ├─ NO → Do the parts share files?
         │        ├─ YES → Pipeline (sequential tasks)
         │        └─ NO  → Consider diamond if parts are truly independent
         └─ YES → Do the parts touch disjoint files?
                   ├─ YES → Diamond pattern (parallel + integrator)
                   └─ NO  → Pipeline, or do it yourself if coordination
                            overhead exceeds the work
```

### 12.5 Anti-Patterns

| Anti-pattern | Problem | Fix |
|--------------|---------|-----|
| **Too many tiny tasks** | Coordination overhead exceeds work. 20 tasks for a 50-line change. | Only decompose when the parts are substantial |
| **Decomposing shared-file work** | Parallel agents edit the same file → overwrites | Use pipeline (sequential) or do it yourself |
| **Forgetting the integrator** | Diamond with no join → parallel outputs never merged | Always `wg add "Integrate..." --after worker-a,worker-b,...` |
| **Deep nesting** | Subtasks create sub-subtasks create sub-sub-subtasks | Keep decomposition shallow; create siblings, not descendants |
| **Decomposing instead of doing** | Creating tasks to avoid doing work you should just do | If total work is small and straightforward, do it |

---

## Quick Reference

**Build a pipeline:**
```bash
wg add "Step 1" --id s1 && wg add "Step 2" --id s2 --after s1 && wg add "Step 3" --id s3 --after s2
```

**Build a diamond:**
```bash
wg add "Plan" --id p && wg add "Work A" --id a --after p && wg add "Work B" --id b --after p && wg add "Integrate" --id i --after a,b
```

**Build a loop:**
```bash
wg add "Write" --id w --after revise --max-iterations 5 && wg add "Review" --id r --after w && wg add "Revise" --id revise --after r
```

**Stop a loop:**
```bash
wg done <task> --converged
```

**Never do this:**
```bash
# WRONG: built-in task tools don't interact with workgraph
TaskCreate(...)   # ← NO
TaskUpdate(...)   # ← NO

# RIGHT: always use wg CLI
wg add "Task title" --id my-task
wg done my-task
```
