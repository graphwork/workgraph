# Cycle Detection Integration: Implementation Specification

**Date:** 2026-02-21
**Status:** Ready for implementation
**Depends on:** [Cycle-Aware Graph Design](cycle-aware-graph.md), `src/cycle.rs` (implemented, 53 tests)
**Edge rename status:** DONE (`blocked_by` → `after`, `blocks` → `before`)

---

## Overview

This spec details four phases of work to integrate cycle detection into workgraph's runtime. Each phase is an independent implementation task. The design doc (`docs/design/cycle-aware-graph.md`) provides the rationale; this spec provides the file-by-file implementation plan.

**What exists:**
- `src/cycle.rs`: Tarjan SCC, Havlak loop nesting, incremental detection, metadata extraction (1030 lines, 53 tests)
- `src/graph.rs`: `LoopEdge`, `LoopGuard`, `evaluate_loop_edges()`, `find_intermediate_tasks()`
- `src/query.rs`: `ready_tasks()`, `ready_tasks_with_peers()`
- `src/check.rs`: `check_cycles()` (DFS-based), `check_loop_edges()`

**What needs to happen:**
1. Wire `src/cycle.rs` algorithms into `WorkGraph` as cached `CycleAnalysis`
2. Add `CycleConfig` to `Task`, modify dispatch for back-edge exemption
3. Migrate `loops_to` edges to structural cycles
4. Remove `loops_to` entirely

---

## Phase 1: Add CycleAnalysis to WorkGraph (non-breaking)

### Goal
Add `CycleAnalysis` as a cached, lazily-computed field on `WorkGraph`. Add `wg cycles` command. Integrate into `wg check`. **No behavioral changes** — `loops_to` continues to work exactly as before.

### Data Structures

Add to `src/graph.rs` (or a new integration section):

```rust
/// Cached cycle analysis derived from `after` edges. Never serialized.
/// Recomputed lazily on structural mutations.
#[derive(Debug, Clone)]
pub struct CycleAnalysis {
    /// Non-trivial SCCs (cycles)
    pub cycles: Vec<CycleInfo>,
    /// Which cycle each task belongs to (task_id → index into cycles)
    pub task_to_cycle: HashMap<String, usize>,
    /// Back-edges: (predecessor_id, header_id) pairs within cycles
    pub back_edges: HashSet<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct CycleInfo {
    /// All task IDs in this cycle's SCC
    pub members: Vec<String>,
    /// The entry point / loop header task ID
    pub header: String,
    /// Is this a reducible cycle (single entry point)?
    pub reducible: bool,
}
```

### File-by-File Changes

#### `src/graph.rs` — LOGIC WORK

**Change 1: Add `cycle_analysis` field to `WorkGraph`**
- Location: `WorkGraph` struct (line ~433)
- Add: `cycle_analysis: Option<CycleAnalysis>` (not serialized — derived data)
- Impact: `WorkGraph::new()` initializes to `None`
- Must also update `Default` impl

**Change 2: Add cache invalidation**
- Location: `add_node()` (line ~445), `remove_node()` (line ~530)
- Add: `self.cycle_analysis = None;` at the start of each method
- Note: The `after` field mutations happen outside `WorkGraph` (callers mutate `Task` directly via `get_task_mut()`), so we need either:
  - (a) A dedicated `invalidate_cycle_cache()` pub method that callers use, OR
  - (b) Always recompute on access (since `cycle_analysis()` is lazy, this is fine)
- **Decision:** Option (b) — always recompute on access. `add_node` and `remove_node` invalidate; for `after`-field changes, the cache is invalidated by the next `add_node`/`remove_node` or explicitly. In practice, commands reload the graph from disk each time, so the cache is always empty on load. This is sufficient for Phase 1 (read-only diagnostic).

**Change 3: Add `cycle_analysis()` accessor**
```rust
impl WorkGraph {
    /// Invalidate cached cycle analysis. Called by structural mutations.
    pub fn invalidate_cycle_cache(&mut self) {
        self.cycle_analysis = None;
    }

    /// Get or compute cycle analysis from `after` edges.
    pub fn get_cycle_analysis(&mut self) -> &CycleAnalysis {
        if self.cycle_analysis.is_none() {
            self.cycle_analysis = Some(compute_cycle_analysis(self));
        }
        self.cycle_analysis.as_ref().unwrap()
    }
}
```

**Change 4: Add `compute_cycle_analysis()` function**
- Bridge between `WorkGraph` (string IDs) and `src/cycle.rs` (numeric IDs)
- Uses `NamedGraph` from `src/cycle.rs` to build adjacency list from `after` edges
- Calls `analyze_graph_cycles()` from `src/cycle.rs`
- Maps `CycleMetadata` (numeric) back to `CycleAnalysis` (string IDs)
- ~50 lines of glue code

