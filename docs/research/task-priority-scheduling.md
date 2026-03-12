# Task Priority and Intelligent Coordinator Scheduling

## Problem Statement

The coordinator currently treats all ready tasks as FIFO with no priority awareness. When all agent slots are occupied by long-running tasks (30-minute research, TUI work), a quick 8-second probe task blocks indefinitely. This was observed live: `validate-core-dispatch` created `cycle-probe-step-3` (trivial, ~8s) but all 6 slots were occupied by unrelated work. The parent was effectively stalled by its child waiting in line behind unrelated tasks.

The core issue: **no differentiation between a task that takes 8 seconds and one that takes 30 minutes when deciding who gets the next slot.**

---

## 1. Priority Model: Hybrid Tiers + Niceness

### Recommendation: Named tiers with integer backing

Use **5 named tiers** for human ergonomics, backed by **integer values (0-99)** for fine-grained internal ordering. This avoids the Unix niceness confusion (lower = higher priority) while keeping the system extensible.

| Tier       | Range | Default for                                              |
|------------|-------|----------------------------------------------------------|
| `critical` | 0-9   | Manual escalation only. Reserved for "everything stops." |
| `high`     | 10-29 | System tasks (`.evaluate-*`, `.verify-flip-*`, `.assign-*`), cycle probes, child tasks unblocking a waiting parent |
| `normal`   | 30-59 | User-created tasks (default: 40)                         |
| `low`      | 60-79 | Long-running research/exploration tasks when explicitly marked |
| `background` | 80-99 | Housekeeping, evolution, non-urgent cleanup              |

**Lower number = higher priority** (like process priority, unlike Unix niceness).

### Task struct addition

```rust
// In graph.rs Task struct:

/// Priority value (0-99). Lower = dispatched first.
/// Named tiers: critical(0-9), high(10-29), normal(30-59), low(60-79), background(80-99).
/// Default: 40 (normal).
#[serde(default = "default_priority", skip_serializing_if = "is_default_priority")]
pub priority: u8,
```

### Automatic priority inference

The coordinator should auto-assign priority based on heuristics when no explicit priority is set:

| Pattern | Inferred Priority | Rationale |
|---------|-------------------|-----------|
| System tasks (`.evaluate-*`, `.assign-*`, `.verify-flip-*`, `.respond-to-*`) | 15 (high) | Quick, unblock the pipeline |
| Tasks created by an agent whose parent is Waiting or InProgress | 20 (high) | Parent is blocked; child completion unblocks it |
| Tasks with `exec` command (shell tasks) | 25 (high) | Typically fast, deterministic |
| Cycle probe tasks (tags contain "probe" or "cycle-test") | 20 (high) | Quick validation, blocking cycle completion |
| Tasks with `estimate.hours < 0.1` (~6 min) | 30 (normal-high) | Short tasks benefit from quick turnaround |
| Evolution tasks (`.evolve-*`) | 85 (background) | Non-urgent system maintenance |
| Everything else | 40 (normal) | Default |

The priority field on Task is the **effective priority**: if set explicitly by the user it's authoritative; if not, the coordinator computes it from heuristics at dispatch time.

### CLI syntax

```bash
# Set priority at creation
wg add "urgent fix" --priority high
wg add "background cleanup" --priority low
wg add "fine-grained" --priority 25

# Modify priority
wg edit task-id --priority critical
wg edit task-id --priority 55

# Tier names resolve to their midpoint:
#   critical=5, high=20, normal=40, low=70, background=90
```

Implementation: `--priority` accepts either a tier name or a raw integer (0-99). Names are syntactic sugar.

---

## 2. Coordinator Scheduling Algorithm

### Current flow (Phase 6 in `coordinator_tick`)

```
ready_tasks_with_peers_cycle_aware() → Vec<&Task>  // FIFO, insertion order
    .iter()
    .take(slots_available)
    → spawn each
```

### Proposed flow: Priority-scored dispatch queue

Replace the simple `take(slots_available)` with a **scored sort**:

