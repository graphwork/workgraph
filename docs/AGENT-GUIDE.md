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

If the request doesn't map to one of these, it's probably a **composition** — see §6.

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

### 2.4 Loop — iterate until convergence

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

**Checking iteration state:**
```bash
wg show <task-id>    # shows loop_iteration and cycle membership
wg cycles            # see all cycles and their current state
```

Previous iterations' logs and artifacts are preserved. Review them with `wg log <task-id> --list` and `wg context <task-id>` to build on prior work rather than starting fresh.

### 2.5 The Critical Structural Rule

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
wg agent create "Planner" --role <architect-hash> --motivation <motivation-hash>
wg agent create "Worker" --role <implementer-hash> --motivation <motivation-hash>
wg agent create "Synthesizer" --role <integrator-hash> --motivation <motivation-hash>

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
| Too few roles | Low scores on specialized tasks | `wg evolve --strategy gap-analysis` |
| Too many roles | Roles with zero assignments | `wg evolve --strategy retirement` |

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

Control patterns describe how you stay coordinated without direct agent-to-agent communication.

### 4.1 Stigmergic Coordination — the graph is truth

**Read `wg show` and `wg context`. Don't assume — the graph is the single source of truth.**

Agents coordinate indirectly through the shared graph. There are no agent-to-agent messages. Every `wg done`, `wg log`, and `wg artifact` call modifies the graph, which stimulates downstream agents.

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

### 4.2 After Code Changes: Rebuild

When you modify source code in a Rust project that uses `cargo install`:

```bash
cargo install --path .
```

This updates the global `wg` binary. Forgetting this step is a common source of "why isn't this working" bugs. Do it after every code change.

### 4.3 Convergence Signaling

Use `wg done --converged` to stop a loop. Use plain `wg done` to iterate.

```bash
# Work is good enough — stop the loop
wg done review --converged

# Work needs another pass — let the cycle continue
wg done review
```

The `--converged` flag tags the cycle header. This prevents further iterations even if `--max-iterations` hasn't been reached. Any cycle member can signal convergence for the entire cycle.

### 4.4 Evolve — the feedback loop

After work accumulates, evaluate and evolve roles:

```bash
# Evaluate completed work
wg evaluate my-task

# Preview evolution proposals
wg evolve --dry-run

# Apply evolution
wg evolve --strategy all
```

**Single-loop learning:** task failed → retry with different agent (`wg retry`).
**Double-loop learning:** task type keeps failing → change the role itself (`wg evolve --strategy mutation`).

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
| **Over-specialization** | Too many roles → coordination overhead exceeds benefit | `wg evolve --strategy retirement` |
| **Under-specialization** | Generalist role for all tasks → poor quality | `wg evolve --strategy gap-analysis` |
| **Skipping evaluation** | No feedback signal → no evolution → performance plateau | Enable `--auto-evaluate` or run `wg evaluate` manually |

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

Each tick: reap zombies → clean dead agents → count alive → find ready tasks → spawn agents.

Ticks happen on two triggers:
- **Immediate:** any graph change (`wg done`, `wg add`, etc.) triggers a tick via IPC
- **Poll:** safety-net every 60 seconds catches missed events

### Pause/resume

```bash
wg service pause    # running agents continue, no new spawns
wg service resume   # resume + immediate tick
```

### Monitoring

```bash
wg service status              # daemon and coordinator state
wg agents                      # all agents with status
wg list --status in-progress   # tasks being worked on
wg tui                         # interactive dashboard
wg status                      # one-screen summary
wg analyze                     # comprehensive health report
```

### Configuration

```toml
# .workgraph/config.toml
[coordinator]
max_agents = 4
poll_interval = 60
executor = "claude"
model = "opus"

[agent]
heartbeat_timeout = 5   # minutes before agent is considered dead

[agency]
auto_evaluate = false
auto_assign = false
```

### Model selection

Priority order: `--model` flag > task's `model` field > `coordinator.model` > `agent.model`.

