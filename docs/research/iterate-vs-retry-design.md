# Design: Iterate vs Retry — Spiral Re-execution Semantics

**Date**: 2026-04-01
**Task**: design-iterate-vs
**Depends on**: design-cycle-to (spiral unrolling design), research-spiral-cycle (gap analysis)
**Status**: Design complete

---

## Executive Summary

Workgraph currently offers two re-execution primitives: **retry** (amnesia reset of failed tasks) and **cycles** (planned repetition with convergence). Neither handles the middle ground: *unplanned re-execution with accumulated context*. This design introduces **`wg iterate`** — a command that re-opens a completed or failed task while structuring prior attempt history into a handoff document that the next agent receives as part of its context.

**Key insight**: Iterate is not retry with memory, nor a 1-iteration cycle. It is a *spiral* — the same work point re-entered at a higher knowledge level. The predecessor's successes, failures, partial artifacts, and diagnostic observations become first-class input to the successor.

**Relationship to spiral unrolling**: The spiral unrolling design (design-cycle-to) addresses *planned* iteration within cycles. This design addresses *unplanned* re-execution of any task — including non-cycle tasks — where prior attempt context should carry forward. The two designs share the `~` archive ID scheme but are otherwise independent. Spiral cycles produce archives automatically on each cycle reset; iterate produces them on-demand when a human or coordinator decides more work is needed.

---

## Design Question Answers

### 1. What does "iterate" mean vs "retry"?

**Retry** is amnesia re-execution. The task resets to Open, the next agent gets the original task description and nothing else. The agent has no knowledge of what was attempted before, what failed, or what was partially accomplished. This is appropriate when:
- The failure was environmental (network timeout, OOM, stale lock)
- The original approach was sound; it just needs a clean run
- Prior attempt produced no useful intermediate state

**Iterate** is spiral re-execution. The task archives its current state (creating `{task-id}~{N}`), resets to Open, and the next agent receives a structured handoff containing:
- What the predecessor accomplished (artifacts, commits, partial work)
- Where they got stuck (failure reason, last log entries, diagnostic observations)
- An LLM-generated attempt summary that distills the above into actionable guidance

This is appropriate when:
- The agent made partial progress that should be preserved and built upon
- The failure reveals information about what approach to try next
- The work needs refinement rather than restart (e.g., evaluation rejected output quality)
- A human wants to redirect the approach based on what they saw in the first attempt

**The spiral metaphor**: Each iteration revisits the same logical work point but at a higher elevation — with more knowledge, more context, and (ideally) a better approach. Retry is horizontal (same point, same elevation); iterate is upward (same point, higher elevation).

| Aspect | `wg retry` | `wg iterate` |
|--------|-----------|-------------|
| Input status | Failed only | Failed, Done, or InProgress |
| Prior context | None (amnesia) | Structured handoff |
| Archive created | No | Yes (`{task-id}~{N}`) |
| Agent output preserved | Only in log archives | In archive task + handoff |
| Iteration counter | `retry_count` incremented | `iterate_count` incremented |
| Artifacts | Lost (task reset) | Moved to archive |
| Use case | Environmental failures | Partial progress, quality iteration |

### 2. How should accumulated context be surfaced?

**Decision: Structured handoff with LLM-generated summary.**

The handoff is assembled in three tiers, from raw to synthesized:

#### Tier 1: Structured metadata (always included)
Injected into the spawned agent's context as a `## Prior Iterations` section:

