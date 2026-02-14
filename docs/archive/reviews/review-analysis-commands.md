# Review: Analysis & Metrics Commands

**Date:** 2026-02-11
**Scope:** 22 command files in `src/commands/`, ~6,100 lines total

## Command Inventory

### Tier 1: Core Commands (Essential - Keep)

| Command | File | Lines | Purpose |
|---------|------|-------|---------|
| `show` | show.rs | 423 | Display full details of a single task (status, blockers, blocks, log, timestamps) |
| `status` | status.rs | 542 | One-screen dashboard: service status, coordinator config, agents, task summary, recent activity |
| `viz` | viz.rs | 1,089 | DAG visualization in dot/mermaid/ASCII formats with critical path highlighting |
| `analyze` | analyze.rs | 1,135 | Comprehensive health report: summary, structural health, bottlenecks, workload, aging, recommendations |

**Notes:** These are the most-used commands and each serves a distinct, well-scoped purpose. `show` is the single-task inspector; `status` is the operational dashboard; `viz` generates visual output; `analyze` is the comprehensive health check.

### Tier 2: Focused Analysis Commands (Useful - Keep with caveats)

| Command | File | Lines | Purpose |
|---------|------|-------|---------|
| `forecast` | forecast.rs | 813 | Project completion forecast with optimistic/realistic/pessimistic scenarios based on velocity |
| `velocity` | velocity.rs | 637 | Weekly task completion rate with trend analysis (increasing/decreasing/stable) |
| `critical-path` | critical_path.rs | 647 | Longest dependency chain by hours, with slack analysis for non-critical tasks |
| `why-blocked` | why_blocked.rs | 490 | For a specific task: show the full blocking tree, root blockers, and actionable advice |
| `next` | next.rs | 321 | Find best next task for an actor based on skill matching and readiness scoring |
| `trajectory` | trajectory.rs | 456 | Follow artifact/context flow from a task through its dependents; suggest paths for actors |

**Notes:** Each of these answers a distinct question. `forecast` depends on `velocity` internally. `why-blocked` is the inverse of `impact` (looks upstream instead of downstream). `next` and `trajectory` are actor-focused planning tools.

### Tier 3: Overlapping/Redundant Commands (Merge candidates)

| Command | File | Lines | Purpose | Overlaps With |
|---------|------|-------|---------|---------------|
| `bottlenecks` | bottlenecks.rs | 333 | Top 10 tasks by transitive impact, with recommendations | `analyze` (bottleneck section) |
| `aging` | aging.rs | 671 | Task age distribution histogram, oldest tasks, stale in-progress | `analyze` (aging section) |
| `workload` | workload.rs | 713 | Per-actor workload breakdown with capacity utilization | `analyze` (workload section) |
| `impact` | impact.rs | 423 | For a specific task: show direct/transitive dependents and hours at risk | `bottlenecks` (same computation) |
| `structure` | structure.rs | 379 | Entry points, dead ends, high-impact roots | `analyze` (dead-end detection, structural health) |
| `loops` | loops.rs | 342 | Cycle detection with classification (intentional/warning/info) | `analyze` (cycle classification, duplicated code) |

### Tier 4: CRUD/Utility Commands (Not analysis - Keep as-is)

| Command | File | Lines | Purpose |
|---------|------|-------|---------|
| `actor` | actor.rs | 428 | Add/list actors (CRUD, not analysis) |
| `resource` | resource.rs | 242 | Add/list resources (CRUD, not analysis) |
| `reschedule` | reschedule.rs | 174 | Set/clear `not_before` on a task (mutation, not analysis) |
| `cost` | cost.rs | 26 | Print total cost of a task including dependencies (thin wrapper around `query::cost_of`) |
| `plan` | plan.rs | 345 | Budget/hours-based sprint planning (what fits in X hours or $Y) |
| `resources` | resources.rs | 525 | Resource utilization tracking (committed vs spent vs available) |

