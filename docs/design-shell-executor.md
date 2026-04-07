# Design: Shell Executor + Retry-Loop Pattern

**Task:** design-shell-executor
**Date:** 2026-04-07
**Status:** Proposed
**Depends on:** [research-shell-executor](research/shell-executor-and-retry-patterns.md)

---

## Executive Summary

The shell executor already exists and works. The main work is (1) adding `--exec` to
`wg add` for ergonomic task creation, (2) exempting shell tasks from the agency pipeline,
and (3) documenting the cycle-based retry pattern so agents and users can use it fluently.

No new execution model is needed. The design composes existing primitives: `task.exec`,
`exec_mode: "shell"`, `CycleConfig`, and the coordinator's auto-routing logic.

---

## 1. Shell Executor for Tasks

### 1.1 How a User Specifies a Shell Command

**Current state:** Users must create a task then set the exec command separately:
```bash
wg add "Run TB trials batch 1"
wg exec --set run-tb-trials-batch-1 "python3 run_trials.py --batch 1"
```

**Proposed:** Add `--exec` flag to `wg add`:
```bash
wg add "Run TB trials batch 1" --exec "python3 run_trials.py --batch 1"
```

Behavior when `--exec` is provided:
- Sets `task.exec = Some(cmd)`
- Auto-sets `task.exec_mode = Some("shell")` unless `--exec-mode` is explicitly provided
- The coordinator already auto-routes tasks with `task.exec` or `exec_mode == "shell"` to the shell executor (`coordinator.rs:3159-3167`)

**Optional `--timeout` flag:**
```bash
wg add "Run long simulation" --exec "python3 sim.py" --timeout 4h
```

Sets `task.timeout` (new field) which flows through to `effective_timeout_secs` in the
spawn path. Currently timeout is resolved from executor config or coordinator config;
a per-task timeout adds the top-priority layer.

### 1.2 Stdout/Stderr Capture and Storage

**Already implemented.** The shell executor wrapper script (`execution.rs:985`) redirects
stdout+stderr to `output.log` in the agent output directory:
```
.workgraph/log/agents/<task-id>/<timestamp>/output.log
```

On completion/failure, the wrapper calls `wg done`/`wg fail` based on exit code.
The output is also available through `wg show <task-id>` (log entries) and through
dependency context injection for downstream tasks.

### 1.3 Exit Code Mapping

**Already implemented.** The wrapper script:
- Exit 0 → `wg done "$TASK_ID"` → status = Done
- Non-zero → `wg fail "$TASK_ID" --reason "exit code N"` → status = Failed, retry_count++

### 1.4 Timeout Handling

**Already implemented** at executor and coordinator levels (`execution.rs:418-437`):
- Resolution order: CLI param > executor config > coordinator config (`agent_timeout`)
- Uses `timeout` command with SIGTERM, then SIGKILL after 30s
- Empty string disables timeout

**Proposed enhancement:** Add `task.timeout` field to `Task` struct for per-task override:
- Resolution becomes: task.timeout > executor config > coordinator config
- Expressed as a duration string (e.g., `"30m"`, `"4h"`)
- Set via `--timeout` on `wg add`

---

## 2. Agency Pipeline Exemption

### 2.1 Design Constraint

**Shell executor tasks MUST be exempt from the agency pipeline.** No `.assign-*`, `.flip-*`,
`.evaluate-*` scaffolding should be created for them. They're commands, not agent work — no
agent identity, no FLIP scoring, no evaluation.

Downstream checker tasks (if Claude agents) still get full agency treatment.

### 2.2 Where to Skip

The agency pipeline is invoked at two points:

1. **Publish-time scaffolding** — `scaffold_eval_for_published()` in `resume.rs:288`,
   which calls `scaffold_full_pipeline()` in `eval_scaffold.rs:110`
2. **Coordinator auto-assign** — `build_auto_assign_tasks()` in `coordinator.rs:822`,
   which calls `scaffold_assign_task()` in `eval_scaffold.rs:287`

Both paths need a shell-task check.

### 2.3 Implementation

Add a predicate function in `eval_scaffold.rs`:

