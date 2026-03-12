# Workgraph Coordination Model — Design Document

**Date:** 2026-03-10
**Status:** Approved Design (from 10-agent deliberation consensus)
**Supersedes:** Ad hoc coordinator-as-task conventions
**Task:** final-integration-design

---

## 1. Executive Summary

Workgraph's coordination model will evolve from its current implicit conventions — where coordinators are special-cased tasks distinguished by string prefix heuristics and communication is split across ad hoc channels — into a **Layered Coordination Model** with three explicit architectural layers: (1) the task graph as a stigmergic medium (the primary coordination mechanism), (2) a typed node model with a minimal two-variant enum (`Task` | `Session`) replacing heuristic prefix matching, and (3) three formally declared interaction channels (stigmergic, message-queue, chat) with mandatory graph tracing. This design preserves workgraph's distinctive property — indirect coordination through traces in a shared graph — while resolving the fundamental tensions between persistent and ephemeral agents, fire-and-forget and conversational work, and human control versus system autonomy. The migration is incremental: each phase delivers value independently, the system remains backward-compatible at every step, and the most impactful changes (NodeKind enum, hard message gate, session renaming) ship first.

---

## 2. Design Principles

These principles emerged from unanimous consensus across all 10 research perspectives and are binding design commitments.

1. **The graph is the system of record.** Every coordination event must leave a trace in the graph. The graph is workgraph's memory, identity, and single source of truth. No coordination channel may operate outside the graph.

2. **Stigmergy first, direct communication second.** Agents coordinate primarily through traces left in the shared graph — task states, artifacts, logs, dependency structures. Direct communication (chat, messages) is a supplement, not a replacement. This is workgraph's core competitive advantage over workflow-as-code systems (Temporal) and DAG-based systems (Airflow).