```rust
fn score_task(task: &Task, graph: &WorkGraph, dir: &Path) -> i64 {
    let mut score: i64 = 0;

    // 1. Priority (dominant factor): invert so lower priority value = higher score
    let effective_priority = task.priority; // already resolved by heuristics
    score += (100 - effective_priority as i64) * 1000; // range: 1000-100000

    // 2. Age boost (starvation prevention): +1 per minute waiting
    if let Some(ref created) = task.created_at {
        if let Ok(created_ts) = created.parse::<DateTime<Utc>>() {
            let age_minutes = Utc::now().signed_duration_since(created_ts).num_minutes();
            score += age_minutes.min(500); // cap at 500 to prevent age from overwhelming priority
        }
    }

    // 3. Critical path bonus: +5000 if task is on the critical path
    // (reuse existing `wg critical-path` logic)
    if is_on_critical_path(task, graph) {
        score += 5000;
    }

    // 4. Parent-child affinity: +3000 if a running task created this one
    //    (detected via: task was added by an agent that is currently InProgress)
    if has_waiting_or_running_parent(task, graph) {
        score += 3000;
    }

    // 5. Estimated duration bonus: short tasks get a small boost
    //    (improves throughput — Shortest Job First element)
    if let Some(ref est) = task.estimate {
        if let Some(hours) = est.hours {
            if hours < 0.1 { score += 2000; }      // < 6 min
            else if hours < 0.5 { score += 1000; }  // < 30 min
        }
    }

    score
}
```

Then in `spawn_agents_for_ready_tasks`:

```rust
let mut scored_ready: Vec<_> = final_ready
    .iter()
    .map(|t| (score_task(t, graph, dir), t))
    .collect();
scored_ready.sort_by(|a, b| b.0.cmp(&a.0)); // highest score first

for (_, task) in scored_ready.iter().take(slots_available) {
    // ... existing spawn logic
}
```

### Score component weights rationale

| Component | Max contribution | Rationale |
|-----------|-----------------|-----------|
| Priority tier | 100,000 | Dominant: critical tasks always beat normal ones |
| Critical path | 5,000 | Significant but doesn't override explicit priority |
| Parent-child affinity | 3,000 | Unblocking a waiting agent is high value |
| Short duration | 2,000 | SJF element for throughput optimization |
| Age (starvation) | 500 | Slow accumulation prevents indefinite starvation |

A `critical` (priority=5) task scores 95,000 from priority alone. A `normal` (priority=40) task that has been waiting 8 hours scores 60,000 + 480 = 60,480. The critical task still wins. But a `normal` task will eventually beat a newly-created `low` task (70 → 30,000 vs 40 → 60,000), so starvation is naturally bounded.

### Why rule-based, not LLM-based

The coordinator already uses LLM calls for triage and assignment (lightweight LLM calls). However, **scheduling should be rule-based**, not LLM-based:

1. **Latency**: Scheduling happens every tick (5s default). LLM calls take 2-5s. Adding per-tick LLM latency doubles the cycle time.
2. **Determinism**: Priority ordering should be predictable. Users/agents need to know "if I mark this critical, it goes next."
3. **Cost**: LLM calls cost money. Scheduling 6 tasks per tick, 12 ticks/minute = 72 LLM calls/minute for scheduling alone.
4. **Simplicity**: The scoring formula above captures all the signals. The coordinator LLM is better used for assignment (agent selection) and triage (task decomposition).

The LLM-assisted coordinator can still **adjust priorities** during triage — e.g., after analyzing the graph state, the coordinator could emit `wg edit task-id --priority high` as an action. But the tick-level dispatch is pure scoring.

---

## 3. Preemption and Slot Reservation

### Recommendation: Reserved slot, no preemption

**Reserve 1 slot for high-priority tasks.**

Modify `max_agents` handling to split into `max_normal_agents` and `reserved_slots`:

```rust
// In coordinator_tick:
let config_max = max_agents;
let reserved = config.coordinator.reserved_priority_slots.unwrap_or(1);
let max_normal = config_max.saturating_sub(reserved);

// Normal tasks can only fill max_normal slots
// High/critical tasks can fill ALL slots (including reserved)
let alive_normal = count_alive_by_priority(registry, graph, "normal+");
let alive_total = count_alive_total(registry);

let normal_slots = max_normal.saturating_sub(alive_normal);
let priority_slots = config_max.saturating_sub(alive_total);

// When dispatching:
// - Tasks with priority < 30 (high/critical): use priority_slots (can use reserved)
// - Tasks with priority >= 30 (normal/low/bg): use normal_slots (cannot use reserved)
```

This means: with `max_agents=6` and `reserved_priority_slots=1`:
- Normal tasks see 5 available slots
- High/critical tasks see all 6
- When all 5 normal slots are full and a high-priority task arrives, it gets the reserved slot immediately

### Why not preemption

Preemption (killing/pausing a running agent) is **not recommended** for v1:

1. **Context loss**: Claude Code agents accumulate context over their session. Killing one means restarting from scratch, wasting the work already done.
2. **Partial writes**: An agent mid-commit or mid-file-edit could leave the workspace in an inconsistent state.
3. **Complexity**: Implementing graceful preemption (send a message asking the agent to checkpoint and yield) requires agents to poll for preemption signals, checkpoint their state, and cleanly exit. This is a large feature.
4. **Reserved slot solves 90%**: The observed problem (probe blocked behind research) is solved by reservation. The probe would get the reserved slot immediately.

### Future: Soft preemption (v2)

If needed later, soft preemption can be implemented using the existing `wg wait` + checkpoint machinery:

1. Coordinator sends `wg msg send <task-id> "PREEMPT: higher priority task needs your slot. Please checkpoint and yield."`
2. Agent (if well-behaved) runs `wg wait --timer 5m` to park itself
3. Coordinator reclaims the slot
4. Agent gets resurrected when a slot opens

This requires no new infrastructure — just agent cooperation. But it's unreliable (agent may ignore the message) and should remain optional.

### Config

```toml
# In .workgraph/config.toml
[coordinator]
reserved_priority_slots = 1  # default: 1 when max_agents >= 4, 0 otherwise
```

---

## 4. Starvation Prevention

### Age-based priority boost

Already incorporated in the scoring algorithm (Section 2): +1 score per minute of age, capped at +500. This means:

- A `background` (priority=90) task accumulates score at 1/min
- After 500 minutes (~8 hours), it has a +500 boost
- Total score: 10,000 + 500 = 10,500
- This is still less than a freshly-created `normal` task (60,000), so explicit priority is respected
- But it will eventually beat other background tasks that arrived later

### Guaranteed minimum throughput

Add a **background flush threshold**: if any task has been ready for longer than `max_ready_age` (configurable, default: 2 hours), the coordinator promotes it to `normal` priority automatically:

```rust
// In coordinator tick, before scoring:
for task in &mut ready_tasks {
    if task.priority >= 60 && task_age_minutes(task) > config.coordinator.max_ready_age_minutes {
        task.effective_priority = 40; // promote to normal
        log!("Promoted {} from {} to normal (starvation prevention)", task.id, task.priority);
    }
}
```

This is a safety net. In practice, the age boost in scoring should be sufficient for most workloads.

---

## 5. CLI and TUI Integration

### CLI changes

**`wg add`:**
```
wg add "task title" --priority high     # named tier
wg add "task title" --priority 25       # raw integer
wg add "task title"                     # default: normal (40)
```

**`wg edit`:**
```
wg edit task-id --priority critical
wg edit task-id --priority 55
```

**`wg ready`** — sort by effective priority (highest first):
```
Ready tasks (by priority):
  [HIGH]   cycle-probe-3 - Probe: validate cycle iteration
  [NORMAL] implement-auth - Implement authentication endpoint
  [LOW]    research-perf - Research: performance optimization
```

**`wg status`** — include priority in summary:
```
Status: 12 tasks (3 done, 2 in-progress, 4 open, 3 blocked)
Priority breakdown (ready): 1 high, 3 normal, 2 low
```