```rust
/// Returns true if a task uses the shell executor (command execution, no LLM).
/// Shell tasks are exempt from the agency pipeline — no .assign-*, .flip-*,
/// or .evaluate-* scaffolding.
pub fn is_shell_task(task: &Task) -> bool {
    task.exec.is_some() || task.exec_mode.as_deref() == Some("shell")
}
```

Wire it into both skip paths:

**In `scaffold_full_pipeline()`** (eval_scaffold.rs:117-118):
```rust
// Skip system tasks (unless pipeline-eligible) and dominated-tag tasks
if workgraph::graph::is_system_task(task_id) && !is_pipeline_eligible_system_task(task_id) {
    return false;
}
// NEW: Skip shell executor tasks — they're commands, not agent work
if let Some(task) = graph.get_task(task_id) && is_shell_task(task) {
    return false;
}
```

**In `scaffold_assign_task()`** (eval_scaffold.rs:296):
```rust
// Skip system tasks
if workgraph::graph::is_system_task(task_id) && !is_pipeline_eligible_system_task(task_id) {
    return false;
}
// NEW: Skip shell executor tasks
if let Some(task) = graph.get_task(task_id) && is_shell_task(task) {
    return false;
}
```

**In `build_auto_assign_tasks()` Phase 1** (coordinator.rs:840):
```rust
ready
    .iter()
    .filter(|t| t.agent.is_none() && t.assigned.is_none())
    .filter(|t| !workgraph::graph::is_system_task(&t.id))
    // NEW: exclude shell tasks from auto-assign
    .filter(|t| t.exec.is_none() && t.exec_mode.as_deref() != Some("shell"))
    .map(|t| (t.id.clone(), t.title.clone(), t.created_at.clone()))
    .collect()
```

### 2.4 What the Checker Gets

The downstream checker task (a Claude agent) still gets:
- Full agency pipeline (`.assign-*`, `.flip-*`, `.evaluate-*`)
- Dependency context from the shell task (stdout/stderr via artifacts + logs)
- Previous attempt logs when in a retry cycle

---

## 3. Reset-from-Downstream (Retry Loop) Pattern

### 3.1 The Pattern

A checker task (Claude agent) inspects the output of an upstream shell task and decides:
retry or accept. This naturally maps to workgraph's existing cycle primitives.

### 3.2 Graph Structure

```
┌─────────────────────────────────────────────────────────────┐
│                    Cycle (max_iterations=5)                  │
│                                                             │
│  ┌──────────────┐         ┌──────────────────────────┐     │
│  │  shell-task   │ ──────▶ │  checker-task             │     │
│  │  (exec_mode:  │         │  (Claude agent)           │     │
│  │   shell)      │         │  (cycle_config owner)     │     │
│  │              │         │                          │     │
│  │  Runs:       │         │  Inspects output,        │     │
│  │  python3     │         │  decides:                │     │
│  │  run.py      │         │  • wg done --converged   │     │
│  │              │         │    → cycle stops ✓        │     │
│  │              │         │  • wg done (no flag)     │     │
│  │              │         │    → cycle iterates ↻    │     │
│  │              │         │  • wg fail               │     │
│  │              │         │    → cycle restarts ↺    │     │
│  └──────────────┘         └──────────────────────────┘     │
│         ▲                           │                       │
│         └───────── back-edge ───────┘                       │
└─────────────────────────────────────────────────────────────┘
```

### 3.3 How the Cycle Works

The checker task owns the `cycle_config` (set via `--max-iterations`). When the checker
completes, `evaluate_cycle_iteration()` (graph.rs:1420-1610) runs:

1. **Checker calls `wg done <checker-id> --converged`** → `converged` tag set → cycle stops.
   Both tasks stay Done. Work is accepted.

2. **Checker calls `wg done <checker-id>`** (no `--converged`) → cycle evaluates: all
   members Done, not converged, iterations remain → all members reset to Open, `loop_iteration`
   increments. Shell task re-runs, then checker re-runs.

3. **Checker calls `wg fail <checker-id> --reason "..."`** → `evaluate_cycle_on_failure()`
   (graph.rs:1661-1830+) restarts all members (same iteration, no increment). Respects
   `max_failure_restarts` (default: 3).

4. **Max iterations hit** → cycle stops regardless of convergence. Final state preserved.

