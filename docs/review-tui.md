# TUI Interface Review

**Scope:** `src/tui/mod.rs` (1,422 lines), `src/tui/app.rs` (1,370 lines), `src/tui/dag_layout.rs` (1,570 lines) — 4,362 lines total.

---

## 1. Architecture Overview

### Event Loop (`mod.rs`)

The TUI follows a standard ratatui architecture:

1. **Setup** — `run()` installs a panic hook that restores the terminal, enables raw mode, enters the alternate screen, then delegates to `run_event_loop()`.
2. **Main loop** — `run_event_loop()` is a tight poll loop: call `app.maybe_refresh()` and `app.poll_log_viewer()`, draw the frame, then `event::poll(250ms)` for keyboard input.
3. **Input dispatch** — Key events are routed by current `View` enum to `handle_key()` (Dashboard), `handle_log_key()` (LogView), or `handle_graph_key()` (GraphExplorer). A global `?` toggles a help overlay that swallows all other keys.
4. **Rendering dispatch** — `draw()` delegates to `draw_dashboard()`, `draw_log_view()`, or `draw_graph_explorer()`, then conditionally draws the help overlay.

The panic hook and terminal restoration pattern is correct. There is a minor redundancy: `run()` calls both a custom `restore_terminal()` via the panic hook and `ratatui::restore()` on normal exit — these are functionally identical but the asymmetry is harmless.

### State Management (`app.rs`)

All mutable state lives in a single `App` struct (~20 fields). Sub-views are represented as `Option<T>`:
- `log_viewer: Option<LogViewer>` — populated only when viewing an agent log
- `graph_explorer: Option<GraphExplorer>` — populated only when in graph explorer mode

**Data refresh cycle:**
1. `maybe_refresh()` checks if `poll_interval` has elapsed since `last_refresh`.
2. `refresh_all()` reloads tasks from `graph.jsonl` and agents from `registry.json`, computes diff-based highlights (magenta flash for 3 seconds on change), preserves selection by ID, and updates graph explorer if open.

**Change detection** uses snapshot maps (`prev_task_snapshots`, `prev_agent_snapshots`) that store `format!("{:?}", status)` strings. New items and changed items are tracked separately. The `first_load` flag suppresses highlighting on initial data load.

### DAG Layout Engine (`dag_layout.rs`)

Uses the `ascii-dag` crate for Sugiyama-style layered layout (layer assignment + crossing minimization). The pipeline:

1. **Build graph** — Map task IDs to numeric IDs for ascii-dag. Collect edges from `blocked_by` relationships.
2. **Cycle detection** — Custom iterative DFS (`detect_back_edges()`) identifies back-edges before passing to ascii-dag (which requires acyclic input). Back-edges are separated and routed distinctly.
3. **Layout via ascii-dag** — `DAG::from_edges()` + `compute_layout()` produces a `LayoutIR` with level/x assignments.
4. **Position computation** — Custom widths based on title length. Nodes packed left-to-right per layer. `center_layers()` centers each layer relative to the widest.
5. **Edge routing** — `reroute_edges()` routes normal edges (vertical → horizontal jog → vertical) and back-edges (right margin, going upward).
6. **Character buffer rendering** — `render_to_buffer()` uses a connectivity grid (4-bit flags: UP/DOWN/LEFT/RIGHT per cell) to resolve Unicode box-drawing characters. Nodes are drawn first, then edges fill empty cells.

### Rendering Pipeline

```
mod.rs::draw()
 ├─ draw_dashboard()
 │   ├─ draw_task_list()      — ratatui List with status colors, highlight flash
 │   ├─ draw_agent_list()     — ratatui List with PID liveness colors
 │   └─ draw_status_bar()     — task/agent counts, service status, key hints
 ├─ draw_log_view()           — agent log with auto-scroll and manual wrapping
 ├─ draw_graph_explorer()
 │   ├─ draw_graph_tree_view() — indented tree with collapse/expand
 │   └─ draw_graph_dag_view()  — DAG render_to_buffer() → styled spans
 └─ draw_help_overlay()       — centered keybinding reference
```

---

## 2. Code Quality Assessment

### Strengths

1. **Clear separation of views.** The `View` enum cleanly routes both input and rendering. Each view has its own key handler and draw function.

