# Research: Shell Executor and Task Reset/Relaunch Patterns

**Task:** research-shell-executor  
**Date:** 2026-04-07

---

## 1. Shell Executor: What Exists Today

The shell executor is a **fully implemented, built-in executor type**. It runs a task's `exec` field as a bash command.

### Implementation Locations

**Default config** — `src/service/executor.rs:1210-1226`  
The `ExecutorRegistry::default_config("shell")` returns:
```rust
ExecutorConfig {
    executor: ExecutorSettings {
        executor_type: "shell",
        command: "bash",
        args: ["-c", "{{task_context}}"],
        env: { TASK_ID: "{{task_id}}", TASK_TITLE: "{{task_title}}" },
        ..
    }
}
```

**Inner command build** — `src/commands/spawn/execution.rs:963-971`  
For shell executor, the spawned command is simply:
```
bash -c <task.exec>
```

**Coordinator auto-detection** — `src/commands/service/coordinator.rs:3159-3167`  
The coordinator automatically routes tasks to the shell executor when:
- `task.exec` is `Some(...)`, OR
- `task.exec_mode == Some("shell")`

**Validation gate** — `src/commands/spawn/execution.rs:207-209`  
Shell executor requires `task.exec` to be set; if missing, it bails with "no exec command for shell executor".

**`wg exec` command** — `src/commands/exec.rs:26-163`  
A standalone CLI command that runs a task's `exec` field directly (outside the service/coordinator flow). It:
1. Claims the task (`Open` → `InProgress`)
2. Runs `sh -c <exec_cmd>`
3. Marks `Done` (exit 0) or `Failed` (non-zero), incrementing `retry_count` on failure
4. Logs stdout/stderr and exit code

**`wg exec --set`** — `src/commands/exec.rs:513-538`  
Sets `task.exec` on an existing task. Also `clear_exec` at line 541.

**exec_mode "shell"** — `src/graph.rs:311-316`  
The `exec_mode` field on a task controls tool access tier. `"shell"` means "no LLM, run task.exec command directly."

### What the Shell Executor Does NOT Do

- It does not assemble or inject a prompt (no LLM involved)
- It does not set up worktree isolation (no branch needed for non-code tasks)
- Stdout/stderr are captured in `output.log` via the standard wrapper script (`write_wrapper_script` at `src/commands/spawn/execution.rs:985`)
- The wrapper script still calls `wg done`/`wg fail` on behalf of the shell command based on exit code

---

## 2. Task Reset Mechanisms

### `wg retry` — `src/commands/retry.rs:12-104`

Resets a **Failed** task to `Open`:
- Only works on Failed tasks (line 33)
- Checks `max_retries` limit (line 43-53)
- Preserves `retry_count` (does NOT increment — already incremented by the failing agent)
- Clears: `failure_reason`, `assigned`, `converged` tag
- Adds log entry: "Task reset for retry (attempt #N)"
- Does NOT clear `log` — **all previous log entries are preserved**

### Cycle Iteration Reset — `src/graph.rs:1420-1610`

`evaluate_cycle_iteration()` is called by `wg done` when a task completes. If all cycle members are Done:
1. Checks convergence tag, `max_iterations`, guard conditions
2. Re-opens all Done members: sets `status = Open`, clears `assigned`, `started_at`, `completed_at`
3. Increments `loop_iteration` on all members
4. Preserves `completed_at` as `last_iteration_completed_at`
5. Adds log entry: "Re-activated by cycle iteration (iteration N/M)"
6. **Logs are preserved** — only appended to, never cleared

### Cycle Failure Restart — `src/graph.rs:1661-1830+`

`evaluate_cycle_on_failure()` is called by `wg fail` when a task in a cycle fails:
1. If `restart_on_failure` is true (default), re-opens ALL cycle members
2. Does NOT increment `loop_iteration` (same iteration is retried)
3. Increments `cycle_failure_restarts` counter
4. Respects `max_failure_restarts` (default: 3)
5. **Logs are preserved** — only appended to

### Can a Task Reset ANOTHER Task Programmatically?

