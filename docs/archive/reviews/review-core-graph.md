# Review: Core Graph Engine

**Files reviewed:** `src/graph.rs` (741 lines), `src/parser.rs` (302 lines), `src/query.rs` (744 lines), `src/check.rs` (332 lines)
**Total:** ~2,119 lines
**Date:** 2026-02-11

---

## 1. `src/graph.rs` — Data Model (741 lines)

### What it does

Defines the core data types for the work graph: `Task`, `Actor`, `Resource`, and the `WorkGraph` container. This is the foundational module — 48 files across the codebase depend on it.

### Key types

| Type | Purpose |
|------|---------|
| `Task` | Work item with status, dependencies (`blocked_by`/`blocks`), estimates, timestamps, log entries, agent assignment, and verification criteria |
| `Actor` | Human or AI agent with capabilities, trust level, rate, capacity, and Matrix integration |
| `Resource` | Budget/compute constraint with availability tracking |
| `Node` | Tagged union (`Task` / `Actor` / `Resource`) with JSON `kind` discriminator |
| `WorkGraph` | `HashMap<String, Node>` wrapper with typed accessor methods |
| `Status` | Task lifecycle: `Open → InProgress → Done` (plus `Blocked`, `Failed`, `Abandoned`, `PendingReview`) |
| `LogEntry` | Timestamped progress note with optional actor attribution |
| `Estimate` | Optional hours/cost pair for planning |

### Key functions

- `WorkGraph::add_node`, `get_task`, `get_task_mut`, `get_actor`, `get_resource`, `remove_node` — standard CRUD
- `WorkGraph::tasks()`, `actors()`, `resources()` — filtered iterators
- `WorkGraph::get_actor_by_matrix_id` — lookup by Matrix user ID
- Custom `Deserialize` impl for `Task` — migrates legacy `identity` field to `agent` hash

### Code smells & issues

#### 1. **Task struct is very large (28 fields)** — HIGH
The `Task` struct has grown organically to 28 fields. Creating one in tests requires specifying every field, leading to the `make_task` helper being duplicated 4 times across test modules (`graph.rs`, `parser.rs`, `query.rs`, `check.rs`). The struct has become a "god object" that mixes core identity (`id`, `title`, `status`), planning (`estimate`, `inputs`, `deliverables`), execution (`assigned`, `exec`, `model`, `agent`), lifecycle (`created_at`, `started_at`, `completed_at`, `retry_count`), and logging (`log`, `failure_reason`).

**Recommendation:** Consider a `Task::new(id, title)` constructor that sets sensible defaults, eliminating the duplicated `make_task` helpers. A builder pattern or `Default` impl would help. Breaking the struct into nested sub-structs (e.g., `TaskLifecycle`, `TaskPlanning`) could improve clarity but may be over-engineering at this scale.

#### 2. **`TaskHelper` duplicates Task field-by-field** — MEDIUM
The `TaskHelper` struct (lines 122-177) mirrors all 28 Task fields plus the legacy `identity` field, purely to support deserializing the old `identity` format. Every time a field is added to `Task`, `TaskHelper` must also be updated — a maintenance hazard.

**Recommendation:** Consider using `#[serde(flatten)]` with a wrapper, or a post-deserialization migration step that operates on `serde_json::Value` before deserializing into `Task`. Alternatively, if the legacy format is no longer in active use, remove the migration code entirely.

#### 3. **`blocks` and `blocked_by` are not kept in sync** — HIGH
These two fields represent the same relationship from opposite directions, but no code enforces bidirectional consistency. If task A's `blocks` contains B, nothing ensures B's `blocked_by` contains A. The query engine (`ready_tasks`, `blocked_by`, `cost_of`) only reads `blocked_by`, so `blocks` is effectively cosmetic — it's only used in visualization commands and orphan checks. This is a latent source of bugs.

**Recommendation:** Either:
- (a) **Remove `blocks` entirely** and compute it on demand from `blocked_by` (the canonical direction). This is the simplest fix since `blocks` is never used for scheduling logic.
- (b) **Enforce sync** by making `add_node` or a graph-level method automatically mirror `blocks` ↔ `blocked_by`.

#### 4. **Timestamps are `Option<String>` instead of typed** — LOW
All timestamp fields (`created_at`, `started_at`, `completed_at`, `not_before`) are `Option<String>`. This means parsing is deferred to call sites (e.g., `query.rs:12` does `timestamp.parse::<DateTime<Utc>>()`), and invalid timestamps silently pass through. Using `Option<DateTime<Utc>>` with serde's chrono support would catch errors at parse time.

