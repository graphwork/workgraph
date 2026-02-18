# Veracity Exchange Integration with Workgraph

**Author:** scout (analyst)
**Date:** 2026-02-18
**Status:** Research / Design Exploration

---

## 1. The Vision

Nikete's veracity exchange concept connects workgraph to a broader trust and performance market. The core idea: workflow sub-units produce measurable real-world outcomes, and those outcomes become the basis for a trust network where demonstrated competence earns access to more valuable work.

Five pillars:

1. **Outcome-scored sub-units** — Each task has measurable real-world outcomes (portfolio P&L, prediction MSE, negative mean squared error). These outcomes become veracity scores for individual work units.

2. **Trust market** — Public (non-sensitive) prompt sections can be posted to a public market where others suggest improvements. Good suggestions signal credibility, which qualifies suggesters for private pay-for-performance tasks.

3. **Network learning** — Over time, learn which peers you want to do veracity exchanges with — a trust network built on demonstrated competence.

4. **Information flow control** — The workflow graph structure itself controls what data leaves your node. Interfaces between tasks define the boundary between public and private work.

5. **Latent payoff** — Some workflow sub-units have potentially latent payoffs — results that take time to evaluate (e.g., portfolio performance over weeks).

This document examines what workgraph already provides, what's missing, and how a veracity exchange system would interact with the existing architecture.

---

## 2. What the Provenance/Logging System Already Supports

The provenance system (designed in `docs/design/provenance-system.md`, core implementation in `src/provenance.rs`) and the agency system (`src/agency.rs`, documented in `docs/AGENCY.md`) together provide substantial infrastructure that a veracity exchange could build on.

### 2.1 Operation Log — Full History

The append-only operation log (`OperationEntry { timestamp, op, task_id, actor, detail }` in `.workgraph/log/operations.jsonl`) captures every graph mutation. With the provenance design fully implemented, this covers:

- Task creation with full initial fields
- Every state transition (claim, done, fail, abandon, retry, pause, resume)
- Every field edit with before/after diffs
- Artifact registration with SHA-256 hashes
- Agent assignments with content-hash references
- Archive and garbage collection events

**Relevance to veracity exchange:** This is the _audit trail_ that proves what happened. A veracity exchange needs to verify claims about work done — the operation log provides a tamper-evident (append-only, timestamped) record of all graph mutations. The SHA-256 hashing of artifacts at registration time (provenance design Section 3) means artifact integrity is verifiable: you can prove that the output registered at time T had a specific content hash.

### 2.2 Agent Conversation Archive

Agent prompts and outputs are archived to `.workgraph/log/agents/<task-id>/<timestamp>/`. The provenance design extends this to:

- Archive prompts at spawn time (not just at completion)
- Archive output on dead-agent detection
- Each retry gets a new timestamped subdirectory

**Relevance:** The prompt archive is the _input specification_ and the output archive is the _work product_. For public prompt sections in a trust market (pillar 2), the archived prompt could be partitioned into public and private sections.

### 2.3 Artifact Traceability

When `wg artifact add` is called, the provenance system records the artifact's path and SHA-256 hash. When a downstream task consumes artifacts from dependencies (via `build_task_context()` in `spawn.rs`), the consumed artifact hashes are recorded in the spawn provenance entry.

**Relevance:** This is exactly the _data lineage_ a veracity exchange needs. You can trace: task A produced artifact X (hash `abc123`) → task B consumed artifact X → task B produced artifact Y (hash `def456`). This chain of custody is the basis for attributing outcomes to specific work units.

### 2.4 Evaluation System — Existing Scoring

The agency system already implements multi-dimensional evaluation of agent work:

```
Evaluation {
    id, task_id, agent_id, role_id, motivation_id,
    score: f64,            // overall score 0.0–1.0
    dimensions: {          // correctness, completeness, efficiency, style_adherence
        "correctness": 0.9,
        "completeness": 0.85,
        ...
    },
    notes: String,
    evaluator: String,     // "claude:opus" etc.
    model: Option<String>, // model used by the agent
}
```