**Yes — via CLI commands.** An agent can run:
- `wg retry <other-task-id>` — resets a Failed task to Open
- `wg fail <other-task-id> --reason "..."` — marks another task as Failed

There is no built-in API for "reset task X from task Y's code" beyond calling wg CLI commands.

---

## 3. Cycle Support for the Checker Pattern

### Current Cycle Mechanics

Cycles are supported in two modes (both in `src/graph.rs:1403-1467`):

1. **SCC cycles (explicit back-edges)**: Tasks form a cycle detected via Tarjan's SCC algorithm. All members must reach Done before the cycle iterates.

2. **Implicit cycles**: A task has `cycle_config` (set via `--max-iterations`) and depends on upstream tasks via `--after`. When the config owner completes, it treats itself + its `after` deps as a virtual cycle.

### Can a Checker Task Reset Its Predecessor?

**Yes, via cycles.** The natural pattern:

```
command-task → checker-task ──(back-edge)──→ command-task
                  (cycle_config on checker, max_iterations=N)
```

When the checker marks itself as Done (not converged), the cycle evaluates:
- If not converged and iterations remain → all cycle members reset to Open
- The command-task re-runs, checker re-runs after it

When the checker is satisfied → `wg done --converged` stops the cycle.

If the checker determines the command failed → `wg fail` triggers `evaluate_cycle_on_failure`, which restarts all members (same iteration, no increment).

**This pattern already works today** for LLM-based checkers. The gap is for shell-based command tasks.

---

## 4. Agent Tools for Task Manipulation

An agent (Claude or shell) can use these `wg` CLI commands to manipulate tasks:

| Command | Effect | Can target other tasks? |
|---------|--------|------------------------|
| `wg done <id>` | Mark done | Yes (any task) |
| `wg done <id> --converged` | Mark done, stop cycle | Yes |
| `wg fail <id> --reason "..."` | Mark failed | Yes |
| `wg retry <id>` | Reset failed→open | Yes |
| `wg log <id> "msg"` | Add log entry | Yes |
| `wg msg send <id> "msg"` | Send message | Yes |
| `wg add "title" --after <id>` | Create subtask | N/A (new task) |
| `wg artifact <id> path` | Record artifact | Yes |
| `wg requeue <id>` | Triage requeue | Yes |

Shell executors have access to all `wg` commands via bash. The wrapper script sets `$TASK_ID` and `$WG_AGENT_ID` environment variables.

---

## 5. Log Preservation on Retry

**Logs are always preserved.** The `log: Vec<LogEntry>` field on a task is append-only:

- **`wg retry`** (`src/commands/retry.rs:63-68`): Appends "Task reset for retry" entry. Does not clear existing logs.
- **Cycle re-activation** (`src/graph.rs:1588-1603`): Appends "Re-activated by cycle iteration" entry. Does not clear existing logs.
- **Cycle failure restart** (`src/graph.rs:1800-1810`): Appends restart entry. Does not clear existing logs.

**Agent output archival** (`src/commands/log.rs:138-168`):
- On `wg done` (and `wg fail` in some paths), the agent's `prompt.txt` and `output.log` are copied to `.workgraph/log/agents/<task-id>/<ISO-timestamp>/`
- Each attempt gets its own timestamped directory
- Previous attempt context is surfaced to retry agents via `build_previous_attempt_context()` (`src/commands/spawn/context.rs:631-714`)

**So on retry:**
1. Task `log` entries are preserved in the graph
2. Agent output files are archived per-attempt
3. The next agent receives previous attempt context (checkpoint > output.log tail > log entries)

---

## 6. Gap Analysis: What's Missing

### Target Workflow
1. User defines a task with a bash command (could take hours)
2. Command runs via shell executor
3. On completion, a downstream checker task (Claude agent) wakes up
4. Checker inspects results, decides: done, or reset+retry
5. Reset preserves all logs from previous attempts
6. Cycle continues until checker is satisfied or max retries hit

### What Already Exists (No Changes Needed)

