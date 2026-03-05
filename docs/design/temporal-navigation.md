# Temporal Navigation and Stream Management

## 0. The Unified Abstraction: Streams and Epochs

Everything in workgraph that accrues over time is a **stream**. Every stream is
segmented into **epochs** — structurally meaningful time boundaries that aren't
just "old vs new" but carry semantic weight.

| Stream Type | What defines an Epoch |
|---|---|
| Chat (inbox/outbox) | Compaction boundary (compactor runs, previous context is archived) |
| Cyclic task | One complete iteration through the cycle |
| Agent output | One agent run (spawn → done/fail) |
| Evaluations | One evaluation batch |
| Coordinator log | One coordinator session (spawn → crash/restart) |

**An epoch is the atomic unit of temporal navigation.** You can't zoom into half
an epoch — you see the current epoch in full, and access previous epochs on demand.

This gives us ONE model for all stream types, not six different solutions.

### Why Epochs, Not Time Windows

A time window ("last hour") cuts across structural boundaries — you'd see half
of a cycle iteration, part of an agent run. Epochs respect the structure: one
complete iteration, one complete agent session. The right grain for navigation
is the grain at which the system operates.

## 1. Temporal Layering

Three layers, same for every stream:

```
┌──────────────────────────────────────────────┐
│  LIVE        Current epoch, auto-scrolling   │  Always visible
│              Latest messages, current iter    │
├──────────────────────────────────────────────┤
│  PREVIOUS    Last 1-2 completed epochs       │  One keypress away (Shift+↑)
│              Collapsed by default             │
├──────────────────────────────────────────────┤
│  ARCHIVE     All completed epochs            │  Search or explicit navigation
│              Indexed, never deleted           │
└──────────────────────────────────────────────┘
```

**Per-stream, not global.** Each stream has its own epoch cursor. A "global
temporal focus" across heterogeneous streams doesn't compose well — a chat
epoch and a cycle epoch have different durations and structure.

**Exception:** The search interface (`/` key) IS global — it searches across
all streams simultaneously and shows results grouped by stream.

## 2. The Spiral: Cyclic Task Iterations

### Representation

Iterations stack vertically in the detail panel. The current iteration is
expanded; previous iterations are collapsed to a one-line summary.

```
┌─ write-tests (iteration 3 of 5) ──────────────┐
│                                                  │
│  ◆ Iteration 3 (current)                  ▼     │
│    Status: in-progress                           │
│    Agent: agent-4821                             │
│    Log: 4 entries                                 │
│    Output: tests/new_feature_test.rs             │
│                                                  │
│  ▶ Iteration 2 (done)           8m ago   ···    │
│  ▶ Iteration 1 (done)          22m ago   ···    │
│                                                  │
└──────────────────────────────────────────────────┘
```

`▶` = collapsed (press Enter to expand). `◆` = current (always expanded).
`···` = has content (vs blank = empty iteration).

### Comparing Iterations

`d` key on a collapsed iteration opens a diff view against the current iteration:

```
┌─ Diff: iteration 1 → 3 ───────────────────────┐
│                                                  │
│  Artifacts:                                      │
│    + tests/edge_case_test.rs    (added iter 2)  │
│    ~ tests/new_feature_test.rs  (modified)      │
│                                                  │
│  Log delta:                                      │
│    iter 1: 3 entries, iter 3: 4 entries          │
│                                                  │
│  Output delta:                                   │
│    iter 1: "Basic tests pass"                    │
│    iter 3: "All tests pass including edge cases" │
│                                                  │
└──────────────────────────────────────────────────┘
```

### What Carries Forward

On cycle iteration:
- **Carries forward:** artifacts list, accumulated log (tagged by iteration),
  messages
- **Archived per-iteration:** agent assignment, output capture, evaluation,
  status transitions, failure reasons

The `log` entries on a Task already have timestamps. We add an `iteration` tag
to each LogEntry so they can be filtered by iteration without losing the
chronological ordering.

## 3. Chat History Management

### Default View

