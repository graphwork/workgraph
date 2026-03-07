# Task Lifecycle Gaps: Restarts, Stale Evals, Supersession

Research produced during task `research-task-lifecycle`, March 2026.

## Background

During the TOCTOU fix work (infra-fix-toctou → toctou-phase1/2/3), several gaps
in workgraph's task lifecycle became visible. The original monolithic task was
abandoned and decomposed into three sequential phases, but the system had no
machinery to handle the resulting orphaned evaluation tasks, lost agent context,
or supersession tracking. This document catalogs each gap with concrete examples
and proposes specific fixes.

---

## Gap 1: Stale Evaluations

### Problem

When a task is abandoned and its work is decomposed into new tasks, any
`.evaluate-*` task created for the abandoned parent keeps retrying against
abandoned output. The coordinator creates eval tasks in
`build_auto_evaluate_tasks()` (`src/commands/service/coordinator.rs:1219`) for
all non-abandoned tasks. However:

1. The eval task's `after` edge points at the abandoned parent.
2. `is_terminal()` returns true for `Abandoned` (`src/graph.rs:204`), so the
   eval task becomes *unblocked* — it looks ready to run.
3. The evaluator reads from `.workgraph/output/{task_id}/` which may be empty
   or contain partial output from the abandoned attempt.

### Concrete Example

The `.evaluate-.verify-flip-toctou-phase1-core` task (status: Failed) was
created to evaluate the FLIP verification of phase1. If the parent task had been
abandoned instead of completed, the eval would still become ready and attempt to
run `wg evaluate run .verify-flip-toctou-phase1-core` against potentially stale
output.

Currently, `build_auto_evaluate_tasks()` at line 1268 filters:
```rust
!matches!(t.status, Status::Abandoned)
```
This prevents *creating new* eval tasks for abandoned tasks, but does **not**
clean up eval tasks that were already created before the parent was abandoned.

### Current Behavior

- `abandon.rs:24` sets status to `Abandoned` but does not touch child tasks
- No cascade logic exists anywhere — grep for `cascade.*abandon` returns nothing
- The eval task stays `Open` and becomes runnable once its blocker is terminal

### Impact

Wasted compute: stale evals consume an agent slot and LLM tokens to evaluate
output that is no longer relevant. In the FLIP pipeline, this can chain —
`.evaluate-.verify-flip-*` evaluating a verification of abandoned work.

---

## Gap 2: Supersession Tracking

### Problem

When a task is abandoned and its work is re-decomposed into new tasks, there is
no link between the old task and its replacements. The old task's eval/verify
tasks become orphaned zombies, and the graph has no way to express "task A was
replaced by tasks B, C, D."

### Concrete Example

The original `infra-fix-toctou` task (if it existed as a single task) would have
been decomposed into `toctou-phase1-core`, `toctou-phase2-command`, and
`toctou-phase3-service`. But there is no `superseded_by` or `replaced_by` field
on the original task, and no `supersedes` field on the new tasks.

In the current graph, there's no way to answer: "What tasks replaced
infra-fix-toctou?" or "What was the original intent behind toctou-phase2?"
without reading log entries manually.

### Current State

The `Task` struct (`src/graph.rs`) has no supersession fields. The `abandon.rs`
command takes an optional `reason` string but nothing structured. The lineage
system (`src/agency/lineage.rs`) tracks role/component ancestry for the agency
system but has no task-level lineage.

### Impact

- Orphaned eval/verify tasks waste compute
- No way to trace task evolution programmatically
- The `wg viz` graph shows disconnected subgraphs where there should be lineage
- Function memory (`src/function_memory.rs`) records `retry_count` per task but
  has no concept of supersession chains

---

## Gap 3: Retry-Aware Evaluation

### Problem

When a task completes after N retries (different agents), the evaluation has no
knowledge of the retry history. The evaluator prompt (`src/commands/evaluate.rs`)
and the evaluation recording (`src/agency/eval.rs`) contain zero references to
`retry_count`. The eval scores only the final output, not the journey.

### What's Missing

1. **Prompt context**: `build_prompt()` in `src/service/executor.rs:359` does
   not include retry_count or previous failure reasons in the prompt given to
   evaluator agents.

2. **Evaluation metadata**: The `Evaluation` struct in `src/agency/types.rs`
   does not store retry_count. The `record_evaluation()` function in
   `src/agency/eval.rs` does not receive it.

3. **Score adjustment**: No mechanism to weight evaluation scores by retry
   effort. A task that took 3 agents to complete may have the same score as one
   that succeeded on the first try.

### Concrete Example

`toctou-phase1-core` has `retry_count: 1` and `toctou-phase2-command` has
`retry_count: 1`. Their evaluations (`.evaluate-toctou-phase1-core`, etc.) were
scored without any knowledge that a previous agent failed on each task.

### Impact

- Evolution decisions based on evaluation data may over-credit agents who
  inherited partially-completed work
- No signal for the auto-evolver about which roles/tradeoffs struggled with
  which task types
