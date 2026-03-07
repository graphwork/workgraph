# Atomic Task Addition: Preventing Coordinator Scheduling Races

## Problem Statement

A race condition exists between task creation/configuration and coordinator dispatch:

1. User runs `wg add "Task X"` — task is created as Open with no deps
2. User runs `wg edit task-x --add-after dep-y` — intending to add a dependency
3. Between steps 1 and 2, the coordinator sees an Open task with no blockers, considers it ready, and spawns an agent
4. The agent runs with unmet dependencies, producing incorrect or wasted work

**Concrete incident:** `cli-wg-activity` was published with no deps, then `--add-after toctou-phase2-command` was added too late — the coordinator had already dispatched it.

### Existing Mitigations

The codebase already has several partial mitigations:

- **Draft mode** (`main.rs:331-342`): Interactive `wg add` creates tasks as `paused=true` by default. Agents use `--immediate` mode. This means interactively-created tasks are invisible to the coordinator until `wg publish`.
- **Grace period** (`coordinator.rs:777-793`): `auto_assign_grace_seconds` (default: 10s) delays auto-assignment for newly created tasks. But this only applies to the assignment phase, not to direct spawn.
- **Live dep enforcement** (`edit.rs:363-408`): When `--add-after` adds deps to an in-progress task, the task is paused (set to Blocked) and the agent assignment is cleared.
- **Atomic claim** (`spawn/execution.rs:258-358`): `mutate_graph()` holds flock during claim, preventing two spawns from racing on the same task.

### Gap Analysis

The mitigations are **necessary but insufficient**:

| Scenario | Protected? | Mechanism |
|----------|-----------|-----------|
| Interactive add → edit deps → publish | Yes | Draft mode (paused) |
| Agent creates task with `--after` inline | Yes | `--immediate` + deps in same `wg add` |
| Agent creates task, then adds deps | **NO** | Agent tasks are `--immediate` (not paused) |
| Interactive add with `--immediate`, then add deps | **NO** | Explicitly skipped draft mode |
| Task published, then new dep added | **Partial** | Live dep enforcement catches in-progress tasks, but not the window between Open and spawn |

The critical gap: **between `wg add --immediate` (or `wg publish`) and the next `wg edit --add-after`, the coordinator can spawn the task.** The `auto_assign_grace_seconds` only covers auto-assignment, not direct spawning. Even if it did, 10 seconds is arbitrary — the user might take longer to type the next command.

---

## Proposals

### 1. Atomic Publish (Recommended — Primary Fix)

**Proposal:** Require all deps upfront in `wg add`, or use draft→publish for multi-step configuration. Enforce this by adding a **spawn grace period** to `spawn_agents_for_ready_tasks`, not just auto-assign.

#### Concrete Changes

**A. Extend grace period to spawn (not just auto-assign)**

Currently `auto_assign_grace_seconds` only gates `build_auto_assign_tasks`. Apply the same grace to `spawn_agents_for_ready_tasks`:

```rust
// coordinator.rs — spawn_agents_for_ready_tasks()
// After the alive-agent check, before spawning:
if grace_seconds > 0 {
    if let Some(ref created_str) = task.created_at {
        if let Ok(created) = created_str.parse::<chrono::DateTime<chrono::Utc>>() {
            let age = Utc::now().signed_duration_since(created);
            if age.num_seconds() < grace_seconds as i64 {
                eprintln!(
                    "[coordinator] Skipping spawn for '{}': created {}s ago (grace: {}s)",
                    task.id, age.num_seconds(), grace_seconds,
                );
                continue;
            }
        }
    }
}
```

File: `src/commands/service/coordinator.rs` (inside `spawn_agents_for_ready_tasks`, ~line 1729)

**B. Rename config to `dispatch_grace_seconds`**

Since the grace period now applies to both assignment and spawn, rename:
- `auto_assign_grace_seconds` → `dispatch_grace_seconds`
- Default: 10 (unchanged)

File: `src/config.rs` (~line 903)

**C. Agent-created tasks also respect draft mode when deps are omitted**

When an agent creates a task without `--after` or `--independent`, make it paused (draft) even in agent context:

```rust
// main.rs — wg add command
let effective_paused = if paused {
    true
} else if immediate {
    false
} else if std::env::var("WG_AGENT_ID").is_ok() {
    // Agent context: immediate only if deps are specified or --independent
    after.is_empty() && !independent  // paused if no deps and not explicitly independent
} else {
    true  // Interactive: always draft
};
```