```markdown
## Prior Iterations

This task has been iterated 2 times. You are attempt #3.

### Attempt #1 (archived as implement-auth~0)
- **Status**: Failed
- **Agent**: agent-7842 (Careful Programmer)
- **Duration**: 12m 34s
- **Failure reason**: cargo test test_auth_rejects_expired_token failed — JWT expiry check used system time instead of token's `iat` claim
- **Artifacts**: src/auth/handler.rs, src/auth/jwt.rs (see implement-auth~0)
- **Commits**: a1b2c3d "feat: add POST /auth/token endpoint (implement-auth)"

### Attempt #2 (archived as implement-auth~1)
- **Status**: Done (evaluation rejected, score 0.41)
- **Agent**: agent-9103 (Thorough Architect)
- **Duration**: 8m 12s
- **Rejection reason**: Missing rate limiting, no input validation on token refresh
- **Artifacts**: src/auth/handler.rs, src/auth/jwt.rs, tests/test_auth.rs (see implement-auth~1)
- **Commits**: d4e5f6a "feat: fix JWT expiry, add refresh endpoint (implement-auth)"
```

#### Tier 2: Log tail (always included)
The last 10 log entries from each prior iteration, giving the successor a timeline of what happened.

#### Tier 3: LLM-generated attempt summary (opt-in, `--summarize`)
An LLM call that reads the prior attempts' archives and produces a concise handoff note. This is the highest-value context but costs tokens and time.

**Summary generation prompt** (runs at iterate time, stored on the task):
```
You are generating a handoff summary for a task that is being re-attempted.

Prior attempt details:
{structured metadata from Tier 1}
{log entries from Tier 2}
{failure reason and evaluation notes if available}

Write a concise handoff (3-8 sentences) covering:
1. What was accomplished (working code, passing tests, artifacts created)
2. Where the work stalled or was rejected (specific failure point)
3. What approach the next agent should consider (based on observed failure)

Do not repeat the task description. Focus on what the next agent needs to know
that they wouldn't know from the task description alone.
```

The generated summary is stored in the task's `iterate_context` field and injected into the agent's prompt as a `## Handoff from Prior Attempt` section.

**Why not just raw logs?** Raw logs are noisy — they contain progress markers, validation outputs, timing data, and internal tooling chatter. An agent processing raw logs from a 15-minute session would waste significant context window on irrelevant detail. The structured metadata gives signal; the LLM summary gives interpretation; the raw logs are available via `wg show {task-id}~{N}` if the agent needs to dig deeper.

### 3. CLI Interface

**Decision: New command `wg iterate`, not a flag on retry.**

Rationale:
- The semantics are fundamentally different (context-preserving vs amnesia)
- The input status constraints differ (retry: Failed only; iterate: Failed, Done, InProgress)
- The data flow differs (iterate creates archives; retry does not)
- A flag (`wg retry --spiral`) would be confusing — is it a retry or an iteration?
- Separate commands make intent clear in provenance logs and audit trails

#### Command: `wg iterate <task-id>`

```
wg iterate <task-id> [OPTIONS]

Re-open a task with accumulated context from prior attempts.
Archives the current task state as {task-id}~{N} before resetting.

OPTIONS:
    --summarize          Generate an LLM handoff summary (costs tokens)
    --note <text>        Human-provided guidance for the next attempt
                         (e.g., "Try using the jose crate instead of jsonwebtoken")
    --max-iterations <N> Set maximum iterate count (default: unlimited)
    --force              Allow iterating an in-progress task (kills current agent)
    --model <model>      Model for summary generation (default: haiku)

EXAMPLES:
    # Iterate a failed task with automatic summary
    wg iterate implement-auth --summarize

    # Iterate a done-but-rejected task with human guidance
    wg iterate implement-auth --note "The rate limiter should use a token bucket, not sliding window"

    # Iterate with both
    wg iterate implement-auth --summarize --note "Focus on the JWT validation path"
```

#### Status transitions

```
Failed     → wg iterate → Open  (archive created as {id}~{N})
Done       → wg iterate → Open  (archive created as {id}~{N})
InProgress → wg iterate --force → Open  (current agent killed, archive created)
Open       → wg iterate → Error ("task is already open, nothing to iterate")
Abandoned  → wg iterate → Error ("task is abandoned; use wg retry or wg resume")
```

#### Comparison table

