# Research: Compaction Timing Data Availability

## 1. Where is the last compaction timestamp stored?

### CompactorState (`src/service/compactor.rs:37-43`)

The primary store is `CompactorState`, persisted at `.workgraph/compactor/state.json`:

```rust
pub struct CompactorState {
    pub last_compaction: Option<String>,  // ISO 8601 RFC 3339 timestamp
    pub last_ops_count: usize,            // Provenance ops at last compaction
    pub last_tick: u64,                   // Coordinator tick at last compaction
    pub compaction_count: u64,            // Total compactions run
}
```

- Written by `run_compaction()` at `src/service/compactor.rs:147-151` after each successful compaction.
- The `last_compaction` field is set to `Utc::now().to_rfc3339()`.

### CoordinatorState (`src/commands/service/mod.rs:254-284`)

The `CoordinatorState` at `.workgraph/service/coordinator-state.json` tracks:

```rust
pub struct CoordinatorState {
    pub accumulated_tokens: u64,  // Tokens since last compaction (resets after compact)
    pub ticks: u64,               // Total coordinator ticks
    pub last_tick: Option<String>, // Timestamp of last tick
    // ... other fields
}
```

- `accumulated_tokens` is incremented by the coordinator agent thread after each LLM turn (`src/commands/service/coordinator_agent.rs:691-701`).
- This is used to gate compaction via token threshold.

### .compact-0 task (graph cycle member)

The `.compact-0` task in the graph has:
- `completed_at`: Set when the compaction task completes its iteration
- `loop_iteration`: Incremented each time the cycle re-activates
- `ready_after`: Set when cycle delay is configured — the ISO 8601 timestamp when the task becomes dispatchable again

Compaction is now **cycle-driven** (not timer-driven). The old `compactor_interval`/`compactor_ops_threshold` config is deprecated (`src/service/compactor.rs:67-72`).

## 2. Where is the cycle-delay configured?

### CycleConfig (`src/graph.rs:8-28`)

The cycle delay lives in the `CycleConfig` struct on cycle header tasks:

```rust
pub struct CycleConfig {
    pub max_iterations: u32,
    pub guard: Option<LoopGuard>,
    pub delay: Option<String>,           // e.g., "30s", "5m", "1h"
    pub no_converge: bool,
    pub restart_on_failure: bool,
    pub max_failure_restarts: Option<u32>,
}
```

- The `delay` field is a human-readable duration string parsed by `parse_delay()` at `src/graph.rs:55-73`.
- Supported units: `s` (seconds), `m` (minutes), `h` (hours), `d` (days).
- Set via `wg add --cycle-delay` or `wg edit --cycle-delay`.
- Only present on the **cycle header** task (the one that owns the `CycleConfig`).

### How delay is applied (`src/graph.rs:1377-1385`)

When a cycle re-activates in `reactivate_cycle()`:

```rust
let ready_after = cycle_config.delay.as_ref().and_then(|d| match parse_delay(d) {
    Some(secs) if secs <= i64::MAX as u64 => {
        Some((Utc::now() + Duration::seconds(secs as i64)).to_rfc3339())
    }
    _ => None,
});
```

This sets `task.ready_after` on the **config owner** task only (line 1401). The `ready_after` field is then checked by `is_time_ready()` in `src/query.rs` to gate dispatch.

## 3. What data is available to compute "time until next compaction"?

### Currently available data

| Data point | Location | Field |
|---|---|---|
| Last compaction timestamp | `.workgraph/compactor/state.json` | `CompactorState.last_compaction` |
| Cycle delay string | `.compact-0` task in graph | `task.cycle_config.delay` |
| `ready_after` timestamp (when next iteration is dispatchable) | `.compact-0` task in graph | `task.ready_after` |
| Current loop iteration | `.compact-0` task in graph | `task.loop_iteration` |
| Task status | `.compact-0` task in graph | `task.status` |
| Accumulated tokens since last compaction | `.workgraph/service/coordinator-state.json` | `CoordinatorState.accumulated_tokens` |
| Token threshold for compaction trigger | Config (computed) | `Config::effective_compaction_threshold()` |
| Compaction progress percentage | `wg status` output | Computed: `accumulated_tokens / threshold * 100` |

### How to compute "time until next compaction"

**If `.compact-0` has a `ready_after` value and status is Open:**
```
time_until_next = ready_after - now
```
This is the direct answer. It's already set when the cycle re-activates with a delay.

**If `.compact-0` is in-progress or done (current iteration running/complete):**
The next compaction will start after the current iteration completes and the cycle re-activates. Time estimate:
```
next_ready_after = last_completed_at + cycle_delay_seconds
time_until_next = next_ready_after - now
```

**If compaction is token-gated (not cycle-delay-gated):**
The `accumulated_tokens / threshold` ratio gives progress percentage. Time estimate would require knowing the token accumulation rate, which is not currently tracked.

### Gaps

1. **No `completed_at` preservation across cycle iterations.** When a cycle reactivates (`reactivate_cycle()` at `src/graph.rs:1398`), `completed_at` is explicitly cleared: `task.completed_at = None`. This means the last-completion timestamp is lost from the task itself once the cycle iterates. The only surviving record is:
   - The `CompactorState.last_compaction` field (compactor-specific, not generic)
   - Log entries on the task (contain timestamps but require parsing)

2. **No generic `last_iteration_completed_at` field on tasks.** There is no dedicated field to record when the most recent cycle iteration completed. The `completed_at` field is meant to be the task's overall completion time, not per-iteration.

