# Unified Synthesis: Design Tensions in Workgraph's Coordination Model

## A Cross-Cutting Analysis of 10 Research Perspectives

---

## 1. Introduction

This document synthesizes the findings of 10 independent research statements, each examining workgraph's coordination model from a distinct disciplinary angle. The researchers studied: coordinator architecture (R1), task messaging asymmetry (R2), agent lifecycle management (R3), TUI/UX design patterns (R4), autopoietic systems theory (R5), abstraction consistency (R6), multi-party conversation protocols (R7), graph-based workflow orchestration (R8), human-agent interaction models (R9), and stigmergic coordination (R10).

The synthesis that follows maps core tensions, clusters themes, articulates trade-offs, surfaces open questions, and proposes a preliminary conceptual framework for resolving the identified tensions.

---

## 2. Mapping the Core Tensions

### 2.1 Tension A: The Uniformity Question — Is a Coordinator a Task?

This is the single most debated question across the 10 statements. Every researcher touches it; none agree fully on the answer.

**The "retain-and-formalize" camp (R1, R3, R5, R10):** These researchers argue that the coordinator-as-task model is fundamentally sound. R1 draws on Erlang/OTP supervisors (same process, different behaviour), Kubernetes controllers (same Pod, different role), and Hewitt's actor model ("everything is an actor") to argue that embedding the controller within the system it controls is a well-established pattern. R3 agrees, mapping the pattern to Kubernetes Deployments vs. Jobs. R5 frames the coordinator's dual nature as the productive tension between autopoietic (centralized, self-maintaining) and sympoietic (distributed, collectively-produced) dynamics. R10 sees the coordinator as a "necessary escape hatch" from pure stigmergy — architecturally exceptional but functionally essential.

**The "separate-entirely" camp (R6):** R6 stands alone in arguing that coordinators should NOT be tasks. After cataloging eight axes of divergence (identity conventions, lifecycle management, cycle configuration, tags, chat system, TUI integration, agent model, and configuration namespace), R6 concludes that the coordinator-as-task abstraction is a leaky abstraction providing "negative value" — it creates a false sense of uniformity while requiring special-case handling in all 49 source files that reference coordinators. R6 recommends a Kubernetes-style Kind system where coordinators and tasks share metadata conventions but are explicitly different node types.

**The nuanced middle (R1, R8):** R1 proposes the most precise formulation: "The coordinator IS a task AND it is special. Both are true." The system's job is to make the specialness visible and well-defined, not to eliminate it. R8, while not directly addressing this question, notes that workgraph's position is "genuinely novel" in the workflow orchestration landscape — it need not conform to patterns from systems with fundamentally different architectures.

**Where the researchers agree:** All 10 agree that coordinators are functionally different from regular tasks. The disagreement is about whether this difference should be modeled as a specialization within a unified type (the Erlang/OTP approach) or as a distinct type with shared metadata (the Kubernetes approach). No researcher argues that coordinators are just tasks with no special treatment needed.

### 2.2 Tension B: The Communication Spectrum — How Should Agents Talk?

**The asymmetry is by design (R2, R7, R10):** R2 provides the most detailed analysis of the communication gap. Coordinators have bidirectional, synchronous, stateful chat channels; task agents have asynchronous, fire-and-forget message queues where delivery is not guaranteed during execution. R2 maps this to Erlang's `call` (synchronous) vs. `cast` (asynchronous) patterns and argues the asymmetry is justified: the coordinator's conversational nature serves its always-available orchestrator role, while task agents are optimized for focused, bounded execution.

R7 goes further: "The graph IS the conversation." Every task completion is an utterance; every dependency edge is a conversational constraint; every artifact is a shared reference. The fan-out + synthesis pattern (used by this very deliberation process) is already a robust conversation protocol — it sacrifices interactivity for parallelism and resilience. R7 cites research suggesting that multi-agent debate does not consistently outperform single-agent approaches, questioning the value of adding complex real-time conversation infrastructure.

R10 reframes communication through stigmergy: the graph itself (task logs, artifacts, status changes) IS the communication medium. The chat channel is a non-stigmergic escape hatch that should "leave footprints" — every chat interaction should deposit a trace in the graph so future coordination can rely on the stigmergic medium.

**The gap needs narrowing (R2, R9):** R2 recommends three specific improvements: (1) enforce message checking as a hard gate on task completion, (2) add interrupt-and-resume for urgent messages, and (3) keep the coordinator uniquely conversable. R9 identifies the "inner loop gap" — users accustomed to Cursor or Copilot's immediacy will find the always-outer-loop model frustrating for small tasks.

### 2.3 Tension C: The Lifecycle Problem — Persistent vs. Ephemeral

