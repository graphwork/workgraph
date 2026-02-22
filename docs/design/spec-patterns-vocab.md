# Spec: Canonical Pattern Vocabulary for Workgraph Agents

**Date:** 2026-02-21
**Status:** Active
**Cross-references:** [Organizational Patterns Research](../research/organizational-patterns.md), [Cycle-Aware Graph Design](cycle-aware-graph.md), [Agent Guide](../AGENT-GUIDE.md)

---

## Purpose

This document defines the canonical vocabulary of patterns for workgraph agents. Each pattern maps to concrete `wg` CLI commands, task graph shapes, and guidance on when to apply it. The vocabulary is organized into four categories: **structure**, **agency**, **control**, and **shorthands**.

---

## 1. Structure Patterns

Structure patterns describe how to arrange tasks in the graph.

### 1.1 Pipeline

**What:** A sequential chain of specialized stages with handoffs. Each stage transforms its predecessor's output.

**Shape:**
```
analyst → implementer → reviewer → deployer
```

**When to use:**
- Work requires sequential specialized transformation (design → build → test → ship)
- Each stage has a different role or expertise
- Ordering is non-negotiable — later stages consume earlier stages' output

**CLI:**
```bash
wg add "Design API" --id design
wg add "Implement API" --id implement --after design
wg add "Review API" --id review --after implement
wg add "Deploy API" --id deploy --after review
```

**Key property:** Throughput is limited by the slowest stage (Theory of Constraints). If review is the bottleneck, add more reviewer agents or decompose review into parallel sub-reviews.

---

### 1.2 Fork-Join (Diamond)

**What:** Split work into parallel independent tasks, then synchronize at an integrator. The "diamond" shape: one node fans out, another fans in.

**Shape:**
```
         ┌─── worker-1 ───┐
planner ─┼─── worker-2 ───┼─── synthesizer
         └─── worker-3 ───┘
```

**When to use:**
- Work decomposes into independent parallel units (modules, files, chapters)
- All parallel branches must complete before integration
- You want to maximize throughput by exploiting parallelism

**CLI:**
```bash
wg add "Plan the work" --id planner
wg add "Implement module A" --id worker-a --after planner
wg add "Implement module B" --id worker-b --after planner
wg add "Implement module C" --id worker-c --after planner
wg add "Integrate results" --id synthesizer --after worker-a,worker-b,worker-c
```

**Key property:** The planner must define clean boundaries between workers. The synthesizer must resolve integration conflicts.

**CRITICAL RULE: Never parallelize tasks that modify the same files.** If worker-a and worker-b both edit `src/main.rs`, they will produce conflicting changes. Either serialize them (pipeline) or refactor the work so each worker owns distinct files.

---

### 1.3 Scatter-Gather

**What:** Fan out to heterogeneous specialists who each examine the same artifact from their own perspective, then collect their views.

**Shape:**
```
            ┌── security-review ──┐
artifact ───┼── perf-review ──────┼─── summary
            └── ux-review ────────┘
```

**When to use:**
- Multiple distinct perspectives are needed on the same work product
- Reviewers have different roles/expertise (unlike fork-join where workers do similar tasks)
- The aggregator may accept partial results — not all reviewers need to finish

**CLI:**
```bash
wg add "Produce artifact" --id artifact
wg add "Security review" --id sec-review --after artifact --skill security
wg add "Performance review" --id perf-review --after artifact --skill performance
wg add "UX review" --id ux-review --after artifact --skill ux
wg add "Summarize reviews" --id summary --after sec-review,perf-review,ux-review
```

**Difference from fork-join:** Workers in a fork-join produce parts of the same output (each builds a module). Workers in scatter-gather produce independent assessments of the same input (each reviews the whole artifact).

---

### 1.4 Loop (Cycle)

**What:** Iterate a chain of tasks until convergence. The graph contains a structural cycle detected by the system.

**Shape:**
```
write ──→ review ──→ revise ──┐
  ▲                           │
  └───────────────────────────┘
```

**When to use:**
- Iterative refinement (draft → review → revise, repeat)
- Convergent processes where quality improves each pass
- Processes that need a bounded number of attempts

**CLI:**
```bash
# The cycle header gets --max-iterations and --after creating the back-edge
wg add "Write draft" --id write --after revise --max-iterations 5
wg add "Review draft" --id review --after write
wg add "Revise draft" --id revise --after review
```

The cycle header (`write`) has `--after revise`, creating a back-edge. On the first iteration, the back-edge is vacuously satisfied. On subsequent iterations, completing `revise` resets all cycle members to `open` with incremented `loop_iteration`.

**Convergence:** Any cycle member can stop the loop:
```bash
wg done review --converged   # signals that the cycle's work has stabilized
```