| Command | Input Status | Archive? | Context? | Counter |
|---------|-------------|----------|----------|---------|
| `wg retry` | Failed | No | None | `retry_count++` |
| `wg iterate` | Failed/Done/InProgress | Yes | Structured handoff | `iterate_count++` |
| `wg requeue` | InProgress | No | None | `triage_count++` |
| Cycle reset | Done (all members) | With `--spiral` | Cycle metadata | `loop_iteration++` |

### 4. Relationship to Cycles

Iterate and cycles are orthogonal mechanisms that can compose:

**Cycles** model *planned repetition* — you know in advance that the work will repeat. The cycle definition encodes the repetition structure (max iterations, guards, delays, convergence criteria). Cycles are structural: they exist in the graph topology as back-edges.

**Iterate** models *unplanned re-execution* — you didn't expect to need another attempt, but the first one wasn't good enough. Iterate is operational: it's a command, not a graph structure.

#### Is iterate a 1-iteration cycle? No.

A cycle has:
- Multiple members that coordinate (A→B→C→A)
- A config owner with `cycle_config`
- Automatic reset when all members reach terminal
- `loop_iteration` tracking
- Convergence semantics (`--converged`)

Iterate has:
- A single task re-executed
- No structural back-edge
- Manual trigger only
- Its own `iterate_count` counter
- No convergence signal (the human/coordinator decides when to stop)

#### Composition: iterating a cycle member

If a task is part of a cycle and you `wg iterate` it individually:
- The archive `{task-id}~{N}` is created outside the cycle structure
- The live task resets to Open within the cycle
- The cycle's `loop_iteration` is *not* incremented (this isn't a cycle iteration)
- The task's `iterate_count` is incremented
- When the cycle next evaluates, the iterated task participates normally

This handles the case where one member of a cycle produced poor output and needs a redo before the cycle should advance.

#### Composition: iterate + spiral mode

When a task is in a spiral-enabled cycle:
- Spiral archives (`{task-id}~0`, `~1`, etc.) track planned cycle iterations
- Iterate archives (`{task-id}~iter-0`, etc.) track unplanned re-executions

To avoid ID collision, iterate archives use the prefix `~iter-` instead of bare `~`:
- Spiral: `build-report~0`, `build-report~1` (cycle iteration 0, 1)
- Iterate: `build-report~iter-0`, `build-report~iter-1` (unplanned iteration 0, 1)

The `parse_spiral_id()` function from the spiral design distinguishes them:
```rust
fn parse_archive_id(id: &str) -> Option<ArchiveType> {
    let (source, suffix) = id.rsplit_once('~')?;
    if let Some(iter_str) = suffix.strip_prefix("iter-") {
        let n = iter_str.parse::<u32>().ok()?;
        Some(ArchiveType::Iterate { source, iteration: n })
    } else {
        let n = suffix.parse::<u32>().ok()?;
        Some(ArchiveType::Spiral { source, iteration: n })
    }
}
```

### 5. Metadata Preserved Across Iterations

#### Moved to archive (cleared from live task)

| Field | Rationale |
|-------|-----------|
| `artifacts` | Per-attempt artifacts lose provenance if accumulated on the live task |
| `token_usage` | Per-attempt cost tracking |
| `session_id` | Links to the specific agent session |
| `assigned` | Which agent worked this attempt |
| `started_at` | Per-attempt timing |
| `completed_at` | Per-attempt timing |
| `checkpoint` | Stale from prior attempt |
| `failure_reason` | Per-attempt failure info |

#### Preserved on live task (structural)

| Field | Rationale |
|-------|-----------|
| `description` | The work specification doesn't change (but `--note` appends to it) |
| `verify` | Validation criteria remain the same |
| `after` / `before` | Graph structure unchanged |
| `tags` | Structural metadata |
| `skills` | Required capabilities |
| `model` | Preferred model (can be overridden per attempt) |
| `agent` | Agency assignment persists across iterations |
| `cycle_config` | Cycle structure unchanged |
| `loop_iteration` | Cycle iteration counter (independent of iterate counter) |