**The binary split is correct (R3):** R3 provides the deepest analysis of agent lifecycle, mapping workgraph's model to Kubernetes (Deployments vs. Jobs), Erlang/OTP (permanent vs. temporary restart strategies), Temporal (durable workflows vs. ephemeral activities), and Dapr (virtual actors). The conclusion: "The persistent coordinator / ephemeral task agent split is fundamentally correct and should be preserved." It maps to the orchestration/worker distinction found in every successful distributed system.

**But the boundaries are blurred (R1, R6):** R1 identifies the "turn-as-iteration conflation" — recording chat turns as cycle iterations is a category error. A chat turn is I/O; a cycle iteration is convergent work. R6 calls this "semantic abuse" of the cycle machinery to model persistence. Both argue that coordinator lifecycle should have its own semantics rather than being shoehorned into the task lifecycle model.

**Persistence is a spectrum (R3):** R3 proposes the most nuanced view: persistence is not binary but a spectrum. Coordinators need high persistence (conversational continuity, event awareness). Task agents need low persistence (clean start, contained failure) with optional persistence hooks (checkpoint, resume) for long-running or interruptible tasks. The `wg wait` + session resume mechanism already provides "partial persistence" — analogous to Dapr's virtual actor model where actors are activated on demand, deactivated when idle, and maintain state across activations.

### 2.4 Tension D: The Interaction Gap — How Should Humans Participate?

**Three distinct perspectives emerge:**

R9 (Human-Agent) frames the user as a "variable-geometry participant" who fluidly shifts between supervisor, architect, and peer. The inner loop (interactive chat) vs. outer loop (autonomous task execution) tension is the fundamental UX challenge. R9 identifies the "seam problem": when a user says "fix this bug" to the coordinator, is this an inner-loop request (fix it now, interactively) or an outer-loop request (create a task, dispatch an agent)? The current rule says always outer-loop, which creates friction for small tasks.

R4 (TUI/UX) provides concrete design recommendations: reduce the 9-tab interface to 4 primary tabs, add a command palette for discovery, implement three-tier navigation (zone focus → within-zone navigation → command palette), add unread indicators, and enlarge touch targets for mobile terminals. R4 draws on tmux, k9s, htop, Slack, and VS Code to establish universal principles: progressive disclosure, persistent orientation cues, Escape as universal "go back," and maximum ~5 primary visible actions.

R8 (Workflow Orchestration) implicitly argues for the outer loop: workgraph's graph-as-data model is its distinctive strength. The graph is inspectable, analyzable, and visualizable in ways that code-based workflow systems (Temporal, Prefect) cannot match. Sacrificing this for inner-loop immediacy would trade away workgraph's competitive advantage.

### 2.5 Tension E: The Self-Organization Boundary — How Much Autonomy?

**Agreement on the risk of excess (R5, R9, R10):** R5 identifies workgraph as "proto-autopoietic" — a sympoietic system with autopoietic mechanisms. The evolution loop (evaluate → evolve → execute) is a genuine self-producing cycle. But R5 warns that the risk is "excessive autopoiesis, not insufficient autopoiesis." Specifically: runaway self-production (recursive task decomposition), loss of coherence (locally-optimizing agents producing globally incoherent graphs), self-modification paradoxes (the evolver modifying the coordinator that dispatches the evolver), and homeostatic traps (premature convergence at local optima).

R9 frames this as a trust calibration problem: over-trusting leads to undetected errors; under-trusting leads to micromanagement. The user's attention is the fundamental scarce resource. Push-based monitoring (exception-driven alerts) is essential because pull-based monitoring (`wg tui`, `wg status`) doesn't scale.

R10 sees the balance as: maximize stigmergic coordination for the routine case, preserve non-stigmergic channels for exceptions. "The optimal design is to maximize stigmergic coordination for the routine case while preserving non-stigmergic channels for the exceptional case."

---

## 3. Theme Clusters

### Theme 1: The Abstraction Boundary (R1, R3, R5, R6, R8, R10)

Six of the ten researchers address the fundamental question of where to draw the abstraction boundary between coordinators and tasks. The positions form a spectrum:

| Position | Researchers | Approach |
|----------|------------|----------|
| Same type, formalized asymmetry | R1, R5 | TaskKind enum, Erlang-style behaviour distinction |
| Same type, enriched persistence spectrum | R3 | Persistence as spectrum with checkpoint/resume hooks |
| Same graph, different Kind | R6 | Kubernetes-style NodeKind with distinct specs |
| Pragmatic hybrid, focus on strengths | R8, R10 | Don't over-engineer the abstraction; focus on what works |

Cross-references: R1§5.3 (synthesis: sound with acknowledged asymmetry), R3§7 (persistence is a spectrum), R5§4 (autopoietic-sympoietic tension), R6§5.2 (Kubernetes-style Kind), R8§8 (novel position in orchestration landscape), R10§4 (necessary deviation from pure stigmergy).

### Theme 2: The Communication Architecture (R2, R7, R10)

