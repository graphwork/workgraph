# Design: Cycle-to-Spiral Unrolling Mechanism

**Date**: 2026-04-01
**Task**: design-cycle-to
**Depends on**: research-spiral-cycle (gap analysis)
**Status**: Design complete

---

## Executive Summary

This design introduces **spiral mode** — an opt-in enhancement to workgraph cycles that preserves per-iteration history as archived snapshot tasks. When a spiral-enabled cycle iterates, completed tasks are cloned as `{task-id}~{iteration}` before the live tasks are reset. This preserves per-iteration FLIP scores, evaluations, token usage, agent assignments, and artifacts while keeping the live task IDs stable for external references.

**Key principle**: The cycle definition remains the structural template. Each iteration materializes as a frozen snapshot. Simple cycles (no `spiral: true`) continue to use the existing lightweight in-place reset.

---

## Design Decisions

### 1. Task ID Scheme

**Decision**: Suffix with tilde + iteration index: `{original-id}~{N}`

| Aspect | Detail |
|--------|--------|
| Separator | `~` (tilde) |
| Format | `{source-task-id}~{zero-based-iteration}` |
| Examples | `build-report~0`, `build-report~1`, `.compact-0~3` |
| Live task | Keeps original ID (`build-report`) — always represents the "current/next" iteration |
| Validation | `~` is reserved for spiral archives; reject in user-supplied task IDs |

**Rationale**:
- `~` is not in the current ID alphabet (`[a-z0-9._-]`), so no ambiguity
- `~` is shell-safe (in quotes), URL-safe, and visually distinct from `-` or `.`
- Suffix-based (not compound key) means task lookups, dependency resolution, and the graph model continue to use simple string IDs — no pervasive refactor
- Alternative considered: `{id}/iter-{N}` — rejected because `/` is a path separator and would break file-based operations
- Alternative considered: compound `(id, iteration)` key — rejected because it requires changes to every `get_task(id)` call site

**ID Parsing**:
```rust
/// Parse a spiral archive task ID into (source_id, iteration).
/// Returns None if the ID is not a spiral archive.
fn parse_spiral_id(id: &str) -> Option<(&str, u32)> {
    let (source, iter_str) = id.rsplit_once('~')?;
    let iteration = iter_str.parse::<u32>().ok()?;
    Some((source, iteration))
}
```

---

### 2. Dependency Rewiring

**Decision**: Archived iteration tasks have **no dependency edges**. They are standalone snapshots.

| Edge type | Handling |
|-----------|----------|
| Intra-iteration (A~0 → B~0) | **Not created**. Execution order is recorded in timestamps. |
| Cross-iteration (A~1 → A~0) | **Not created** by default. Convergence comparison uses iteration index, not edges. |
| External → cycle member | **Unchanged**. External tasks depend on the live task ID, which remains stable. |
| Archived → anything | **None**. Archives are read-only history. |