Evaluations propagate to three levels: the agent's `PerformanceRecord`, the role's record (with motivation as context), and the motivation's record (with role as context). The evolution system (`wg evolve`) uses this data to improve the agency over time.

**Relevance:** This is a _proto-veracity score_. The evaluation dimensions (correctness, completeness, efficiency, style_adherence) are internal quality metrics. A veracity exchange extends this by connecting internal scores to _external outcome measures_ — the real-world results that the work was supposed to produce.

### 2.5 Content-Hash Identity

Every role, motivation, and agent is identified by a SHA-256 content hash of its identity-defining fields. The same content always produces the same ID.

**Relevance:** Content-hash IDs are a natural fit for a trust network. An agent's identity is verifiable and immutable — you can prove that agent `a3f7c21d` has specific capabilities and a specific performance track record, and that record can't be retroactively modified (the hash would change).

### 2.6 Lineage and Evolution

The lineage system tracks evolutionary history: parent IDs, generation number, creation source. The evolution system creates new roles/motivations from existing ones based on performance data.

**Relevance:** Lineage provides _provenance of capabilities_. If agent `a3f7c21d` descended from agent `b4e8d92f` via mutation, and `b4e8d92f` has a strong track record, that ancestry is verifiable evidence of quality.

---

## 3. What's Missing

### 3.1 Outcome Scoring

**Gap:** The evaluation system scores based on _internal quality_ (does the output match the spec?). It doesn't connect to _external outcomes_ (did the code actually work in production? did the portfolio strategy actually make money?).

**What's needed:**

- **Outcome definition on tasks.** A way to specify what real-world metric a task is measured against. The `deliverables` field on `Task` is close but only lists expected paths, not measurement criteria.
- **Outcome recording.** A way to record measured outcomes, possibly long after the task completes. Current evaluations happen at task completion time; outcome scores may arrive days or weeks later (latent payoff — pillar 5).
- **Outcome-to-evaluation bridge.** Connect recorded outcomes back to the agent/role/motivation that produced the work, updating their performance records.

**Data model sketch:**

```rust
struct OutcomeSpec {
    metric: String,          // "portfolio_pnl", "prediction_mse", "test_pass_rate"
    direction: Direction,    // Higher or Lower is better
    target: Option<f64>,     // optional target value
    measurement_window: Option<Duration>, // how long to wait before measuring
}

struct OutcomeRecord {
    task_id: String,
    spec: OutcomeSpec,
    measured_value: f64,
    measured_at: String,     // timestamp
    veracity_score: f64,     // normalized 0.0–1.0
    evidence: Value,         // supporting data / proof
}
```

**Where this lives:** Outcome specs could be a new field on `Task` (alongside `deliverables`). Outcome records would go in `.workgraph/outcomes/<task-id>.json`, with a new `wg outcome record <task-id> --metric <name> --value <float>` command.

### 3.2 Public/Private Task Classification

**Gap:** All task data is equally private today. There's no concept of visibility levels.

**What's needed:**

- **Visibility field on tasks.** At minimum: `private` (default), `public-prompt` (prompt can be shared), `public` (prompt and output can be shared).
- **Prompt partitioning.** The ability to mark sections of a prompt as public or private. The current prompt template (`src/service/executor.rs`) is a single template string. Partitioning it requires structured prompt sections.
- **Redaction layer.** When exporting for the exchange, automatically strip private sections from prompts and outputs.

**Interaction with existing systems:**

The `build_task_context()` function in `spawn.rs` aggregates dependency artifacts and logs into a context string. If upstream tasks are private, their artifacts must not leak into a public downstream task's context. The DAG structure provides natural information boundaries (pillar 4): a task only sees artifacts from its direct dependencies. Making this a _security_ boundary rather than just a _convenience_ boundary requires enforcing visibility at the context-building layer.

### 3.3 Exchange Protocol

**Gap:** No mechanism for publishing work units, receiving suggestions, or managing credibility.

**What's needed:**