```rust
fn compute_cycle_analysis(graph: &WorkGraph) -> CycleAnalysis {
    use crate::cycle::{NamedGraph, analyze_graph_cycles};

    let mut named = NamedGraph::new();

    // Add all tasks as nodes
    for task in graph.tasks() {
        named.add_node(&task.id);
    }

    // Add edges: if task A has after: [B], edge is B → A
    // (B must complete before A; B is a predecessor of A)
    for task in graph.tasks() {
        for pred_id in &task.after {
            if graph.get_task(pred_id).is_some() {
                named.add_edge(pred_id, &task.id);
            }
        }
    }

    let metadata = named.analyze_cycles();

    let mut cycles = Vec::new();
    let mut task_to_cycle = HashMap::new();
    let mut back_edges = HashSet::new();

    for (idx, cm) in metadata.iter().enumerate() {
        let members: Vec<String> = cm.members.iter()
            .map(|&id| named.get_name(id).to_string())
            .collect();
        let header = named.get_name(cm.header).to_string();

        for &(pred, hdr) in &cm.back_edges {
            back_edges.insert((
                named.get_name(pred).to_string(),
                named.get_name(hdr).to_string(),
            ));
        }

        for member in &members {
            task_to_cycle.insert(member.clone(), idx);
        }

        cycles.push(CycleInfo {
            members,
            header,
            reducible: cm.reducible,
        });
    }

    CycleAnalysis { cycles, task_to_cycle, back_edges }
}
```

#### `src/check.rs` — LOGIC WORK

**Change 1: Enhance `check_cycles()` to use Tarjan SCC**
- Current implementation (line ~61): recursive DFS cycle detection — works but doesn't identify SCCs, headers, or reducibility
- Replace with call to `compute_cycle_analysis()` (or just `find_cycles` from `src/cycle.rs`)
- Return richer data: cycle members, header, reducibility
- The existing `CheckResult.cycles: Vec<Vec<String>>` format can remain for backward compat, but add new fields

**Change 2: Add irreducible cycle warning**
- In `check_all()` (line ~257): after computing cycle analysis, check for irreducible cycles
- Add new issue type to `CheckResult`: `irreducible_cycles: Vec<IrreducibleCycleWarning>`
  ```rust
  pub struct IrreducibleCycleWarning {
      pub members: Vec<String>,
      pub entry_points: Vec<String>,
  }
  ```
- Irreducible cycles make `ok = false` (they will deadlock without manual intervention)

**Change 3: Add unconfigured cycle warning (prep for Phase 2)**
- For now, just detect cycles in `after` edges and report them as info
- In Phase 2, this becomes: "cycle exists but no `CycleConfig` → warn: will deadlock"

#### `src/commands/check.rs` — MECHANICAL

- Update display to show enhanced cycle info (header, reducibility, members)
- Add display for irreducible cycle warnings
- ~20 lines of formatting changes

#### `src/commands/loops.rs` — LOGIC WORK (EXTEND)

**Rename/extend to serve as the `wg cycles` command**

- Option A: Add a new `src/commands/cycles.rs` file
- Option B: Extend `loops.rs` to show both loop edges AND structural cycles
- **Decision:** Option A — create `src/commands/cycles.rs`. Keep `loops.rs` as-is for backward compat.

#### NEW: `src/commands/cycles.rs` — NEW FILE (~150 lines)

```
wg cycles [--json]

Output:
  Detected cycles: 2

  1. write → review → write  [REDUCIBLE]
     Header: write
     Members: write, review
     Back-edges: review → write

  2. spec → impl → test → spec  [REDUCIBLE]
     Header: spec
     Members: spec, impl, test
     Back-edges: test → spec

  Irreducible cycles: 0
```

Implementation:
- Load graph
- Call `graph.get_cycle_analysis()` (note: needs `&mut` — see below)
- Format and display cycles with metadata
- JSON output with full metadata

**Mutability note:** `get_cycle_analysis()` takes `&mut self` because it may compute and cache. For commands that only need read access to cycle analysis, we have two options:
1. Compute cycle analysis separately (call `compute_cycle_analysis(&graph)` as a free function)
2. Use interior mutability (`RefCell` or `OnceCell`)
**Decision:** Make `compute_cycle_analysis()` a public free function. Commands can call it directly without needing `&mut WorkGraph`. The cached version on `WorkGraph` is for the coordinator (long-lived process).

#### `src/main.rs` — MECHANICAL

- Add `Commands::Cycles` variant (line ~403, near `Commands::Loops`)
- Add routing in match (line ~2321, near `Commands::Loops`)
- Add to command category for help display
- ~5 lines

#### `src/lib.rs` — MECHANICAL

- Ensure `pub mod cycle;` is present (it should be already)
- Add `pub use graph::{CycleAnalysis, CycleInfo};` if desired

### Tests

#### `src/graph.rs` — Unit tests for `compute_cycle_analysis()`
- `test_cycle_analysis_empty_graph` — no cycles
- `test_cycle_analysis_linear_chain` — no cycles
- `test_cycle_analysis_simple_two_node_cycle` — A after B, B after A
- `test_cycle_analysis_three_node_cycle_with_external_entry` — header identification
- `test_cycle_analysis_isolated_cycle` — picks smallest ID as header
- `test_cycle_analysis_irreducible` — multiple entry points
- `test_cycle_analysis_cache_invalidation` — add_node clears cache

#### `tests/integration_cycles.rs` — NEW integration test file
- `test_wg_cycles_shows_detected_cycles` — CLI output
- `test_wg_cycles_json_output` — JSON format
- `test_wg_check_warns_on_irreducible_cycles` — check integration

### Summary: Phase 1 File Changes

