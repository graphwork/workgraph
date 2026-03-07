# Task Self-Decomposition: Agents Spawning Subtasks as Own Blockers

**Task:** research-task-self
**Date:** 2026-03-07

---

## 1. Problem Statement

Large tasks are the #1 source of stuck agents and wasted compute. Evidence:

- **toctou-phase2-command**: 3 agents died, 4th burned 57M+ tokens on a 9-file migration that could have been 9 parallel single-file tasks.
- Agents running into context limits have no graceful exit — they either loop until killed or produce incomplete work.

The solution: agents should be able to decompose their own task into subtasks at runtime, making those subtasks blockers on the parent. The coordinator then dispatches subtask agents, and when all complete, a finalization pass runs on the parent.

---

## 2. Current State Analysis

### 2.1 What Already Works

**`wg done` blocks on incomplete dependencies** (`src/commands/done.rs:148-178`):
```rust
let blockers = query::after(graph, id);
// ... filters cycle-aware blockers ...
if !effective_blockers.is_empty() {
    anyhow::bail!(
        "Cannot mark '{}' as done: blocked by {} unresolved task(s):\n{}",
        id, effective_blockers.len(), blocker_list.join("\n")
    );
}
```
This is the critical safety net — an agent that creates subtask blockers cannot accidentally mark the parent done before subtasks complete.

**`wg edit --add-after` transitions in-progress tasks to Blocked** (`src/commands/edit.rs:363-408`):
When new dependencies are added to an in-progress task and those deps aren't done, the task is moved to `Blocked` status and the agent assignment is cleared. This is exactly the "recall" behavior needed for self-decomposition.

**Prompt already teaches decomposition** (`src/service/executor.rs:170-218`):
The `AUTOPOIETIC_GUIDANCE` section tells agents when/how to decompose. The `ETHOS_SECTION` encourages growing the graph. Pattern keyword glossary triggers for relevant vocabulary.

**Guardrails exist** (`src/config.rs:135-165`):
- `max_child_tasks_per_agent`: Default 35 tasks per session
- `max_task_depth`: Default 8 levels deep

### 2.2 What's Missing

1. **No `wg decompose` convenience command** — Agents must manually `wg add` + `wg edit --add-after`, which is error-prone and requires multiple commands.

2. **Agent gets killed, not gracefully suspended** — When `wg edit --add-after` transitions the parent to `Blocked`, the current agent is still running. It doesn't know it's been recalled. It might continue working, wasting tokens, or try `wg done` and get an error.

3. **No finalization-pass guidance** — The prompt doesn't distinguish between "first run" and "finalization pass after subtasks completed". A finalization agent might redo all the work instead of integrating subtask results.

4. **No detection of tasks that should decompose** — The coordinator doesn't suggest decomposition for tasks that keep failing or consuming excessive tokens.

---

## 3. Proposed Changes

### 3.1 New `wg decompose` Command

A single atomic command that replaces the multi-step `wg add` + `wg edit --add-after` pattern.

**Usage:**
```bash
wg decompose <parent-task-id> \
  --subtask "Migrate file_a.rs" -d "..." \
  --subtask "Migrate file_b.rs" -d "..." \
  [--finalize-description "Integrate: verify all files compile together"]
```

