# Automatic Task Placement — `.place-*` System Tasks

## Overview

When a new task is added to the graph, a lightweight system task (`.place-<task>`)
analyzes it and wires it into the correct position before it becomes eligible for
dispatch. This prevents tasks from running "free" (with no dependency context),
reducing file conflicts and ensuring work flows through the graph in the right order.

**Design principles:**
- On by default, optional per-task
- Cheap (haiku-tier agent, ~1-2k tokens)
- Respects user-provided `--after` deps, only adds additional edges
- Fast enough to not noticeably delay dispatch (< 30s)

---

## 1. Core Lifecycle

### Current flow (without placement)
```
wg add "Fix auth" --after login-task
  → task created (status: Open, paused: false)
  → coordinator tick: build_auto_assign_tasks() creates .assign-fix-auth (Done)
  → coordinator tick: spawn agent for fix-auth
```

### New flow (with placement)
```
wg add "Fix auth"  # no --after needed
  → task created (status: Open, paused: true)     # draft by default
  → coordinator tick: build_placement_tasks() creates .place-fix-auth
  → .place-fix-auth runs (haiku agent):
      - analyzes graph, files, artifacts
      - adds edges: wg edit fix-auth --add-after login-task
      - runs: wg publish fix-auth
  → task is now unpaused, eligible for dispatch
  → coordinator tick: build_auto_assign_tasks() creates .assign-fix-auth
  → coordinator tick: spawn agent for fix-auth
```

### State transitions
```
                  wg add (no flags)
                        │
                        ▼
              ┌──────────────────┐
              │   Open + paused  │  (draft)
              │   placed = false │
              └────────┬─────────┘
                       │
           .place-* agent runs
           (adds edges, then publishes)
                       │
                       ▼
              ┌──────────────────┐
              │  Open + !paused  │  (placed)
              │   placed = true  │
              └────────┬─────────┘
                       │
           .assign-* + spawn
                       │
                       ▼
              ┌──────────────────┐
              │   In-Progress    │
              └──────────────────┘
```

---

## 2. Placement Signals

The placer agent receives context and uses these signals (ordered by reliability):

### 2.1 File overlap (primary signal)
The strongest placement signal. If a task description mentions specific files or
modules, and another in-progress or recently-completed task has those files as
artifacts, they likely need to be serialized.

**Context provided to placer:**
- Artifacts from all non-terminal tasks (file paths)
- Artifacts from recently-completed tasks (last 10)
- File paths mentioned in the new task's description (extracted via regex)

**Example:** Task "Fix auth middleware" mentions `src/middleware/auth.rs`. Task
`refactor-auth` (in-progress) has artifact `src/middleware/auth.rs`. Placer adds
`--after refactor-auth`.

### 2.2 Git history of prior tasks
Completed tasks have commit hashes in their logs. The placer can use
`git diff --name-only <commit>` to see what files a prior task actually modified,
even if those files weren't recorded as artifacts.

**Context provided:** Last 5 completed task commit hashes + changed file lists.

### 2.3 Conceptual affinity
Tasks with similar naming prefixes, tags, or description keywords likely belong
in the same subgraph.

**Signals:**
- Tag overlap (e.g., both tagged `safety`)
- ID prefix overlap (e.g., `safety-*` tasks)
- Description keyword similarity (lightweight — shared nouns/verbs)

### 2.4 Integration gates
If the graph has integration/synthesis tasks (tasks with many `--after` deps),
new tasks in those domains should wire before the integration task.

**Context provided:** List of integration tasks (tasks with 5+ after deps) and
their domains.

### 2.5 Conflict detection
If two tasks would modify the same file in parallel (neither depends on the other),
the placer should serialize them by adding an edge.

**Rule:** Same files = sequential edges. Never parallelize tasks that touch the
same files.

---

## 3. Opt-in / Opt-out Mechanism

### Default behavior: placement ON
All tasks created via `wg add` (without `--after` deps) are created in draft
(paused) state and get a `.place-*` system task.

### Skip placement
```bash
# Explicit no-placement — task stays free-standing
wg add "Quick fix" --no-place

# Task with explicit deps — placement optional but still runs
wg add "Step 2" --after step-1
```

**`--no-place` flag behavior:**
- Task is created with `paused: false` (not draft)
- No `.place-*` task is created
- Task becomes immediately eligible for dispatch
- An `unplaced: true` field on the task records that this was intentional

### When user provides `--after` deps
If the user provides `--after`, placement still runs by default, but:
- The placer **respects** user-provided deps (never removes them)
- The placer **adds** additional edges if it detects file conflicts or missing
  integration gate connections
