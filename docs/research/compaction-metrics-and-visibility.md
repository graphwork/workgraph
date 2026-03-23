# Compaction Metrics and Visibility Surface

*Research task: compact-viz-research*

---

## 1. What Triggers Compaction?

**Current model: cycle-driven** (not timer/ops-threshold).

The old `should_compact()` function (`src/service/compactor.rs:73`) checked tick intervals (`compactor_interval`) and ops growth (`compactor_ops_threshold`), but is **deprecated** — the daemon no longer calls it.

### Current trigger logic (`src/commands/service/mod.rs:1373`)

`run_graph_compaction()` fires **every daemon poll** and checks three gates in order:

1. **`.compact-0` must be graph-ready** (`status=Open` with all `after` deps in terminal state).  
   - Readiness uses `ready_tasks_with_peers_cycle_aware()` — cycle-aware, not simple terminal check.  
   - Implemented at `src/commands/service/mod.rs:1386–1394`.

2. **Token threshold gate** (`src/commands/service/mod.rs:1392–1404`):  
   - Reads `accumulated_tokens` from `CoordinatorState` (persisted at `.workgraph/service/coordinator-state.json`).  
   - Compares against `config.effective_compaction_threshold()` (`src/config.rs:2276`).  
   - Threshold = `compaction_threshold_ratio × model.context_window` (default: 80% of coordinator model's context window).  
   - Falls back to hardcoded `compaction_token_threshold` if model lookup fails.
   - **Example from live state**: `accumulated_tokens=136065`, threshold ≈ 160000 (80% × 200k for opus).

3. **If both gates pass**: marks `.compact-0` as `InProgress`, calls `compactor::run_compaction()`, then marks `Done` (or reverts to `Open` on error).

### Where accumulated_tokens come from

The coordinator agent thread (`src/commands/service/coordinator_agent.rs:688–708`) increments `accumulated_tokens` after every LLM turn:

```
total = input_tokens + output_tokens  (cache_creation NOT counted — prompt-cache bug fix)
cs.accumulated_tokens += total
```

This is reset to `0` after every successful compaction (`src/commands/service/mod.rs:1444–1453`).

---

## 2. What Metrics Are Available During Compaction?

### Data we have today

| Metric | Where stored | Accessible from |
|--------|-------------|-----------------|
| `accumulated_tokens` | `.workgraph/service/coordinator-state.json` | `CoordinatorState::accumulated_tokens` |
| Compaction threshold | `config.effective_compaction_threshold()` | Computed from model registry |
| Progress % | Derived: `(accumulated / threshold) × 100` | Rendered in `wg service status` and TUI status bar |
| `compaction_count` | `.workgraph/compactor/state.json` | `CompactorState::compaction_count` |
| `last_compaction` timestamp | `.workgraph/compactor/state.json` | `CompactorState::last_compaction` |
| `.compact-0` task status | `.workgraph/graph.jsonl` | Task `status` field |
| `.compact-0` task log | `.workgraph/graph.jsonl` | Task `log` entries |
| `.compact-0` `started_at` / `completed_at` | `.workgraph/graph.jsonl` | Task timestamps |
| `.compact-0` `loop_iteration` | `.workgraph/graph.jsonl` | Task `loop_iteration` counter |
| Compaction error count | In-memory only | `compaction_error_count` var in daemon |

### Data we don't have (would need to add)

| Metric | Why it's missing | How to add |
|--------|-----------------|------------|
| Compaction **duration** | `started_at` exists but duration isn't computed/stored | Compute `completed_at - started_at` at completion; store in `CompactorState` |
| Per-compaction token savings | No before/after measurement | Would need to measure prompt length before vs after |
| LLM call progress (streaming) | `run_lightweight_llm_call()` is synchronous; no streaming progress | Add streaming callback or token-count events |
| Error details (current run) | `compaction_error_count` is in-memory, lost on restart | Persist to `CompactorState` |
| `.compact-0` **intra-run** progress | Single LLM call: no sub-steps | N/A — it's a single call (~2 min timeout); progress = waiting→running→done |

---

## 3. Where Does `.compact-0` Live?

### Graph structure (`src/commands/service/mod.rs:1232–1366`)

```
.coordinator-0  ──→  .compact-0  ──→  .coordinator-0  (cycle)
 (unlimited iterations)               (compact-loop tag)
```

- Both tasks created by `ensure_coordinator_task()` on daemon startup.
- `.coordinator-0`: tagged `coordinator-loop`, `max_iterations=0` (unlimited), `after=[".compact-0"]`
- `.compact-0`: tagged `compact-loop`, no `cycle_config`, `after=[".coordinator-0"]`

### Coordinator interaction lifecycle

1. **Coordinator does work** → each LLM turn increments `accumulated_tokens` in `coordinator-state.json`
2. **Coordinator marks done** → `.coordinator-0` transitions Done → `.compact-0` becomes graph-ready (cycle reactivation)
3. **Daemon poll fires** → `run_graph_compaction()` checks gates → marks `.compact-0` InProgress
4. **Single LLM call** → `run_lightweight_llm_call(config, DispatchRole::Compactor, prompt, 120s)`
5. **Success** → writes `.workgraph/compactor/context.md`, marks `.compact-0` Done, resets `accumulated_tokens=0`
6. **Failure** → reverts `.compact-0` to Open (for retry), increments `compaction_error_count`

### State tracking files

| File | Contents |
|------|----------|
| `.workgraph/service/coordinator-state.json` | `accumulated_tokens`, `ticks`, `enabled`, `max_agents` |
| `.workgraph/compactor/state.json` | `last_compaction`, `compaction_count`, `last_ops_count` |
| `.workgraph/graph.jsonl` | `.compact-0` task: `status`, `started_at`, `completed_at`, `loop_iteration`, `log[]` |
| `.workgraph/compactor/context.md` | Output: Rolling Narrative + Persistent Facts + Evaluation Digest |

---

## 4. Current Viz Rendering: Coordinator and Compactor Nodes

### Key insight: these are NOT filtered out

`is_internal_task()` (`src/commands/viz/mod.rs:158`) **exempts** tasks tagged `coordinator-loop` or `compact-loop`. They always appear in the graph viz.

### Node rendering (ASCII viz, `src/commands/viz/ascii.rs`)

Each node renders as:
```
<id>  (<status>[·tokens])[ ⏳delay][ elapsed][ ✉msgs][ annotation][ ↺ (iter N)]
```

For `.compact-0`, this means:
- **id**: `.compact-0` (cyan for in-progress, green for done, etc.)
- **status**: `open`, `in-progress`, `done`
- **loop info**: ` ↺ (iter N)` — where N = `loop_iteration` (compaction count visible in graph)
- **tokens**: none currently (compaction doesn't set `token_usage` on the task)
- **annotation**: none currently (annotations only set for `.assign-*`, `.evaluate-*`, etc.)
- **timestamp**: relative time since `started_at` (when in-progress) or `completed_at` (when done)

### Component sorting: coordinator/compact nodes are "hot"

The WCC sort in `ascii.rs:490–522` checks if a component contains a `coordinator-loop` task with a recent log entry (within 60 seconds). If so, the entire `{.coordinator-0, .compact-0}` component sorts to the **top** of the display.

### TUI status bar compaction counter

The TUI renders a compaction progress indicator in the status bar (`src/tui/viz_viewer/render.rs:5176`):

```
C:136K/160K(85%)
```

- Enabled via `counters=compact` config key
- Color: Red when ≥ 80%, Blue otherwise
- Data: `time_counters.compact_accumulated / compact_threshold`

### Current annotation hooks

`AnnotationInfo` (`src/commands/viz/mod.rs:13–21`) attaches `text` strings after a node's status. Currently populated **only** for active internal pipeline tasks (`.assign-*`, `.evaluate-*`, `.verify-*`, `.flip-*`). The `.compact-0` node receives **no annotation** today.

---

## 5. What Would "Compaction Progress" Look Like?

### Binary: it's a single LLM call

Compaction is **one synchronous LLM call** with a 120-second timeout (`src/service/compactor.rs:136`). There are no sub-steps. The possible states are:

| `.compact-0` status | Meaning |
|---------------------|---------|
| `Open` (after coordinator done) | Waiting to fire — gates not yet met |
| `Open` (below token threshold) | Deferred — `accumulated_tokens < threshold` |
| `InProgress` | LLM call in flight — no further resolution |
| `Done` | Complete — context.md written |
| `Open` (reverted after error) | Failed, will retry next poll |

The **meaningful progress metric** is the **token fill level** (how close accumulated tokens are to the threshold). That's the leading indicator for when compaction will trigger.

---

## Summary: Data We Have vs. Data We Need to Add

### Have today (no code changes needed)

1. `accumulated_tokens` / `threshold` → fill percentage (surfaced in `wg service status` and TUI status bar)
2. `loop_iteration` on `.compact-0` → total compaction count (visible in viz as `↺ (iter N)`)
3. `CompactorState.last_compaction` → last timestamp
4. `CompactorState.compaction_count` → cumulative count
5. `.compact-0` task timestamps → when compaction started/ended (per-run, from graph)

### Need to add

1. **Compaction duration** — compute and store `completed_at - started_at` (ms) in `CompactorState`
2. **Compaction error count persistence** — currently lost on daemon restart (in-memory only)
3. **Token usage on `.compact-0`** — `run_lightweight_llm_call` returns `LlmCallResult.token_usage`; store on the task

---

## Recommendation: Compactor Node Annotation Format

For the `.compact-0` node in the ASCII/TUI viz, the annotation should show the **fill level** (primary signal) and the **compaction count** (for historical context). Since `loop_iteration` already renders as `↺ (iter N)`, the annotation slot should focus on the token fill:

**Proposed annotation text** (appended via `AnnotationInfo`):

```
[⊟ 85%]           # when threshold is configured and approaching (≥ 50%)
[⊟ 136K/160K]     # when in-progress (compaction running)
[✓ compacted]     # briefly after completion (≤ 30s ago)
```

**How to implement**:

The annotation system is a `HashMap<String, AnnotationInfo>` passed to `generate_ascii()`. To add compaction annotations, `generate_graph()` (`src/commands/viz/graph.rs`) needs access to:
- `CoordinatorState.accumulated_tokens`
- `config.effective_compaction_threshold()`
- `.compact-0` task status and `completed_at`

These can be plumbed into `generate_graph_with_overrides()` as additional parameters, or computed inside the graph generation by reading `CoordinatorState` directly from the `workgraph_dir`.

**Priority rank for annotation data**:

1. **Token fill % when `.compact-0` is Open** — most actionable, shows when next compaction fires
2. **"running" indicator when InProgress** — `.compact-0` only shows as InProgress for ~2–30 seconds
3. **Compaction count** — already shown via `↺ (iter N)`, don't duplicate
4. **Last compaction time** — low value in annotation (already in task's `completed_at`)

---

## Validation Checklist

- [x] **Q1 — What triggers compaction**: Cycle-driven. `.compact-0` must be graph-ready **and** `accumulated_tokens ≥ threshold`. References: `src/commands/service/mod.rs:1373`, `src/service/compactor.rs:73`.
- [x] **Q2 — Metrics available**: `accumulated_tokens`, threshold, fill %, `compaction_count`, `last_compaction`, per-run `started_at`/`completed_at` on task. References: `src/commands/service/coordinator-state.json`, `src/service/compactor.rs:38–43`.
- [x] **Q3 — `.compact-0` cycle location**: Forms `{.coordinator-0 → .compact-0 → .coordinator-0}` cycle. References: `src/commands/service/mod.rs:1232–1366`.
- [x] **Q4 — Graph viz rendering**: `.compact-0` always visible, renders with `↺ (iter N)`, no fill annotation today. TUI status bar has `C:NNN/NNN(%)` counter. References: `src/commands/viz/mod.rs:158`, `src/tui/viz_viewer/render.rs:5176`.
- [x] **Q5 — Compaction progress**: Single LLM call, binary waiting/running/done. Progress = token fill level. References: `src/service/compactor.rs:115–153`.
- [x] **Clear distinction between 'have' vs 'need to add'**: See summary table above.
- [x] **Concrete annotation recommendation**: Token fill `[⊟ N%]` when Open, `[⊟ running]` when InProgress.