#### New field: `iterate_context`

Stored on the live task after `wg iterate`. Contains the structured handoff (Tier 1 + Tier 2 metadata) and optionally the LLM summary (Tier 3). Injected into the agent's prompt by `build_task_context()` in `src/commands/spawn/context.rs`.

```rust
/// Structured context from prior iterate attempts.
/// Set by `wg iterate`, consumed by spawn context assembly.
/// Contains markdown-formatted handoff with attempt history,
/// artifacts, failure reasons, and optionally an LLM summary.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub iterate_context: Option<String>,
```

#### New field: `iterate_count`

Tracks how many times this task has been iterated (distinct from `retry_count` and `loop_iteration`).

```rust
/// Number of times this task has been iterated (via wg iterate).
/// Each iteration creates an archive task and preserves prior context.
#[serde(default, skip_serializing_if = "is_zero")]
pub iterate_count: u32,
```

### 6. LLM-Generated Attempt Summary

**Decision: Opt-in via `--summarize` flag, generated at iterate time, stored on the task.**

#### Why opt-in?

- Not all iterations need a summary (sometimes `--note "try X instead"` is sufficient)
- Summary generation costs tokens and time (~3-5 seconds with haiku)
- For simple failure-and-retry patterns, the structured metadata (Tier 1) is enough

#### When is it most valuable?

- Multi-attempt tasks where the failure mode is subtle
- Evaluation-rejected tasks where the evaluator's feedback needs synthesis with the agent's work
- Tasks where a human redirects the approach and wants the LLM to contextualize the pivot

#### Generation flow

```
wg iterate implement-auth --summarize
  │
  ├─ 1. Read current task state (artifacts, logs, failure_reason)
  ├─ 2. Read any existing iterate archives ({task-id}~iter-*)
  ├─ 3. Assemble structured metadata (Tier 1)
  ├─ 4. Call LLM with summary prompt + structured metadata
  │     Model: --model flag or default haiku (cheap, fast)
  │     Max tokens: 500 (concise handoff)
  ├─ 5. Combine Tier 1 + Tier 2 (log tail) + Tier 3 (summary)
  ├─ 6. Store combined handoff in task.iterate_context
  ├─ 7. Create archive task {task-id}~iter-{N}
  ├─ 8. Reset live task to Open
  └─ 9. Log: "Iterated (attempt #{N+1}), archived as {task-id}~iter-{N}"
```

#### Summary storage

The summary is stored in `iterate_context` on the live task. It accumulates across iterations:

```markdown
## Handoff from Prior Attempts

### Attempt #1 → #2 Summary
The first attempt implemented the auth endpoint but used system time for JWT
expiry checks. The test_auth_rejects_expired_token test fails because tokens
with `iat` in the past are accepted. The successor should use the token's own
`iat` + `exp` claims for validation. The endpoint structure and routing are
correct and should be preserved.

### Attempt #2 → #3 Summary
JWT validation was fixed, but the evaluator rejected the work for missing
rate limiting and input validation. The auth handler processes token refresh
requests without any throttling. Consider adding middleware-level rate limiting
using tower::limit or a custom token-bucket implementation.
```

Each `wg iterate --summarize` appends a new section. The full history is available to the next agent, showing the progression of attempts and lessons learned.

---

## Data Model Changes

### Task struct (`src/graph.rs`)

Two new fields:

```rust
pub struct Task {
    // ... existing fields ...

    /// Number of unplanned re-executions via `wg iterate`.
    /// Distinct from retry_count (amnesia re-runs) and loop_iteration (cycle iterations).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub iterate_count: u32,

    /// Structured handoff context from prior iterate attempts.
    /// Set by `wg iterate`, injected into agent prompt by build_task_context().
    /// Contains: structured metadata, log tails, optional LLM summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterate_context: Option<String>,
}
```

