# Research: Spiral/Cycle-Unrolling Gap Analysis

**Date**: 2026-04-01
**Task**: research-spiral-cycle
**Audience**: Design task (design-cycle-to) and implementation planning

---

## Executive Summary

Workgraph cycles reset task state **in-place**, reusing the same task IDs across iterations. This means per-iteration artifacts (evaluations, FLIP scores, agent logs, token usage, session IDs, validation results) are either **overwritten** or **orphaned** — they exist in external storage but lose their iteration provenance. The "spiral" concept proposes that each cycle iteration materializes as unique tasks, preserving full per-iteration history while the cycle definition remains the structural template.

**Key finding**: There is no existing spiral implementation, design doc, or partial code. The gap is significant but well-bounded. The closest existing mechanism is `wg replay` + the runs snapshot system, which archives full graph state before resetting tasks — a coarser version of per-iteration preservation.

---

## 1. Current Cycle Reset Behavior

### 1.1 Reset Trigger

Cycle iteration is evaluated in two places:

- **`evaluate_cycle_iteration()`** (`src/graph.rs:1390`): Called when a task transitions to Done. Checks if all SCC members are terminal, then evaluates whether to iterate.
- **`evaluate_all_cycle_iterations()`** (`src/graph.rs:1591`): Called by the coordinator as a safety net — proactively scans all cycles for any that should iterate.

Both call **`reactivate_cycle()`** (`src/graph.rs:1440`), the shared logic.

### 1.2 What Happens During Normal Cycle Reset

When all cycle members reach Done and iteration conditions are met (`src/graph.rs:1522-1577`), each member task is mutated in-place:

| Field | Action | Impact |
|-------|--------|--------|
| `status` | Set to `Open` | Previous status lost |
| `assigned` | Set to `None` | Agent assignment lost |
| `started_at` | Set to `None` | Start timestamp lost |
| `completed_at` | Saved to `last_iteration_completed_at`, then cleared | Preserved (one level deep only) |
| `triage_count` | Set to `0` | Triage history lost |
| `loop_iteration` | Incremented to `new_iteration` | **Preserved** — this is the iteration counter |
| `ready_after` | Set if delay configured (config owner only) | Overwritten per iteration |
| `log` | **Appended** with re-activation entry | **Preserved** — accumulates across iterations |
| `artifacts` | **Not cleared** | Accumulate but lose iteration provenance |
| `failure_reason` | **Not cleared** on normal reset | Stale if prior iteration failed |
| `token_usage` | **Not cleared** | Overwritten by next agent run |
| `session_id` | **Not cleared** | Overwritten by next agent run |
| `checkpoint` | **Not cleared** | Stale from prior iteration |
| `verify` | **Not cleared** | Structural — appropriate |
| `tags` | **Not modified** (except `converged` cleared on config owner during failure restart) | Tags accumulate |
| `description` | **Not modified** | Structural — appropriate |

### 1.3 What Happens During Failure Restart

`reactivate_cycle_on_failure()` (`src/graph.rs:1712-1812`) is more aggressive:

| Field | Action |
|-------|--------|
| `status` | Set to `Open` |
| `assigned` | Set to `None` |
| `started_at` | Set to `None` |
| `completed_at` | Set to `None` (**not** saved to `last_iteration_completed_at`) |
| `failure_reason` | Set to `None` |
| `triage_count` | Set to `0` |
| `loop_iteration` | **Not changed** (failure restart is a retry of same iteration) |
| `cycle_failure_restarts` | Incremented (config owner only) |
| `converged` tag | Removed from config owner |

### 1.4 `CycleConfig` Structure

Defined at `src/graph.rs:8-28`:

```rust
pub struct CycleConfig {
    pub max_iterations: u32,
    pub guard: Option<LoopGuard>,
    pub delay: Option<String>,
    pub no_converge: bool,
    pub restart_on_failure: bool,
    pub max_failure_restarts: Option<u32>,
}
```

Only the cycle "header" task (one member with `cycle_config` set) carries the config. Other members are identified via SCC analysis (`compute_cycle_analysis()`).

---

## 2. Prior Spiral Work

### 2.1 Code Search Results

| Search Term | Results |
|------------|---------|
| `spiral` | **No code references**. One poem line in `docs/poetry/workgraph-poems.typ` |
| `unroll` | **No results** in code or git history |
| `materialize` | **No results** in code or git history |
| `git log --grep spiral` | **No commits** |
| `git log --grep unroll` | **No commits** |