**Guards and delays:**
```bash
wg add "Write" --id write --after revise --max-iterations 5 \
  --cycle-guard "task:review=failed" --cycle-delay "5m"
```

---

## 2. Agency Patterns

Agency patterns describe how to staff work — which roles to define and how to assign them.

### 2.1 Planner-Workers-Synthesizer

**What:** One thinker decomposes the problem, many doers execute in parallel, one integrator combines results. This is the agency staffing of a fork-join structure.

**Roles:**
| Role | Purpose |
|------|---------|
| Architect / Planner | Decomposes work, defines interfaces between workers |
| Implementer / Worker | Executes one slice of the decomposed work |
| Integrator / Synthesizer | Combines worker outputs into a coherent whole |

**CLI (agency setup):**
```bash
wg role add "Architect" --outcome "Clear decomposition with non-overlapping boundaries" --skill architecture
wg role add "Implementer" --outcome "Working, tested code for assigned module" --skill coding --skill testing
wg role add "Integrator" --outcome "Cohesive merged output with conflicts resolved" --skill integration

wg motivation add "Thorough" --accept "Slower delivery" --reject "Skipping edge cases"

wg agent create "Planner" --role <architect-hash> --motivation <thorough-hash>
wg agent create "Worker" --role <implementer-hash> --motivation <thorough-hash>
wg agent create "Synthesizer" --role <integrator-hash> --motivation <thorough-hash>

wg assign planner <planner-agent-hash>
wg assign worker-a <worker-agent-hash>
wg assign synthesizer <synthesizer-agent-hash>
```

**When to use:** Any fork-join or diamond structure where the planning, execution, and integration phases require different capabilities.

---

### 2.2 Specialist

**What:** One role owns one domain. Tasks requiring that domain are routed to agents with that role.

**Roles:** One per knowledge domain — `security-analyst`, `database-expert`, `ml-engineer`, etc.

**CLI:**
```bash
wg role add "Security Analyst" --outcome "Vulnerabilities identified and mitigated" --skill security --skill threat-modeling
wg role add "Database Expert" --outcome "Optimized schema and queries" --skill sql --skill performance
```

**When to use:**
- Tasks require deep domain expertise that a generalist would handle poorly
- Maps to Team Topologies' "complicated-subsystem team"
- The domain is narrow enough that one role can cover it

**Ashby's Law check:** Ensure you have at least as many distinct specialist roles as you have distinct task types requiring specialized knowledge.

---

### 2.3 Stream-Aligned

**What:** One role follows one thread of work end-to-end, from inception to delivery. The default role type — most roles should be this.

**Roles:** Aligned to a work stream (feature, product area, user journey), not to a function.

**CLI:**
```bash
wg role add "Auth Stream" --outcome "Complete, working authentication flow" \
  --skill coding --skill testing --skill documentation
```

**When to use:**
- The work is a coherent feature or product area
- Minimizing handoffs matters more than deep specialization
- Mirrors Team Topologies' "stream-aligned team"

**Difference from specialist:** A specialist owns a domain (security everywhere); a stream-aligned role owns a flow (auth feature, end-to-end).

---

### 2.4 Platform

**What:** One role produces shared infrastructure that other roles depend on. Platform tasks appear as `after` dependencies for stream-aligned tasks.

**Roles:** Provide tooling, CI/CD, shared libraries, test infrastructure.

**CLI:**
```bash
wg role add "Platform" --outcome "Self-service infrastructure that accelerates other agents" --skill devops --skill tooling

# Platform task that others depend on
wg add "Set up CI pipeline" --id setup-ci
wg add "Implement feature A" --id feature-a --after setup-ci
wg add "Implement feature B" --id feature-b --after setup-ci
```

**When to use:**
- Multiple stream-aligned agents need the same foundation
- The infrastructure is complex enough to warrant a dedicated role
- Mirrors Team Topologies' "platform team"

---

## 3. Control Patterns

Control patterns describe how the system governs itself — feedback, adaptation, and self-regulation.

### 3.1 Stigmergic

**What:** Agents read the graph, not each other. The graph is the communication channel. No agent-to-agent messages — all coordination happens through task state.

**Mechanism:** This is not a pattern you implement; it is the fundamental operating principle of workgraph. Every `wg done`, `wg log`, `wg artifact` call modifies the shared graph, which stimulates downstream agents.

**Key practices:**
- Write descriptive task titles and descriptions — these are the "pheromone trails" for downstream agents
- Use `wg log` to leave progress traces — they become context for dependent tasks
- Use `wg artifact` to mark outputs — they appear in `wg context` for successors

```bash
# Agent A completes work, leaving traces
wg log implement-api "Implemented REST endpoints in src/api.rs"
wg artifact implement-api src/api.rs
wg done implement-api

# Agent B (working on a task after implement-api) reads the traces
wg context write-tests
# Shows: From implement-api (done): Artifacts: src/api.rs
```

