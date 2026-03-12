# Agency Pipeline Lifecycle: Full Task Lifecycle & Auxiliary Agents

This document maps the complete agency pipeline from task creation to evaluation,
covering every auxiliary agent type, the eval scaffold system, assignment vs
placement, model routing, and identity gaps.

## Full Lifecycle Flow Diagram

```
                         ┌─────────────┐
                         │  wg add /   │
                         │  wg draft   │
                         └─────┬───────┘
                               │
                               ▼
                     ┌─────────────────────┐
                     │  DRAFT (paused=true) │  (only if wg draft; wg add → Open)
                     └─────────┬───────────┘
                               │
              ┌────────────────┼──────────────────┐
              │ auto-place     │ fast path         │ wg publish
              │ (no deps)      │ (deps, no         │ (manual)
              │                │  file overlap)    │
              ▼                ▼                   ▼
    ┌──────────────┐   ┌─────────────┐     ┌──────────────┐
    │ .place-<id>  │   │ AUTO-PLACED │     │  PUBLISHED   │
    │ (agent LLM)  │   │ (unpaused)  │     │  (unpaused)  │
    └──────┬───────┘   └──────┬──────┘     └──────┬───────┘
           │                  │                   │
           └──────────────────┼───────────────────┘
                              │
                              ▼
               ┌──────────────────────────────┐
               │ EVAL SCAFFOLD (at publish    │
               │ or coordinator tick)          │
               │ ─ .assign-<id> created       │
               │ ─ .flip-<id> created         │
               │ ─ .evaluate-<id> created     │
               └──────────────┬───────────────┘
                              │
                              ▼
                   ┌────────────────────┐
                   │ .assign-<id> runs  │  (lightweight LLM call or inline)
                   │ selects best agent │
                   │ sets exec_mode,    │
                   │ context_scope      │
                   └────────┬───────────┘
                            │ (marks .assign Done → unblocks source task)
                            ▼
                   ┌────────────────────┐
                   │   TASK EXECUTION   │
                   │   (agent spawned   │
                   │    by coordinator) │
                   │   status: InProg.  │
                   └────────┬───────────┘
                            │
              ┌─────────────┼─────────────────┐
              │ wg done     │ agent dies       │ wg fail
              ▼             ▼                  ▼
         ┌────────┐  ┌───────────┐      ┌──────────┐
         │  DONE  │  │  TRIAGE   │      │  FAILED  │
         └───┬────┘  │ (LLM call)│      └────┬─────┘
             │       └─────┬─────┘           │
             │             │ done/continue/  │
             │             │ restart         │
             │             ▼                 │
             │       ┌───────────┐           │
             │       │ REOPENED/ │           │
             │       │ DONE/FAIL │           │
             │       └─────┬─────┘           │
             │             │                 │
             └─────────────┼─────────────────┘
                           │
                           ▼
                ┌─────────────────────┐
                │ .flip-<id> runs     │  (FLIP: 2-phase intent fidelity)
                │  Phase 1: inference │
                │  Phase 2: compare   │
                └─────────┬───────────┘
                          │
            ┌─────────────┼──────────────────┐
            │             │                  │
            │ FLIP < threshold?              │
            │             │                  │
            ▼             ▼                  ▼
  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
  │.verify-flip- │  │ (no verify)  │  │ (disabled)   │
  │  <id> runs   │  │              │  │              │
  │(Opus verify) │  │              │  │              │
  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘
         │                 │                 │
         └─────────────────┼─────────────────┘
                           │
                           ▼
                ┌─────────────────────┐
                │ .evaluate-<id> runs │  (evaluator LLM: scores the task)
                │ Produces Evaluation │
                │ record in YAML      │
                └─────────┬───────────┘
                          │
                          ▼
                ┌─────────────────────┐
                │ PERFORMANCE UPDATE  │
                │ ─ Agent score       │
                │ ─ Role performance  │
                │ ─ Eval gate check   │
                └─────────────────────┘
                          │
                          ▼
              ┌───────────────────────────┐
              │ EVOLUTION (periodic)      │
              │ .evolve-* when threshold  │
              │ evals accumulate or       │
              │ avg score drops           │
              └───────────────────────────┘
```