**Behavior:**
1. Creates all subtask nodes in the graph
2. Adds each subtask ID to the parent's `after` list
3. If parent is `InProgress`, transitions it to `Open` (not `Blocked` — `Open` with unmet deps won't be dispatched by `ready_tasks()`)
4. Clears parent's `assigned` field so it gets re-dispatched later
5. If `--finalize-description` is provided, updates the parent's description to include finalization guidance
6. Logs the decomposition event on the parent task
7. All done atomically in a single `mutate_workgraph` call

**Why a single command matters:**
- Atomicity: no race between `wg add` and `wg edit --add-after`
- Discoverability: agents see `wg decompose` in the prompt
- Error prevention: validates subtask IDs, prevents circular deps, enforces guardrails in one pass

**Implementation:** New file `src/commands/decompose.rs`. Internally calls `mutate_workgraph` once, creating subtask `Task` nodes and modifying the parent's `after` list. Reuses existing `generate_id()` from `add.rs` and guardrail checks.

### 3.2 Graceful Agent Exit After Decomposition

When an agent decomposes its own task, it should exit cleanly rather than continuing to run.

**Proposed approach — `wg decompose` returns a signal:**

The `wg decompose` command prints a clear message:
```
Decomposed 'big-task' into 3 subtasks: sub-1, sub-2, sub-3
Task 'big-task' is now blocked. Exit your session — the coordinator will re-dispatch when subtasks complete.
```

The agent prompt should instruct: "After running `wg decompose`, immediately run `wg fail <task-id> --reason 'Decomposed into subtasks'` or simply exit. Do NOT continue working."

**Why `wg fail` is wrong:** Failed tasks get retried, which is the correct behavior — when subtasks complete, the parent becomes unblocked and the coordinator re-dispatches it. But the "failed" framing is confusing.

**Better: new status or convention:**

Option A — **Use existing Blocked status**: `wg decompose` already transitions to a state where `ready_tasks()` won't dispatch. When subtasks complete, parent becomes ready again. The agent just needs to exit. No new status needed.

Option B — **`wg yield` command**: A new command that says "I've done partial work, please re-dispatch me later." Sets status to `Open`, clears assignment. Different from `fail` (no retry counter increment) and `done` (not finished).

**Recommendation: Option A.** The existing `Blocked` → `Open` transition when deps complete is exactly right. The agent should:
1. Run `wg decompose`
2. Log what it did: `wg log <task-id> "Decomposed into N subtasks, exiting for finalization pass"`
3. Exit (the wrapper script will see status is no longer `InProgress` and skip the auto-done/fail logic)

### 3.3 Finalization Pass Detection

When the parent task is re-dispatched after subtasks complete, the agent needs to know it's doing a finalization pass, not starting from scratch.

**Current mechanism — already partially works:**
- `build_task_context()` in `src/commands/spawn/context.rs:16-67` already includes artifacts and logs from dependency tasks (the `after` list)
- When subtasks are in the parent's `after` list and they're `Done`, their last 5 log entries and artifact paths are injected into the parent's prompt context

**Proposed enhancement — explicit finalization marker:**

Add a `decomposed` tag to the parent task when `wg decompose` runs. Then in prompt assembly (`src/service/executor.rs`), detect this tag and inject a finalization preamble:

```
## Finalization Pass

This task was previously decomposed into subtasks. You are now doing the FINALIZATION pass.
Your job is NOT to redo the work — it's to:
1. Review subtask artifacts and logs (included below)
2. Integrate results (resolve conflicts, verify consistency)
3. Run final validation (cargo build, cargo test)
4. Mark the task done

The subtasks already completed the heavy lifting. Focus on integration, not reimplementation.
```

**Implementation:** In `build_prompt()`, check if the task has the `decomposed` tag. If so, include the finalization section and ensure subtask artifacts/logs are prominently displayed.

### 3.4 Updated Agent Prompt — Self-Decomposition Guidance

Add to `AUTOPOIETIC_GUIDANCE` in `src/service/executor.rs`:

```
### Self-Decomposition (blocking yourself on subtasks)

If your task is too large, you can decompose it into subtasks that block your own task:

\`\`\`bash
# Create subtasks and block yourself on them
wg decompose {{task_id}} \
  --subtask "Part 1: migrate file_a.rs" -d "..." \
  --subtask "Part 2: migrate file_b.rs" -d "..."

# Log what you did and exit
wg log {{task_id}} "Decomposed into subtasks, exiting for finalization pass"
\`\`\`

After `wg decompose`, your task becomes blocked. Exit immediately — the coordinator
will dispatch agents for each subtask. When all subtasks complete, your task becomes
ready again and a new agent will do the finalization pass (integrating results, running
final tests).

**Signs you should decompose:**
- Task involves 3+ independent files or modules
- You're worried about context window limits
- The work has clearly separable phases with no file overlap
```

### 3.5 Coordinator-Side Detection (Optional / Future)

The coordinator could detect tasks that might benefit from decomposition:

**Heuristic triggers:**
- Task has failed 2+ times with token-exhaustion or context-limit errors
- Agent has been running for 30+ minutes with no `wg done` or `wg fail`
- Task description mentions 5+ distinct files

**Action:** Send a message via `wg msg`:
```
wg msg send <task-id> "This task has failed twice. Consider decomposing it with 'wg decompose'. See the subtask examples in your prompt."
```

**Recommendation:** Defer this to a follow-up task. Manual self-decomposition by agents is the first priority. Coordinator auto-detection is an optimization.

---

## 4. Edge Cases

### 4.1 Agent Dies Mid-Decomposition

**Scenario:** Agent runs `wg decompose` but crashes before exiting cleanly.

**Analysis:** `wg decompose` is atomic (single `mutate_workgraph` call). Either:
- It completes: subtasks exist, parent is blocked. Coordinator dispatches subtasks. When they complete, parent becomes ready. Dead agent detection cleans up the parent's `assigned` field.
- It doesn't complete: no graph changes. Parent remains in-progress. Dead agent detection eventually marks it failed, and retry re-dispatches it.

**Verdict:** Already safe. No special handling needed.

### 4.2 Subtask Failure

**Scenario:** One of the subtasks fails.

**Analysis:** `ready_tasks()` considers both `Done` and `Failed` as terminal states (via `status.is_terminal()`). So if a subtask fails, the parent becomes unblocked. The `wg done` check in `done.rs:149-178` uses `query::after()` which filters to non-terminal tasks — so `Done` AND `Failed` blockers are considered resolved.

This means the parent will be re-dispatched even if a subtask failed. The finalization agent should check subtask statuses and either:
- Retry the failed subtask: `wg retry <subtask-id>`
- Work around the failure
- Fail the parent: `wg fail <parent-id> --reason "Subtask X failed"`

**Proposed addition to finalization prompt:** "Check subtask statuses. If any subtask failed, decide whether to retry it, work around it, or fail the parent task."

### 4.3 Circular Dependencies

**Scenario:** Agent creates subtask that depends on the parent (A → B → A).

**Analysis:** `wg add` already prevents self-blocking (`src/commands/add.rs:241-243`). The `wg decompose` command creates subtasks that the parent depends on (parent `--after` subtask), not subtasks that depend on the parent. No circular dependency is created.

If an agent manually tries to create a cycle (subtask `--after parent`), this creates a legitimate cycle that requires `--max-iterations`. The `wg decompose` command should NOT allow this — subtasks should only flow INTO the parent.

### 4.4 Nested Decomposition (Subtask Decomposes Further)

**Scenario:** Subtask agent decides its own work is too large and decomposes again.

**Analysis:** This works naturally — the subtask becomes blocked on sub-subtasks. The parent remains blocked (its subtask isn't done yet). When sub-subtasks complete, the subtask gets a finalization pass, completes, and then the parent gets its finalization pass.

**Guardrail:** `max_task_depth` (default 8) prevents infinite nesting. Each decomposition level adds +1 depth. An agent 8 levels deep will get an error from `wg add`'s depth check.

### 4.5 Race Condition: Agent Continues Working After Decomposition

**Scenario:** Agent runs `wg decompose`, which transitions the task to Blocked, but the agent doesn't exit and keeps working.

**Analysis:** The agent's work won't be saved via `wg done` because `done.rs` checks for unresolved blockers. The agent will either:
- Get an error from `wg done` and realize it should exit
- Keep working uselessly until timeout kills it

**Mitigation:** The `wg decompose` command should print a clear exit instruction. The prompt guidance should emphasize exiting immediately. The wrapper script's `wg show --json` status check will see `blocked` instead of `in-progress` and skip auto-completion.

### 4.6 Many Subtasks Modifying Same Files

**Scenario:** Agent decomposes a task into subtasks that all need to edit the same file.

**Analysis:** This is the scatter-then-merge problem. If subtasks run in parallel and edit the same file, they'll overwrite each other.

**Mitigation:** The prompt already warns: "When NOT to decompose: The subtasks would all modify the same files (serialize instead)." For `wg decompose`, we could add validation that prints a warning if subtask descriptions reference overlapping file paths. But this is hard to automate reliably.

**Recommendation:** Rely on agent judgment and prompt guidance. Add a note: "If subtasks must modify the same file, chain them sequentially: `--subtask 'Part 1' --after part-0`."

---

## 5. Example End-to-End Workflow

### Scenario: 9-file TOCTOU migration

**Step 1: Agent is dispatched for big-task**
```
Agent receives task: "Migrate all 9 command files from load_graph+save_graph to mutate_workgraph"
```

**Step 2: Agent realizes the task is too large**
The agent sees 9 independent files that can be migrated in parallel.

**Step 3: Agent decomposes**
```bash
wg decompose big-task \
  --subtask "Migrate src/commands/done.rs to mutate_workgraph" \
  --subtask "Migrate src/commands/edit.rs to mutate_workgraph" \
  --subtask "Migrate src/commands/add.rs to mutate_workgraph" \
  --subtask "Migrate src/commands/fail.rs to mutate_workgraph" \
  --subtask "Migrate src/commands/retry.rs to mutate_workgraph" \
  --subtask "Migrate src/commands/claim.rs to mutate_workgraph" \
  --subtask "Migrate src/commands/abandon.rs to mutate_workgraph" \
  --subtask "Migrate src/commands/link.rs to mutate_workgraph" \
  --subtask "Migrate src/commands/resume.rs to mutate_workgraph"
```

Output:
```
Created 9 subtasks: migrate-src-commands-done-rs, migrate-src-commands-edit-rs, ...
Task 'big-task' is now blocked on 9 subtasks.
Exit your session — the coordinator will dispatch subtask agents.
```

**Step 4: Agent exits**
```bash
wg log big-task "Decomposed into 9 parallel single-file migration tasks. Exiting for finalization pass."
# Agent exits
```

**Step 5: Coordinator dispatches 9 parallel agents**
Each agent migrates a single file. Fast, focused, no context overflow.

**Step 6: All 9 subtasks complete**
big-task becomes ready again.

**Step 7: Finalization agent is dispatched**
Agent receives the finalization prompt with all 9 subtask artifacts and logs. It:
1. Runs `cargo build` and `cargo test`
2. Resolves any integration issues
3. Runs `wg done big-task`

---

## 6. Implementation Plan

### Phase 1: Core Command (Required)

| Change | File | Effort |
|--------|------|--------|
| New `wg decompose` command | `src/commands/decompose.rs` | Medium |
| CLI integration | `src/cli.rs`, `src/main.rs` | Small |
| Finalization tag + prompt section | `src/service/executor.rs` | Small |
| Update `AUTOPOIETIC_GUIDANCE` | `src/service/executor.rs` | Small |
| Tests | `src/commands/decompose.rs` | Medium |

### Phase 2: Polish (Nice-to-have)

| Change | File | Effort |
|--------|------|--------|
| Coordinator token-exhaustion detection | `src/commands/service/triage.rs` | Medium |
| `wg msg` nudge for failing tasks | `src/commands/service/coordinator.rs` | Small |
| Subtask progress visualization in TUI | `src/tui/` | Medium |

### Phase 3: Validation (Post-implementation)

- Smoke test: create a task, decompose it, verify subtask dispatch and finalization
- Integration test: decompose mid-flight, verify parent transitions correctly
- Edge case test: subtask failure, nested decomposition, depth limit

---

## 7. Alternatives Considered

### Alternative A: No new command — just better prompting

Agents already can `wg add` + `wg edit --add-after`. Just improve the prompt to teach this pattern explicitly.

**Rejected because:** The two-step pattern is fragile. If the agent crashes between `wg add` and `wg edit --add-after`, subtasks exist but the parent isn't blocked on them. The coordinator dispatches them, they complete, but the parent doesn't know about them. Atomicity matters.

### Alternative B: Coordinator auto-decomposition

The coordinator detects large tasks and decomposes them automatically based on heuristics.

**Deferred because:** Hard to decompose tasks without domain knowledge. The agent working on the task is best positioned to decide how to split the work. Auto-decomposition could be a later optimization for repeated failure patterns.

### Alternative C: `wg yield` as a new status

Instead of the agent exiting after decomposition, introduce a `Yield` status that means "I've done partial work, re-dispatch me when conditions change."

**Deferred because:** The existing `Blocked` status already serves this purpose. When subtasks complete, `ready_tasks()` picks up the parent. Adding a new status adds complexity without clear benefit for this use case. May revisit if other "yield" scenarios emerge.

---

## 8. Summary of Proposals

1. **New `wg decompose` command** — Atomic subtask creation + parent blocking in a single call
2. **Finalization prompt injection** — Detect `decomposed` tag, inject integration-focused prompt
3. **Updated agent guidance** — Add self-decomposition section to `AUTOPOIETIC_GUIDANCE`
4. **No new status needed** — Existing `Blocked` → ready flow handles the lifecycle
5. **Edge cases are already covered** — `wg done` blocker check, `max_task_depth`, dead agent detection, atomic mutations
6. **Coordinator detection deferred** — Agent-driven decomposition first, auto-detection later