| File | Type | Lines | Description |
|------|------|-------|-------------|
| `src/graph.rs` | Logic | ~80 | CycleAnalysis struct, compute fn, cache on WorkGraph |
| `src/check.rs` | Logic | ~40 | Enhanced check_cycles, irreducible warning |
| `src/commands/check.rs` | Mechanical | ~20 | Display enhanced cycle info |
| `src/commands/cycles.rs` | **New file** | ~150 | `wg cycles` command |
| `src/main.rs` | Mechanical | ~10 | Add Cycles command variant + routing |
| `tests/integration_cycles.rs` | **New file** | ~100 | Integration tests |
| **Total** | | **~400** | |

---

## Phase 2: CycleConfig and Cycle-Aware Dispatch

### Goal
Add `CycleConfig` to `Task`. Modify `ready_tasks()` for back-edge exemption. Add `evaluate_cycle_iteration()`. New CLI flags. Both `loops_to` and structural cycles work in parallel.

### Data Structures

Add to `src/graph.rs`:

```rust
/// Configuration for structural cycle iteration.
/// Only present on the cycle header task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CycleConfig {
    /// Hard cap on cycle iterations
    pub max_iterations: u32,
    /// Condition that must be true to iterate (None = always, up to max_iterations)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<LoopGuard>,
    /// Time delay before re-activation (e.g., "30s", "5m", "1h")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<String>,
}
```

### File-by-File Changes

#### `src/graph.rs` — LOGIC WORK

**Change 1: Add `CycleConfig` struct** (see above, ~15 lines)

**Change 2: Add `cycle_config` field to `Task`**
- Location: `Task` struct (line ~148), after `loop_iteration`
- Add: `#[serde(default, skip_serializing_if = "Option::is_none")] pub cycle_config: Option<CycleConfig>`
- Also add to `TaskHelper` for deserialization (line ~255)
- Also add to `Task::deserialize()` impl (line ~321)

**Change 3: Add `evaluate_cycle_iteration()` function**
- Alongside existing `evaluate_loop_edges()` (line ~582)
- ~80 lines (see design doc §9.3 for implementation sketch)
- Logic:
  1. Check if completed task is in a cycle (`task_to_cycle` lookup)
  2. Check if ALL cycle members are Done
  3. Check convergence tag on header
  4. Check `cycle_config` on header (max_iterations, guard, delay)
  5. If all pass: re-open all cycle members, increment iteration
- **Does NOT call `find_intermediate_tasks()`** — cycle membership replaces BFS

**Change 4: Add `evaluate_guard()` reuse**
- The existing `evaluate_guard()` (line ~557) already works for `LoopGuard`
- `CycleConfig` reuses the same `LoopGuard` enum, so no change needed

#### `src/query.rs` — LOGIC WORK

**Change 1: Add `ready_tasks_cycle_aware()` function**
- ~30 lines
- Same logic as `ready_tasks()` but accepts `&CycleAnalysis`
- Back-edge exemption: if a task is a cycle header and a predecessor is a back-edge source within the same cycle, that predecessor is exempt from the "must be terminal" check
- BUT only if the header has a `CycleConfig` (unconfigured cycles deadlock intentionally)

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
            // Normal check: predecessor is terminal
            if graph.get_task(pred_id)
                .map(|t| t.status.is_terminal())
                .unwrap_or(true)
            {
                return true;
            }

            // Cycle-aware: back-edge exemption for configured cycle headers
            if analysis.back_edges.contains(&(pred_id.clone(), task.id.clone())) {
                // Only exempt if the header has a CycleConfig
                if task.cycle_config.is_some() {
                    return true;
                }
            }

            false
        })
    }).collect()
}
```

**Change 2: Add `ready_tasks_with_peers_cycle_aware()` function**
- Same pattern as `ready_tasks_with_peers()` (line ~305) but with back-edge exemption
- ~30 lines
- Alternatively: refactor both `ready_tasks` variants to use a shared helper that optionally accepts `CycleAnalysis`

**Refactoring approach (recommended):**
```rust
/// Shared readiness check logic. cycle_analysis is optional.
fn is_task_ready(
    task: &Task,
    graph: &WorkGraph,
    workgraph_dir: Option<&Path>,
    cycle_analysis: Option<&CycleAnalysis>,
) -> bool {
    if task.status != Status::Open { return false; }
    if task.paused { return false; }
    if !is_time_ready(task) { return false; }

    task.after.iter().all(|pred_id| {
        let satisfied = match workgraph_dir {
            Some(dir) => is_predecessor_satisfied(pred_id, graph, Some(dir)),
            None => graph.get_task(pred_id)
                .map(|t| t.status.is_terminal())
                .unwrap_or(true),
        };
        if satisfied { return true; }

        // Back-edge exemption
        if let Some(analysis) = cycle_analysis {
            if analysis.back_edges.contains(&(pred_id.clone(), task.id.clone()))
                && task.cycle_config.is_some()
            {
                return true;
            }
        }

        false
    })
}
```

Then `ready_tasks()`, `ready_tasks_with_peers()`, and the cycle-aware variants all call this helper. This minimizes code duplication. The existing non-cycle-aware functions pass `cycle_analysis: None`.

#### `src/commands/done.rs` — LOGIC WORK

**Change 1: Call `evaluate_cycle_iteration()` after `evaluate_loop_edges()`**
- Location: line ~63 (after `evaluate_loop_edges`)
- Add: compute cycle analysis, then call `evaluate_cycle_iteration()`
- Both functions can fire on the same completion — `evaluate_loop_edges` handles `loops_to`, `evaluate_cycle_iteration` handles structural cycles
- They don't conflict because structural cycles use `after` edges (not `loops_to`)

```rust
// Existing: evaluate loop edges
let reactivated = evaluate_loop_edges(&mut graph, &id_owned);