**Key insight from organizational-patterns.md §1:** Stigmergy makes workgraph scale. Adding agents does not increase communication overhead because the coordination cost is absorbed by the shared graph.

---

### 3.2 Requisite Variety

**What:** Match the number of distinct roles to the number of distinct task types. Ashby's Law: **R ≥ V** (roles ≥ variety of tasks).

**Diagnosis:**
```bash
wg agency stats    # check role coverage
wg skill list      # see all skill types in the graph
```

**Symptoms of violation:**
| Violation | Symptom | Fix |
|-----------|---------|-----|
| Too few roles (R < V) | Low evaluation scores on specialized tasks | `wg evolve --strategy gap-analysis` |
| Too many roles (R > V) | Roles with zero task assignments | `wg evolve --strategy retirement` |

**CLI for remediation:**
```bash
# Gap analysis: let the evolver propose new roles for unmet needs
wg evolve --strategy gap-analysis

# Retirement: remove roles that aren't earning their keep
wg evolve --strategy retirement
```

---

### 3.3 Evolve

**What:** Evaluate completed work, then mutate roles and motivations to produce better agents. The execute → evaluate → evolve cycle.

**Mechanism:**
```bash
# After a task completes, evaluate it
wg evaluate my-task

# Periodically evolve the agency based on accumulated evaluations
wg evolve --strategy all
wg evolve --dry-run   # preview first
```

**Strategies:**
| Strategy | Description |
|----------|-------------|
| `mutation` | Modify a single existing role to improve weak dimensions |
| `crossover` | Combine traits from two high-performing roles |
| `gap-analysis` | Create new roles/motivations for unmet needs |
| `retirement` | Remove poor-performing entities |
| `motivation-tuning` | Adjust trade-offs on existing motivations |

**Automation:**
```bash
wg config --auto-evaluate true   # create evaluation tasks for every completed task
wg config --evaluator-model opus  # use a strong model for quality assessment
```

**Key insight from organizational-patterns.md §5:** The evolve loop is autopoietic — the system produces the components (agent definitions) that produce the system (task completions). An agency that does not evolve becomes structurally rigid.

---

### 3.4 Double-Loop

**What:** Don't just retry a failed task with a different agent (single-loop). Change the role definition itself (double-loop).

**Single-loop (adjust within existing framework):**
```bash
# Task failed → retry with different agent assignment
wg retry my-task
wg assign my-task <different-agent>
```

**Double-loop (change the framework):**
```bash
# Task failed → the role itself is wrong → evolve the role
wg evolve --strategy mutation
# The evolver modifies the role's skills, description, or desired_outcome
# Future agents built from the evolved role handle this task type better
```

**When to escalate from single to double loop:**
- The same task type fails repeatedly across different agents
- Evaluation scores plateau despite agent re-assignment
- The role definition doesn't match the actual work required

**Key insight from organizational-patterns.md §6.4:** Organizations that cannot double-loop learn become rigid. In workgraph, evolving roles and motivations (not just re-assigning agents) is what prevents performance plateaus.

---

## 4. One-Word Shorthands

Quick vocabulary for conversations, task descriptions, and documentation.

| Shorthand | Full pattern | Section |
|-----------|-------------|---------|
| **pipeline** | Sequential chain with handoffs | §1.1 |
| **diamond** | Fork-join: fan out, fan in | §1.2 |
| **loop** | Iterate until convergence via structural cycle | §1.4 |
| **map-reduce** | Data-parallel diamond: planner decomposes → N workers → reducer aggregates | §1.2 variant |
| **scaffold** | Platform role produces infrastructure that stream-aligned roles depend on | §2.4 |
| **evolve** | Evaluate → mutate roles → better agents | §3.3 |

### The Key Phrase

> **"Diamond with specialists"** — a planner forks to role-matched workers, a synthesizer joins.

This is the most common compound pattern: the structure is a diamond (§1.2), the staffing is planner-workers-synthesizer (§2.1), and the workers are specialists (§2.2) matched to their slice of work.

**CLI for "diamond with specialists":**
```bash
# Structure
wg add "Plan decomposition" --id plan
wg add "Implement auth module" --id auth --after plan --skill security
wg add "Implement data layer" --id data --after plan --skill database
wg add "Implement UI" --id ui --after plan --skill frontend
wg add "Integrate" --id integrate --after auth,data,ui

# Agency
wg role add "Architect" --outcome "Clean decomposition" --skill architecture
wg role add "Security Dev" --outcome "Secure auth" --skill security --skill coding
wg role add "Data Dev" --outcome "Efficient data layer" --skill database --skill coding
wg role add "Frontend Dev" --outcome "Responsive UI" --skill frontend --skill coding
wg role add "Integrator" --outcome "Working integrated system" --skill integration

# Assignment (via auto-assign or manual)
wg assign plan <architect-agent>
wg assign auth <security-agent>
wg assign data <data-agent>
wg assign ui <frontend-agent>
wg assign integrate <integrator-agent>
```

