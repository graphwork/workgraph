# DAG Assumptions Survey

**Date:** 2026-02-14
**Scope:** Entire workgraph codebase and documentation
**Purpose:** Catalog every location where the code assumes the task graph is a DAG (directed acyclic graph), to inform future work on cycle support.

---

## Executive Summary

The workgraph codebase has a **mixed posture** toward cycles. Documentation explicitly states cycles are allowed ("Unlike traditional DAG-based task managers, Workgraph allows cycles for recurring tasks"), and several subsystems have been hardened with `visited` sets to tolerate cycles. However, the codebase retains significant DAG terminology (the `wg dag` command itself), uses the `ascii-dag` crate which requires acyclic input, and has several algorithms that would infinite-loop or produce incorrect results on cyclic graphs without their protective `visited` guards.

**Finding count:** 42 distinct locations across 8 categories.

---

## Category 1: Explicit "DAG" / "Acyclic" Mentions in Code & Docs

### 1.1 — `wg dag` command name
- **File:** `src/main.rs:419-428`
- **What:** CLI subcommand `Dag` with help text "Show ASCII DAG of the dependency graph"
- **Assumption:** Naming implies a DAG; the command itself works on cyclic graphs (delegates to `viz.rs` ASCII output)
- **Difficulty to change:** **Trivial** — rename to `wg graph` or `wg deps`, update help text. Could alias `dag` for backward compat.

### 1.2 — `wg dag` command routing
- **File:** `src/main.rs:1600-1607`
- **What:** `Commands::Dag` match arm routes to `commands::viz::run` with ASCII format
- **Difficulty to change:** **Trivial** — follows from renaming the command.

### 1.3 — README: "dependency DAG" references
- **File:** `README.md:427`
- **What:** "Graph Explorer — tree view of the dependency DAG with task status"
- **Difficulty to change:** **Trivial** — change "DAG" to "graph"

### 1.4 — README: `d` keybinding description
- **File:** `README.md:457`
- **What:** "Toggle between tree and DAG view"
- **Difficulty to change:** **Trivial** — rename to "graph view"

### 1.5 — README: `wg dag` command reference
- **File:** `README.md:609`
- **What:** `wg dag # ASCII dependency graph (--all to include done)`
- **Difficulty to change:** **Trivial** — follows from renaming.

### 1.6 — SKILL.md: `wg dag` references (3 occurrences)
- **File:** `.claude/skills/wg/SKILL.md:47,157-159`
- **What:** Documentation of `wg dag` command and its flags
- **Difficulty to change:** **Trivial** — follows from renaming.

### 1.7 — Cargo.toml: `ascii-dag` crate dependency
- **File:** `Cargo.toml:34`
- **What:** `ascii-dag = "0.8"` — crate name itself assumes DAG
- **Difficulty to change:** **N/A** — external crate name, cannot change. The code already works around this by stripping back-edges before passing to ascii-dag.

### 1.8 — docs/README.md: "typically a DAG"
- **File:** `docs/README.md:81`
- **What:** "Tasks form a directed graph through `blocked_by` relationships. While typically a DAG (directed acyclic graph), cycles are permitted for iterative/recurring work patterns."
- **Difficulty to change:** **N/A** — this is already the correct nuanced statement.

### 1.9 — docs/workgraph-analysis-research.md: DAG mention
- **File:** `docs/workgraph-analysis-research.md:24`
- **What:** "Cycles are allowed: Unlike traditional DAG-based task managers, Workgraph allows cycles for recurring tasks"
- **Difficulty to change:** **N/A** — already cycle-aware documentation.

---

## Category 2: Topological Sort Algorithms or Algorithms Assuming No Cycles

### 2.1 — TUI tree builder: topological sort comment
- **File:** `src/tui/app.rs:583-588`
- **What:** Comment says "Strategy: topological sort, then render tasks as an indented tree." The actual implementation is DFS from roots with a `placed` HashSet to avoid revisiting.
- **Assumption:** The comment claims topological sort, but the code uses DFS with cycle protection. Cycles create "orphan" nodes that are appended at the end (line 657-681).
- **Difficulty to change:** **Trivial** — the algorithm already handles cycles; just fix the misleading comment.