2. **Correct terminal lifecycle management.** Panic hook ensures terminal restoration even on crashes. The pattern `ratatui::init()` + `ratatui::restore()` on normal exit is correct.

3. **Good data refresh model.** Polling with configurable interval, diff-based change detection, and visual feedback (highlight flash) for changes is a well-thought-out UX for a monitoring dashboard.

4. **Selection preservation across refreshes.** Both dashboard and graph explorer preserve selection by task/agent ID through refreshes, which is important for usability.

5. **Robust cycle handling in DAG layout.** The DFS-based back-edge detection with iterative stack (avoids stack overflow on deep graphs), separate back-edge rendering with distinct styling, and clean filtering before passing to ascii-dag all demonstrate careful engineering.

6. **Deterministic layout.** Task IDs are sorted before processing to ensure HashMap iteration order doesn't affect layout. Verified by a dedicated test (`test_dag_layout_deterministic_ordering`).

7. **Good test coverage for dag_layout.** Eight tests covering simple chains, diamonds, fan-out, skip-layer edges, cycles, multiple cycles, acyclic verification, and determinism.

### Issues

#### Medium Severity

**M1. Duplicated sort-key logic.** `TaskEntry::sort_key()` (app.rs:34–44) and `sort_key_for_status()` (app.rs:750–760) are identical functions. The agent sort order (app.rs:1153–1163) is a third near-duplicate. This risks divergence if new statuses are added.

**M2. Duplicated graph-loading in `toggle_detail()`.** Both `toggle_detail()` and `dag_toggle_detail()` (app.rs:433–555) re-parse `graph.jsonl` from disk to load a single task. This could be avoided by caching the graph during `rebuild()` or by indexing the already-loaded data.

**M3. `format!("{:?}", status)` for snapshot diffing.** Using Debug formatting as a comparison key (app.rs:1105, 1183) is fragile — if the Debug output changes or if two distinct states happen to format identically, the diff breaks. Should use `PartialEq` on the actual enum values or derive `Hash`/`Eq` on the snapshot structs with proper fields.

**M4. Log viewer `viewport_height` hardcoded fallback.** `handle_log_key()` uses a hardcoded `viewport_height = 20` (mod.rs:231) for scroll-down and page-up/down calculations. The actual viewport height is only known during `draw()`. This causes scroll jumps to be inaccurate until the next render. Should store the viewport height in `LogViewer` during draw and use it in key handlers.

**M5. Redundant `if let Some(ref mut explorer) = app.graph_explorer` patterns.** In `handle_graph_key()` (mod.rs:109–225), nearly every match arm repeats `if let Some(ref mut explorer) = app.graph_explorer`. This could be simplified by extracting the explorer early and handling the `None` case once.

#### Low Severity

**L1. `left` arrow key handler has split logic.** In `handle_graph_key()` (mod.rs:170–183), `collapse()` is called inside `if let Some(ref mut explorer)` but `refresh_graph_explorer()` is called outside. This works but is confusing — the refresh is only needed for tree mode, yet it's checked separately via `if view_mode == GraphViewMode::Tree`. The same pattern exists for the `right` key (mod.rs:184–196).

**L2. `compute_critical_path` re-derives the adjacency structure.** `compute_critical_path()` (app.rs:764–865) builds its own `tasks`, `children`, and `roots` maps even though `build_graph_tree()` builds the same structures. These could share a pre-computed adjacency.

**L3. No scrolling in tree view.** `draw_graph_tree_view()` uses a `ListState` for stateful widget rendering, which handles scroll automatically via ratatui. This is correct, but worth noting that the tree view relies on ratatui's built-in scroll vs. the manual scroll offset approach used in log viewer and DAG view — this is fine but worth being aware of for consistency.

**L4. Back-edge routing can overflow buffer.** `reroute_edges()` routes back-edges to `max_x + 1 + i` (dag_layout.rs:610). While `BACK_EDGE_MARGIN = 3` is added to `layout.width`, if there are more than 3 back-edges, routing will exceed the buffer. `render_to_buffer()` adds `+ 2` padding (line 691) which helps, but it's not guaranteed sufficient for many cycles.

