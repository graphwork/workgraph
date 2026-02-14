# Service Layer & Coordination Review

**Scope:** ~8,800 lines across 15 files covering the service daemon, coordinator loop, executor framework, agent registry, and agent lifecycle commands.

---

## 1. Full Lifecycle Documentation

### Service Start → Agent Completion

```
wg service start --max-agents N
        │
        ▼
   run_start()  [commands/service.rs:888]
        │  Checks for existing daemon (ServiceState)
        │  Forks a new process: `wg --dir <dir> service daemon --socket <sock>`
        │  Saves ServiceState (PID, socket path, started_at)
        ▼
   run_daemon()  [commands/service.rs:1056]
        │  Opens DaemonLogger, binds UnixListener, sets non-blocking
        │  Initializes CoordinatorState on disk
        │  Enters main loop:
        │
        ├──► Accept IPC connections (non-blocking poll at 100ms)
        │    └─ handle_connection() → handle_request()
        │       Dispatch: Spawn, Agents, Kill, Heartbeat, Status,
        │                 Shutdown, GraphChanged, Pause, Resume, Reconfigure
        │
        └──► Background coordinator tick (on poll_interval OR GraphChanged IPC)
             │
             ▼
        coordinator_tick()  [commands/service.rs:304]
             │
             ├─ 1. cleanup_dead_agents()  [service.rs:635]
             │     Load locked registry, detect dead (process exited),
             │     mark Dead in registry, unclaim InProgress tasks (→ Open),
             │     capture output for Done/Failed tasks (safety net)
             │
             ├─ 2. Count alive agents, early-exit if at max_agents
             │
             ├─ 3. Load graph, get ready_tasks()
             │
             ├─ 4. [Optional] auto_assign subgraph construction
             │     For unassigned ready tasks (without "assignment"/"evaluation"/"evolution" tags),
             │     create `assign-{task-id}` task that blocks the original
             │
             ├─ 5. [Optional] auto_evaluate subgraph construction
             │     Create `evaluate-{task-id}` tasks blocked by originals,
             │     unblock eval tasks whose source Failed
             │
             ├─ 6. Re-load graph (post-subgraph), re-compute ready_tasks()
             │
             └─ 7. Spawn agents on ready tasks (up to slots_available)
                  │
                  ▼
             spawn::spawn_agent()  [commands/spawn.rs:396]
                  │  Load graph, verify task Open (not claimed/done)
                  │  Build context from dependency artifacts
                  │  Create TemplateVars (identity, skills preamble, etc.)
                  │  Load ExecutorConfig (from registry or .toml file)
                  │  Resolve effective model (CLI > task.model > none)
                  │  Build inner_command (claude pipe, shell -c, or custom)
                  │  Generate wrapper script (run.sh):
                  │    ┌─────────────────────────────────────┐
                  │    │ Run inner command, capture exit code │
                  │    │ Check task status via `wg show`      │
                  │    │ If still in-progress:                │
                  │    │   exit 0 → wg done / wg submit      │
                  │    │   exit N → wg fail                   │
                  │    └─────────────────────────────────────┘
                  │  Spawn `bash run.sh` detached (setsid)
                  │  Claim task (InProgress, assigned = agent-N)
                  │  Register in AgentRegistry, write metadata.json
                  ▼
             Agent process runs independently
                  │  Claude: prompt piped from prompt.txt → claude --print
                  │  Shell: exec command via bash -c
                  │
                  │  Agent calls `wg done` / `wg fail` / `wg submit`
                  │  These commands send GraphChanged IPC to daemon
                  │  Daemon wakes, runs coordinator_tick, which sees
                  │  the task is Done and doesn't re-spawn
                  │
                  ▼
             Next coordinator_tick: cleanup_dead_agents()
                  │  Detects process exited → marks agent Dead
                  │  If task still InProgress → unclaims (safety net)
                  │  Captures output if not already captured
                  ▼
             Cycle continues until all tasks done
```

### IPC Protocol

Unix domain socket at `/tmp/wg-{project}.sock`. Newline-delimited JSON, one request/response per line. Tagged union via `"cmd"` field.

Key flow: `wg done <task-id>` → notifies service via `GraphChanged` IPC → daemon forces immediate coordinator tick → newly unblocked tasks get agents spawned.

---

## 2. Complexity Hotspots

### 2.1 Massive Code Duplication in spawn.rs

**`spawn::run()` (lines 111-392) and `spawn::spawn_agent()` (lines 396-656) are nearly identical** — ~260 lines duplicated with only minor differences:
- `run()` is for `wg spawn` CLI, prints output
- `spawn_agent()` is for the coordinator, returns `(agent_id, pid)`

This is the single largest maintenance hazard in the service layer. Any fix to one must be manually applied to the other.

**Recommendation:** Extract common logic into a shared `spawn_inner()` that returns a result struct. Both `run()` and `spawn_agent()` become thin wrappers.