---

## Overlap Analysis

### 1. `build_reverse_index` / `collect_transitive_dependents` - Duplicated 6 times

The exact same helper functions are copy-pasted across:
- `analyze.rs` (lines 451-479)
- `forecast.rs` (lines 303-331)
- `impact.rs` (lines 167-195)
- `bottlenecks.rs` (lines 156-184)
- `show.rs` (lines 303-316)
- `structure.rs` (uses equivalent logic)

**Recommendation:** Extract into a shared `graph_utils` module in the library crate.

### 2. `format_hours` - Duplicated 2 times

Identical function in `viz.rs` (line 764) and `critical_path.rs` (line 241).

**Recommendation:** Move to shared utility.

### 3. `classify_cycle` - Duplicated 2 times

`analyze.rs` (lines 217-258) contains a copy of the cycle classification logic from `loops.rs` (lines 39-80), with a comment acknowledging the duplication.

**Recommendation:** Have `analyze.rs` import from `loops.rs` (it already imports the types but duplicates the function).

### 4. `bottlenecks` vs `analyze` bottleneck section

`bottlenecks.rs` computes the same transitive-impact ranking as `analyze.rs`'s `compute_bottlenecks()`. The standalone version shows top 10 with recommendations; `analyze` shows top 5. Functionally near-identical.

**Recommendation:** **Merge** `bottlenecks` into `analyze`. Users who want just bottlenecks can use `wg analyze --json | jq .bottlenecks`, or add a `--section bottlenecks` flag.

### 5. `aging` vs `analyze` aging section

`aging.rs` provides a detailed age distribution histogram plus stale-in-progress detection. `analyze.rs`'s `compute_aging()` does the same stale detection with simpler bucketing. The standalone version is richer (5-bucket histogram with ASCII bars).

**Recommendation:** **Merge** `aging` into `analyze` by enhancing the aging section to include the histogram. The richer `aging.rs` logic should replace the simpler `analyze.rs` aging computation.

### 6. `workload` vs `analyze` workload section

`workload.rs` shows per-actor breakdown (assigned count, hours, in-progress, capacity, load%). `analyze.rs` `compute_workload()` does the same but shows only overloaded actors.

**Recommendation:** **Merge** `workload` into `analyze` as an expanded workload section, or keep `workload` as the detailed view and have `analyze` reference it.

### 7. `impact` vs `bottlenecks`

`impact.rs` shows downstream impact of a single task (direct + transitive dependents, hours at risk, dependency chains). `bottlenecks.rs` ranks all tasks by that same metric. `impact` is per-task; `bottlenecks` is global.

**Recommendation:** **Keep `impact` as `wg impact <task-id>`** since it serves a different UX (single-task investigation). Remove standalone `bottlenecks` in favor of `analyze`.

### 8. `structure` vs `analyze`

`structure.rs` finds entry points, dead ends, and high-impact roots. `analyze.rs` already has `find_dead_end_open_tasks()` and structural health checks. The two have significant overlap.

**Recommendation:** **Merge** `structure` into `analyze` structural section.

### 9. `loops` vs `analyze`

`loops.rs` provides cycle detection + classification. `analyze.rs` imports the types from `loops.rs` but duplicates the `classify_cycle` function.

**Recommendation:** **Keep `loops` as standalone** (useful for targeted cycle debugging) but fix `analyze.rs` to call `loops::classify_cycle` instead of duplicating it.

### 10. `forecast` vs `velocity`

`forecast.rs` internally calls `velocity::calculate_velocity()` and extends it with completion scenarios, blockers, and critical path. They are complementary rather than redundant.

**Recommendation:** **Keep both.** `velocity` is the raw metric; `forecast` is the projection. Consider making `velocity` a `--velocity` flag on `forecast` to reduce command count, but the standalone velocity view with its histogram is useful.