**L5. Abandoned status counted as `done` in TaskCounts.** `load_tasks()` increments `counts.done` for `Status::Abandoned` (app.rs:1089). This may be intentional (grouping "finished" tasks) but the `done` field name is misleading since it includes abandoned tasks.

---

## 3. Complexity Hotspots in `dag_layout.rs`

### Hotspot 1: `render_to_buffer()` (lines 686–928, ~242 lines)

This is the most complex function in the TUI. It:
1. Allocates a 2D character buffer
2. Builds a parallel 2D connectivity grid (4-bit flags per cell)
3. Processes each edge segment (vertical/horizontal) into connectivity flags
4. Marks arrow positions
5. Draws nodes
6. Resolves connectivity flags to box-drawing characters, merging with node borders
7. Draws back-edges with corners and arrows

**Recommendation:** Extract into smaller functions:
- `build_connectivity_grid(layout) -> Vec<Vec<u8>>` — edge connectivity computation
- `render_edges(buf, conn, arrows)` — character resolution from connectivity
- `render_back_edges(buf, layout)` — back-edge rendering with corners

### Hotspot 2: `DagLayout::compute()` (lines 202–448, ~246 lines)

This function does too many things:
1. Builds ID mappings
2. Collects edges and detects cycles
3. Invokes ascii-dag
4. Computes custom node widths
5. Groups by layer and assigns coordinates
6. Builds LayoutNode/LayoutEdge structs

**Recommendation:** This is the natural complexity of a layout pipeline and most stages are already sequential. Could extract "compute node widths" and "assign coordinates" into helper functions to improve readability, but the current structure is acceptable.

### Hotspot 3: Back-edge rendering in `render_to_buffer()` (lines 831–925, ~94 lines)

The back-edge rendering (dashed lines, corner detection, arrows) is interleaved with the main rendering. The corner-direction logic (lines 892–911) with 8 boolean flags (`from_left`, `from_right`, `from_above`, `from_below`, `to_left`, `to_right`, `to_above`, `to_below`) is hard to read.

**Recommendation:** Extract back-edge rendering into its own function. The corner-direction logic could use a similar connectivity-flag approach as normal edges instead of 8 separate booleans.

---

## 4. UX Observations & Suggestions

### Current UX Strengths
- **Vim-style navigation** (j/k/h/l) alongside arrow keys
- **Auto-scroll with pause/resume** in log viewer (scroll up pauses, G resumes)
- **Visual wavefront** in tree view — active agents are bright green with `●`, in-progress without agents are yellow, completed tasks are dimmed
- **Change flash highlighting** — magenta background for 3 seconds on status changes
- **Critical path highlighting** — longest incomplete chain shown in red
- **Cycle indicators** in DAG view with back-edge count in status bar

### Suggested Improvements

1. **Search/filter in task list.** With many tasks, there's no way to find a specific task by name or filter by status. A `/` key to enter a search mode would be valuable.

2. **Status filter for graph explorer.** Option to hide completed/abandoned tasks to focus on active work. Something like `f` to cycle through filter modes.

3. **More precise viewport_height in log viewer key handling.** Store the last-known viewport height in `LogViewer` and update it during each draw. This eliminates the `20` fallback and makes page-up/down accurate.

4. **Resize-aware DAG view.** On terminal resize, the DAG scroll position could be recalculated to keep the selected node centered. Currently the resize event is a no-op (mod.rs:81-83), relying on `dag_ensure_visible()` at next draw.

5. **Keyboard shortcut to switch between agent log viewers.** When multiple agents are running, `[` and `]` to switch between agent logs without going back to the dashboard first.

---

## 5. Summary

The TUI is well-structured for its size. The three-file split (event loop / state / DAG layout) provides good separation of concerns. The ratatui integration follows standard patterns. The DAG layout engine is the most complex component but handles edge cases (cycles, skip-layer edges, deterministic ordering) correctly with good test coverage.

**Key refactoring opportunities:**
- Consolidate duplicated sort-key functions
- Extract `render_to_buffer()` sub-functions to reduce its 242-line scope
- Replace Debug-format snapshot diffing with proper equality checks
- Fix the hardcoded `viewport_height = 20` fallback in log viewer key handlers

**No critical issues found.** The code is production-quality for a monitoring TUI.
