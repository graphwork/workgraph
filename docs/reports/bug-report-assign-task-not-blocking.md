# Bug Report: `.assign-*` Task Not Wired as Blocking Dependency Before Target Task

## Summary

When a task is spawned via the coordinator and no `.assign-*` task exists yet, the
defense-in-depth code path in `execution.rs` creates the `.assign-*` task at spawn time
as an audit trail — but does **not** add it to the source task's `after` list, and creates
it already in `Done` status. This means the assignment runs concurrently with (or after)
the task agent spawn, defeating the purpose of assignment gating.

## Root Cause

There are **two code paths** that create `.assign-*` tasks, and they have different wiring behaviors:

### Path 1: `scaffold_assign_task` in `eval_scaffold.rs` (correct)

**File:** `src/commands/eval_scaffold.rs:287-339`

This is the proper scaffolding path, called from `build_auto_assign_tasks` Phase 1 in the
coordinator. It correctly:

1. Creates the `.assign-*` task with `status: Open` and `before: vec![task_id]`
2. Adds `.assign-*` to the source task's `after` list (line 327-330), creating the
   blocking dependency
3. The source task is now blocked until `.assign-*` completes

```rust
// eval_scaffold.rs:310-331 — correct wiring
let assign_task = Task {
    id: assign_task_id.clone(),
    // ...
    status: Status::Open,
    before: vec![task_id.to_string()],
    // ...
};
graph.add_node(Node::Task(assign_task));

// Add blocking edge: source task depends on .assign-*
if let Some(source) = graph.get_task_mut(task_id)
    && !source.after.iter().any(|a| a == &assign_task_id)
{
    source.after.push(assign_task_id.clone());
}
```

### Path 2: Defense-in-depth in `execution.rs` (buggy)

**File:** `src/commands/spawn/execution.rs:591-626`

This is the defense-in-depth path that runs at spawn time when no `.assign-*` task exists.
It has two problems:

**Problem A:** The `.assign-*` task is created with `status: Done` and identical
`created_at`, `started_at`, `completed_at` timestamps. It's born dead — it never gates anything.

**Problem B:** The source task's `after` list is **never updated**. The `.assign-*` task
has `before: vec![task_id_str.clone()]`, but the source task doesn't have a reciprocal
`after` edge. Without the `after` edge on the source task, the source task is never
blocked by the `.assign-*` task.

```rust
// execution.rs:606-626 — missing source.after wiring, born-Done status
graph.add_node(Node::Task(Task {
    id: assign_task_id,
    // ...
    status: Status::Done,                    // <-- born Done, never gates
    before: vec![task_id_str.clone()],       // <-- has before edge...
    created_at: Some(now.clone()),
    started_at: Some(now.clone()),
    completed_at: Some(now),                 // <-- instant completion
    // ...
}));
// NOTE: no source.after.push(assign_task_id) — blocking edge not wired
```

### The Race Condition

The specific race sequence that triggers this:

1. Tasks are created with `--paused`
2. A quality pass (or other mechanism) assigns an agent AND resumes tasks
3. In the coordinator tick, `build_auto_assign_tasks` Phase 1 runs and scans for ready
   unassigned tasks — but the task already has an agent (pre-assigned by quality pass),
   so the scaffolding condition may not fire
4. Phase 2 finds no Open `.assign-*` tasks to process
5. `spawn_agents_for_ready_tasks` finds the task is ready with an agent assigned
6. The spawn code in `execution.rs` creates `.assign-*` as an audit trail (born Done)
7. The agent starts simultaneously — assignment was never a gate

## Observed Behavior

- `.assign-pi-role-swap` was created at spawn time with log: "Created at spawn time (no prior .assign-* task existed)"
- It has **no functional blocking edge** to `pi-role-swap`
- `pi-role-swap` was spawned at the exact same timestamp (20:29:48.706) as `.assign-pi-role-swap` was created AND completed
- The assignment task completed instantly (created = started = completed, all same timestamp)
- Agent identity was chosen simultaneously with dispatch, not before it

## Expected Behavior

- `.assign-pi-role-swap` should have a `before: pi-role-swap` edge AND `pi-role-swap` should have `.assign-pi-role-swap` in its `after` list
- `pi-role-swap` should be blocked until `.assign-*` completes
- The coordinator should create the `.assign-*` task BEFORE the target becomes ready, not at spawn time
- When a quality pass pre-assigns an agent and resumes a task, the `.assign-*` scaffolding should still happen during Phase 1, before the spawn decision

## Reproduction Steps

1. Create a batch of tasks with `--paused`:
   ```bash
   wg add "Task A" --paused
   wg add "Task B" --paused
   ```
2. Pre-assign agents and resume tasks (simulating a quality pass):
   ```bash
   wg edit task-a --agent <agent-hash>
   wg resume task-a
   ```
3. Start the coordinator:
   ```bash
   wg service start
   ```
4. Observe that `.assign-task-a` is created at spawn time (in `execution.rs`) with:
   - `status: Done`
   - No `after` edge on the source task
   - Same timestamp for created/started/completed

## Files to Investigate

| File | Lines | Description |
|------|-------|-------------|
| `src/commands/spawn/execution.rs` | 591-626 | Defense-in-depth `.assign-*` creation (buggy path) |
| `src/commands/eval_scaffold.rs` | 287-339 | `scaffold_assign_task` (correct path) |
| `src/commands/service/coordinator.rs` | 831-910 | `build_auto_assign_tasks` Phase 1 scaffolding |
| `src/commands/service/coordinator.rs` | 3078-3085 | Spawn guard that skips unassigned tasks |

## Suggested Fix

### Option A: Fix the defense-in-depth path (minimal)

Make the `execution.rs` code path wire the `after` edge on the source task, matching what
`scaffold_assign_task` does. The `.assign-*` is still born Done (it's an audit trail at
this point), but the graph is at least structurally consistent.

### Option B: Prevent reaching the defense-in-depth path (proper)

Ensure `build_auto_assign_tasks` Phase 1 always scaffolds `.assign-*` for tasks that are
about to become ready, even when they already have a pre-assigned agent. The Phase 1 check
should look at whether the task has an `.assign-*` task, not whether it has an agent. This
way the defense-in-depth path in `execution.rs` is truly never needed for normal operation.

Specifically in `coordinator.rs` Phase 1, the ready-task filter should include tasks that
have been pre-assigned an agent but lack an `.assign-*` task — currently it may skip them
because the task already has an `agent` field set.

### Option C: Both (recommended)

Apply Option B to fix the root cause (scaffolding always happens in Phase 1), and keep
Option A as true defense-in-depth in `execution.rs` for any edge cases that slip through.