**`wg list --sort priority`** — sort by priority column.

### TUI integration

In the TUI viz viewer:

1. **Color coding**: Priority tiers get distinct colors:
   - Critical: red/bold
   - High: yellow
   - Normal: default (white/gray)
   - Low: dim/gray
   - Background: very dim

2. **Sort order**: In the task list panel, ready tasks sort by effective priority (after current status grouping).

3. **Priority badge**: Show a small badge in the task detail view:
   ```
   [HIGH 20] implement-fix - Fix critical bug in auth
   ```

4. **Node rendering**: In the graph view, priority could affect node border style (thick border for high priority, dashed for background).

---

## 6. Implementation Plan

### Phase 1: Core priority field (small, self-contained)

**Files to change:**
- `src/graph.rs`: Add `priority: u8` field to `Task` struct with default 40
- `src/cli.rs`: Add `--priority` flag to `Add` and `Edit` subcommands
- `src/commands/add.rs`: Wire `--priority` to task creation
- `src/commands/edit.rs`: Wire `--priority` to task modification

**Estimated scope:** ~100 lines changed. No behavioral change yet.

### Phase 2: Priority-aware dispatch

**Files to change:**
- `src/commands/service/coordinator.rs`:
  - Add `score_task()` function
  - Add `infer_priority()` for automatic priority assignment to system tasks
  - Modify `spawn_agents_for_ready_tasks()` to sort by score before spawning
  - Implement reserved slot logic in `coordinator_tick()` Phase 1 / Phase 6
- `src/config.rs`: Add `reserved_priority_slots` to coordinator config

**Estimated scope:** ~200 lines changed. This is the behavioral change.

### Phase 3: CLI display

**Files to change:**
- `src/commands/ready.rs`: Sort output by priority, add tier labels
- `src/commands/status.rs`: Show priority breakdown
- `src/commands/list.rs`: Add `--sort priority` option
- `src/commands/show.rs`: Display priority in task details

**Estimated scope:** ~100 lines changed.

### Phase 4: TUI (optional, can be deferred)

**Files to change:**
- `src/tui/viz_viewer/render.rs`: Color coding for priority
- `src/tui/viz_viewer/state.rs`: Sort by priority in task list

### Phase 5: Starvation prevention

**Files to change:**
- `src/commands/service/coordinator.rs`: Add age-based promotion logic
- `src/config.rs`: Add `max_ready_age_minutes` config

---

## 7. Interaction with Existing Systems

### Auto-assign

The assignment LLM call already selects agents and sets exec_mode/context_scope. Priority is **orthogonal** — it determines *when* a task gets a slot, not *who* runs it. No changes needed to assignment logic.

### Cycle iteration

When a cycle re-activates tasks (Phase 2.5/2.6 in `coordinator_tick`), the re-activated tasks should **inherit their original priority**. Cycle probes that were originally high-priority stay high-priority across iterations.

### Evaluation / verification / evolution

System tasks (`.evaluate-*`, `.verify-flip-*`, `.evolve-*`) get automatic priority via inference (Section 1). Evaluation and verification are quick and should be dispatched promptly (high). Evolution is background work (background priority).

### `ready_tasks_with_peers_cycle_aware`

This function in `src/query.rs` returns ready tasks in graph insertion order. The scoring layer in `spawn_agents_for_ready_tasks` re-orders them. No change needed to the query function itself — the ordering responsibility moves to the dispatch layer.

---

## 8. Design Decisions Summary

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Priority model | Hybrid: named tiers + integer 0-99 | Human-friendly + machine-precise |
| Default priority | 40 (normal) | Safe middle ground |
| Scheduling | Rule-based scoring | Fast, deterministic, cheap |
| Preemption | No (v1), soft preempt via messaging (v2) | Context loss risk too high |
| Reserved slots | 1 slot reserved for high/critical | Simple, solves observed problem |
| Starvation | Age boost (+1/min, cap 500) + promotion at 2h | Bounded, predictable |
| Coordinator intelligence | Rules for dispatch, LLM for priority adjustment during triage | Best of both worlds |
