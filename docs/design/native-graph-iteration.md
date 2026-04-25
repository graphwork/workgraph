# Native Graph Iteration — Design Document

**Date:** 2026-04-10
**Task:** design-native-graph-iteration
**Status:** Design Recommendation

---

## Executive Summary

Currently FLIP uses a **scaffold pattern** — separate `.flip-*` and `.verify-flip-*` tasks that run after a task completes, triggering re-runs when fidelity is low. This works but is bolted-on: iteration is a post-hoc scaffolding concern, not a first-class graph primitive.

The proposed design makes iteration **native to the graph structure itself**. A task can self-retry, marking itself as needing another pass. The graph tracks iteration context (round number, agent used, key decisions) and propagates it downstream. Completed tasks can be **reverted to Open** when a new iteration of a dependency occurs, ensuring downstream tasks re-evaluate whether they need to re-run.

This document specifies the state machine, propagation rules, API surface, and migration path.

---

## 1. Task State Transitions

### 1.1 The Core Problem with "Done"

In the current model, `Done` is terminal. The FLIP scaffold works around this by creating separate verification tasks, but this means:

- "Done" tasks can't be self-improved — only externally verified
- Iteration history is invisible in the task itself (scattered across `.verify-*` task logs)
- Downstream tasks don't know a dependency iterated — they just see "done"

### 1.2 Proposed State Machine

**Add a new compound state concept: `iterated_count`** — a counter on every task recording how many times it has been iterated. A task is conceptually "done/iterated-N" where N is this count.

The actual `Status` enum does NOT change. Instead, `Done` gains semantics:

```
Done (iterated_count = 0)  → Task completed cleanly, never iterated
Done (iterated_count = N)   → Task completed after N iterations
Failed (iterated_count = N) → Task failed on iteration N
```

This preserves backward compatibility: existing `Status` serialization, CLI output, and graph analysis code that treats `Done` as terminal remains correct.

### 1.3 Self-Retry Mechanism

An agent signals self-retry by emitting a special directive in its output:

```
__WG_ITERATE__:{"reason":"quality below threshold","agent":"opus","model":"claude-opus-4-latest"}
```

The coordinator detects this directive when the task transitions to `Done`. On detection:

1. **Task reverts to `Open`** (not `InProgress` — agent must be reassigned)
2. **`retry_count` increments** (existing field — already tracks retries after failure)
3. **`loop_iteration` increments** (only for self-retry; structural cycles use this separately)
4. **Agent is cleared** — new agent will be assigned on next coordinator tick
5. **Iteration record is appended** to `iteration_history`

The key difference from `wg retry` (manual retry for Failed tasks):
- Self-retry happens on a **Done** task that wants to improve itself
- Manual retry recovers from **Failure**
- Both use `retry_count` but self-retry also records iteration context

### 1.4 Task State Transitions Diagram

```
                    ┌─────────────────────────────────────────┐
                    │                                         │
                    ▼                                         │
Open ──► InProgress ────────────────────────────────────────►│
                    │                                         │
                    │  [agent completes successfully]         │
                    ▼                                         │
              ┌──── Done                                      │
              │      │                                        │
              │      │  __WG_ITERATE__ detected               │
              │      ├─────────────────────────────────────┐  │
              │      │                                     │  │
              │      ▼                                     │  │
              │  Open (retry_count++, loop_iteration++)    │  │
              │      │                                     │  │
              │      │  agent completes again              │  │
              │      ▼                                     │  │
              │  Done (iterated_count = N) ◄───────────────┘  │
              │      │                                        │
              │      │  max_retries exceeded                 │
              │      ▼                                        │
              └──── Failed                                    │
                    │                                         │
                    │  wg retry                              │
                    ▼                                         │
                  Open (retry_count preserved) ───────────────┘
```

### 1.5 Iteration History Field

Add a new field to `Task`:

```rust
/// History of iterations for this task. Each entry records one self-retry cycle.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub iteration_history: Vec<IterationRecord>,
```

Where:

```rust
pub struct IterationRecord {
    /// Which iteration this was (1-indexed)
    pub iteration: u32,
    /// Agent composition used in this iteration
    pub agent: Option<String>,
    /// Model used in this iteration
    pub model: Option<String>,
    /// Why the iteration was triggered
    pub reason: String,
    /// Token usage for this iteration
    pub token_usage: Option<TokenUsage>,
    /// When this iteration completed
    pub completed_at: String,
    /// What the agent decided/produced in this iteration (summary)
    pub summary: Option<String>,
}
```