### 3.4 No New Commands Needed

The checker does NOT need `wg retry <upstream>`. The cycle mechanism handles reset
automatically. The checker's only job is to report its own status:
- `wg done --converged` = accept
- `wg done` = reject (retry)
- `wg fail` = error (restart)

This is cleaner than direct task manipulation because:
- The cycle respects `max_iterations` — no infinite loops
- All log entries are preserved across resets
- The coordinator handles re-dispatch automatically
- No race conditions from cross-task status manipulation

---

## 4. Log Preservation

### 4.1 Current Behavior (Already Correct)

Logs are **append-only** across all reset paths:

| Reset Path | Log Behavior | Location |
|-----------|-------------|----------|
| `wg retry` | Appends "Task reset for retry" | `retry.rs:63-68` |
| Cycle re-activation | Appends "Re-activated by cycle iteration (N/M)" | `graph.rs:1588-1603` |
| Cycle failure restart | Appends restart entry | `graph.rs:1800-1810` |

### 4.2 Agent Output Archival (Already Correct)

On `wg done` or `wg fail`, agent output is archived to:
```
.workgraph/log/agents/<task-id>/<ISO-timestamp>/
  ├── output.log      # stdout+stderr
  ├── prompt.txt       # prompt sent to agent
  └── stream.jsonl     # streaming events (Claude agents only)
```

Each attempt gets its own timestamped directory. Previous attempt context is injected
into retry agents via `build_previous_attempt_context()` (context.rs:631-714).

### 4.3 What the Checker Sees

When the checker task runs (as a cycle member after the shell task completes), it
receives via dependency context:
- The shell task's **artifacts** (if any registered via `wg artifact`)
- The shell task's **log entries** (all iterations, append-only)
- The shell task's **output.log tail** from the most recent run

The checker can also run `wg show <shell-task-id>` to inspect full task state including
all historical logs and the current `loop_iteration`.

### 4.4 No Changes Needed

The existing log preservation and context injection are sufficient. No new storage
mechanisms required.

---

## 5. Agent UX

### 5.1 Creating a Shell-Task + Checker Cycle

**One-shot creation:**
```bash
# Step 1: Create the shell task
wg add "Run TB trials batch 1" \
  --exec "python3 run_trials.py --batch 1" \
  --timeout 2h

# Step 2: Create the checker task with cycle back-edge
wg add "Check TB batch 1 results" \
  --after run-tb-trials-batch-1 \
  --max-iterations 5 \
  -d "## Description
You are a checker agent. Inspect the output of the upstream shell task
'run-tb-trials-batch-1' and decide whether the results are acceptable.

## What to check
- All 10 trials completed (look for 'Trial N: COMPLETE' in output)
- No trial had error rate > 5%
- Output CSV exists and has the expected columns

## Actions
- If results look good: \`wg done check-tb-batch-1-results --converged\`
- If results need a re-run: \`wg done check-tb-batch-1-results\` (cycle will restart both tasks)
- If something is broken and needs human attention: \`wg fail check-tb-batch-1-results --reason 'describe issue'\`

## Context
You can inspect the shell task's output with: \`wg show run-tb-trials-batch-1\`
The dependency context already includes the task's stdout/stderr."
```

### 5.2 Checking Without a Cycle (One-Shot)

For cases where retry isn't desired:
```bash
wg add "Run migration" --exec "python3 migrate.py --env staging"
wg add "Verify migration" --after run-migration \
  -d "Check that the staging database migration succeeded.
Run \`wg show run-migration\` to inspect output.
Verify row counts match expectations."
```

### 5.3 What the Checker Agent Sees in its Prompt

The checker agent receives a task assignment with:
1. Its own task description (includes instructions on what to check and what commands to run)
2. **Context from dependencies** — automatically injected by the coordinator:
   - `From run-tb-trials-batch-1: artifacts: ...`
   - `From run-tb-trials-batch-1 logs: <tail of output.log>`
3. **Cycle context** — if in a retry loop:
   - Current `loop_iteration` visible via `wg show`
   - Previous attempt logs preserved in task log
4. **Available tools** — full Claude Code session (Read, Grep, Bash, `wg` CLI)

### 5.4 Example: Agent's Decision Logic