- **Publishing.** Export a task's public-facing data (redacted prompt, outcome spec, outcome result) to an exchange format. This is an _export_ problem, not a core workgraph change.
- **Suggestion intake.** Accept suggested improvements from external parties, attribute them, and evaluate their quality. This could work through workgraph's existing task model: a suggestion is a task with a reference back to the original.
- **Exchange identity.** How a workgraph node identifies itself on the exchange. The content-hash agent identity system is a strong foundation, but needs a way to present a public profile without revealing internal role/motivation details.

### 3.4 Credibility Tracking

**Gap:** The agency system tracks internal agent performance but has no concept of cross-node credibility.

**What's needed:**

- **Peer performance records.** Track how well suggestions from external peers have performed. Similar to the existing `PerformanceRecord` but scoped to peer identity rather than role/motivation.
- **Trust decay.** Credibility should decrease without recent positive evidence. The evolution system's retirement heuristics (`retention_heuristics` in config) are analogous but only apply to internal agents.
- **Trust transitivity.** If peer A trusts peer B, and peer B trusts peer C, should peer A have partial trust in C? This is a network property that goes beyond the current per-entity performance tracking.

---

## 4. Interface Design

How would a veracity exchange system interact with workgraph? Three options, from least to most integrated.

### 4.1 Option A: External Service with CLI Bridge

The exchange runs as a separate service. Workgraph interacts with it via CLI commands or a thin adapter.

```
workgraph (local)           veracity exchange (remote)
    │                              │
    ├── wg exchange publish ───────►  publish task outcomes
    ├── wg exchange suggest ───────►  submit improvement suggestions
    ├── wg exchange pull ──────────►  receive suggestions for my tasks
    ├── wg exchange peers ─────────►  list trusted peers
    │                              │
    ◄── wg exchange apply ─────────  apply accepted suggestion as task
```

**Implementation:** New `wg exchange` subcommand group. Exchange client library as a dependency. Outcome recording, visibility fields, and credibility tracking are local workgraph features. The exchange protocol is the only networked component.

**Pros:**
- Clear separation of concerns. Workgraph handles task management; the exchange handles the market.
- Exchange can evolve independently.
- Local workgraph features (outcome scoring, visibility) are useful even without the exchange.

**Cons:**
- Manual publish/pull workflow adds friction.
- Two systems to maintain.

### 4.2 Option B: Event Hooks

Workgraph emits events that an exchange plugin can subscribe to. The provenance system's operation log is already an event stream.

```
wg done task-x
    │
    ├── provenance::record("done", ...) ──► operations.jsonl (existing)
    │
    ├── hook: "on_done" ──────────────────► exchange plugin
    │     │
    │     ├── if task.visibility == "public":
    │     │     publish outcome to exchange
    │     │
    │     └── if task has pending suggestions:
    │           evaluate suggestion quality
```

**Implementation:** Add a hook system to workgraph that fires on task state transitions. The provenance system already records these transitions — hooks would be a "side-effect" triggered by the same events. Hooks could be configured in `config.toml`:

```toml
[hooks]
on_done = ["wg-exchange-hook publish {{task_id}}"]
on_outcome = ["wg-exchange-hook record {{task_id}}"]
```

**Pros:**
- Automatic publishing without manual intervention.
- The provenance system already captures all the events hooks would need.
- Hooks are useful for many other integrations (CI triggers, notifications, etc.), so the infrastructure is reusable.

**Cons:**
- Hook execution adds latency to task operations (can be mitigated by async/background hooks).
- Hook failures need to be isolated (same `let _ =` pattern as provenance).

### 4.3 Option C: Native Integration

The exchange is a first-class workgraph feature, like the agency system.

**Implementation:** New module (`src/exchange.rs`) alongside `src/agency.rs`. Exchange identity, credibility tracking, and outcome scoring are core workgraph types. The exchange protocol is part of the workgraph daemon.

**Pros:**
- Tightest integration. Outcome scoring and credibility tracking work with zero configuration.
- Can leverage internal workgraph state directly (graph structure, evaluation history, agent identities).