3. **Token accumulation rate is not tracked.** Only the current accumulated total is stored, not timestamps of when tokens were accumulated, so you cannot project when the threshold will be reached.

## 4. Generic cycle timing: when did a loop task last complete an iteration?

### Current state

There is **no dedicated field** for this. When a cycle iteration completes and re-activates:
- `completed_at` is cleared (`src/graph.rs:1398`)
- `started_at` is cleared (`src/graph.rs:1397`)
- `loop_iteration` is incremented (`src/graph.rs:1399`)
- A log entry is appended: `"Re-activated by cycle iteration (iteration N/M)"` (`src/graph.rs:1404-1418`)

The only way to recover per-iteration timing is to parse log entries, which is fragile and expensive.

### What would be needed

A new field on `Task`, e.g.:

```rust
/// Timestamp when the current cycle iteration last completed (before re-activation)
pub last_iteration_completed_at: Option<String>,
```

This would be set in `reactivate_cycle()` just before clearing `completed_at`. It would allow:
- Computing "time since last iteration": `now - last_iteration_completed_at`
- Computing "time until next iteration": `last_iteration_completed_at + delay - now`
- Displaying iteration frequency/cadence

### For .compact-0 specifically

`CompactorState.last_compaction` already fills this role for compaction. But for other loop tasks (`.coordinator`, evolve cycles, custom user cycles), there is no equivalent.

## 5. Existing CLI commands that show cycle/coordinator state

| Command | What it shows | Cycle/timing info |
|---|---|---|
| `wg show <task-id>` | Full task details | `loop_iteration`, `cycle_config` (max_iterations, delay, guard), `ready_after`, `completed_at`, `started_at`, timestamps |
| `wg status` | Dashboard overview | Compaction progress: `accumulated_tokens/threshold (%)`, `last_compaction` ago. Service uptime. Agent summary. |
| `wg agents` / `wg agents --alive` | Agent registry | Agent uptime, task assignment, PID, status |
| `wg service status` | Same as `wg status` | Same as above |
| `wg cycles` | Detected cycle topology | Cycle members, back-edges, iteration counts. No timing data. |
| `wg viz` | ASCII dependency graph | Shows status icons but no timing |
| `wg list --status <status>` | Task listing | Shows task status, no timing |
| `wg ready` | Tasks ready for dispatch | Includes `not_before`/`ready_after` check but doesn't show countdown |

### What's missing from CLI

1. **No "time until next compaction/iteration" display.** `wg status` shows compaction token progress but not the time-based delay countdown.
2. **No cycle-specific timing view.** `wg cycles` shows topology but not when each cycle last iterated or when it will next iterate.
3. **No countdown for `ready_after`.** `wg show` displays the raw `ready_after` timestamp but does show a countdown (`format_countdown()` at `show.rs:447`). This is actually already implemented — it shows "(in 2h 15m)" or "(elapsed)".

## 6. TUI views that show coordinator info

### Coordinator tab bar (`src/tui/viz_viewer/state.rs`)

The TUI has a multi-coordinator tab bar:
- `active_coordinator_id: u32` — currently viewed coordinator
- `coordinator_chats: HashMap<u32, ChatState>` — per-coordinator chat state
- `coordinator_active: bool` — whether the coordinator service is running
- Tab bar shows coordinator IDs, with [+] to add and [×] to remove

### StatusBar (`src/tui/viz_viewer/state.rs:1102+`)

The `StatusBar` struct includes:
- `show_compact: bool` — whether to display compaction info
- `compact_accumulated: u64` — accumulated tokens
- `compact_threshold: u64` — threshold

### Detail panels

- Chat panel: Shows coordinator conversation history with streaming
- Task detail panel: Shows selected task info (same data as `wg show`)

### What's missing from TUI

1. **No compaction countdown/timing in status bar.** The status bar shows token progress but not time-based information.
2. **No "last compaction: Xm ago" in TUI.** The `wg status` CLI command shows this, but the TUI does not surface it.
3. **No per-cycle iteration timing.** The TUI can show task `ready_after` but doesn't compute or display "next iteration in X".

## Summary: Data Sources for Timing

### Exists and is sufficient
- `CompactorState.last_compaction` — last compaction timestamp (`.workgraph/compactor/state.json`)
- `CycleConfig.delay` — cycle delay duration (on cycle header task in graph)
- `Task.ready_after` — when a delayed cycle task becomes dispatchable (on task in graph)
- `CoordinatorState.accumulated_tokens` / `Config::effective_compaction_threshold()` — token progress
- `Task.loop_iteration` — current iteration number
- `Task.completed_at` — completion timestamp (but only valid for current iteration)

### Gaps that need to be filled
1. **`last_iteration_completed_at` on Task** — Generic field for when a cycle task's most recent iteration completed. Currently cleared on reactivation. Needed for all loop tasks (not just .compact-0).
2. **Token accumulation rate** — No timestamps on token accumulation, so time-to-threshold can't be projected. Lower priority since cycle-driven compaction is the primary model now.

### CLI commands needing changes
- `wg status` — Add cycle timing info (last iteration time, next iteration countdown)
- `wg show` — Already shows `ready_after` with countdown; could add "last iteration completed" if field is added
- `wg cycles` — Add per-cycle timing column (last completed, next due)

### TUI views needing changes
- **Status bar** — Add "last compaction: Xm ago" and optionally "next: in Ym"
- **Task detail panel** — Show cycle iteration timing when viewing cycle tasks
- **Coordinator tab** — Could show compaction timing inline
