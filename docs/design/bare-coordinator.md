# Bare Coordinator Design

**Author:** Documenter-Structured
**Date:** 2026-04-09
**Status:** Design
**Depends on:** `docs/design/coordinator-as-regular-agent.md`, `docs/design/unified-lifecycle-state-machine.md`, `docs/design/safe-coordinator-cycle.md`, `docs/design/agent-lifecycle.md`

## Summary

The **bare coordinator** is a coordinator agent that runs as a regular looping task in the workgraph — not as a special entity outside the graph. Context compaction is a first-class graph task (`.compact-N`), not an implicit crash-recovery mechanism. The coordinator exits cleanly when context approaches its limit, a compaction task runs to produce a summary, and a new coordinator era begins with the compacted context injected.

This document specifies the complete bare coordinator design: its lifecycle state machine, journal-based compaction, signal/exit/resume protocol, deprecation plan, and the invariants that must hold.

---

## 1. Lifecycle State Machine

The bare coordinator operates as a **task** with the `coordinator-loop` tag. It is not a daemon-level special entity. Its lifecycle consists of **eras** — each era is a single invocation of the coordinator agent that runs until context exhaustion, explicit compaction trigger, or crash.

### 1.1 Era States

```
┌─────────────────────────────────────────────────────────────────────┐
│                         COORDINATOR ERA LIFECYCLE                    │
└─────────────────────────────────────────────────────────────────────┘

                    ┌──────────────────────────────────────────┐
                    │              Era N (InProgress)            │
                    │                                            │
                    │  The coordinator agent is running.         │
                    │  It reads inbox, manages graph,            │
                    │  dispatches tasks, responds to user.       │
                    │                                            │
                    │  Exit triggers:                             │
                    │    - Context at 80% capacity → [COMPACT]   │
                    │    - Explicit user/admin signal            │
                    │    - Crash (process dies)                  │
                    └─────────────────────┬──────────────────────┘
                                          │
                         ┌────────────────▼────────────────────────┐
                         │              Era N (Done)                 │
                         │  Coordinator agent exited cleanly.      │
                         │  Exit reason recorded.                  │
                         └────────────────┬───────────────────────┘
                                          │
                         ┌────────────────▼────────────────────────┐
                         │         Compact-N (InProgress)           │
                         │  Compaction task runs as a graph task.   │
                         │  Produces context.md (summary).          │
                         │  Coordinator is NOT running.             │
                         └────────────────┬───────────────────────┘
                                          │
                         ┌────────────────▼────────────────────────┐
                         │         Compact-N (Done)                 │
                         │  Compaction complete. context.md ready.  │
                         └────────────────┬───────────────────────┘
                                          │
                    ┌─────────────────────▼──────────────────────────┐
                    │            Era N+1 (InProgress)                 │
                    │  New coordinator spawns with context.md        │
                    │  as seed. Picks up where Era N left off.       │
                    │  Processes pending inbox messages.             │
                    └──────────────────────────────────────────────┘
```

### 1.2 Coordinator Task State Machine

The coordinator task itself (e.g., `.coordinator-0`) follows the unified lifecycle state machine from `unified-lifecycle-state-machine.md`. The bare coordinator adds **era tracking** as a field on the task:

```rust
pub struct Task {
    pub id: String,
    pub status: Status,           // Open → InProgress → Done → Open (loop)
    pub era: u64,                // Incremented on each coordinator spawn
    pub exit_reason: Option<ExitReason>,
    pub tags: Vec<String>,       // Includes "coordinator-loop"
    // ... other fields
}

pub enum ExitReason {
    /// Context approaching limit — clean exit for compaction
    ContextLimit,
    /// Explicit compaction trigger from coordinator agent
    ExplicitCompact,
    /// Process crashed or was killed
    Crashed,
    /// Daemon shutdown (service stop)
    DaemonShutdown,
    /// Manual coordinator stop
    ManualStop,
}
```

### 1.3 Era Transitions

| From State | To State | Trigger |
|------------|----------|---------|
| Era N (InProgress) | Era N (Done) | Coordinator agent exits cleanly |
| Era N (Done) | Compact-N (InProgress) | Daemon detects era done |
| Compact-N (InProgress) | Compact-N (Done) | Compaction task completes |
| Compact-N (Done) | Era N+1 (InProgress) | Daemon spawns next era |

### 1.4 Blocked vs. Waiting for the Coordinator

The coordinator task itself can be in `Blocked` or `Waiting` states:

- **Blocked:** The coordinator task is waiting for its `after` dependencies to complete (e.g., waiting for `.compact-N` to finish before starting Era N+1). This is structural — the coordinator is explicitly `after .compact-N`.
- **Waiting:** A coordinator era is parked waiting for user input or an external event. This uses the `Waiting` status and `WaitCondition` from the unified lifecycle.

The cycle dependency graph for the bare coordinator:

```
.coordinator-0 → .compact-0 → .coordinator-0 (cycle: natural via after edge)
```

**This is NOT a circular deadlock** because:
1. `.coordinator-0` starts as `Open`, becomes `InProgress` when spawned
2. When `.coordinator-0` exits (Done), `.compact-0` becomes ready (all after deps met)
3. `.compact-0` runs, produces context.md, marks itself Done
4. `.coordinator-0` is still `Done` (terminal in old era), but the **loop edge** re-activates it for the next era
5. On re-activation, `.coordinator-0` becomes `Open` again, gets re-spawned as Era 1

---

## 2. Journal-Based Compaction

Compaction is **journal-based** in the bare coordinator model. Rather than an ad-hoc crash-recovery mechanism, compaction is:

1. A **first-class task** in the graph
2. Triggered by **journal events** (context exhaustion signals)
3. Produces a **structured artifact** (`.workgraph/compactor/context.md`)
4. Evaluated by **automated checks**

### 2.1 Compaction Journal

The compactor maintains a **journal** of its operations in `.workgraph/compactor/state.json`:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct CompactorState {
    /// RFC3339 timestamp of last compaction
    pub last_compaction: Option<String>,
    /// Provenance ops count at last compaction (for growth tracking)
    pub last_ops_count: usize,
    /// Daemon tick number at last compaction
    pub last_tick: u64,
    /// Total compaction count (era number - 1)
    pub compaction_count: u64,
    /// Duration of last compaction LLM call in milliseconds
    pub last_compaction_duration_ms: Option<u64>,
    /// Token usage from last compaction
    pub last_compaction_tokens: Option<TokenUsage>,
    /// Byte size of context.md written
    pub last_compaction_context_bytes: Option<u64>,
    /// Consecutive compaction errors
    pub error_count: u64,
}
```

### 2.2 Compaction Trigger Conditions

Compaction fires when **any** of these conditions are met:

| Condition | Threshold | Mechanism |
|-----------|-----------|-----------|
| Token budget | 80% of context window | Coordinator agent tracks accumulated tokens; exits with `[COMPACT]` |
| Interaction count | 50 turns | Turn counter in coordinator agent |
| Time-based | 4 hours | Daemon tracks era start time |
| Explicit signal | User or admin trigger | `wg compact` command or daemon IPC |
| Ops growth | 100 new provenance ops since last compaction | Journal-based count |

The coordinator agent's prompt includes:

> "When you estimate your context is approaching 80% capacity, or after 50 interactions, end your response with `[COMPACT]` to signal that you need context compaction. The daemon will handle the rest."

### 2.3 Compaction Task Input and Output

**Input to compaction task:**
- Era N conversation history (bounded: messages since last compaction)
- Era N output log (artifacts produced)
- Current graph state (full refresh)
- Previous era's summary (if available)
- Compactor state (journal)

**Compaction task output:**
- `.workgraph/compactor/context.md` — 3-layer summary:
  1. **Rolling Narrative** (~2000 tokens): What happened this era
  2. **Persistent Facts** (~500 tokens): Key decisions, user preferences, project state
  3. **Evaluation Digest** (~500 tokens): Evaluation scores and verdicts

**Compaction task structure:**

```yaml
id: .compact-0
title: Compact coordinator context (Era 0 → Era 1)
after: [.coordinator-0]
status: open
tags: [compact-loop, auto-spawn]
description: |
  Distill the coordinator's Era 0 context into a concise summary.
  
  ## Input
  - Era 0 conversation history
  - Current graph state
  - Previous context (if any)
  
  ## Output
  - .workgraph/compactor/context.md (Rolling Narrative + Persistent Facts + Evaluation Digest)