## Every Auxiliary Agent Type

### Summary Table

| Prefix | What it does | When created | Trigger | Exec mode | Identity | Model tier | Code path |
|--------|-------------|--------------|---------|-----------|----------|------------|-----------|
| `.assign-<id>` | Selects best agent for a task via lightweight LLM call | At publish (eval_scaffold) or coordinator tick Phase 3 | Task is ready + unassigned | `bare` (inline) | Configurable (`assigner_agent`) | Fast (Haiku) | `src/commands/eval_scaffold.rs:83`, `src/commands/service/coordinator.rs:1037` |
| `.place-<id>` | Wires a draft task into the graph (adds deps, publishes) | Coordinator tick Phase 2.9 | Draft task (paused, not unplaced, no `placed` tag) with file overlap or no deps | `bare` | Configurable (`placer_agent`) | Fast (Haiku) | `src/commands/service/coordinator.rs:742` |
| `.flip-<id>` | FLIP evaluation: 2-phase intent fidelity probe | At publish (eval_scaffold) when FLIP enabled | Source task completes (Done/Failed) | `bare` (inline) | Configurable (`evaluator_agent`) | Standard (Sonnet) inference + Fast (Haiku) comparison | `src/commands/eval_scaffold.rs:36`, `src/commands/evaluate.rs:580` |
| `.evaluate-<id>` | Post-task evaluation: scores quality, completeness, efficiency | At publish (eval_scaffold) or coordinator tick Phase 4 | Source task Done/Failed (and .flip if enabled) | `bare` (inline) | Configurable (`evaluator_agent`) | Standard (Sonnet) | `src/commands/eval_scaffold.rs:160`, `src/commands/evaluate.rs:103` |
| `.verify-flip-<id>` | Independent verification of tasks with low FLIP scores | Coordinator tick Phase 4.5 | FLIP score < `flip_verification_threshold` | `light` | None (bare) | Premium (Opus) | `src/commands/service/coordinator.rs:1538` |
| `.evolve-*` | Evolves agency roles/tradeoffs based on accumulated eval data | Coordinator tick Phase 4.6 | Threshold trigger (N evals) or reactive trigger (avg score drop) | `bare` | Configurable (`evolver_agent`) | Standard (Sonnet) | `src/commands/service/coordinator.rs:1746` |
| `.create-*` | Expands primitive store (new roles, tradeoffs, components) | Coordinator tick Phase 4.7 or assigner `create_needed` flag | N completed tasks since last creation, or assigner flags poor match | `bare` | Configurable (`creator_agent`) | Premium (Opus) | `src/commands/service/coordinator.rs:1915`, `src/commands/service/coordinator.rs:1333` |
| `.respond-to-<id>` | Handles unread messages on a completed task when downstream is active | Coordinator tick Phase 2.8 (resurrection) | Done task has unread messages + downstream InProgress/Done | inherits parent session | Inherits parent's agent | (parent's model) | `src/commands/service/coordinator.rs:529` |
| `.coordinator-*` | Persistent coordinator chat agent (long-lived LLM session in daemon) | Daemon startup | `wg service start` | stream-json CLI | N/A (system) | N/A | `src/commands/service/coordinator_agent.rs:1` |

### Detailed Descriptions

#### `.assign-<id>` — Agent Assignment

**What:** Selects the best agent (role + tradeoff combination) for a task using a lightweight LLM call. Evaluates available agents against task requirements, sets `agent`, `exec_mode`, and `context_scope` on the source task.

**When created:** At publish time via `scaffold_assign_task()` in `src/commands/eval_scaffold.rs:83`, or lazily by the coordinator during Phase 3 (`build_auto_assign_tasks`) at `src/commands/service/coordinator.rs:1037`. The assign task is created as a **blocking dependency** — the source task cannot start until `.assign-*` is Done.