**Recommendation:** Migrate to `Option<chrono::DateTime<Utc>>` with `#[serde(with = "...")]` for ISO 8601 formatting. This would remove defensive parsing scattered through query code.

#### 5. **`is_zero` and `is_default_*` helper functions** — LOW
Small free functions (`is_zero`, `is_default_trust`, `is_default_actor_type`) exist solely for `skip_serializing_if`. These are idiomatic serde, but could be simplified with a generic `is_default<T: Default + PartialEq>(val: &T) -> bool` helper.

### Test coverage

Good coverage (lines 465-741): node CRUD, serialization/deserialization roundtrips, legacy identity migration, timestamp handling, `skip_serializing_if` behavior. Tests are thorough but verbose due to the large `make_task` constructor.

---

## 2. `src/parser.rs` — JSONL Persistence (302 lines)

### What it does

Loads and saves the graph to/from a JSONL file (one JSON object per line). Implements file-level advisory locking via `flock(2)` to prevent concurrent access corruption. This is the second most depended-upon module (43 files).

### Key types & functions

| Item | Purpose |
|------|---------|
| `ParseError` | Error enum: IO, JSON (with line number), Lock |
| `FileLock` | RAII guard using `flock(2)` on Unix, no-op on other platforms |
| `load_graph(path)` | Read JSONL, skip blanks/comments, deserialize nodes |
| `save_graph(graph, path)` | Serialize all nodes to JSONL with truncation |
| `get_lock_path(path)` | Derives `graph.lock` path from graph file path |

### Code smells & issues

#### 1. **Lock is held during full read/write, not atomic swap** — MEDIUM
`save_graph` truncates the file and writes line-by-line while holding the lock. If the process crashes mid-write, the file is partially written and data is lost. The concurrent access test (line 253) exercises locking but doesn't verify data integrity after concurrent writes — the assertion is just `final_graph.len() > 0`.

**Recommendation:** Write to a temporary file first, then atomically rename (via `std::fs::rename`) while holding the lock. This is the standard pattern for crash-safe file updates.

#### 2. **Lock scope includes deserialization/serialization** — LOW
The lock is held while deserializing all nodes during `load_graph`. For large graphs, this blocks other processes unnecessarily. With atomic rename for writes, reads wouldn't need locking at all (reads of renamed files are atomic on POSIX).

#### 3. **`save_graph` doesn't guarantee node ordering** — LOW
Nodes come from a `HashMap`, so serialization order is non-deterministic. This means `git diff` of the graph file is noisy. Deterministic ordering (e.g., sort by ID) would make diffs more readable.

**Recommendation:** Sort `graph.nodes()` by ID before writing. This is a one-line change (`let mut nodes: Vec<_> = graph.nodes().collect(); nodes.sort_by_key(|n| n.id());`).

#### 4. **Unused import: `AsRawFd` at module level** — LOW
Line 8: `use std::os::unix::io::AsRawFd;` is imported at module scope (behind `#[cfg(unix)]`) and then re-imported inside `FileLock::acquire` (line 33). The module-level import is dead code.

#### 5. **No-op lock on non-Unix is silent** — LOW
On non-Unix platforms, `FileLock::acquire` returns a no-op. This could lead to data corruption on Windows with no warning. A log warning or compile-time notice would help.

### Test coverage

Good coverage (lines 141-302): empty file, single/multiple nodes, comment/blank line handling, invalid JSON, roundtrip, nonexistent file, concurrent access. The concurrent access test is a good idea but the assertions are weak (only checks `len() > 0`).

---

## 3. `src/query.rs` — Query Engine (744 lines)

### What it does

Implements the core scheduling queries: which tasks are ready, what's blocking a task, transitive cost calculation, project summary, and budget/hours fitting. Used by 14 files including the service coordinator.

### Key types & functions

| Item | Purpose |
|------|---------|
| `ready_tasks(graph)` | Tasks that are `Open`, past `not_before`, with all `blocked_by` deps `Done` |
| `blocked_by(graph, id)` | Undone blockers of a task |
| `cost_of(graph, id)` | Transitive cost through dependency chain |
| `project_summary(graph)` | Counts by status, total cost/hours of open tasks |
| `tasks_within_budget(graph, budget)` | Greedy fit: ready tasks first, then newly-unblocked |
| `tasks_within_hours(graph, hours)` | Same as budget but using hours |
| `is_time_ready(task)` | Check `not_before` timestamp |
| `ProjectSummary`, `FitResult`, `TaskFitInfo` | Result structs |