```
# As the checker agent, I received context showing:
# - loop_iteration: 2 (this is the 3rd attempt)
# - upstream output.log shows "Trial 7: TIMEOUT"
# - 9/10 trials completed successfully
#
# Decision: retry is worth it (only 1 failure, not systemic)
# Action:
wg done check-tb-batch-1-results
# This triggers cycle re-evaluation → both tasks reset → shell re-runs
```

### 5.5 Workflow Function (Future Enhancement)

A reusable workflow function could wrap the two-step creation:

```bash
wg func apply exec-check-loop \
  --input title="Run TB trials" \
  --input command="python3 run_trials.py --batch 1" \
  --input max_iterations=5 \
  --input checker_description="Verify all trials completed with <5% error rate"
```

This is a nice-to-have, not a blocker. The two-command pattern is simple enough.

---

## 6. Full Workflow Example

### Setup
```bash
# Create a long-running shell task
wg add "Run POVRay render batch" \
  --exec "cd renders && ./run_batch.sh --quality high --frames 1-100" \
  --timeout 6h \
  --tag shell-executor

# Create a checker that forms a cycle with it
wg add "Verify render batch quality" \
  --after run-povray-render-batch \
  --max-iterations 3 \
  -d "## Description
Check the render output for quality issues.

## Steps
1. Run \`wg show run-povray-render-batch\` to see exit code and output
2. Check that renders/output/ contains 100 PNG files
3. Spot-check a few frames for obvious artifacts
4. Verify the quality metrics log shows no frames below threshold

## Actions
- All frames rendered correctly: \`wg done verify-render-batch-quality --converged\`
- Some frames failed (worth retrying): \`wg done verify-render-batch-quality\`
- Systematic failure (needs human): \`wg fail verify-render-batch-quality --reason 'describe'\`"
```

### Runtime Sequence

```
Iteration 0:
  coordinator detects run-povray-render-batch is ready (shell, no deps)
  coordinator spawns: bash -c "cd renders && ./run_batch.sh --quality high --frames 1-100"
  ... 4 hours later ...
  wrapper: exit 0 → wg done run-povray-render-batch
  output.log archived to .workgraph/log/agents/run-povray-render-batch/<ts>/

  coordinator detects verify-render-batch-quality is ready (dep done)
  coordinator spawns Claude agent with full task context
  agent inspects output, finds 3 frames corrupted
  agent: wg done verify-render-batch-quality  (no --converged → retry)

  cycle evaluates: all members Done, not converged, iteration 0 < max 3
  → both tasks reset to Open, loop_iteration incremented to 1

Iteration 1:
  run-povray-render-batch re-runs (fresh shell execution)
  ... renders again ...
  checker re-runs with context including iteration 1 logs
  agent sees all 100 frames good
  agent: wg done verify-render-batch-quality --converged

  cycle evaluates: converged → cycle stops
  both tasks stay Done ✓
```

---

## 7. Code Changes Required

### 7.1 High Priority — `--exec` on `wg add`

| File | Change |
|------|--------|
| `src/cli.rs:46-166` (Add variant) | Add `--exec` field: `exec: Option<String>` |
| `src/commands/add.rs:136` (add fn signature) | Add `exec: Option<&str>` parameter |
| `src/commands/add.rs:~400` (Task construction) | Set `task.exec` and auto-set `exec_mode = "shell"` |
| `src/main.rs` (Add dispatch) | Pass `exec` through to `add::add()` |

**~30 lines of code.** Straightforward plumbing.

### 7.2 High Priority — Agency Pipeline Exemption

| File | Change |
|------|--------|
| `src/commands/eval_scaffold.rs` | Add `is_shell_task()` predicate |
| `src/commands/eval_scaffold.rs:117` | Skip shell tasks in `scaffold_full_pipeline()` |
| `src/commands/eval_scaffold.rs:296` | Skip shell tasks in `scaffold_assign_task()` |
| `src/commands/service/coordinator.rs:840` | Exclude shell tasks from auto-assign Phase 1 |

**~15 lines of code.** All in skip-condition checks.

### 7.3 Medium Priority — Per-Task Timeout