- The placer may determine no additional edges are needed and simply publish

To skip placement entirely when deps are provided:
```bash
wg add "Step 2" --after step-1 --no-place
```

### Behavioral matrix

| Flags | Draft? | .place-* created? | Notes |
|---|---|---|---|
| `wg add "Task"` | Yes | Yes | Full placement flow |
| `wg add "Task" --after X` | Yes | Yes | Placer respects X, may add more |
| `wg add "Task" --no-place` | No | No | Free-standing, immediate dispatch |
| `wg add "Task" --after X --no-place` | No | No | User-specified deps only |
| `wg add "Task" --paused` | Yes | No | User-managed draft (existing behavior) |
| `wg add ".system-task"` | No | No | System tasks skip placement |

---

## 4. Placement Hints

### `--place-near` syntax
```bash
wg add "Fix auth token refresh" --place-near refactor-auth,security-audit
```

**Semantics:**
- Hints are suggestions, not mandates
- The placer uses them as starting context: examines those tasks' artifacts,
  deps, and position to inform placement
- The placer may add edges to, from, or near the hinted tasks
- The placer may also add edges to tasks not in the hint list

### `--place-before` syntax
```bash
wg add "Validate auth" --place-before integrate-spark-v3
```

**Semantics:**
- Stronger hint: the new task should complete before the specified task
- Placer adds `new-task` to the specified task's `after` list
- Still non-dogmatic: placer can add more edges beyond this

### Implementation
Hints are stored as task metadata (not persisted as graph edges) and passed to
the `.place-*` task's description:

```
Placement hints:
  near: refactor-auth, security-audit
  before: integrate-spark-v3
```

---

## 5. Automation vs. Agent Placement

### Decision boundary

| Condition | Action | Rationale |
|---|---|---|
| User provided `--after` deps + task has no file overlap with active tasks | **Auto-place**: just publish | User already specified position, no conflicts |
| User provided `--after` deps + file overlap detected | **Agent-place** | Need intelligent conflict serialization |
| No `--after` deps, hints provided | **Agent-place** | Need to analyze graph position |
| No `--after` deps, no hints | **Agent-place** | Full analysis needed |
| System task (dot-prefix) | **Skip** | System tasks don't need placement |

### Auto-placement (fast path)
When auto-placement triggers, the coordinator skips creating a `.place-*` task
entirely:
1. Check if user-provided `--after` deps exist
2. Check if any active task's artifacts overlap with files mentioned in the
   new task's description
3. If no overlap → publish immediately (no agent needed)
4. Log the auto-placement decision to the task's log

**Cost:** ~0 (no LLM call). **Latency:** < 1 second.

### Agent-placement (full path)
When agent-placement triggers:
1. Create `.place-<task-id>` system task
2. Dispatch to haiku-tier agent with placement context
3. Agent analyzes signals and calls `wg edit <task> --add-after <dep>` for each
   edge
4. Agent calls `wg publish <task>` when done

**Cost:** ~$0.001 (haiku, ~1-2k tokens). **Latency:** 10-30 seconds.

### Context provided to the placer agent

The `.place-*` task description includes:

```
## Task to place
ID: fix-auth-refresh
Title: Fix auth token refresh
Description: [full description]
Files mentioned: src/middleware/auth.rs, src/auth/token.rs

## Placement hints
near: refactor-auth, security-audit

## Active tasks (non-terminal)
- refactor-auth (in-progress): artifacts=[src/middleware/auth.rs, src/auth/mod.rs]
- security-audit (open): artifacts=[]
- tui-fix-paste (in-progress): artifacts=[src/tui/editor.rs]

## Recently completed tasks (last 10)
- login-flow (done): artifacts=[src/auth/login.rs], commit=abc123
  changed files: src/auth/login.rs, src/auth/session.rs

## Integration gates
- integrate-spark-v3: 40 deps, domains=[safety, infra, agency, human, tui]

## Instructions
1. Analyze file overlap between the new task and active tasks
2. Add dependency edges using: wg edit fix-auth-refresh --add-after <dep-id>
3. If the task belongs before an integration gate, add it:
   wg edit integrate-spark-v3 --add-after fix-auth-refresh
4. When done, publish the task: wg publish fix-auth-refresh
5. If placement is ambiguous, publish anyway — some placement is better than none
```

---

## 6. Assigner Integration

### Recommendation: Separate (placer runs first)

**Pipeline:** `.place-*` → publish → `.assign-*` → spawn

**Rationale:**
- **Separation of concerns**: Placement is about graph topology; assignment is
  about agent/model selection. Mixing them creates a "god task" that's hard to
  debug and evaluate.