### 2.2 — `ascii-dag` crate: layer assignment is topological depth
- **File:** `src/tui/dag_layout.rs:8`
- **What:** Comment: "Layer assignment (topological depth)" — the ascii-dag crate internally performs topological sorting to assign layers.
- **Assumption:** The ascii-dag crate requires a DAG. The code handles this by detecting and stripping back-edges before passing to ascii-dag (lines 259-266), then rendering back-edges separately as upward arrows.
- **Difficulty to change:** **Already handled** — back-edge detection (`detect_back_edges` at line 120) strips cycles before calling ascii-dag. The code explicitly handles cycles with `has_cycles` flag and `back_edges` list.

### 2.3 — docs: topological sort references in research docs
- **Files:** `docs/workgraph-analysis-research.md:81`, `docs/petri-nets-research.md:358-363,523,593,603,608`, `docs/rust-ecosystem-research.md:31,70-87`, `docs/review-core-graph.md:138`
- **What:** Research documents discuss topological sort as a technique for cycle detection, critical path analysis, and task ordering.
- **Difficulty to change:** **N/A** — research documents, not active code.

---

## Category 3: Cycle Detection Code That Treats Cycles as Errors

### 3.1 — `check.rs`: cycle detection (`check_cycles`)
- **File:** `src/check.rs:21-41`
- **What:** `check_cycles()` finds all cycles using DFS with recursion stack. `check_all()` at line 117 sets `ok = cycles.is_empty() && orphan_refs.is_empty()`, meaning cycles make the graph "not OK."
- **Assumption:** **Partially treats cycles as errors.** The `CheckResult.ok` field is false when cycles exist. However, the `wg check` command (see 3.2) treats them as warnings, not errors.
- **Difficulty to change:** **Moderate** — the `ok` field conflates cycles with actual errors (orphan refs). Should separate "has_unintentional_cycles" from "has_orphans" or remove cycles from the `ok` check entirely.

### 3.2 — `commands/check.rs`: cycles shown as warnings
- **File:** `src/commands/check.rs:22-28`
- **What:** Displays cycles as "Warning: Cycles detected (this is OK for recurring tasks):" — but the exit code (via `anyhow::bail!` on line 40) only fails on orphan references, not on cycles alone.
- **Assumption:** **Already cycle-tolerant** — cycles are warnings, not errors. Exit code is OK if only cycles (no orphan refs).
- **Difficulty to change:** **Trivial** — already handles cycles gracefully in the command layer.

### 3.3 — `commands/loops.rs`: cycle classification system
- **File:** `src/commands/loops.rs:1-189`
- **What:** Full cycle analysis command that classifies cycles as `Intentional`, `Warning`, or `Info` based on tags (`recurring`, `cycle:intentional`) and cycle length.
- **Assumption:** **Cycle-aware** — this is the primary cycle management command. Short cycles without tags are warnings; tagged cycles are intentional.
- **Difficulty to change:** **N/A** — already designed for cycle support.

### 3.4 — `commands/analyze.rs`: cycle classification in health check
- **File:** `src/commands/analyze.rs:285-326`
- **What:** Structural health check classifies cycles and shows warnings for unintentional ones, OK for intentional ones. Uses same classification as `loops.rs`.
- **Assumption:** **Mostly cycle-aware** — distinguishes intentional from unintentional cycles.
- **Difficulty to change:** **Trivial** — already well-structured.

---

## Category 4: Critical Path Calculations (Only Valid for DAGs)