Show the current epoch (since last compaction) in full. If no compaction has
run, show the last N messages (configurable, default 50).

The compaction boundary IS the archive boundary. When the compactor runs:
1. All messages before the compaction point become archived
2. The compactor's summary replaces them as "context"
3. The archived messages remain in the JSONL files, accessible via search

```
┌─ Chat ─────────────────────────────────────────┐
│  ┄┄┄ 142 earlier messages (/ to search) ┄┄┄   │
│                                                  │
│  [14:30] user: Add auth endpoint                │
│  [14:30] coordinator: Created task add-auth...  │
│  [14:35] user: What's the status?               │
│  [14:35] coordinator: 3 tasks in progress...    │
│                                                  │
│ ─────────────────────────────────────────────── │
│  > _                                             │
└──────────────────────────────────────────────────┘
```

The `┄┄┄ 142 earlier messages` line is clickable/selectable — pressing Enter
on it opens the archive viewer.

### Search

`/` activates search mode. Search is full-text across all chat history
(inbox + outbox), with results shown inline:

```
┌─ Chat ── search: "auth" ───────────────────────┐
│  3 results across 142 messages                   │
│                                                  │
│  [12:05] user: We need auth for the API         │
│  [12:06] coordinator: Created auth-research...  │
│  [14:30] user: Add auth endpoint                │
│                                                  │
│  [Esc] close search  [↑↓] navigate results     │
│ ─────────────────────────────────────────────── │
│  / auth_                                         │
└──────────────────────────────────────────────────┘
```

### Compaction Integration

The compactor (see `docs/design/coordinator-compactor-architecture.md`) produces
`context.md` which summarizes archived messages. The chat view shows the
compactor's summary as a foldable "session context" block at the top of the
chat when scrolled to the beginning:

```
┌─ Chat ─────────────────────────────────────────┐
│  ▶ Session context (compacted 14:00)            │
│  ┄┄┄ 142 earlier messages (/ to search) ┄┄┄   │
│  ...                                             │
```

Expanding the session context block shows the compactor's rolling narrative.

## 4. Multiple Coordinators

### Switching

Coordinators appear as named tabs at the top of the Chat panel:

```
┌─ Chat ─ [main] | staging | docs ───────────────┐
│                                                  │
│  [main coordinator messages here]                │
│                                                  │
└──────────────────────────────────────────────────┘
```

Switch with `1`-`9` number keys or `Tab`/`Shift+Tab` when in the Chat panel.

### Unified View

No "meta-coordinator" or merged view. Coordinators are independent streams with
independent epoch boundaries. The TUI shows one at a time, switchable.

Rationale: merging streams from independent coordinators into a timeline
creates confusion about who said what and what context they had. The graph view
already shows all tasks from all coordinators together — that's the unified view.

### Relationship Between Coordinators

Coordinators don't fork/merge from each other in the stream sense. They're
parallel, independent streams that operate on the same graph. Their relationship
is visible through the graph: tasks created by coordinator A may have dependencies
on tasks created by coordinator B.

If coordinators need to communicate, they use the existing message system
(`wg msg`), which appears in the Messages tab per-task.

## 5. Stream Forking and Merging

This is already represented in the graph: fan-out edges are visible as a task
with multiple `before` edges, and integration tasks have multiple `after` edges.

### Navigation

In the TUI graph view, pressing `Enter` on a task follows the dependency chain:
- On a fan-out task: shows the child tasks with their status
- On an integration task: shows which upstream tasks are done/pending

The existing graph visualization handles this. No new stream-level abstraction
is needed — forking/merging is a graph concept, not a temporal one.

### Visualization

The graph view already renders dependency edges. Adding temporal information:
tasks show their epoch (iteration number for cycle members) as a badge.

```
  ┌──────────┐     ┌──────────┐     ┌──────────┐
  │ research │────▶│ implement│────▶│ verify   │
  │ ✓ done   │     │ ● iter 2 │     │ ○ open   │
  └──────────┘     └──────────┘     └──────────┘
                          │                ▲
                          └────── loop ────┘
```