```

### 2.4 Compaction as Graph Task (Not Inline)

In the bare coordinator model, compaction runs as a **graph task**, not inline in the daemon. This means:

1. The daemon does NOT call `run_compaction()` directly
2. Instead, when `.coordinator-0` exits (Done), the daemon marks `.compact-0` as `Open`
3. The normal task dispatch mechanism picks up `.compact-0` and spawns an agent for it
4. The compaction agent (which could be a lightweight LLM call or aClaude CLI agent) produces `context.md`
5. On `.compact-0` Done, the daemon spawns the next coordinator era

**Exception:** The `wg compact` CLI command still runs compaction inline (single-shot, not part of a cycle).

### 2.5 Journal Compaction and the Unified Lifecycle

Journal-based compaction interacts with the unified lifecycle as follows:

- The coordinator era's `InProgress → Done` transition is triggered by the agent exiting (via `[COMPACT]` or normal completion)
- The `Done → Resuming` transition (for the next era) happens via the loop edge
- The compaction task runs between `Done` and `Resuming` as a blocking dependency

```
.coordinator-0 (Era N) ──exit──→ .coordinator-0 (Done)
                                          │
                                   (loop edge fires)
                                          │
                         .compact-N ──done──→ .coordinator-0 (Era N+1)
```

---

## 3. Signal/Exit/Resume Protocol

### 3.1 Exit Protocol

When a coordinator era ends, it follows this protocol:

```
Coordinator Era N
│
├─[COMPACT]──→ Agent calls wg done, exits with exit_reason = ContextLimit
│                    │
│                    ▼
│              Coordinator Task: InProgress → Done
│                    │
│                    ▼ (daemon detects Done)
│              Daemon checks exit_reason
│                    │
│                    ▼
│              .compact-N becomes ready (after deps met)
│                    │
│                    ▼
│              Compaction task runs
│                    │
│                    ▼
│              context.md written
│                    │
│                    ▼
│              Era N+1 spawns with context.md injected
│
├─crash───→ Agent process dies unexpectedly
│                 │
│                 ▼
│           Coordinator Task: InProgress → (stuck detection)
│                 │
│                 ▼
│           Triage verdict → restart or fail
│
└─SIGTERM──→ Daemon sends SIGTERM to coordinator agent
                  │
                  ▼
            Graceful shutdown: agent finishes current turn, calls wg done, exits
```

### 3.2 Signal Handling

**SIGINT (Ctrl+C):** Sent by daemon to interrupt coordinator. The Claude CLI handles this by stopping generation and emitting TurnComplete, preserving conversation context. Coordinator can then decide to compact and exit, or continue.

**SIGTERM:** Sent by daemon for graceful shutdown. Coordinator agent finishes current work, writes checkpoint, calls `wg done`, and exits cleanly.

**SIGKILL:** Last resort. Coordinator agent does not handle — process is killed. On restart, crash recovery context is injected (last N messages + graph state).

### 3.3 Resume Protocol

When a new coordinator era spawns, it follows the resume protocol:

```
Era N+1 Spawn
│
├─ Daemon loads context.md from .workgraph/compactor/
│
├─ Daemon checks for pending inbox messages since Era N ended
│
├─ Daemon injects into coordinator agent prompt:
│   "You are continuing as the coordinator for this project.
│    Context summary from previous era:
│    [context.md contents]
│    
│    Pending messages:
│    [messages since Era N ended]
│    
│    Current graph state:
│    [graph summary]
│    
│    Continue your work."
│
├─ Coordinator agent starts with context as seed
│
└─ Coordinator processes pending messages, continues work
```

### 3.4 Resume Delta

Between eras, the coordinator needs to know what changed while it was away. The **resume delta** is built from:

1. **Graph state delta:** What tasks changed status? What new tasks were added?
2. **Pending messages:** Inbox messages that arrived during the gap
3. **Compaction output:** The `context.md` artifact itself

The daemon injects a condensed version of these changes as context for the new era:

```
## Resume Context
Your previous era ended with context compaction.

### What Changed While You Were Away
- task-X: completed (produced artifact: feature.md)
- task-Y: failed (reason: test suite broken)
- 3 new tasks added

### Pending Messages
- [2026-04-09T15:30:00Z] user: Can you review the PR?
- [2026-04-09T15:45:00Z] agent-abc: Task task-X complete, here's the artifact

### Your Compacted Context
[context.md summary]

Continue your work on this project.
```

### 3.5 Context Injection Path

For the bare coordinator, context injection follows this path:

```
.compact-N (Done) → .workgraph/compactor/context.md
                                    │
                                    ▼ (daemon reads on era N+1 spawn)
                            Coordinator Era N+1 prompt
                                    │
                                    ▼
                            Coordinator agent (InProgress)