### 4.1 — `commands/critical_path.rs`: critical path with cycle exclusion
- **File:** `src/commands/critical_path.rs:39-237`
- **What:** Computes critical path (longest dependency chain by hours). **Explicitly detects cycles** (lines 74-76) and **excludes cycle nodes** from the critical path calculation. Reports "N cycle(s) were skipped" in output.
- **Assumption:** Critical path is only meaningful for the acyclic portion of the graph. Cycle nodes are entirely excluded rather than being handled.
- **Difficulty to change:** **Hard** — critical path is fundamentally a DAG algorithm. For cycles, you'd need a different concept (e.g., "longest acyclic path" or "critical path ignoring back-edges"). The current approach (skip cycles) is the standard workaround.

### 4.2 — `commands/critical_path.rs`: longest path calculation (memoized DFS)
- **File:** `src/commands/critical_path.rs:278-321`
- **What:** `calculate_longest_path()` uses memoization. It checks `cycle_nodes` set to skip cycle participants, preventing infinite recursion.
- **Assumption:** Without the cycle exclusion guard, this would infinite-loop on cycles.
- **Difficulty to change:** **Hard** — inherent to the algorithm.

### 4.3 — `commands/viz.rs`: critical path in visualization
- **File:** `src/commands/viz.rs:342-433`
- **What:** `calculate_critical_path()` and `calc_longest_path()` compute the longest dependency chain for highlighting in DOT/Mermaid output. Uses `visited` set for cycle detection (line 397).
- **Assumption:** Without the visited guard, would infinite-loop on cycles.
- **Difficulty to change:** **Hard** — same inherent limitation as 4.1.

### 4.4 — `commands/forecast.rs`: critical path in forecast
- **File:** `src/commands/forecast.rs:318-408`
- **What:** `find_critical_path()` and `find_longest_path_from()` compute the critical path for project completion estimates. **No explicit cycle protection** — `find_longest_path_from()` recurses without a `visited` set.
- **Assumption:** **Assumes DAG** — will stack overflow on cyclic graphs.
- **Difficulty to change:** **Moderate** — add a `visited` HashSet parameter to `find_longest_path_from()`, similar to how `viz.rs` does it.

---

## Category 5: Dependency Resolution That Would Infinite-Loop on Cycles

### 5.1 — `query.rs`: `ready_tasks()` — safe
- **File:** `src/query.rs:241-262`
- **What:** Checks if all blockers are done. Only looks one level deep (direct blockers), so cycles don't cause infinite loops.
- **Assumption:** **Safe** — no recursion, no DAG assumption.
- **Difficulty to change:** **N/A**

### 5.2 — `query.rs`: `tasks_within_constraint()` — potential infinite loop
- **File:** `src/query.rs:113-222`
- **What:** Second pass (lines 175-215) uses `while changed` loop to find tasks that become unblocked. On a cycle, tasks could keep "unblocking" each other if all tasks in the cycle get `completed_in_plan`.
- **Assumption:** **Weak DAG assumption** — in practice, the `completed_in_plan` set prevents re-processing (line 179), so it terminates. But the logic is DAG-flavored: it assumes completing blockers unlocks dependents in a forward direction.
- **Difficulty to change:** **Moderate** — for cycles, you'd need to decide: does completing one task in a cycle unblock the others? The current behavior would: once one cycle member fits the budget, all others in the cycle become "unblocked."

### 5.3 — `query.rs`: `cost_of_recursive()` — protected
- **File:** `src/query.rs:283-310`
- **What:** Computes transitive cost with a `visited` HashSet to prevent infinite recursion.
- **Assumption:** **Cycle-safe** — visited set handles cycles.
- **Difficulty to change:** **N/A**

### 5.4 — `commands/forecast.rs`: `find_longest_path_from()` — **UNSAFE**
- **File:** `src/commands/forecast.rs:370-408`
- **What:** Recursive function with **no visited set**. If a cycle exists in the active task graph, this will recurse infinitely until stack overflow.
- **Assumption:** **Assumes strict DAG** — no cycle protection.
- **Difficulty to change:** **Moderate** — add `visited: &mut HashSet<String>` parameter.