- Function memory records retry_count but the evaluation pipeline ignores it

---

## Gap 4: Agent Restart Context

### Problem

When an agent dies and a new one picks up the same task (via retry), the new
agent may redo work the previous agent already did. The prompt system
(`src/service/executor.rs:build_prompt()`) does not include information about
previous attempts.

### What Exists

- `retry.rs:38-41` clears `status`, `failure_reason`, `assigned`, and
  `session_id` but preserves the task's `log` entries and `artifacts`
- The task log contains entries from the previous agent (timestamped, with actor)
- `build_resume_delta()` in `coordinator.rs:324` provides context for
  *resumed* tasks (same agent, woken by message) but not for *retried* tasks
  (new agent after failure)
- The `output/` directory may contain `changes.patch` from the previous attempt
  (captured by `capture_task_output` in `src/agency/output.rs:19`)

### Concrete Example

When `toctou-phase1-core` was retried (retry_count went from 0 to 1), the
second agent (agent-7018) received a fresh prompt. The log entries from the
first agent's attempt were in the task log but NOT in the prompt. The agent had
to independently discover that "add.rs was already converted" by reading the
code.

### What's Missing

1. **Previous attempt summary**: The prompt should include a section like
   "## Previous Attempts" with the failure reason and key log entries from prior
   agents.
2. **Artifact awareness**: If previous agents registered artifacts, the new
   agent should know about them.
3. **Output diff**: The `changes.patch` from the previous attempt (if it exists
   in `output/{task_id}/`) should be mentioned or summarized.

### Impact

- Duplicated work: agents redo investigation/changes that were already done
- Longer task completion times on retries
- Risk of conflicting approaches when the new agent takes a different path

---

## Gap 5: Automatic Stale Detection

### Problem

When a task is abandoned, its child tasks (`.evaluate-*`, `.verify-flip-*`) are
not automatically affected. They remain `Open` and will be dispatched by the
coordinator, wasting agent slots on evaluating or verifying abandoned work.

### Current Behavior

The `abandon` command (`src/commands/abandon.rs:11-63`) only modifies the target
task. It does not:
- Check for `.evaluate-{id}` tasks and abandon them
- Check for `.verify-flip-{id}` tasks and abandon them
- Check for `.respond-to-{id}` tasks and abandon them
- Propagate to any tasks whose only dependency is the abandoned task

The `gc.rs` command will eventually clean up completed internal tasks, but:
- It only runs on explicit `wg gc` invocation
- It respects `--older` age thresholds
- Between gc runs, stale eval/verify tasks consume agent slots

### What Should Happen

Two levels of fix:

**Level 1 (reactive)**: When `wg abandon` is called, automatically abandon all
system tasks (`.evaluate-*`, `.verify-flip-*`, `.assign-*`) that reference the
abandoned task.

**Level 2 (proactive)**: The coordinator's `build_auto_evaluate_tasks()` should
check whether the source task is abandoned *at dispatch time*, not just at
creation time. Add to the ready-task filter in coordinator dispatch:

```rust
// Skip eval tasks whose source is abandoned
if task.id.starts_with(".evaluate-") {
    let source_id = task.id.strip_prefix(".evaluate-").unwrap();
    if let Some(source) = graph.get_task(source_id) {
        if source.status == Status::Abandoned {
            continue;
        }
    }
}
```

### Concrete Example

If `toctou-phase1-core` had been abandoned (instead of completing), the
following tasks would remain open and dispatchable:
- `.evaluate-toctou-phase1-core`
- `.verify-flip-toctou-phase1-core`
- `.evaluate-.verify-flip-toctou-phase1-core`

Each would consume an agent slot to evaluate/verify abandoned work.

### Impact

- Wasted agent slots (at `max_agents` limit, stale tasks block real work)
- Wasted LLM tokens on meaningless evaluations
- Confusing graph state with orphaned internal tasks

---

## Proposed Solutions

### Solution 1: Cascade Abandon for System Tasks

**Files**: `src/commands/abandon.rs`

After setting the target task to Abandoned, scan for system tasks that reference
it and abandon them too:

```rust
// After the main abandon mutation, cascade to system tasks
let prefixes = [".evaluate-", ".verify-flip-", ".assign-", ".respond-to-"];
let cascade_targets: Vec<String> = graph.tasks()
    .filter(|t| {
        prefixes.iter().any(|p| t.id.starts_with(p))
            && t.after.contains(&id.to_string())
            && !t.status.is_terminal()
    })
    .map(|t| t.id.clone())
    .collect();

for target_id in &cascade_targets {
    if let Some(t) = graph.get_task_mut(target_id) {
        t.status = Status::Abandoned;
        t.failure_reason = Some(format!("Parent task '{}' was abandoned", id));
        t.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: format!("Auto-abandoned: parent '{}' was abandoned", id),
            ..Default::default()
        });
    }
}
```

**Priority**: HIGH — prevents wasted compute immediately.

### Solution 2: Supersession Fields on Task

**Files**: `src/graph.rs` (Task struct), `src/commands/abandon.rs`