**How it runs:** The coordinator's Phase 2 processes Open `.assign-*` tasks inline: it calls `run_lightweight_assignment()` (`src/commands/service/assignment.rs:200`) which builds a prompt with the agent catalog, task details, and assignment criteria, then makes a single LLM API call via `run_lightweight_llm_call()` with `DispatchRole::Assigner`. If the exec command is present, it can also be spawned as an inline task (`spawn_assign_inline`, `src/commands/service/coordinator.rs:2244`).

**Output:** Sets `task.agent`, `task.exec_mode`, `task.context_scope` on the source task. Records a `TaskAssignmentRecord` in `.workgraph/agency/assignments/`. Marks `.assign-*` as Done with a description summarizing the assignment.

**Identity:** Uses `config.agency.assigner_agent` if configured. Otherwise runs bare.

**Skips:** System tasks (dot-prefixed), tasks tagged `assignment`, `evaluation`, `evolution`, or `flip` (DOMINATED_TAGS).

#### `.place-<id>` — Graph Placement

**What:** Analyzes a draft task and wires it into the graph by adding dependency edges, then publishes it (unpauses).

**When created:** Coordinator tick Phase 2.9 (`build_placement_tasks`, `src/commands/service/coordinator.rs:742`). Only for tasks that are `paused=true`, not `unplaced`, not system tasks, and not already tagged `placed`.

**Fast path (no agent):** If the draft task already has `--after` deps AND no file overlap with active tasks' artifacts, it's auto-placed immediately — no `.place-*` task is created (`src/commands/service/coordinator.rs:813`).

**Agent path:** When the fast path doesn't apply (no deps, or file overlap detected), a `.place-<id>` task is created with placement context including active tasks, their artifacts, recently completed tasks, and integration gates. The agent analyzes this and runs `wg edit <id> --add-after <dep>` then `wg publish <id>`.

**Why intermittent:** Placement only fires for draft tasks. Tasks created with `wg add` (not `wg draft`) go directly to Open and skip placement entirely. The fast-path auto-placement handles most drafted tasks silently without creating a visible `.place-*` task.

**Identity:** Uses `config.agency.placer_agent` if configured. Otherwise runs bare.

#### `.flip-<id>` — Fidelity via Latent Intent Probing

**What:** Two-phase roundtrip intent fidelity evaluation:
1. **Inference** (`DispatchRole::FlipInference`): An LLM sees only the task output (artifacts, logs, diff) and reconstructs what the original prompt/task must have been.
2. **Comparison** (`DispatchRole::FlipComparison`): A second LLM compares the inferred prompt to the actual task description and scores similarity.

**When created:** At publish time via `scaffold_flip_task()` (`src/commands/eval_scaffold.rs:36`), called from `scaffold_eval_task()` when FLIP is enabled (globally via `config.agency.flip_enabled` or per-task via the `flip-eval` tag).

**How it runs:** As an inline task with exec command `wg evaluate run <task-id> --flip`. The coordinator spawns it via `spawn_eval_inline()` (`src/commands/service/coordinator.rs:2061`). The `run_flip()` function (`src/commands/evaluate.rs:580`) executes both phases sequentially.

**Output:** An `Evaluation` record with `source: "flip"` saved to `.workgraph/agency/evaluations/`. Dimensions: `semantic_match`, `requirement_coverage`, `specificity_match`, `hallucination_rate`.

**Dependency chain:** `.flip-<id>` depends on `<id>` (source task). `.evaluate-<id>` depends on `.flip-<id>` (when FLIP is enabled), so eval incorporates the FLIP score.

#### `.evaluate-<id>` — Post-Task Evaluation

**What:** Produces a structured evaluation of the completed task, scoring quality dimensions (correctness, completeness, efficiency, style_adherence) and organizational impact (downstream_usability, coordination_overhead, blocking_impact). Incorporates FLIP score if available.