### 2.2 coordinator_tick() is 300+ lines (service.rs:304-611)

This function does too many things:
1. Dead agent cleanup
2. Alive agent counting
3. Ready task computation
4. Auto-assign subgraph construction (~90 lines)
5. Auto-evaluate subgraph construction (~120 lines)
6. Graph re-loading
7. Agent spawning

Each of the auto-assign and auto-evaluate blocks loads, mutates, and saves the graph independently, with the graph being loaded **up to 4 times** in a single tick (once for cleanup, once for ready tasks, once for auto-assign, once for auto-evaluate, then a final re-load).

**Recommendation:**
- Extract `auto_assign_subgraph()` and `auto_evaluate_subgraph()` into separate functions
- Load the graph once, pass `&mut WorkGraph` through, save once at the end
- The current pattern of repeated load/save is both inefficient and creates TOCTOU windows

### 2.3 Two Parallel Prompt Systems

There are **two completely independent prompt/executor systems**:

1. **`src/service/executor.rs` + `src/service/claude.rs`**: The `Executor` trait with `ClaudeExecutor`, `ShellExecutor`, `DefaultExecutor`, and `ExecutorRegistry`. This has proper prompt building, metadata writing, and `AgentHandle` management.

2. **`src/commands/spawn.rs`**: Independently reimplements everything — builds prompts, constructs commands, creates wrapper scripts. Does NOT use the `Executor` trait at all.

The `Executor` trait and its implementations (`ClaudeExecutor::spawn()`, `ShellExecutor::spawn()`) are **not used in production**. The coordinator calls `spawn::spawn_agent()`, which does everything from scratch. The `ExecutorRegistry` is only used for its `load_config()` method — the actual `Executor::spawn()` is never called by the coordinator or CLI.