### Code smells & issues

#### 1. **`ready_tasks` is called redundantly in `project_summary` and `tasks_within_constraint`** — MEDIUM
`project_summary` calls `ready_tasks` to get the ready set, then iterates all tasks again. `tasks_within_constraint` also calls `ready_tasks` internally. When these are composed (e.g., a dashboard showing summary + fit), `ready_tasks` runs multiple times. This is O(n * m) where m is dependency chain depth, repeated per caller.

**Recommendation:** Either cache the ready set or pass it in as a parameter. A `GraphAnalysis` struct that lazily computes and caches `ready_tasks` would prevent redundant work.

#### 2. **`tasks_within_constraint` has O(n²) inner loop** — MEDIUM
The "second pass" (lines 176-215) iterates all open tasks in a `while changed` loop. Each iteration scans all tasks to find newly-unblockable ones. For a graph with k layers of sequential dependencies, this is O(k * n). Fine for typical use (< 1000 tasks) but could be improved with a topological sort approach.

#### 3. **`ready_tasks` treats missing blockers as unblocked** — LOW (intentional)
Line 242: `unwrap_or(true)` means if a `blocked_by` reference points to a non-existent task, the dependency is considered satisfied. This is documented as intentional but could mask data quality issues. Consider logging a warning.

#### 4. **`is_time_ready` treats invalid timestamps as ready** — LOW (intentional)
Line 14: invalid `not_before` timestamps are treated as "ready." Same concern — this silently masks bad data. Logging would help.

#### 5. **`project_summary` doesn't count `Failed`/`Abandoned` in any bucket** — LOW
Failed and abandoned tasks silently fall through the match arms (lines 81-83). The `ProjectSummary` struct has no field for these. Users may wonder why task counts don't add up to total.

**Recommendation:** Add `failed` and `abandoned` counts to `ProjectSummary`, or at least a `total` field.

#### 6. **`cost_of` only considers `cost`, not `hours`** — NAMING
The function name `cost_of` is ambiguous — it only sums `estimate.cost`, not `estimate.hours`. A companion `hours_of` doesn't exist. The `wg cost` command uses this function.

**Recommendation:** Either rename to `total_cost_of` for clarity, or make it generic (like `tasks_within_constraint`) to work with either field.

### Test coverage

Excellent coverage (lines 296-744): empty graph, single/multiple tasks, blocked/unblocked transitions, cycle handling in `cost_of`, project summary with mixed statuses, budget fitting with priority, time-based readiness. 18 test functions covering edge cases thoroughly.

---

## 4. `src/check.rs` — Validation (332 lines)

### What it does

Validates graph integrity: cycle detection in `blocked_by` dependencies and orphan reference detection (references to non-existent nodes). Used by only 3 files.

### Key types & functions

| Item | Purpose |
|------|---------|
| `check_cycles(graph)` | DFS-based cycle detection on `blocked_by` edges |
| `check_orphans(graph)` | Find references to non-existent nodes (blocked_by, blocks, assigned, requires) |
| `check_all(graph)` | Combined check returning `CheckResult` |
| `CheckResult` | `{ cycles, orphan_refs, ok }` |
| `OrphanRef` | `{ from, to, relation }` describing a dangling reference |

### Code smells & issues

#### 1. **Cycle detection may report duplicate/overlapping cycles** — LOW
The DFS-based cycle detection (lines 43-72) doesn't deduplicate. If A→B→C→A is a cycle, it may be reported starting from A and again starting from B depending on traversal order. The `visited` set prevents re-traversal of the *same* start node, but cycles are detected when a back-edge is found to any node in `rec_stack`, and the same cycle can be "found" from different entry points.

**Recommendation:** Normalize cycles (e.g., rotate to start with the lexicographically smallest ID) and deduplicate.

#### 2. **`check_orphans` doesn't check `blocks`↔`blocked_by` consistency** — MEDIUM
As noted in the `graph.rs` review, `blocks` and `blocked_by` can be inconsistent. `check_orphans` validates that referenced nodes *exist* but doesn't verify that the reverse edge is present. This is the natural place to add such a check.

**Recommendation:** Add a `check_consistency` function that verifies: if A.blocks contains B, then B.blocked_by contains A (and vice versa). Include it in `check_all`.