**Rationale**:
- Mirroring intra-iteration deps would create historical cycles in the graph, confusing `compute_cycle_analysis()` (Havlak's algorithm). Filtering them adds complexity.
- Cross-iteration edges would create long chains that pollute `wg viz` and topological sort.
- Archived tasks are **passive artifacts**, not executable work items. Their value is in their data (evaluations, artifacts, logs), not their graph position.
- Ordering within an iteration is recoverable from `started_at`/`completed_at` timestamps on the archived tasks.

**Exception**: If a future use case requires cross-iteration dependency tracking (e.g., "iteration 3 was triggered because iteration 2 failed validation"), this can be added as an optional `spiral_parent: Option<String>` field on archived tasks pointing to the previous iteration's archive. Deferred to a follow-up.

---

### 3. Evaluation Linkage

**Decision**: Evaluation scaffolding (`.flip-*`, `.evaluate-*`) is created during the archival step, targeting the archived task ID.

#### Current Flow (non-spiral)
```
task Done → coordinator creates .flip-{task-id} → FLIP agent evaluates
                                                      → .evaluate-{task-id} created
```

#### Spiral Flow
```
All cycle members Done
  → reactivate_cycle_spiral() fires
    → Archive: create {task-id}~{N} (Done, spiral-archive tag)
    → Create .flip-{task-id}~{N} --after {task-id}~{N}
    → Reset live {task-id} to Open (iteration N+1)

FLIP agent evaluates {task-id}~{N}
  → Evaluation stored as eval-{task-id}~{N}-{timestamp}.json
  → Evaluation.task_id = "{task-id}~{N}"
  → Natural per-iteration indexing — no schema change needed
```

**Scaffolding Pipeline Change**: The coordinator's scaffolding pipeline must detect when a completed task is a member of a spiral cycle and **skip** `.flip-*` / `.evaluate-*` creation. The archival step handles it instead.

Detection logic:
```rust
fn is_in_spiral_cycle(task_id: &str, cycle_analysis: &CycleAnalysis, graph: &WorkGraph) -> bool {
    if let Some(&cycle_idx) = cycle_analysis.task_to_cycle.get(task_id) {
        let cycle = &cycle_analysis.cycles[cycle_idx];
        // Find config owner and check spiral flag
        cycle.members.iter().any(|mid| {
            graph.get_task(mid)
                .and_then(|t| t.cycle_config.as_ref())
                .map(|c| c.spiral)
                .unwrap_or(false)
        })
    } else {
        false
    }
}
```

**Evaluation struct**: No schema change required. `Evaluation.task_id` naturally stores the archived task ID (e.g., `"build-report~2"`), which encodes both the source task and iteration.

**EvaluationRef**: Similarly uses the archived task ID. `PerformanceRecord.evaluations` will have entries like:
```json
{"score": 0.85, "task_id": "build-report~0", "timestamp": "..."}
{"score": 0.91, "task_id": "build-report~1", "timestamp": "..."}
```

This makes per-iteration score comparison trivial.

**Agent registry**: No schema change. When the coordinator dispatches an agent for the live `build-report` task, the registry records `task_id: "build-report"`. The `loop_iteration` at dispatch time is available from the task. For post-hoc correlation, match the agent's `started_at` against the archived task's `started_at`.

---

### 4. Convergence Detection

**Decision**: Manual convergence (`--converged`) works as today. Add optional auto-convergence based on cross-iteration score comparison.

#### Manual (unchanged)
```bash
wg done build-report --converged
```
Sets `converged` tag → `reactivate_cycle()` checks for it → stops iteration. Identical to current behavior.

#### Auto-convergence (new, optional)
New `CycleConfig` field:
```rust
/// Auto-converge when the absolute score delta between consecutive iterations
/// falls below this threshold. Requires spiral mode. Checked after evaluation
/// of iteration N completes.
pub spiral_convergence_threshold: Option<f64>,
```

**Convergence check flow** (runs when `.flip-{task-id}~{N}` completes):
1. Parse `N` from the FLIP task's dependency
2. Look for `.flip-{task-id}~{N-1}` — if not found or not Done, skip
3. Load both evaluations: `eval-{task-id}~{N}` and `eval-{task-id}~{N-1}`
4. Compute delta: `|score_N - score_(N-1)|`
5. If delta < `spiral_convergence_threshold`: add `converged` tag to the live task

**Implementation note**: This check runs in the coordinator's scaffolding pipeline, after FLIP task completion. It's a new step in the pipeline, not a change to `reactivate_cycle()`.

**Deferred**: This auto-convergence is a Phase 2 enhancement. Phase 1 delivers archival + manual convergence. The design accommodates it without structural changes.

---

### 5. Storage

**Decision**: Archived iteration tasks are **normal graph entries** in `graph.jsonl`.

| Aspect | Detail |
|--------|--------|
| Location | Same `graph.jsonl` as all other tasks |
| Serialization | Standard Task JSON (same schema) |
| Tags | `spiral-archive` (identifies archives), task's original tags preserved |
| Status | `Done` (frozen at completion) |
| Queryability | All existing commands work: `wg show`, `wg list --tag spiral-archive`, `wg viz` |

**Task fields on archived snapshot**:

| Field | Value | Source |
|-------|-------|--------|
| `id` | `{source}~{N}` | Generated |
| `title` | Copy from live task (prefixed: `"[iter {N}] {title}"`) | Live task |
| `description` | Copy from live task | Live task |
| `status` | `Done` | Fixed |
| `assigned` | Copy (the agent that worked this iteration) | Live task |
| `started_at` | Copy | Live task |
| `completed_at` | Copy | Live task |
| `artifacts` | **Move** from live task (live task's artifacts are cleared on reset) | Live task |
| `log` | **Partition**: entries from this iteration only (by timestamp, since last reactivation) | Live task |
| `token_usage` | Copy | Live task |
| `session_id` | Copy | Live task |
| `tags` | Original tags + `spiral-archive` | Live task |
| `loop_iteration` | `N` | Iteration counter |
| `verify` | Copy (structural, for reference) | Live task |
| `agent` | Copy (agency hash of assigned agent) | Live task |
| `cycle_config` | **Not set** (archives are not cycle participants) | — |
| `after` | **Empty** (no deps) | — |
| `before` | **Empty** (no deps) | — |
| `model` | Copy | Live task |
| `created_at` | Archival timestamp | Generated |

**Log partitioning**: Each reactivation appends a "Re-activated by cycle iteration" log entry. The archival step copies only log entries with timestamps **after** the most recent reactivation entry (or all entries for iteration 0). This gives each archive a clean, self-contained log.

**Storage growth mitigation**:
- 3-task cycle × 100 iterations = 303 tasks (300 archived + 3 live). Plus ~300 `.flip-*` tasks.
- Each archived task is smaller than a live task (no deps, no cycle_config, iteration-only logs).
- Existing `archived` tag infrastructure can compact spiral archives: strip logs, keep metadata.
- `wg gc` can prune old spiral archives beyond a retention window (e.g., keep last 10 iterations).

**Filtering from cycle analysis**: `compute_cycle_analysis()` already filters system scaffolding by prefix. Add a filter for spiral archives:

```rust
fn is_system_scaffolding(id: &str) -> bool {
    id.starts_with(".assign-")
        || id.starts_with(".flip-")
        || id.starts_with(".evaluate-")
        || id.starts_with(".place-")
        || id.contains('~')  // spiral archive
}
```

---

### 6. Migration Strategy

**Decision**: Zero migration required. Spiral mode is opt-in per cycle.

#### Enabling spiral on a new cycle
```bash
wg add "task-a" --after task-b --max-iterations 5 --spiral
# Sets cycle_config.spiral = true on the config owner
```

#### Enabling spiral on an existing cycle
```bash
wg config-task <config-owner-id> --spiral
# Or edit graph.jsonl directly: add "spiral": true to cycle_config
```

**What happens when you enable spiral mid-cycle**:
- Current `loop_iteration` is 3 (iterations 0-2 ran without archival)
- Iteration 3 completes → archival creates `{task}~3`
- Iterations 0-2 have no archives (data was lost to in-place mutation)
- This is acceptable — spiral captures history **from when it's enabled**, not retroactively

**What happens when you disable spiral on an active cycle**:
- Set `spiral: false` in cycle_config
- Next iteration uses in-place reset (no archival)
- Existing archives remain in the graph (tagged `spiral-archive`, can be gc'd)

#### No schema migration
- `CycleConfig` gains `spiral: bool` with `#[serde(default)]` → existing JSON without the field defaults to `false`
- `spiral_convergence_threshold: Option<f64>` with `skip_serializing_if = "Option::is_none"` → invisible in existing data
- No changes to existing Task fields

---

### 7. Backwards Compatibility

**Decision**: Full backwards compatibility. Non-spiral cycles are completely unaffected.

| Scenario | Behavior |
|----------|----------|
| Existing cycle, no `spiral` field | `spiral` defaults to `false` → in-place reset (unchanged) |
| New cycle without `--spiral` | Same as above |
| `wg done --converged` | Works identically in both modes |
| External tasks depending on cycle members | Depend on live task IDs → unaffected by archival |
| `wg list`, `wg show`, `wg viz` | Archives appear as normal tasks; filter with `--tag spiral-archive` to include/exclude |
| `wg cycles` | Archives filtered from cycle analysis → only live cycles shown |
| `wg replay` / runs system | Operates on live graph → archives are part of the graph and get snapshotted |
| Agent dispatch | Agents work on live task IDs → no change to coordinator dispatch |
| FLIP / evaluation | Non-spiral: unchanged. Spiral: evaluations target archived task IDs |

**Simple cycles (retry loops, short feedback cycles)**: Continue using lightweight in-place reset. The `spiral` flag is only set when per-iteration history matters. The default (`spiral: false`) preserves current behavior exactly.

---

## Data Model Changes

### CycleConfig (src/graph.rs:8-28)

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CycleConfig {
    pub max_iterations: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<LoopGuard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_converge: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub restart_on_failure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failure_restarts: Option<u32>,

    // ── NEW FIELDS ──

    /// Enable spiral mode: archive each iteration as `{task-id}~{N}` snapshot
    /// tasks before resetting live tasks. Preserves per-iteration history.
    #[serde(default, skip_serializing_if = "is_false")]
    pub spiral: bool,

    /// Auto-converge when |score(iter N) - score(iter N-1)| < threshold.
    /// Requires `spiral: true`. Phase 2 enhancement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spiral_convergence_threshold: Option<f64>,
}
```

### No changes to Task struct

Archived task identity and iteration are encoded in the task ID (`~` separator) and tags (`spiral-archive`). No new fields on Task.

### No changes to Evaluation struct

`Evaluation.task_id` stores the archived task ID (e.g., `"build-report~2"`), which naturally encodes the iteration.

### No changes to AgentEntry struct

Agent registry records the live task ID. Post-hoc correlation uses timestamp matching against archived tasks.

### CLI changes

| Command | Change |
|---------|--------|
| `wg add ... --spiral` | New flag: sets `cycle_config.spiral = true` |
| `wg add ... --spiral-threshold 0.05` | New flag: sets `spiral_convergence_threshold` |
| `wg config-task <id> --spiral` | Enable spiral on existing cycle config owner |
| `wg iterations <task-id>` | **New command**: list all spiral archives for a task |
| `wg list --tag spiral-archive` | Existing mechanism, works out of the box |
| `wg viz --show-spiral` | New flag: include spiral archives in visualization |

---

## Sequence Diagram: Cycle Member Completes an Iteration (Spiral Mode)

```
                          Agent
                            │
                            │  wg done build-report
                            ▼
                     ┌──────────────┐
                     │  Task: Done  │  build-report.status = Done
                     │  iter = 1    │  build-report.completed_at = now
                     └──────┬───────┘
                            │
              ┌─────────────▼──────────────┐
              │ evaluate_cycle_iteration()  │
              │ (src/graph.rs:1390)         │
              └─────────────┬──────────────┘
                            │
                    ┌───────▼───────┐
                    │ All members   │───── No ──► return [] (wait)
                    │ terminal?     │
                    └───────┬───────┘
                           Yes
                    ┌───────▼───────┐
                    │ Converged?    │───── Yes ─► return [] (stop)
                    └───────┬───────┘
                           No
                    ┌───────▼───────┐
                    │ max_iterations│───── Hit ─► return [] (stop)
                    │ check         │
                    └───────┬───────┘
                          Pass
                    ┌───────▼───────┐
                    │ spiral: true? │───── No ──► reactivate_cycle() [existing]
                    └───────┬───────┘
                          Yes
              ┌─────────────▼──────────────┐
              │ reactivate_cycle_spiral()   │  NEW FUNCTION
              └─────────────┬──────────────┘
                            │
          ┌─────────────────▼─────────────────┐
          │  FOR EACH Done member (skip        │
          │  Abandoned/Archived):              │
          │                                    │
          │  1. CREATE ARCHIVE TASK            │
          │     id: "{member-id}~{N}"          │
          │     status: Done                   │
          │     title: "[iter {N}] {title}"    │
          │     tags: [...original, spiral-    │
          │            archive]                │
          │     Copy: assigned, started_at,    │
          │       completed_at, token_usage,   │
          │       session_id, agent, model,    │
          │       verify                       │
          │     Move: artifacts (live→archive) │
          │     Partition: log entries for      │
          │       this iteration only          │
          │     No deps (after=[], before=[])  │
          │     No cycle_config                │
          │                                    │
          │  2. CREATE FLIP TASK               │
          │     id: ".flip-{member-id}~{N}"    │
          │     after: ["{member-id}~{N}"]     │
          │                                    │
          │  3. RESET LIVE TASK                │
          │     status: Open                   │
          │     assigned: None                 │
          │     started_at: None               │
          │     completed_at → last_iteration_ │
          │       completed_at, then None      │
          │     artifacts: [] (moved to arch.) │
          │     token_usage: None              │
          │     session_id: None               │
          │     checkpoint: None               │
          │     triage_count: 0                │
          │     loop_iteration: N+1            │
          │     Append log: "Re-activated      │
          │       (spiral iter {N+1}/{max},    │
          │       archived as {id}~{N})"       │
          │     ready_after: set if delay      │
          │       configured (config owner)    │
          │                                    │
          └─────────────────┬─────────────────┘
                            │
                            ▼
                    Return reactivated IDs
                            │
                            ▼
              Coordinator dispatches agents
              for newly-Open live tasks
```

---

## Concrete Example: .compact-0 Cycle

The `.compact-0` cycle is the coordinator's self-optimization cycle. It has:
- **Members**: `.compact-0` (config owner with `cycle_config`)
- **Purpose**: Periodically distill graph state into `context.md`
- **Current behavior**: Resets in-place, accumulating logs. Token usage from each compaction is overwritten. No per-iteration evaluation possible.

### With Spiral Mode Enabled

```bash
wg config-task .compact-0 --spiral
```

**Iteration 0 completes**:
- `.compact-0` marked Done (compaction produced `context.md`)
- Archival creates `.compact-0~0`:
  - `status: Done`
  - `title: "[iter 0] compact-0"`
  - `tags: ["spiral-archive"]`
  - `artifacts: [".workgraph/compactor/context.md"]` (moved from live task)
  - `token_usage: { input: 15000, output: 2000, cost: 0.05 }` (preserved!)
  - `session_id: "sess-abc123"` (preserved!)
  - `log: [entries from iteration 0 only]`
- `.flip-.compact-0~0` created → FLIP agent evaluates compaction quality → score: 0.72
- Live `.compact-0` reset to Open, `loop_iteration: 1`

**Iteration 1 completes**:
- Archival creates `.compact-0~1`:
  - `token_usage: { input: 18000, output: 2500, cost: 0.06 }`
  - `artifacts: [".workgraph/compactor/context.md"]` (version from iter 1)
- `.flip-.compact-0~1` → score: 0.81

**Observable convergence history**:
```
Iteration  Score  Tokens   Cost
0          0.72   15000    $0.05
1          0.81   18000    $0.06
2          0.84   16000    $0.055
3          0.85   16500    $0.056  ← converging, delta < 0.02
```

The user (or auto-convergence) can now make informed decisions about when compaction has stabilized.

**Slip tracking**: If iteration 2 scores lower than iteration 1, that's a regression detectable by comparing `eval-.compact-0~2` vs `eval-.compact-0~1`.

---

## Implementation Phases

### Phase 1: Core Archival (MVP)
Scope: ~8-10 tasks

1. **CycleConfig changes**: Add `spiral: bool`, `spiral_convergence_threshold: Option<f64>` with serde defaults
2. **`reactivate_cycle_spiral()` function**: New function parallel to `reactivate_cycle()`, called when `spiral: true`
3. **Archive task creation**: Clone task data, generate `~N` ID, set tags, partition logs
4. **Artifact transfer**: Move artifacts from live to archive, clear on live
5. **Scaffolding pipeline gate**: Skip `.flip-*` creation for spiral cycle members
6. **FLIP creation in archival**: Create `.flip-{id}~{N}` during `reactivate_cycle_spiral()`
7. **Cycle analysis filter**: Add `id.contains('~')` to `is_system_scaffolding()`
8. **CLI: `--spiral` flag**: For `wg add` and `wg config-task`
9. **Task ID validation**: Reject `~` in user-supplied IDs
10. **Tests**: Unit tests for archival, integration test for full spiral cycle lifecycle

### Phase 2: Query & Visualization
Scope: ~4-6 tasks

1. **`wg iterations <task-id>`**: New command listing all `{task-id}~*` archives with scores
2. **`wg viz --show-spiral`**: Include archives in visualization
3. **`wg show` enhancement**: Show iteration history summary when viewing a live cycle task
4. **`wg list` filter**: `--spiral-source <id>` to list all iterations of a specific task

### Phase 3: Auto-Convergence
Scope: ~3-4 tasks

1. **Convergence check**: After `.flip-{id}~{N}` completes, compare scores with `{id}~{N-1}`
2. **Auto-converge**: If delta < threshold, add `converged` tag to live task
3. **`--spiral-threshold` CLI flag**: Set convergence threshold
4. **Coordinator integration**: Wire convergence check into scaffolding pipeline

### Phase 4: Compaction & Retention
Scope: ~2-3 tasks

1. **`wg gc --spiral-retain N`**: Keep only last N spiral archives per source task
2. **Archive compaction**: Strip logs from old archives, keep metadata + score

---

## Open Questions (Deferred)

1. **Cross-iteration context injection**: Should the agent working iteration N+1 automatically see artifacts/results from iteration N? Could add `{task-id}~{N}` as a soft input. Deferred — agents can use `wg show {task}~{N}` manually.

2. **Failure restart in spiral mode**: When `restart_on_failure` triggers during spiral, should the failed iteration be archived (with Failed status)? Tentative: yes, archive as `{id}~{N}` with `status: Failed` for debugging visibility. Details TBD in implementation.

3. **Federation**: Spiral archives contain per-iteration data. Which visibility zone applies? Tentative: inherit from the live task. Details TBD when federation reaches spiral-aware code.

4. **Replay interaction**: `wg replay` snapshots the entire graph. Spiral archives are part of the graph and get snapshotted. No special handling needed, but verify this doesn't cause unexpected graph bloat during replay.

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Graph bloat from many iterations | Medium | Medium | Phase 4 retention policy + gc |
| Performance degradation (large graph) | Low | Medium | Archives are passive (no dispatch, no cycle analysis) |
| Race between scaffolding pipeline and archival | Low | High | Archival runs inside flock-protected `reactivate_cycle()` |
| Confusion: user sees both live and archived tasks | Medium | Low | `spiral-archive` tag + `wg list` defaults exclude them |
| Breaking existing cycles | Very Low | High | `spiral: false` default + serde backward compat |
