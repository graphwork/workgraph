# Design: TUI Iteration History Browsing + Viz Self-Loop Indicator

## 1. Data Model: Iteration History

### What exists today

**Cycle iteration tracking** (`src/graph.rs:278-284`):
- `loop_iteration: u32` — current cycle iteration (0 = first run, incremented on re-activation)
- `last_iteration_completed_at: Option<String>` — timestamp when most recent iteration completed
- `cycle_failure_restarts: u32` — failure-triggered restart count (on cycle config owner only)
- `cycle_config: Option<CycleConfig>` — only on cycle header task; contains `max_iterations`, `guard`, `delay`, `no_converge`, `restart_on_failure`, `max_failure_restarts`

**Retry tracking** (`src/graph.rs:256-259`):
- `retry_count: u32` — number of retries after failure
- `max_retries: Option<u32>` — cap on retries
- `triage_count: u32` — requeue count via failed-dependency triage
- `resurrection_count: u32` — Done→Open transitions triggered by messages

**Log entries** (`src/graph.rs:76-85`):
- `Vec<LogEntry>` on each task — **accumulated across iterations**, never cleared on reset
- Each cycle reset appends: `"Re-activated by cycle iteration (iteration N/M)"` (`src/graph.rs:1577-1589`)
- Each failure restart appends: `"Re-activated by cycle failure restart"` (`src/graph.rs:1816-1828`)
- Log entries are the primary record of iteration boundaries

**Agent archives** (`src/commands/log.rs:127-168`):
- On task completion, `archive_agent()` copies agent `prompt.txt` + `output.log` to `.workgraph/log/agents/<task-id>/<ISO-timestamp>/`
- Each retry/iteration gets its own timestamped subdirectory
- `find_latest_archive()` in the TUI (`src/tui/viz_viewer/state.rs:12620-12643`) retrieves only the **most recent** archive
- CLI: `wg log agent <task-id>` lists all archived attempts (`src/commands/log.rs:174-250`)

### What's preserved between iterations

| Data | Preserved? | Notes |
|------|-----------|-------|
| Log entries | Yes | Accumulated, never cleared. Iteration boundaries detectable via "Re-activated" messages |
| Agent prompt/output archives | Yes | One timestamped dir per iteration in `.workgraph/log/agents/<task-id>/` |
| Artifacts list | Yes | `task.artifacts: Vec<String>` survives reset |
| `started_at`, `completed_at` | No | Cleared on reset (`src/graph.rs:1569-1570`) |
| `assigned` | No | Cleared on reset (`src/graph.rs:1568`) |
| `failure_reason` | No | Cleared on failure restart (`src/graph.rs:1807`) |
| `loop_iteration` | Overwritten | Set to new iteration number (`src/graph.rs:1572`) |

### What's missing for iteration browsing

1. **No per-iteration snapshot of task state.** Only the current iteration's `started_at`/`completed_at`/`assigned` are stored. Previous values are lost (except `last_iteration_completed_at` which stores one timestamp).

2. **No iteration index on log entries.** Log entries have timestamps but no explicit `iteration` field. The iteration boundary is inferred from "Re-activated" messages, which is fragile for display purposes.

3. **No iteration index on archived agent outputs.** Archives are timestamped directories, not keyed by iteration number. Mapping archive → iteration requires correlating timestamps.

### Retries vs. Cycle Iterations — different mechanisms

| Aspect | Retries | Cycle Iterations |
|--------|---------|-----------------|
| Trigger | Task failure + `max_retries` | All cycle members complete |
| Counter | `retry_count` (incremented) | `loop_iteration` (incremented) |
| Reset | `evaluate_cycle_on_failure` | `evaluate_cycle_iteration` |
| `loop_iteration` | Unchanged (same iteration retried) | Incremented to N+1 |
| Archives | Each retry archived separately | Each iteration archived separately |
| Log entry | "Re-activated by cycle failure restart" | "Re-activated by cycle iteration" |

Both produce archived agent outputs in the same directory structure, making the TUI browsing UX identical.

---

## 2. TUI Mockup: Iteration History Browsing

### Proposed location: Detail tab, new "Iterations" section

The Detail tab (`load_hud_detail` at `src/tui/viz_viewer/state.rs:6041`) already shows sections like Description, Prompt, Output, Cycle, Timing, etc. Add an **"── Iterations ──"** section between "Cycle" and "Log" that lists all past iterations with navigable entries.

### ASCII mockup

```
── my-cycle-task ──
Title: Process data batch
Status: in-progress
Agent: agent-abc123

── Cycle ──
  Iteration: 3/5
  Delay:     5m
  Last iter: 12m ago

── Iterations ──                          ← NEW SECTION
  ▶ Iteration 3 (current)   in-progress   2m ago
    Iteration 2              done          17m ago    [Enter: view]
    Iteration 1              done          34m ago    [Enter: view]
    Iteration 0              done          51m ago    [Enter: view]

── Description ──
  Process the next batch of data...
```