3. **Minimal type taxonomy.** Two node kinds (`Task`, `Session`), not three or more. History shows that node-type taxonomies proliferate without bound (Airflow's operator taxonomy is the cautionary tale). The taxonomy is closed for now and opened through deliberate code changes, never through autonomous self-modification.

4. **Explicit over heuristic.** Relationships, node kinds, and internal status are expressed as structured data fields, not inferred from string patterns. `is_internal_task()` becomes a field lookup, not `id.starts_with('.')`.

5. **Fire-and-forget as default, conversation as upgrade.** Most work units don't need bidirectional communication. Tasks default to fire-and-forget with mailbox messaging. Sessions get bidirectional chat. The system optimizes for the common case.

6. **Observability is non-negotiable.** Fire-and-forget does not mean fire-and-forget-about. Real-time log streaming, lifecycle state indicators, and completion notifications provide engagement without architectural overhead.

7. **Mechanical enforcement, not prompt enforcement.** Invariants (evolution boundaries, message checking gates, fast-path guardrails) are enforced in code, not in agent prompts. Prompts are suggestions; code is law.

8. **Incremental migration, continuous value.** Every change ships independently. No "big bang" rewrite. Each phase is backward-compatible with existing graph data through serde defaults and migration scripts.

9. **Sympoietic, not autopoietic.** The system is collectively produced by humans and machines. It can self-organize within defined boundaries, but those boundaries are human-controlled. The `invariants.toml` file is the mechanical expression of this principle.

10. **The graph is alive.** Agents are nodes in a living system. They don't just complete tasks — they grow the graph where it needs growing, decompose work that's too large, and create follow-up work when they discover it.

---

## 3. Architecture

### 3.1 What Is the System Today?

Before describing the target, we must be precise about the current state. Today's workgraph has:

**A single `Task` struct** (`src/graph.rs:201-346`) with 40+ fields that serves all purposes — regular work items, coordinator processes, evaluation scaffolds, assignment decisions, and FLIP checks. There is no type discrimination in the data model; all nodes are `Task`.

**String-prefix heuristics** for distinguishing node roles. `is_system_task()` (`src/graph.rs:350`) checks `task_id.starts_with('.')` to identify internal tasks. `system_task_parent_id()` parses the parent task ID from strings like `.assign-{parent_id}` and `.flip-{parent_id}`. The coordinator is identified by the pattern `.coordinator-N`. These heuristics are referenced across at least 8 source files (graph.rs, coordinator.rs, viz/mod.rs, spawn/execution.rs, resume.rs, eval_scaffold.rs, func_extract.rs, viz/ascii.rs).

**Two communication regimes** that coexist without formal relationship:
- **Chat** (`src/chat.rs`): A real-time bidirectional channel between the user and the coordinator, implemented as stdin/stdout streaming over a persistent Claude CLI process.
- **Message queue** (`src/messages.rs`): A JSONL-based mailbox system (`wg msg send/read`) where messages are stored per-task. No executor supports real-time delivery (`supports_realtime()` returns `false` for all three adapters — Claude, Amplifier, Shell). Delivery is best-effort via notification files.

**A persistent coordinator** (`src/commands/service/coordinator_agent.rs`) that is architecturally unique: it runs as a long-lived LLM session inside the service daemon, has crash recovery with restart rate limiting, conversation history rotation, and context refresh. This is fundamentally different from task agents, which are spawned as single-turn processes (`claude --print`) that execute and exit.

**An implicit lifecycle model** where task states (Open, InProgress, Done, Failed, Waiting, PendingValidation, Abandoned) are defined as an enum (`Status`) but the valid transitions are scattered across implementation files rather than being centralized as a state machine.

### 3.2 Layer 1: The Stigmergic Medium

The task graph (`.workgraph/graph.jsonl`) is the primary coordination mechanism. This is already true today and is workgraph's most important design property. The deliberation confirmed this unanimously and elevated it from an implicit convention to a stated architectural principle.

**What this means concretely:**

- Every agent action that affects coordination must produce a graph trace. Creating a task, completing a task, sending a message, producing an artifact — all of these already trace to the graph. The principle demands that this remains true as the system evolves.

- Chat interactions, which currently operate partly outside the graph, must be auto-traced at the decision level. When a user tells the coordinator to reprioritize work, the reprioritization decision is traced to the graph as a structured decision-log entry. The raw transcript is not stored in the graph (it lives in chat history files); the decision is.

- The graph supports structural operations that make stigmergic coordination powerful: dependency edges (indicating ordering), cycle edges (indicating iteration), artifacts (indicating outputs), logs (indicating progress), and status transitions (indicating lifecycle). These are all forms of sematectonic stigmergy — the work product IS the coordination signal.

**Event-sourced graph model (new):**

The graph will move toward an event-sourced architecture, with `operations.jsonl` as the canonical append-only event store. This was a joint proposal from R3 (agent lifecycle) and R8 (workflow orchestration) that achieved consensus.

Two event categories:
- **Structural events**: `task_created`, `status_changed`, `edge_added`, `artifact_registered`, `message_sent`
- **Decision events**: `dispatch_decision`, `retry_decision`, `evolution_decision` — each carrying rationale and alternatives considered

The current `graph.jsonl` becomes a materialized view, derivable by replaying the operation log. Crash recovery becomes log replay. Evolution events become first-class entries, enabling causal tracing of how and why the system changed itself.

This is not a rewrite of graph persistence — it is a new append-only log that runs alongside the existing system, initially as a secondary record, eventually as the primary source of truth.

### 3.3 Layer 2: The Typed Node Model

This is the most structurally significant change. The current system treats all graph nodes as untyped `Task` structs. The target model introduces explicit typing.

#### The `NodeKind` Enum

```rust
/// The fundamental kind of a graph node.
/// Closed enum — new variants require deliberate code changes.
pub enum NodeKind {
    /// Fire-and-forget work unit.
    /// Linear lifecycle (Open → InProgress → Done/Failed).
    /// Mailbox communication via `wg msg`.
    /// Single-turn agent execution.
    Task,

    /// Persistent interactive node.
    /// Unbounded lifecycle (Starting → Active → Recovering/Shutdown).
    /// Bidirectional chat channel.
    /// Crash-recoverable, dispatch authority.
    Session,
}
```

**Why two kinds, not three:** The deliberation considered a three-variant model (Task, Session, Lifecycle) where internal tasks like `.assign-*` and `.flip-*` would be a separate kind. This was rejected because the third variant carries no unique semantics — lifecycle tasks have identical execution semantics to regular tasks. The distinction is better captured by a boolean flag.

**Why not one kind:** A single kind forces every feature to be gated on runtime checks rather than type-level invariants. Sessions have fundamentally different lifecycle semantics (persistent, crash-recoverable, dispatch-authorized), communication semantics (bidirectional chat), and execution semantics (long-running process) from tasks. Merging them means every code path that touches node-specific behavior must dynamically check "am I a session?" — which is exactly what happens today with the string-prefix heuristic, except less reliable.

#### New Fields on `Task` (to be renamed `GraphNode`)

```rust
pub struct GraphNode {
    // ... existing fields ...

    /// The fundamental kind of this node. Default: Task.
    #[serde(default = "default_task_kind")]
    pub kind: NodeKind,

    /// Whether this is an internal/system-generated node.
    /// Replaces the `.` prefix heuristic.
    #[serde(default, skip_serializing_if = "is_bool_false")]
    pub internal: bool,

    /// Explicit parent relationship.
    /// Links lifecycle tasks (.assign-*, .flip-*, .evaluate-*) to their parent.
    /// Replaces string-parsing of parent ID from task ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}
```

**`internal` flag replaces prefix matching.** Today, `is_system_task()` checks if the ID starts with `.`. This is fragile (what if someone creates a task starting with `.` by accident?) and makes the system's own organizational structure invisible — you can't filter or query by "show me all internal tasks" without parsing strings. The `internal` flag makes this a first-class data field. The `.` prefix convention continues as a naming convention but is no longer the source of truth.

**`parent_id` makes relationships explicit.** Today, to find which task `.assign-my-feature` belongs to, you parse the string to extract `my-feature`. This breaks on task IDs containing hyphens in unexpected positions and requires every consumer to implement the same parsing logic. With `parent_id`, the relationship is explicit data.

#### Capability Invariants Per Kind

Each `NodeKind` carries enforced capability invariants:

| Capability | Task | Session |
|---|---|---|
| Execution model | Single-turn (spawn → work → exit) | Persistent process (daemon lifetime) |
| Communication | Mailbox (`wg msg`) | Bidirectional chat + mailbox |
| Lifecycle | Open → InProgress → {Done, Failed, Waiting, PendingValidation, Abandoned} | Starting → Active → {Recovering, Shutdown} |
| Dispatch authority | None (executes assigned work) | Can dispatch work to other nodes |
| Crash recovery | Retry with optional session resume | Automatic restart with rate limiting |
| Chat access | No | Yes |
| Graph mutation | Can create tasks, add edges | Full graph manipulation |

These invariants are currently scattered across implementation files. They should be documented as first-class design artifacts and enforced in the type system where possible.

#### Session Lifecycle State Machine

```
                    ┌──────────────┐
                    │   Starting   │
                    └──────┬───────┘
                           │ process spawned
                           ▼
                    ┌──────────────┐
            ┌──────│    Active     │◄─────┐
            │      └──────┬───────┘      │
            │             │               │ restart (≤3/10min)
crash/exit  │             │ shutdown      │
            │             ▼               │
            │      ┌──────────────┐      │
            │      │   Shutdown   │      │
            │      └──────────────┘      │
            │                             │
            ▼                             │
     ┌──────────────┐                    │
     │  Recovering  │────────────────────┘
     └──────────────┘
```

#### Task Lifecycle State Machine

```
     ┌──────┐
     │ Open │
     └──┬───┘
        │ agent spawned
        ▼
  ┌────────────┐     verify fails    ┌─────────────────────┐
  │ InProgress │────────────────────►│ PendingValidation   │
  └──┬────┬────┘                     └──────────┬──────────┘
     │    │                                     │ verify passes
     │    │ wg wait                             ▼
     │    │          ┌─────────┐         ┌──────────┐
     │    └─────────►│ Waiting │────────►│   Done   │
     │               └─────────┘ resume  └──────────┘
     │
     │ wg fail / max retries
     ▼
  ┌────────┐
  │ Failed │
  └────────┘
```

### 3.4 Layer 3: The Interaction Channels

Three communication channels, each with distinct semantics and mandatory graph tracing.

#### Channel 1: Stigmergic (Artifacts, Logs, State)

This is not a "channel" in the traditional sense — it IS the graph. Agents coordinate by reading and writing graph state. An agent completes a task and produces artifacts; a downstream agent reads those artifacts as input context. No direct communication occurs. This is textbook sematectonic stigmergy.

**Properties:**
- Medium: Task artifacts, log entries, status transitions, dependency edges
- Parties: Any agent → Graph → Any agent
- Synchronization: Fully asynchronous
- Graph trace: IS the graph (tautologically complete)
- Addressing: Broadcast (any agent with graph access can read)

This channel is workgraph's default and most important coordination mechanism. The design must ensure it remains the path of least resistance — creating a task and writing artifacts should always be easier than sending a direct message.

#### Channel 2: Message Queue (`wg msg`)

Task-attached, JSONL-based mailbox messaging for cases where stigmergic coordination is insufficient — typically when an agent needs to communicate updated requirements, corrections, or context to a running task.

**Properties:**
- Medium: JSONL files in `.workgraph/messages/{task-id}.jsonl`
- Parties: Task ↔ Task, User → Task
- Synchronization: Semi-asynchronous (delivery via notification file, polling by agent)
- Graph trace: Already in the graph (messages stored in `.workgraph/`)
- Addressing: Point-to-point (task-bound)
- Delivery guarantee: Best-effort (agent may not check)

**Key change: Hard completion gate.** Currently, `wg done` warns if unread messages exist but allows completion. The consensus promotes this to a hard error: `wg done` rejects if unread messages exist. This is the single most impactful change for message reliability — it guarantees that agents process all messages before declaring completion, without requiring real-time delivery infrastructure.

#### Channel 3: Chat (`wg chat`)

Real-time bidirectional communication between the user and a Session node. This is qualitatively different from the message queue — it is direct, synchronous, coupled, and interactive.

**Properties:**
- Medium: Coordinator inbox/outbox JSONL, stdin/stdout streaming
- Parties: User ↔ Session
- Synchronization: Synchronous (user waits for response)
- Graph trace: Auto-traced at decision level (new)
- Addressing: Direct (session-scoped)
- Delivery guarantee: Synchronous (guaranteed)

**Key change: Decision-level auto-tracing.** Sessions will automatically produce structured decision-log entries from substantive chat interactions. The trace captures what was decided and why, not the raw transcript. Example: "User directed focus to error handling; tasks X, Y deprioritized" rather than the full message exchange. This closes the stigmergic gap — the chat channel becomes transparent to graph inspection.

**Chat and message-queue remain separate systems.** R10's argument was decisive: they are qualitatively different coordination mechanisms. Chat is direct/coupled; message-queue is stigmergic/decoupled. They share observability tooling (both are visible in the TUI) but not semantics or implementation.

#### Channel Metadata

Each channel declares formal metadata:

```rust
struct ChannelMetadata {
    visibility: Visibility,       // Public | TaskScoped | SessionPrivate
    durability: Durability,       // Ephemeral | SessionLifetime | Permanent
    addressing: Addressing,       // PointToPoint | TaskBound | Broadcast
    delivery: DeliveryGuarantee,  // BestEffort | GuaranteedEventual | Synchronous
    trace_policy: TracePolicy,    // None | DecisionLevel | Full
}
```

This metadata is descriptive (documenting channel properties) rather than configurational (allowing arbitrary channel creation). The three channels are fixed; the metadata makes their properties explicit and inspectable.

### 3.5 The Session Fast-Path

Sessions may execute small changes inline without spawning a separate agent. This addresses R9's finding that the inner development loop (small edits, quick fixes) has excessive overhead when every change requires task creation → agent spawn → execution → completion.

**Conditions (ALL must hold):**
- Estimated completion < 30 seconds
- Single file modification
- Under ~50 lines of change
- No dependencies on in-flight tasks
- Unambiguous user intent

**Mechanism:**
1. Session evaluates the request against guardrails (mechanically enforced, not prompt-enforced)
2. If guardrails pass, session executes inline
3. A task node is created **retrospectively** — immediately after execution, indistinguishable from a dispatched task
4. If the change exceeds guardrails mid-execution, the session escalates to standard dispatch

The retrospective node ensures graph completeness. An external observer looking at the graph after the fact cannot tell whether a task was executed via fast-path or standard dispatch. This preserves the stigmergic principle: the graph is the system of record, and it records all work.

### 3.6 Evolution Boundaries: `invariants.toml`

The system can self-organize within defined boundaries. `invariants.toml` is the mechanical expression of those boundaries.

```toml
# invariants.toml — Machine-readable evolution boundary
# This file is immutable to the evolver. Violations are hard failures.

[organization]
# Never self-modifiable
graph_execution_model = "locked"        # How the graph runs
stigmergic_coordination = "locked"      # Graph-as-medium principle
human_oversight = "locked"              # Human remains in the loop
evaluation_before_evolution = "locked"  # Eval gates evolution

[variable]
# Modifiable under constraints
agent_roles = "mutable"                 # Evolver can mutate roles
prompt_amendments = "mutable"           # Evolver can amend prompts
dispatch_heuristics = "mutable"         # Assignment logic can evolve
evolution_parameters = "mutable"        # Evolution tuning
```

The evolver reads this file before any mutation. Attempts to modify locked invariants are hard failures, not warnings. The invariants file itself is never writable by the evolver — it can only be changed by human developers through the normal code review process.

---

## 4. Migration Path

The migration follows six phases, each delivering independent value. Phases can overlap where they touch different parts of the codebase.

### Phase 1: Data Model Extension (NodeKind, internal, parent_id)

**What changes:**
- Add `kind: NodeKind` field to `Task` struct with `#[serde(default = "default_task_kind")]` (defaults to `Task` for backward compatibility)
- Add `internal: bool` field with `#[serde(default)]`
- Add `parent_id: Option<String>` field with `#[serde(default)]`
- Write a one-time migration that reads `graph.jsonl` and sets:
  - `kind = Session` for tasks matching `.coordinator-*`
  - `internal = true` for tasks matching `.` prefix
  - `parent_id` by parsing existing ID patterns (`.assign-{parent}`, `.flip-{parent}`, `.evaluate-{parent}`)

**What stays the same:**
- `is_system_task()` continues to work (prefix check still valid)
- All existing task IDs preserved
- No behavior changes — this is purely additive data

**Backward compatibility:** Existing `graph.jsonl` files deserialize correctly because all new fields have serde defaults. The migration script enriches existing data but the system functions without it.

**Validation:** `cargo test` passes; graph loads correctly with and without new fields; migration script is idempotent.

### Phase 2: Hard Message Completion Gate

**What changes:**
- `wg done` checks for unread messages and returns an error (not a warning) if any exist
- Error message includes the count of unread messages and instructions to read them

**What stays the same:**
- `wg msg send/read` semantics unchanged
- Agents already instructed to check messages before `wg done`

**Impact:** This is the highest-leverage change for message reliability. It costs almost nothing to implement (change a warning to an error in the `done` command handler) and guarantees message processing without requiring real-time delivery infrastructure.

**Validation:** Write a test that `wg done` fails with unread messages; verify existing agents' workflows still pass (they already check messages).

### Phase 3: Rename "Coordinator" to "Session"

**What changes:**
- CLI: `wg chat` stays (it's already user-facing and well-known), but internal references change
- Code: `CoordinatorAgent` → `SessionAgent`, `coordinator_agent.rs` → `session_agent.rs`, etc.
- TUI: "Coordinator" label → "Session"
- Task IDs: New sessions created as `.session-N` (existing `.coordinator-N` tasks recognized as aliases)
- Documentation: All references updated

**What stays the same:**
- Behavior is identical
- Existing `.coordinator-N` task IDs continue to work (alias resolution)

**Why this matters:** "Coordinator" carries hierarchical connotation — it implies the coordinator is in charge. "Session" conveys persistence, interactivity, and user-facing scope without hierarchy. This aligns with the sympoietic principle: the session is a participant in the graph, not its controller.

### Phase 4: Code Migration to Field-Based Checks

**What changes:**
- `is_system_task(id)` → `node.internal` (field lookup instead of string parsing)
- `system_task_parent_id(id)` → `node.parent_id` (field lookup instead of string parsing)
- All 8 files with `is_internal_task`/`is_system_task` references updated
- `is_system_task()` kept as a backward-compatibility helper that checks both `internal` flag and `.` prefix, with a deprecation note

**What stays the same:**
- External behavior identical
- `.` prefix convention preserved as a naming convention (it's useful for human readability)

**Impact:** Eliminates the fragile string-parsing heuristics that were a recurring source of bugs. Makes node relationships queryable through structured data.

### Phase 5: Decision-Level Chat Tracing

**What changes:**
- Session agent produces structured decision-log entries when chat interactions lead to system actions
- Format: `{ "type": "decision", "summary": "...", "actions": [...], "timestamp": "..." }`
- Decision logs stored as task log entries on the session's task node
- TUI surfaces decision trail for graph inspection

**What stays the same:**
- Chat mechanics unchanged
- Raw chat history still stored in `.workgraph/chat/` files (not removed)
- Message queue semantics unchanged

**Why this is Phase 5 (not earlier):** Implementing decision-level tracing requires understanding what constitutes a "decision" in chat context. This needs the Session abstraction (Phase 3) to be in place. The tracing logic lives in the session agent, not in the chat transport.

### Phase 6: Event-Sourced Operation Log

**What changes:**
- New file: `.workgraph/operations.jsonl` as an append-only event store
- All graph mutations emit events to this log
- Events include structural (task_created, status_changed) and decision (dispatch_decision, retry_decision with rationale)
- Graph state files remain as materialized views

**What stays the same:**
- `graph.jsonl` continues to exist as a materialized view
- All existing tooling that reads `graph.jsonl` continues to work
- The operation log is additive — it doesn't replace anything initially

**Why last:** This is the most architecturally significant change and benefits from having the typed node model (Phases 1, 3, 4) in place. Events about sessions have different semantics than events about tasks, and that distinction should be in the type system before it's in the event log.

### What Changes Immediately (Phases 1-2)

The data model extension and hard message gate can ship in the first iteration. They are small, well-scoped, backward-compatible, and deliver immediate value. The data model extension is ~50 lines of struct changes plus a migration script. The message gate is a one-line change from warning to error.

### What Changes Incrementally (Phases 3-5)

The rename, code migration, and chat tracing are interlinked and should proceed together over 2-3 iterations. The rename (Phase 3) is mostly mechanical — find-and-replace across the codebase. The code migration (Phase 4) touches more files but each change is local and testable. Chat tracing (Phase 5) requires design iteration to get the "decision level" abstraction right.

### What Stays the Same

- **Graph-as-stigmergic-medium**: This is preserved and strengthened, not changed.
- **`wg msg` semantics**: Message queue behavior is unchanged; only the completion gate is tightened.
- **Agent execution model**: Task agents still spawn, work, and exit. Sessions still run persistently. The execution infrastructure doesn't change.
- **Agency system**: Roles, evolution, evaluation are orthogonal to the coordination model. They continue to operate on top of whatever node types exist.
- **Cycle/loop semantics**: Cycle edges, `--max-iterations`, `--converged` are unchanged. They operate on tasks regardless of NodeKind.

---

## 5. UX Implications

### 5.1 TUI Evolution

The TUI should evolve to reflect the typed node model. Key changes based on R4's research:

**Tab reduction.** The current TUI has too many tabs (~9). The target is ~4 primary views: (1) Graph overview with task/session distinction, (2) Active agent monitoring, (3) Chat interface, (4) Detail view for selected node. Additional functionality moves to a command palette accessible via a keyboard shortcut.

**Node kind visualization.** Tasks and Sessions should be visually distinct in the graph view. Sessions should have a different icon/color than tasks. Internal nodes should be de-emphasized (dimmed, smaller) by default, with a toggle to show/hide them.

**Decision trail.** When a session is selected, the TUI should display the decision trail — the sequence of decision-log entries produced by chat auto-tracing. This makes the session's impact on the graph transparent.

**Three-zone layout.** R4 proposed a three-zone TUI: (1) Graph overview (left), (2) Detail/chat (center), (3) Agent status (right). This maps naturally to the three architectural layers: the graph (Layer 1), the selected node's details and communication (Layers 2-3), and the live agent state.

### 5.2 CLI Evolution

**New commands:**
- `wg session` — list/manage sessions (replaces `wg chat` for session management; `wg chat` continues as the user-facing chat command)
- `wg trace` — view the decision trail for a session
- `wg invariants` — inspect the current invariants

**Modified commands:**
- `wg add --kind session` — create a Session node (default remains Task)
- `wg done` — hard error on unread messages (Phase 2)
- `wg show` — displays NodeKind, internal flag, parent_id in output

**Unchanged commands:**
- `wg msg` — same semantics
- `wg chat` — same behavior
- `wg add` (without `--kind`) — creates a Task (unchanged default)

### 5.3 Attention Budgeting

The system should actively manage the user's attention:
- **Batched notifications**: When multiple tasks complete in rapid succession, batch them into a single notification rather than N separate ones
- **Priority ordering**: Failures > low-confidence completions > routine completions
- **Decomposition cost warnings**: When an agent decomposes a task into many subtasks, surface the expected cost/time before dispatching

---

## 6. Open Questions

These are acknowledged unknowns that emerged from the deliberation. They are not blocking — they are watchpoints for future design iterations.

### 6.1 Architecture

1. **Multi-session composition (CoordinatorScope).** How do multiple sessions divide dispatch scope? The root session manages everything; additional sessions can be scoped to subtrees. But the scoping mechanism (explicit scope declaration? automatic by subtree? Kubernetes-namespace-style?) is unspecified. Deferred to implementation discovery.

2. **Generic checkpoint trait.** Checkpoint/resume currently works only with Claude sessions (`--resume session_id`). A generic interface would enable persistence across executor types (Amplifier, Shell, future executors). The trait surface is clear; the serialization format needs design.

3. **Scale boundaries.** At what graph size do current assumptions break? R8's research on orchestration systems suggests that graph persistence, event replay, and visualization all have scaling limits. The event-sourced model (Phase 6) is designed with replay efficiency in mind, but concrete benchmarks are needed.

### 6.2 Communication

4. **Stigmergic cost inversion.** R10 identified that `wg add` (stigmergic coordination) has higher friction than direct communication in some cases. The goal is to make stigmergic coordination the path of least resistance — but the specific UX improvements (templates? auto-completion? smart defaults?) need design.

5. **Message typing.** R7 proposed typed messages (request, inform, correction, priority-change) with routing based on type. This would enable smarter message handling (e.g., corrections interrupt; informs don't). But the type taxonomy and routing rules need careful design to avoid over-engineering.

6. **Real-time message delivery.** Currently no executor supports real-time delivery. Is this acceptable long-term? For fire-and-forget tasks, yes — the hard completion gate is sufficient. For long-running tasks with complex requirements, maybe not. The session resume mechanism (wait → message → resume) is a partial solution.

### 6.3 Self-Organization

7. **Evolution event tracing.** R5 requested that evolver mutations be first-class events in the operation log. When a role is mutated, the causal chain (evaluation → evolution decision → mutation) should be traceable. This is architecturally straightforward (add event types to Phase 6) but needs concrete event schema design.

8. **Meta-regulation thresholds.** The system should auto-regulate meta-task compute (evaluation, evolution, FLIP, assignment) vs. productive-task compute. But what's the right threshold? 30% was proposed as a starting point. This needs empirical calibration.

9. **`internal` flag automation.** R8 argued that the `internal` flag should be set mechanically by the system at task creation time, not by agent convention. Lifecycle tasks (assign, flip, evaluate) should be auto-marked as internal. The heuristic is clear (tasks created by the coordinator for system purposes); the mechanism needs implementation.

### 6.4 Cross-Cutting

10. **Struct rename timing.** Should `Task` be renamed to `GraphNode` in Phase 1 (alongside the field additions) or deferred? Renaming is a larger change (touches every file that references `Task`) and could be disruptive. The consensus leans toward deferral — add fields now, rename later.

11. **Confidence signal calibration.** The `--confidence high|medium|low` flag on `wg done` is agreed in principle but needs calibration: what does "high confidence" mean? How does the system learn to trust or distrust an agent's confidence over time? This connects to the evaluation/evolution system.

12. **Turn-count vs loop-iteration semantics.** Session chat turns should use a dedicated `turn_count` metric, separate from `loop_iteration` (which tracks convergence iterations). This is a data model clarification, not a behavior change.

---

## 7. Implementation Roadmap

The following tasks implement the design. They are ordered by dependency and priority.

### Immediate (Phase 1-2) — Ship First

| Task | Description | Dependencies | Validation |
|---|---|---|---|
| Add NodeKind enum | Add `kind: NodeKind`, `internal: bool`, `parent_id: Option<String>` to Task struct | None | `cargo test`; graph loads with/without new fields |
| Write migration script | One-time script to populate kind/internal/parent_id from existing data | NodeKind enum | Idempotent; produces correct values for all existing tasks |
| Hard message completion gate | Change `wg done` warning to error when unread messages exist | None | Test: `wg done` fails with unread messages |

### Near-term (Phase 3-4) — Follow-up Iteration

| Task | Description | Dependencies | Validation |
|---|---|---|---|
| Rename coordinator to session | Rename types, files, docs, TUI labels | NodeKind enum | `cargo test`; TUI shows "Session" |
| Add `.session-N` ID pattern | New sessions use `.session-N`; `.coordinator-N` accepted as alias | Rename | Existing coordinators still work |
| Migrate to field-based checks | Replace all `is_system_task()` string checks with `node.internal` | NodeKind enum, migration | All 8 files updated; `cargo test` |
| Deprecate `is_system_task()` | Mark as deprecated; add note to check `internal` field | Migrate to field-based | Deprecation warning in docs |

### Medium-term (Phase 5) — Design Iteration Needed

| Task | Description | Dependencies | Validation |
|---|---|---|---|
| Design decision-log format | Define the schema for session decision-log entries | Rename to session | Schema documented; example entries |
| Implement chat auto-trace | Sessions produce decision-log entries on substantive actions | Decision-log format | Decision trail visible in logs |
| Add `wg trace` command | CLI command to view decision trail | Chat auto-trace | `wg trace .session-1` shows decisions |
| Create `invariants.toml` | Initial invariant set with enforcement in evolution pipeline | None (can start anytime) | Evolver respects locked invariants |

### Longer-term (Phase 6) — Architectural Evolution

| Task | Description | Dependencies | Validation |
|---|---|---|---|
| Design operation log schema | Event types, serialization format, replay semantics | NodeKind enum | Schema documented |
| Implement operation log emission | Graph mutations emit events to `operations.jsonl` | Schema design | Events emitted on all mutations |
| Implement log replay | Graph state derivable from operation log replay | Log emission | Replayed state matches direct state |
| Add evolution events | Evolver mutations as first-class operation log entries | Operation log | Causal chain traceable |

### Ongoing — No Phase Dependency

| Task | Description | Dependencies | Validation |
|---|---|---|---|
| TUI tab reduction | Reduce from ~9 to ~4 tabs + command palette | None | UX testing |
| Attention budgeting | Batched notifications, priority ordering | None | User-observable improvement |
| `--confidence` flag on `wg done` | Adaptive review routing based on agent confidence | None | Flag accepted; stored on task |
| Session Fast-Path | Inline execution with retrospective graph nodes | Session abstraction | Fast-path produces valid graph nodes |
| Resource-proportional meta-regulation | Auto-regulate meta-task vs productive-task compute ratio | None | Meta-task ratio stays within threshold |

---

## Appendix A: Resolved Tensions Summary

| Tension | Resolution | Key Insight |
|---|---|---|
| Unified type vs separate kinds | 2-kind enum (Task/Session) + internal flag | The question was never "are coordinators tasks?" but "what's the minimal set of role-specific semantics?" |
| Conversational vs fire-and-forget | Fire-and-forget default; sessions get chat | Not every task needs conversation. Observability provides engagement without architectural cost. |
| Inner loop inclusion | Session Fast-Path with retrospective graph nodes | The graph can record work retrospectively. Inline execution preserves graph completeness while eliminating context switches. |
| Stigmergic purity vs pragmatic chat | Chat as separate channel with mandatory auto-trace | Chat is a legitimate direct channel; the graph is the system of record. Auto-trace at decision level closes the stigmergic gap. |
| Self-organization vs human control | invariants.toml + deferred approval | Mechanical enforcement of evolution boundaries. The system is sympoietic, not fully autopoietic. |
| 2 vs 3 enum variants | 2 variants + internal flag (isomorphic to 3 variants) | Both encodings carry identical information. 2+flag is simpler, cleaner UX, avoids taxonomy trap. |

## Appendix B: Participant Contributions

| Researcher | Perspective | Key Contribution to Design |
|---|---|---|
| R1 | Coordinator Architecture | NodeKind enum, SessionLifecycle state machine, channel metadata declarations |
| R2 | Task Messaging | Hard completion gate, capability invariants per kind |
| R3 | Agent Lifecycle | Lifecycle state machines, checkpoint trait, event-sourced graph |
| R4 | TUI/UX Design | Three-zone UI, tab reduction, command palette |
| R5 | Autopoiesis | invariants.toml, meta-regulation, sympoietic framing |
| R6 | Abstraction Consistency | NodeKind enum design, parent_id field, phased migration path |
| R7 | Conversation Protocols | Message typing proposal, decision-level tracing ("minutes, not transcripts") |
| R8 | Workflow Orchestration | Event-sourced graph, scale boundary analysis, orchestration system comparisons |
| R9 | Human-Agent Interaction | Session Fast-Path, confidence signals, attention budgeting |
| R10 | Stigmergic Coordination | Graph-as-medium principle, stigmergic cost inversion, auto-trace for chat |

## Appendix C: Terminology

| Term | Definition |
|---|---|
| **Graph node** | Any entity in the task graph (Task or Session) |
| **Task** | Fire-and-forget work unit with linear lifecycle and mailbox communication |
| **Session** | Persistent interactive node with bidirectional chat (formerly "coordinator") |
| **Internal node** | System-generated node (assignment, evaluation, FLIP check) |
| **Stigmergic coordination** | Indirect coordination through traces left in the shared graph |
| **Decision-level tracing** | Logging what was decided and why, not the raw message transcript |
| **Fast-path** | Session inline execution of small changes with retrospective graph nodes |
| **Operation log** | Append-only event store of all graph mutations |
| **Invariants** | Machine-readable evolution boundaries that the evolver cannot modify |

## Appendix D: Current Codebase References

Files that will be modified during migration (non-exhaustive):

| File | Current Pattern | Target Pattern |
|---|---|---|
| `src/graph.rs:201-346` | `pub struct Task` | Add `kind`, `internal`, `parent_id` fields |
| `src/graph.rs:350` | `is_system_task()` prefix check | Deprecate; migrate callers to `node.internal` |
| `src/commands/service/coordinator.rs` | 6× `is_internal_task` references | Replace with `node.internal` field checks |
| `src/commands/spawn/execution.rs` | 2× prefix checks | Replace with field checks |
| `src/commands/viz/mod.rs` | 15× internal task filtering by prefix | Replace with `node.internal` |
| `src/commands/viz/ascii.rs` | 1× prefix check | Replace with field check |
| `src/commands/resume.rs` | 1× prefix check | Replace with field check |
| `src/commands/eval_scaffold.rs` | 1× prefix check | Replace with field check |
| `src/commands/func_extract.rs` | 1× prefix check | Replace with field check |
| `src/messages.rs` | `wg done` warning on unread messages | Promote to hard error |
| `src/commands/service/coordinator_agent.rs` | `CoordinatorAgent` struct | Rename to `SessionAgent` (Phase 3) |