### 5.5 — `commands/bottlenecks.rs`: `collect_transitive_dependents()` — protected
- **File:** `src/commands/bottlenecks.rs:157-168`
- **What:** Uses `visited.insert()` return value to short-circuit. Cycle-safe.
- **Difficulty to change:** **N/A**

### 5.6 — `commands/impact.rs`: `collect_transitive_dependents()` — protected
- **File:** `src/commands/impact.rs:168-179`
- **What:** Same pattern as bottlenecks — uses visited set. Cycle-safe.
- **Difficulty to change:** **N/A**

### 5.7 — `commands/impact.rs`: `find_dependency_chains()` — protected
- **File:** `src/commands/impact.rs:183-213`
- **What:** Uses `visited` set (line 201) to terminate chain traversal on cycles.
- **Difficulty to change:** **N/A**

### 5.8 — `commands/structure.rs`: transitive dependent counting — protected
- **File:** `src/commands/structure.rs:134-147`
- **What:** Uses `visited` set with stack-based DFS. Cycle-safe.
- **Difficulty to change:** **N/A**

---

## Category 6: Status Propagation Logic Assuming Linear Flow

### 6.1 — `commands/done.rs`: blocks completing tasks with open blockers
- **File:** `src/commands/done.rs:29-40`
- **What:** `wg done` checks if the task has unresolved blockers and refuses to mark it done if so. In a cycle, **no task can be marked done** because each blocks the other.
- **Assumption:** **Assumes linear forward progress** — in a cycle A→B→A, you can never complete either A or B because each waits for the other.
- **Difficulty to change:** **Hard** — this is a fundamental design question. Options: (1) allow force-completing with `--force`, (2) complete all cycle members simultaneously, (3) allow "done" to break cycle constraints for tagged cycles.

### 6.2 — Status enum: linear state machine
- **File:** `src/graph.rs:25-35`
- **What:** `Status` enum: Open, InProgress, Done, Blocked, Failed, Abandoned, PendingReview. No "Recurring" or "CycleActive" status.
- **Assumption:** Status transitions are forward-only: Open→InProgress→Done. No concept of Done→Open transitions that cycles would need.
- **Difficulty to change:** **Moderate** — adding a "Recurring" status or allowing Done→Open transitions requires updating all status-matching code across the codebase.

### 6.3 — `query.rs`: `ready_tasks()` — blocked tasks in cycles are never ready
- **File:** `src/query.rs:241-262`
- **What:** A task is ready only if ALL blockers are Done. In a cycle, at least one blocker is never Done, so cyclic tasks are perpetually blocked.
- **Assumption:** **Assumes forward-only progress** — cycles create permanent blockage.
- **Difficulty to change:** **Moderate** — would need cycle-aware readiness: if all non-cycle blockers are done, a cycle member could be considered "ready."

---

## Category 7: The `wg dag` Command Name Itself

### 7.1 — CLI command definition
- **File:** `src/main.rs:419`
- **What:** `Dag` variant in `Commands` enum with `/// Show ASCII DAG of the dependency graph`
- **Difficulty to change:** **Trivial** — rename to `Graph` or `Deps`

### 7.2 — Command name string
- **File:** `src/main.rs:1418`
- **What:** `Commands::Dag { .. } => "dag"` — used for logging/telemetry
- **Difficulty to change:** **Trivial**

### 7.3 — `OutputFormat` parsing accepts "dag" as alias for ASCII
- **File:** `src/commands/viz.rs:26`
- **What:** `"ascii" | "dag" => Ok(OutputFormat::Ascii)` — "dag" is accepted as a format name
- **Difficulty to change:** **Trivial** — keep as backward compat alias

---

## Category 8: DAG Layout Engine (TUI)