### 11. `critical_path` vs `forecast` critical path vs `viz --critical-path`

Critical path is computed in three places:
- `critical_path.rs`: Dedicated command with slack analysis
- `forecast.rs` `find_critical_path()`: Simpler version for forecast display
- `viz.rs` `calculate_critical_path()`: For visual highlighting

All three use slightly different implementations of the same algorithm.

**Recommendation:** Extract a single `critical_path::calculate()` in the library crate. `critical_path.rs` keeps its slack analysis as added value. `forecast` and `viz` call the shared implementation.

---

## Recommendations Summary

### Remove (merge into `analyze`)
| Command | Lines saved | Merge into |
|---------|------------|------------|
| `bottlenecks` | 333 | `analyze` bottleneck section (already exists) |
| `structure` | 379 | `analyze` structural section (already exists) |

**Lines saved: ~712** (after removing test boilerplate)

### Consider merging (if reducing command count is a priority)
| Command | Lines | Merge into | Trade-off |
|---------|-------|------------|-----------|
| `aging` | 671 | `analyze` aging section | Loses nice standalone histogram |
| `workload` | 713 | `analyze` workload section | Loses detailed per-actor view |

**Additional lines saved if merged: ~1,384**

### Keep as-is
| Command | Reason |
|---------|--------|
| `show` | Essential single-task inspector |
| `status` | Essential operational dashboard |
| `viz` | Unique output format (visual) |
| `analyze` | Central health report |
| `forecast` | Unique value (completion projections) |
| `velocity` | Unique value (trend analysis) |
| `critical-path` | Unique value (slack analysis) |
| `why-blocked` | Unique value (upstream blocking tree) |
| `impact` | Unique value (downstream impact per-task) |
| `next` | Unique value (actor-task matching) |
| `trajectory` | Unique value (artifact flow tracing) |
| `loops` | Useful for targeted cycle debugging |
| `plan` | Unique value (budget/hours sprint planning) |
| `resources` | Unique value (resource utilization) |
| `cost` | Tiny, thin wrapper - harmless |
| `actor` | CRUD, not analysis |
| `resource` | CRUD, not analysis |
| `reschedule` | Mutation, not analysis |

### Code quality improvements (regardless of merging decisions)

1. **Extract `build_reverse_index` + `collect_transitive_dependents`** into `src/graph_utils.rs` or `src/lib.rs` — currently duplicated 6 times across files.

2. **Extract `format_hours`** into a shared utility — duplicated in `viz.rs` and `critical_path.rs`.

3. **Fix `analyze.rs` cycle classification** — currently duplicates `loops::classify_cycle` instead of calling it. The import is set up but the function is re-implemented.

4. **Extract critical path computation** into the library crate — currently three different implementations in `critical_path.rs`, `forecast.rs`, and `viz.rs`.

5. **Extract `make_task` test helper** — every single test module (22 files!) has its own `make_task()` helper that constructs an identical default Task. This should be a `#[cfg(test)]` helper in one place.

---

## Test Bloat

A significant portion of these files is test code with repeated boilerplate. The `make_task()` helper alone is duplicated in all 22 files (~30 lines each = ~660 lines of pure duplication). Similarly, `make_actor()` appears in 5+ files. Creating a `test_helpers` module would reduce total line count significantly.

---

## Summary

- **22 commands reviewed**, ~6,100 lines
- **2 commands clearly redundant** (`bottlenecks`, `structure`) — merge into `analyze`
- **2 commands potentially redundant** (`aging`, `workload`) — could merge into `analyze` but have standalone value
- **6 instances of duplicated utility code** (`build_reverse_index`, `collect_transitive_dependents`)
- **3 implementations of critical path** should share one library function
- **~660 lines of duplicated test helpers** across all files
- **Conservative line savings from merges: ~712 lines; aggressive: ~2,096 lines**
- **Code dedup savings (shared utilities + test helpers): ~800-1000 additional lines**