When the user presses Enter on a past iteration, replace the Output/Prompt sections with that iteration's archived content:

```
── Iterations ──
    Iteration 3 (current)   in-progress   2m ago
  ▶ Iteration 2              done          17m ago    ← SELECTED
    Iteration 1              done          34m ago
    Iteration 0              done          51m ago

── Output (Iteration 2) ──
  [archived agent output from .workgraph/log/agents/my-cycle-task/2026-04-02T10:15:00Z/output.txt]

── Prompt (Iteration 2) ──
  [archived prompt from .workgraph/log/agents/my-cycle-task/2026-04-02T10:15:00Z/prompt.txt]
```

### Alternative: Keybinding approach (simpler)

Instead of a navigable section, add `[` and `]` keybindings to cycle through iterations when viewing a task with `loop_iteration > 0` or multiple archives. Show a small indicator in the header:

```
── my-cycle-task ── [iter 2/5, viewing: 2]
Title: Process data batch
Status: done (historical — current iteration is 3)
```

This is simpler to implement and avoids modifying the section layout.

### Recommendation

Start with the **keybinding approach** (`[`/`]` to browse iterations) for implementation simplicity, then add the navigable section as a follow-up.

### Edge cases

- **0 iterations (loop_iteration=0, no archives):** No iteration section shown. `[`/`]` are no-ops.
- **50+ iterations:** Show most recent 20 in the section view, with a "... and 30 more" indicator. `[`/`]` can navigate all.
- **Nested cycles (cycle within cycle):** Each task tracks its own `loop_iteration` independently. The TUI shows the selected task's iteration, not the cycle's.
- **Retried tasks (no cycle_config, retry_count > 0):** Same UX — the archive directory has multiple timestamped subdirs. Label them "Attempt N" instead of "Iteration N".
- **Task with archives but no cycle_config or retry_count:** Possible if config was removed. Fall back to archive count for navigation.

---

## 3. Viz Mockup: Self-Loop Indicator

### Current behavior

**Two-task cycles** show bidirectional arcs via the back-edge rendering in Phase 2 (`draw_back_edge_arcs` at `src/commands/viz/ascii.rs:1066`):
```
.coordinator-17  (in-progress) [turn 10] ←─┐
└→ .compact-17  (open) ────────────────────┘
```

**Self-loops** (task with itself in `after` list, detected as `blocker_line == dependent_line` at line 1082) currently append ` ↺` to the line (`src/commands/viz/ascii.rs:1091`):
```
my-self-loop-task  (in-progress) ↺
```

**Tasks with `cycle_config` or `loop_iteration > 0`** show iteration info in the node label (`src/commands/viz/ascii.rs:298-312`):
```
my-cycle-header  (in-progress) ↺ (iter 3/10)
```

### The problem

The `↺` appended by `draw_back_edge_arcs` for self-loop back-edges and the `↺` in the node label from `format_node` for cycle config tasks are **two separate mechanisms**:

1. `format_node` at line 307: `" ↺ (iter N/M)"` or `" ↺ (iter N)"` — for any task with `cycle_config` or `loop_iteration > 0`
2. `draw_back_edge_arcs` at line 1091: `" ↺"` — for self-loop back-edges detected during arc rendering

A task can trigger both (resulting in double `↺`), or only one. The current rendering of self-loop edges is **functional but minimal** — just appending `↺` without any arc visualization.

### Proposed enhancement

The self-loop `↺` from back-edge detection (mechanism 2) is already present. The improvement should focus on:

**A. Better visual distinction between the two ↺ meanings:**
- Node label `↺ (iter N/M)` = "this task participates in a cycle and shows current iteration"
- Back-edge `↺` = "this task has a structural self-dependency"

When both are present, they currently collide: `task (in-progress) ↺ (iter 3/10) ↺`

**B. Proposed rendering:**

For self-loop tasks, use `⟳` (U+27F3, clockwise gapped circle arrow) instead of appending a bare `↺`, and add coloring consistent with other back-edge arcs:

Before:
```
my-self-loop  (in-progress) ↺ (iter 3/10) ↺
```

After:
```
my-self-loop  (in-progress) ↺ (iter 3/10) ⟳
```

Or, deduplicate: if the node label already contains `↺`, suppress the back-edge `↺`:

After (preferred):
```
my-self-loop  (in-progress) ↺ (iter 3/10)
```

For tasks that are self-loops but have NO cycle_config (unusual, but possible):
```
standalone-self-loop  (open) ⟳
```

### How self-loops are represented in the graph model

A self-loop occurs when a task's `after` list contains its own ID. The `CycleAnalysis::from_graph` (`src/graph.rs:1099`) uses Tarjan/Havlak to detect SCCs, which finds single-task SCCs with self-edges. In the viz, this creates a `BackEdgeArc` where `blocker_line == dependent_line` (`src/commands/viz/ascii.rs:1082`).

### Unicode options for self-loop indicator

