# Design: Cyclic Process Support for Workgraph

**Date:** 2026-02-14
**Status:** Draft — awaiting review
**Dependencies:** [research-cyclic-processes](research/cyclic-processes.md), [dag-assumptions-survey](dag-assumptions-survey.md)

---

## Problem

Workgraph models task dependencies as a directed graph. While the documentation says cycles are allowed, and the codebase has been partially hardened for them (visited sets, back-edge rendering, cycle classification), **cyclic tasks cannot actually execute**:

1. `ready_tasks()` requires ALL blockers to be Done — a cycle member's blockers are never all Done.
2. `wg done` refuses to mark a task done if any blocker is unresolved — in a cycle, at least one always is.
3. The coordinator never dispatches cyclic tasks because they're never "ready."

Real workflows need cycles: review-revise loops, CI retry pipelines, monitor-alert-fix-verify, sprint ceremonies. This design proposes a minimal mechanism to make them work.

---

## Design Principles

1. **Don't break the DAG.** Scheduling, topological sort, `wg ready`, critical path — all keep working. Loop edges are a separate concept layered on top.
2. **Explicit over implicit.** Cycles require opt-in via `loops_to` edges. No accidental infinite loops.
3. **Bounded by default.** Every loop edge must have `max_iterations`. No unbounded cycles.
4. **Observable.** Every re-activation is logged with iteration count.
5. **Incremental.** Each phase is independently useful.

---

## 1. Loop Edges — The Core Mechanism

### Concept

A new edge type, `loops_to`, represents a conditional back-edge. It says: "when this task completes, evaluate a condition — if true and iterations remain, re-open the target task."

Loop edges are **not** blocking edges. They are ignored by `ready_tasks()`, topological sort, and critical path. They only fire on task completion.

### Data Model Changes

