# Bare Coordinator Design

## Overview

The "bare coordinator" is the minimal execution unit in the workgraph system. It manages task lifecycle, self-compacts its own context, and enforces the invariant that it never spawns tasks to manage its own state.

## Bare Coordinator Invariant

**Core Rule**: A coordinator NEVER spawns tasks to manage its own state.

This fundamental invariant means:
- No `.compact-*` tasks spawned by the coordinator itself
- No helper tasks for state management
- Self-contained operations only

## Coordinator Lifecycle State Machine

```
START → RUN → COMPACT-IN-PLACE → RESUME → EXIT
  │      │           │              │
  └──────┴───────────┴──────────────┘
         (can exit from any state)
```

### States

| State | Description |
|-------|-------------|
| START | Initialize, load journal if exists |
| RUN | Normal operation, process tasks |
| COMPACT-IN-PLACE | Self-compacting (no task spawning) |
| RESUME | After compaction, resuming |
| EXIT | Clean shutdown |

### Transitions

| From | To | Trigger |
|------|-----|---------|
| START | RUN | Initialize complete |
| RUN | COMPACT-IN-PLACE | Context pressure threshold |
| RUN | EXIT | Task complete or shutdown |
| COMPACT-IN-PLACE | RESUME | Compaction complete |
| COMPACT-IN-PLACE | EXIT | Compaction failed |
| RESUME | RUN | Resume complete |
| Any | EXIT | Fatal error |

## Journal-Based Self-Compaction

See `DESIGN-journal-based-coordinator-self-compaction.md` for full details.

Key principles:
- Self-contained: Compaction within coordinator process
- No task spawning: Coordinator compacts its own context
- Journaled: State changes for crash recovery
- Reentrant: Resume from any compaction point

### Trigger Conditions
- Context exceeds threshold (default 80% of limit)
- Manual via SIGUSR1
- Scheduled

### Algorithm
1. Write COMPACT_START to journal
2. Build summary of recent operations
3. Identify removable entries
4. Truncate oldest, keep summaries
5. Write compacted context
6. Update journal position
7. Write COMPACT_COMPLETE
8. Continue execution

## Signal/Exit/Resume Protocol

### Signals

| Signal | Action |
|--------|--------|
| SIGTERM | Graceful shutdown |
| SIGUSR1 | Trigger compaction |
| SIGUSR2 | Dump state |

### Clean Exit
1. Receive shutdown signal
2. Set should_exit flag
3. Write EXIT_REQUESTED to journal
4. Complete in-flight operations
5. Write final checkpoint
6. Exit with code 0

### Crash Recovery
1. Check for existing journal
2. Read last entry, validate
3. Reconstruct state
4. Truncate to recovery point
5. Resume from checkpoint

## Deprecation Plan for .compact-* Tasks

### Migration Phases

1. **Phase 1**: Feature flag WG_BARE_COORDINATOR=1
2. **Phase 2**: New coordinators opt into bare mode
3. **Phase 3**: Default switch to bare mode
4. **Phase 4**: Disable old mode
5. **Phase 5**: Remove deprecated code

### Invariant Enforcement

```rust
fn spawn_task_for_self(&self, task_type: &str) -> Result<TaskId, CoordinatorError> {
    Err(CoordinatorError::InvariantViolation(
        format!("Coordinator cannot spawn {} tasks", task_type)
    ))
}
```

## Configuration

```yaml
coordinator:
  bare_mode: true
  compaction:
    trigger_threshold: 0.8
    journal_enabled: true
  signals:
    sigterm_action: graceful_shutdown
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| WG_BARE_COORDINATOR | 0 | Enable bare mode |
| WG_COMPACTION_THRESHOLD | 0.8 | Trigger threshold |
| WG_JOURNAL_PATH | .workgraph/coordinator.journal | Journal path |

## File Inventory

| File | Purpose |
|------|---------|
| src/coordinator/mod.rs | Main coordinator |
| src/coordinator/state.rs | State machine |
| src/coordinator/journal.rs | Journal persistence |
| src/coordinator/compaction.rs | Self-compaction |
| src/coordinator/signals.rs | Signal handling |
| docs/design/bare-coordinator.md | This document |

## Verification

- All state transitions validated
- Journal recovery tested
- Compaction reduces without data loss
- Signals handled gracefully
- Invariant enforced at runtime