// New: evaluate structural cycle iteration
let cycle_analysis = compute_cycle_analysis(&graph);
let cycle_reactivated = evaluate_cycle_iteration(&mut graph, &id_owned, &cycle_analysis);
```

- Print cycle reactivations alongside loop reactivations

#### `src/commands/service.rs` — LOGIC WORK

**Change 1: Use cycle-aware ready_tasks in coordinator**
- Location: lines ~383, ~418, ~907, ~1022 (every call to `ready_tasks_with_peers`)
- Compute cycle analysis once per tick, pass to `ready_tasks_with_peers_cycle_aware()`
- Or: use the refactored `is_task_ready()` helper

**Change 2: Call `evaluate_cycle_iteration()` alongside `evaluate_loop_edges()`**
- Location: line ~1163 (in dead agent triage)
- After `evaluate_loop_edges`, also call `evaluate_cycle_iteration()`

#### `src/commands/add.rs` — LOGIC WORK

**Change 1: Add `--max-iterations`, `--cycle-guard`, `--cycle-delay` parameters**
- Add parameters to `run()` function signature (line ~45)
- Build `CycleConfig` if `--max-iterations` is provided
- Set on the new task

```rust
let cycle_config = max_iterations.map(|max| CycleConfig {
    max_iterations: max,
    guard: cycle_guard.map(|g| parse_guard_expr(g)).transpose()?,
    delay: cycle_delay.map(|d| {
        // Validate delay format
        parse_delay(d).ok_or_else(|| anyhow::anyhow!("Invalid delay: {}", d))?;
        Ok::<_, anyhow::Error>(d.to_string())
    }).transpose()?,
});
```

**Change 2: `parse_guard_expr()` is reused as-is** (already parses `LoopGuard`)

#### `src/commands/edit.rs` — LOGIC WORK

**Change 1: Add `--max-iterations`, `--cycle-guard`, `--cycle-delay` edit parameters**
- Add to `run()` function signature (line ~12)
- Modify task's `cycle_config` field
- ~30 lines

#### `src/main.rs` — MECHANICAL

**Change 1: Add new CLI flags to `Commands::Add`**
```rust
/// Maximum iterations for structural cycle (sets cycle_config on this task)
#[arg(long = "max-iterations")]
max_iterations: Option<u32>,

/// Guard condition for cycle iteration: 'task:<id>=<status>' or 'always'
#[arg(long = "cycle-guard")]
cycle_guard: Option<String>,

/// Delay between cycle iterations (e.g., 30s, 5m, 1h)
#[arg(long = "cycle-delay")]
cycle_delay: Option<String>,
```

**Change 2: Add new CLI flags to `Commands::Edit`**
```rust
/// Set maximum iterations for structural cycle
#[arg(long = "max-iterations")]
max_iterations: Option<u32>,

/// Set guard condition for cycle iteration
#[arg(long = "cycle-guard")]
cycle_guard: Option<String>,