**New struct in `graph.rs`:**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopEdge {
    /// Task ID to re-activate when this task completes
    pub target: String,
    /// Condition that must be true to loop (optional — loops unconditionally if absent)
    pub guard: Option<LoopGuard>,
    /// Hard cap on iterations (required — no unbounded loops)
    pub max_iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoopGuard {
    /// Loop if a specific task has this status
    TaskStatus { task: String, status: Status },
    /// Loop if iteration count < N (redundant with max_iterations but explicit)
    IterationLessThan(u32),
    /// Always loop (up to max_iterations)
    Always,
}
```

**New fields on `Task`:**

```rust
pub struct Task {
    // ... existing fields ...

    /// Back-edges that can re-activate upstream tasks on completion
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops_to: Vec<LoopEdge>,

    /// Current iteration (0 = first run, incremented on each re-activation)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub loop_iteration: u32,
}
```

### graph.jsonl Representation

```jsonl
{"id":"write-draft","title":"Write draft","status":"open","blocked_by":[],"loops_to":[],"loop_iteration":0}
{"id":"review-draft","title":"Review draft","status":"open","blocked_by":["write-draft"],"loops_to":[]}
{"id":"revise-draft","title":"Revise based on review","status":"open","blocked_by":["review-draft"],"loops_to":[{"target":"write-draft","guard":{"TaskStatus":{"task":"review-draft","status":"Failed"}},"max_iterations":3}]}
```

### Execution Semantics

When a task with `loops_to` edges transitions to Done:

1. For each `LoopEdge`:
   a. Evaluate the guard condition against the current graph state.
   b. If guard is true (or absent, meaning "always") **and** `target.loop_iteration < max_iterations`:
      - Set target task status to `Open`.
      - Clear target's `started_at` and `completed_at` timestamps.
      - Increment target's `loop_iteration`.
      - Add log entry: `"Re-activated by loop from {source} (iteration {n}/{max})"`.
      - Also re-open any tasks between source and target in the cycle (their blockers are no longer satisfied).
   c. If guard is false or iterations exhausted: do nothing — task stays Done, graph proceeds normally.

2. The re-opened target task will become "ready" through normal means once its non-loop blockers are satisfied. Since loop edges are not blocking edges, `ready_tasks()` doesn't need to change for this path.

### Key Insight: Loop Edges Don't Block

This is the critical design choice. `loops_to` edges:
- Are **not** in `blocked_by`. They don't prevent execution.
- Are **not** traversed by `ready_tasks()`. They don't affect scheduling.
- Are **only** evaluated when the source task completes.
- Are **rendered** distinctly (magenta upward arrows, as `dag_layout.rs` already does for back-edges).

This means we don't need to modify `ready_tasks()` or `wg done`'s blocker check at all. The cycle is expressed as a forward chain of `blocked_by` edges (which are acyclic) plus a backward `loops_to` edge (which is separate).

---

## 2. Task Status in Cycles

### Can a Done Task Re-Open?

Yes — but only via loop edges, not manually. The re-activation is:
- **Automatic**: triggered by a downstream task's completion + guard condition.
- **Bounded**: capped by `max_iterations`.
- **Logged**: every re-activation creates a log entry with the iteration count.

### No New Status Values Needed

The existing `Status` enum works:
- `Open` → task is available for work (or re-available after loop re-activation)
- `InProgress` → agent is working on it
- `Done` → completed (may be re-opened by a loop edge)
- `Failed` → used as a guard condition trigger ("loop if review failed")

Adding a `Recurring` status was considered and rejected — it conflates the task's current execution state with its lifecycle pattern. A task's "recurring-ness" is expressed by the existence of loop edges pointing at it, not by its status.

### Intermediate Tasks in the Cycle

When a loop edge re-opens task A, all tasks between A and the loop source must also be re-opened, because their blockers (including A) are no longer Done. This cascade is straightforward:

```
write-draft → review-draft → revise-draft --loops_to→ write-draft
```

When `revise-draft` completes and loops:
1. `write-draft` → re-opened to `Open` (direct target)
2. `review-draft` → re-opened to `Open` (blocker `write-draft` is no longer Done)
3. `revise-draft` stays `Done` (it just completed; it'll become ready again after `review-draft` completes)

Actually, `review-draft` doesn't need explicit re-opening — `ready_tasks()` already won't mark it ready because its blocker `write-draft` is Open. When `write-draft` completes again, `review-draft` naturally becomes ready. The only task that needs explicit status reset is the loop target itself.

---

## 3. DAG Assumptions: What Must Change vs. What's Fine

### Must Change (Phase 1)

| Location | Change | Why |
|----------|--------|-----|
| `graph.rs` Task struct | Add `loops_to: Vec<LoopEdge>` and `loop_iteration: u32` | Core data model |
| `parser.rs` | Parse `loops_to` and `loop_iteration` fields | Read/write new fields |
| `commands/done.rs` | After marking Done, evaluate loop edges | Trigger re-activation |
| `check.rs` `check_cycles` | Distinguish `blocked_by` cycles (problematic) from `loops_to` cycles (intentional) | Currently all cycles are detected via `blocked_by`; `loops_to` edges should be validated separately |
| `commands/loops.rs` | Show `loops_to` edges in cycle analysis | User visibility |

### Can Relax (Phase 2)

| Location | Change | Why |
|----------|--------|-----|
| `check.rs` `CheckResult.ok` | Don't set `ok=false` for `loops_to`-only cycles | These are intentional by construction |
| `commands/check.rs` | Show `loops_to` edges as "Loop edges" (green), not "Warning: Cycles" | UX clarity |

### Leave Alone

| Location | Why It's Fine |
|----------|--------------|
| `ready_tasks()` | Loop edges aren't in `blocked_by`, so readiness logic is unaffected |
| `done.rs` blocker check | Same — loop edges aren't blockers |
| Critical path (`critical_path.rs`) | Loop edges aren't dependency edges; critical path computation unchanged |
| `dag_layout.rs` back-edge rendering | Already handles back-edges; `loops_to` edges can reuse this rendering |
| `forecast.rs` (the unsafe one) | Separate bug — should get a visited set regardless of this feature |
| All `visited`-set-protected traversals | Already cycle-safe |

### Should Fix Independently

| Location | Issue |
|----------|-------|
| `forecast.rs:370-408` `find_longest_path_from()` | No visited set — will stack overflow on any cycle. Add `visited: &mut HashSet<String>`. Not related to `loops_to` but should be fixed. |

---

## 4. Coordinator Handling of Cyclic Tasks

### How the Service Daemon Handles Loops

The coordinator's tick loop (`commands/service.rs`) already follows this pattern:
1. Find ready tasks
2. Spawn agents for them

Loop re-activation happens in `wg done` (or `wg submit`), which the agent calls when it finishes. The flow:

1. Agent completes work, calls `wg done fix-bug`.
2. `done.rs` marks `fix-bug` as Done.
3. **New:** `done.rs` evaluates `fix-bug.loops_to` edges.
4. If a loop fires: target task (e.g., `investigate-bug`) is set to Open, iteration incremented.
5. Next coordinator tick: `ready_tasks()` finds `investigate-bug` is Open with all blockers Done → dispatches a new agent.

No changes to the coordinator's tick logic are needed. The coordinator just sees a task that's Open and ready — it doesn't need to know it was re-activated by a loop.

### Does the Same Agent Get Re-Spawned?

No, by default. Each iteration is a fresh dispatch — the coordinator spawns whatever agent is appropriate (via auto-assign or the task's `agent` field). This is correct because:
- The same agent type may not exist anymore (evolution).
- A fresh agent gets full context from the task's log, which now includes previous iteration results.
- If the user wants the same agent, they can set the task's `agent` field.

### Backpressure

If multiple loop edges fire simultaneously (e.g., a task has `loops_to` edges pointing at three different targets), all targets are re-opened at once. The coordinator's `max_agents` limit naturally provides backpressure — it won't spawn more agents than slots available.

---

## 5. Interaction with Agency/Evolution

### Agent Assignment per Iteration

Each loop iteration goes through the normal agent assignment flow:
- If `auto_assign` is on, the coordinator creates an `assign-{task-id}` subtask and an assigner agent picks the best agent for the task.
- If the task has a fixed `agent` field, that agent is used.
- The assignment can change between iterations — the assigner agent sees the task's iteration count and history, and may pick a different agent if previous iterations failed.

### Evolution Across Iterations

Loop iterations provide natural evaluation points. If `auto_evaluate` is on:
- Each iteration that completes triggers evaluation.
- Evaluation results accumulate, giving the evolution system data on how the agent/task pattern is performing.
- A consistently-failing loop (e.g., iterations 1-3 all fail review) signals that the agent type needs adjustment.

No special changes to the evolution system are needed — it already evaluates tasks on completion, and a re-activated task simply gets evaluated again each time.

---

## 6. CLI Interface

### Creating Loop Edges

```bash
# When adding a task:
wg add "Revise draft" --id revise-draft --blocked-by review-draft \
  --loops-to "write-draft" --loop-max 5

# With a guard condition:
wg add "Verify fix" --id verify-fix --blocked-by fix-bug \
  --loops-to "investigate-bug" --loop-guard "task:test-suite=Failed" --loop-max 3

# When editing an existing task:
wg edit revise-draft --add-loops-to "write-draft" --loop-max 5
```

### Viewing Loop State

```bash
# Show task with loop info:
wg show revise-draft
# Output includes:
#   Loops to: write-draft (guard: task:review-draft=Failed, max: 5, current iteration: 2)

# Show all loops in the graph:
wg loops
# Output includes loops_to edges alongside existing cycle detection

# In the TUI, loop edges shown as magenta upward arrows (already supported)
```

### Manual Loop Control

```bash
# Force-stop a loop (set iteration to max so it won't fire again):
wg edit write-draft --loop-iteration 999

# Reset iteration count (allow looping again):
wg edit write-draft --loop-iteration 0
```

---

## 7. Phased Implementation Plan

### Phase 1: Data Model + Loop Execution (Core)

**Goal:** Loop edges exist and fire on task completion.

**Files to change:**

| File | Change |
|------|--------|
| `src/graph.rs` | Add `LoopEdge` struct, `LoopGuard` enum. Add `loops_to: Vec<LoopEdge>` and `loop_iteration: u32` to `Task`. |
| `src/parser.rs` | Serialize/deserialize new fields in graph.jsonl. |
| `src/commands/done.rs` | After marking task Done, call new `evaluate_loop_edges()` function. Re-open target tasks, increment iteration, log. |
| `src/commands/add.rs` | Parse `--loops-to`, `--loop-guard`, `--loop-max` flags. |
| `src/commands/edit.rs` | Parse `--add-loops-to`, `--loop-iteration` flags. |
| `src/commands/show.rs` | Display loop edges and iteration count. |
| `src/main.rs` | Wire new CLI args through to commands. |

**Estimated scope:** ~300 lines of new code, ~50 lines of changes to existing code.

### Phase 2: Validation + Safety

**Goal:** `wg check` and `wg loops` understand loop edges and enforce bounds.

**Files to change:**

| File | Change |
|------|--------|
| `src/check.rs` | Add `check_loop_edges()`: validate targets exist, `max_iterations > 0`, guards reference valid tasks. Don't count `loops_to` cycles in `ok` flag. |
| `src/commands/check.rs` | Display loop edge validation results. |
| `src/commands/loops.rs` | Show `loops_to` edges as "Loop edges" with their guards and iteration state. Classify them as Intentional. |
| `src/commands/analyze.rs` | Include loop edge info in structural health check. |

**Estimated scope:** ~150 lines of new code, ~30 lines of changes.

### Phase 3: Visualization + TUI

**Goal:** Loop edges visible in all graph views.

**Files to change:**

| File | Change |
|------|--------|
| `src/tui/dag_layout.rs` | Render `loops_to` edges alongside existing back-edges (already has infrastructure). Add iteration count display. |
| `src/commands/viz.rs` | Include `loops_to` edges in DOT and Mermaid output with distinct styling. |
| `src/tui/app.rs` | Show loop iteration in task detail panel. |

**Estimated scope:** ~100 lines of new code, ~30 lines of changes.

### Phase 4: Fix Existing Bugs

**Goal:** Fix the unsafe `forecast.rs` traversal found by the DAG assumptions survey.

| File | Change |
|------|--------|
| `src/commands/forecast.rs` | Add `visited: &mut HashSet<String>` to `find_longest_path_from()`. |

**Estimated scope:** ~10 lines of changes.

### Not In Scope (Future Work)

- **Recurring templates** (sprint ceremonies, standups): The research proposes a `wg template` system for schedule-based instantiation. This is independently useful but separate from loop edges. Recommend as a follow-up.
- **Event-driven re-activation via service daemon**: Loop edges fire synchronously in `wg done`. An async event system (signals, webhooks) that fires loops is a future enhancement.
- **Renaming "DAG" terminology**: The survey found ~15 cosmetic locations. Worth doing but orthogonal to this feature.

---

## 8. Examples

### Review-Revise Loop

```bash
wg add "Write draft" --id write-draft
wg add "Review draft" --id review-draft --blocked-by write-draft
wg add "Revise draft" --id revise-draft --blocked-by review-draft \
  --loops-to write-draft --loop-guard "task:review-draft=Failed" --loop-max 5
wg add "Publish" --id publish --blocked-by revise-draft
```

Graph: `write-draft → review-draft → revise-draft → publish`
Loop: `revise-draft --loops_to→ write-draft` (fires if review failed, up to 5 times)

Execution:
1. `write-draft` completes → `review-draft` becomes ready
2. `review-draft` completes as Failed → `revise-draft` becomes ready
3. `revise-draft` completes → loop guard checks `review-draft` status = Failed → re-opens `write-draft` (iteration 1)
4. `write-draft` completes again → `review-draft` becomes ready again
5. If `review-draft` succeeds this time → `revise-draft` completes → loop guard checks `review-draft` status = Done (not Failed) → loop doesn't fire → `publish` becomes ready

### CI Pipeline with Retry

```bash
wg add "Build" --id build
wg add "Test" --id test --blocked-by build
wg add "Deploy" --id deploy --blocked-by test \
  --loops-to build --loop-guard "task:test=Failed" --loop-max 3
```

### Monitoring Loop

```bash
wg add "Monitor" --id monitor
wg add "Investigate" --id investigate --blocked-by monitor
wg add "Fix" --id fix --blocked-by investigate
wg add "Verify" --id verify --blocked-by fix \
  --loops-to monitor --loop-max 10
```

Unconditional loop (no guard) — always loops back to monitoring after verification, up to 10 cycles.

---

## 9. Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| Infinite loops from guard logic errors | `max_iterations` is mandatory and cannot be disabled. Runtime enforcement in the loop evaluation code. |
| Task log bloat from many iterations | Each re-activation adds one log entry. Log is already append-only. Consider a `--compact-log` flag in the future if this becomes an issue. |
| Agent confusion from re-activated tasks | Each iteration's agent gets full task log including "Re-activated by loop (iteration N/M)" — clear signal that this is a repeat run. |
| Concurrent loop edge evaluation | `wg done` holds a file lock on graph.jsonl during write. Only one task can complete at a time. No race conditions. |
| Backward compatibility | New fields use `serde(default)` — old graph files without `loops_to` parse fine (empty Vec). Old `wg` binaries ignore unknown fields in jsonl. |

---

## 10. Decision Log

| Decision | Rationale | Alternatives Considered |
|----------|-----------|------------------------|
| Loop edges are separate from `blocked_by` | Avoids modifying `ready_tasks()` and `wg done` blocker check — highest-risk code paths. | Soft blockers (new edge type in `blocked_by` with a "soft" flag) — rejected because it changes the semantics of an existing, well-tested system. |
| Guard conditions are optional | Unconditional loops (loop N times then stop) are a valid pattern (monitoring loops, warm-up iterations). | Required guards — rejected because it prevents simple "repeat N times" patterns. |
| `max_iterations` is mandatory | Defense against infinite loops. There is no valid use case for an unbounded loop in a task graph. | Optional with high default — rejected because defaults get forgotten and infinite loops are hard to debug. |
| No new Status enum values | The existing states (Open, InProgress, Done, Failed) fully describe a task's current execution state. "Recurring" is a property of the graph topology, not the task state. | `Status::Recurring` — rejected because it conflates lifecycle with execution state. |
| Re-activation is synchronous in `wg done` | Simplest implementation. The agent calls `wg done`, loop evaluation happens, graph is updated atomically. | Async event via service daemon — more complex, needed only when external triggers (webhooks, schedules) are added later. |
| Phase 4 (forecast bug fix) is separate | It's a pre-existing bug unrelated to this feature, but flagged by the survey. | Fix it in Phase 1 — rejected because it's unrelated scope. |