| Symbol | Unicode | Name | Pros | Cons |
|--------|---------|------|------|------|
| ↺ | U+21BA | Anticlockwise open circle arrow | Already used in node labels | Collides with existing ↺ |
| ⟳ | U+27F3 | Clockwise gapped circle arrow | Distinct from ↺, visually "loop" | May not render in all terminals |
| 🔄 | U+1F504 | Counterclockwise arrows | Very visible | Emoji, double-width, inconsistent rendering |
| ⥀ | U+2940 | Anticlockwise closed circle arrow | Compact | Obscure, poor font support |
| ↻ | U+21BB | Clockwise open circle arrow | Similar to ↺ | Too similar, hard to distinguish |

**Recommendation:** Use `⟳` for the back-edge self-loop indicator (distinct from `↺` used in node labels). If the node label already has `↺ (iter ...)`, suppress the back-edge self-loop indicator entirely since the iteration info already communicates "this task loops."

---

## 4. Implementation Plan

### Feature 1: TUI Iteration History Browsing

#### Step 1: Add iteration browsing state (`src/tui/viz_viewer/state.rs`)
- Add `viewing_iteration: Option<u32>` field to `VizApp` struct (~line 3340)
- Add `iteration_archives: Vec<(String, PathBuf)>` to cache discovered archives (timestamp, path)
- Add `fn load_iteration_archives(&mut self, task_id: &str)` — scans `.workgraph/log/agents/<task-id>/` and builds the list
- Add `fn viewing_iteration_label(&self) -> Option<String>` — returns "Iteration N" or "Attempt N"

#### Step 2: Add keybinding handlers (`src/tui/viz_viewer/event.rs`)
- Add `KeyCode::Char('[')` → decrement `viewing_iteration` (older)
- Add `KeyCode::Char(']')` → increment `viewing_iteration` (newer) or None for current
- Only active when `RightPanelTab::Detail` is focused and selected task has archives
- Show toast: "Viewing iteration 2/5" or "Viewing current"

#### Step 3: Modify `load_hud_detail` (`src/tui/viz_viewer/state.rs:6041`)
- When `viewing_iteration.is_some()`, load Output/Prompt from the corresponding archive directory instead of the live agent dir
- Add "(Iteration N)" suffix to the Output/Prompt section headers
- Add indicator in the task header line: `── my-task ── [viewing iter 2/5]`

#### Step 4: Add "Iterations" section to detail view (`src/tui/viz_viewer/state.rs:6463`)
- After the existing "── Cycle ──" section, add "── Iterations ──"
- List all archived iterations with timestamps and relative time
- Mark current iteration with `(current)` label

#### Step 5: Add help text (`src/tui/viz_viewer/render.rs`)
- Add `[/]` to the bottom hints bar when viewing a cycling/retried task

### Feature 2: Viz Self-Loop Indicator

#### Step 1: Deduplicate ↺ (`src/commands/viz/ascii.rs:1089-1093`)
- In the self-loop rendering block, check if the line already contains `↺` (from `format_node`)
- If it does, skip appending the back-edge `↺`
- If it doesn't (self-loop without cycle_config), append `⟳` instead of `↺`

#### Step 2: Apply arc coloring to self-loop indicator (`src/commands/viz/ascii.rs:1089-1093`)
- When `use_color` is true, wrap `⟳` in the `arc_color_code` ANSI sequence
- Consistent with how back-edge arcs are colored

#### Step 3: Update `char_edge_map` for self-loops (`src/commands/viz/ascii.rs`)
- Currently self-loops don't get an entry in `char_edge_map`
- Add the `⟳` character position to `char_edge_map` so TUI click-to-highlight works for self-loop edges

#### Step 4: Add/update tests (`src/commands/viz/ascii.rs` test module)
- Existing tests at ~line 1974 already test `↺` presence
- Add test: self-loop task WITH cycle_config → single `↺ (iter N/M)`, no duplicate
- Add test: self-loop task WITHOUT cycle_config → `⟳` indicator
- Add test: non-self-loop cycle task → no `⟳`, standard back-edge arc

### Files to modify (ordered)

| Order | File | Changes |
|-------|------|---------|
| 1 | `src/commands/viz/ascii.rs` | Self-loop dedup + `⟳` indicator + coloring + char_edge_map + tests |
| 2 | `src/tui/viz_viewer/state.rs` | `VizApp` fields + `load_iteration_archives` + modify `load_hud_detail` + "Iterations" section |
| 3 | `src/tui/viz_viewer/event.rs` | `[` / `]` keybinding handlers for iteration browsing |
| 4 | `src/tui/viz_viewer/render.rs` | Hint bar update for `[/]` keys |

### Complexity estimate

- Viz self-loop (Feature 2): Small — ~30 lines of code changes in `ascii.rs`
- TUI iteration browsing (Feature 1): Medium — ~150-200 lines across state/event/render files

### Dependencies between features

None — these can be implemented in parallel since they modify different code paths (viz ascii renderer vs. TUI detail panel).