**When created:** At publish time via `scaffold_eval_task()` (`src/commands/eval_scaffold.rs:160`), or by the coordinator in Phase 4 (`build_auto_evaluate_tasks`, `src/commands/service/coordinator.rs:1438`) as a catch-all for tasks not published with eager scaffolding.

**How it runs:** Inline via `spawn_eval_inline()`. Exec command: `wg evaluate run <task-id>`. The `run()` function (`src/commands/evaluate.rs:103`) loads the task's agent identity, artifacts, git diff, downstream tasks, and FLIP/verify-flip data, then calls the LLM.

**Output:** `Evaluation` record saved to `.workgraph/agency/evaluations/`. Updates agent/role/tradeoff performance records via `record_evaluation_with_inference()`. Can trigger the **eval gate** (`check_eval_gate`, `src/commands/evaluate.rs:1320`) which fails the source task if the score is below `eval_gate_threshold`.

**Dependency chain:** Depends on `.flip-<id>` (if FLIP enabled) or directly on `<id>`. When FLIP verification triggers, `.evaluate-<id>` also gains a dep on `.verify-flip-<id>`.

**Identity:** Uses `config.agency.evaluator_agent` if configured. The evaluator's own performance is also tracked via a meta-evaluation.

#### `.verify-flip-<id>` — FLIP Verification

**What:** Independently verifies whether a task's work was actually completed, triggered when the FLIP score falls below a configurable threshold. Uses a premium model (Opus by default) for reliable verification.

**When created:** Coordinator tick Phase 4.5 (`build_flip_verification_tasks`, `src/commands/service/coordinator.rs:1538`). Only when `config.agency.flip_verification_threshold` is set and a FLIP evaluation falls below it.

**How it runs:** Spawned as a regular agent (not inline). The task description includes the original task's description, verification commands, and instructions to independently check git log, run tests, and verify artifacts. The agent uses `wg log` and `wg fail` to record the verdict.

**Output:** Log entries on the source task. If verification fails, the source task is failed via `wg fail`. The verification task itself also gets scaffolded with `.evaluate-.verify-flip-<id>`.

**Side effect:** `.evaluate-<id>` gains `.verify-flip-<id>` as an additional dependency (`src/commands/service/coordinator.rs:1717`), so evaluation waits for verification to complete.

**Identity:** Currently runs **bare** (no `agent` field set, `src/commands/service/coordinator.rs:1667`). This is an identity gap — see Identity Gaps section.

#### `.evolve-*` — Agency Evolution

**What:** Runs `wg evolve` to mutate/refine agency roles, tradeoffs, and components based on accumulated evaluation data. Uses safe strategies (mutation, not crossover/bizarre-ideation).

**When created:** Coordinator tick Phase 4.6 (`build_auto_evolve_task`, `src/commands/service/coordinator.rs:1746`). Requires `config.agency.auto_evolve = true`. Two triggers:
1. **Threshold:** N new evaluations since last evolution (configurable via `evolution_threshold`)
2. **Reactive:** Average score drops below `evolution_reactive_threshold`

**At most one `.evolve-*` task is active at a time.**

**Identity:** Uses `config.agency.evolver_agent` if configured.

#### `.create-*` — Primitive Store Expansion

**What:** Runs `wg agency create` to expand the primitive store with new role components, desired outcomes, and tradeoff configurations.

**When created:** Two paths:
1. **Auto-create threshold:** Coordinator tick Phase 4.7 (`build_auto_create_task`, `src/commands/service/coordinator.rs:1915`). When N completed tasks since last creation exceeds `auto_create_threshold`.
2. **Assigner signal:** When an assigner's verdict has `create_needed: true`, indicating no good agent match exists (`src/commands/service/coordinator.rs:1333`).

**Identity:** Uses `config.agency.creator_agent` if configured.

#### `.respond-to-<id>` — Message-Triggered Resurrection Child

