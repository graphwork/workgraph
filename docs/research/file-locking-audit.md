# File Locking Audit: Workgraph Concurrent Access

**Date:** 2026-02-18
**Scope:** Correctness of file locking under 5x agent parallelism

---

## Executive Summary

Workgraph uses `flock(2)` advisory locking around `graph.jsonl` reads and writes, plus atomic write-rename for crash safety. These mechanisms **prevent corruption and partial reads**, but do **not prevent lost updates** due to a classic TOCTOU (time-of-check-time-of-use) gap. Under 5x parallelism, every `wg` command that mutates the graph (done, fail, log, artifact, claim) follows a load→modify→save pattern where the lock is held separately for the load and the save, not across the entire read-modify-write cycle. This means concurrent writes **will silently overwrite each other**.

**Severity: HIGH** — Lost updates are virtually guaranteed under normal 5-agent operation. The most likely symptom is log entries, status changes, or artifact registrations silently vanishing.

---

## 1. Current Locking Mechanisms

### 1.1 graph.jsonl — `parser.rs`

**Mechanism:** `flock(2)` advisory lock on `.workgraph/graph.lock`

- `load_graph()` acquires an **exclusive lock** (`LOCK_EX`), reads the file, then releases the lock when `FileLock` is dropped (line 96–132).
- `save_graph()` acquires an **exclusive lock**, writes to a temp file (`.graph.tmp.<pid>`), calls `fsync()`, then atomically renames to `graph.jsonl`, then releases the lock (line 138–184).
- The lock file is `graph.lock` in the same directory as `graph.jsonl`.
- On non-Unix systems, locking is a **no-op** (line 64–68).

**Assessment:** Each individual load or save is atomic and safe. The problem is that they are **independently locked operations**, not a single transaction.

### 1.2 Agent Registry — `service/registry.rs`

**Mechanism:** `flock(2)` advisory lock on `.workgraph/service/.registry.lock`

- `AgentRegistry::load()` / `save()` — **no locking** (line 127, 161). Used by `spawn.rs`.
- `AgentRegistry::load_locked()` — acquires exclusive flock, returns a `LockedRegistry` that holds the lock until dropped or saved (line 196–237). Used by `cleanup_dead_agents()` in `service.rs`.

**Assessment:** The coordinator correctly uses `load_locked()` for its cleanup path, but `spawn.rs` uses the unlocked `load()`/`save()` pair. Two concurrent spawns could both load the registry, each add an agent, and one's registration is lost. In practice the coordinator serializes spawns (single-threaded tick loop), so this is low risk.

### 1.3 Provenance Log — `provenance.rs`

**Mechanism:** **No locking.** Uses `OpenOptions::append(true)` (line 62–66).

- `append_operation()` opens the file with `O_APPEND` and writes a single line.
- `O_APPEND` is atomic on Linux for writes under `PIPE_BUF` (4096 bytes) — each JSON line is typically well under this limit.
- Rotation (compress + truncate) has **no locking** and could race with concurrent appenders.

**Assessment:** Append-only access is safe under normal operation (small JSON lines). The rotation path is unsafe under concurrency but rotation only triggers at 10 MB, which is rarely hit during normal agent operation.

### 1.4 Daemon Log — `service.rs` DaemonLogger

**Mechanism:** `Mutex<DaemonLoggerInner>` (in-process).

**Assessment:** Safe — the daemon is a single process and uses a Mutex for internal logging. Not relevant to inter-process concurrency.

### 1.5 daemon.sock (Unix Domain Socket)

**Mechanism:** IPC for `GraphChanged`, `Spawn`, `Reconfigure`, `Status` requests.

- Commands like `wg done`, `wg log`, `wg artifact` call `notify_graph_changed()` which sends a `GraphChanged` IPC message to the daemon (line 131–133 in `mod.rs`).
- This is a **notification** mechanism only — it tells the coordinator to wake up and check for new work, but does **not** serialize graph mutations through the daemon.

**Assessment:** The IPC channel exists and works, but is not used for write serialization. All graph mutations happen via direct file access from agent processes.

---

## 2. The TOCTOU Race Condition