```

**Invariant:** `context.md` MUST exist before the next coordinator era spawns. If compaction fails, the daemon falls back to the crash-recovery approach (bounded history + graph state).

---

## 4. Deprecation Plan

The bare coordinator deprecates the **special-entity coordinator** approach — where the coordinator runs as a daemon-level `CoordinatorAgent` struct with its own process management, crash recovery, and context injection machinery.

### 4.1 Current Special-Entity Problems

| Problem | Impact |
|---------|--------|
| Dedicated ~973 lines of coordinator_agent.rs | High maintenance burden |
| Custom crash recovery in daemon | Not visible in graph, not evaluatable |
| Context injection via stdin injection | Fragile, Claude CLI-specific |
| No compaction as graph task | Compaction is implicit, not auditable |
| Coordinator uses different spawn path | Executor changes don't apply to coordinator |

### 4.2 Deprecation Phases

#### Phase A: Extract Coordinator Prompt into Role/Bundle (Low Risk)

Move the hardcoded system prompt from `build_system_prompt()` into the agency system:

```yaml
# .workgraph/agency/roles/coordinator.yaml
name: coordinator
description: "Persistent coordinator agent that manages the task graph"
system_prompt: |
  (current content of build_system_prompt())
default_exec_mode: full
default_bundle: coordinator
```

```toml
# .workgraph/bundles/coordinator.toml
[bundle]
name = "coordinator"
description = "Coordinator agent — inspect + create, no implement"
tools = ["wg:*"]
context_scope = "graph"
```

**No behavioral change.** The coordinator still runs as a special entity, but its configuration comes from the agency system.

#### Phase B: Add Era Tracking (Medium Risk)

1. Add `era: u64` field to the coordinator task
2. Track `exit_reason` on coordinator task
3. When coordinator crashes/exits, record era + reason
4. On restart, increment era and inject crash recovery context

**Behavioral change:** Era tracking begins. The coordinator's lifecycle is now visible in the graph.

#### Phase C: Compaction as Graph Task (Medium Risk)

1. Create `.compact-N` task automatically when coordinator starts Era N
2. When coordinator exits with `ContextLimit`, mark `.compact-N` as ready
3. Remove inline `run_compaction()` from daemon tick path
4. Compaction now runs as a graph task, produces `context.md`

**Behavioral change:** Compaction is now a first-class graph task. The daemon no longer runs compaction inline.

#### Phase D: Switch to Regular Agent Spawn Path (Higher Risk)

1. Remove `CoordinatorAgent` struct and `agent_thread_main()` from `coordinator_agent.rs`
2. The coordinator task is spawned via the normal executor path
3. The coordinator agent reads inbox directly instead of stdin injection
4. The daemon detects coordinator exit and triggers compaction → restart

**Behavioral change:** The coordinator is no longer a persistent subprocess. It's a series of agent invocations with compacted context between them.

### 4.3 Feature Flag

Throughout the deprecation, a feature flag controls the mode:

```toml
[coordinator]
# Modes: "special" (current), "bare" (new), "single-turn" (future)
mode = "special"
```

- `"special"`: Current special-entity coordinator
- `"bare"`: Bare coordinator (compaction as graph task, regular spawn path)
- `"single-turn"`: Future single-turn coordinator (no persistent session)

### 4.4 Rollback

At any phase, setting `mode = "special"` reverts to the current approach. No data migration required.

---

## 5. Bare Coordinator Invariants

These invariants MUST hold for the bare coordinator design to be correct:

### 5.1 Context Isolation Invariant

**"Each era operates on a consistent snapshot of context."**

- At era start, the coordinator receives `context.md` from the previous compaction
- Any messages or graph changes during compaction are queued and processed in the next era
- No context is lost between eras — compaction output preserves all essential information

**Violation symptom:** Coordinator in Era N+1 doesn't know about a task completed in Era N.

### 5.2 Compaction Completeness Invariant

**"Compaction always produces a valid context.md before the next era starts."**

- `context.md` MUST exist in `.workgraph/compactor/` before Era N+1 spawns
- If compaction fails, the daemon MUST fall back to crash-recovery context (not proceed without any context)
- `context.md` MUST be non-empty and parseable

**Violation symptom:** Era N+1 spawns with no context, losing all prior context.

### 5.3 No Circular Coordinator↔Archive Dependency Invariant

**"The coordinator cycle MUST NOT include archive as a dependency."**

```
.coordinator-N → .compact-N → .coordinator-N (OK)
.archive-N → (independent, NOT in coordinator cycle)
```

- Archive runs on its own schedule/threshold, NOT gated by coordinator
- Coordinator never waits for archive
- Archive never waits for coordinator

**Violation symptom:** Deadlock — coordinator waits for archive, archive waits for coordinator.

### 5.4 Era Continuity Invariant

**"The coordinator can always resume from where the previous era left off."**

- Each era's `exit_reason` is recorded
- Each era's era number is tracked and incremented
- Resume always injects: context.md + pending messages + graph state delta

**Violation symptom:** Coordinator in new era is confused about prior state, re-does work already done.

### 5.5 Slot Accounting Invariant

**"The coordinator era does NOT consume an agent slot while in Done/Waiting state."**

- While `.compact-N` is running, `.coordinator-N` is `Done` (no agent slot consumed)
- While the coordinator is between eras (Done → compact → Resuming), no slot is consumed
- Only when Era N is `InProgress` does it count against `max_agents`

This is already guaranteed by the unified lifecycle state machine.

### 5.6 Signal Safety Invariant

**"SIGTERM always results in a clean exit, never a crash."**

- When daemon sends SIGTERM to coordinator agent, the agent:
  1. Finishes current turn (if InProgress)
  2. Writes any pending checkpoint
  3. Calls `wg done` with `exit_reason = DaemonShutdown`
  4. Exits cleanly
- SIGKILL is only sent as last resort and treated as crash (triggers crash recovery)

**Violation symptom:** Coordinator exit not recorded as `Done`, next era doesn't know previous era ended properly.

### 5.7 Transition Atomicity Invariant

**"Era transitions are atomic — either a full era completes or it doesn't."**

- When coordinator exits with `[COMPACT]`, the transition `InProgress → Done` is atomic
- Compaction task does not start until the transition is complete
- If compaction fails, the coordinator is NOT re-spawned until compaction succeeds or fallback is used

**Violation symptom:** Two coordinator eras running simultaneously.

---

## 6. Comparison with Special-Entity Coordinator

| Dimension | Special Entity (Current) | Bare Coordinator |
|-----------|-------------------------|-----------------|
| **Process management** | Dedicated CoordinatorAgent struct (~973 lines) | Regular task spawn path |
| **Crash recovery** | Inline summary in daemon | Compaction task (graph-visible) |
| **Context injection** | stdin injection (Claude CLI-specific) | Prompt assembly (executor-agnostic) |
| **Compaction** | Implicit, ad-hoc | Explicit, graph task |
| **Evaluability** | None (opaque) | Per-era (compaction evaluatable) |
| **Executor portability** | Claude CLI only | Any executor |
| **Visibility** | Hidden (daemon detail) | Graph-visible (task + artifacts) |
| **Slot accounting** | Separate from agent slots | Unified with agent slots |
| **Context continuity** | Via session persistence | Via compaction artifact |
| **Exit signals** | Custom handling in agent thread | Unified lifecycle transitions |

---

## 7. Open Questions

1. **Should compaction be mandatory?** For small projects with few interactions, compaction overhead may not be worth it. Could allow `coordinator_compact = false` to disable it.

2. **How many eras to retain?** Keeping all era summaries is cheap (they're small), but the coordinator only needs the most recent one. Retention policy: keep last 5 eras, garbage-collect older ones.

3. **What if compaction fails?** The compaction agent could fail (API error, bad summary). Fallback: use the crash recovery approach (inline summary from history) and retry compaction later.

4. **Should compaction block coordinator restart?** Currently proposed as sequential (compact → restart). Alternative: start the new coordinator immediately with a simpler summary, run compaction in parallel, and inject the full summary on next interaction.

5. **How does this interact with the native executor?** The native executor runs agents in-process via `tokio::spawn`. For the coordinator, this means no subprocess at all — the coordinator is a Rust task within the daemon process. Compaction and era management work identically regardless of executor type.

---

## 8. References

- Coordinator as regular agent: `docs/design/coordinator-as-regular-agent.md`
- Unified lifecycle state machine: `docs/design/unified-lifecycle-state-machine.md`
- Safe coordinator cycle: `docs/design/safe-coordinator-cycle.md`
- Agent lifecycle: `docs/design/agent-lifecycle.md`
- Compactor implementation: `src/service/compactor.rs`
- Coordinator tick logic: `src/commands/service/coordinator.rs`
- Coordinator agent (special entity): `src/commands/service/coordinator_agent.rs`
- Cycle-aware graph: `docs/design/cycle-aware-graph.md`