#### 3. **`OrphanRef.relation` is a String, not an enum** — LOW
The `relation` field uses string literals ("blocked_by", "blocks", "assigned", "requires"). An enum would provide type safety and exhaustiveness checking.

#### 4. **Cycle detection only follows `blocked_by`, not `blocks`** — LOW
The `find_cycles` function only traverses `blocked_by` edges. Since `blocks` is the reverse of `blocked_by`, cycles should be detectable from either direction. However, if `blocks` and `blocked_by` are out of sync (see issue above), a cycle could exist in `blocks` that isn't in `blocked_by`. This is another argument for either removing `blocks` or enforcing consistency.

#### 5. **No severity levels for check results** — LOW
All issues are treated equally. Some orphan references might be benign (e.g., an actor that was removed) while cycles are always errors. A severity level would help users prioritize fixes.

### Test coverage

Good coverage (lines 136-332): empty graph, linear chains, 2-node and 3-node cycles, valid references, orphan blocked_by, orphan assigned, combined `check_all`. 10 test functions.

---

## Cross-Cutting Concerns

### 1. `make_task` helper duplication (4 copies)

The identical `make_task` test helper is defined in `graph.rs:469`, `parser.rs:148`, `query.rs:301`, and `check.rs:141`. Each must be updated when a field is added to `Task`.

**Recommendation:** Add `impl Default for Task` or a `Task::test_fixture(id, title)` method behind `#[cfg(test)]` in `graph.rs`, then reuse it in other test modules. Alternatively, create a `test_helpers.rs` module.

### 2. The `blocks` field is a liability

Across all four modules, `blocks` is never used for scheduling logic. Only `blocked_by` drives `ready_tasks`, `cost_of`, and cycle detection. `blocks` is only used in:
- Orphan checking (`check.rs:89-96`)
- Visualization (`commands/graph.rs`, `commands/viz.rs`)
- Display (`commands/show.rs`)

It's a denormalized reverse index that's not maintained. This is the single biggest architectural issue in the core engine.

**Strong recommendation:** Remove `blocks` from the `Task` struct and compute it on demand:
```rust
impl WorkGraph {
    pub fn blocks(&self, task_id: &str) -> Vec<&str> {
        self.tasks()
            .filter(|t| t.blocked_by.contains(&task_id.to_string()))
            .map(|t| t.id.as_str())
            .collect()
    }
}
```

### 3. No typed IDs

Task, Actor, and Resource IDs are all `String`. This means you can accidentally pass an Actor ID where a Task ID is expected, and the compiler won't catch it. Newtype wrappers (`TaskId(String)`, `ActorId(String)`) would add type safety at minimal cost. This is a low-priority improvement but would prevent a class of bugs.

### 4. Dependency chain depth isn't bounded

`cost_of_recursive` handles cycles via a `visited` set, but there's no depth limit for acyclic but very deep chains. For pathological graphs (1000+ deep dependency chains), this could stack overflow. In practice this is unlikely.

---

## Summary of Recommendations by Priority

### High Priority
1. **Remove or sync `blocks` field** — The unsynchronized dual representation is a data integrity hazard. Removing `blocks` and computing it on demand is the cleanest fix.
2. **Add `Task::new(id, title)` constructor** — Eliminates 4 duplicated test helpers and makes the 28-field struct manageable.

### Medium Priority
3. **Atomic file writes in `save_graph`** — Write to temp file + rename to prevent data loss on crash.
4. **Add `blocks`↔`blocked_by` consistency check** — If `blocks` is kept, `check.rs` should validate bidirectional consistency.
5. **Eliminate `TaskHelper` duplication** — Use `serde_json::Value` pre-processing or remove legacy migration if no longer needed.
6. **Cache `ready_tasks` results** — Prevent redundant computation when multiple queries are composed.

### Low Priority
7. **Deterministic node ordering in `save_graph`** — Sort by ID for cleaner git diffs.
8. **Add `failed`/`abandoned` counts to `ProjectSummary`** — Task counts should add up.
9. **Typed timestamps** — `Option<DateTime<Utc>>` instead of `Option<String>`.
10. **Deduplicate cycle reports** — Normalize and deduplicate detected cycles.
11. **Enum for `OrphanRef.relation`** — Type safety over string literals.
12. **Newtype IDs** — `TaskId`, `ActorId`, `ResourceId` for compile-time safety.