Three researchers focus specifically on how information flows through the system:

- **R2** analyzes the structural causes of messaging unreliability: single-turn execution model, no interrupt mechanism, probabilistic prompt compliance. Proposes: hard gate on message checking, interrupt-and-resume for urgent messages, keep coordinator uniquely conversable.
- **R7** surveys classical protocols (blackboard, CNP, FIPA-ACL) and modern frameworks (AutoGen, CrewAI, LangGraph, MetaGPT). Concludes the fan-out + synthesis pattern is superior to real-time multi-party chat for most workgraph use cases. Proposes: formalize fan-out+synthesis as a trace function, add minimal message typing (`inform`, `request`, `acknowledge`, `position`), invest in semi-sync TUI threads.
- **R10** frames all communication as stigmergy — the graph is the shared medium, chat is the exception. Proposes: make chat interactions deposit traces in the graph.

Key agreement: Don't build real-time multi-party chat. The evidence that it improves outcomes over async fan-out is weak (R7§7.2), and the complexity cost is high.

Cross-references: R2§8 (three-part position), R7§9 (the graph IS the conversation), R10§5 (could the chat itself be stigmergic?).

### Theme 3: The User Experience (R4, R9)

Two researchers focus on how humans interact with the system, but from complementary angles:

- **R4** provides concrete, actionable UX recommendations grounded in analysis of 14+ external sources (tmux, k9s, htop, WAI-ARIA, VS Code, Slack, Ratatui, Termux). The three-tier navigation architecture (zone focus → within-zone → command palette) is the central proposal, with reducing tabs from 9 to 4 as the most impactful change.
- **R9** provides the theoretical framing: the user is a meta-agent operating at multiple abstraction levels simultaneously. The inner loop gap, the attention problem, and the trust calibration challenge are the three key UX risks.

Key agreement: Progressive disclosure is essential — show only what's needed now, make everything else one action away.

Cross-references: R4§9 (preliminary position: three-tier architecture), R9§6 (inner loop vs. outer loop fundamental tension).

### Theme 4: The Theoretical Grounding (R5, R10, R7)

Three researchers provide theoretical frameworks that illuminate the design:

- **R5** applies Maturana & Varela's autopoiesis theory, Beth Dempster's autopoietic-sympoietic distinction, and homeostasis theory. Key insight: "The coordinator provides just enough top-down structure to prevent the sympoietic agent swarm from losing coherence, while the stigmergic graph medium provides just enough bottom-up flexibility to prevent the top-down structure from becoming rigid."
- **R10** applies Grassé's stigmergy (sematectonic and marker-based), Crowston's FLOSS stigmergy work, and Heylighen's universal coordination framework. Key insight: "Workgraph is a predominantly stigmergic system with strategically placed non-stigmergic escape hatches."
- **R7** applies blackboard systems, contract net protocol, and FIPA-ACL speech act theory. Key insight: "The question isn't 'why can't they all be conversations?' but 'when is conversation valuable enough to justify its cost?'"

Cross-references: R5§9 (sympoietic with autopoietic mechanisms), R10§10 (stigmergic with escape hatches), R7§9 (the graph IS the conversation).

### Theme 5: Precedent Systems (R1, R2, R3, R6, R8)

Five researchers draw on specific external systems for comparison:

| System | Used by | Lesson for Workgraph |
|--------|---------|---------------------|
| Erlang/OTP | R1, R2, R3 | Same type, different behaviour; call vs cast; permanent vs temporary |
| Kubernetes | R1, R3, R6, R8 | Uniform metadata, specialized Kind; controller-as-workload |
| Temporal | R3, R8 | Durable execution via event sourcing; workflow-as-code trade-offs |
| Actor Model (Hewitt) | R1, R2 | "Everything is an actor" — specialness is implementation, not ontology |
| Airflow | R8 | DAG limitations; operator/sensor/hook taxonomy; deferrable operators |
| Netflix Maestro | R3, R8 | Cyclic workflow support via patterns; horizontal scaling |
| Cylc | R8 | Domain-specific cycling model for periodic workflows |
| AutoGen | R7 | GroupChatManager as host/moderator pattern |
| LangGraph | R2, R7 | State-mediated communication; interrupt-and-resume |

The most cited system is Erlang/OTP (3 researchers), followed by Kubernetes (4 researchers). These two provide the strongest external validation for workgraph's architectural choices.

---

## 4. Trade-Off Analysis

### Trade-off 1: Unified Type vs. Separate Kinds

**Unified type (coordinator is a task with TaskKind field):**
- *Gain:* Simpler mental model — everything is a node in the graph. Single data structure. All graph analysis tools work on all nodes. Observability is automatic. Composability is preserved.
- *Lose:* Semantic overloading — every consumer must check the kind. The Task struct accumulates coordinator-specific fields (chat config, persistence config) that are meaningless for regular tasks. Lifecycle states (Open, InProgress, Done) don't map cleanly to coordinator states (Running, Paused, Restarting).