### 2.1 The Pattern

Every graph-mutating command follows this pattern:

```
1. load_graph()        — acquires flock, reads file, releases flock
2. modify in memory    — no lock held
3. save_graph()        — acquires flock, writes temp file, renames, releases flock
```

The lock is **not held** between steps 1 and 3. This means:

```
Time    Agent A (wg done task-1)          Agent B (wg log task-2 "msg")
────    ────────────────────────          ─────────────────────────────
T1      load_graph() → version V1
T2                                        load_graph() → version V1
T3      modify: task-1.status = Done
T4      save_graph() → version V2
T5                                        modify: task-2.log += "msg"
T6                                        save_graph() → version V3
                                          (based on V1, not V2!)
```

**Result:** Agent A's `task-1.status = Done` is silently lost. The graph at V3 contains Agent B's log entry but task-1 is still in its pre-Done state.

### 2.2 Affected Commands

Every command using the `load_workgraph_mut()` → `save_graph()` pattern:

| Command | File | Mutates |
|---------|------|---------|
| `wg done` | `done.rs:14–57` | status, completed_at, log, loop edges |
| `wg fail` | `fail.rs:13–55` | status, retry_count, failure_reason, log |
| `wg claim` | `claim.rs:13–72` | status, assigned, started_at, log |
| `wg unclaim` | `claim.rs:83–112` | status, assigned, log |
| `wg log` | `log.rs:12–25` | task.log |
| `wg artifact` | `artifact.rs:11–27` | task.artifacts |
| `wg add` | `add.rs` | adds new node, updates blocked_by/blocks |
| `wg spawn` | `spawn.rs:131–382` | status, assigned, started_at, log |
| Coordinator tick | `service.rs:826–848` | auto-assign, auto-evaluate |
| Dead agent cleanup | `service.rs:935–1002` | status, assigned, log |

### 2.3 Spawn-specific Double Race

`spawn.rs` has a particularly dangerous pattern at line 362–400:

1. `load_graph()` — check task status (Open?)
2. Modify task: set InProgress, assigned
3. `save_graph()` — persist claim
4. `cmd.spawn()` — start agent process
5. If spawn fails: `load_graph()` again, unclaim, `save_graph()` — **another TOCTOU!**

Two concurrent `wg spawn` calls (e.g., from two coordinator ticks) could both see the task as Open, both claim it, and the second save overwrites the first. This is partially mitigated because the coordinator is single-threaded, but `wg spawn` can also be called directly from the CLI.

---

## 3. Concrete Race Conditions Under 5x Parallelism

### Race 1: Lost Status Transition (HIGH severity)

**Scenario:** Agent A runs `wg done task-A` while Agent B runs `wg done task-B` simultaneously.

**Impact:** One agent's `done` status is lost. The coordinator sees the task as still in-progress, may spawn a new agent on it, or the task is permanently stuck.

**Likelihood:** HIGH under 5 agents — two agents finishing near-simultaneously is common.

### Race 2: Lost Log Entries (MEDIUM severity)

**Scenario:** Agent A runs `wg log task-A "progress"` while Agent B runs `wg log task-B "started"`.

**Impact:** One log entry vanishes. This is primarily an observability loss — the graph state may still be correct.

**Likelihood:** HIGH — agents log progress frequently and concurrently.

### Race 3: Coordinator vs. Agent (HIGH severity)

**Scenario:** The coordinator's `cleanup_dead_agents()` loads the graph (line 935) and modifies tasks. Simultaneously, a live agent calls `wg done`.

**Impact:** Either the coordinator's cleanup is lost (task stays stuck) or the agent's `done` is lost (task incorrectly unclaimed).

**Likelihood:** MEDIUM — depends on timing of dead agent cleanup and live agent completion.

### Race 4: Coordinator Auto-assign vs. Agent (MEDIUM severity)

**Scenario:** The coordinator's tick loads the graph (line 826), runs auto-assign (creates new tasks, line 834–848), and saves. An agent simultaneously saves after running `wg done`.

**Impact:** Either auto-assign tasks are lost or the agent's completion is lost.