Both fields use `#[serde(default)]` for backward compatibility — existing tasks without these fields deserialize with `iterate_count: 0` and `iterate_context: None`.

### Archive task identity

Iterate archives use the `~iter-{N}` suffix:
- `implement-auth~iter-0` (first iterate archive)
- `implement-auth~iter-1` (second iterate archive)

This distinguishes them from spiral cycle archives (`~{N}` without prefix):
- `build-report~0` (spiral cycle iteration 0)

### TaskHelper (`src/graph.rs`)

Add corresponding fields to the deserialization helper:

```rust
#[serde(default)]
iterate_count: u32,
#[serde(default)]
iterate_context: Option<String>,
```

### No changes to CycleConfig

Iterate is independent of cycle semantics. No CycleConfig modifications needed.

### No changes to Evaluation struct

Evaluations of iterate archives use the archive task ID (`implement-auth~iter-1`), just as spiral archives use their archive IDs.

---

## Context Injection

### Modification to `build_task_context()` (`src/commands/spawn/context.rs`)

After the existing dependency context and cycle metadata injection, add:

```rust
// Inject iterate handoff context if present
if let Some(ref ctx) = task.iterate_context {
    context_parts.push(ctx.clone());
}
```

This is a 3-line change. The `iterate_context` field contains pre-formatted markdown that can be injected directly.

### Modification to agent prompt template (`src/service/executor.rs`)

No change needed. The `iterate_context` flows through `build_task_context()` which is already included in the `## Context from Dependencies` section of the agent prompt. The markdown headers (`## Prior Iterations`, `## Handoff from Prior Attempts`) naturally structure it within the prompt.

---

## CLI Implementation

### New command: `wg iterate`

Add to `src/commands/iterate.rs` and wire into `src/cli.rs` / `src/main.rs`.

Core flow:

```rust
pub fn run(dir: &Path, id: &str, opts: IterateOpts) -> Result<()> {
    // 1. Load graph, validate task status
    // 2. Assemble structured handoff (Tier 1 + Tier 2)
    // 3. If --summarize: call LLM for Tier 3 summary
    // 4. If --note: append human guidance
    // 5. Create archive task: {id}~iter-{iterate_count}
    //    - Copy: all per-attempt fields (see table above)
    //    - Move: artifacts from live to archive
    //    - Tag: "iterate-archive"
    // 6. Update live task:
    //    - iterate_count += 1
    //    - iterate_context = assembled handoff
    //    - status = Open
    //    - Clear: assigned, started_at, completed_at, token_usage,
    //             session_id, checkpoint, failure_reason
    //    - Preserve: description, verify, after, before, tags, skills,
    //               model, agent, cycle_config, loop_iteration
    // 7. Log: "Iterated (attempt #{N+1}), archived as {id}~iter-{N}"
    // 8. If --force: kill current agent process
    // 9. Record provenance
}
```

### Modified command: `wg show`

When displaying a task with `iterate_count > 0`, append:

```
Iterate history: 2 prior attempts
  - implement-auth~iter-0 (Failed, 12m 34s, agent-7842)
  - implement-auth~iter-1 (Done/rejected, 8m 12s, agent-9103)
```

### New command: `wg iterations`

Already proposed in the spiral design. Extend to show both spiral and iterate archives:

```
$ wg iterations implement-auth
Spiral iterations: (none)
Iterate attempts:
  #0  implement-auth~iter-0  Failed      12m 34s  agent-7842  2026-04-01T10:00:00Z
  #1  implement-auth~iter-1  Done        8m 12s   agent-9103  2026-04-01T11:00:00Z
  Current: implement-auth     Open (attempt #3)
```

### Cycle analysis filter

Add iterate archives to the system scaffolding filter:

```rust
fn is_system_scaffolding(id: &str) -> bool {
    id.starts_with(".assign-")
        || id.starts_with(".flip-")
        || id.starts_with(".evaluate-")
        || id.starts_with(".place-")
        || id.contains("~iter-")  // iterate archive
        || id.contains('~')       // spiral archive (from spiral design)
}
```