### 2.2 Related Prior Research

**`docs/research/cyclic-processes.md`** (2026-02-14) is the closest prior art. It surveyed 8 workflow systems and identified two philosophies:

- **Philosophy A: Immutable runs, new instances** (Temporal, Airflow, Argo, Cylc) — each iteration creates a new execution/run/node. The spiral concept aligns with this philosophy.
- **Philosophy B: Mutable status, re-activation** (n8n, BPMN, Step Functions) — the same task transitions back. **This is what workgraph currently implements.**

The research recommended a hybrid approach (Section 3.2):
> "For retry/revision loops (short cycles, same work unit): Allow Done -> Open re-activation on the same task."
> "For recurring processes (sprint cycles, periodic reviews): Create new task instances from a template."

The Cylc pattern was specifically called out as elegant: "parameterizing tasks by cycle point — `task_A[cycle=3]` depends on `task_A[cycle=2]`" where "the definition is cyclic, but each instance is a DAG."

### 2.3 Related Existing Mechanisms

**`wg replay` + runs system** (`src/commands/replay.rs`, `src/runs.rs`):
- Snapshots the entire `graph.jsonl` to `.workgraph/runs/run-NNN/` before resetting tasks
- `reset_task()` clears: status, assigned, started_at, completed_at, artifacts, loop_iteration, failure_reason, paused
- Preserves: log, after, blocks, description, tags, skills
- This is a **manual, graph-wide** mechanism, not per-cycle-iteration

**Agent registry** (`src/service/registry.rs`):
- `AgentEntry` records per-agent-run: pid, task_id, executor, started_at, output_file, model, completed_at
- Registry is **append-only** — completed agents stay with status Done/Failed/Dead
- `output_file` points to the agent's log file, which survives cycle reset
- **But**: the `task_id` in the registry is the bare task ID (e.g., "my-task"), with no iteration suffix. Multiple agents working different iterations of the same task produce separate registry entries, but correlation requires timestamp matching.

**Evaluation storage** (`src/agency/eval.rs`, `.workgraph/agency/evaluations/`):
- Evaluations are stored as JSON files: `eval-{task_id}-{timestamp}.json`
- Each `Evaluation` has `task_id`, `agent_id`, `role_id`, `tradeoff_id`, `score`, `dimensions`, `notes`, `timestamp`
- **No `loop_iteration` field** — evaluations don't know which iteration they belong to
- Multiple evaluations for the same task_id accumulate in `PerformanceRecord.evaluations` as `EvaluationRef` entries
- The `build_score_map()` in replay takes the **highest** score, collapsing iteration history

---

## 3. Per-Iteration Data: Preservation vs. Loss Inventory

### 3.1 Data That SURVIVES Cycle Reset

| Data | Storage Location | Survival Mechanism | Iteration-Aware? |
|------|-----------------|-------------------|-------------------|
| Log entries | `task.log` (in graph.jsonl) | Appended, never cleared | Implicitly (via timestamps + re-activation log entries) |
| `loop_iteration` counter | `task.loop_iteration` | Incremented | Yes — but only current value, no history |
| `last_iteration_completed_at` | `task.last_iteration_completed_at` | Overwritten each reset | No — only most recent |
| Evaluation JSONs | `.workgraph/agency/evaluations/` | Separate files, never deleted | No — keyed by task_id, not task_id+iteration |
| Agent registry entries | `.workgraph/service/registry.json` | Append-only | No — task_id only, no iteration field |
| Agent output logs | File referenced by `AgentEntry.output_file` | Separate files persist | No — file names don't encode iteration |
| Git commits | `.git/` | Immutable | No — commit messages reference task_id, not iteration |
| Provenance log | `.workgraph/provenance/` | Append-only | No — records task_id events |

### 3.2 Data That Is LOST or OVERWRITTEN

| Data | What Happens | Impact |
|------|-------------|--------|
| `status` at completion | Overwritten to `Open` | Can't see per-iteration final status |
| `assigned` (agent) | Cleared to `None` | Can't see which agent worked iteration N |
| `started_at` | Cleared | Per-iteration duration calculation impossible |
| `completed_at` | Moved to `last_iteration_completed_at` (1-deep) | Only last iteration's completion time preserved |
| `token_usage` | Overwritten by next agent | Per-iteration cost tracking lost |
| `session_id` | Overwritten | Can't resume/inspect prior iteration's session |
| `checkpoint` | Stale from prior iteration | Misleading if inspected |
| `triage_count` | Reset to 0 | Per-iteration triage history lost |
| `failure_reason` | Not cleared on normal reset, cleared on failure restart | Stale/misleading |
| `artifacts` | Not cleared but lose provenance | Can't tell which artifacts belong to which iteration |
| `cycle_failure_restarts` | Accumulates globally | No per-iteration breakdown |