/// Set delay between cycle iterations
#[arg(long = "cycle-delay")]
cycle_delay: Option<String>,
```

**Change 3: Wire new parameters through to `commands::add::run()` and `commands::edit::run()`**
- ~10 lines of plumbing in the match arm

#### `src/check.rs` — LOGIC WORK

**Change 1: Add unconfigured cycle check**
- After computing cycle analysis, check: for each cycle, does the header have `CycleConfig`?
- If not → warning: "Cycle detected but no max_iterations configured — cycle will deadlock"
- Add to `CheckResult`:
  ```rust
  pub unconfigured_cycles: Vec<UnconfiguredCycleWarning>,
  ```
- ~20 lines

**Change 2: Add conflicting config check**
- If multiple tasks in the same cycle have `CycleConfig` → error
- "Only the cycle header should have cycle_config"
- ~15 lines

#### `src/service/executor.rs` — MECHANICAL

- Location: `task_loop_info()` (lines ~50-63)
- Add `cycle_config` info to the prompt context sent to spawned agents
- ~10 lines

#### `src/trace_function.rs` — MECHANICAL

- Add `cycle_config` field to `TaskTemplate` struct (line ~105)
- ~5 lines

#### `src/commands/trace_instantiate.rs` — MECHANICAL

- Handle `cycle_config` in template instantiation
- ~10 lines

#### `src/commands/show.rs` — MECHANICAL

- Display `cycle_config` in task detail output alongside existing loop info
- ~15 lines

### Tests

#### `src/graph.rs` — Unit tests
- `test_evaluate_cycle_iteration_all_done_iterates`
- `test_evaluate_cycle_iteration_partial_done_no_iterate`
- `test_evaluate_cycle_iteration_max_iterations_stops`
- `test_evaluate_cycle_iteration_converged_stops`
- `test_evaluate_cycle_iteration_guard_prevents`
- `test_evaluate_cycle_iteration_delay_applied`
- `test_evaluate_cycle_iteration_no_config_no_iterate`

#### `src/query.rs` — Unit tests
- `test_ready_tasks_cycle_aware_header_ready_first_iteration`
- `test_ready_tasks_cycle_aware_non_header_waits`
- `test_ready_tasks_cycle_aware_external_deps_block_header`
- `test_ready_tasks_cycle_aware_no_config_no_exemption`

#### `tests/integration_cycles.rs` — Extend
- `test_cycle_iteration_via_wg_done`
- `test_cycle_converged_via_wg_done`
- `test_cycle_max_iterations_stops`
- `test_wg_add_with_max_iterations`
- `test_wg_edit_cycle_config`

### Summary: Phase 2 File Changes

| File | Type | Lines | Description |
|------|------|-------|-------------|
| `src/graph.rs` | Logic | ~120 | CycleConfig struct, evaluate_cycle_iteration(), cycle_config field |
| `src/query.rs` | Logic | ~60 | Cycle-aware ready_tasks, shared helper refactor |
| `src/check.rs` | Logic | ~35 | Unconfigured/conflicting cycle checks |
| `src/commands/done.rs` | Logic | ~15 | Call evaluate_cycle_iteration |
| `src/commands/service.rs` | Logic | ~30 | Cycle-aware dispatch, cycle iteration in triage |
| `src/commands/add.rs` | Logic | ~25 | --max-iterations, --cycle-guard, --cycle-delay |
| `src/commands/edit.rs` | Logic | ~30 | Cycle config editing |
| `src/main.rs` | Mechanical | ~25 | New CLI flags + wiring |
| `src/service/executor.rs` | Mechanical | ~10 | Cycle info in agent prompt |
| `src/trace_function.rs` | Mechanical | ~5 | cycle_config in templates |
| `src/commands/trace_instantiate.rs` | Mechanical | ~10 | Template instantiation |
| `src/commands/show.rs` | Mechanical | ~15 | Display cycle_config |
| Tests (unit + integration) | | ~200 | See test plan |
| **Total** | | **~580** | |

---

## Phase 3: Migrate loops_to to Structural Cycles

### Goal
Add `wg migrate-loops` command. Add serde migration for automatic conversion on deserialization. Deprecation warnings on `--loops-to`. Both models still work.

### Migration Logic

For each task with `loops_to` edges:
```
For each LoopEdge { target, guard, max_iterations, delay }:
  1. Add source_task.id to target_task.after
     (creates the back-edge: target is "after" source, meaning
      target depends on source completing before re-running)
  2. Set target_task.cycle_config = CycleConfig {
         max_iterations,
         guard,
         delay,
     }
  3. Remove the LoopEdge from source_task.loops_to
```

**Edge direction note:** In the current `loops_to` model, `source.loops_to = [target]` means "when source completes, re-open target." In the structural cycle model, this becomes `target.after = [source]` — target is after source in the cycle. The cycle analysis then detects the back-edge (since target was previously a predecessor of source via the dependency chain).

Wait — this needs careful thought. Consider the current pattern:

```
A (target) → B → C (source, has loops_to: [A])
```

Where `→` means "is after". So B is after A, C is after B. C loops_to A.

To make this a structural cycle: A must be after C. So we add C to A's `after` list:
```
A.after = [C]  (new back-edge)
B.after = [A]  (existing)
C.after = [B]  (existing)
```

Now A → B → C → A is a cycle. A is the header (entered from outside or has external deps).

So the migration is:
```
For each LoopEdge on source_task with target=target_id:
  1. Add source_task.id to target_task.after  (target is after source)
  2. Set target_task.cycle_config = CycleConfig { ... }
  3. Remove the LoopEdge