- **Placement needs to complete before assignment**: The assigner's context_scope
  and exec_mode decisions depend on the task's position in the graph (e.g., a
  task deep in a dependency chain might need full context). The assigner can't
  make good decisions without knowing the task's neighbors.
- **Different model tiers**: Placement is a haiku-tier job (cheap, fast).
  Assignment is already haiku-tier via lightweight LLM. Keeping them separate
  means each can use its optimal model.
- **Idempotency**: If placement fails, only placement needs to retry. If
  assignment fails, only assignment needs to retry. Combined tasks are
  all-or-nothing.

### Rejected alternatives

**Combined (assigner checks placement inline):**
- Pro: One fewer system task, slightly faster
- Con: Assigner becomes more complex, harder to evaluate independently, model
  tier conflict (assigner might need a smarter model for placement)
- Con: If the assigner is already doing lightweight LLM, adding graph analysis
  makes it medium-weight

**Assigner-gated (assigner refuses unplaced tasks):**
- Pro: Explicit gate, easy to understand
- Con: Creates a dependency loop: assigner waits for placer, but who creates the
  placer? Coordinator already handles this, so the gate adds complexity without
  benefit.

### Coordinator tick phases (updated)

```
Phase 1: Evaluate wait conditions, cycle iterations, failure restarts
Phase 2: Build placement tasks for draft tasks          ← NEW
Phase 3: Build auto-assign tasks for placed, ready tasks
Phase 4: Build evaluation tasks for completed tasks
Phase 5: Spawn agents for ready tasks
```

Phase 2 runs before Phase 3, ensuring that by the time the assigner sees a task,
it's already been placed (or auto-placed).

---

## 7. Draft-by-Default Workflow

### Interaction with `wg publish`
`wg publish` already exists and unpauses a task + its downstream subgraph. The
placement system uses this: after adding edges, the placer calls `wg publish`.

No changes needed to the publish command itself.

### `--no-place` / `--immediate` flag
- `--no-place` is the recommended flag name (clear, matches system naming)
- `--immediate` could be an alias but adds cognitive overhead. Prefer one flag.

### When does the coordinator create `.place-*` tasks?
**On coordinator tick (Phase 2)**, not on `wg add`:
- `wg add` creates the task in draft state
- Next coordinator tick sees the draft task and creates `.place-*`
- This avoids making `wg add` depend on the coordinator being running

**Detection logic:**
```rust
fn build_placement_tasks(graph: &mut WorkGraph, config: &Config, dir: &Path) -> bool {
    let tasks_needing_placement: Vec<_> = graph.tasks()
        .filter(|t| {
            t.paused                              // Draft task
            && !t.unplaced                        // Not intentionally unplaced
            && !is_system_task(&t.id)             // Not a system task
            && !graph.get_task(&format!(".place-{}", t.id)).is_some()  // No existing placer
            && !t.tags.iter().any(|tag| tag == "placed")               // Not already placed
        })
        .map(|t| (t.id.clone(), t.title.clone(), t.description.clone(), t.after.clone()))
        .collect();

    // For each: decide auto-place vs agent-place...
}
```

### System tasks (dot-prefix) skip placement
System tasks (`.assign-*`, `.evaluate-*`, `.place-*`) are created by the
coordinator with known positions. They skip placement entirely:
- They're created with `paused: false`
- The `is_system_task()` check excludes them from Phase 2
- This prevents infinite regress (`.place-.place-*`)

---

## 8. Naming

### Recommendation: `.place-*`

| Name | Pros | Cons |
|---|---|---|
| `.place-*` | Intuitive, matches spatial metaphor ("place in the graph"), user preference | Slightly ambiguous (could mean "location") |
| `.slot-*` | Implies fitting into a position | Less common verb, might confuse |
| `.wire-*` | Implies connecting edges | Too mechanical, sounds like infrastructure |

`.place-*` is the clear winner. It's intuitive, concise, and already the user's
stated preference.

---

## 9. Key Design Questions

### What context does the placer agent need?
Minimal viable context (to keep cost < $0.005):
1. The new task's title, description, and any mentioned file paths (~200 tokens)
2. List of active tasks with their artifacts (~300 tokens for 20 tasks)
3. List of integration gates (~100 tokens)
4. Placement hints if any (~50 tokens)
5. Instructions (~200 tokens)

**Total: ~850 tokens input, ~100 tokens output ≈ $0.001 on haiku.**

Additional context (if available, but not required):
- Recent git log (last 5 commits with changed files)
- File tree of `src/` top-level directories