**Separate Kinds (Kubernetes-style NodeKind enum):**
- *Gain:* Type safety — coordinator-specific behavior is enforced at compile time. No leaky abstraction. Cleaner code in every module that currently checks `is_coordinator_task()`.
- *Lose:* Migration cost — touching every file that reads/writes tasks. Risk of duplicating graph analysis tooling for each Kind. Loss of the "everything is in one graph" simplicity.

**Recommendation from the research:** The middle path — a `task_type` or `NodeKind` field within the existing Task struct (R6's Option C) — captures most benefits of both. It replaces stringly-typed discrimination with enum-based typing while minimizing structural changes. Over time, coordinator-specific fields can be factored into a sub-struct (R6's Phase 2).

### Trade-off 2: Conversational Tasks vs. Fire-and-Forget

**Making every task conversable:**
- *Gain:* Symmetric communication model. Mid-task course corrections become possible. Agents can ask clarifying questions.
- *Lose:* Resource consumption (persistent LLM sessions for every active task). Complexity (concurrent reads/writes, deadlock prevention). Questionable reliability (LLMs may not context-switch coherently mid-task).

**Keeping the asymmetry (narrowed):**
- *Gain:* Resource efficiency. Clean failure containment. Simpler agent model.
- *Lose:* Messages can be missed. No mid-flight corrections without the interrupt-and-resume mechanism (which has its own costs).

**Recommendation from the research:** Keep the asymmetry but narrow it (R2). Enforce message checking as a completion gate. Add interrupt-and-resume for urgent messages. Don't make every task conversable — the cost-benefit doesn't work out (R7).

### Trade-off 3: Inner Loop Inclusion vs. Outer Loop Focus

**Including inner loop (orchestrator can do small work directly):**
- *Gain:* Reduced friction for trivial tasks. Users don't have to wait for task dispatch for a 5-line fix.
- *Lose:* Blurs the clean separation between orchestrator and worker. Creates ambiguity about what's in the graph and what's not. The orchestrator's context gets polluted with implementation details.

**Outer loop only (current design):**
- *Gain:* Clean separation. Everything is in the graph. Parallel execution. Legible record. Systematic evaluation.
- *Lose:* Overhead for small tasks. Loss of interactivity. Context discontinuity.

**Recommendation from the research:** R9 argues for a threshold heuristic (<50 lines, single file, clear spec → inline execution), but R8 warns against sacrificing the graph-as-data advantage. The compromise: make the outer loop fast enough that the overhead is negligible, rather than adding inner-loop capabilities that fragment the system's coherence.

### Trade-off 4: Stigmergic Purity vs. Pragmatic Direct Coordination

**More stigmergic (eliminate or minimize chat, make all coordination graph-mediated):**
- *Gain:* Scales better (no N² communication overhead). Better history (everything is in the graph). Simpler architecture.
- *Lose:* Responsiveness — stigmergic coordination is inherently asynchronous. User experience — humans think in conversations, not graph mutations. Error correction — when agents go off-track, the delay before correction is costly.

**More direct coordination (richer chat, real-time messaging, conversation protocols):**
- *Gain:* Responsiveness. Better user experience. Faster error correction.
- *Lose:* Scalability. Complexity (conversation state management, turn-taking protocols). History fragmentation (important decisions made in chat but not in the graph).

**Recommendation from the research:** R10's formulation is the most precise: "Make the escape hatch leave footprints." Keep direct coordination as the exception, but ensure every direct interaction deposits a trace in the stigmergic medium. Over time, the medium becomes richer and the need for direct coordination diminishes.

### Trade-off 5: Self-Organization vs. Human Control

**More autonomous (deeper evolution, self-bootstrapping, dynamic boundary management):**
- *Gain:* Reduced human overhead. Adaptive optimization. The system gets better at coordination over time.
- *Lose:* Risk of coherence loss (locally-optimizing agents producing globally incoherent output). Self-modification paradoxes. Resource exhaustion via meta-work. Homeostatic traps.

**More controlled (human approval gates, static agency definitions, limited evolution):**
- *Gain:* Predictability. Alignment with human intent. No runaway processes.
- *Lose:* Scalability ceiling (human attention is the bottleneck). Slower adaptation. Missed optimization opportunities.

**Recommendation from the research:** R5's principle: "The system should help produce itself under human supervision — a sympoietic partnership between human intent and machine self-organization." The deferred-approval mechanism for self-modifying operations is correctly designed. The immutable base prompt (base-system-prompt.md, behavioral-rules.md) defines an organizational invariant that evolution cannot touch.

---

## 5. Open Questions

### 5.1 Questions About Architecture