| Capability | Status | Location |
|-----------|--------|----------|
| Shell executor (bash -c) | **Complete** | `src/commands/spawn/execution.rs:963-971` |
| `task.exec` field on tasks | **Complete** | `src/graph.rs:238` |
| Coordinator auto-routes shell tasks | **Complete** | `src/commands/service/coordinator.rs:3159-3167` |
| `wg exec` for manual execution | **Complete** | `src/commands/exec.rs` |
| `wg retry` resets failed→open | **Complete** | `src/commands/retry.rs` |
| Cycle iteration (all-done → re-open) | **Complete** | `src/graph.rs:1420-1610` |
| Cycle failure restart | **Complete** | `src/graph.rs:1661-1830+` |
| Log preservation across retries | **Complete** | Append-only logs + archival |
| Previous attempt context injection | **Complete** | `src/commands/spawn/context.rs:631-714` |
| Agent can call `wg retry`/`wg fail` | **Complete** | CLI is available to all agents |
| `max_retries` / `max_iterations` | **Complete** | `src/graph.rs:259, 10` |
| `CycleConfig.restart_on_failure` | **Complete** | `src/graph.rs:21` |
| `CycleConfig.max_failure_restarts` | **Complete** | `src/graph.rs:27` |

### The Workflow Is Already Possible Today

Using existing primitives:

```bash
# Step 1: Create the long-running command task
wg add "Run simulation" \
  --exec "python run_simulation.py --config params.json" \
  --exec-mode shell

# Step 2: Create the checker task as a cycle with the command task
wg add "Check simulation results" \
  --after run-simulation \
  --max-iterations 5
```

When the checker agent completes and the cycle evaluates:
- If checker does `wg done check-simulation-results` (no --converged): cycle re-opens both tasks
- If checker does `wg done check-simulation-results --converged`: cycle stops
- If checker does `wg fail check-simulation-results`: cycle restarts from failure (same iteration)

### Gaps / Friction Points

| Gap | Severity | Description |
|-----|----------|-------------|
| **No `--exec` flag on `wg add`** | **Medium** | Users must use `wg exec --set <id> "cmd"` as a separate step after `wg add`. Adding `--exec` to `wg add` would be a simple UX improvement. |
| **Shell executor output not as rich** | **Low** | Shell executor captures stdout/stderr but doesn't produce structured stream.jsonl (it gets bookend Init+Result events only). The checker task sees the output via artifacts/logs but not as rich as Claude agent output. |
| **No task.exec on `wg add` CLI** | **Medium** | The `wg add` command in `src/commands/add.rs` doesn't accept an `--exec` parameter. The `exec` field exists on Task but can only be set post-creation via `wg exec --set`. |
| **Checker must know predecessor ID** | **Low** | The checker agent must know which task to inspect. This is already provided via dependency context (`build_task_context` includes dep artifacts and logs). |
| **No "exec + check" composite pattern** | **Low** | There's no built-in command like `wg add-with-checker` that creates the command task + checker task + cycle in one shot. This could be a workflow function (`wg func`). |
| **Cycle requires checker to complete** | **Low** | The cycle only evaluates when ALL members are Done. If the checker wants to restart the command, it must complete itself first (Done, not converged), which triggers the cycle to re-open both. This is correct behavior but may be unintuitive. |

### Concrete Changes Needed

1. **Add `--exec` flag to `wg add`** (Priority: High, Effort: Small)
   - File: `src/commands/add.rs` and `src/cli.rs`
   - Set `task.exec = Some(cmd)` during task creation
   - Also auto-set `exec_mode = "shell"` when `--exec` is provided

2. **Create a workflow function for "exec + check" pattern** (Priority: Medium, Effort: Small)
   - A `wg func` template that creates: shell task → checker task → back-edge cycle
   - Could live in `.workgraph/functions/exec-check-loop.toml`

3. **Improve shell executor output visibility** (Priority: Low, Effort: Medium)
   - Currently stdout/stderr go to output.log. Consider also saving to a structured format or making the output available as an artifact automatically.

---

## Summary

The shell executor and retry-loop pattern is **largely already implemented**. The core workflow (shell command → checker → retry cycle) works with existing cycle primitives. The main gap is UX: adding `--exec` to `wg add` and potentially creating a reusable workflow function for the common pattern.