| File | Change |
|------|--------|
| `src/graph.rs:238` (Task struct) | Add `timeout: Option<String>` field |
| `src/cli.rs:46` (Add variant) | Add `--timeout` flag |
| `src/commands/add.rs` | Wire timeout through |
| `src/commands/spawn/execution.rs:418` | Add task.timeout to resolution order |

**~20 lines of code.** The timeout resolution chain already exists; this adds one more layer.

### 7.4 Low Priority — Workflow Function

| File | Change |
|------|--------|
| `.workgraph/functions/exec-check-loop.toml` | New template file |

Convenience only. The two-command pattern works fine without it.

---

## 8. What Does NOT Change

These systems already work correctly and need no modification:

- **Shell executor** (`execution.rs:963-971`) — runs `bash -c <task.exec>`
- **Coordinator routing** (`coordinator.rs:3159-3167`) — auto-detects shell tasks
- **Cycle iteration** (`graph.rs:1420-1610`) — all-done → re-open
- **Cycle failure restart** (`graph.rs:1661-1830+`) — failure → restart members
- **Log preservation** — append-only across all reset paths
- **Previous attempt context** (`context.rs:631-714`) — injected into retry agents
- **`wg exec`** (`exec.rs`) — standalone manual execution
- **`wg retry`** (`retry.rs`) — manual failed→open reset (still available but not needed in cycles)
- **Output archival** — timestamped per-attempt directories

---

## 9. Testing Strategy

### New Tests

```rust
#[test]
fn test_add_with_exec_sets_shell_mode() {
    // wg add "title" --exec "echo hi" should set exec and exec_mode="shell"
}

#[test]
fn test_shell_task_skips_agency_pipeline() {
    // Task with exec set should not get .assign-*, .flip-*, .evaluate-*
}

#[test]
fn test_shell_task_skips_auto_assign() {
    // Shell tasks should be filtered out in coordinator auto-assign Phase 1
}

#[test]
fn test_checker_downstream_gets_agency_pipeline() {
    // A non-shell task depending on a shell task should still get full pipeline
}

#[test]
fn test_shell_checker_cycle_iteration() {
    // Shell task → checker → done (no converged) → both reset to Open
}

#[test]
fn test_per_task_timeout_resolution() {
    // task.timeout takes priority over executor and coordinator config
}
```

### Existing Tests (Must Pass)

All 1528+ existing tests must continue to pass. Key areas:
- `tests/integration_cycle_detection.rs` — cycle detection logic
- Eval scaffold tests in `src/commands/eval_scaffold.rs`
- Coordinator tests in `src/commands/service/coordinator.rs`

---

## 10. Migration / Backwards Compatibility

No migration needed. All changes are additive:
- New `--exec` flag on `wg add` — optional, existing workflows unaffected
- New `task.timeout` field — optional, `skip_serializing_if = "Option::is_none"`
- Shell task pipeline exemption — shell tasks today don't get pipeline scaffolding
  anyway (they complete too fast for the coordinator to scaffold), but the explicit
  skip makes this a guaranteed invariant rather than a race condition
- `serde(default)` on new fields means existing graph.jsonl files load fine

---

## Appendix: Decision Log

**Q: Should we add a new `wg retry-from-checker` command?**
A: No. The existing cycle mechanism handles this cleanly. The checker just calls
`wg done` (without `--converged`) and the cycle auto-resets everything. A new command
would add API surface for no benefit.

**Q: Should the checker directly `wg retry <shell-task>`?**
A: No. Direct cross-task manipulation creates ordering issues and bypasses max_iterations.
The cycle mechanism is the right abstraction — it respects limits, preserves logs, and
the coordinator handles re-dispatch.

**Q: Should shell tasks get their own `--checker` flag that auto-creates the cycle?**
A: Deferred. The two-command pattern (`wg add --exec` + `wg add --after --max-iterations`)
is simple enough. A `--checker` flag or workflow function can be added later if the pattern
proves common enough to warrant it.

**Q: Where does the agency exemption check go — coordinator or eval_scaffold?**
A: Both. `eval_scaffold` handles publish-time scaffolding, coordinator handles runtime
auto-assign. The `is_shell_task()` predicate is defined once in `eval_scaffold.rs` and
used from both call sites.