1. **The bootstrap problem:** Who dispatches the coordinator? The coordinator can't dispatch itself. `ensure_coordinator_task` creates it outside the normal lifecycle. Is this an acceptable asymmetry or a design smell? (Raised by R1§3.2, echoed by R6§2.2)

2. **Multi-coordinator coherence:** When multiple coordinators exist (.coordinator-0, .coordinator-1), how do they coordinate with each other? Is there a meta-coordinator? Does the graph alone provide sufficient coordination? (Raised by R1§3.3)

3. **The NodeKind migration path:** If the system moves toward a Kind-based model, how does backward compatibility work with existing graph.jsonl files? What is the migration cost in terms of code changes? (Raised by R6§5.2-5.3)

### 5.2 Questions About Communication

4. **Message urgency semantics:** R2 proposes interrupt-and-resume for "urgent" messages, but what defines urgency? Should it be caller-declared, system-inferred, or both? What is the false-positive cost of unnecessary interruptions? (Raised by R2§8, R7§6.4)

5. **Message typing granularity:** R7 proposes minimal message types (inform, request, acknowledge, position). Is this enough? Too much? How do agents know which type to use — is it specified in the prompt or enforced by the system? (Raised by R7§9)

6. **Chat-as-graph-trace implementation:** R10 proposes making chat interactions leave traces in the graph. What is the right granularity? Every message? Every "decision"? How do you avoid flooding the graph with chat noise? (Raised by R10§5)

### 5.3 Questions About UX

7. **Command palette implementation:** R4 recommends a command palette as the central navigation mechanism. What is the scope — tabs only, or also tasks, coordinators, and global actions? How does fuzzy matching work with 691 tasks? (Raised by R4§7)

8. **The inner loop threshold:** R9 proposes a threshold for inline execution (<50 lines, single file). How is this threshold determined? Is it configurable? What happens when the orchestrator misjudges and a "small" task turns out to be large? (Raised by R9§7)

9. **Attention budgeting:** R9 recommends batching reviews when many tasks complete simultaneously. What is the right batch size? How do you prioritize which completions to surface first? (Raised by R9§4)

### 5.4 Questions About Self-Organization

10. **Meta-work budget:** R5 warns that evaluation, evolution, and coordination can crowd out real work. What is the right ratio of meta-work to productive work? How do you measure and enforce this? (Raised by R5§7.4)

11. **Evolution convergence:** R5 identifies the risk of homeostatic traps — premature convergence at local optima. How do you detect when the evolution system is stuck? What perturbation mechanisms should be available? (Raised by R5§7.5)

12. **Stigmergy enrichment:** R10 proposes that over time, the stigmergic medium should become richer, reducing the need for direct coordination. How do you measure stigmergic richness? What signals indicate the medium is "rich enough"? (Raised by R10§10)

### 5.5 Questions That Span Multiple Perspectives

13. **The naming problem:** R6 argues that ".coordinator" implies hierarchy, conflicting with workgraph's stigmergic design. Alternatives proposed include "session," "channel," "nexus," "hub," and "steward." Which term best captures the coordinator's actual role? Does it matter? (Raised by R6§6)

14. **Graph explosion mitigation:** R8 identifies the risk that agents over-decompose work into fine-grained tasks. Current limits (999 subtasks, 999 depth) are generous. Should the system dynamically adjust decomposition depth based on graph size or agent performance? (Raised by R8§6, R5§7.1)

15. **Failure propagation model:** R8 notes that workgraph's "terminal-unblocks-all" model is unusual — most orchestration systems treat upstream failure as a hard block. Is this the right default for all cases? Should there be a way to declare "hard dependencies" that DO block on failure? (Raised by R8§6)

---

## 6. Preliminary Conceptual Framework: The Layered Coordination Model

Drawing on all 10 research perspectives, we propose a **Layered Coordination Model** that resolves the identified tensions by organizing workgraph's design around three explicit layers:

### Layer 1: The Stigmergic Medium (The Graph)

**Principle:** The graph is the primary coordination mechanism. All durable state lives here.

This layer embodies R10's insight that the graph is a stigmergic medium — agents coordinate indirectly through traces left in the shared environment. It validates R7's position that "the graph IS the conversation" and R8's argument that graph-as-data is workgraph's distinctive competitive advantage.