This ensures agent-created tasks without explicit deps are paused until `wg publish` or `wg edit --add-after` + `wg publish`.

**Tradeoff:** Most agent-created tasks already include `--after`. The few that don't would need `--independent` to opt out of draft mode. This is the safest default.

### 2. Explicit Independence Flag

**Proposal:** Add `--independent` flag to `wg add`. Tasks must specify either `--after X` or `--independent`. If neither is provided, the task is created in draft mode (paused).

#### Concrete Changes

**A. Add `--independent` CLI flag**

```rust
// cli.rs — Add command
#[arg(long)]
/// Mark task as explicitly having no dependencies (skip draft mode)
independent: bool,
```

File: `src/cli.rs` (~line 152)

**B. Validation in `wg add`**

```rust
// add.rs — run()
if after.is_empty() && !independent && !paused {
    // No deps and not explicitly independent: warn and default to paused
    eprintln!("Warning: task has no dependencies. Use --independent to confirm, or --after to set deps.");
    // Task will be created paused (draft mode)
}
```

**C. `wg publish` already validates deps**

The existing `publish` command (`resume.rs:16-19`) validates that all `after` references exist. No change needed.

**Assessment:** This is a UX improvement but doesn't close the race by itself. Combined with Proposal 1, it eliminates the "I forgot to add deps" scenario.

### 3. Spawn-Time Dep Re-check

**Proposal:** The coordinator should re-verify all deps are met at spawn time inside the `mutate_graph` closure, not just check task status.

#### Current Behavior

`spawn_agent_inner` (`spawn/execution.rs:271-297`) re-checks:
- Task status (Open/Blocked)
- Task assignment (not already assigned)

It does **not** re-check whether all `after` deps are satisfied.

#### Concrete Change

```rust
// spawn/execution.rs — inside mutate_graph closure (~line 271)
mutate_graph(&graph_path, |g| -> Result<()> {
    let t = g.get_task_mut_or_err(&task_id_owned)?;

    // Re-check status under the lock
    match t.status {
        Status::Open | Status::Blocked => {}
        Status::InProgress => {
            anyhow::bail!("Task '{}' was claimed by another agent", task_id_owned);
        }
        other => {
            anyhow::bail!("Task '{}' changed to {:?} before claim", task_id_owned, other);
        }
    }
    if t.assigned.is_some() {
        anyhow::bail!("Task '{}' already assigned", task_id_owned);
    }

    // NEW: Re-check all deps are satisfied under the lock
    for dep_id in &t.after {
        let dep_done = g.get_task(dep_id)
            .map(|d| d.status.is_terminal())
            .unwrap_or(false);
        if !dep_done {
            anyhow::bail!(
                "Task '{}' has unsatisfied dep '{}' (added after readiness check)",
                task_id_owned, dep_id
            );
        }
    }

    t.status = Status::InProgress;
    // ... rest of claim logic
    Ok(())
})
```

File: `src/commands/spawn/execution.rs` (~line 271)

**Assessment:** This is a defense-in-depth measure. It catches the case where deps are added between the coordinator's readiness check and the actual spawn. The `mutate_graph` flock ensures atomicity — if `wg edit --add-after` is running concurrently, either the dep addition or the spawn will go first, but not both.

**Cost:** One extra iteration over `task.after` during claim. Negligible performance impact.

**Recommendation:** Implement regardless of other proposals. This is the only proposal that eliminates the race with zero user workflow changes.

### 4. Post-Publish Dep Addition (Live Enforcement)

**Proposal:** When deps are added to a published/active task, the task should be immediately blocked and any running agent should be recalled.

#### Current Behavior

`edit.rs:363-408` already handles this for **in-progress** tasks:
- Sets status to Blocked
- Clears `assigned`
- Clears `agent` field
- Abandons the `.assign-*` task

But it does NOT handle **Open** (not-yet-spawned) tasks that have been published. For Open tasks, adding a dep just adds it to the `after` list — the `ready_tasks()` check in the next coordinator tick will correctly see it as blocked. This is actually fine because:

1. If the task is Open and not yet spawned → next tick will see the new dep and not spawn it
2. If the task is Open and being spawned concurrently → Proposal 3 (spawn-time re-check) catches this
3. If the task is InProgress → existing live enforcement handles it

#### Concrete Change: Kill running agent on dep addition

The current live enforcement pauses the task but doesn't kill the running agent. The agent continues executing with stale context until it finishes or times out. Enhancement:

```rust
// edit.rs — after setting status to Blocked (~line 391)
if !unmet_deps.is_empty() {
    // ... existing Blocked/unclaim logic ...

    // Kill the running agent if one exists
    if let Some(ref assigned_agent) = original_assigned {
        if let Ok(registry) = workgraph::service::registry::AgentRegistry::load(dir) {
            if let Some(agent) = registry.agents.get(assigned_agent) {
                if agent.is_alive() && workgraph::service::is_process_alive(agent.pid) {
                    eprintln!("Killing agent {} (PID {}) — task deps changed", assigned_agent, agent.pid);
                    let _ = workgraph::service::kill_process_graceful(agent.pid, 5);
                }
            }
        }
    }
}
```

File: `src/commands/edit.rs` (~line 390)

**Assessment:** This is aggressive but correct. An agent working on a task with unmet deps is wasting compute. Graceful kill (SIGTERM + 5s, then SIGKILL) gives the agent time to checkpoint. The task will be retried when deps are satisfied.

**Alternative:** Send a message to the agent via `wg msg` instead of killing. But this requires the agent to check messages, which is not guaranteed.

### 5. Draft Mode Enforcement

**Proposal:** Editing deps should automatically pause (re-draft) the task, requiring an explicit `wg publish` to make it dispatchable again.

#### Analysis

This is the most conservative approach and would prevent the race entirely, but at a significant UX cost:

- Every `wg edit --add-after` would require a follow-up `wg publish`
- Agents creating subtasks with incremental dep wiring would need extra steps
- Breaks the common pattern: `wg add "X" && wg edit X --add-after Y`

#### Assessment: NOT recommended as a standalone proposal

The workflow burden outweighs the safety benefit, especially since Proposals 1+3 already close the race. However, a **weaker variant** is viable:

**Weaker variant:** Only re-draft if the task is Open and has no agent assigned:
```rust
// Only pause if task is Open (not yet in progress)
if task.status == Status::Open && !task.paused {
    task.paused = true;
    println!("Task re-drafted (deps changed). Run `wg publish {}` when ready.", task_id);
}
```

This catches the case where a user adds deps to an already-published task, but doesn't disrupt in-progress tasks (which are handled by Proposal 4).

---

## Recommended Implementation Plan

### Phase 1: Defense-in-depth (immediate, zero-UX-change)

1. **Spawn-time dep re-check** (Proposal 3) — Add dep re-verification inside the `mutate_graph` closure in `spawn_agent_inner`. This eliminates the TOCTOU window between coordinator readiness check and spawn.

   Files: `src/commands/spawn/execution.rs`

2. **Extend grace period to spawn** (Proposal 1A) — Apply `dispatch_grace_seconds` to `spawn_agents_for_ready_tasks`, not just auto-assign. Rename config field.

   Files: `src/commands/service/coordinator.rs`, `src/config.rs`

### Phase 2: Explicit independence (improved UX)

3. **`--independent` flag** (Proposal 2) — Add flag to `wg add`. When neither `--after` nor `--independent` is provided, task defaults to paused.

   Files: `src/cli.rs`, `src/main.rs`, `src/commands/add.rs`

4. **Agent draft-by-default when no deps** (Proposal 1C) — Agents creating tasks without `--after` get draft mode, requiring `wg publish` or `--independent`.

   Files: `src/main.rs`

### Phase 3: Agent recall on dep change (nice-to-have)

5. **Kill agent on dep addition** (Proposal 4) — Graceful kill when deps are added to in-progress tasks.

   Files: `src/commands/edit.rs`

---

## Summary Table

| Proposal | Closes Race? | UX Impact | Complexity | Recommended? |
|----------|-------------|-----------|------------|-------------|
| 1A. Spawn grace period | Reduces window | None | Low | Yes (Phase 1) |
| 1C. Agent draft-by-default | Yes (agent tasks) | Low | Low | Yes (Phase 2) |
| 2. `--independent` flag | Prevents user error | Low | Low | Yes (Phase 2) |
| 3. Spawn-time dep re-check | **Yes (all cases)** | **None** | **Low** | **Yes (Phase 1)** |
| 4. Kill agent on dep change | Yes (in-progress) | None | Medium | Yes (Phase 3) |
| 5. Draft on dep edit | Yes | High | Low | No (too disruptive) |

**The critical fix is Proposal 3** — it's the only change that closes the race with zero UX impact and handles all edge cases atomically.
