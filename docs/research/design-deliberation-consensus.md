# Workgraph Design Deliberation — Consensus Document

**Date:** 2026-03-10
**Process:** 10 researchers × 3 rounds of structured deliberation + synthesis + facilitation
**Result:** Unanimous convergence (10/10 participants declared convergence)

---

## The Agreed Framework: Layered Coordination Model

The deliberation produced a three-layer architectural framework for workgraph's coordination model. All 10 participants confirmed this framework as settled.

### Layer 1: The Stigmergic Medium

The task graph is the primary coordination mechanism. Agents coordinate indirectly through traces left in the shared graph — task states, artifacts, logs, dependency structures. This is workgraph's most distinctive architectural property and its core competitive advantage over workflow-as-code (Temporal) and DAG-based (Airflow) systems.

**Design principle:** Every coordination event must leave a trace in the graph. The graph is the system's memory, identity, and single source of truth.

### Layer 2: The Typed Node Model

Graph nodes are explicitly typed with a minimal enum:

```rust
pub enum NodeKind {
    Task,       // Fire-and-forget work unit — linear lifecycle, mailbox communication
    Session,    // Persistent interactive node — unbounded lifecycle, bidirectional chat
}

pub struct GraphNode {
    pub kind: NodeKind,
    pub internal: bool,              // True for .assign-*, .flip-*, .evaluate-*, etc.
    pub parent_id: Option<String>,   // Links lifecycle tasks to their parent
    // ... existing fields
}
```