**What lives here:**
- Task definitions, statuses, dependencies, artifacts, logs
- Evaluation scores, evolution history, agency definitions
- Message queues (the `wg msg` system — already stigmergic per R10§3)
- Traces of non-stigmergic interactions (R10's "make the escape hatch leave footprints")

**Design rule:** Every coordination event should leave a trace in this layer, even if the event originated from a non-stigmergic channel.

### Layer 2: The Typed Node Model (The Ontology)

**Principle:** Nodes in the graph have explicit types. Uniformity is at the metadata level; specialization is at the behavior level.

This layer resolves the uniformity question by adopting R1's synthesis ("sound architecture with acknowledged asymmetry") and R6's implementation proposal (a `NodeKind` or `task_type` field), modulated by R3's insight that persistence is a spectrum.

**The type hierarchy:**
```
GraphNode (shared metadata: id, title, status, tags, log, created_at, ...)
├── Task (standard work item)
│   ├── Standard (default, ephemeral agent)
│   ├── Seed (generative, creates subgraph)
│   └── Lifecycle (internal: .assign, .evaluate, .flip)
└── Session (what is currently called "coordinator")
    ├── Interactive (persistent agent, user-facing chat)
    └── Background (persistent agent, no chat — future)
```

Key design choices:
- **Rename coordinator → session** (per R6§6): avoids hierarchical implication, conveys persistence and interactivity.
- **Separate lifecycle semantics** (per R1§6): Sessions have Running/Paused/Restarting/Stopped states, distinct from the Task lifecycle (Open/InProgress/Done/Failed).
- **Unified graph storage** (per R1§6, R6§5.2): Both Tasks and Sessions live in graph.jsonl with the same metadata envelope. The graph analysis tools (critical path, bottleneck detection, cycle detection) work on all node types.
- **Compile-time discrimination** (per R6§3.3): Replace all `is_coordinator_task()` string checks with enum-based pattern matching.

### Layer 3: The Interaction Channels (The Communication)

**Principle:** Different interaction patterns have different channels, but all channels feed back into the stigmergic medium.

This layer resolves the communication spectrum tension by adopting R2's "narrow the gap, don't eliminate it" position and R10's "footprints" principle.

**Three channels, ordered by synchronicity:**

| Channel | Parties | Synchronicity | Medium | Graph Trace |
|---------|---------|---------------|--------|-------------|
| **Stigmergic** | Any → Graph → Any | Asynchronous | Task artifacts, logs, statuses | IS the graph |
| **Message Queue** | Task ↔ Task | Semi-async | `wg msg` JSONL | Already in graph |
| **Chat** | User ↔ Session | Synchronous | Chat inbox/outbox | NEW: Deposit summaries |

Design rules:
- **Stigmergic channel is default.** Agents coordinate through the graph. No direct communication is needed for the routine case (task → artifacts → downstream task).
- **Message queue for directed communication.** When one task needs input from another, use `wg msg send`. Enforce message checking as a completion gate (R2's recommendation). Add minimal message typing: `inform`, `request`, `acknowledge` (R7's recommendation).
- **Chat for user interaction only.** The chat channel remains exclusive to Sessions (coordinators), not available for regular Tasks. This respects R2's finding that the coordinator's conversational nature serves its specific role.
- **All channels trace to the graph.** Chat interactions produce lightweight graph events (e.g., "User requested: focus on error handling" logged to the Session's task log). This implements R10's "footprints" principle without flooding the graph.

### Cross-Cutting Concerns

**Self-organization boundary (R5, R9):**
- The immutable base prompt and behavioral rules define the system's organizational invariant — the "DNA" that evolution cannot modify.
- The deferred-approval mechanism gates high-risk self-modification.
- Meta-work budgeting should be explicit: the system tracks the ratio of meta-tasks to productive tasks and alerts when it exceeds a configurable threshold.

**UX and attention management (R4, R9):**
- Three-tier navigation: zone focus → within-zone → command palette (R4's architecture).
- Reduce primary tabs to 4: Chat, Detail, Log, and "+" (command palette shortcut).
- Exception-based notifications: push alerts for failures, low FLIP scores, and urgent messages.
- Progressive disclosure: `wg status` → `wg show` → `wg context --scope full`.

**The inner loop question (R9):**
- Rather than blurring the orchestrator/worker boundary, optimize the outer loop for speed. Target sub-minute dispatch-to-completion for trivial tasks. If the outer loop is fast enough, the inner loop gap becomes irrelevant for all but the most interactive use cases.
- For genuinely interactive workflows (pair programming, exploratory debugging), integrate with inner-loop tools (Claude Code, Cursor) rather than trying to replicate their functionality within workgraph.

### How the Framework Resolves Each Tension

| Tension | Resolution |
|---------|------------|
| A: Uniformity | Typed nodes with shared metadata — unified storage, specialized behavior |
| B: Communication | Three channels with mandatory graph tracing — stigmergic default, sync exception |
| C: Lifecycle | Session-specific lifecycle states (Running/Paused/...) separate from Task lifecycle |
| D: Interaction | Three-tier TUI navigation + fast outer loop + inner-loop tool integration |
| E: Self-organization | Immutable core + deferred approval + meta-work budgeting |

---

## 7. Conclusion: The View From All Ten Angles

The ten research perspectives, while differing on specifics, converge on a shared understanding of workgraph's fundamental nature:

1. **Workgraph is a stigmergic system with strategic non-stigmergic escape hatches** (R10, validated by R5, R7). The graph is the primary coordination medium, and this should be deepened, not replaced.

2. **The coordinator-task distinction is real and should be made explicit** (all 10 researchers). Whether through a TaskKind field (R1), a NodeKind enum (R6), or simply "acknowledged asymmetry" (R1, R5), the fiction that coordinators are just tasks should be retired in favor of honest typing.

3. **The communication asymmetry is a feature, not a bug** (R2, R7). Not every task should be a conversation. The graph provides sufficient coordination for the routine case. Direct communication should be reserved for exceptions.

4. **The persistence spectrum is the right model for agent lifecycle** (R3). Binary persistent/ephemeral is too coarse. Checkpoint, resume, and session continuity should be available as a continuum.

5. **The UX bottleneck is attention, not functionality** (R4, R9). The TUI has too many options and too little progressive disclosure. The command palette + reduced tab count would be the single highest-impact UX improvement.

6. **Self-organization should be bounded by immutable organizational invariants** (R5). The evolution loop is workgraph's most distinctive feature. It should be deepened (resource-aware self-regulation, environmental sensing) while maintaining the safety boundaries that prevent the system from modifying its own control logic without human approval.

7. **Workgraph occupies a genuinely novel position in the orchestration landscape** (R8). Structural cycles, graph-as-data, terminal-unblocks-all, and seed tasks are distinctive innovations. The system should resist the temptation to become a general-purpose workflow engine and instead deepen its strengths in AI agent coordination.

The preliminary Layered Coordination Model proposed in §6 offers one path toward resolving these tensions. It is not the only path — the deliberation phase that follows will stress-test these proposals against the expertise of each researcher. But it provides a shared vocabulary and structural framework for that discussion.

---

## Appendix A: Research Statement Cross-Reference Matrix

| Theme | R1 | R2 | R3 | R4 | R5 | R6 | R7 | R8 | R9 | R10 |
|-------|:--:|:--:|:--:|:--:|:--:|:--:|:--:|:--:|:--:|:---:|
| Coordinator-as-task | ●● | ○ | ● | | ● | ●● | | ○ | | ● |
| Communication model | ○ | ●● | | | | | ●● | | ○ | ●● |
| Agent lifecycle | ● | ● | ●● | | | ● | | | | |
| TUI/UX design | | | | ●● | | ○ | | | ●● | |
| Self-organization | | | | | ●● | | | | ● | ● |
| Graph-as-data | | | | | | | ● | ●● | | ●● |
| Human role | | | | ● | | | | | ●● | |
| Precedent systems | ●● | ● | ●● | ● | ● | ●● | ●● | ●● | ● | ● |

●● = primary focus, ● = significant discussion, ○ = mentioned

---

## Appendix B: Multi-Coordinator Architecture Design

*This appendix integrates key findings from the multi-coordinator research (task: research-multi-coordinator), which explored how to support multiple concurrent coordinators / chat windows. These findings directly inform several of the synthesis themes — particularly the Uniformity Question (Tension A), the Communication Spectrum (Tension B), and the Layered Coordination Model's Session type.*

### B.1 Current Single-Coordinator Architecture

The existing architecture centers on a single `.coordinator` task:

- **Task lifecycle:** The daemon's `run_daemon` calls `ensure_coordinator_task()` on startup, creating a single `.coordinator` task with `Status::InProgress`, unlimited cycle iterations (`max_iterations: 0`), and the `coordinator-loop` tag.
- **IPC mechanism:** The daemon binds a Unix domain socket at `.workgraph/service/daemon.sock`. Clients send JSON-encoded `IpcRequest` variants; the server responds with `IpcResponse`. Chat uses `IpcRequest::UserChat { message, request_id, attachments }`.
- **Chat data flow:** User messages flow through `UserChat` IPC → `.workgraph/chat/inbox.jsonl` → Claude CLI subprocess (stdin) → stdout parsed by reader thread → `.workgraph/chat/outbox.jsonl` → TUI cursor-based polling. Each turn increments `loop_iteration` on the `.coordinator` task.

**Key files:**
- `.workgraph/chat/inbox.jsonl` — user messages
- `.workgraph/chat/outbox.jsonl` — coordinator responses
- `.workgraph/chat/.cursor` — TUI read position
- `.workgraph/chat/.coordinator-cursor` — daemon read position
- `.workgraph/service/daemon.sock` — Unix socket
- `.workgraph/service/coordinator-state.json` — tick stats

### B.2 Proposed Multi-Coordinator Graph Structure

Each coordinator gets its own task and chat channel:

- `.coordinator-0` (default, backward-compatible alias for `.coordinator`)
- `.coordinator-1`, `.coordinator-2`, etc.

Each coordinator is an independent root in the graph. Tasks created by a coordinator do NOT have a dependency on their coordinator — coordinators observe and dispatch, they don't block work.

**Chat channel isolation** uses per-coordinator subdirectories:
- `.workgraph/chat/0/inbox.jsonl` + `outbox.jsonl` + cursors
- `.workgraph/chat/1/inbox.jsonl` + `outbox.jsonl` + cursors

**IPC extension:** The `UserChat` request gains an optional `coordinator_id: Option<u32>` field (default 0).

### B.3 TUI Integration — Tab-Based Chat

The recommended approach is a tab bar within the Chat panel:

```
┌─ Chat ─────────────────────────────┐
│ [0: Main] [1: Auth Design] [+ New] │
│────────────────────────────────────│
│ user> help me plan the auth system │
│ coordinator> I'll create tasks...  │
│────────────────────────────────────│
│ > _                                │
└────────────────────────────────────┘
```

Each tab maintains independent `ChatState` (messages, scroll, editor, cursor). The chat state becomes `HashMap<u32, ChatState>` instead of a single `ChatState`. A CLI escape hatch (`wg chat --coordinator N`) provides access without TUI changes.

### B.4 Graph Visibility — Showing Coordinators in the Viz

Currently, `.coordinator` is filtered out as a system task when `show_internal` is false. The proposal:

1. **Exempt coordinators from internal-task filtering** by checking for the `coordinator-loop` tag.
2. **Special rendering:** Distinct visual treatment (cyan border, double-line box), status showing turn count (`Coordinator [turn 47]`), no progress bar (infinite cycle).
3. **Edge rendering:** Dashed/dotted "spawned-by" edges from coordinator → tasks it created, using provenance tracking.

### B.5 Visual Connection Between Chat and Graph

- **Color coding:** Each coordinator gets a unique color (coordinator-0 = cyan, coordinator-1 = green, etc.).
- **Selection sync:** Clicking a coordinator in the graph switches the chat tab; switching chat tabs highlights the coordinator in the graph.
- **Active indicator:** A `●` dot marks the currently-active coordinator in both the graph and the tab bar.

### B.6 Agent Pool and Isolation

**Shared agent pool (recommended):** All coordinators share the single `AgentRegistry` and `max_agents` limit. Separate pools would require complex quota management and could lead to underutilization.

**Conflict avoidance:**
- `coordinator_id: Option<u32>` field on Task for provenance tracking
- Existing graph-level locking (atomic write-to-temp-and-rename) handles concurrency
- Each coordinator gets its own Claude CLI subprocess with independent conversation context

**Resource limits:** `coordinator.max_coordinators = 4` (default) prevents unbounded agent creation. Lazy spawning starts a coordinator's subprocess only on first message.

### B.7 Relevance to the Synthesis

The multi-coordinator research directly addresses several open questions from the synthesis:

| Open Question | Multi-Coordinator Finding |
|---------------|--------------------------|
| **§5.1 Q2: Multi-coordinator coherence** | Coordinators coordinate through the shared graph, not through each other. No meta-coordinator needed — the stigmergic medium (Layer 1) provides sufficient coordination. |
| **§2.1 Tension A: Uniformity Question** | Multiple coordinators reinforce the case for a `NodeKind`/`Session` type — `.coordinator-0` through `.coordinator-N` are clearly a different kind of graph node than regular tasks. |
| **§6 Layer 2: Session type** | The multi-coordinator design naturally maps to the proposed `Session.Interactive` type, with each coordinator instance being one Session node in the graph. |
| **§6 Layer 3: Chat channel** | Per-coordinator chat subdirectories implement the proposed channel isolation, with the graph-trace requirement satisfied by logging turn summaries to each coordinator's task log. |
| **§2.5 Tension E: Self-organization boundary** | The `max_coordinators` limit is a concrete example of bounding self-organization — the system cannot spawn unlimited coordination overhead. |

---

## Appendix C: Researcher Key

| Code | Research Task ID | Focus Area |
|------|-----------------|------------|
| R1 | research-coordinator-architecture | Coordinator as special task — Erlang/OTP, Kubernetes, Actor Model |
| R2 | research-task-messaging | Messaging asymmetry — call vs. cast, delivery guarantees |
| R3 | research-agent-lifecycle | Agent lifecycle — persistent vs. ephemeral, checkpoint/resume |
| R4 | research-tui-ux | TUI/UX design — navigation, progressive disclosure, mobile |
| R5 | research-autopoiesis-and | Autopoietic systems theory — self-production, homeostasis |
| R6 | research-abstraction-consistency | Abstraction consistency — leaky abstractions, NodeKind proposal |
| R7 | research-multi-party | Multi-party conversation protocols — blackboard, CNP, FIPA-ACL |
| R8 | research-graph-based | Graph-based workflow orchestration — Airflow, Temporal, Cylc |
| R9 | research-human-agent | Human-agent interaction — inner/outer loop, attention, trust |
| R10 | research-stigmergic-coordination | Stigmergic coordination — indirect coordination, graph as medium |