## 6. UI Primitives

### New Elements (4 total)

**1. Epoch selector** — appears in the Detail panel header for tasks with
multiple epochs (iterations/retries). Left/right arrows or number keys navigate.

```
  ◀ Iteration 2 of 5 ▶
```

**2. Archive boundary marker** — a visual separator in scrollable panels
(Chat, Log) showing where archived content begins.

```
  ┄┄┄ 142 earlier messages (/ to search) ┄┄┄
```

**3. Search overlay** — activated by `/` in any panel. Shows search results
inline within that panel's stream.

**4. Coordinator tabs** — sub-tabs within the Chat panel when multiple
coordinators exist.

### Keybindings

| Key | Context | Action |
|---|---|---|
| `/` | Any panel | Open search within current stream |
| `Esc` | Search active | Close search |
| `Shift+↑` | Any stream panel | Scroll to previous epoch boundary |
| `Shift+↓` | Any stream panel | Scroll to next epoch boundary |
| `d` | Detail, on collapsed iteration | Diff this iteration vs current |
| `1`-`9` | Chat panel | Switch coordinator (if multiple) |
| `Tab` | Chat panel | Next coordinator |
| `Enter` | On archive boundary | Open full archive viewer |

### Mouse

- Click on collapsed iteration to expand
- Click on archive boundary to open archive
- Scroll wheel works as normal within epochs
- Click coordinator tabs to switch

### Agent Search (CLI)

Agents need to search their own history too. The existing `wg log` and
`wg msg read` commands work for current-epoch data. For archive search:

```bash
wg search "pattern" --stream chat      # search chat history
wg search "pattern" --stream log       # search task logs
wg search "pattern" --task <id>        # search all streams for a task
wg search "pattern"                    # search everything
```

Output format: JSONL for machine parsing, human-readable table for TTY.

## 7. Data Model Changes

### LogEntry: Add iteration tag

```rust
pub struct LogEntry {
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub message: String,
    // NEW: which cycle iteration this entry belongs to (None = non-cyclic)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u32>,
}
```

### Task: Add epoch metadata

```rust
pub struct Task {
    // ... existing fields ...

    // NEW: history of per-iteration snapshots for cycle members
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iteration_history: Vec<IterationSnapshot>,
}

/// Snapshot of a completed cycle iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationSnapshot {
    pub iteration: u32,
    pub status: Status,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub agent: Option<String>,
    pub failure_reason: Option<String>,
    pub artifacts: Vec<String>,
    /// Summary line for collapsed view
    pub summary: Option<String>,
}
```

### ChatMessage: Add epoch marker

```rust
pub struct ChatMessage {
    // ... existing fields ...

    // NEW: compaction epoch this message belongs to (0 = before first compaction)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub epoch: u64,
}
```

### New: Search index

A lightweight full-text index file for search across archived content:

```
.workgraph/search/
  chat.idx        # trigram index of chat messages
  logs.idx        # trigram index of task logs
```

Built lazily on first search, updated incrementally. Simple trigram index —
no external dependencies. Falls back to linear scan if index doesn't exist.

## 8. Migration Path

All changes are backward-compatible:
- New fields have `#[serde(default)]` — old data loads fine without them
- `iteration_history` is empty by default — no migration needed
- `epoch` on ChatMessage defaults to 0 — all existing messages are "epoch 0"
- Search index is built on demand — no upfront migration

When `evaluate_cycle_iteration()` re-opens cycle members, it now also snapshots
the completed iteration into `iteration_history` before resetting. This is the
only behavioral change and it's additive.

## 9. Implementation Phases

### Phase 1: Iteration History (foundation)
**What:** Add `IterationSnapshot` and `iteration` tag to `LogEntry`. Update
`evaluate_cycle_iteration()` to snapshot before re-opening. Display in TUI
Detail panel as collapsed/expanded iterations.

**Why first:** This is the most structurally meaningful change — cycles are
workgraph's distinctive feature and currently lose per-iteration context.