### How to handle ambiguous placement?
**Publish anyway.** Some placement is better than none. If the placer can't
determine dependencies with confidence, it should:
1. Log its uncertainty: `wg log <task> "Placement uncertain — no clear file overlap or conceptual affinity found"`
2. Publish the task (it becomes a free-standing task, like `--no-place`)
3. The task will be dispatched and may discover its own dependencies at runtime

### Should placement be reversible?
**Not in v1.** Placement adds edges, which can be removed with `wg edit --remove-after`.
A dedicated `wg unplace` / `wg replace` command adds complexity without clear need.
If the placer makes a mistake, the user or another agent can fix it manually.

**Future consideration:** If placement errors are common, add `wg replace <task>`
which removes placer-added edges (tracked via provenance) and re-runs placement.

### Performance
- **Auto-placement:** < 1 second (no LLM, pure graph analysis)
- **Agent-placement:** 10-30 seconds (haiku LLM call)
- **Coordinator tick interval:** typically 5-10 seconds
- **Total added latency:** 1-2 ticks (5-20 seconds for auto, 15-40 seconds for agent)

This is acceptable. Tasks already wait for assignment (another tick) and spawning.
The placement phase adds one tick of latency in the common case.

---

## 10. Concrete Examples

### Example 1: New task with file overlap (agent placement)

**Current graph state:**
```
refactor-auth (in-progress)
  artifacts: [src/middleware/auth.rs, src/auth/mod.rs]

security-audit (open, waiting for refactor-auth)
  after: [refactor-auth]

integrate-spark-v3 (open, 40+ deps)
  after: [..., security-audit, ...]
```

**User adds:**
```bash
wg add "Fix auth token expiry validation" \
  -d "Fix the token expiry check in src/middleware/auth.rs — currently allows expired tokens through"
```

**What happens:**
1. Task `fix-auth-token` created in draft state (paused=true)
2. Coordinator tick Phase 2: detects draft task, sees file overlap with
   `refactor-auth` (both touch `src/middleware/auth.rs`)
3. Creates `.place-fix-auth-token` system task
4. Haiku agent runs, receives context showing:
   - `refactor-auth` is in-progress with artifact `src/middleware/auth.rs`
   - `security-audit` depends on `refactor-auth`
   - `integrate-spark-v3` is an integration gate depending on `security-audit`
5. Agent decides:
   - `fix-auth-token` must run after `refactor-auth` (file overlap)
   - `fix-auth-token` should run before `integrate-spark-v3` (same domain)
   - Could run in parallel with `security-audit` if they don't touch the same files
6. Agent runs:
   ```bash
   wg edit fix-auth-token --add-after refactor-auth
   wg edit integrate-spark-v3 --add-after fix-auth-token
   wg publish fix-auth-token
   ```
7. Task is now placed and eligible for assignment/dispatch

### Example 2: Task with explicit deps, no conflicts (auto-placement)

**User adds:**
```bash
wg add "Write unit tests for login flow" --after login-flow \
  -d "Add tests for the login flow in tests/test_login.rs"
```

**What happens:**
1. Task `write-unit-tests` created in draft state (paused=true)
2. Coordinator tick Phase 2: detects draft task with `--after login-flow`
3. Auto-placement check: scans active task artifacts for `tests/test_login.rs`
4. No overlap found (no other task touches test files)
5. Auto-places: publishes immediately, no `.place-*` task created
6. Logs: "Auto-placed: user deps sufficient, no file conflicts detected"

### Example 3: Free-standing task with hints

**User adds:**
```bash
wg add "Research WebSocket support" --place-near tui-firehose-inspector \
  -d "Research adding WebSocket streaming for live agent output in the TUI"
```

**What happens:**
1. Task `research-websocket` created in draft state
2. Coordinator tick: creates `.place-research-websocket`
3. Haiku agent receives hint `near: tui-firehose-inspector`
4. Agent examines `tui-firehose-inspector`:
   - It's a TUI task, currently in-progress
   - Other TUI tasks exist: `tui-unified-markdown`, `tui-remap-panel`
   - Integration gate `integrate-spark-v3` depends on all TUI tasks
5. Agent decides:
   - Research can run in parallel with TUI implementation tasks (no file overlap)
   - But should complete before integration validation
   - No need to serialize with any specific task
6. Agent runs:
   ```bash
   wg edit integrate-spark-v3 --add-after research-websocket
   wg publish research-websocket
   ```

---

## 11. Implementation Plan

### Task Breakdown

#### Phase 1: Core infrastructure (4 tasks, pipeline)

