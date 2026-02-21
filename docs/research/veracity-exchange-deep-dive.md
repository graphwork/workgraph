# Veracity Exchange × Workgraph: Deep Dive

**Last updated:** 2026-02-20 — revised based on nikete's actual design doc (`nikete/main:docs/design-veracity-exchange.md`) and `nikete/vx-adapter` branch code.

## Executive Summary

Veracity Exchange is a system for scoring workflow sub-units against real-world outcomes and using those scores to build a peer trust network. Workgraph is a task coordination system with composable agent identities, provenance logging, and performance evaluation. This report analyzes how these systems integrate, based on nikete's actual design documents and code rather than speculation.

**Key findings from nikete's actual code and design:**

1. **The design doc is comprehensive.** nikete's `design-veracity-exchange.md` (738 lines) defines 8 new data structures (Outcome, Attribution, Sensitivity, Challenge, Suggestion, PeerCredibility, RewardPolicy, DataFlowEvent), a full CLI command set, and a 5-phase implementation plan. This is far more detailed than what we theorized.

2. **The vx-adapter branch is research + architecture convergence, not VX implementation.** The branch renames agency→identity, evaluate→reward, motivation→objective, and independently adds provenance logging, model registry, and GC — all converging on our architecture. No actual VX protocol code exists yet.

3. **Sensitivity, not Visibility.** nikete uses a 4-level `Sensitivity` enum (Public, Interface, Opaque, Private) rather than our proposed 3-level `Visibility`. The `Interface` level — share what a task does but not its data — is a genuinely useful middle ground we missed.

4. **Attribution is more sophisticated than expected.** The design includes Ablation, Shapley, Replacement, Manual, and ProportionalToEval methods for attributing outcomes to sub-tasks. We only speculated about Shapley values.

5. **Canon as suggestion format.** Suggestions from peers ARE Canons (structured knowledge artifacts). This connects the distill/canon system to the exchange — even though canon was removed from the vx-adapter branch, the design doc still builds on it.

6. **He recommends CLI bridge first, not native integration.** More conservative than our Phase 1-4 roadmap. Explicitly advises against native integration until the exchange protocol is stable.

---

## 1. What Veracity Exchange Is (Updated from Design Doc)

### Core Model (from nikete's design doc)

Veracity Exchange is NOT just an API marketplace. From `design-veracity-exchange.md`:

> A workgraph for a measurable task produces **real-world outcomes** (P&L, -MSE). These outcomes can be attributed back to individual work units in the graph. This attribution creates **veracity scores** — ground-truth quality signals grounded in reality, not LLM self-evaluation.

The key insight is that **proper scoring rules** underpin the entire system. A scoring rule is *proper* if the optimal strategy is to report your true belief. When suggestions are scored against real-world outcomes (not self-report or LLM judgment), participants are incentivized to suggest genuinely good improvements — not to game evaluators.

Concretely:
- **Prediction tasks**: scored by -MSE or log score against realized values (proper scoring rules)
- **Portfolio tasks**: scored by risk-adjusted returns (Sharpe, P&L over a defined period)
- **Sub-unit attribution**: marginal contribution measured by ablation or replacement