---

## Usage Scenarios

### Scenario 1: Failure Recovery with Accumulated Context

**Situation**: An agent implementing an auth endpoint fails because the JWT library API changed between versions. The agent's partial work (endpoint routing, request parsing, test scaffolding) is good, but the JWT validation code is wrong.

**Without iterate (current behavior)**:
```bash
wg retry implement-auth
# New agent gets original task description only.
# It may repeat the same JWT library mistake.
# It has to re-discover that endpoint routing is already done.
# Wasted work: reimplements what already works.
```

**With iterate**:
```bash
wg iterate implement-auth --summarize --note "Use jose crate v4 API, not v3"
# Archive created: implement-auth~iter-0
#   Contains: all partial code, test scaffolding, failure logs
# New agent receives:
#   - Structured metadata showing the JWT validation failure
#   - LLM summary: "Endpoint routing and request parsing are complete and tested.
#     JWT validation failed because jsonwebtoken 9.x changed the decode() signature.
#     The successor should keep the existing handler structure and fix only the
#     jwt.rs validation logic."
#   - Human note: "Use jose crate v4 API, not v3"
# Result: agent builds on existing work, doesn't repeat routing/parsing.
```

**Outcome**: The second agent completes in ~5 minutes instead of ~15 because it inherits the working foundation and knows exactly where to focus.

### Scenario 2: Incremental Refinement after Evaluation Rejection

**Situation**: An agent writes a research document. The FLIP evaluation scores it 0.41 — below threshold. The evaluation notes say: "Missing comparison with existing solutions; conclusions are not supported by evidence from the analysis."

**Without iterate**:
```bash
wg retry research-caching-strategy
# New agent writes the entire document from scratch.
# May produce a structurally different document that has other gaps.
# The specific evaluation feedback is lost — the new agent doesn't know
# what the evaluator found lacking.
```

**With iterate**:
```bash
wg iterate research-caching-strategy --summarize
# Archive: research-caching-strategy~iter-0
# Handoff includes:
#   - Evaluation score: 0.41
#   - Evaluation notes: "Missing comparison with existing solutions..."
#   - LLM summary: "The document covers Redis, Memcached, and local LRU caches
#     but lacks a comparison matrix. Section 4 (Conclusions) recommends Redis
#     without referencing the latency benchmarks from Section 2. The next attempt
#     should add a comparison table and ensure conclusions reference specific
#     findings from the analysis."
# New agent receives this + the original document as an artifact reference.
# Result: agent revises the existing document rather than rewriting it.
```

**Outcome**: The refined document scores 0.78 — the agent focused on the specific gaps identified by the evaluation rather than starting over.

### Scenario 3: Human-Guided Approach Pivot

**Situation**: A task to optimize a database query completes successfully (Done), but the human reviewer sees that the agent chose to add an index when the real fix should be a query rewrite.

```bash
wg iterate optimize-user-query \
  --note "Don't add indexes. The query itself needs restructuring — use a CTE
          to avoid the N+1 pattern. The index you added may hurt write
          performance. Revert the migration and rewrite the query instead."
# Archive: optimize-user-query~iter-0 (status: Done)
# New agent receives the human's note as primary guidance.
# It can inspect the archive to see what the index approach looked like
# and understand why it's being redirected.
```

---

## Interaction with Existing Systems

### Retry (`wg retry`)

Unchanged. Retry remains the amnesia re-execution for failed tasks. The two commands have different entry points and different semantics. A user who wants amnesia retry continues to use `wg retry`.

### Requeue (`wg requeue`)

Unchanged. Requeue is for coordinator-level triage of in-progress tasks (e.g., when a dependency fails). It doesn't create archives or carry context.

### Cycles