**Files:** `src/graph.rs`, `src/tui/viz_viewer/render.rs` (detail panel)

**Effort:** Small. Data model change + render logic.

### Phase 2: Chat Archive Boundary
**What:** Add archive boundary marker to Chat panel. Show message count above
the boundary. Add `Shift+↑/↓` for epoch navigation. Wire up compaction epoch
tracking.

**Why second:** Chat is the primary user interface. The archive boundary
immediately reduces cognitive load.

**Files:** `src/chat.rs`, `src/tui/viz_viewer/render.rs` (chat panel),
`src/tui/viz_viewer/event.rs`

**Effort:** Small. Mostly render logic.

### Phase 3: Search
**What:** Implement `wg search` CLI command. Add `/` search overlay to TUI
panels. Build trigram index lazily.

**Why third:** Search enables archive access — without it, archived content
is invisible. But phases 1-2 reduce the need for search (current epoch is
usually sufficient), so search can wait.

**Files:** New `src/search.rs`, `src/tui/viz_viewer/event.rs`,
`src/tui/viz_viewer/render.rs`

**Effort:** Medium. Trigram index + search UI.

### Phase 4: Multi-Coordinator Tabs
**What:** Add coordinator tabs to Chat panel. Support switching with Tab/number
keys.

**Why fourth:** Multiple coordinators are a future feature. The tab infrastructure
should exist before it's needed, but doesn't block any current workflow.

**Files:** `src/tui/viz_viewer/state.rs`, `src/tui/viz_viewer/render.rs`

**Effort:** Small. Tab switching + coordinator discovery.

### Phase 5: Iteration Diff
**What:** `d` key on collapsed iteration shows diff view. Compare artifacts,
logs, outputs between iterations.

**Why last:** Nice-to-have for debugging cycle convergence. Lower priority than
basic navigation.

**Files:** `src/tui/viz_viewer/render.rs`, `src/tui/viz_viewer/event.rs`

**Effort:** Medium. Diff computation + overlay rendering.

## 10. Simplicity Evaluation

### What We're NOT Building

- **No timeline scrubber.** Scrubbers suggest continuous time; epochs are
  discrete. `Shift+↑/↓` is simpler and more precise.
- **No global temporal focus.** "Show me everything from the last hour" sounds
  useful but cuts across structural boundaries. Per-stream epoch navigation
  is more coherent.
- **No separate archive viewer.** Archived content appears inline when scrolled
  to or searched for — no separate mode/screen.
- **No meta-coordinator view.** Multiple coordinators are shown one at a time
  with tabs. The graph IS the unified view.
- **No database.** Search uses a simple trigram file index. JSONL files remain
  the source of truth.

### Complexity Budget

| Addition | New code estimate | New UI elements |
|---|---|---|
| IterationSnapshot + iteration tag | ~100 lines | 1 (collapsed iterations) |
| Archive boundary | ~50 lines | 1 (boundary marker) |
| Epoch navigation (Shift+↑/↓) | ~80 lines | 0 (keybinding only) |
| Search overlay | ~400 lines | 1 (search bar + results) |
| Coordinator tabs | ~100 lines | 1 (tab bar) |
| Iteration diff | ~200 lines | 0 (reuses existing overlay) |
| **Total** | **~930 lines** | **4 new elements** |

4 new UI elements. ~930 lines of new code spread across 5 phases. Each phase
is independently useful and shippable. The unified abstraction (streams + epochs)
means the patterns learned in Phase 1 apply to all subsequent phases.

### The Knife Test

For each feature, ask: "Would removing this make the system worse?"
- Iteration history: YES — cycles currently lose context, this is a real gap
- Archive boundary: YES — chat grows forever, boundary gives orientation
- Search: YES — archived content becomes inaccessible without it
- Coordinator tabs: MAYBE — only needed when multi-coordinator ships
- Iteration diff: NICE TO HAVE — useful for debugging convergence

Phase 4 can be deferred until multi-coordinator is closer. Everything else
addresses real current pain points.