This is append-only. The current `loop_iteration` field is incremented on self-retry (in addition to its existing use for structural cycles). The `retry_count` field continues to track retries-after-failure as before.

### 1.6 Manual Retry Trigger

Users can trigger iteration manually:

```bash
wg retry <task-id> --reason "manual review"        # For Failed tasks (existing)
wg iterate <task-id> --reason "improve quality"   # NEW: For Done tasks
```

The `wg iterate` command reverts a Done task to Open, increments `retry_count` and `loop_iteration`, and records an `IterationRecord`. Unlike self-retry, manual iteration does NOT require `--after` reprocessing — the task simply becomes available for reassignment.

---

## 2. Iteration Context Propagation

### 2.1 How Downstream Knows About Iterations

When task A iterates (Done → Open → Done), task B that depends on A via `before` or `after` edges needs to know. There are two propagation strategies:

**Option A: Automatic Re-run (aggressive)**
When A iterates, all downstream tasks are reverted to `Open` and must re-confirm they are still satisfied. This is safe but can cause cascade thrashing.

**Option B: Annotation Only (passive) — RECOMMENDED**
When A iterates, downstream tasks receive iteration context but are NOT automatically reverted. Instead:
- A's `iteration_history` is visible in `wg show B`
- B can self-retry if its agent detects the iteration context and decides re-evaluation is needed
- A new field `downstream_iteration_trigger` on Task records "this task was triggered by iteration N of {dep}"

The coordinator's task readiness check is extended: a Done task whose direct dependency has an `iterated_count` greater than the last-known-iterated-count at task completion should be flagged for review, but not auto-reverted.

### 2.2 Iteration Context Fields

Add to `Task`:

```rust
/// Chain of iteration events that triggered or influenced this task.
/// Populated when a task is re-evaluated due to a dependency iterating.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub iteration_triggers: Vec<IterationTrigger>,
```

```rust
pub struct IterationTrigger {
    /// The task ID that iterated
    pub source_task: String,
    /// The iteration number it moved to
    pub source_iteration: u32,
    /// When the trigger was detected
    pub detected_at: String,
    /// Whether this task was reverted as a result
    pub reverted: bool,
}
```

### 2.3 Propagation Algorithm

When task X transitions to `Done` with an incremented `loop_iteration`:

1. For each task Y in `X.before` (tasks that depend on X):
   - Append an `IterationTrigger` to `Y.iteration_triggers`
   - Log: `"Downstream task '{Y}' notified of iteration {N} of '{X}'"`
2. The coordinator's readiness check for Y (when Y is `Done`) compares:
   - `Y.last_known_dependency_iteration` — the max iteration of any dependency at the time Y completed
   - Current max iteration of any dependency
   - If current > last_known: Y is flagged "may need re-evaluation" (status unchanged)

### 2.4 Auto-reversion Option

For pipelines where downstream correctness depends on upstream output (e.g., integration test depends on implementation), add a task tag:

```bash
wg add "Integration test" --after implement --tag iterate-on-dependency-change
```

When a dependency iterates, tasks with this tag are automatically reverted to `Open`. The agent is cleared for reassignment, and the coordinator picks them up in the next tick.

This is the "flip done → incomplete" behavior requested in the vision.

---

## 3. Agent Reassignment on Retry

### 3.1 Reassignment Strategy Options

**Option A: Same agent** — preserves context, faster for incremental work
**Option B: Fresh agent** — avoids confirmation bias, can catch errors
**Option C: Configurable per task or globally** — flexible but complex

**Recommendation: Option C, defaulting to fresh agent**

Default behavior: when a task self-retries or is manually iterated, the `assigned` field is cleared and the coordinator re-runs assignment from scratch. This is the safest default.

Override via task field:

```rust
/// If Some, prefer this agent on retry. If None, reassign fresh.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub retry_agent: Option<String>,
```

Or via CLI:

```bash
wg iterate <task-id> --agent <agent-id>   # Pin agent for retry
wg iterate <task-id> --fresh             # Force fresh assignment (default)
```