### 8.1 — `dag_layout.rs`: entire module
- **File:** `src/tui/dag_layout.rs:1-1525+`
- **What:** Module named `dag_layout` using `ascii-dag` crate. The module is extensively DAG-named: `DagLayout` struct, `DagLayout::compute()`, `dag_selected`, `dag_scroll_x`, etc.
- **Assumption:** The `ascii-dag` crate requires acyclic input. The code handles this by detecting back-edges (line 120 `detect_back_edges`), stripping them before passing to ascii-dag (line 261-266), and rendering them separately as upward arrows in magenta (lines 836-900).
- **Difficulty to change:** **Moderate** (naming) / **Already handled** (functionality). The layout engine already fully supports cycles via back-edge detection and separate rendering. Only the names use "DAG" terminology.

### 8.2 — `app.rs`: DAG-related field names
- **File:** `src/tui/app.rs:268-299`
- **What:** `GraphViewMode::Dag`, `dag_layout`, `dag_selected`, `dag_scroll_x`, `dag_scroll_y` — all named with "dag" prefix.
- **Difficulty to change:** **Moderate** — widespread renaming across TUI code, but purely cosmetic.

### 8.3 — `app.rs`: DAG-related methods
- **File:** `src/tui/app.rs:498-576`
- **What:** `toggle_view_mode()`, `dag_select_next()`, `dag_select_prev()`, `dag_scroll_left()`, `dag_scroll_right()`, `dag_selected_task_id()`, `dag_toggle_detail()`, `dag_ensure_visible()` — all "dag_" prefixed.
- **Difficulty to change:** **Moderate** — widespread renaming.

---

## Category 9: Dependency Addition Without Cycle Checking

### 9.1 — `commands/add.rs`: no cycle check on new tasks
- **File:** `src/commands/add.rs:55-62`
- **What:** When adding a task with `--blocked-by`, the blocked_by list is set directly without checking if it would create a cycle.
- **Assumption:** **No DAG enforcement** — cycles are allowed to be created. This is consistent with the "cycles permitted" design.
- **Difficulty to change:** **N/A** — this is intentionally permissive.

### 9.2 — `commands/edit.rs`: no cycle check when adding blockers
- **File:** `src/commands/edit.rs:55-63`
- **What:** When editing a task to add `--add-blocked-by`, blockers are added without cycle checking.
- **Assumption:** **No DAG enforcement** — same as 9.1.
- **Difficulty to change:** **N/A** — intentionally permissive.

---

## Summary Table