**Cons:**
- Massively increases scope. The agency system is ~2,346 lines; an exchange module would be comparable or larger.
- Couples workgraph to a specific exchange protocol.
- Forces all workgraph users to install exchange dependencies even if they don't use the feature.

### 4.4 Recommendation

**Start with Option A (CLI bridge) for the protocol, combined with local features that support all options.**

The local features — outcome scoring, visibility classification, credibility tracking — are valuable regardless of how the exchange is accessed. Build these as core workgraph features. Then implement the exchange interaction as a CLI subcommand that can be upgraded to event hooks (Option B) when friction becomes a felt problem.

Do not build native integration (Option C) until the exchange protocol is stable and proven. Premature coupling to an unstable protocol is worse than the friction of a CLI bridge.

---

## 5. What Requires Forking vs. Extensions

### 5.1 Changes to Core Workgraph (Requires Modification)

These changes affect fundamental data structures and must happen in the main codebase:

| Change | What | Where | Effort |
|--------|------|-------|--------|
| Outcome spec on tasks | New `outcome_spec` field on `Task` | `src/graph.rs` | Small — one new optional field, serde support |
| Visibility field | New `visibility` enum on `Task` | `src/graph.rs` | Small — one new field with default `Private` |
| Outcome recording command | `wg outcome record` | `src/commands/outcome.rs` | Medium — new command, outcome storage, performance update |
| Outcome-aware evaluation | Connect outcomes to agent performance | `src/agency.rs` | Medium — extend `record_evaluation()` to accept outcome data |
| Provenance: outcome events | Record outcome events in operation log | Existing `provenance.rs` | Small — one new op type |

**Total core changes: ~300-400 lines.** Comparable to adding one new command (e.g., `wg assign` was ~200 lines).

### 5.2 Extensions (No Core Changes Needed)

These can be built as separate tools/scripts that use workgraph's existing CLI and data files:

| Extension | What | Why It's External |
|-----------|------|-------------------|
| Exchange client | Publish outcomes, receive suggestions | Network protocol is exchange-specific |
| Redaction layer | Strip private data from exports | Read-only operation on existing data |
| Peer credibility DB | Track peer performance | Separate trust model from internal agency |
| Suggestion-to-task converter | Create tasks from external suggestions | Uses `wg add` CLI |
| Exchange identity manager | Manage public keys and profiles | Separate identity from internal agent hashes |

### 5.3 Features That Bridge Both

Some features start as extensions but may later warrant core integration:

| Feature | Start As | Migrate To Core When |
|---------|----------|---------------------|
| Credibility tracking | External DB | When it informs agent assignment decisions |
| Prompt partitioning | Convention (markdown sections) | When the redaction needs to be enforced, not just conventional |
| Trust network | External graph | When trust scores affect task routing |

---

## 6. Relationship to Agency Evaluation/Evolution

The agency system's evaluate → evolve loop is the closest existing analog to veracity exchange. Here's how they connect:

### 6.1 Evaluation as Internal Veracity

The current evaluation system measures whether an agent did what it was asked to do. This is _internal veracity_ — did the agent meet the spec?

A veracity exchange adds _external veracity_ — did the spec itself lead to good real-world outcomes? The two form a chain:

```
Agent Performance          Task Outcome            Veracity Score
(did agent follow spec?)  (did spec work?)        (combined measure)
       │                       │                        │
  evaluation.score        outcome.value            veracity_score
  (0.0–1.0)              (domain metric)           (normalized)
       │                       │                        │
       └───────────┬───────────┘                        │
                   │                                    │
          composite veracity ────────────────────────────┘
```

An agent might score 0.95 on internal evaluation (it perfectly implemented the spec) but the task might score 0.3 on outcome (the spec was wrong). The composite veracity distinguishes agents that execute well from agents that also produce good outcomes.

### 6.2 Evolution with Outcome Data

The evolution system (`wg evolve`) currently uses evaluation scores to improve roles and motivations. With outcome scoring, it could also evolve based on real-world results:

- **Outcome-weighted evolution.** Roles whose tasks have high outcome scores should be favored. A role that produces correct-but-useless code (high evaluation, low outcome) should be deprioritized vs. one that produces imperfect-but-effective code (moderate evaluation, high outcome).
- **Outcome-informed gap analysis.** The `gap-analysis` evolution strategy identifies unmet needs. With outcome data, it can identify gaps between _evaluated quality_ and _real-world impact_ — a much more valuable signal.
- **Latent payoff patience.** The evolution system currently runs on available evaluation data. With latent payoffs (pillar 5), some tasks won't have outcome scores yet. The evolver needs to handle incomplete outcome data gracefully — probably by weighting available outcome data more heavily as it arrives, rather than blocking on it.

### 6.3 Credibility as Extended Performance

The agency system tracks performance per agent, per role, and per motivation, with cross-references (`context_id`). Credibility tracking in a veracity exchange is structurally identical but scoped to _external peers_ instead of internal agent identities:

```rust
// Existing (internal)
PerformanceRecord {
    task_count: u32,
    avg_score: Option<f64>,
    evaluations: Vec<EvaluationRef>,  // context_id = motivation_id or role_id
}

// New (external, same shape)
PeerCredibility {
    suggestion_count: u32,
    avg_impact: Option<f64>,
    suggestions: Vec<SuggestionRef>,  // context_id = task_id or domain
}
```

The synergy matrix (`wg agency stats`) that shows how roles perform with different motivations could be extended to show how peers perform across different task domains.

### 6.4 Trust Levels and the Trust Market

The `TrustLevel` enum (`Verified`, `Provisional`, `Unknown`) on agents is a simple version of what the trust market needs. Currently trust is set manually. With a veracity exchange, trust could be _earned_ through demonstrated performance:

```
Unknown ──(first suggestion accepted)──► Provisional ──(sustained track record)──► Verified
```

This maps naturally to the existing `TrustLevel` transitions but makes them data-driven rather than manual.

---

## 7. The DAG as Information Flow Controller

Pillar 4 — the workflow graph structure controls what data leaves your node — is perhaps the most elegant insight. The DAG is already an information flow controller:

- Tasks only see artifacts from their direct dependencies (`build_task_context()` in `spawn.rs`)
- The `blocked_by` edges define what information flows where
- Artifacts are the explicit interface between tasks

To make this a _security_ boundary for a veracity exchange:

### 7.1 Boundary Tasks

Designate certain tasks as "boundary tasks" — the interface between private and public work. A boundary task:

- Has `visibility: public-prompt` or `visibility: public`
- Consumes private upstream artifacts but produces public outputs
- Its prompt is the public specification; its internal process may reference private data but its _output_ is public

The DAG naturally supports this: everything upstream of a boundary task is private (internal computation), and the boundary task's output is what gets shared.

```
[private: data-prep] ──► [private: model-train] ──► [boundary: publish-predictions] ──► [public: evaluate-accuracy]
```

### 7.2 Graph Slicing for Export

When publishing to the exchange, export a _slice_ of the DAG: the subgraph rooted at boundary tasks, with private upstream tasks replaced by their public interface (output artifact hashes, outcome specs, but not prompts or internal logs).

This is a read-only operation on the existing graph structure. No core changes needed — just a new `wg exchange export` command that walks the DAG and applies visibility filtering.

---

## 8. Latent Payoff Handling

Some task outcomes can't be measured immediately. A portfolio strategy takes weeks to evaluate. A prediction's accuracy depends on future events. This requires:

### 8.1 Deferred Outcome Recording

The `OutcomeSpec` should include a `measurement_window` — how long to wait before the outcome can be measured. The system should:

1. At task completion: record the expected measurement window
2. Periodically (or on trigger): check which tasks have matured past their measurement window
3. Prompt for outcome recording (or auto-record if connected to a data source)

### 8.2 Provisional vs. Final Scores

A task's veracity score should have a status:

- **Pending** — task completed, outcome not yet measurable
- **Provisional** — early outcome data available but measurement window hasn't elapsed
- **Final** — measurement window elapsed, outcome score is definitive