### 3.2 Model Selection on Retry

The coordinator already supports `tried_models` for tier escalation on spawn failure. Extend this for iteration:

- When a task iterates, the previous model is added to `tried_models`
- The assigner sees `tried_models` and can choose the same model (appropriate for incremental fixes) or escalate to a more capable model (appropriate for fundamental rework)
- The coordinator config `retry_context_tokens` controls how much of the previous attempt's context is injected (already exists, applies here)

### 3.3 Iteration Summary for Agent Context

When an agent is assigned to an iterating task, the prompt includes:

```
## Iteration History

This task has been iterated {N} time(s).

Iteration 1: agent=opus model=claude-opus-4-latest reason="initial attempt"
  Summary: "Implemented X, Y, Z but performance was poor"

Iteration 2: agent=sonnet model=claude-sonnet-4 reason="performance below threshold"
  Summary: "Optimized algorithms but introduced a bug in W"

You are now on iteration 3. Previous work is in artifacts. Review the iteration
history and decide: continue from the last approach, or try a different strategy?
```

This uses the `iteration_history` field.

---

## 4. Integration with Existing FLIP Scaffold

### 4.1 The Two Systems Are Complementary

The existing FLIP scaffold (`.evaluate-*`, `.flip-*`, `.verify-flip-*` tasks) and the proposed native iteration are orthogonal concerns:

- **FLIP scaffold**: Evaluates task output quality after completion, triggers verification when fidelity is low
- **Native iteration**: Enables a task to self-improve by retrying, with history tracked in the task itself

They compose: a task completes → FLIP evaluates → FLIP score is low → `.verify-flip-*` task runs → verification agent fixes issues → **native iteration** records the fix as iteration N.

### 4.2 Migration Path

Phase 1: Add iteration primitives (no FLIP changes)
- Add `iteration_history` and `iteration_triggers` fields to Task
- Implement `__WG_ITERATE__` directive detection in coordinator
- Implement `wg iterate` CLI command
- Implement iteration context propagation

