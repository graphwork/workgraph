# Phantom Edge Analysis

> **Contributor doc — not required to USE workgraph.** This is a research
> snapshot of how `wg add` validates `--after` blockers. The user-facing
> rules for chat agents creating tasks (always use `--after`, validate
> dependencies exist) are summarized in `wg agent-guide` (bundled with the
> `wg` binary). Read this doc only if you are working on the validation
> code itself.

Research task: `research-phantom-edge`

## 1. Current Validation in `wg add`

**Finding: `wg add` does NOT reject phantom edges. It emits a warning and proceeds.**

When `--after dep1,dep2` is passed, the add command (`src/commands/add.rs:342-362`) validates each blocker inside the `modify_graph` closure:

```rust
// src/commands/add.rs:342-362
for blocker_id in after {
    if blocker_id == &task_id {
        error = Some(anyhow::anyhow!("Task '{}' cannot block itself", task_id));
        return false;
    }
    if workgraph::federation::parse_remote_ref(blocker_id).is_some() {
        // Cross-repo dependency — validated at resolution time, not here
    } else if graph.get_node(blocker_id).is_none() {
        eprintln!(
            "Warning: blocker '{}' does not exist yet (will be treated as unresolved until created)",
            blocker_id
        );
        // Suggest fuzzy match if a close task ID exists
        let all_ids: Vec<&str> = graph.tasks().map(|t| t.id.as_str()).collect();
        if let Some((suggestion, _)) =
            workgraph::check::fuzzy_match_task_id(blocker_id, all_ids.iter().copied(), 3)
        {
            eprintln!("  → Did you mean '{}'?", suggestion);
        }
    }
}
```

The non-existent dependency is stored verbatim in the task's `after` field (`src/commands/add.rs:372`). The warning message is printed to stderr but **does not prevent task creation**.

**Rationale in the code comment:** "will be treated as unresolved until created" — implying this is an intentional design choice to support burst graph construction where a coordinator creates tasks out of topological order.

## 2. Batch/Publish Mechanism

**Finding: A paused/publish mechanism exists and DOES validate dependencies.**

The coordinator prompt (`src/commands/service/coordinator_prompt_fallback.txt:155`) instructs the coordinator to create batch tasks with `--paused --no-place`, then use `wg publish` to atomically unpause the batch.

The `wg publish` command (`src/commands/resume.rs:178-205`) calls `validate_task_deps()` which **hard-fails on dangling dependencies**:

```rust
// src/commands/resume.rs:178-205
fn validate_task_deps(graph: &WorkGraph, task_id: &str, is_publish: bool) -> Result<()> {
    let task = graph.get_task_or_err(task_id)?;
    let mut missing = Vec::new();
    for dep_id in &task.after {
        if workgraph::federation::parse_remote_ref(dep_id).is_some() {
            continue;
        }
        if graph.get_node(dep_id).is_none() {
            // ... fuzzy match suggestion ...
            missing.push(msg);
        }
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "Cannot {} task '{}': dangling dependencies:\n  {}",
            if is_publish { "publish" } else { "resume" },
            task_id,
            missing.join("\n  ")
        );
    }
    Ok(())
}
```

The subgraph variant (`validate_subgraph` at line 208) validates **all tasks in the downstream subgraph**, not just the root task.

**Cross-referencing within a batch:** When the coordinator creates tasks A, B, C where B depends on A, and all are paused, the IDs exist in the graph even though they're paused. So `wg publish` will find them. The publish mechanism is specifically designed to catch phantoms that are typos or references to never-created tasks.

However, note that the coordinator is **not always using paused mode** — the quality pass workflow is described in the fallback prompt but is not enforced. Individual `wg add` calls without `--paused` create immediately-active tasks, and phantom edges in those calls only produce warnings.

## 3. Graph Integrity Checks

**Finding: Multiple layers of detection exist, but none auto-repair.**

### a) `wg check` (`src/check.rs:139-170`, `src/commands/check.rs`)
Runs `check_orphans()` which scans all tasks for dangling `after`, `before`, and `requires` references. Orphan refs are classified as **errors** (not warnings), and `wg check` returns a non-zero exit code when found.

### b) `wg status` (`src/commands/status.rs:531-554`)
Calls `gather_dangling_deps()` which runs `check_orphans()` and filters to `after`-relation orphans. Surfaces them in the status output:
```
⚠ Attention: 2 task(s) have unresolved dependencies:
  task-a → missing-dep (missing)
  Run 'wg check' for details.
```

### c) `wg viz` / DOT/Mermaid output (`src/commands/viz/dot.rs:89-103`)
Renders phantom dependencies as red dashed nodes labeled "⚠ missing-dep (missing)" with red dashed edges. This makes phantom edges visually obvious in graph visualizations.

### d) On service start / coordinator tick
The coordinator (`src/commands/service/coordinator.rs`) uses `ready_tasks_with_peers_cycle_aware()` to determine what to dispatch. There is **no explicit phantom-edge check at service start**. The coordinator relies on the readiness logic to implicitly handle phantoms by treating them as blocking.

## 4. Edge Cases / Failure Modes

### Failure Scenario 1: Permanently blocked task (most common)

`ready_tasks()` (`src/query.rs:264-274`) treats a missing blocker as unsatisfied:

```rust
// src/query.rs:269-274
task.after.iter().all(|blocker_id| {
    graph
        .get_task(blocker_id)
        .map(|t| t.status.is_terminal())
        .unwrap_or(false)  // Missing blocker = NOT satisfied
})
```

A task with a phantom `after` edge will **never become ready**. The coordinator will never dispatch it. Any task that depends on it (transitively) is also permanently blocked.

### Failure Scenario 2: `wg done` silently ignores phantom blockers

`query::after()` (`src/query.rs:419-429`) uses `filter_map`:

```rust
task.after
    .iter()
    .filter_map(|id| graph.get_task(id))
    .filter(|t| !t.status.is_terminal())
    .collect()
```

This **silently skips** non-existent task IDs. So if a user manually runs `wg done task-id`, the phantom blocker won't prevent completion. This creates an inconsistency: `ready_tasks()` blocks on phantoms (preventing dispatch), but `wg done` does not block on phantoms (allowing manual completion).

### Failure Scenario 3: `wg edit --add-after` has no validation

The edit command (`src/commands/edit.rs:111-113`) adds `after` dependencies without any existence check:

```rust
for dep in add_after {
    if !task.after.contains(dep) {
        task.after.push(dep.clone());
    }
}
```

No warning, no fuzzy match suggestion, no validation whatsoever. This is the most permissive entry point for phantom edges.

### Failure Scenario 4: Bidirectional consistency corruption

When `wg add` stores a phantom edge, it also tries to update the blocker's `before` list (`src/commands/add.rs:429-438`):

```rust
for dep in after {
    if let Some(blocker) = graph.get_task_mut(dep)
        && !blocker.before.contains(&task_id)
    {
        blocker.before.push(task_id.clone());
    }
}
```

When the blocker doesn't exist, the `if let Some(blocker)` silently fails, so the forward `after` edge exists but no corresponding `before` edge exists. If the referenced task is created later, the `before` backlink is never retroactively added, leaving the graph's bidirectional invariant broken.

### Failure Scenario 5: `why-blocked` shows phantom as Open

`why_blocked.rs:66-67`:
```rust
let task = graph.get_task(task_id);
let status = task.map(|t| t.status).unwrap_or(Status::Open);
```

A phantom blocker is shown as `Open` (the default), which is misleading — it doesn't exist at all, but the user sees it listed as a blocker with `Open` status.

### Failure Scenario 6: `task_depth()` treats phantoms as depth 0

`graph.rs:1350`: "Returns 0 for unknown task IDs or tasks with no dependencies." When computing depth limits for the guardrails system, a phantom parent contributes depth 0, which can allow tasks to be created at incorrect depths.

## 5. Coordinator Behavior

**Finding: The coordinator creates tasks via individual `wg add` CLI calls, creating a race window for phantom edges.**

The coordinator prompt (`src/commands/service/coordinator_prompt_fallback.txt`) instructs the coordinator to:

1. Use `wg add` for each task
2. For batches (2+ tasks), use `--paused --no-place` and then `wg publish`

However:
- The coordinator runs as an LLM agent executing shell commands sequentially
- Between two `wg add` calls, there's a window where task A references task B (via `--after`) but B hasn't been created yet
- If the coordinator creates A first (with `--after B`), then B is a phantom until the second `wg add B` completes
- If the coordinator crashes or is killed between these calls, the phantom becomes permanent

**The paused/publish workflow mitigates this:** paused tasks are invisible to the dispatch loop, so the phantom window doesn't cause premature blocking. But this relies on the coordinator LLM correctly following the batch protocol.

## Code Paths Where Dependencies Are Set

| Path | File | Validates existence? | Notes |
|------|------|---------------------|-------|
| `wg add --after` | `src/commands/add.rs:342-362` | Warning only (stderr) | Fuzzy match suggestion |
| `wg edit --add-after` | `src/commands/edit.rs:111-113` | **No** | No warning at all |
| `wg link` | `src/commands/link.rs:30-36` | **Yes (hard fail)** | Both tasks must exist |
| `wg publish` | `src/commands/resume.rs:178-205` | **Yes (hard fail)** | Validates at publish time |
| Auto back-edge (cycle) | `src/commands/add.rs:443-460` | Implicit (only if task exists) | Silent no-op if phantom |

## Summary

The phantom edge problem is real and has multiple failure modes. The system has a **warn-but-allow** stance at creation time (`wg add`), a **hard-fail** stance at publish time, and **detection** via `wg check`/`wg status`/`wg viz`. The most significant gap is `wg edit --add-after`, which has no validation at all.

The asymmetric handling in the readiness vs. completion path (phantom blocks dispatch but doesn't block `wg done`) is a design tension: it prevents unsafe premature dispatch but can leave tasks in limbo if the phantom is never resolved.

Key recommendations for the design phase:
1. `wg edit --add-after` should at minimum warn (like `wg add`)
2. Consider whether `wg add` should hard-fail by default with `--allow-phantom` opt-in
3. The `ready_tasks()` vs `query::after()` inconsistency should be resolved
4. Bidirectional consistency (the `before` backlink gap) should be addressed
5. `why-blocked` should clearly label phantom blockers as "missing" rather than defaulting to "Open"