| # | Location | File:Line(s) | DAG Assumption | Cycle-Safe? | Change Difficulty |
|---|----------|-------------|----------------|-------------|-------------------|
| 1.1 | `wg dag` command name | main.rs:419 | Naming only | N/A | Trivial |
| 1.2 | `wg dag` routing | main.rs:1600 | Naming only | N/A | Trivial |
| 1.3 | README "dependency DAG" | README.md:427 | Naming only | N/A | Trivial |
| 1.4 | README keybinding | README.md:457 | Naming only | N/A | Trivial |
| 1.5 | README command ref | README.md:609 | Naming only | N/A | Trivial |
| 1.6 | SKILL.md references | SKILL.md:47,157-159 | Naming only | N/A | Trivial |
| 1.7 | ascii-dag crate | Cargo.toml:34 | External crate | Handled via back-edge stripping | N/A |
| 1.8 | docs/README.md | docs/README.md:81 | Already nuanced | N/A | N/A |
| 1.9 | Analysis research doc | docs/workgraph-analysis-research.md:24 | Already cycle-aware | N/A | N/A |
| 2.1 | TUI tree comment | tui/app.rs:583-588 | Misleading comment | Code is safe | Trivial |
| 2.2 | ascii-dag topo depth | tui/dag_layout.rs:8 | Crate requires DAG | Handled | Already handled |
| 2.3 | Research docs | Multiple docs | Discussion only | N/A | N/A |
| 3.1 | `check_cycles()` | check.rs:21-41,117 | `ok` false on cycles | Partially | Moderate |
| 3.2 | `wg check` display | commands/check.rs:22-28 | Warning, not error | Yes | Trivial |
| 3.3 | `wg loops` | commands/loops.rs:1-189 | Cycle classifier | Yes | N/A |
| 3.4 | `wg analyze` health | commands/analyze.rs:285-326 | Cycle classifier | Yes | Trivial |
| 4.1 | Critical path | commands/critical_path.rs:39-237 | Skips cycle nodes | Handled | Hard |
| 4.2 | Longest path (CP) | commands/critical_path.rs:278-321 | Cycle exclusion guard | Handled | Hard |
| 4.3 | Viz critical path | commands/viz.rs:342-433 | Visited guard | Handled | Hard |
| 4.4 | Forecast critical path | commands/forecast.rs:318-408 | **No cycle protection** | **NO** | Moderate |
| 5.1 | `ready_tasks()` | query.rs:241-262 | One-level check | Safe | N/A |
| 5.2 | `tasks_within_constraint()` | query.rs:113-222 | Weak DAG assumption | Safe (terminates) | Moderate |
| 5.3 | `cost_of_recursive()` | query.rs:283-310 | Visited set | Safe | N/A |
| 5.4 | `find_longest_path_from()` | forecast.rs:370-408 | **No visited set** | **NO** | Moderate |
| 5.5 | Bottleneck dependents | bottlenecks.rs:157-168 | Visited set | Safe | N/A |
| 5.6 | Impact dependents | impact.rs:168-179 | Visited set | Safe | N/A |
| 5.7 | Impact chains | impact.rs:183-213 | Visited set | Safe | N/A |
| 5.8 | Structure counting | structure.rs:134-147 | Visited set | Safe | N/A |
| 6.1 | `wg done` blocker check | commands/done.rs:29-40 | Linear forward flow | **Blocks cycles** | Hard |
| 6.2 | Status enum | graph.rs:25-35 | Linear states | No cycle status | Moderate |
| 6.3 | `ready_tasks()` readiness | query.rs:241-262 | Forward-only | **Blocks cycles** | Moderate |
| 7.1 | CLI command def | main.rs:419 | Naming | N/A | Trivial |
| 7.2 | Command name string | main.rs:1418 | Naming | N/A | Trivial |
| 7.3 | Format alias | viz.rs:26 | Naming | N/A | Trivial |
| 8.1 | dag_layout module | tui/dag_layout.rs:* | Naming + crate | Already handled | Moderate |
| 8.2 | TUI field names | tui/app.rs:268-299 | Naming | N/A | Moderate |
| 8.3 | TUI methods | tui/app.rs:498-576 | Naming | N/A | Moderate |
| 9.1 | `wg add` no cycle check | commands/add.rs:55-62 | Permissive | N/A | N/A |
| 9.2 | `wg edit` no cycle check | commands/edit.rs:55-63 | Permissive | N/A | N/A |

---

## Critical Bugs (Would Crash or Infinite-Loop on Cycles)

1. **`forecast.rs:370-408`** — `find_longest_path_from()` has no visited set and will stack overflow on cyclic task graphs. This is the only true bug found.

## Semantic Issues (Cycles Create Dead Ends)

1. **`done.rs:29-40`** — Tasks in a cycle can never be marked done because each blocks the other. This is the biggest functional limitation of the current design for supporting cycles.
2. **`query.rs:241-262`** — Tasks in cycles are perpetually "not ready" because their blockers are never done.
3. **`check.rs:117`** — `CheckResult.ok` is false when cycles exist, even though cycles are allowed.

## Naming/Terminology Issues (Cosmetic)

~15 locations use "DAG" in names, comments, or help text. All are trivial to moderate difficulty to rename.

## Already Handled

The `dag_layout.rs` module is the most sophisticated cycle handler — it detects back-edges, strips them for the ascii-dag crate, renders them separately with distinct styling, and tracks `has_cycles`. The `why_blocked.rs`, `bottlenecks.rs`, `impact.rs`, `structure.rs`, and `cost_of` functions all use visited sets correctly.