**What:** Handles unread messages on a completed task when downstream tasks are already active (so reopening the parent would cause conflicts).

**When created:** Coordinator tick Phase 2.8 (`resurrect_done_tasks`, `src/commands/service/coordinator.rs:529`). Triggered when a Done task has unread messages from whitelisted senders (user, coordinator, or other agents). If no downstream is active, the task is simply reopened instead of creating a child.

**Identity:** Inherits the parent task's `session_id` and `checkpoint` for continuity.

#### `.coordinator-*` — Persistent Coordinator Agent

**What:** A long-lived LLM session inside the service daemon for user chat interaction. Not a task in the graph — it's a subprocess managed by the daemon.

**When created:** On `wg service start`. Managed by `CoordinatorAgent` in `src/commands/service/coordinator_agent.rs`.

**Features:** Auto-restart with crash recovery (max 3 restarts/10 min), conversation history rotation, context refresh on each message with graph summary and recent events.

## Eval Scaffold System

The eval scaffold system (`src/commands/eval_scaffold.rs`) eagerly creates lifecycle tasks at **publish time**, so every published task has a complete lifecycle chain as real graph edges:

```
.assign-foo → foo → .flip-foo → .evaluate-foo
```

### When it fires

1. **At publish time:** `wg publish` calls `scaffold_assign_tasks_batch()` and `scaffold_eval_tasks_batch()` for all tasks being published.
2. **At coordinator tick time:** As a catch-all, `build_auto_evaluate_tasks()` (Phase 4) and `build_auto_assign_tasks()` (Phase 3) scaffold tasks that weren't published with eager scaffolding (backward compatibility for tasks created via `wg add` before the scaffold system existed).

### What it creates

| Function | Creates | Dependency edge | Code |
|----------|---------|-----------------|------|
| `scaffold_assign_task()` | `.assign-<id>` | `.assign-<id>.before = [<id>]` + `<id>.after += [.assign-<id>]` (blocking) | `eval_scaffold.rs:83` |
| `scaffold_flip_task()` | `.flip-<id>` | `.flip-<id>.after = [<id>]` | `eval_scaffold.rs:36` |
| `scaffold_eval_task()` | `.evaluate-<id>` (+ `.flip-<id>` if FLIP enabled) | `.evaluate-<id>.after = [.flip-<id>]` or `[<id>]` | `eval_scaffold.rs:160` |

### Idempotency

All scaffold functions are idempotent — they check if the target task already exists before creating. The source task is tagged with `eval-scheduled` after scaffolding to prevent recreation after GC.

### Exclusions (DOMINATED_TAGS)

Tasks tagged with `evaluation`, `assignment`, `evolution`, or `flip` do NOT get their own eval/assign/flip tasks (prevents infinite regress).

## Assignment vs Placement

These are two distinct mechanisms that operate at different stages:

| | Assignment | Placement |
|---|---|---|
| **Question answered** | "Which agent should do this task?" | "Where does this task fit in the graph?" |
| **Operates on** | Ready, unassigned tasks | Draft (paused) tasks |
| **Creates** | `.assign-<id>` | `.place-<id>` (or auto-places) |
| **Output** | Sets `task.agent`, `task.exec_mode`, `task.context_scope` | Adds `--after` dependency edges, then publishes (unpauses) |
| **When in tick** | Phase 3 | Phase 2.9 (before assignment) |
| **Required?** | Yes (always happens for non-system tasks when auto_assign=true) | Only for drafted tasks (tasks from `wg draft`); `wg add` tasks skip placement |
| **LLM call** | Always (lightweight, single API call via `DispatchRole::Assigner`) | Only when fast-path fails (no deps or file overlap detected) |
| **Code** | `coordinator.rs:1037` (scaffold) + `assignment.rs:200` (LLM) | `coordinator.rs:742` |