**Design decisions:**
- **Two kinds, not three.** Avoids taxonomy proliferation (Airflow's operator taxonomy cautionary tale). Lifecycle/internal tasks are `Task` nodes with `internal: true`.
- **`internal` flag replaces prefix-matching heuristics.** `is_internal_task()` and `system_task_parent_id()` become field lookups, not string parsing.
- **`parent_id` makes relationships explicit data.** No more inferring parent from `.assign-{parent_id}` string patterns.
- **Closed for now, open by design.** New kinds can be added through normal development (code change → review → merge), not through autonomous self-modification.
- **Each kind carries enforced capability invariants:**
  - `Task`: single-turn execution, mailbox-based messaging, finite lifecycle
  - `Session`: persistent process, bidirectional chat channel, crash-recoverable, dispatch authority

### Layer 3: The Interaction Channels

Three communication channels, all with mandatory graph tracing:

| Channel | Medium | Parties | Sync | Graph Trace |
|---------|--------|---------|------|-------------|
| **Stigmergic** | Artifacts, logs, task state | Any → Graph → Any | Async | IS the graph |
| **Message-queue** | `wg msg` JSONL | Task ↔ Task | Semi-async | Already in graph |
| **Chat** | Coordinator inbox/outbox | User ↔ Session | Sync | Auto-traced at decision level |

**Key decisions:**
- Chat and message-queue remain **conceptually and implementationally separate** (R10's argument: they are qualitatively different coordination mechanisms). Chat is direct/coupled; message-queue is stigmergic/decoupled.
- Chat is **auto-traced at decision level** — sessions produce structured decision-log entries, not message transcripts ("minutes, not transcripts" — R7). Every substantive chat interaction that leads to a system action generates a graph trace automatically.
- Each channel declares **metadata**: visibility scope, durability guarantee, addressing model, delivery guarantee, and trace policy (R1's proposal, expanded by R2).

---

## Settled Architectural Decisions

These 12 decisions have unanimous or near-unanimous agreement. They are binding design commitments.

### 1. Graph as Stigmergic Medium
The graph is the primary coordination mechanism. All design decisions must preserve graph inspectability, mutability, and self-description.

### 2. "Session" Replaces "Coordinator"
The persistent interactive agent is called a "session," not a "coordinator." Session conveys persistence, interactivity, and user-facing scope without hierarchical connotation.

### 3. Mandatory Graph Tracing
All communication must leave traces in the graph. No coordination channel may operate outside the graph. Private channels are convenience UIs; the graph is the system of record.

### 4. Decision-Level Chat Tracing
Sessions produce structured decision-log entries from chat interactions, not raw transcripts. Trace at the decision level: "User directed focus to error handling; tasks X, Y deprioritized."

### 5. Session Fast-Path (Inline Execution)
Sessions may execute small changes inline without spawning a separate agent, when ALL conditions hold:
- Estimated completion <30 seconds
- Single file modification
- Under ~50 lines of change
- No dependencies on in-flight tasks
- Unambiguous user intent

A task node is created **retrospectively** (immediately after execution), indistinguishable from a dispatched task. If the change exceeds guardrails mid-execution, the session escalates to standard dispatch. Guardrails are mechanically enforced, not prompt-enforced.

### 6. Fire-and-Forget as Default
Tasks default to fire-and-forget with mailbox communication. Sessions get bidirectional chat. Fire-and-forget tasks must provide observability: real-time log streaming, lifecycle state indicators, and completion notifications.

### 7. Message Checking as Hard Completion Gate
`wg done` rejects if unread messages exist. The existing warning is promoted to a hard error.

### 8. Channel Metadata Declarations
Each communication channel declares:
```rust
struct ChannelMetadata {
    visibility: Visibility,       // Public | TaskScoped | SessionPrivate
    durability: Durability,       // Ephemeral | SessionLifetime | Permanent
    addressing: Addressing,       // PointToPoint | TaskBound | Broadcast
    delivery: DeliveryGuarantee,  // BestEffort | GuaranteedEventual | Synchronous
    trace_policy: TracePolicy,    // None | DecisionLevel | Full
}
```

### 9. Chat and Message-Queue as Separate Systems
`wg msg` (stigmergic, task-attached, graph-visible) and `wg chat` (direct, session-scoped, auto-traced) remain separate systems. They share observability tooling but not semantics or implementation.

### 10. invariants.toml for Evolution Boundary
A machine-readable file specifying what the evolver can and cannot modify, enforced at the code level. The invariants file itself is immutable to the evolver. Violations are hard failures, not warnings.

Invariant organization (never self-modifiable): graph execution model, stigmergic coordination principle, human oversight requirement, evaluation-before-evolution constraint.
Variable structure (modifiable under constraints): agent roles, prompt amendments, dispatch heuristics, evolution parameters.

### 11. Event-Sourced Graph with Decision Rationale
Joint R3+R8 proposal:
- `operations.jsonl` as the canonical append-only event store
- Two event categories: **structural** (task_created, status_changed, edge_added) and **decision** (dispatch_decision, retry_decision, with rationale and alternatives considered)
- Graph state files as materialized views, derivable by replay
- Crash recovery via operation log replay
- Evolution events as first-class entries in the log

### 12. Fan-out + Synthesis as Workflow Pattern
Structured multi-party discussion (fan-out parallel tasks + synthesis integration + moderated cycle) is a named workflow pattern, not a fourth communication channel. Should be formalized as a reusable function (`wg func apply discussion`).

---

## Acknowledged Design Principles

These items have broad agreement on direction but implementation details are deferred to dedicated engineering tasks. They are stated commitments, not aspirational nice-to-haves.

### A. Resource-Proportional Meta-Regulation
The system should monitor and auto-regulate the ratio of meta-task compute (evaluation, evolution, assignment, FLIP) to productive-task compute. When meta-work exceeds a configurable threshold (e.g., 30%), reduce meta-task frequency. Standard practice in orchestration systems (Airflow scheduler overhead, Temporal rate-limiting, Netflix Maestro orchestration overhead).

### B. Confidence Signals on Task Completion
Agents should communicate calibrated uncertainty about their work quality:
```
wg done <task-id> --confidence high|medium|low
```
Drives adaptive review: high confidence → lightweight review; low confidence → flagged for detailed review. Agent confidence calibration feeds into the evaluation system over time.

### C. Attention Budgeting
The system should actively manage the user's attention budget, not just provide monitoring tools. This means: batched notifications when multiple tasks complete, priority ordering in review queues (failures > low-confidence > routine), and decomposition cost warnings.

### D. Formal Lifecycle State Machines
Each node kind has an explicit lifecycle state machine with defined states and transitions:
- Task: Open → InProgress → {Done, Failed, Waiting, PendingValidation, Abandoned}
- Session: Starting → Active → {Recovering, Shutdown}

These are first-class design artifacts, not emergent behavior scattered across implementation files. Compatible with self-organization (R5's argument: state machines define organizational constraints, not structural restrictions).

### E. Turn-Count vs Loop-Iteration Separation
Session chat turns are I/O events, not convergent iterations. They should use a dedicated `turn_count` metric, separate from `loop_iteration` (which is reserved for tasks that iterate toward convergence).

### F. CoordinatorScope for Multi-Session Composition
Multiple sessions should have bounded, non-overlapping dispatch scopes. The root session manages everything; additional sessions can be scoped to subtrees (analogous to Kubernetes namespaces or Erlang application trees).

### G. Generic Checkpoint Trait
Checkpoint/resume should work across executor types through a generic interface, not be coupled to the Claude executor's `--resume session_id`. This enables the persistence spectrum for all executor types.

---

## Resolved Tensions

| Tension | Resolution | Key Insight |
|---------|-----------|-------------|
| **Unified type vs separate kinds** | 2-kind enum (Task/Session) + internal flag | The question was never "are coordinators tasks?" but "what's the minimal set of role-specific semantics?" |
| **Conversational vs fire-and-forget** | Fire-and-forget default; sessions get chat | Not every task needs conversation. Observability (log streaming, progress indicators) provides engagement without architectural cost. |
| **Inner loop inclusion** | Session Fast-Path with retrospective graph nodes | The graph can record work retrospectively. Inline execution preserves graph completeness while eliminating context switches. |
| **Stigmergic purity vs pragmatic chat** | Chat as separate channel with mandatory auto-trace | Chat is a legitimate direct channel; the graph is the system of record. Auto-trace at decision level closes the stigmergic gap. |
| **Self-organization vs human control** | invariants.toml + deferred approval | Mechanical enforcement of evolution boundaries. The system is sympoietic (collectively produced by humans and machines), not fully autopoietic. |
| **2 vs 3 enum variants** | 2 variants + internal flag (isomorphic to 3 variants) | Both encodings carry identical information. 2+flag is simpler, has cleaner UX, avoids taxonomy trap. |

---

## Remaining Disagreements (Acknowledged, Not Blocking)

- **R10 notes stigmergic cost inversion** is addressed in principle but not in mechanism — `wg add` should be made easier to use to ensure stigmergic coordination is the path of least resistance.
- **R8 wants the `internal` flag to be mechanically set by the system**, not by agent convention — lifecycle tasks should be auto-marked as internal at creation time.
- **R5 requests evolution events in the operation log** — when the evolver mutates a role, it should be a first-class event for causal tracing.
- **R1's multi-coordinator composition** (CoordinatorScope) is acknowledged but under-specified — details deferred to implementation.

None of these are blocking. They are implementation watchpoints.

---

## Recommended Next Steps

1. **Add `NodeKind` enum, `internal` flag, and `parent_id` field** to the Task struct. Migrate existing code from prefix-matching to field-based checks. (Phase 1-5 migration path from R6.)
2. **Promote message-checking warning to hard error** on `wg done`.
3. **Implement chat auto-trace** — sessions automatically produce decision-log entries from substantive chat exchanges.
4. **Rename "coordinator" to "session"** throughout codebase and TUI.
5. **Create `invariants.toml`** with initial invariant set; add enforcement to evolution pipeline.
6. **Design the event-sourced operation log** (R3+R8 joint proposal) as a dedicated engineering task.
7. **Implement Session Fast-Path** with mechanical guardrails and retrospective node creation.
8. **Add `--confidence` flag to `wg done`** for adaptive review routing.
9. **Reduce TUI tabs** from 9 to ~4, implement command palette (R4's highest-leverage UX change).

---

## Appendix: Participant Convergence Declarations

| Researcher | Perspective | Convergence | Key Contribution |
|---|---|---|---|
| R1 | Coordinator Architecture | Round 3 ✓ | NodeRole enum, SessionLifecycle, channel declarations |
| R2 | Task Messaging | Round 3 ✓ | Hard completion gate, interrupt-and-resume, capability invariants |
| R3 | Agent Lifecycle | Round 3 ✓ | Lifecycle state machines, decision log, checkpoint trait |
| R4 | TUI/UX Design | Round 3 ✓ | Three-zone UI, tab reduction, command palette |
| R5 | Autopoiesis | Round 3 ✓ | invariants.toml, meta-regulation, FLIP-as-homeostasis |
| R6 | Abstraction Consistency | Round 3 ✓ | NodeKind enum, parent_id, migration path |
| R7 | Conversation Protocols | Round 3 ✓ | Message typing, latency-aware routing, fan-out formalization |
| R8 | Workflow Orchestration | Round 3 ✓ | Event-sourced graph, scale boundaries, orchestration comparisons |
| R9 | Human-Agent Interaction | Round 3 ✓ | Session Fast-Path, confidence signals, attention budgeting |
| R10 | Stigmergic Coordination | Round 3 ✓ | Graph-as-medium, auto-stigmergize chat, cost inversion |