---

## 5. Pattern Composition

Real workflows combine patterns. The table below shows common compositions.

| Composition | Structure | Example |
|-------------|-----------|---------|
| **Pipeline of diamonds** | Sequential phases, each phase is a fork-join | Design → [impl-A, impl-B, impl-C] → integrate → [test-unit, test-e2e] → deploy |
| **Diamond with pipeline workers** | Fork to workers, each worker is a mini-pipeline | Plan → [design-A → impl-A, design-B → impl-B] → integrate |
| **Loop around a diamond** | Iterate a fork-join until convergence | [write-spec, write-impl, write-tests] → review → (back to specs if review fails) |
| **Scaffold then stream** | Platform role first, then stream-aligned parallel work | setup-ci → [feature-A, feature-B, feature-C] |

---

## 6. The Critical Rule

**Never parallelize tasks that modify the same files.**

This is the single most important constraint when designing parallel work:

- If two tasks edit `src/main.rs` concurrently, one will overwrite the other's changes
- The graph encodes ordering — if two tasks share file targets, add an `--after` edge between them
- When decomposing a diamond, the planner must ensure each worker owns distinct files

**Diagnosis:**
```bash
wg list --status in-progress   # check what's running concurrently
wg show <task-id>              # check deliverables for overlap
```

**Prevention:** When the planner creates worker tasks, it should explicitly list each worker's file scope in the task description. The synthesizer/integrator is the only task that may touch all files.

---

## 7. Pattern Selection Guide

| Situation | Pattern | Graph shape |
|-----------|---------|-------------|
| Sequential specialized stages | **pipeline** | Chain: A → B → C |
| Independent parallelizable work | **diamond** | Fan-out/fan-in: A → [B,C,D] → E |
| Multiple perspectives on same artifact | **scatter-gather** | Like diamond, but heterogeneous workers |
| Data-parallel analysis | **map-reduce** | Planner → N workers → reducer |
| Iterative refinement | **loop** | Cycle: A → B → C → A (with `--max-iterations`) |
| One thinker, many doers | **planner-workers-synthesizer** | Diamond staffing pattern |
| Deep domain expertise needed | **specialist** | Role per domain |
| End-to-end feature ownership | **stream-aligned** | Role per feature |
| Shared infrastructure first | **scaffold** | Platform tasks as `after` for all others |
| System self-improvement | **evolve** | evaluate → evolve → execute cycle |
| Failed pattern not working | **double-loop** | Change the role, not just the agent |

---

## 8. Anti-Patterns

| Anti-pattern | What goes wrong | Fix |
|--------------|----------------|-----|
| **Parallel file conflict** | Two concurrent tasks edit the same file → overwrites | Serialize with `--after` or split files |
| **Monolithic task** | One giant task with no decomposition → no parallelism, no feedback | Break into diamond or pipeline |
| **Over-specialization** | Too many roles → coordination overhead exceeds benefit | Retire unused roles (`wg evolve --strategy retirement`) |
| **Under-specialization** | One generalist role for all tasks → poor quality on specialized work | Add specialist roles (`wg evolve --strategy gap-analysis`) |
| **Evolve too often** | Roles change faster than agents can stabilize → instability | Evolve periodically, not continuously |
| **Never evolve** | Roles ossify while task landscape changes → performance plateau | Run `wg evolve` after accumulating evaluations |
| **Unbounded loop** | Cycle without `--max-iterations` → infinite iteration | Always set `--max-iterations` on cycle headers |
| **Skip evaluation** | No monitoring → quality drift → no evolution signal | Enable `--auto-evaluate` |

---

## Glossary

| Term | Definition |
|------|-----------|
| **after** | Edge expressing temporal ordering. "A is after B" means B runs before A. Stored field on tasks. |
| **before** | Computed inverse of `after`. "A is before B" means A runs before B. |
| **cycle** | A set of tasks whose `after` edges form a loop, detected by Tarjan's SCC algorithm. |
| **cycle header** | The task in a cycle that carries `CycleConfig` (max_iterations, guard, delay). |
| **convergence** | A cycle stops iterating because an agent signals `--converged` on `wg done`. |
| **diamond** | The fork-join graph shape: one node fans out to N, another fans in from N. |
| **loop_iteration** | Counter on each task tracking which pass of the cycle it's on (0-indexed). |
| **ready** | A task is ready when: status is open, all `after` predecessors are terminal, and time constraints are met. |
| **scaffold** | A platform task that must complete before dependent stream-aligned tasks can start. |
| **stigmergy** | Indirect coordination through traces left in a shared environment (the task graph). |