**Why placement seems intermittent:** Most tasks are created with `wg add` (which creates Open tasks directly), not `wg draft` (which creates paused tasks). Placement only applies to drafted tasks. Additionally, the auto-place fast path silently handles tasks that have explicit `--after` deps and no file overlap — these never create a visible `.place-*` task.

## Model Routing — DispatchRole → Model Resolution

Each auxiliary agent type maps to a `DispatchRole` (`src/config.rs:431`), which determines its model via a 6-level cascade:

### DispatchRole → Default Tier Mapping

| DispatchRole | Default Tier | Default Model | Used by |
|---|---|---|---|
| `Triage` | Fast | claude-haiku-4-5 | Dead-agent triage, checkpoint summaries |
| `FlipComparison` | Fast | claude-haiku-4-5 | FLIP Phase 2 |
| `Assigner` | Fast | claude-haiku-4-5 | `.assign-*` tasks |
| `Compactor` | Fast | claude-haiku-4-5 | Context compaction |
| `CoordinatorEval` | Fast | claude-haiku-4-5 | Coordinator per-turn scoring |
| `Placer` | Fast | claude-haiku-4-5 | `.place-*` tasks |
| `FlipInference` | Standard | claude-sonnet-4 | FLIP Phase 1 |
| `TaskAgent` | Standard | claude-sonnet-4 | Main task agents |
| `Evaluator` | Standard | claude-sonnet-4 | `.evaluate-*` tasks |
| `Evolver` | Standard | claude-sonnet-4 | `.evolve-*` tasks |
| `Creator` | Premium | claude-opus-4-6 | `.create-*` tasks |
| `Verification` | Premium | claude-opus-4-6 | `.verify-flip-*` tasks |

*(Source: `src/config.rs:524`, `default_tier()` method)*

### Model Resolution Cascade (`resolve_model_for_role`, `src/config.rs:978`)

1. **Role-specific `[models]` config** — `[models.evaluator]`, `[models.triage]`, etc.
2. **Legacy per-role config** — `agency.evaluator_model`, `agency.assigner_model`, etc. (deprecated)
3. **Tier override** — `[models.<role>].tier = "premium"` → look up tier's model in registry
4. **Role default tier** — `DispatchRole::default_tier()` → tier registry lookup
5. **`[models.default]`** — fallback model in `[models]` section
6. **`agent.model`** — global fallback

## Coordinator Tick Phases

The coordinator tick (`coordinator_tick`, `src/commands/service/coordinator.rs:2820`) runs periodically (configurable interval). Here is the complete phase sequence:

| Phase | What | Code line |
|-------|------|-----------|
| 0 | Process chat inbox | `:2834` |
| 1 | Clean up dead agents (triage) | `:2837` |
| 1.3 | Zero-output agent detection/kill | `:2844` |
| 1.5 | Auto-checkpoint alive agents | `:2865` |
| 2 | Load graph | `:2868` |
| 2.5 | Cycle iteration (reactivate completed cycles) | `:2877` |
| 2.6 | Cycle failure restart | `:2893` |
| 2.7 | Evaluate waiting tasks (wait conditions) | `:2909` |
| 2.8 | Message-triggered resurrection | `:2919` |
| 2.9 | Build placement tasks (draft → published) | `:2929` |
| 3 | Auto-assign (scaffold + LLM assignment) | `:2939` |
| 4 | Auto-evaluate (scaffold eval/flip tasks) | `:2944` |
| 4.5 | FLIP verification (low-score tasks) | `:2950` |
| 4.6 | Auto-evolve (evolution trigger) | `:2953` |
| 4.7 | Auto-create (primitive store expansion) | `:2958` |
| 5 | Check for ready tasks | `:2971` |
| 5.5 | Check global API-down backoff | `:2976` |
| 6 | Spawn agents for ready tasks | `:2988` |

## Typical Task Sequence Walkthrough

A user creates a task:
```bash
wg add "Implement auth endpoint" --after setup-db \
  --verify "cargo test test_auth passes" \
  -d "Add POST /auth/token endpoint..."
```