This maps to the evaluation system's existing pattern of recording evaluations with timestamps. Provisional and final scores are just evaluations at different points in time. The `PerformanceRecord` already stores evaluation history, so an agent's track record naturally incorporates the progression from provisional to final scores.

### 8.3 Discount Rate

Older outcomes should carry less weight than recent ones. The evolution system's trend computation (comparing first and second halves of recent scores) is a simple version of this. A veracity exchange might use exponential discounting:

```
weight(outcome) = e^(-λ * age_in_days)
```

This is a computation on top of existing data, not a structural change.

---

## 9. Implementation Sequence

Ordered by what unlocks the most value with the least coupling to exchange-specific decisions.

### Phase 1: Local Foundations (No Exchange Dependency)

These features are useful independently of any exchange.

1. **Outcome spec field on Task** — Add `outcome_spec: Option<OutcomeSpec>` to the Task struct. Small change to `graph.rs`.

2. **`wg outcome record` command** — Record measured outcomes against tasks. Store in `.workgraph/outcomes/`. Update provenance log with outcome events.

3. **Visibility field on Task** — Add `visibility: Visibility` with `{Private, PublicPrompt, Public}`. Default `Private`. Small change to `graph.rs`.

4. **Outcome-aware performance updates** — Extend `record_evaluation()` to optionally incorporate outcome data when available.

### Phase 2: Exchange Primitives (Light External Coupling)

5. **Graph slice export** — `wg exchange export` that produces a redacted DAG slice for sharing. Read-only, no core changes.

6. **Suggestion intake** — `wg exchange import-suggestion` that creates a task from an external suggestion with attribution metadata.

7. **Peer credibility tracking** — Local DB of peer performance based on suggestion outcomes. Similar shape to `PerformanceRecord`.

### Phase 3: Exchange Integration (Requires Protocol)

8. **Exchange client** — Publish outcomes, receive suggestions, manage identity on a specific exchange network.

9. **Event hooks** — Automatic publishing on task completion for tasks with public visibility.

10. **Trust-informed assignment** — Use peer credibility data to influence task routing decisions in the coordinator.

---

## 10. Open Questions

1. **What exchange protocol?** The document deliberately avoids specifying a protocol. The local features (outcome scoring, visibility, credibility) work regardless of protocol. The protocol choice can be deferred until there's a concrete exchange implementation to target.

2. **Who measures outcomes?** Some outcomes are self-reported (the task owner measures their own results). Others need third-party verification. The system should support both, with third-party verification carrying more credibility weight.

3. **Privacy of outcome data.** Outcome scores themselves may be sensitive (revealing portfolio P&L, for example). The visibility system needs to handle outcome privacy separately from prompt/output privacy.

4. **Scale of trust network.** The agency system's performance tracking works well for tens of agents. A trust network might have thousands of peers. The `PerformanceRecord` structure may need indexing or summarization for large peer sets.

5. **Bootstrapping the trust market.** The trust market requires an initial set of public tasks with measurable outcomes. Workgraph's existing evaluation system could bootstrap this: tasks with high evaluation scores and public visibility become the initial "proven work" that attracts suggestions.

6. **Incentive alignment.** Why would someone post good suggestions publicly? The answer is credibility → access to private paid tasks. But this only works if there are enough private paid tasks to make credibility valuable. Bootstrapping both sides of this market is a classic chicken-and-egg problem.

---

## 11. References

- **Provenance system design:** `docs/design/provenance-system.md` — operation log, artifact archival, agent capture, replay capability
- **Logging gaps research:** `docs/research/logging-gaps.md` — comprehensive audit of what is/isn't captured
- **Nikete's replay system review:** `docs/research/nikete-logging-review.md` — trace/distill/replay pipeline, canon concept
- **Agency system documentation:** `docs/AGENCY.md` — roles, motivations, agents, evaluation, evolution
- **Agency implementation:** `src/agency.rs` — evaluation recording, performance tracking, content-hash identity
- **Provenance implementation:** `src/provenance.rs` — append-only operation log with zstd rotation
- **Amplifier integration proposal:** `docs/research/amplifier-integration-proposal.md.typ` — executor model, prompt template system