```
YOUR NODE                                    PEER NETWORK
┌──────────────────────────┐                ┌──────────────┐
│  workgraph               │   public       │  peer nodes  │
│  ┌──────┐  ┌──────────┐ │  challenges    │              │
│  │task A│→ │task B     │ │ ──────────→    │  suggest     │
│  │(priv)│  │(public)   │ │               │  improvements│
│  └──────┘  └──────────┘ │ ←──────────    │              │
│       ↓         ↓        │  suggestions   └──────────────┘
│  ┌──────────────────┐   │                       │
│  │ outcome measure  │   │   veracity scores     │
│  │ (P&L, -MSE)      │   │ ──────────────────→   │
│  └──────────────────┘   │   (ground truth)      │
│       ↓                  │                       │
│  attribution → per-task  │   credibility         │
│  veracity scores         │ ←──────────────────   │
│       ↓                  │   (accumulated)       │
│  qualify peers for       │                       │
│  private tasks           │                       │
└──────────────────────────┘                       │
                                                   ↓
                                          peer network learning:
                                          who to exchange with,
                                          for which topics
```
*(Diagram from nikete's design doc)*

### Previous Understanding vs. Reality

| What we theorized | What nikete actually designed |
|-------------------|------------------------------|
| Simple API bridge to `POST /veracity/v1/run-day` | Full exchange system with Outcomes, Attribution, Challenges, Suggestions, Credibility |
| 3-level Visibility enum | 4-level Sensitivity enum with graph constraints |
| Veracity executor as recommended approach | CLI bridge recommended; native integration explicitly deferred |
| Single scoring metric (P&L + MSE) | Multi-metric, multi-horizon scoring with configurable attribution methods |
| Vague notion of "peer trust" | Concrete PeerCredibility with Brier scoring for calibration |
| Portfolio positions as the unit of exchange | Challenges (sanitized task postings) as the unit, with Canon-formatted suggestions |

---

## 2. New Data Structures (from nikete's design doc)

nikete defines 8 new types that form the VX integration layer. These are concrete Rust struct definitions, not speculative.

### A. Outcome

An externally-observed, ground-truth measurement tied to a workgraph or sub-graph:

```rust
pub struct Outcome {
    pub id: String,                    // "outcome-{graph_id}-{timestamp}"
    pub graph_id: String,              // which workgraph (or sub-graph root) this measures
    pub metric: String,                // "sharpe", "pnl", "neg_mse", "log_score"
    pub value: f64,
    pub period: Option<OutcomePeriod>, // time window measured
    pub source: String,                // "manual", "api:alpaca", "api:databento"
    pub recorded_at: String,
    pub metadata: HashMap<String, String>,
}
```

**Surprise:** Outcomes are generic, not tied to a specific API. Multiple outcomes can be recorded per graph (daily P&L, weekly Sharpe, etc.). This is more flexible than our assumption of a single `run-day` score.

### B. Attribution

Maps an outcome to individual tasks, quantifying each task's contribution:

```rust
pub enum AttributionMethod {
    Ablation,                              // run without each task, measure delta
    Shapley,                               // average marginal contribution across orderings
    Replacement { alternative_run_id: String }, // replace with alternative, measure delta
    Manual,
    ProportionalToEval,                    // cheap fallback using existing eval scores
}
```

**Surprise:** This is much more rigorous than we expected. The `Replacement` method directly answers "did this suggestion improve outcomes?" — it's the natural method for evaluating peer suggestions. nikete recommends: Replacement for suggestion scoring, Ablation for initial attribution, Shapley only for small sub-graphs where complementarity matters.

### C. Sensitivity (replaces our proposed Visibility)

```rust
pub enum Sensitivity {
    Public,     // Description, interfaces, verification, evaluation — all shareable
    Interface,  // Description shareable, inputs/outputs sanitized (what, not data)
    Opaque,     // Only existence and type visible
    Private,    // Never leaves the node, not even referenced in public postings
}
```

**Key difference from our proposal:** The `Interface` level is the important addition. It lets you share *what a task does* without sharing *the data it operates on*. Our 3-level `Visibility` (Private, PublicDefinition, PublicFull) missed this useful middle ground.

Graph constraints enforce sensitivity:
- A `Public` task can only depend on `Public` or `Interface` tasks
- `wg veracity check` validates these constraints
- Default is `Private` — opt-in to sharing

### D. Challenge

A sanitized, public posting of a task seeking improvement suggestions:

```rust
pub struct Challenge {
    pub id: String,
    pub task_id: String,
    pub title: String,
    pub description: String,         // sanitized per sensitivity
    pub interface: ChallengeInterface, // inputs/outputs with schemas
    pub verify: Option<String>,
    pub baseline_score: Option<f64>,
    pub metric: String,
    pub reward_policy: RewardPolicy,
    pub required_credibility: f64,   // minimum credibility to submit (0.0 = open)
}
```

Challenges intentionally DO NOT include: the actual solution, private dependencies, raw data.

### E. Suggestion

A peer's proposed improvement — formatted as a Canon:

```rust
pub struct Suggestion {
    pub id: String,
    pub challenge_id: String,
    pub peer_id: String,
    pub canon: Canon,                // the suggested approach
    pub confidence: Option<f64>,     // peer's self-assessed confidence
    pub status: SuggestionStatus,    // Pending → Accepted → Tested → Scored
}
```

**Surprise:** Suggestions ARE Canons. This means the distill/canon system and the exchange system share the same format. A suggestion is injected into agent prompts via `{{task_canon}}` during replay testing. Even though nikete removed canon from the vx-adapter branch code, the design doc still treats Canon as the core exchange format.

### F. PeerCredibility

```rust
pub struct CredibilityScore {
    pub score: f64,
    pub n_suggestions: u32,
    pub n_improvements: u32,
    pub n_degradations: u32,
    pub avg_delta: f64,
    pub brier_score: f64,    // calibration: confidence vs actual improvement rate
}
```

**Surprise:** Brier scoring for calibration is more rigorous than we expected. It measures whether peers know when they're right — a proper scoring rule that incentivizes honest confidence reporting.

### G. RewardPolicy

Per-challenge incentive structure:

```rust
pub enum RewardPolicy {
    ProportionalToImprovement { share_fraction: f64 },
    Bounty { amount: f64, min_improvement: f64 },
    CredibilityOnly,
    Custom { description: String },
}
```

### H. DataFlowEvent

Audit trail for everything crossing the node boundary:

```rust
pub struct DataFlowEvent {
    pub direction: FlowDirection,      // Outbound or Inbound
    pub event_type: FlowEventType,     // ChallengePosted, SuggestionReceived, etc.
    pub content_hash: String,          // SHA-256 of data that crossed
    pub sensitivity: Sensitivity,
}
```

---

## 3. Protocol Bridge: What nikete Actually Built vs. What We Theorized

### What We Theorized (Original §2.4)

We proposed three options and recommended a layered approach:
1. **Veracity Executor** (recommended) — new executor type handling portfolio submission
2. **Post-Completion Hook** — general hook system
3. **Two-Phase Task Pattern** — works today, no code changes

### What nikete Actually Designed

nikete evaluated three *different* integration options (from `veracity-exchange-integration.md`):

**Option A: External Service with CLI Bridge (recommended)**

```
workgraph (local)           veracity exchange (remote)
    │                              │
    ├── wg exchange publish ───────►  publish task outcomes
    ├── wg exchange suggest ───────►  submit improvement suggestions
    ├── wg exchange pull ──────────►  receive suggestions
    ├── wg exchange peers ─────────►  list trusted peers
    │                              │
    ◄── wg exchange apply ─────────  apply accepted suggestion
```

**Option B: Event Hooks** — provenance log as event stream, hooks fire on task transitions.

**Option C: Native Integration** — exchange as first-class module alongside identity system.

**nikete's recommendation:** Start with Option A. Build local features (outcome scoring, sensitivity, credibility) that are useful regardless of exchange protocol. Explicitly: "Do not build native integration (Option C) until the exchange protocol is stable and proven."

### Key Difference from Our Approach

We recommended building toward a Veracity executor (tight integration). nikete recommends keeping the exchange external and only building local foundations first. His reasoning: premature coupling to an unstable protocol is worse than CLI bridge friction. The local features (outcomes, sensitivity, credibility) have value independently.

### Concrete CLI Design (from nikete's design doc)

```bash
# Outcome scoring
wg veracity outcome <graph-root> --metric <metric> --value <value>
wg veracity outcome <graph-root> --source api:alpaca
wg veracity attribute <outcome-id> [--method ablation|shapley|eval]
wg veracity scores [--task <task-id>] [--run <run-id>]

# Challenge/suggestion exchange
wg veracity challenge <task-id> [--reward proportional:0.1]
wg veracity suggest <challenge-id> --canon <canon-file>
wg veracity test <suggestion-id>      # replay with suggestion, measure
wg veracity score <suggestion-id>     # score vs baseline

# Peer network
wg veracity peers [--topic <skill>]
wg veracity peer <peer-id>

# Data governance
wg veracity sensitivity <task-id> <public|interface|opaque|private>
wg veracity check                      # validate sensitivity constraints
wg veracity audit [--outbound] [--inbound]
```

---

## 4. The vx-adapter Branch: What's Actually There

The `nikete/vx-adapter` branch is **not a VX implementation**. It is:

### 4.1 Terminology Rename

A systematic rename across the entire codebase:

| Old term | New term | Files affected |
|----------|----------|---------------|
| `agency` | `identity` | `src/agency.rs` → `src/identity.rs`, all docs, tests |
| `evaluate` | `reward` | `src/commands/evaluate.rs` → `src/commands/reward.rs` |
| `motivation` | `objective` | `src/commands/motivation.rs` → `src/commands/objective.rs` |
| `EvaluationRef` | `RewardRef` | `identity.rs` (field `score` → `value`) |
| `PerformanceRecord` | `RewardHistory` | `identity.rs` (field `avg_score` → `mean_reward`) |
| `evaluate_loop_edges` | `reward_loop_edges` | `graph.rs` |

The rename is thorough — it extends into YAML storage directories (`.workgraph/agency/` → `.workgraph/identity/`), doc references, and internal function names.

**Assessment:** The terminology shift is considered. "Identity" is arguably more precise than "agency" (agents have an *identity*, not an *agency*). "Reward" is more precise than "evaluate" (the score is a reward signal, not a comprehensive evaluation). "Objective" is more precise than "motivation" (it defines the goal, not the internal drive). However, adopting these renames would touch every file and break all existing deployments.

### 4.2 Architecture Convergence

The vx-adapter branch independently added features we already have:

| Feature | Our implementation | nikete's (vx-adapter) | Notes |
|---------|-------------------|----------------------|-------|
| Provenance | `src/provenance.rs` (326 lines) | `src/provenance.rs` (~same) | Nearly identical: append-only JSONL, zstd rotation, same `OperationEntry` struct |
| Model registry | `src/models.rs` (414 lines) | `src/models.rs` | Similar: model catalog with cost/capability/tier |
| GC | `src/commands/gc.rs` | `src/commands/gc.rs` | Similar |

**Key insight:** Independent convergence on the same architecture validates our design decisions. Both codebases arrived at append-only JSONL with zstd rotation for provenance, content-hash identity for agents, and separate model registries.

### 4.3 Canon/Distill Removed

The vx-adapter branch deletes `src/canon.rs`, `src/commands/canon_cmd.rs`, and `src/commands/distill.rs`. The LLM-assisted knowledge distillation pipeline is abandoned.

**However:** The design doc still treats Canon as the suggestion format for the exchange. This suggests nikete sees Canon as a *data structure* (spec + tests + interaction_patterns) that's useful for exchange, even if the automated distillation pipeline to produce canons from traces was premature.

### 4.4 Research Documents Added

The branch contains extensive VX-related research:

| Document | Lines | Content |
|----------|-------|---------|
| `docs/research/veracity-exchange-deep-dive.md` | 723 | Their version of this analysis (parallel effort) |
| `docs/research/veracity-exchange-integration.md` | 489 | Integration architecture analysis with 3 options |
| `docs/research/logging-veracity-gap-analysis.md` | 222 | Gap analysis rating current system 2/5 for outcome tracking |
| `docs/research/gepa-integration.md` | 426 | GEPA prompt optimization framework integration |
| `docs/design/provenance-system.md` | 327 | Comprehensive provenance design (3 PRs, storage estimates) |
| `docs/research/nikete-fork-deep-review.md` | 497 | Self-review of the fork |

### 4.5 What Does NOT Exist on vx-adapter

- No `Outcome`, `Attribution`, `Sensitivity`, `Challenge`, `Suggestion`, `PeerCredibility` structs in code
- No `wg veracity` subcommand implementation
- No exchange protocol or network code
- No sensitivity enforcement in `build_task_context()`
- No outcome recording or attribution logic

**Bottom line:** The vx-adapter branch is design research + architecture convergence + terminology alignment. The actual VX implementation has not started.

---

## 5. Mapping to Existing Infrastructure (Updated)

nikete's design doc maps VX concepts to existing workgraph primitives:

| VX Concept | Existing Infrastructure | Gap |
|------------|------------------------|-----|
| Veracity score | `Evaluation.score` + dimensions | Scored by outcome, not LLM — need external score source |
| Suggestion format | Canon (spec + tests + patterns) | Canon removed from vx-adapter; needs reimplementation or alternative |
| A/B comparison | Replay system (snapshot, reset, re-run) | Works, but `parent_run` field is unused — needed for branching |
| Peer identity | Agent with content-hash ID + PerformanceRecord | Need cross-node identity, not just local |
| Credibility history | `RewardHistory` (evaluations list) | Same shape, but scoped to peer, not role/objective |
| Task interface | `Task.inputs` + `Task.deliverables` + `Task.verify` | Need sanitization for public posting |
| Information boundary | `Task.skills` + DAG structure | Need Sensitivity field |
| Alternative paths | `RunMeta.parent_run` | Exists but unwired |

### Key Gaps to Fill (from nikete's analysis)

1. **Real-world outcome ingestion** — external score source, not LLM-generated
2. **Attribution from outcome to sub-unit scores** — ablation/Shapley/replacement
3. **Sensitivity classification on tasks** — 4-level enum with graph constraints
4. **Sanitization pipeline** — strip private data before posting challenges
5. **Suggestion submission and tracking** — receive, test, score external suggestions
6. **Credibility accumulation with proper scoring** — Brier-scored calibration
7. **Peer registry and trust learning** — topic-specific credibility
8. **Data flow audit log** — every boundary crossing logged

---

## 6. Updated Analysis: Agent Definitions as Market Goods

### What nikete's Design Confirms

Our original §2.3 theorized that agent definitions (role + motivation) would be natural market goods. nikete's design doc confirms and extends this:

- **Peer identity = agent identity pattern.** External peers use the same content-hash model as agents. `peer_id = SHA-256(public_key)` parallels `agent_id = SHA-256(role_id + objective_id)`.
- **PeerCredibility parallels RewardHistory.** Both track score history over time. The synergy matrix (role × objective performance) could extend to peer × domain performance.
- **Lineage provides provenance of capabilities.** If an agent descended from a proven ancestor, that ancestry is verifiable evidence of quality.

### What nikete's Design Adds Beyond Our Theory

1. **Credibility gating.** Challenges can require a minimum credibility score (`required_credibility: f64`). This creates a tiered access model: open challenges for bootstrapping, credibility-gated challenges for valuable work.

2. **Topic-specific credibility.** `PeerCredibility.by_topic: HashMap<String, CredibilityScore>` means a peer trusted for feature engineering isn't automatically trusted for risk management. This is more nuanced than a single trust score.

3. **Brier-scored calibration.** A peer who says "I'm 90% confident" and improves outcomes 90% of the time has a good Brier score. This incentivizes honest confidence reporting — a proper scoring rule.

4. **Trust as earned, not assigned.** The existing `TrustLevel` enum (Verified, Provisional, Unknown) is currently set manually. In the VX model, trust is earned through demonstrated performance: `Unknown → (first accepted suggestion) → Provisional → (sustained track record) → Verified`.

### The GEPA Connection (Surprise Finding)

nikete's `gepa-integration.md` proposes using the GEPA prompt optimization framework as the inner loop inside workgraph's evolutionary system. A role description becomes a GEPA optimization target:

```python
def evaluate_role(role_description: str) -> tuple[float, dict]:
    # Deploy role in wg, run N tasks, return mean reward + diagnostics
    ...

result = optimize_anything(
    seed_candidate=current_role.description,
    evaluator=evaluate_role,
    objective="Optimize this agent role description to maximize task performance",
)
```

This replaces our single-shot `wg evolve` LLM call with GEPA's multi-iteration reflective search (Pareto frontier of role variants). Combined with veracity scores as the evaluation metric, this creates a fully grounded optimization loop: evolve roles → measure real-world outcomes → select Pareto-optimal variants.

---

## 7. Information Flow Control (Updated from Design Doc)

### nikete's Framing: "The DAG as Information Flow Controller"

Our original §2.5 identified the DAG as creating natural information boundaries. nikete's design doc goes further, framing the DAG as an explicit *security* boundary:

> The DAG structure already IS an information flow controller. Making it a *security* boundary rather than just a *convenience* boundary requires enforcing sensitivity at the context-building layer.

### Sensitivity Enforcement (nikete's concrete proposal)

Graph constraints prevent information leakage:

```
Public task → can only blocked_by tasks that are Public or Interface
Interface task → can blocked_by Public, Interface, or Opaque
Opaque task → can blocked_by anything
Private task → can blocked_by anything
```

Rationale: if a Public task depends on a Private task, completing the Public task potentially reveals information about the Private task's output.

`wg veracity check` validates these constraints and warns on violations.

### Sanitization Pipeline

When posting a challenge:
1. Strip references to Private task IDs or outputs
2. Replace specific data paths with schema descriptions
3. Remove internal identifiers (agent IDs, run IDs)
4. Optional human review before posting (`require_review_before_post: true` in config)

The sanitized challenge is content-hashed and logged in the audit trail before it leaves the node.

### Boundary Tasks

Designate certain tasks as "boundary tasks" — the interface between private and public work:

```
[private: data-prep] → [private: model-train] → [boundary: publish-predictions] → [public: evaluate-accuracy]
```

Everything upstream of a boundary task is private. The boundary task's output is what gets shared. This is a read-only graph-slicing operation for export.

---

## 8. Forking vs. Extension (Updated)

### What nikete's vx-adapter Branch Tells Us

The vx-adapter branch demonstrates that nikete is NOT forking to diverge — he's converging. The branch:
- Adds the same features we have (provenance, models, GC)
- Removes premature features (canon/distill)
- Does a terminology alignment pass (agency→identity)
- Focuses on research docs, not divergent code

### nikete's Own Classification (from `veracity-exchange-integration.md`)

**Core changes needed (~300-400 lines):**

| Change | Where | Effort |
|--------|-------|--------|
| `outcome_spec` field on Task | `graph.rs` | Small — one new optional field |
| `visibility` (sensitivity) field on Task | `graph.rs` | Small — one new field, default Private |
| `wg outcome record` command | new `commands/outcome.rs` | Medium |
| Outcome-aware reward | `identity.rs` | Medium — extend `record_reward()` |
| Outcome events in provenance | existing `provenance.rs` | Small — one new op type |

**Extensions (no core changes):**

| Extension | Why external |
|-----------|-------------|
| Exchange client | Network protocol is exchange-specific |
| Redaction layer | Read-only operation on existing data |
| Peer credibility DB | Separate trust model from internal identity |
| Suggestion-to-task converter | Uses `wg add` CLI |

**Bridge features (start external, may migrate to core):**

| Feature | Migrate when... |
|---------|-----------------|
| Credibility tracking | It informs agent assignment decisions |
| Prompt partitioning | Redaction needs enforcement, not just convention |
| Trust network | Trust scores affect task routing |

### Updated Assessment

nikete's approach is more conservative than our original roadmap. He explicitly warns against premature native integration:

> "Do not build native integration until the exchange protocol is stable and proven. Premature coupling to an unstable protocol is worse than the friction of a CLI bridge."

This is sound advice. Our original Phase 3 ("Native Integration") assumed the VX protocol was well-defined. In practice, the protocol is still being designed.

---

## 9. Implementation Roadmap (Updated from nikete's Design)

### nikete's 5-Phase Plan

**Phase 1: Outcome Scoring (~400 LOC)**
- `Outcome`, `Attribution` types
- `wg veracity outcome` command (record outcomes)
- `wg veracity attribute` command (ablation and proportional-to-eval methods)
- `wg veracity scores` command (display)
- Backfill veracity into evaluation/reward records
- Config section

**Phase 2: Challenges and Suggestions (~500 LOC)**
- `Sensitivity` field on tasks, enforcement in `wg check`
- `Challenge`, `ChallengeInterface`, `Suggestion` types
- `wg veracity sensitivity` command
- `wg veracity challenge` command (post)
- `wg veracity suggest` command (receive)
- Sanitization pipeline
- Integration with replay (test suggestion = replay with suggested canon)

**Phase 3: Credibility and Peers (~400 LOC)**
- `PeerCredibility`, `CredibilityScore`, `CredibilityEvent` types
- `wg veracity test` and `wg veracity score` commands
- Credibility accumulation with Brier scoring
- `wg veracity peers` command
- Topic-specific credibility

**Phase 4: Data Governance (~200 LOC)**
- `DataFlowEvent` types
- `wg veracity audit` command
- Automatic logging of all boundary crossings
- Review-before-post workflow

**Phase 5: Network Learning (~300 LOC, future)**
- Peer recommendation
- Trust transitivity (weak signals only)
- Auto-discovery via public challenge participation
- Bilateral exchange agreements

### Implementation Status

| Phase | Status | Notes |
|-------|--------|-------|
| Phase 1 | **Not started** | Design doc complete, no code |
| Phase 2 | **Not started** | Sensitivity enum designed but not in code |
| Phase 3 | **Not started** | PeerCredibility designed but not in code |
| Phase 4 | **Not started** | DataFlowEvent designed but not in code |
| Phase 5 | **Not started** | Future work |

### Prerequisites from nikete's Provenance Design

nikete's `docs/design/provenance-system.md` identifies 3 PRs needed before VX work:

1. **PR 1: Complete Operation Log Coverage** — instrument all 9 remaining graph-mutating commands with `provenance::record()` calls (~100 lines). *We have already done this.*

2. **PR 2: Robust Agent Archive** — archive prompt at spawn time (not just completion); archive output on dead-agent detection (~30 lines). *Partially done in our codebase.*

3. **PR 3: Operation Log Filtering CLI** — `--task`, `--actor`, `--since`, `--until`, `--op` filters on `wg log --operations` (~60 lines). *Not yet done in our codebase.*

---

## 10. Design Tradeoffs (from nikete's analysis)

nikete evaluates 6 design tradeoffs with ranked options:

### A. Attribution Method
**Recommendation:** Default to Replacement for suggestion scoring, Ablation for initial attribution, Shapley for small complementary sub-graphs.

### B. Information Boundary Granularity
**Recommendation:** Start with task-level sensitivity (B1). Add sub-graph boundary surfaces (B2) as convenience. Never rely on automatic content inference (B3).

### C. Peer Identity Model
**Recommendation:** Content-hash (like existing agents). Sybil-resistant via credibility — new identity starts at 0. Consistent with existing agent identity patterns.

### D. Market Mechanism
**Recommendation:** Start with bulletin board (post challenges, receive suggestions asynchronously). Credibility IS the price signal. Add auction for high-value private tasks later.

### E. Outcome Timing
**Recommendation:** Multi-horizon scoring. Record outcomes at multiple horizons (daily, weekly, quarterly), let queries filter. -MSE can be computed daily; Sharpe is meaningful at weekly+ horizons.

### F. Trust Transitivity
**Recommendation:** Start with no transitivity (only trust peers you've directly tested). Add weak transitivity later as an optional signal.

---

## 11. Workflow Examples (from nikete's design doc)

### Workflow 1: Score a portfolio workflow

```bash
# Record outcome after trading period
wg veracity outcome portfolio-root --metric sharpe --value 1.42 \
  --period 2025-01-01:2025-03-31 --source manual

# Attribute to sub-tasks
wg veracity attribute outcome-portfolio-root-20250401 --method ablation
#   data-ingestion:         0.15
#   feature-engineering:    0.52  ← biggest contributor
#   signal-generation:      0.38
#   portfolio-optimization: 0.28
#   execution:              0.09
```

### Workflow 2: Post challenge and receive suggestions

```bash
# Mark task as public
wg veracity sensitivity feature-engineering public

# Post challenge
wg veracity challenge feature-engineering --reward proportional:0.1 --metric sharpe
# Shares: title, sanitized description, interface, baseline score (0.52)

# Test a received suggestion
wg veracity test suggestion-abc123-peer7f3a-20250402
# Replays feature-engineering with suggested canon → runs portfolio → measures outcome

# Score after trading period
wg veracity score suggestion-abc123-peer7f3a-20250402
#   baseline sharpe:  1.42
#   with suggestion:  1.58
#   delta:           +0.16
#   peer credibility: peer7f3a now 0.73 (was 0.68)
```

### Workflow 3: Build peer network

```bash
wg veracity peers
#   PEER      CREDIBILITY  SUGGESTIONS  IMPROVEMENTS  AVG_DELTA  BRIER
#   peer7f3a  0.73         12           9             +0.08      0.18
#   peera2c1  0.61          8           5             +0.04      0.25
#   peer9d4e  0.45          6           2             -0.01      0.41

# Peer7f3a: high credibility + good calibration → invite for private work
# Peer9d4e: poorly calibrated → deprioritize
```

---

## 12. Storage Layout (from nikete's design)

```
.workgraph/
  veracity/
    outcomes/
      outcome-{id}.json              # recorded real-world outcomes
    attributions/
      attribution-{outcome_id}.json  # per-task score attributions
    challenges/
      challenge-{id}.json            # posted public challenges
    suggestions/
      suggestion-{id}.json           # received suggestions
      suggestion-{id}.canon.yaml     # the suggested canon
    peers/
      peer-{id}.json                 # PeerCredibility records
    audit/
      flow-{timestamp}.jsonl         # append-only data flow audit log
    config.toml                      # veracity-specific config
```

---

## 13. Surprises and Changes from Expectations

### Things we got right
1. **Integration is architecturally natural.** Confirmed by nikete's own mapping table.
2. **Agent definitions as market goods.** nikete explicitly uses agent identity patterns for peer identity.
3. **DAG as information flow controller.** nikete goes further, making this a security boundary.
4. **Content-hash identity is the right foundation.** Both peer and agent identity use the same pattern.
5. **Provenance log is essential.** nikete's design treats it as a prerequisite.

### Things we got wrong or incomplete
1. **We over-specified the API bridge.** We focused on `POST /run-day` as the integration point. nikete's design is protocol-agnostic — the exchange protocol is explicitly deferred.
2. **We under-specified attribution.** We mentioned Shapley values in passing. nikete has 5 attribution methods with concrete tradeoff analysis.
3. **We missed the Interface sensitivity level.** Our 3-level Visibility missed the crucial middle ground of sharing *what* without sharing *data*.
4. **We recommended native integration too early.** nikete explicitly warns against this. CLI bridge first.
5. **We didn't anticipate Canon as suggestion format.** The connection between distill/canon and the exchange system is elegant — even though canon was removed from vx-adapter as premature implementation.
6. **We didn't anticipate GEPA integration.** Using an external prompt optimization framework as the inner loop of role evolution, with veracity scores as the objective, is a novel combination we missed entirely.

### Open questions resolved
1. **Latent payoffs** → nikete proposes Pending/Provisional/Final outcome states with multi-horizon scoring and exponential decay weighting.
2. **Partial portfolio attribution** → nikete specifies 5 attribution methods with recommendations.
3. **Peer network topology** → Content-hash identity, credibility-gated access, topic-specific trust, no transitivity initially.

### Open questions remaining
1. **Transport layer.** How do challenges and suggestions actually move between nodes? Options: git-based exchange, HTTP API, Matrix protocol, dedicated P2P protocol. Deliberately unresolved.
2. **Who measures outcomes.** Self-report vs. third-party verification. System supports both, but third-party carries more credibility weight.
3. **Privacy of outcome data.** Outcome scores (portfolio P&L) may themselves be sensitive. Needs separate sensitivity treatment from prompt/output privacy.
4. **Scale of trust network.** RewardHistory works for tens of agents. Thousands of peers may need indexing/summarization.
5. **Bootstrapping the chicken-and-egg.** Trust market requires both proven public work AND enough private paid tasks to make credibility valuable.
6. **Adversarial suggestions.** A peer could submit suggestions that look good short-term but are fragile. Multi-horizon scoring and credibility decay help, but targeted defenses may be needed.

---

## 14. Relationship to Identity Reward/Evolution (from VX integration doc)

nikete's `veracity-exchange-integration.md` explicitly maps the VX system to the existing identity reward loop:

### Internal vs. External Veracity

```
Agent Performance          Task Outcome            Veracity Score
(did agent follow spec?)  (did spec work?)        (combined measure)
       │                       │                        │
  reward.score        outcome.value            veracity_score
  (0.0–1.0)              (domain metric)           (normalized)
       │                       │                        │
       └───────────┬───────────┘                        │
                   │                                    │
          composite veracity ────────────────────────────┘
```

An agent might score 0.95 on internal reward (perfectly followed spec) but 0.3 on outcome (the spec was wrong). Composite veracity distinguishes agents that execute well from agents that also produce good outcomes.

### Evolution with Outcome Data

- **Outcome-weighted evolution:** Roles whose tasks have high outcome scores are favored. High-reward/low-outcome agents (correct-but-useless) deprioritized vs. moderate-reward/high-outcome (imperfect-but-effective).
- **Outcome-informed gap analysis:** The gap-analysis evolution strategy can identify gaps between rewarded quality and real-world impact.
- **Latent payoff patience:** The evolver handles incomplete outcome data by weighting available outcome data more heavily as it arrives.

---

## 15. Conclusion (Updated)

nikete's design work is substantially more concrete and rigorous than our original analysis anticipated. The design doc defines 8 new data types, a full CLI, 4 detailed workflows, and a 5-phase ~1,800 LOC implementation plan. The vx-adapter branch shows architecture convergence (provenance, models, GC) but no VX implementation code yet.

**Key strategic implications:**

1. **The design is protocol-agnostic.** All local features (outcomes, sensitivity, attribution, credibility) are useful without any exchange network. Build these first.

2. **Canon is the exchange format** — even though it was removed from vx-adapter as premature implementation. Any exchange integration needs a structured knowledge artifact format. Canon (or something like it) will return.

3. **Attribution methods matter.** The choice between Ablation, Shapley, and Replacement has real computational and accuracy tradeoffs. Replacement is the natural method for suggestion scoring.

4. **Proper scoring rules are the foundation.** The system's incentive alignment rests on scoring against real-world outcomes. Without this, credibility and trust become gameable.

5. **The next concrete step** is implementing Phase 1 (Outcome Scoring, ~400 LOC): Outcome and Attribution types, `wg veracity outcome` and `wg veracity attribute` commands, and backfilling veracity scores into the reward system.