1. **Task created** as Open in the graph.

2. **Coordinator tick Phase 2.9:** Task is not drafted (paused=false), so placement is skipped.

3. **Coordinator tick Phase 3:** `build_auto_assign_tasks` detects a ready, unassigned task without `.assign-*`. Calls `scaffold_assign_task()` which creates `.assign-implement-auth` with `before: [implement-auth]` and adds `.assign-implement-auth` to `implement-auth.after`. The source task is now blocked.

4. **Same tick (Phase 3 continued):** The `.assign-implement-auth` task is Open, so the coordinator runs `run_lightweight_assignment()` — a single Haiku API call that evaluates the agent catalog and selects the best match. Sets `task.agent = "agent-abc123"`, `task.exec_mode = "full"`, `task.context_scope = "task"`. Marks `.assign-*` as Done.

5. **Coordinator tick Phase 4:** `build_auto_evaluate_tasks` creates `.flip-implement-auth` (if FLIP enabled) and `.evaluate-implement-auth`. Tags source task with `eval-scheduled`.

6. **Coordinator tick Phase 6:** Source task `implement-auth` is now ready (`.assign-*` is Done, `setup-db` is Done). The coordinator resolves the agent's executor and model, then calls `spawn_agent()` to start a Claude Code session.

7. **Agent executes:** Works on the task, creates files, runs tests, calls `wg done implement-auth`.

8. **Next tick Phase 1:** Dead agent cleanup detects the finished agent process, extracts token usage and session ID from stream files.

9. **Next tick Phase 6:** `.flip-implement-auth` is now ready (source task Done). Spawned inline via `spawn_eval_inline()` → runs `wg evaluate run implement-auth --flip`:
   - Phase 1 (FlipInference/Sonnet): sees only output, reconstructs prompt
   - Phase 2 (FlipComparison/Haiku): compares inferred vs actual prompt
   - Records FLIP evaluation (e.g., score 0.82)

10. **Next tick Phase 6:** `.evaluate-implement-auth` is now ready (`.flip-*` Done). Spawned inline → runs `wg evaluate run implement-auth`:
    - Loads agent identity, artifacts, git diff, downstream tasks, FLIP score
    - Evaluator LLM (Sonnet) produces structured evaluation
    - Records evaluation, updates agent/role performance
    - Checks eval gate (if configured)

11. **If FLIP score < threshold (Phase 4.5):** Creates `.verify-flip-implement-auth` (Opus), which independently verifies the work by running tests and checking git.

12. **If enough evaluations accumulate (Phase 4.6):** Creates `.evolve-auto-*` to evolve agency primitives.

## Identity Gaps

Auxiliary tasks that **currently run bare** (no configured agent identity) but could benefit from being evolvable:

| Task type | Current identity | Why it should be evolvable |
|-----------|-----------------|---------------------------|
| `.verify-flip-<id>` | **Bare** (`agent: None`, `coordinator.rs:1667`) | Verification quality varies; tracking which verification strategies produce reliable verdicts would improve the system |
| `.respond-to-<id>` | **Inherits parent** (no own identity) | These are ephemeral and inherit context, so bare is acceptable |
| `.coordinator-*` | **System process** (not an agent) | Not a graph task, so identity doesn't apply |

All other auxiliary types have configurable agent identity fields:
- `.assign-*` → `config.agency.assigner_agent`
- `.evaluate-*` → `config.agency.evaluator_agent`
- `.flip-*` → `config.agency.evaluator_agent` (shared with evaluate)
- `.evolve-*` → `config.agency.evolver_agent`
- `.create-*` → `config.agency.creator_agent`
- `.place-*` → `config.agency.placer_agent`

The **`.verify-flip-*`** task is the clearest identity gap. It runs with Premium tier (Opus) and makes high-stakes pass/fail decisions, but has no agent identity for performance tracking or evolution. Adding `config.agency.verifier_agent` would close this gap.
