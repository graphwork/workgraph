# Provenance System Design

Based on: `docs/research/logging-gaps.md`

## Status Quo

`src/provenance.rs` already provides an append-only JSONL operation log with zstd-compressed rotation. Three commands (`add`, `done`, `fail`) call `provenance::record()`. The remaining nine graph-mutating commands do not. Agent prompts and outputs are archived to `.workgraph/log/agents/<task-id>/<timestamp>/` on task completion. The infrastructure is sound; the gaps are coverage and completeness.

This design extends what exists rather than replacing it.

---

## 1. Operation Log — Complete Coverage

### Current State

`OperationEntry { timestamp, op, task_id, actor, detail }` written to `.workgraph/log/operations.jsonl`. Only `add`, `done`, `fail` record entries.

### Change: Instrument All Mutations

Every graph-mutating command gets a `provenance::record()` call after `save_graph()`, following the existing pattern (best-effort, `let _ =`). The `detail` field captures before/after state diffs as applicable.

| Command | `op` value | `detail` contents |
|---------|-----------|-------------------|
| `add` | `"add"` | `{title, description, blocked_by, tags, skills, model}` (already done) |
| `edit` | `"edit"` | `{fields: [{field, old, new}, ...]}` — one entry per changed field |
| `done` | `"done"` | `{prev_status, loop_resets: [task_ids]}` (extend existing) |
| `fail` | `"fail"` | `{reason, retry_count, prev_status}` (extend existing) |
| `abandon` | `"abandon"` | `{reason, prev_assigned}` |
| `retry` | `"retry"` | `{attempt, prev_failure_reason}` |
| `claim` | `"claim"` | `{prev_status, prev_assigned}` |
| `unclaim` | `"unclaim"` | `{prev_assigned}` |
| `pause` | `"pause"` | `{}` |
| `resume` | `"resume"` | `{}` |
| `archive` | `"archive"` | `{task_ids: [...]}` — all tasks moved in the batch |
| `gc` | `"gc"` | `{removed: [{id, status, title}, ...]}` — record what was deleted |
| `artifact add` | `"artifact_add"` | `{path}` |
| `artifact remove` | `"artifact_rm"` | `{path}` |
| `assign` | `"assign"` | `{agent_hash, role_id}` |

The `actor` field uses a structured convention:
- `"cli"` — human at terminal (default when no agent context)
- `"agent:<agent-id>"` — e.g. `"agent:agent-42"`
- `"coordinator"` — service daemon

### OperationEntry — No Schema Change

The existing struct is sufficient. The `detail: serde_json::Value` field is intentionally open-ended and already handles arbitrary payloads. No new fields needed.

### Implementation

One PR. For each uninstrumented command:
1. Capture relevant "before" state before the mutation.
2. After `save_graph()`, call `provenance::record()` with the appropriate `op` and `detail`.
3. Follow the `let _ =` pattern — provenance failures never block the mutation.

Estimated touch: 9 command files, ~5-10 lines each. Purely additive changes.

---

## 2. Agent Conversation Capture

### Current State

`archive_agent()` (in `commands/log.rs`) already copies `prompt.txt` and `output.log` from `.workgraph/agents/<agent-id>/` to `.workgraph/log/agents/<task-id>/<timestamp>/` when `wg done` or `wg fail` runs. This archive is permanent and survives agent directory cleanup.

### Problem

If an agent is killed (OOM, timeout, daemon restart) without calling `done`/`fail`, the archive step never runs. The live agent directory may get cleaned up by `gc` or manual intervention, losing the prompt and output.

### Change: Archive on Spawn + Append on Completion

1. **At spawn time** (`spawn_agent_inner`): immediately archive `prompt.txt` to `.workgraph/log/agents/<task-id>/<timestamp>/prompt.txt`. This ensures the prompt is preserved even if the agent dies without completing.

2. **At completion** (`done`/`fail`): archive `output.log` → `output.txt` to the same timestamped directory (existing behavior, no change).