Add two optional fields to `Task`:

```rust
/// Tasks that this task was replaced by (set on abandon with decomposition)
pub superseded_by: Vec<String>,
/// Task that this task replaces (set on new tasks created as replacements)
pub supersedes: Option<String>,
```

Update the `abandon` command to accept `--superseded-by task1,task2,...` flag.
Update `wg show` and `wg viz` to display supersession links.

**Priority**: MEDIUM — improves traceability but not urgent for correctness.

### Solution 3: Retry Context in Evaluation

**Files**: `src/commands/evaluate.rs`, `src/agency/types.rs`

a) Add `retry_count: u32` to the `Evaluation` struct.
b) Include retry_count in the evaluator prompt (in `evaluate.rs`'s prompt
   construction around line 432).
c) Add a scoring note: "This task required {N} retries. Consider whether the
   final quality reflects the total effort."

**Priority**: MEDIUM — improves evaluation quality for the auto-evolver.

### Solution 4: Previous Attempt Context in Agent Prompt

**Files**: `src/service/executor.rs` (build_prompt and TemplateVars)

Add a new prompt section for retried tasks:

```
## Previous Attempts

This task has been attempted {retry_count} time(s) before.

### Last failure reason
{failure_reason from provenance log}

### Previous agent's log entries
{filtered log entries from prior agents}

### Files modified by previous attempts
{summary from output/{task_id}/changes.patch if exists}
```

This requires:
a) Adding `previous_attempts_info: String` to `TemplateVars`
b) Populating it in the coordinator's task dispatch path when `retry_count > 0`
c) Reading the provenance log (`wg provenance`) for the last `fail` event on
   this task

**Priority**: HIGH — directly reduces duplicated work on retries.

### Solution 5: Coordinator Skip-Abandoned Filter

**Files**: `src/commands/service/coordinator.rs`

In the task dispatch loop (where ready tasks are selected for agent spawn), add
a filter that skips `.evaluate-*` and `.verify-flip-*` tasks whose source task
is abandoned. This is a safety net in addition to Solution 1.

Also add to `build_auto_evaluate_tasks()`:
```rust
// Early return: don't create eval task if source is abandoned
if source_task.status == Status::Abandoned {
    continue;
}
```

This already exists at line 1268 for *creation*, but the filter should also
apply at *dispatch* time (coordinator.rs dispatch loop).

**Priority**: HIGH — defense in depth alongside Solution 1.

---

## Priority Ordering

| Priority | Gap | Solution | Rationale |
|----------|-----|----------|-----------|
| 1 | Gap 5 | Cascade abandon + coordinator filter | Prevents immediate compute waste |
| 2 | Gap 4 | Previous attempt context in prompt | Highest productivity impact |
| 3 | Gap 1 | (Covered by solutions 1+5) | Stale evals eliminated |
| 4 | Gap 3 | Retry-aware evaluation | Improves evolver signal quality |
| 5 | Gap 2 | Supersession fields | Nice-to-have traceability |

### Implementation Order

1. **First**: Solutions 1 + 5 together (cascade abandon + coordinator filter).
   These are small, surgical changes (~50 lines in abandon.rs, ~20 lines in
   coordinator.rs) that eliminate the stale eval/verify problem.

2. **Second**: Solution 4 (previous attempt context). This is a larger change
   touching executor.rs and the prompt pipeline, but has the highest impact on
   agent productivity during retries.

3. **Third**: Solution 3 (retry-aware evaluation). Small change to types.rs and
   evaluate.rs, gated behind the evaluation pipeline's existing patterns.

4. **Fourth**: Solution 2 (supersession fields). Schema change requires
   migration consideration and touches serialization.

---

## Impact on Existing Evaluation/Agency Pipeline

### Cascade Abandon (Solutions 1+5)

- Evaluation tasks that would have run against abandoned work will be
  auto-abandoned, so no evaluation is recorded — this is correct behavior
- The `eval-scheduled` tag on the source task survives, preventing re-creation
  after gc. This is fine: if the task is abandoned, we don't want a new eval
- No impact on the FLIP pipeline for non-abandoned tasks

### Retry Context (Solution 4)

- Adds ~100-500 tokens to the prompt for retried tasks
- No change for first-attempt tasks (retry_count == 0)
- The context scope system (`ContextScope::Task` and above) should include this
  section at all scope levels since retry info is always relevant

### Retry-Aware Evaluation (Solution 3)

- The `Evaluation` struct gains a field; existing evaluations deserialize with
  `retry_count: 0` via `#[serde(default)]`
- Evolution decisions that aggregate evaluation scores can optionally weight by
  retry effort (e.g., penalize roles that consistently need retries)
- Function memory already tracks retry_count per task outcome, so this
  aligns the evaluation pipeline with the existing signal

### Supersession (Solution 2)

- New optional fields with `#[serde(default)]` — backward compatible
- `wg viz` can render supersession edges as dashed lines
- `wg show` can display "Superseded by: task1, task2, task3"
- No impact on coordinator dispatch logic