Phase 2: Wire FLIP to native iteration (opt-in)
- When `.verify-flip-*` completes successfully, record the fix as a native iteration of the source task (not a separate task)
- The source task's `iteration_history` gains an entry from the verification agent's work
- The separate `.verify-flip-*` task can be marked as absorbed (its work is now part of the source task's history)

Phase 3: Deprecate scaffold for self-retry use cases
- For self-improvement (not external verification), prefer native iteration over `.verify-flip-*`
- `.verify-flip-*` remains for cases where a separate Opus-class agent is genuinely needed
- Document the boundary: "native iteration for same-agent self-improvement, `.verify-flip-*` for expert verification"

### 4.3 FLIP Task Lifecycle Under Native Iteration

```
Task completes
  → .evaluate-* (quality eval, runs)
  → .flip-* (fidelity eval, runs in parallel)
  → if FLIP < threshold:
       .verify-flip-* created (separate agent, runs)
         → on success: native iteration recorded on source task
         → source task becomes "done/iterated-N"
         → downstream notified via iteration_triggers
```

The `.verify-flip-*` task still exists for expert verification, but its outcome is now recorded as a native iteration of the source task rather than a separate artifact.

---

## 5. UX Semantics

### 5.1 TUI Display

**Task list view:**
```
task-a     ● Done/iter3  3 iterations ago  [agent: opus]
task-b     ◐ Done        may need review (dep iterated)  [agent: sonnet]
task-c     ● Done
```

**Task detail view (for iterated task):**
```
Status: Done (iteration 3 of 3)
Iteration History:
  1. agent=opus model=claude-opus-4-latest  2026-04-10  "Initial implementation"
  2. agent=sonnet model=claude-sonnet-4  2026-04-10  "Performance fix — introduced bug in W"
  3. agent=opus model=claude-opus-4-latest  2026-04-10  "Bug fix for W"
```

**Downstream iteration notification:**
```
task-b is marked "may need review" because:
  └─ task-a iterated (now at iteration 3, was at iteration 1 when task-b completed)
    Run `wg iterate task-b --reason "dependency iterated"` to re-evaluate.
```

### 5.2 CLI Commands

```bash
# Manual iteration (new command)
wg iterate <task-id> [--reason <text>] [--agent <agent-id>|--fresh] [--force]

# List iteration history
wg show <task-id> --iteration-history

# Trigger downstream re-evaluation
wg reevaluate <task-id>   # Reverts task to Open if deps have iterated

# Suppress iteration warning for a downstream task
wg acknowledge <task-id>  # Clears iteration_triggers, marks as "reviewed"
```

### 5.3 Coordinator Loop Interaction

The coordinator's tick loop already handles cycle iteration via `evaluate_cycle_iteration`. Native self-retry is orthogonal:

- **Structural cycle iteration**: Controlled by `cycle_config` on the cycle header task. All members reactivate together. `loop_iteration` increments.
- **Native self-retry**: Triggered by `__WG_ITERATE__` directive on an individual task. Only that task reactivates. `loop_iteration` increments independently.

The two CAN interact: a task in a structural cycle can self-retry within an iteration, and the cycle can still iterate to the next round. Both increment `loop_iteration`, but the meaning differs:
- Structural cycle iteration: whole-system progress round
- Self-retry: individual task refinement within a round

Both are valid uses of `loop_iteration`. The `iteration_history` disambiguates: structural cycle iterations have no `IterationRecord` (they're not self-initiated), while native iterations do.

---

## 6. State Machine Summary

### 6.1 Task Fields Added/Modified

**New fields on `Task`:**

| Field | Type | Purpose |
|-------|------|---------|
| `iteration_history` | `Vec<IterationRecord>` | Append-only history of self-retry cycles |
| `iteration_triggers` | `Vec<IterationTrigger>` | Notification log of dependency iterations |
| `downstream_iteration_trigger` | `Option<u32>` | Last-known max dependency iteration (for may-need-review flag) |
| `retry_agent` | `Option<String>` | Preferred agent on retry (overrides fresh assignment) |

**Modified fields:**

| Field | Change |
|-------|--------|
| `loop_iteration` | Now incremented on native self-retry (in addition to structural cycle iteration) |
| `retry_count` | Already exists; used for retry budget tracking |

### 6.2 New Types

```rust
pub struct IterationRecord {
    pub iteration: u32,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub reason: String,
    pub token_usage: Option<TokenUsage>,
    pub completed_at: String,
    pub summary: Option<String>,
}

pub struct IterationTrigger {
    pub source_task: String,
    pub source_iteration: u32,
    pub detected_at: String,
    pub reverted: bool,
}
```

### 6.3 Coordinator Tick Changes

**Phase 0 (existing):** Triage dead agents, reconcile orphaned tasks
**Phase 1 (existing):** Auto-assign ready tasks
**Phase 2 (new):** Detect `__WG_ITERATE__` directives on completed tasks → self-retry
**Phase 3 (existing):** Execute ready tasks
**Phase 4 (existing):** Auto-evaluate completed tasks
**Phase 4.5 (existing):** FLIP-triggered verification
**Phase 5 (new):** Propagate iteration context to downstream tasks

---

## 7. API and CLI Surface

### 7.1 New CLI Commands

```bash
# Iterate a done task
wg iterate <task-id> [options]

# Options:
#   --reason <text>          Why this iteration is happening
#   --agent <agent-id>       Pin specific agent for retry
#   --fresh                 Force fresh assignment (default)
#   --model <model>         Pin model for retry
#   --force                 Iterate even if task is not done

# Show iteration history
wg show <task-id> --iterations

# Re-evaluate downstream (if deps iterated)
wg reevaluate <task-id>

# Acknowledge iteration trigger (suppress warning)
wg acknowledge <task-id>
```

### 7.2 New Graph Directive

Agents emit this in stdout to self-retry:

```
__WG_ITERATE__:{"reason":"<text>","agent":"<agent-id>","model":"<model>","summary":"<text>"}
```

The coordinator parses this from the task's output.log after the task transitions to Done.

### 7.3 Configuration

In `config.yaml`:

```yaml
agency:
  # Default retry strategy: "fresh" (reassign) or "same" (preserve agent)
  retry_strategy: fresh
  
  # Max native iterations per task (0 = unlimited, subject to retry_count budget)
  max_native_iterations: 5
  
  # Auto-revert downstream tasks with iterate-on-dependency-change tag
  auto_iterate_tagged_downstream: true
```

---

## 8. Migration Plan for Existing FLIP Tasks

### 8.1 Immediate (No Breaking Changes)

- Add `iteration_history` and `iteration_triggers` fields as optional/empty by default
- Existing tasks have empty iteration history — correct, they haven't been iterated
- `.verify-flip-*` tasks continue to work exactly as today

### 8.2 Short-term (Backwards-Compatible)

- Wire `.verify-flip-*` completion to record a native iteration on the source task
- Source task gains visible iteration history
- Downstream tasks can opt into `iterate-on-dependency-change` tag
- `wg iterate` command available for manual use

### 8.3 Long-term (Deprecation of Scaffold for Self-Retry)

- FLIP scaffold remains for fidelity evaluation and expert verification
- Native iteration replaces self-retry use cases (where the same task wants another pass)
- Document clear decision tree: "Need same-agent refinement → native iteration; Need expert verification → `.verify-flip-*`"

### 8.4 Migration of Existing FLIP Evaluation Records

Existing `.flip-*` and `.verify-flip-*` task outputs remain as standalone tasks with their own logs. They are NOT absorbed into the source task's `iteration_history` unless explicitly wired in Phase 2. This preserves the audit trail for existing evaluations.

New evaluations use the native iteration mechanism, with FLIP evaluation remaining as a separate concern.

---

## 9. Implementation Phases

### Phase 1: Core Primitives (Medium complexity)

**Files:** `src/graph.rs` (Task struct, new types), `src/commands/service/coordinator.rs` (`__WG_ITERATE__` detection), `src/commands/iterate.rs` (new file)

1. Add `IterationRecord` and `IterationTrigger` types to `src/graph.rs`
2. Add `iteration_history` and `iteration_triggers` fields to `Task`
3. Implement `__WG_ITERATE__` directive detection in coordinator tick (Phase 2)
4. Create `wg iterate` command
5. Add `iteration_history` to `wg show` output
6. Tests: iteration state machine, directive detection

### Phase 2: Propagation (Medium complexity)

**Files:** `src/commands/service/coordinator.rs` (Phase 5), `src/graph.rs` (iteration propagation logic), `src/tui/viz_viewer/` (TUI display)

1. Implement downstream notification on task iteration (Phase 5 in coordinator)
2. Add "may need review" flag to downstream tasks
3. Add `iterate-on-dependency-change` tag handler for auto-reversion
4. TUI: display iterated count, iteration history, downstream warning
5. `wg reevaluate` and `wg acknowledge` commands
6. Tests: propagation, auto-reversion, TUI display

### Phase 3: FLIP Integration (Low complexity)

**Files:** `src/commands/service/coordinator.rs` (`.verify-flip-*` completion handler)

1. Wire `.verify-flip-*` completion to record native iteration on source task
2. Source task's `iteration_history` shows verification agent's work
3. Downstream notified via `iteration_triggers`
4. Tests: FLIP → native iteration wiring

### Phase 4: Configuration and Polish (Low complexity)

**Files:** `src/config.rs`, `src/cli.rs`

1. Add `retry_strategy`, `max_native_iterations` config options
2. Add `--agent`, `--fresh`, `--model` flags to `wg iterate`
3. Update documentation

---

## 10. Validation Checklist

- [x] **Section 1 (State Transitions):** State machine diagram with Done having iterated_count semantics; self-retry via `__WG_ITERATE__` directive; manual `wg iterate` command
- [x] **Section 2 (Propagation):** `iteration_triggers` on downstream tasks; optional auto-reversion via tag; "may need review" flag
- [x] **Section 3 (Agent Reassignment):** Configurable strategy (fresh vs. same); `retry_agent` override; `tried_models` integration; iteration history in prompt
- [x] **Section 4 (FLIP Integration):** Complementary systems; Phase 2 wiring of `.verify-flip-*` to native iteration; migration path with clear decision tree
- [x] **Section 5 (UX):** TUI display specs; CLI commands (`iterate`, `reevaluate`, `acknowledge`); coordinator loop interaction documented
- [x] **State machine:** New types, modified fields, coordinator tick phases
- [x] **API/CLI:** Complete command surface with options
- [x] **Migration plan:** Backwards-compatible phases; FLIP records preserved; decision tree for scaffold vs. native iteration
