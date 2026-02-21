# Cycle-Aware Graph: Unified Edge Model with Structural Cycle Detection

**Date:** 2026-02-21
**Updated:** 2026-02-21
**Status:** Draft → Revised (edge rename: `blocked_by` → `after`)
**Depends on:** [Cycle Detection Algorithms Research](../research/cycle-detection-algorithms.md)

---

## Executive Summary

This document designs two coupled changes to workgraph's graph model:

1. **Edge rename:** `blocked_by` → `after`, `blocks` → `before`. Edges express temporal ordering, not obstruction. Data flows through context injection, not through the edge itself. "Blocking" becomes a derived runtime state, not a relationship type.

2. **Cycle unification:** Replace explicit `loops_to` back-edges with dynamic cycle detection derived from `after` edges. Cycles are a *structural property of the graph*, not an *edge-level declaration*. If A is after B is after C is after A, the system recognizes the cycle and manages iteration automatically.

The edge rename is the conceptual foundation. Once edges mean "temporal ordering" rather than "hard gate," cycles in those edges become natural — task A is after task C, which is after task B, which is after task A. That's just a process that repeats.

### Why rename the edge?

| Problem with `blocked_by` | How `after` fixes it |
|---|---|
| "Blocked" conflates the relationship (ordering) with runtime state (not yet satisfied) | `after` = relationship; "waiting" = runtime state — different words for different things |
| Implies a gate/obstacle — negative framing | `after` = temporal ordering — neutral framing |
| Awkward in cycles: "spec is blocked by review" (but review doesn't exist yet on first iteration) | "spec is after review" — true on re-iteration, vacuously satisfied on first iteration |
| Requires a separate `loops_to` edge type for cycles | Cycles emerge naturally from `after` edges — no special edge type needed |
| `blocked_by` carries implicit data-dependency meaning | Data flows through context injection (already exists), not through the edge |

### Edge semantics

The `after` field on a task answers: **"what is this task after?"**

```yaml
id: impl
after: [spec]        # impl is after spec → spec runs first
before: [review]     # impl is before review → review runs later (computed inverse)
```

- `after` is the authoritative field (stored). Equivalent to current `blocked_by`.
- `before` is the computed inverse (derived). Equivalent to current `blocks`.
- A task is **waiting** (not "blocked") when something in its `after` list hasn't finished.
- A task is **ready** when it's open, not paused, past time constraints, and everything in `after` is terminal.
- **Context injection** (existing coordinator feature) pulls artifacts and logs from completed predecessors. The edge carries ordering; the context system carries data.

### CLI

```bash
wg add "impl" --after spec
wg add "review" --after impl
wg add "spec" --after review --max-iterations 3   # cycle emerges naturally
```

### Migration

```rust
#[serde(alias = "blocked_by")]
pub after: Vec<String>,

#[serde(alias = "blocks")]
pub before: Vec<String>,   // computed inverse, not serialized
```

Old graphs deserialize without changes. `--blocked-by` accepted as hidden CLI alias during transition.

The design follows a four-phase migration path that maintains backward compatibility throughout. Phase 1 adds cycle analysis as a read-only diagnostic. Phase 2 enables natural cycles in `after` as a parallel mechanism alongside `loops_to`. Phase 3 migrates existing `loops_to` edges. Phase 4 removes `loops_to` entirely.

---

## 1. Data Model Changes

### 1.1 Current Model

```
Task {
    blocked_by: Vec<String>       // forward dependency edges (DAG-only by convention)
    blocks: Vec<String>           // inverse of blocked_by (computed convenience)
    loops_to: Vec<LoopEdge>       // separate back-edges with metadata
    loop_iteration: u32           // per-task iteration counter
}

LoopEdge {
    target: String                // task to re-open
    guard: Option<LoopGuard>      // firing condition
    max_iterations: u32           // hard cap
    delay: Option<String>         // time delay before re-activation
}

WorkGraph {
    nodes: HashMap<String, Node>  // tasks + resources
    // No cycle analysis; loops_to edges are checked on task completion
}
```

Key properties:
- `loops_to` edges are NOT in `blocked_by` — they don't affect `ready_tasks()`.
- Iteration tracking is per-task (`loop_iteration` field).
- `evaluate_loop_edges()` fires on `wg done`, re-opens target + intermediates.
- Convergence via `--converged` flag (adds tag, prevents loop firing).

### 1.2 Proposed Model

```
Task {
    after: Vec<String>                   // ALL ordering edges, including cycle-forming ones
                                         // serde alias: "blocked_by" for backward compat
    before: Vec<String>                  // computed inverse (was: blocks)
    cycle_config: Option<CycleConfig>    // only on cycle header tasks
    loop_iteration: u32                  // retained for iteration tracking (per-task)
    // loops_to: Vec<LoopEdge>           // REMOVED (Phase 4)
    // blocked_by: Vec<String>           // RENAMED to after (Phase 2)
}

CycleConfig {
    max_iterations: u32                  // hard cap on cycle iterations
    guard: Option<LoopGuard>             // firing condition (retained from LoopEdge)
    delay: Option<String>                // time delay before re-activation
}

WorkGraph {
    nodes: HashMap<String, Node>
    cycle_analysis: Option<CycleAnalysis>   // cached, invalidated on mutation
}
```

### 1.3 Cycle Analysis (Computed, Not Stored)

The `CycleAnalysis` struct is computed from graph structure and cached. It is never serialized to disk — it's derived data that can be recomputed from `after` edges at any time.

```rust
/// Cached cycle analysis, recomputed on graph mutations.
/// This is derived data — never serialized.
struct CycleAnalysis {
    /// Non-trivial SCCs (cycles), indexed by CycleId
    cycles: Vec<Cycle>,
    /// Which cycle each task belongs to (if any)
    task_to_cycle: HashMap<String, usize>,  // usize = index into cycles
    /// Edges classified as back-edges (create the cycle)
    back_edges: HashSet<(String, String)>,
}

struct Cycle {
    /// All task IDs in this cycle's SCC
    members: Vec<String>,
    /// The entry point / loop header
    header: String,
    /// Is this a reducible cycle (single entry point)?
    reducible: bool,
}
```

### 1.4 Where Metadata Lives

| Metadata | Current Location | Proposed Location | Rationale |
|----------|-----------------|-------------------|-----------|
| `max_iterations` | `LoopEdge` | `CycleConfig` on header task | Header is the user-facing entry point |
| `guard` | `LoopEdge` | `CycleConfig` on header task | Guards are evaluated per-cycle, not per-edge |
| `delay` | `LoopEdge` | `CycleConfig` on header task | Delay applies to the whole cycle iteration |
| `loop_iteration` | `Task` (per-task) | `Task` (on header task only) | Single counter per cycle, on the header |
| Convergence | `"converged"` tag on source | `"converged"` tag on header | Same mechanism, different anchor point |
| Cycle membership | N/A (implicit) | `CycleAnalysis.task_to_cycle` | Computed, not stored |
| Back-edge identity | N/A (implicit) | `CycleAnalysis.back_edges` | Computed, not stored |

**Design decision:** `CycleConfig` lives on the header task because:
1. The header is what the user interacts with (it's the cycle's "name").
2. `--converged` already targets a task, not an edge — consistency.
3. Avoids a separate data store for cycle-level configuration.
4. The header is stable (it's the entry point to the cycle).

---

## 2. Cycle Detection Integration

### 2.1 Algorithm Choice

**Primary: Tarjan's SCC** — custom iterative implementation in `src/cycle.rs` (std-only, no external dependencies).

Rationale:
- O(V+E) time, O(V) space — effectively free for workgraph's scale (<1000 tasks).
- Custom iterative implementation avoids stack overflow on large graphs.
- Based on Tarjan (1972) with the iterative transformation from Pearce (2016).
- Finds all cycles simultaneously, grouped into SCCs.
- No external dependency needed — the algorithm is straightforward to implement with std collections.

**Also implemented (read-only analysis):**
- **Havlak's Loop Nesting Forest** — identifies loop headers, nesting structure, and back edges for both reducible and irreducible loops. Available for diagnostic use.
- **Incremental Cycle Detection** — detects whether adding a single edge creates a cycle without full recomputation. Useful for real-time validation as edges are added.
- **Cycle Metadata Extraction** — extracts header, reducibility, back edges, and nesting depth from detected SCCs.

> **Implementation note:** The original design considered using `petgraph::algo::tarjan_scc()`, but the actual implementation uses a standalone std-only module (`src/cycle.rs`) with no external dependencies. This is simpler and avoids adding a dependency for something that's ~160 lines of Rust. All four algorithms (53 tests) are implemented and validated.

### 2.2 When to Recompute

Cycle analysis is recomputed **lazily on access** after any structural mutation:

```rust
impl WorkGraph {
    /// Invalidate cached cycle analysis. Called by any structural mutation.
    fn invalidate_cycle_cache(&mut self) {
        self.cycle_analysis = None;
    }

    /// Get or compute cycle analysis.
    fn cycle_analysis(&mut self) -> &CycleAnalysis {
        if self.cycle_analysis.is_none() {
            self.cycle_analysis = Some(analyze_cycles(self));
        }
        self.cycle_analysis.as_ref().unwrap()
    }
}
```

Mutations that invalidate the cache:
- `add_node()` — new task may create or break cycles.
- `remove_node()` — removing a task may break cycles.
- Any modification to a task's `after` field.

Mutations that do NOT invalidate the cache:
- Status changes (Open → InProgress → Done).
- Tag changes, assignment changes, log entries.
- Any non-structural metadata update.

### 2.3 Cycle Analysis Pipeline

```
1. Build adjacency list from `after` edges
   - Each task is a node (mapped to numeric NodeId via NamedGraph)
   - Edge: predecessor → dependent (i.e., if task A has after: [B], edge is B → A)

2. Run tarjan_scc() from src/cycle.rs
   - Returns Vec<Scc>, each with a members: Vec<NodeId>

3. Filter to non-trivial SCCs via find_cycles()
   - SCCs with size > 1 are cycles
   - Self-loops optionally included

4. Extract metadata via extract_cycle_metadata():
   a. Identify entry nodes: nodes with at least one predecessor outside the SCC
   b. If exactly one entry node → reducible cycle, entry = header
   c. If no entry nodes → isolated cycle (all predecessors are internal)
      Pick the node with the smallest ID as header
      (deterministic, stable across recomputations)
   d. If multiple entry nodes → irreducible cycle (see §2.4)

5. Identify back-edges: predecessors of the header that are within the SCC.
   Classification: an edge (u, header) within an SCC is a back-edge.

6. Compute nesting depth from SCC containment relationships.
```

### 2.4 Irreducible Cycles

An irreducible cycle has multiple entry points — no single node dominates all others.

**Example:**
```
X → A → B → A   (A is entered from X)
Y → B → A → B   (B is entered from Y)
```

**Decision: Reject irreducible cycles in v1.**

Workgraph's use cases (review-revise, CI retry, monitor-fix-verify) are all naturally reducible — they have a clear starting point. Irreducible cycles would indicate a modeling error.

When an irreducible cycle is detected:
- `wg check` reports it as a warning.
- `wg cycles` displays it with a note: "Multiple entry points — requires explicit header annotation."
- The cycle is NOT executed (no iteration, no re-opening).
- The user can resolve it by restructuring dependencies or adding an explicit `cycle_header: true` annotation (future extension).

### 2.5 Self-Loops

A task with `after: ["self"]` is a trivial cycle (SCC of size 1 with a self-edge). This is already rejected by `wg add` (`"Task cannot depend on itself"`). No change needed.

---

## 3. Dispatch Changes

### 3.1 The Core Problem

Currently, `ready_tasks()` requires ALL predecessors to be terminal:

```rust
// src/query.rs:265
task.after.iter().all(|pred_id| {       // was: blocked_by
    graph.get_task(pred_id)
        .map(|t| t.status.is_terminal())
        .unwrap_or(true)
})
```

In a cycle `A → B → C → A`, task A has `after: [C]`. If C is not Done, A is never ready. But C is after B, which is after A. Deadlock.

### 3.2 Solution: Back-Edge Exemption

The cycle header's back-edge predecessors are exempt from the readiness check. Only the header gets this treatment — non-header tasks in the cycle still wait for their predecessors normally.

```rust
pub fn ready_tasks_cycle_aware(
    graph: &WorkGraph,
    analysis: &CycleAnalysis,
) -> Vec<&Task> {
    graph.tasks().filter(|task| {
        if task.status != Status::Open { return false; }
        if task.paused { return false; }
        if !is_time_ready(task) { return false; }

        task.after.iter().all(|pred_id| {
            // Normal check: predecessor is terminal
            if graph.get_task(pred_id)
                .map(|t| t.status.is_terminal())
                .unwrap_or(true)
            {
                return true;
            }

            // Cycle-aware check: if this task is a cycle header and
            // the predecessor is in the same cycle, the back-edge
            // is satisfied (it will be re-evaluated after the cycle iterates)
            if analysis.back_edges.contains(&(pred_id.clone(), task.id.clone())) {
                return true;
            }

            false
        })
    }).collect()
}
```

### 3.3 First Iteration vs Re-Iteration

**First iteration:** The header becomes ready when all its *external* dependencies (outside the cycle) are terminal. Back-edge dependencies are exempt. This starts the cycle.

**Re-iteration:** After all cycle members complete and the cycle hasn't converged / hit max_iterations, the header and all cycle members are re-opened. The header becomes ready immediately (back-edge dependency is again exempt).

**Non-header tasks in the cycle:** These wait for their normal predecessors within the cycle. When the header completes, its immediate dependents become ready, and so on through the cycle body.

### 3.4 Integration with `ready_tasks_with_peers()`

The peer-aware variant in `src/query.rs:305` also needs the cycle-aware check. Both `ready_tasks()` and `ready_tasks_with_peers()` should call a shared helper that accepts `CycleAnalysis`.

---

## 4. Completion and Re-opening

### 4.1 Current Behavior

`evaluate_loop_edges()` in `src/graph.rs:582` fires when a task transitions to Done:
1. Check for `"converged"` tag on source → skip if present.
2. For each `LoopEdge`: check guard + iteration limit.
3. If loop fires: re-open target, clear assigned/timestamps, increment iteration.
4. `find_intermediate_tasks()` via BFS: re-open intermediate tasks.
5. Re-open source task itself.

### 4.2 Proposed Behavior

Replace `evaluate_loop_edges()` with `evaluate_cycle_iteration()`:

```
on task_completion(task_id):
    1. Mark task as Done (existing)
    2. analysis = get_or_compute_cycle_analysis(graph)
    3. If task_id is NOT in any cycle → return (no cycle behavior)
    4. cycle = analysis.get_cycle(task_id)
    5. Check if ALL members of the cycle are now Done
       - If not all Done → return (cycle still executing this iteration)
    6. header = cycle.header
    7. config = header.cycle_config (if None → no iteration, cycle is one-shot)
    8. Check convergence:
       a. If header has "converged" tag → return (cycle terminated by convergence)
       b. If header.loop_iteration >= config.max_iterations → return (hit cap)
       c. If config.guard is Some and !evaluate_guard(config.guard, graph) → return
    9. Iterate the cycle:
       a. header.loop_iteration += 1
       b. For each member in cycle.members:
          - Set status = Open
          - Clear assigned, started_at, completed_at
          - Set loop_iteration = header.loop_iteration
          - Add log entry: "Re-opened by cycle iteration N/max"
       c. If config.delay is Some:
          - Set header.ready_after = now + delay
```

### 4.3 Key Difference: "All Done" Trigger

The current model fires on the source task's completion (the task with `loops_to`). The proposed model fires when the **last task in the cycle** completes — i.e., when all cycle members are Done. This is cleaner because:

1. There's no designated "source" in a structural cycle — any task could be the last to complete.
2. The "all Done" check naturally handles parallel execution within the cycle.
3. It matches the intuition: "the cycle iterates when the current iteration is complete."

**Optimization:** Only check the "all Done" condition when a cycle member completes. This avoids checking on every task completion. The `task_to_cycle` map in `CycleAnalysis` makes this lookup O(1).

### 4.4 Convergence

The `--converged` mechanism is preserved with one change: the tag goes on the **header task**, not the "source" task (since there's no dedicated source in structural cycles).

```bash
# Current: converge the loop by marking the source task
wg done review-task --converged

# Proposed: converge the cycle by marking the completing task
# (the system moves the "converged" tag to the header)
wg done any-cycle-member --converged
```

When `--converged` is passed on any cycle member:
1. The `"converged"` tag is added to the **cycle header** (not necessarily the task being completed).
2. The cycle iteration check (step 8a above) sees the tag and stops.

This is more flexible — any agent in the cycle can signal convergence, not just the one at the "end."

---

## 5. Migration Path

### Phase 1: Add Cycle Analysis (Non-Breaking)

**Changes:**
- Add `CycleAnalysis` struct and `analyze_cycles()` function to `src/graph.rs` (or new `src/cycle.rs` module).
- Add `cycle_analysis: Option<CycleAnalysis>` to `WorkGraph`.
- Add `wg cycles` command (or enhance existing `wg loops`) to display detected cycles.
- Integrate cycle detection into `wg check` (validate that all cycles in `after` edges are reducible or intentional).

**No behavioral changes.** `loops_to` continues to work exactly as before. Cycle analysis is read-only.

**Effort:** ~300 lines of Rust. 1-2 days.

### Phase 2: Rename edges and support natural cycles

**Changes:**
- Rename `blocked_by` → `after`, `blocks` → `before` with serde aliases for backward compat.
- Rename CLI flag `--blocked-by` → `--after` (keep `--blocked-by` as hidden alias).
- Add `CycleConfig` field to `Task` struct (optional, serde skip_serializing_if None).
- Modify `ready_tasks()` and `ready_tasks_with_peers()` to accept `CycleAnalysis` and exempt back-edge predecessors for cycle headers.
- Add `evaluate_cycle_iteration()` alongside existing `evaluate_loop_edges()`.
- Add CLI support: `wg add --max-iterations N` to set `CycleConfig.max_iterations` on a task.
- `wg add --after` now allows creating cycles (currently produces a warning via `wg check`; this becomes a valid operation when `--max-iterations` is set).

**Both `loops_to` and structural cycles work.** Users can choose either model. This phase runs in parallel with the existing system.

**Effort:** ~500 lines of Rust. 2-3 days.

### Phase 3: Migrate `loops_to` to Structural Cycles

**Changes:**
- Add `wg migrate-loops` command that converts `loops_to` edges to `after` edges with `CycleConfig`:
  ```
  For each task with loops_to edges:
    For each LoopEdge { target, guard, max_iterations, delay }:
      1. Add target to task.after (creates the back-edge)
      2. Set target.cycle_config = CycleConfig { max_iterations, guard, delay }
      3. Remove the LoopEdge from task.loops_to
  ```
- Add serde migration: on deserialization, if a task has `loops_to` and no `cycle_config`, automatically apply the migration.
- Deprecation warning: `wg add --loops-to` prints "deprecated, use --blocked-by with --max-iterations instead."

**Effort:** ~200 lines of Rust. 1 day.

### Phase 4: Remove `loops_to`

**Changes:**
- Remove `LoopEdge` struct from `src/graph.rs`.
- Remove `loops_to` field from `Task` (keep serde deserialization for backward compat, but ignore the data).
- Remove `evaluate_loop_edges()` — all loop behavior is now in `evaluate_cycle_iteration()`.
- Remove `--loops-to`, `--loop-max`, `--loop-guard`, `--loop-delay` from `wg add`.
- Remove or repurpose `src/commands/loops.rs` → merge into `wg cycles`.
- Update `check_loop_edges()` in `src/check.rs` → `check_cycles()` covers this.
- Clean up `find_intermediate_tasks()` — cycle membership replaces BFS.

**Effort:** ~400 lines of Rust (mostly deletions). 1-2 days.

### 5.1 Backward Compatibility

**Serde migration strategy:**

```rust
impl<'de> Deserialize<'de> for Task {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> {
        let helper = TaskHelper::deserialize(deserializer)?;

        // Migration: if old loops_to field present and no cycle_config,
        // convert to cycle_config on the target task.
        // (This is a best-effort migration — the full migration
        //  requires updating the target task too, which happens
        //  in the wg migrate-loops command.)
        let cycle_config = match (helper.cycle_config, &helper.loops_to) {
            (Some(config), _) => Some(config),
            (None, loops) if !loops.is_empty() => None, // handled by migrate-loops
            _ => None,
        };

        Ok(Task { cycle_config, ..rest })
    }
}
```

**Key constraint:** Old `.workgraph/` directories with `loops_to` edges must continue to work during the transition. The serde layer reads old format; the `wg migrate-loops` command converts in place; the new code writes the new format.

---

## 6. CLI Changes

### 6.1 Creating Cycles

```bash
# Current: cycles in blocked_by are a warning
wg add "review" --blocked-by "write"
wg add "write" --blocked-by "review"   # warning: creates cycle

# Proposed: --after replaces --blocked-by, cycles are valid when max-iterations is set
wg add "write" --max-iterations 5       # this task is a cycle header
wg add "review" --after "write"
wg add "write" --after "review"         # valid: cycle has a configured header
```

Alternatively, set cycle config after the fact:

```bash
wg edit write --max-iterations 5
wg edit write --cycle-guard "task:review=failed"
wg edit write --cycle-delay "5m"
```

### 6.2 `wg cycles` Command

Replaces/extends `wg loops`:

```
$ wg cycles

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

With `--json`:

```json
{
  "cycles": [
    {
      "members": ["write", "review"],
      "header": "write",
      "reducible": true,
      "max_iterations": 5,
      "current_iteration": 1,
      "converged": false,
      "guard": "task:review=failed",
      "delay": "5m"
    }
  ]
}
```

### 6.3 `wg add` Changes

| Flag | Current | Proposed |
|------|---------|----------|
| `--blocked-by` | Sets blocked_by | Renamed to `--after` (hidden alias kept) |
| `--after` | N/A | Sets `after` field — can create cycles |
| `--loops-to` | Creates LoopEdge | Deprecated in Phase 3; removed in Phase 4 |
| `--loop-max` | Sets LoopEdge.max_iterations | Deprecated; use `--max-iterations` |
| `--loop-guard` | Sets LoopEdge.guard | Deprecated; use `--cycle-guard` |
| `--loop-delay` | Sets LoopEdge.delay | Deprecated; use `--cycle-delay` |
| `--max-iterations` | N/A | Sets CycleConfig.max_iterations |
| `--cycle-guard` | N/A | Sets CycleConfig.guard |
| `--cycle-delay` | N/A | Sets CycleConfig.delay |

### 6.4 `wg done` Changes

`--converged` continues to work. The only change is that when a cycle member completes with `--converged`, the tag is propagated to the cycle header.

### 6.5 Validation

`wg check` gains new cycle-related checks:

- **Unconfigured cycle:** A cycle exists in `after` edges but no member has `CycleConfig`. Report as warning: "Cycle detected but no max_iterations configured — cycle will deadlock."
- **Irreducible cycle:** Multiple entry points. Report as error.
- **Conflicting configs:** Multiple tasks in the same cycle have `CycleConfig`. Report as error: "Only the cycle header should have cycle_config."

---

## 7. Test Plan

### 7.1 Cycle Detection Tests

| Test | Description | Validates |
|------|-------------|-----------|
| `test_detect_simple_cycle` | A→B→A via `after` edges | SCC detection finds 2-node cycle |
| `test_detect_three_node_cycle` | A→B→C→A | SCC detection finds 3-node cycle |
| `test_no_false_positives` | Linear chain A→B→C | No cycles detected |
| `test_multiple_independent_cycles` | A→B→A and C→D→C | Two separate cycles detected |
| `test_overlapping_sccs` | Complex graph with shared nodes | Correct SCC decomposition |
| `test_header_identification_single_entry` | External X→A in cycle A→B→C→A | A is identified as header |
| `test_header_identification_no_entry` | Isolated cycle A→B→C→A | Lexicographically smallest ID is header |
| `test_irreducible_cycle_detected` | X→A, Y→B in cycle A→B→A | Flagged as irreducible |
| `test_cycle_analysis_deterministic` | Same graph, multiple analyses | Same results every time |

### 7.2 Dispatch Tests

| Test | Description | Validates |
|------|-------------|-----------|
| `test_header_becomes_ready_first_iteration` | Cycle A→B→C→A, external dep X done | A is ready (back-edge from C exempt) |
| `test_non_header_waits_for_predecessor` | Cycle A→B→C→A, A is done | B is ready, C is not |
| `test_external_deps_block_header` | Cycle A→B→A, A also blocked by X (not done) | A is NOT ready |
| `test_back_edge_exemption_only_for_header` | Cycle A→B→C→A | C does NOT get back-edge exemption |

### 7.3 Iteration Tests

| Test | Description | Validates |
|------|-------------|-----------|
| `test_cycle_iterates_when_all_done` | A→B→C→A, all complete, max=3 | All re-opened, iteration=1 |
| `test_cycle_stops_at_max_iterations` | All complete, iteration already at max | No re-opening |
| `test_convergence_stops_cycle` | Member completes with --converged | No re-opening |
| `test_partial_completion_no_iteration` | A done, B done, C still open | No re-opening yet |
| `test_iteration_counter_increments` | Two full iterations | Counter goes 0→1→2 |
| `test_delay_applied_on_iteration` | Cycle with 5m delay | header.ready_after set correctly |
| `test_guard_prevents_iteration` | Guard condition not met | No re-opening despite all Done |

### 7.4 Nested Cycle Tests

| Test | Description | Validates |
|------|-------------|-----------|
| `test_nested_cycle_detection` | Outer A→B→C→A, inner B→D→B | Two cycles detected, inner nested |
| `test_inner_cycle_iterates_independently` | Inner cycle runs to completion | Inner cycle iterates without affecting outer |
| `test_outer_cycle_after_inner_converges` | Inner converges, outer continues | Outer cycle picks up after inner finishes |

### 7.5 Dynamic Graph Tests

| Test | Description | Validates |
|------|-------------|-----------|
| `test_add_task_creates_cycle` | Add dependency that forms cycle | CycleAnalysis updated |
| `test_remove_task_breaks_cycle` | Remove task from cycle | Cycle disappears from analysis |
| `test_add_edge_during_iteration` | Add new member to active cycle | New member included in next iteration |
| `test_cycle_analysis_invalidation` | Mutate graph, re-query analysis | Cache invalidated, fresh analysis |

### 7.6 Migration Tests

| Test | Description | Validates |
|------|-------------|-----------|
| `test_migrate_simple_loop` | Task with loops_to → `after` cycle | Correct conversion |
| `test_migrate_preserves_max_iterations` | LoopEdge.max_iterations → CycleConfig | Value preserved |
| `test_migrate_preserves_guard` | LoopEdge.guard → CycleConfig.guard | Guard preserved |
| `test_migrate_preserves_delay` | LoopEdge.delay → CycleConfig.delay | Delay preserved |
| `test_deserialize_old_format` | Read graph.jsonl with loops_to | Still loads correctly |
| `test_roundtrip_old_to_new` | Read old format, write new format | Data preserved |

### 7.7 Edge Cases

| Test | Description | Validates |
|------|-------------|-----------|
| `test_single_task_not_a_cycle` | Task with no `after` | No cycles |
| `test_cycle_with_failed_member` | One member is Failed | Cycle does not iterate (not all Done) |
| `test_convergence_on_non_cycle_task` | --converged on task not in cycle | Tag added, no error |
| `test_multiple_cycles_share_no_state` | Two cycles, different max_iterations | Independent iteration counters |
| `test_cycle_header_removed` | Delete the header task | Cycle breaks, other tasks unblocked |

---

## 8. Open Design Questions

### 8.1 Should We Do This At All?

The research document (Section 5.1) raises valid concerns:

**For structural cycles + rename:**
- Eliminates a special edge type — simpler conceptual model.
- Cycles are a graph property, not an edge property — more principled.
- Users create cycles naturally with `--after` without learning `loops_to`.
- `after`/`before` separates the relationship (ordering) from the runtime state (waiting) — `blocked_by` conflated these.
- Data flows through context injection, not edges — `after` makes this clear.

**Against (keeping `loops_to` and `blocked_by`):**
- `loops_to` works. 100+ tests, handles all use cases.
- Explicit is better than implicit. A cycle in `after` could be accidental.
- Renaming `blocked_by` → `after` across the entire codebase is a large mechanical change.

**Recommendation:** Proceed with the phased approach. Phase 1 (diagnostic only) has no downside. Phase 2 (parallel support) lets us evaluate the model in practice. Phases 3-4 only happen if the structural model proves superior.

### 8.2 Accidental Cycles

When `after` edges can form cycles, users might create them accidentally. The system must distinguish intentional cycles from bugs.

**Approach:** A cycle is "configured" (intentional) only if the header task has a `CycleConfig`. An unconfigured cycle is flagged by `wg check` and treated as a deadlock — no iteration, no back-edge exemption. The user must explicitly add `--max-iterations` to opt in.

This preserves the "explicit is better than implicit" principle while using structural detection.

### 8.3 Cycle Identity Stability

When the graph changes, cycle analysis may change. A cycle that existed before might split or merge. This affects iteration counters and convergence state.

**Mitigation:** The header task ID is the stable cycle identifier. `loop_iteration` and `"converged"` tag live on the header task. As long as the header exists and is in a cycle, state is preserved. If the header is removed, state is lost (acceptable — removing a task is a destructive operation).

### 8.4 Multiple Cycles Through the Same Node

A task can participate in multiple elementary cycles within a single SCC. With structural detection, the SCC is treated as one cycle with one header. Individual elementary cycles are not distinguished.

This is a simplification. If per-elementary-cycle configuration is needed, the user should restructure their graph to make each cycle a separate SCC (by adding intermediary tasks).

---

## 9. Implementation Sketch

### 9.1 New Module: `src/cycle.rs` — Implemented

> **Status:** Implemented and validated. The actual implementation in `src/cycle.rs` (~1030 lines, 53 tests) supersedes this sketch. Key differences from the original sketch:
>
> - Uses **std-only** (no petgraph dependency) with a custom iterative Tarjan implementation.
> - Implements **four algorithms**: Tarjan SCC, Havlak Loop Nesting, Incremental Cycle Detection, and Cycle Metadata Extraction.
> - The entry-node logic below had a double-add bug (lines 736-743) — the actual implementation correctly uses `rev_adj` for predecessor lookup without duplication.
> - Uses `NodeId = usize` internally with a `NamedGraph` adapter for string-based task IDs.
>
> See `src/cycle.rs` for the canonical implementation.

<details>
<summary>Original sketch (kept for reference — contains known bugs)</summary>

```rust
// NOTE: This sketch has known issues. See src/cycle.rs for the correct implementation.
use petgraph::algo::tarjan_scc;
// ... (original sketch code)
```

</details>

### 9.2 Modified `ready_tasks()` (Phase 2)

```rust
pub fn ready_tasks_cycle_aware<'a>(
    graph: &'a WorkGraph,
    analysis: &CycleAnalysis,
) -> Vec<&'a Task> {
    graph.tasks().filter(|task| {
        if task.status != Status::Open { return false; }
        if task.paused { return false; }
        if !is_time_ready(task) { return false; }

        task.after.iter().all(|pred_id| {
            if graph.get_task(pred_id)
                .map(|t| t.status.is_terminal())
                .unwrap_or(true)
            {
                return true;
            }
            // Back-edge exemption for cycle headers
            analysis.back_edges.contains(&(pred_id.clone(), task.id.clone()))
        })
    }).collect()
}
```

### 9.3 `evaluate_cycle_iteration()` (Phase 2)

```rust
pub fn evaluate_cycle_iteration(
    graph: &mut WorkGraph,
    completed_task_id: &str,
    analysis: &CycleAnalysis,
) -> Vec<String> {
    // Is this task in a cycle?
    let cycle_idx = match analysis.task_to_cycle.get(completed_task_id) {
        Some(&idx) => idx,
        None => return vec![],
    };
    let cycle = &analysis.cycles[cycle_idx];

    // Are ALL cycle members Done?
    let all_done = cycle.members.iter().all(|id| {
        graph.get_task(id)
            .map(|t| t.status == Status::Done)
            .unwrap_or(false)
    });
    if !all_done { return vec![]; }

    let header = &cycle.header;

    // Check convergence
    if let Some(task) = graph.get_task(header) {
        if task.tags.contains(&"converged".to_string()) {
            return vec![];
        }
    }

    // Check cycle_config
    let (max_iterations, guard, delay) = match graph.get_task(header)
        .and_then(|t| t.cycle_config.as_ref())
    {
        Some(config) => (config.max_iterations, config.guard.clone(), config.delay.clone()),
        None => return vec![],  // No config = one-shot cycle, no iteration
    };

    let current_iteration = graph.get_task(header)
        .map(|t| t.loop_iteration)
        .unwrap_or(0);

    if current_iteration >= max_iterations { return vec![]; }

    // Check guard
    if let Some(ref guard) = guard {
        if !evaluate_guard(&Some(guard.clone()), graph) {
            return vec![];
        }
    }

    // Iterate: re-open all cycle members
    let new_iteration = current_iteration + 1;
    let mut reactivated = Vec::new();

    for member_id in &cycle.members {
        if let Some(task) = graph.get_task_mut(member_id) {
            task.status = Status::Open;
            task.assigned = None;
            task.started_at = None;
            task.completed_at = None;
            task.loop_iteration = new_iteration;
            // Apply delay only to header
            if member_id == header {
                task.ready_after = delay.as_ref().and_then(|d| {
                    parse_delay(d).map(|secs| {
                        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339()
                    })
                });
            }
            task.log.push(LogEntry {
                timestamp: chrono::Utc::now().to_rfc3339(),
                actor: None,
                message: format!(
                    "Re-opened by cycle iteration {}/{}",
                    new_iteration, max_iterations
                ),
            });
            reactivated.push(member_id.clone());
        }
    }

    reactivated
}
```

---

## 10. Risk Assessment

| Risk | Impact | Likelihood | Mitigation |
|------|--------|------------|------------|
| Existing loops_to tests break | High | Low (phased approach) | Phase 1-2 don't touch loops_to |
| Accidental cycles cause deadlocks | Medium | Medium | Require explicit CycleConfig to enable iteration |
| Cycle analysis performance | Low | Very Low | O(V+E) Tarjan, <1ms for 1000 tasks |
| Irreducible cycles confuse users | Medium | Low | Reject in v1, clear error messages |
| Migration loses loop metadata | High | Low | Automated migration with validation |
| Header identification is wrong | Medium | Medium | Entry-node heuristic + explicit override option |

---

## 11. Decision Log

| Decision | Chosen | Alternatives Considered | Rationale |
|----------|--------|------------------------|-----------|
| Algorithm | Tarjan SCC (std-only, custom iterative) | petgraph, Havlak, Johnson, incremental | Simple, no external dependency needed |
| Header identification | Entry-node heuristic | Dominator-based, explicit annotation, DFS-based | Works for common cases, no extra computation |
| Metadata location | CycleConfig on header task | Per-edge, separate cycle store | Header-centric is intuitive, aligns with --converged |
| Iteration trigger | All-Done | Source completion, any completion, back-edge | Clean semantics, handles parallel execution |
| Irreducible cycles | Reject in v1 | Pick one header, allow multiple headers | Workgraph use cases are reducible |
| Migration | 4-phase incremental | Big bang, permanent dual-model | Low risk, allows evaluation at each phase |
| Convergence | Tag on header | Tag on completing task, separate flag | Header is the stable cycle identifier |