### 3.3 Implicit Data Loss via External Systems

Even for data that "survives" in external storage, **iteration correlation is broken**:

- **Evaluations** reference `task_id: "my-task"` — if `my-task` ran 3 iterations, there may be 3 evaluations, but nothing in the evaluation schema links them to iterations 0, 1, 2. You'd have to match timestamps against the task's log entries.
- **FLIP scores** have the same issue — they reference the task_id, not the iteration.
- **Agent performance records** aggregate all evaluations for a task_id into `avg_score` — iteration-level scoring is collapsed.

---

## 4. Gap Analysis: Current -> Spiral

### 4.1 Core Semantic Gap

**Current**: A cycle iteration is a *mutation* of existing task state. The task ID is an identity that persists across iterations, and each iteration overwrites the previous execution state.

**Desired (spiral)**: A cycle iteration *materializes* as new, unique tasks. The cycle definition is a template. Each iteration produces `task-id/iter-0`, `task-id/iter-1`, etc. All iteration history is fully preserved and independently queryable.

### 4.2 Specific Gaps

#### Gap 1: Task ID Uniqueness

**Current**: Task IDs are flat strings, enforced unique in the graph. There's no concept of a "task instance" vs. "task definition."

**Needed**: Either:
- (a) Generate unique IDs per iteration (e.g., `my-task~0`, `my-task~1`) while preserving the "template" ID for structural reference
- (b) Add an iteration index to the task identity (keep same ID but add `(id, iteration)` compound key)

Option (a) is more compatible with existing code — task lookups, dependency resolution, and the graph model all assume string IDs. Option (b) would require pervasive changes to every place that does `graph.get_task(id)`.

**Complexity**: Medium-High. Touches task creation, dependency wiring, all display/query commands.

#### Gap 2: Dependency Rewiring

**Current**: Cycle members reference each other by ID in `after` edges. When iteration N completes, the *same* tasks are re-opened.