**Recommendation:** Either:
- (a) Remove the `Executor` trait and `ClaudeExecutor`/`ShellExecutor` structs (they're dead code), or
- (b) Refactor `spawn.rs` to delegate to `Executor::spawn()` and move the wrapper script logic there

### 2.4 `src/executors/` Directory is Pure Dead Weight

The three files in `src/executors/` are just re-exports:
```rust
pub use crate::service::claude::*;
pub use crate::service::shell::*;
```
No consumer imports from `crate::executors`. This adds confusion about where the "real" code lives.

**Recommendation:** Remove `src/executors/` entirely.

---

## 3. State Transitions and Error Handling

### 3.1 Agent Status State Machine

```
                  ┌──────────┐
     register()──►│ Working  │──── process exits ────►┌──────┐
                  └──────────┘                        │ Dead │
                       │                               └──────┘
                  set_status()                              │
                       │                          cleanup_dead_agents()
                  ┌──────────┐                   unclaims task → Open
                  │  Idle    │
                  └──────────┘
                       │
                  ┌──────────┐
                  │ Stopping │──── kill ────►┌──────────┐
                  └──────────┘               │ (removed)│
                                             └──────────┘
```

**Issues:**
- `Starting` status is defined but never set. Agents go directly to `Working`.
- `Idle` status is defined but never set by the coordinator or wrapper. Only usable by custom executors.
- `Done` and `Failed` agent statuses exist but are never set. Dead agents go `Working → Dead`, not `Working → Done`.
- The agent status is largely vestigial — the coordinator uses `is_process_alive()` as the primary signal, not registry status.

### 3.2 Task Status Transitions During Spawning

```
Task: Open ──spawn_agent()──► InProgress (assigned=agent-N)
                                    │
                ┌───────────────────┼───────────────────┐
                ▼                   ▼                   ▼
      Agent calls `wg done`  Agent calls `wg fail`  Agent crashes
      Task → Done            Task → Failed          cleanup_dead_agents()
                                                    Task → Open (unclaimed)
```

**Race condition window:** In `spawn_agent()`, the process is spawned *before* the task is claimed (lines 612-636). If the agent starts working before the graph is saved with `InProgress`, concurrent operations could see stale state. In practice, the short window and single-writer coordinator make this unlikely.

### 3.3 Error Handling Gaps

1. **No retry logic in coordinator_tick:** If `spawn_agent()` fails for a task (e.g., can't create output dir), the task stays Open but gets re-attempted on every tick. There's no backoff or max-failure tracking at the coordinator level.

2. **Silenced errors in auto-assign/auto-evaluate:** `save_graph` failures are logged with `eprintln!` but execution continues. The task graph could be in an inconsistent state (subgraph tasks created in memory but not persisted).

3. **`cleanup_dead_agents` loads graph 3 times:** Once for unclaiming, once for output capture, and the caller loads it again for ready tasks. If any of these fail, partial state changes may have been committed.

4. **Wrapper script `wg show --json` parsing is fragile:** The bash regex `grep -o '"status": *"[^"]*"'` could break if `wg show` output format changes. A dedicated `wg status <task-id>` returning just the status would be more robust.

---

## 4. Architectural Observations

### 4.1 IPC Over Unix Sockets is Well-Designed

The daemon/IPC architecture is clean:
- Non-blocking accept with 100ms sleep polling
- Proper connection timeouts (5s read/write)
- File-based state persistence (state.json, coordinator-state.json, registry.json)
- Atomic registry saves (write-to-temp-then-rename)
- File locking for concurrent registry access (`flock`)
- Zombie reaping to avoid PID aliasing
- `setsid()` for agent independence from daemon lifecycle

### 4.2 DaemonLogger is Simple and Effective

The rotating file logger with panic hook is appropriate for a daemon. 10MB rotation with one backup is reasonable.

### 4.3 The `wg agent` Command is a Separate System

`commands/agent.rs` implements a WAKE/CHECK/WORK/SLEEP agent loop that is completely independent of the coordinator/service daemon. It:
- Operates on "actors" (graph-level entities with capabilities), not "agents" (registry-level process entries)
- Claims and executes tasks directly (in-process `sh -c`), not via wrapper scripts
- Has its own persistent state (`.workgraph/agents/{actor-id}.json`)
- Uses skill matching for task selection

This is essentially a standalone predecessor to the coordinator system. Its continued existence alongside the service daemon creates confusion about which system to use.

### 4.4 Heartbeat is Vestigial for the Coordinator

The coordinator detects dead agents via `is_process_alive(pid)` only. The heartbeat-based detection in `dead_agents.rs` is only used by the standalone `wg dead-agents` command. The daemon loop auto-bumps heartbeats for alive processes (`service.rs:647-649`), making heartbeat staleness meaningless when the daemon is running.

---

## 5. Simplification Recommendations

### High Priority

1. **Deduplicate `spawn.rs`**: Extract `spawn_inner()` shared by both `run()` and `spawn_agent()`. This eliminates ~250 lines of duplication and prevents drift.

2. **Reduce graph loads in `coordinator_tick()`**: Load graph once, pass `&mut WorkGraph`, save once. Current pattern loads up to 5 times per tick.

3. **Resolve the Executor trait question**: Either use the trait in production (move wrapper script logic into `Executor::spawn()`) or remove the trait and implementations.

### Medium Priority

4. **Extract auto-assign and auto-evaluate into functions**: Each is 90-120 lines embedded in `coordinator_tick()`. They should be standalone functions taking `&mut WorkGraph`.

5. **Add coordinator-level spawn failure tracking**: Prevent re-attempting tasks that consistently fail to spawn (e.g., missing executor).

6. **Remove `src/executors/` re-export layer**: Dead indirection.

7. **Clean up unused `AgentStatus` variants**: `Starting`, `Idle`, `Done`, `Failed` are never set by the coordinator path.

### Low Priority

8. **Clarify `wg agent` vs `wg service start` relationship**: Consider deprecating `wg agent` in favor of the service/coordinator, or document when each is appropriate.

9. **Make wrapper script status check more robust**: Replace `grep -o` JSON parsing with a dedicated `wg task-status <id>` command.

10. **Consider moving `DEFAULT_CLAUDE_PROMPT` in `claude.rs` to the executor config**: It's currently unused (the `ExecutorRegistry::default_config("claude")` prompt in `executor.rs:522-576` is what actually gets used).

---

## 6. File-by-File Summary

| File | Lines | Role | Complexity |
|------|-------|------|------------|
| `service/mod.rs` | 23 | Re-exports | Trivial |
| `service/executor.rs` | 969 | Executor trait, config, template system, `ExecutorRegistry` | Medium — well-tested but partially unused |
| `service/registry.rs` | 917 | `AgentRegistry`, `LockedRegistry`, status tracking | Low — clean CRUD with good tests |
| `service/claude.rs` | 532 | `ClaudeExecutor` implementation | Low — but `Executor::spawn()` is unused |
| `service/shell.rs` | 567 | `ShellExecutor` implementation | Low — but `Executor::spawn()` is unused |
| `executors/*` | ~20 | Re-exports only | Dead code |
| `commands/service.rs` | 2,293 | Daemon, coordinator, IPC, start/stop/status | **High** — coordinator_tick is the complexity center |
| `commands/spawn.rs` | 998 | Agent spawning, wrapper scripts | **High** — massive duplication between run()/spawn_agent() |
| `commands/agent.rs` | 924 | Autonomous agent runtime (standalone) | Medium — independent system |
| `commands/agents.rs` | 378 | List agents with PID-aware status | Low |
| `commands/dead_agents.rs` | 537 | Dead agent detection and cleanup | Low |
| `commands/kill.rs` | 419 | Kill agents, unclaim tasks | Low |
| `commands/heartbeat.rs` | 434 | Heartbeat for actors and agents | Low |
| `commands/exec.rs` | 306 | Direct task execution (`wg exec`) | Low |
| `commands/coordinate.rs` | 405 | Coordination status view | Low |

**Total reviewed:** ~8,722 lines of source (including tests).
