# Feature: Cycle Delay Semantics — `--delay-before` / `--delay-after`

**Date:** 2026-02-24
**Status:** Proposal
**Component:** Cycle iteration (`graph.rs:reactivate_cycle`)

---

## Current Behavior

`--cycle-delay 5m` on a cycle header sets `ready_after = now + 5m` on the config owner task when the cycle re-activates (after the last task in the cycle marks done). This means:

```
iteration N completes
  → evaluate_cycle_iteration() fires
  → all cycle members re-opened immediately
  → config owner gets ready_after = now + delay
  → coordinator waits until ready_after passes to dispatch
```

So the delay is effectively **delay-before** — a pause before the next iteration's header task gets dispatched. The other cycle members are re-opened immediately but can't run because they're blocked by the header.

### Where it happens

`src/graph.rs` lines ~840-861 in `reactivate_cycle()`:

```rust
let ready_after = cycle_config
    .delay
    .as_ref()
    .and_then(|d| match parse_delay(d) {
        Some(secs) if secs <= i64::MAX as u64 => {
            Some((Utc::now() + Duration::seconds(secs as i64)).to_rfc3339())
        }
        _ => None,
    });

// ...
if *member_id == config_owner_id {
    task.ready_after = ready_after.clone();
}
```

Note: only the config owner gets `ready_after`. Other members are re-opened without delay.

---

## Problem

The current `--cycle-delay` has ambiguous semantics:

1. **"Delay between iterations"** — when does it start? After the last task completes? Before the first task dispatches? These are different in practice if evaluation/re-activation has overhead.

2. **No delay-after option** — sometimes you want a cooldown *after* completion before even evaluating whether to iterate. Use case: a monitoring loop that checks every 5 minutes — you want the delay after the check completes, not before it starts.

3. **Only delays the config owner** — in a multi-task cycle (A→B→C→A), only the header task gets delayed. If the first task to run after re-activation is not the config owner, it starts immediately.

---

## Proposal

Replace `--cycle-delay` with explicit `--delay-before` and `--delay-after` (or keep `--cycle-delay` as alias for `--delay-before` for backwards compat).

### `--delay-before <duration>`

Pause before dispatching the next iteration. Current behavior — `ready_after` on cycle header after re-activation.

**Use cases:**
- Rate-limited API calls (don't hammer immediately)
- Human review loops (give time to review before next pass)
- Resource cooldown (let infra recover between iterations)

### `--delay-after <duration>`

Pause after the cycle completes before evaluating whether to iterate. This would mean:

```
iteration N completes
  → wait delay-after
  → evaluate_cycle_iteration()
  → if iterating: re-open members immediately (or with delay-before if set)
```

**Use cases:**
- Monitoring loops — check every 5 minutes means "5 minutes after the check finishes"
- Polling patterns — wait-then-check, not check-then-wait
- Convergence checks — let system settle before re-evaluating

### Combined

Both can be set simultaneously:

```
iteration N completes
  → wait delay-after
  → evaluate guard + max_iterations
  → if iterating: re-open members
  → config owner gets ready_after = now + delay-before
  → coordinator dispatches after delay-before passes
```

---

## Data Model Changes

```rust
pub struct CycleConfig {
    pub max_iterations: u32,
    pub guard: Option<LoopGuard>,
    /// Delay before dispatching next iteration (existing field, renamed internally)
    pub delay_before: Option<String>,
    /// Delay after completion before evaluating whether to iterate (new)
    pub delay_after: Option<String>,
}
```

For backwards compat, keep `delay` as serde alias for `delay_before`:

```rust
#[serde(alias = "delay")]
pub delay_before: Option<String>,
```

### CLI

```
wg add "Monitor" --after check --max-iterations 100 --delay-after 5m
wg add "Write"   --after review --max-iterations 3  --delay-before 30s
wg add "Poll"    --after eval   --max-iterations 50  --delay-before 10s --delay-after 5m
```

Keep `--cycle-delay` as deprecated alias for `--delay-before`.

---

## Implementation Notes

### delay-after is trickier

`delay-before` is simple: set `ready_after` on re-activation. The coordinator naturally respects it.

`delay-after` requires delaying the *evaluation* itself. Options:

1. **Deferred evaluation in coordinator tick** — when a cycle-eligible task completes, don't evaluate immediately. Instead, record `cycle_evaluate_after = now + delay` on the task. The coordinator checks this on each tick and only calls `evaluate_cycle_iteration()` when the time passes.

2. **Immediate evaluation with ready_after on all members** — evaluate immediately but set `ready_after` on all re-opened tasks (not just config owner). Semantically different but simpler.

3. **Timer-based** — spawn a background timer that fires evaluation after delay. More complex, probably unnecessary since coordinator already polls.

Option 1 is cleanest — it preserves the semantic difference (delay-after delays *evaluation*, delay-before delays *dispatch*) and fits naturally into the coordinator's tick loop.

### New field on Task

```rust
/// Timestamp after which cycle iteration should be evaluated (for delay-after)
#[serde(skip_serializing_if = "Option::is_none")]
pub cycle_evaluate_after: Option<String>,
```

---

## Migration

- Existing `delay` field in CycleConfig becomes `delay_before` with serde alias
- `--cycle-delay` CLI flag becomes deprecated alias for `--delay-before`
- No breaking changes to stored graphs (alias handles old format)