```bash
wg add "Simple fix" --model haiku      # cheap model for simple work
wg add "Complex design" --model opus   # strong model for hard work
```

---

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

## 9. Trace Functions (Workflow Templates)

Trace functions let you extract proven workflows into reusable templates and instantiate them with new inputs. There are three layers, each building on the previous one.

### 9.1 The Three Layers

| Layer | Version | What it does | When to use |
|-------|---------|-------------|-------------|
| **Static** | 1 | Fixed task topology, `{{input.X}}` substitution | Routine workflows where structure never varies |
| **Generative** | 2 | Planning node decides topology at instantiation time | Workflows where structure depends on inputs |
| **Adaptive** | 3 | Generative + trace memory from past runs | Workflows that benefit from learning over time |

### 9.2 Extracting Functions

Extract a template from completed work:

```bash
# Static: extract from one completed task
wg trace extract impl-auth --name impl-feature --subgraph

# Generative: compare multiple completed traces
wg trace extract impl-auth impl-caching impl-logging \
  --generative --name impl-feature

# With LLM generalization (replaces specific values with placeholders)
wg trace extract fix-login --name bug-fix --generalize
```

Static extraction captures the exact task graph. Generative extraction compares multiple traces, identifies variable topology, and produces a planning node + constraints.

### 9.3 Instantiating Functions

Create tasks from a template:

```bash
# Basic instantiation
wg trace instantiate impl-feature \
  --input feature_name=auth \
  --input description="Add OAuth support"

# With dependency on existing work
wg trace instantiate impl-feature \
  --input feature_name=auth \
  --after design-phase

# Preview without creating tasks
wg trace instantiate impl-feature \
  --input feature_name=auth --dry-run

# From a federated peer
wg trace instantiate impl-feature --from alice:impl-feature \
  --input feature_name=caching
```

For **generative functions** (version 2+), the first instantiation creates a planner task. Once the planner completes (producing YAML output in its logs or artifacts), re-running instantiate parses the plan, validates it against constraints, and creates the planned tasks.

For **adaptive functions** (version 3), past run summaries are automatically injected into the planner prompt via `{{memory.run_summaries}}`, so the planner can learn from previous instantiations.

### 9.4 Making Functions Adaptive

Upgrade a generative function to learn from past runs:

```bash
wg trace make-adaptive impl-feature
# Scans provenance for past instantiations, builds run summaries,
# injects {{memory.run_summaries}} into planner template, bumps to v3
```

The function must be version 2 (generative) first. If you have a static function, re-extract with `--generative` from multiple traces.

### 9.5 The Meta-Function (Self-Bootstrapping)

The extraction process itself can be expressed as a trace function:

```bash
# Bootstrap the built-in extract-function meta-function
wg trace bootstrap

# Use it to extract a new function via a managed workflow
wg trace instantiate extract-function \
  --input source_task_id=impl-auth \
  --input function_name=impl-feature-v2

# Make the extractor adaptive (learns from past extractions)
wg trace make-adaptive extract-function
```

### 9.6 Discovering and Sharing Functions

```bash
# List local functions
wg trace list-functions

# Include federated peer functions
wg trace list-functions --include-peers

# Filter by visibility
wg trace list-functions --visibility peer

# Inspect a function
wg trace show-function impl-feature
```

Functions carry a visibility field (`internal`, `peer`, `public`) that controls what crosses organizational boundaries during export.

### 9.7 Trace Function Anti-Patterns

| Anti-pattern | What goes wrong | Fix |
|--------------|----------------|-----|
| **Static when generative needed** | Fixed topology doesn't fit varying inputs | Re-extract with `--generative` from multiple traces |
| **Skipping constraints** | Planner generates invalid task graphs | Set `constraints` with min/max tasks, required skills, etc. |
| **No static fallback** | Planner failure = total failure | Set `static_fallback: true` in planning config |
| **Unbounded memory** | Too many past runs slow the planner | Set `--max-runs` to a reasonable limit (10-20) |

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