**Likelihood:** MEDIUM — the coordinator polls every ~30 seconds.

### Race 5: Artifact Registration Loss (LOW severity)

**Scenario:** Two agents both register artifacts near-simultaneously.

**Impact:** One artifact registration is lost. This affects downstream tasks that depend on artifacts.

**Likelihood:** LOW — artifact registration is less frequent than log/status operations.

---

## 4. Severity Assessment

| Issue | Severity | Data Loss? | Corruption? | Silent? |
|-------|----------|------------|-------------|---------|
| TOCTOU lost updates | **HIGH** | Yes (lost state transitions) | No (file is well-formed) | **Yes** — no error or warning |
| Lost log entries | **MEDIUM** | Yes (observability) | No | **Yes** |
| Provenance rotation race | **LOW** | Possible truncation | Possible | Possible error on read |
| Registry concurrent save | **LOW** | Lost agent registrations | No | **Yes** |

**Key insight:** The graph file is never corrupted (atomic rename ensures this), but state transitions are silently lost. This is worse than corruption in some ways — corruption produces an error, but a lost update is invisible.

---

## 5. Recommended Fixes

### Option A: Hold flock Across Read-Modify-Write (Simplest)

Create a new function that holds the lock for the entire transaction:

```rust
pub fn with_graph_locked<F, T>(path: &Path, f: F) -> Result<T, ParseError>
where
    F: FnOnce(&mut WorkGraph) -> T,
{
    let lock_path = get_lock_path(path);
    let _lock = FileLock::acquire(&lock_path)?;  // held for entire scope

    let graph = load_graph_unlocked(path)?;  // internal version, no lock
    let mut graph = graph;
    let result = f(&mut graph);
    save_graph_unlocked(&graph, path)?;       // internal version, no lock

    Ok(result)
    // lock released here
}
```

**Pros:** Minimal code change, uses existing infrastructure.
**Cons:** All readers block all writers and vice versa. Under 5 agents, contention on the single lock file could cause noticeable latency (each flock blocks until released). Read-only commands (`wg show`, `wg list`) would also block writers.

**Optimization:** Use `LOCK_SH` (shared) for reads and `LOCK_EX` (exclusive) for writes. This allows concurrent reads but serializes writes.

### Option B: Atomic Compare-and-Swap via Checksum

Add a checksum/version field to the graph file. On save, verify the checksum matches what was loaded. If not, reload and retry.

```rust
pub fn save_graph_checked(graph: &WorkGraph, path: &Path, expected_checksum: u64) -> Result<(), SaveConflict> {
    let _lock = FileLock::acquire(&lock_path)?;
    let current_checksum = compute_checksum(path)?;
    if current_checksum != expected_checksum {
        return Err(SaveConflict::StaleRead);
    }
    save_graph_unlocked(graph, path)?;
    Ok(())
}
```

**Pros:** Detects conflicts instead of silently losing them. Can implement retry logic.
**Cons:** Requires retry logic in every caller. More complex. Doesn't prevent conflicts, just detects them.

### Option C: Serialize Graph Mutations Through the Coordinator (Best)

Route all graph-mutating operations through the daemon's IPC socket. Instead of agents directly modifying `graph.jsonl`, they send mutation requests to the coordinator, which applies them serially.

```
Agent calls `wg done task-1`
  → Instead of load/modify/save:
  → Sends IPC message: { "cmd": "mutate", "op": "done", "task_id": "task-1" }
  → Coordinator receives, loads graph, applies mutation, saves
  → Returns result to agent via IPC response
```

**Pros:** Perfect serialization. No locking needed for the graph file. The coordinator already has an IPC socket (`daemon.sock`). Natural single-writer architecture.
**Cons:** Significant refactor. Requires the daemon to be running for any mutation (or fallback to direct access when daemon is offline). Adds latency (IPC round-trip).

**Hybrid approach:** Use IPC when the daemon is running, fall back to flock-based direct access when it's not. The daemon is always running during multi-agent operation (it's the coordinator), so the critical path is covered.

### Option D: flock Wrapper Function (Pragmatic Middle Ground)

Create a higher-level function that wraps the load-modify-save pattern with a held lock:

```rust
pub fn mutate_graph<F>(dir: &Path, f: F) -> Result<()>
where
    F: FnOnce(&mut WorkGraph) -> Result<()>,
{
    let path = graph_path(dir);
    let lock_path = get_lock_path(&path);
    let _lock = FileLock::acquire(&lock_path)?;

    let mut graph = load_graph_inner(&path)?;  // no separate lock
    f(&mut graph)?;
    save_graph_inner(&graph, &path)?;          // no separate lock

    Ok(())
}
```

Then refactor all mutating commands to use `mutate_graph()` instead of separate `load_graph()` + `save_graph()` calls.

**Pros:** Straightforward refactor. Correct under all concurrency scenarios. No IPC dependency.
**Cons:** Serializes all writes through a single lock. Under 5 agents, a worst-case scenario where all 5 try to write simultaneously would serialize them (each waits for the previous). In practice, writes are fast (< 100ms) so this is likely acceptable.

### Recommendation

**Option D (flock wrapper)** for immediate fix — it's the smallest change that eliminates the TOCTOU race. Long-term, **Option C (IPC serialization)** is architecturally cleaner and would eliminate file locking entirely for the multi-agent case.

---

## 6. Can daemon.sock IPC Serialize Graph Mutations?

**Yes, but with caveats.**

The existing IPC infrastructure in `service.rs` already supports:
- `IpcRequest::Spawn` — spawn an agent (indirectly mutates graph)
- `IpcRequest::GraphChanged` — notification
- `IpcRequest::Reconfigure` — update daemon settings
- `IpcRequest::Status` — query daemon state

Adding mutation commands (Done, Fail, Log, Artifact, Claim) is architecturally feasible. The daemon already has the graph path and could centralize all writes.

**Caveats:**
1. **Daemon must be running** — When no daemon is running (single-user mode, `wg done` from CLI), there's no IPC endpoint. Need a fallback.
2. **Synchronous response** — Agents need to know if their `wg done` succeeded. The IPC must be request-response, which it already is (`IpcResponse`).
3. **Performance** — IPC round-trip adds ~1ms latency per operation. Acceptable.
4. **Error handling** — If the daemon crashes mid-mutation, the agent needs to know. The socket disconnect would signal this.

**Recommendation:** This is the right long-term architecture. For now, Option D is simpler and sufficient.

---

## 7. Files Not Affected

| File | Reason |
|------|--------|
| `archive.jsonl` | No such file exists — archives are per-task directories under `.workgraph/agents/` and `.workgraph/archive/`. Written once per task completion, no concurrent access risk. |
| `.workgraph/agency/*.yaml` | Role/motivation/agent configs. Written during `wg role`/`wg motivation` commands, not during agent execution. No concurrency risk. |
| `.workgraph/config.toml` | Read-only during agent operation. |
| Agent output files (`output.log`) | Each agent writes to its own file. No shared access. |

---

## 8. Existing Test Coverage

The file `parser.rs` includes a `test_concurrent_access_with_locking` test (line 788–839) that spawns 10 threads doing concurrent load-modify-save. The test asserts:
- At least some operations succeed
- The final graph is parseable (no corruption)

**Critically, it does NOT assert that all operations are preserved.** The comment "At least some operations should have succeeded" implicitly acknowledges the lost-update problem. Under the current implementation, this test would show that out of 10 concurrent writes, only 1–2 are preserved (the rest are silently overwritten).

---

## 9. Summary of Findings

1. **graph.jsonl**: flock + atomic rename prevents corruption, but TOCTOU gap causes lost updates under concurrency. **This is the primary bug.**
2. **Agent registry**: Unlocked load/save in spawn.rs, locked load in coordinator. Low risk due to coordinator serialization.
3. **Provenance log**: Append-only with `O_APPEND`, safe for small writes. Rotation is unprotected.
4. **daemon.sock**: Exists for notification only, not for write serialization. Could be extended.
5. **Recommended fix**: `mutate_graph()` wrapper holding flock across the full read-modify-write cycle (Option D). Long-term: IPC serialization through the coordinator (Option C).