```

### File-by-File Changes

#### NEW: `src/commands/migrate_loops.rs` — LOGIC WORK (~120 lines)

```rust
pub fn run(dir: &Path, dry_run: bool) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let mut migrations = Vec::new();

    // Collect all migrations first (can't mutate while iterating)
    for task in graph.tasks() {
        for edge in &task.loops_to {
            migrations.push(LoopMigration {
                source_id: task.id.clone(),
                target_id: edge.target.clone(),
                max_iterations: edge.max_iterations,
                guard: edge.guard.clone(),
                delay: edge.delay.clone(),
            });
        }
    }

    if migrations.is_empty() {
        println!("No loops_to edges to migrate.");
        return Ok(());
    }

    println!("Found {} loops_to edge(s) to migrate:", migrations.len());
    for m in &migrations {
        println!("  {} --loops_to--> {} (max: {}, guard: {:?}, delay: {:?})",
            m.source_id, m.target_id, m.max_iterations,
            m.guard, m.delay);
    }

    if dry_run {
        println!("\nDry run — no changes made.");
        return Ok(());
    }

    // Apply migrations
    for m in &migrations {
        // 1. Add source to target's after list (creates back-edge)
        if let Some(target) = graph.get_task_mut(&m.target_id) {
            if !target.after.contains(&m.source_id) {
                target.after.push(m.source_id.clone());
            }
            // 2. Set cycle_config on target (the cycle header)
            if target.cycle_config.is_none() {
                target.cycle_config = Some(CycleConfig {
                    max_iterations: m.max_iterations,
                    guard: m.guard.clone(),
                    delay: m.delay.clone(),
                });
            }
        }
        // 3. Remove the LoopEdge from source
        if let Some(source) = graph.get_task_mut(&m.source_id) {
            source.loops_to.retain(|e| e.target != m.target_id);
        }
    }

    save_graph(&graph, &path)?;
    println!("\nMigrated {} loops_to edge(s) to structural cycles.", migrations.len());
    Ok(())
}
```

#### `src/graph.rs` — LOGIC WORK

**Change 1: Serde migration in `Task::deserialize()`**
- Location: `TaskHelper` / `Task::deserialize()` (line ~321)
- On deserialization, if `loops_to` is non-empty and task has no `cycle_config`:
  - Emit a deprecation warning to stderr
  - Do NOT auto-convert (that would require modifying the target task, which isn't available during single-task deserialization)
  - The `wg migrate-loops` command handles the full graph migration

Actually, auto-conversion during deserialization is limited because we only see one task at a time. The migration needs the full graph. So the serde layer just emits warnings:

```rust
// In Task::deserialize():
if !helper.loops_to.is_empty() && helper.cycle_config.is_none() {
    eprintln!(
        "Warning: task '{}' uses deprecated loops_to edges. \
         Run 'wg migrate-loops' to convert to structural cycles.",
        helper.id
    );
}
```

#### `src/commands/add.rs` — MECHANICAL

**Change 1: Deprecation warning on `--loops-to`**
- Location: after building the LoopEdge (~line 130)
- Add: `eprintln!("Warning: --loops-to is deprecated. Use --after with --max-iterations instead.");`
- ~3 lines

#### `src/commands/edit.rs` — MECHANICAL

**Change 1: Deprecation warning on `--add-loops-to`**
- Similar to add.rs
- ~3 lines

#### `src/main.rs` — MECHANICAL

**Change 1: Add `Commands::MigrateLoops` variant**
```rust
/// Migrate loops_to edges to structural cycles (after edges + cycle_config)
MigrateLoops {
    /// Show what would be migrated without making changes
    #[arg(long)]
    dry_run: bool,
},
```

**Change 2: Wire routing**
- ~5 lines

### Tests

#### `tests/integration_migrate_loops.rs` — NEW file
- `test_migrate_simple_loop` — single loops_to converts correctly
- `test_migrate_preserves_max_iterations` — value preserved
- `test_migrate_preserves_guard` — guard preserved
- `test_migrate_preserves_delay` — delay preserved
- `test_migrate_dry_run_no_changes` — dry run doesn't modify graph
- `test_migrate_no_loops_noop` — empty graph is fine
- `test_migrate_idempotent` — running twice is safe

### Summary: Phase 3 File Changes

| File | Type | Lines | Description |
|------|------|-------|-------------|
| `src/commands/migrate_loops.rs` | **New file** | ~120 | Migration command |
| `src/graph.rs` | Logic | ~10 | Deprecation warning in deserialize |
| `src/commands/add.rs` | Mechanical | ~3 | Deprecation warning |
| `src/commands/edit.rs` | Mechanical | ~3 | Deprecation warning |
| `src/main.rs` | Mechanical | ~10 | MigrateLoops command + routing |
| `tests/integration_migrate_loops.rs` | **New file** | ~150 | Migration tests |
| **Total** | | **~296** | |

---

## Phase 4: Remove loops_to

### Goal
Remove `LoopEdge`, `loops_to` field, `evaluate_loop_edges()`, and all `--loops-to`/`--loop-max`/`--loop-guard`/`--loop-delay` CLI flags. Merge `wg loops` into `wg cycles`. Clean up `find_intermediate_tasks()`.

### File-by-File Changes

#### `src/graph.rs` — LOGIC WORK (mostly deletion)

**Removals:**
1. **`LoopEdge` struct** (lines 9-21) — DELETE entirely
2. **`LoopGuard` enum** (lines 24-32) — KEEP (reused by `CycleConfig`)
3. **`loops_to` field** on `Task` (line 218) — DELETE field
   - Keep `#[serde(default)] loops_to: Vec<serde_json::Value>` in `TaskHelper` for backward compat deserialization (silently ignore old data)
4. **`loop_iteration` field** on `Task` (line 221) — KEEP (still used by cycle iteration)
5. **`evaluate_loop_edges()` function** (lines 582-746) — DELETE entirely (~165 lines)
6. **`evaluate_guard()` function** (lines 557-568) — KEEP (used by `evaluate_cycle_iteration()`)
7. **`find_intermediate_tasks()` function** (lines 758-822) — DELETE (~65 lines)
   - Cycle membership from `CycleAnalysis` replaces BFS
8. **`remove_node()` cleanup** (line 538): Remove `task.loops_to.retain(|edge| edge.target != id);`
9. **`parse_delay()` function** (lines 36-54) — KEEP (used by `CycleConfig.delay`)

**Net deletion:** ~250 lines

#### `src/check.rs` — LOGIC WORK (mostly deletion)

**Removals:**
1. **`LoopEdgeIssue` struct** (lines 40-45) — DELETE
2. **`LoopEdgeIssueKind` enum** (lines 48-58) — DELETE
3. **`check_loop_edges()` function** (lines 114-169) — DELETE entirely
4. **`loop_edge_issues` field** on `CheckResult` (line 10) — DELETE
5. **In `check_all()`** (line 260): Remove `check_loop_edges()` call
6. **`ok` computation** (line 266): Remove `loop_edge_issues.is_empty()` from condition