Independent. Iterate archives use `~iter-{N}` which doesn't conflict with spiral archives (`~{N}`). Iterating a cycle member doesn't affect cycle state.

### FLIP / Evaluation

Evaluation of iterate archives works like spiral archives — the evaluation targets the archive task ID (`implement-auth~iter-1`). No schema changes needed.

### Agent registry

No changes. The registry records agents by `agent_id` with `task_id` pointing to the live task. Archive task IDs are not registered (they're historical snapshots, not live work items).

### Agent archive (`wg log agent`)

No changes. The existing agent archive system (`src/commands/log.rs:128-168`) already stores per-attempt conversation logs in `.workgraph/log/agents/{task-id}/{timestamp}/`. The iterate system complements this by surfacing that history in a structured format.

### Provenance

A new `"iterate"` event type is recorded:
```json
{
  "op": "iterate",
  "task": "implement-auth",
  "detail": {
    "iterate_count": 2,
    "archive_id": "implement-auth~iter-1",
    "summarized": true,
    "note": "Use jose crate v4 API"
  }
}
```

---

## Implementation Phases

### Phase 1: Core Iterate (MVP)

1. Add `iterate_count` and `iterate_context` fields to Task struct + TaskHelper
2. Implement `wg iterate` command (archive creation, task reset, structured handoff Tier 1+2)
3. Inject `iterate_context` in `build_task_context()` (3-line change)
4. Task ID validation: reject `~iter-` in user-supplied IDs
5. Filter iterate archives from cycle analysis
6. Add `--note` flag for human guidance
7. Tests: unit tests for archive creation, context assembly, status transitions

### Phase 2: LLM Summary

1. Implement `--summarize` flag with LLM call
2. Summary prompt engineering and testing
3. Store accumulated summaries in `iterate_context`
4. `--model` flag for summary generation model selection

### Phase 3: Query & Display

1. Extend `wg show` to display iterate history
2. Extend `wg iterations` to include iterate archives
3. `wg list --tag iterate-archive` (works out of the box)
4. `wg viz` filtering for iterate archives

### Phase 4: Coordinator Integration

1. Auto-iterate on evaluation rejection (configurable threshold)
2. Coordinator can iterate tasks instead of retry when `iterate_count < max_iterations`
3. Cost tracking across iterate chains

---

## Open Questions (Deferred)

1. **Auto-iterate on evaluation rejection**: Should the coordinator automatically iterate (instead of retry) when FLIP rejects a task? This would require a configuration option like `auto_iterate_on_rejection: true` in the coordinator config. Deferred — manual iteration first, automation second.

2. **Cross-attempt artifact diffing**: Should the handoff include a diff between attempt N and attempt N-1's artifacts? This would help the agent see exactly what changed. Deferred — the agent can `wg show {id}~iter-{N}` and inspect manually.

3. **Iterate budget**: Should there be a global `max_iterate_attempts` guardrail (like `max_triage_attempts`)? Probably yes, to prevent infinite iteration loops. The `--max-iterations` flag on the command handles per-invocation limits, but a global guardrail in config would catch runaway automation.

4. **Cost accumulation**: Should `wg iterate` track the total cost across all attempts? The iterate archives preserve per-attempt `token_usage`, so total cost can be computed by summing across archives. A convenience display in `wg show` could surface this.

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Context window bloat from long iterate_context | Medium | Medium | Cap iterate_context at ~2000 tokens; rotate old attempt summaries |
| LLM summary hallucination | Low | Medium | Summary is supplemental; structured metadata provides ground truth |
| Confusion between iterate and retry | Medium | Low | Clear CLI help, distinct commands, different status constraints |
| Archive accumulation | Low | Medium | `wg gc` can prune old iterate archives like spiral archives |
| ID collision with spiral archives | Very Low | High | Distinct suffix schemes: `~{N}` vs `~iter-{N}` |
| Performance impact of LLM summary call | Low | Low | Opt-in via `--summarize`; uses cheap model (haiku) |