1. **Add `unplaced` and `placed` fields to Task struct**
   - Add `unplaced: bool` (default false) and tag-based `placed` tracking
   - Update `wg add` to accept `--no-place` flag
   - When `--no-place`: set `unplaced=true`, `paused=false`
   - When no `--no-place` and no explicit `--paused`: set `paused=true` (draft-by-default)
   - Files: `src/graph.rs`, `src/cli.rs`, `src/commands/add.rs`
   - Verify: `cargo test`, `wg add --no-place` creates unpaused task

2. **Add `DispatchRole::Placer` to config**
   - Add Placer variant to `DispatchRole` enum
   - Default model: `haiku` (budget tier)
   - Add `placer_agent` field to `AgencyConfig`
   - Files: `src/config.rs`
   - Verify: `cargo test`, placer resolves to haiku by default

3. **Implement `build_placement_tasks()` in coordinator**
   - New Phase 2 function in `coordinator.rs`
   - Detects draft tasks needing placement
   - Implements auto-placement fast path (publish immediately if deps exist + no
     file conflicts)
   - Creates `.place-*` system tasks for agent placement
   - Builds placement context (active tasks, artifacts, integration gates)
   - Files: `src/commands/service/coordinator.rs`
   - Verify: integration test showing draft task gets `.place-*` task

4. **Add `--place-near` and `--place-before` flags to `wg add`**
   - Store hints as task metadata (new field or in description)
   - Pass hints through to `.place-*` task description
   - Files: `src/cli.rs`, `src/commands/add.rs`, `src/graph.rs`
   - Verify: `wg add --place-near foo` includes hint in task

#### Phase 2: Placer agent prompt (2 tasks)

5. **Design and implement placer agent prompt template**
   - Create prompt builder function that assembles placement context
   - Include: task info, active tasks + artifacts, integration gates, hints,
     instructions
   - Keep under 2000 tokens total
   - Files: `src/agency/prompt.rs` or new `src/placement.rs`
   - Verify: prompt renders correctly with test data

6. **Wire placer execution into coordinator spawn logic**
   - Placer tasks dispatch like eval tasks: lightweight, inline or shell
   - Placer runs `wg edit` + `wg publish` commands
   - Handle placer failure: if `.place-*` fails, publish task anyway (fallback)
   - Files: `src/commands/service/coordinator.rs`
   - Verify: end-to-end test with mock executor

#### Phase 3: Integration and testing (2 tasks)

7. **Integration tests**
   - Test: draft task → `.place-*` created → task placed → dispatch
   - Test: `--no-place` skips placement
   - Test: auto-placement fast path
   - Test: system tasks skip placement
   - Test: placer failure → fallback publish
   - Files: `tests/integration_placement.rs`

8. **Documentation and agent guide update**
   - Update `docs/AGENT-GUIDE.md` with placement info
   - Update `wg quickstart` output to mention placement
   - Files: `docs/`, `src/commands/quickstart.rs`

### Dependency graph
```
[1: Task struct] → [2: DispatchRole] → [3: build_placement_tasks]
                                             │
[4: CLI flags] ──────────────────────────────┘
                                             │
                                             ▼
                                     [5: Prompt template]
                                             │
                                             ▼
                                     [6: Coordinator wiring]
                                             │
                                             ▼
                                     [7: Integration tests]
                                             │
                                             ▼
                                     [8: Documentation]
```

### Estimated cost
- 8 implementation tasks
- Placement agent: haiku-tier ($0.001/placement)
- Total development: ~4-6 agent sessions

---

## 12. Tradeoff Summary

| Decision | Chosen | Alternative | Rationale |
|---|---|---|---|
| Default behavior | Placement ON | Placement OFF | Prevents free-running tasks, catches file conflicts |
| Placer model | Haiku | Sonnet | Placement is pattern-matching, not creative work. Haiku is 10x cheaper |
| Assigner integration | Separate pipeline | Combined/gated | Separation of concerns, independent failure modes |
| Draft-by-default | Yes (paused=true) | No (existing behavior) | Required for placement to work; tasks must wait for placement |
| Skip for --after tasks | Still run placement | Skip entirely | User deps may be incomplete; placer catches missing edges |
| System task naming | `.place-*` | `.slot-*`, `.wire-*` | User preference, most intuitive |
| Ambiguous placement | Publish anyway | Block/fail | Some placement > no placement; don't delay dispatch for uncertainty |
| Reversibility | Not in v1 | `wg unplace` command | YAGNI; manual edge editing suffices |
| Hint syntax | `--place-near`, `--place-before` | `--hint` generic | Specific flags are more discoverable |
| Auto vs agent boundary | Deps provided + no file overlap → auto | Always agent | Saves agent cost for trivial cases |
| Placer failure handling | Fallback publish | Retry / block | Don't let placer failures block the pipeline |