**Replacements:**
- The cycle-related checks from Phase 2 (unconfigured cycles, conflicting configs, irreducible cycles) now handle all validation
- The `ok` condition becomes: `orphan_refs.is_empty() && irreducible_cycles.is_empty() && conflicting_configs.is_empty()`

**Net deletion:** ~70 lines of code, ~150 lines of tests

#### `src/commands/loops.rs` — DELETE or REDIRECT

**Option A: Delete entirely, make `wg loops` an alias for `wg cycles`**
- Delete file (~285 lines)
- In `main.rs`, route `Commands::Loops` to `commands::cycles::run()`
- Print deprecation notice: "wg loops is deprecated, use wg cycles"

**Option B: Delete file, remove command**
- Cleaner but breaking change

**Decision:** Option A — alias with deprecation warning, then remove in a future release.

#### `src/commands/done.rs` — MECHANICAL

**Change 1: Remove `evaluate_loop_edges()` call** (line ~63)
- Only `evaluate_cycle_iteration()` remains
- ~3 lines changed

#### `src/commands/service.rs` — MECHANICAL

**Change 1: Remove `evaluate_loop_edges` import** (line ~35)
**Change 2: Remove `evaluate_loop_edges()` call** (line ~1163)
- Only `evaluate_cycle_iteration()` remains

#### `src/commands/add.rs` — LOGIC WORK

**Removals:**
1. **`parse_guard_expr()` function** (lines 9-42) — MOVE to a shared location (still needed by `--cycle-guard`), or keep in `add.rs` as it's still used
   - Actually: keep `parse_guard_expr()` — it parses `LoopGuard` which is now used by `CycleConfig`
2. **`loops_to` parameter** from `run()` signature (line ~61)
3. **`loop_max` parameter** from `run()` signature (line ~62)
4. **`loop_guard` parameter** from `run()` signature (line ~63)
5. **`loop_delay` parameter** from `run()` signature (line ~64)
6. **LoopEdge construction logic** — DELETE (~20 lines)
7. Remove `use workgraph::graph::{LoopEdge, parse_delay};` — keep `parse_delay` (still used by `--cycle-delay`)

#### `src/commands/edit.rs` — LOGIC WORK

**Removals:**
1. **`add_loops_to` parameter** (line ~24)
2. **`loop_max` parameter** (line ~25)
3. **`loop_guard` parameter** (line ~26)
4. **`loop_delay` parameter** (line ~27)
5. **`remove_loops_to` parameter** (line ~28)
6. **`loop_iteration` parameter** (line ~29)
7. **All loop edge editing logic** (~50 lines)
8. Remove `use workgraph::graph::LoopEdge;`

#### `src/main.rs` — MECHANICAL

**Removals:**
1. **`--loops-to` flag** from `Commands::Add` (lines 107-109)
2. **`--loop-max` flag** from `Commands::Add` (lines 111-113)
3. **`--loop-guard` flag** from `Commands::Add` (lines 115-117)
4. **`--loop-delay` flag** from `Commands::Add` (lines 119-121)
5. **`--add-loops-to` flag** from `Commands::Edit` (lines 169-171)
6. **`--loop-max` flag** from `Commands::Edit` (lines 173-175)
7. **`--loop-guard` flag** from `Commands::Edit` (lines 177-179)
8. **`--loop-delay` flag** from `Commands::Edit` (lines 181-183)
9. **`--remove-loops-to` flag** from `Commands::Edit` (lines 185-187)
10. **`--loop-iteration` flag** from `Commands::Edit` (lines 189-191)
11. **Routing for removed parameters** in the match arms (~10 lines)
12. Optionally: redirect `Commands::Loops` to `Commands::Cycles`

#### `src/service/executor.rs` — MECHANICAL

- Remove `task_loop_info()` function or update it to only report cycle info
- ~10 lines changed

#### `src/commands/show.rs` — MECHANICAL

- Remove `loops_to` display section (~20 lines)
- Cycle info display from Phase 2 replaces it

#### `src/commands/viz.rs` — MECHANICAL

- Remove loop edge rendering (loop arrows, loop labels)
- Replace with cycle membership visualization
- ~30 lines changed

#### `src/tui/mod.rs` — MECHANICAL

- Remove `CellStyle::LoopEdge`, `LoopEdgeArrow`, `LoopEdgeLabel` variants
- Remove loop edge rendering in graph layout
- ~30 lines

#### `src/commands/analyze.rs` — MECHANICAL

- Remove loop edge analysis metrics
- Replace with cycle metrics
- ~10 lines

#### `src/trace_function.rs` — LOGIC WORK

- Remove `LoopEdgeTemplate` struct
- Remove `loops_to` field from `TaskTemplate`
- Cycle config is already handled from Phase 2
- ~15 lines deleted

#### `src/commands/trace_instantiate.rs` — MECHANICAL

- Remove loop template instantiation logic
- ~15 lines deleted

#### `src/commands/trace_extract.rs` — MECHANICAL

- Remove loop edge extraction
- ~10 lines deleted

#### `src/commands/trace_function_cmd.rs` — MECHANICAL

- Update help text, remove loop documentation references
- ~10 lines

#### `src/commands/spawn.rs` — MECHANICAL

- Remove loop info from spawn output
- ~5 lines

#### `src/commands/replay.rs` — MECHANICAL

- Update to use cycle_config instead of loops_to in replay logic
- ~10 lines