3. **Dead agent cleanup** (coordinator's dead-agent detection): when the coordinator detects a dead agent, also archive any `output.log` that exists before cleaning up.

### Storage Layout (unchanged)

```
.workgraph/log/agents/
  <task-id>/
    <ISO-timestamp>/         # one directory per execution attempt
      prompt.txt             # full prompt sent to agent
      output.txt             # full stdout+stderr from agent
```

Each retry of a task creates a new timestamped subdirectory, so the full history of all attempts is retained.

### Implementation

One PR. Changes to `spawn_agent_inner` (~10 lines) and dead-agent cleanup in `service.rs` (~15 lines).

---

## 3. Artifact Archival

### Current State

`task.artifacts` is a `Vec<String>` of file paths. `wg artifact add` appends a path, `wg artifact remove` removes one. No content is captured — only the path string.

### Change: Snapshot on Record

When `wg artifact add <task-id> <path>` is called:
1. Record the provenance entry (Section 1 above).
2. If the path points to an existing file, compute its SHA-256 hash and file size.
3. Store the hash and size in the `detail` field of the provenance entry:
   ```json
   {"path": "docs/design.md", "sha256": "abc123...", "size": 4096}
   ```

This captures *what the artifact was at the time it was registered* without duplicating the file content. The hash is enough to verify whether the artifact has changed since it was recorded. If full content snapshots are later desired, the hash can index into a content-addressed store (see Future Work).

### Traceability

When a downstream task consumes artifacts from its dependencies (via `task_context` in `spawn_agent_inner`), the spawn provenance entry should record which artifact hashes were included:

```json
{"op": "claim", "detail": {"consumed_artifacts": [{"task_id": "design", "path": "docs/design.md", "sha256": "abc123..."}]}}
```

This closes the loop: you can trace from a task's output back through which specific artifact versions it consumed.

### Implementation

Part of the "instrument all mutations" PR (Section 1). Add ~20 lines to `artifact.rs` for the hash computation. Add artifact-hash capture to `spawn_agent_inner`.

---

## 4. Log Rotation

### Current State

Already implemented in `provenance.rs`. When `operations.jsonl` exceeds a configurable threshold (default 10 MB via `config.log.rotation_threshold`), the file is zstd-compressed to `<UTC-timestamp>.jsonl.zst` and a fresh `operations.jsonl` is started. `read_all_operations()` transparently reads across all rotated files.

### No Changes Needed

The existing rotation is correct for an audit trail:
- Rotated files are never deleted (append-only history).
- Compression ratio on structured JSONL is typically 5:1–10:1 with zstd level 3.
- `read_all_operations()` handles seamless read-back across files.
- The threshold is configurable via `config.toml`.

### Configuration

Already in `config.toml`:
```toml
[log]
rotation_threshold = 10485760  # 10 MB, in bytes
```

No changes needed.

---

## 5. Replay Capability

### Goal

The operation log should be complete enough to reconstruct the graph's historical state. Given the log, answer: *what tasks existed at time T? What was their state? What prompts were sent? What outputs came back?*

### What Makes This Possible

With Sections 1–3 implemented, the operation log contains:
- Every task creation with full initial fields (`add` entries)
- Every state transition with before/after (`done`, `fail`, `claim`, etc.)
- Every field edit with old/new values (`edit` entries)
- Every dependency change (captured in `edit` detail)
- Agent prompts archived per-attempt (Section 2)
- Artifact hashes at registration time (Section 3)

### Reconstruction Algorithm

To reconstruct graph state at time T:

1. Read all operation entries with `timestamp ≤ T` (using `read_all_operations()`, filter by timestamp).
2. Starting from an empty graph, apply each entry in order:
   - `add` → create task with initial fields from `detail`
   - `edit` → apply field changes from `detail.fields[]`
   - `claim` → set `status = InProgress`, `assigned = actor`
   - `done` → set `status = Done`, `completed_at = timestamp`
   - `fail` → set `status = Failed`, `failure_reason = detail.reason`
   - `gc` → remove listed task IDs
   - etc.
3. The resulting graph is the state at time T.

### Design Decision: Forward Replay Only (Not Snapshots)

Two approaches were considered:

**A. Periodic snapshots + forward replay from nearest snapshot.** Faster reconstruction for large histories, but requires snapshot management, increases storage, and adds complexity around when to snapshot.

**B. Forward replay from the beginning.** Simpler, no additional storage, works with the existing append-only log. Slower for very long histories, but adequate for the scale we target (thousands of tasks, not millions).

**Choice: B (forward replay), with snapshot support as future work.** For a project with 10,000 operations, replay takes milliseconds — JSON parsing is fast. If projects grow to 100K+ operations, snapshots can be added as an optimization without changing the log format.

### What's NOT Captured (Accepted Limitations)

- **Files the agent read from the filesystem** — not in scope. Would require executor-level instrumentation (intercepting file reads). The prompt and output provide sufficient replay context.
- **Structured agent conversation turns** — Claude's `--print` mode produces flat text, not structured turns. Parsing this is fragile and executor-specific. The raw output is archived as-is.
- **LLM token counts and costs** — requires executor cooperation. Can be added later by parsing the `stream-json` output format.

---

## 6. CLI

### Existing Commands (already implemented)

- `wg log <task-id> <message>` — add a log entry to a task
- `wg log <task-id> --list` — show a task's log entries
- `wg log --operations` — show the operation log
- `wg log --agent <task-id>` — show archived agent prompt+output for a task

### New Subcommands

#### `wg log --operations --task <task-id>`

Filter the operation log to entries for a specific task. Trivial filter on `OperationEntry::task_id`.

#### `wg log --operations --actor <actor>`

Filter by actor (e.g., `--actor cli`, `--actor agent:agent-42`, `--actor coordinator`).

#### `wg log --operations --since <timestamp> --until <timestamp>`

Filter by time range. Timestamps in RFC 3339 or shorthand (`1h`, `24h`, `7d`).

#### `wg log --operations --op <op-type>`

Filter by operation type (e.g., `--op claim`, `--op edit`).

These filters compose: `wg log --operations --task build-widget --op edit --since 1h`.

#### `wg replay <timestamp>` (future — not MVP)

Reconstruct and display the graph state at a point in time. Uses the reconstruction algorithm from Section 5. Output format: same as `wg list` but showing the historical state.

This is explicitly deferred. The log format supports it, but the reconstruction logic can be built when needed.

### Implementation

One PR. Add filter flags to the existing `log --operations` command (~40 lines of filter logic). The `replay` command is a separate future PR.

---

## 7. Storage Estimate

For a project with 1000 tasks, each going through a typical lifecycle (add → claim → done, with 2-3 edits):

| Data | Entries | Avg size/entry | Total |
|------|---------|---------------|-------|
| Operation log | ~5,000 | ~200 bytes | ~1 MB |
| Agent prompts | ~1,000 | ~5 KB | ~5 MB |
| Agent outputs | ~1,000 | ~10 KB | ~10 MB |
| Rotated compressed logs | — | ~5:1 ratio | ~200 KB per 1 MB |

**Total: ~16 MB uncompressed, ~4 MB after rotation compression on the operation log.**

For comparison, `.workgraph/graph.jsonl` for 1000 tasks is already ~2-5 MB. The provenance system roughly triples the storage footprint, which is very manageable. Agent prompts and outputs dominate — and they're already being archived today.

---

## Implementation Plan: 3 PRs

### PR 1: Complete Operation Log Coverage

**Scope:** Add `provenance::record()` calls to all 9 uninstrumented commands (edit, abandon, retry, claim, unclaim, pause, resume, archive, gc) plus artifact add/remove and assign.

**Files touched:**
- `src/commands/edit.rs` — capture old field values, record after save
- `src/commands/abandon.rs` — record with reason
- `src/commands/retry.rs` — record with attempt number
- `src/commands/claim.rs` — record with prev_status
- `src/commands/unclaim.rs` — record with prev_assigned
- `src/commands/pause.rs` — record
- `src/commands/resume.rs` — record
- `src/commands/archive.rs` — record with task_ids batch
- `src/commands/gc.rs` — record with removed task details
- `src/commands/artifact.rs` — record add/remove with SHA-256 hash
- `src/commands/assign.rs` — record with agent_hash

**Estimated size:** ~100 lines net across all files. Each command gets 5-10 lines of capture + record. No structural changes.

**Tests:** Add integration test that performs a sequence of operations and verifies all appear in `read_all_operations()` output.

### PR 2: Robust Agent Archive

**Scope:** Archive prompt at spawn time; archive output on dead-agent detection.

**Files touched:**
- `src/commands/spawn.rs` or `src/service/executor.rs` — archive prompt.txt immediately after writing it
- `src/commands/service.rs` — in dead-agent cleanup, archive output.log before removing agent directory

**Estimated size:** ~30 lines net. Reuses existing `archive_agent()` logic from `commands/log.rs`.

**Tests:** Integration test that spawns an agent, kills it, and verifies prompt.txt is still in the archive.

### PR 3: Operation Log Filtering CLI

**Scope:** Add `--task`, `--actor`, `--since`, `--until`, `--op` filters to `wg log --operations`.

**Files touched:**
- `src/commands/log.rs` — add filter arguments and filtering logic
- `src/main.rs` — add CLI args for the new flags

**Estimated size:** ~60 lines net. Filter logic operates on the `Vec<OperationEntry>` returned by `read_all_operations()`.

**Tests:** CLI integration tests exercising each filter and combinations.

### Future: PR 4+ (Not MVP)

- `wg replay <timestamp>` — graph state reconstruction
- Content-addressed artifact store (store file contents by SHA-256)
- Structured daemon log (replace text daemon.log with JSONL)
- LLM cost tracking (parse stream-json output for token counts)
- Evolve/evaluate prompt archival (save the prompts sent during agency evolution)

---

## Design Constraints

- **Backward compatible.** No changes to `graph.jsonl` format. The operation log is purely additive.
- **Best-effort recording.** Provenance failures (`let _ =`) never block graph mutations. The graph is the source of truth; the log is supplementary.
- **No new dependencies.** Uses existing `serde_json`, `chrono`, `zstd`, `sha2` crates.
- **No new data structures.** `OperationEntry` is unchanged. All new information goes into the `detail` field.
- **Performance.** Append-only writes add <1ms per operation. No read overhead on hot paths (the log is only read by `wg log --operations`).