**Needed**: When materializing iteration N+1, new tasks need:
- Internal edges: `my-task~1` depends on `setup-task~1` (same-iteration deps)
- Cross-iteration edges: `my-task~1` depends on `my-task~0` (or the previous iteration's final task) if spiral should see prior results
- External edges: Non-cycle tasks that depended on cycle members need to point to the *latest* iteration

**Complexity**: High. The dependency resolution in `reactivate_cycle()`, `wg ready`, topological sort, and SCC analysis all need to understand materialized iterations.

#### Gap 3: Evaluation/FLIP Linkage

**Current**: `Evaluation.task_id` is a bare string. `EvaluationRef` in `PerformanceRecord` also uses bare task_id.

**Needed**: Evaluations must link to `(task_id, iteration)` or to the materialized task ID. The `FlipInferenceInput` and `FlipComparisonInput` structs similarly need iteration context.

**Complexity**: Low-Medium. Schema change + evaluation file naming convention.

#### Gap 4: Coordinator Dispatch Logic

**Current**: The coordinator checks `wg ready` for Open tasks, dispatches agents. Cycle evaluation happens in `evaluate_all_cycle_iterations()` during the coordinator loop.

**Needed**: Instead of mutating existing tasks to Open, the coordinator would need to:
1. Detect cycle completion
2. Instantiate new iteration tasks from the template
3. Wire dependencies
4. Let normal dispatch pick them up

**Complexity**: Medium. Coordinator logic in `src/commands/service/coordinator.rs` needs a new "materialize iteration" step.

#### Gap 5: Query/Display Adaptation

**Current**: `wg list`, `wg show`, `wg viz`, `wg status` treat each task as a single entity.

**Needed**:
- `wg show my-task` could show all iterations (or latest by default)
- `wg list` needs filtering by iteration
- `wg viz` needs to render the spiral — either collapsed (showing iteration count) or expanded
- `wg cycles` needs to distinguish template cycles from materialized iteration chains

**Complexity**: Medium. Many commands need iteration-awareness.

#### Gap 6: Storage Growth

**Current**: Cycle members are a fixed set of tasks. 100 iterations of a 3-task cycle = 3 tasks.

**Spiral**: 100 iterations of a 3-task cycle = 300 tasks in the graph. With log entries, evaluations, and agent records per task, this is a significant storage increase.

**Mitigation**: Compaction/archival of old iterations. The existing `archived` tag pattern could be extended.

### 4.3 What Already Works (and can be leveraged)

1. **`loop_iteration` counter** — already tracked, provides the iteration index for naming
2. **`last_iteration_completed_at`** — shows the one-deep preservation pattern was already desired
3. **`CycleConfig`** — already captures max_iterations, guards, delays — serves as the template config
4. **Runs/snapshot system** — `wg replay` already snapshots full graph state, providing a recovery pattern
5. **Agent registry** — already append-only, just needs an iteration field
6. **Evaluation storage** — already file-per-eval, just needs iteration in the schema
7. **Log entries** — already accumulate with timestamps, providing implicit iteration history
8. **`compute_cycle_analysis()`** — SCC detection already identifies cycle membership

---

## 5. Recommended Approach

### Option A: Full Spiral Materialization (High effort, Maximum fidelity)

Each cycle iteration creates new tasks with unique IDs (`task-id~N`). Cycle definition stored as a template. Dependencies rewired per iteration.

- **Pros**: Clean history, per-iteration queryability, aligns with Cylc/Airflow/Argo patterns
- **Cons**: Large refactor, touches graph model, all query commands, coordinator, viz
- **Rough scope**: 15-25 tasks, multiple agents, 2-4 weeks

### Option B: Iteration Snapshots (Medium effort, Good fidelity)

Keep in-place mutation but snapshot per-iteration state before reset. Add `iteration` field to evaluations and agent records. Store snapshots in `.workgraph/iterations/{task-id}/iter-{N}.json`.

- **Pros**: Minimal graph model changes, preserves all per-iteration data, backward compatible
- **Cons**: Iteration history is in a side-channel, not in the graph itself. Querying requires joining
- **Rough scope**: 8-12 tasks, 1-2 weeks

### Option C: Hybrid — Lazy Materialization (Medium-High effort, Best balance)

Keep in-place mutation as default. Add `--spiral` flag to CycleConfig. When spiral mode is active, `reactivate_cycle()` materializes the completed iteration as archived tasks (`task-id~N` with status Done + `archived` tag) before resetting the "live" tasks. The live tasks always use the original IDs.

- **Pros**: Backward compatible, preserves history as real tasks, query/viz "just work" for archived tasks, opt-in per cycle
- **Cons**: Archived tasks accumulate, need compaction strategy. Dependencies from materialized tasks point to other materialized tasks (needs wiring)
- **Rough scope**: 10-15 tasks, 1.5-3 weeks

### Recommendation

**Option C (Hybrid — Lazy Materialization)** is the best balance. It:
- Preserves the simple in-place cycle model for short retry loops
- Offers full spiral history for cycles where per-iteration data matters
- Leverages existing `archived` tag infrastructure
- Keeps live task IDs stable (no breakage for external references)
- Can be implemented incrementally (archive step first, then query/viz enhancements)

---

## 6. Architectural Blockers and Edge Cases

1. **Task ID format**: The `~` separator (or whatever delimiter) must not conflict with existing ID patterns. Current IDs are kebab-case; `~` is safe.

2. **Graph size**: A 3-task cycle with 100 iterations + spiral = 303 tasks in graph.jsonl. With log entries this could be large. **Mitigation**: Compaction of archived iteration tasks (strip logs, keep metadata).

3. **Dependency cycles in materialized tasks**: Materialized iteration tasks (`task-a~0 -> task-b~0 -> task-a~0`) would form cycles in the graph. These must be tagged/classified as "historical" cycles, not active ones, to avoid confusing `compute_cycle_analysis()`.

4. **FLIP/evaluation pipeline**: `.flip-*` and `.evaluate-*` scaffolding tasks are already filtered from cycle analysis (`src/graph.rs:1083-1090`). Materialized iteration tasks need similar treatment.

5. **Cross-iteration agent context**: If iteration N+1 should see what iteration N produced, the materialized tasks serve as a natural dependency source. The `wg context` command could follow cross-iteration edges.

6. **Race condition**: During the window between "snapshot completed iteration" and "reset live tasks", another agent could modify the graph. The existing flock-based locking in `reactivate_cycle()` (called within graph modification) should cover this.