#### Lower-impact files (mechanical, ~2-5 lines each):

| File | Change |
|------|--------|
| `src/commands/quickstart.rs` | Remove loops_to initialization in help text |
| `src/commands/agent.rs` | Remove loops_to init if present |
| `src/commands/critical_path.rs` | Remove loops_to from task creation |
| `src/commands/notify.rs` | Remove loops_to from task creation |

### Test Files

All test files that reference `loops_to`, `LoopEdge`, or `evaluate_loop_edges` need updates:

| Test File | References | Changes |
|-----------|-----------|---------|
| `tests/integration_loops.rs` | 106 refs | **MAJOR REWRITE**: Convert all loop tests to structural cycle tests |
| `tests/integration_loop_workflow.rs` | 59 refs | **MAJOR REWRITE**: Convert workflow tests |
| `tests/integration_error_paths.rs` | 56 refs | Update error tests for cycle_config validation |
| `tests/integration_trace_functions.rs` | 25 refs | Update template tests |
| `tests/integration_cross_repo_dispatch.rs` | 21 refs | Update dispatch tests |
| `tests/integration_service_coordinator.rs` | 14 refs | Update coordinator tests |
| `tests/integration_replay_exhaustive.rs` | 11 refs | Update replay tests |
| `tests/integration_check_context.rs` | 9 refs | Update check tests |
| `tests/integration_auto_assignment.rs` | 9 refs | Update ready_tasks tests |
| `tests/integration_cli_commands.rs` | 5 refs | Remove loop CLI flag tests |
| `src/graph.rs` (unit tests) | ~10 refs | Remove evaluate_loop_edges tests, find_intermediate_tasks tests |
| `src/check.rs` (unit tests) | ~15 refs | Remove check_loop_edges tests |

### Summary: Phase 4 File Changes

| File | Type | Lines Changed | Description |
|------|------|---------------|-------------|
| `src/graph.rs` | Deletion | -250 | Remove LoopEdge, evaluate_loop_edges, find_intermediate_tasks |
| `src/check.rs` | Deletion | -70 code, -150 tests | Remove check_loop_edges, LoopEdgeIssue |
| `src/commands/loops.rs` | Redirect | -280 (or redirect) | Alias to wg cycles |
| `src/commands/done.rs` | Mechanical | -5 | Remove evaluate_loop_edges call |
| `src/commands/service.rs` | Mechanical | -5 | Remove evaluate_loop_edges call |
| `src/commands/add.rs` | Logic | -25 | Remove loop parameters |
| `src/commands/edit.rs` | Logic | -50 | Remove loop editing |
| `src/main.rs` | Mechanical | -30 | Remove loop CLI flags |
| `src/service/executor.rs` | Mechanical | -10 | Remove loop info |
| `src/commands/show.rs` | Mechanical | -20 | Remove loop display |
| `src/commands/viz.rs` | Mechanical | -30 | Remove loop rendering |
| `src/tui/mod.rs` | Mechanical | -30 | Remove loop visualization |
| `src/commands/analyze.rs` | Mechanical | -10 | Remove loop metrics |
| `src/trace_function.rs` | Logic | -15 | Remove LoopEdgeTemplate |
| `src/commands/trace_instantiate.rs` | Mechanical | -15 | Remove loop instantiation |
| `src/commands/trace_extract.rs` | Mechanical | -10 | Remove loop extraction |
| `src/commands/trace_function_cmd.rs` | Mechanical | -10 | Remove loop help text |
| `src/commands/spawn.rs` | Mechanical | -5 | Remove loop info |
| `src/commands/replay.rs` | Mechanical | -10 | Update for cycle_config |
| Minor files (4) | Mechanical | -10 | Remove loops_to init |
| Tests (12 files) | Major | ~-500, +300 | Rewrite loop tests as cycle tests |
| **Total net deletion** | | **~-1000** | |

---

## Cross-Phase Dependencies

```
Phase 1 ──────→ Phase 2 ──────→ Phase 3 ──────→ Phase 4
(diagnostic)    (runtime)       (migration)      (cleanup)

Phase 1 is prerequisite for Phase 2 (CycleAnalysis must exist).
Phase 2 is prerequisite for Phase 3 (CycleConfig must exist).
Phase 3 is prerequisite for Phase 4 (migration must be available).
```

Phases 1 and 2 can be merged into a single implementation task if desired. Phases 3 and 4 should be separate — Phase 3 adds migration tooling, Phase 4 removes old code after a deprecation period.

## Risk Mitigation

1. **Phase 1 has zero behavioral risk** — purely additive, read-only diagnostics
2. **Phase 2 runs in parallel** — existing `loops_to` works alongside new structural cycles
3. **Phase 3 provides migration tooling** — users can migrate at their own pace
4. **Phase 4 is a cleanup** — only done after migration is confirmed working

At every phase, all existing tests must continue to pass.

## Appendix: Edge Direction Reference

The `after` field means "this task comes after (depends on) the listed tasks":
- `task.after = ["dep"]` means `dep` must complete before `task`
- In adjacency list terms: edge goes from `dep` → `task` (dep is a predecessor)
- In cycle analysis: if A.after=[C] and C.after=[B] and B.after=[A], the cycle is A→B→C→A
- Back-edge: the edge from C to A (since A is the header entered from outside)
