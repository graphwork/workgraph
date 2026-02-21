# External Information Flows in Workgraph
## And Where VX Plugs In

**Date:** 2026-02-20
**For:** nikete
**Re:** vx-adapter branch vocabulary changes, integration architecture, and credit where due

---

## Acknowledgment

nikete's work directly pushed workgraph forward. The ideas behind trace, replay, filtered exports, distill, and the three-zone sharing model (internal/public/credentialed) shaped our implementation of `wg trace`, `wg replay`, `wg runs`, trace functions (extract/instantiate), and the organizational patterns doc's new sections on organizational memory and routines. The conversation about VX forced us to think about what workgraph looks like from the outside, which led to `--json` output auditing, `wg watch`, and the smooth integration design. These were all nikete-encouraged ideas and the system is better for them.

What follows is a response to the *vocabulary renames* on the vx-adapter branch, not a rejection of the ideas.

---

## The Short Version

VX is one instance of a generic problem: **how do external information sources plug into workgraph?** The answer is the same for VX, CI systems, monitoring dashboards, user feedback, market data, or any other external signal: workgraph has defined ingestion points at every level, and adapters translate at the boundary. No core vocabulary changes needed.

---

## 1. Generic External Information Flows

Workgraph needs to ingest external information at every level — not just evaluation scores. The pattern is the same regardless of the source:

```
External Source          Adapter            Ingestion Point         Consumer
─────────────────────    ──────────         ───────────────         ────────
VX portfolio scores  ─┐
CI test results      ─┤  translate to   ┌─ Evaluations            Evolver
User feedback        ─┤  wg formats     │  (wg evaluate record)   (reads aggregate
Analytics dashboards ─┘                 │                          performance)
                                        │
GitHub issues        ─┐                 ├─ Tasks                   Coordinator
Monitoring alerts    ─┤  create tasks   │  (wg add)               (dispatches to
Scheduled triggers   ─┘                 │                          agents)
                                        │
Peer trace exports   ─┐                 ├─ Context                 Agent prompts
Knowledge bases      ─┤  inject into    │  (wg trace import,      ({{context}},
RAG results          ─┘  agent context  │   context enrichment)    templates)
                                        │
Deployment webhooks  ─┐                 ├─ State changes           Graph
External approvals   ─┤  mutate graph   │  (wg done, wg fail,     (triggers
Pipeline completions ─┘  state          │   wg log)               downstream)
                                        │
All of the above     ───  append to  ───┤─ Operations log          wg trace,
                                        │  (automatic)             wg watch,
                                        │                          audit
                                        │
wg watch --json      ─── stream out  ───┘─ Event stream            External
                                           (to adapters)           systems
```

### The Five Ingestion Points

| Level | Command | What flows in | Who consumes it |
|-------|---------|---------------|-----------------|
| **Evaluation** | `wg evaluate record` | Scores with dimensional breakdown and `source` field | Evolver (reads performance summary) |
| **Task** | `wg add` | New work items with deps, skills, descriptions | Coordinator (dispatches to agents) |
| **Context** | `wg trace import`, context enrichment | Peer exports, knowledge artifacts, external docs | Agent prompts (injected via templates) |
| **State** | `wg done`, `wg fail`, `wg log` | Status changes, progress events | Graph (triggers unblocking, loops) |
| **Observation** | `wg watch --json` | Event stream OUT | External systems (VX, dashboards, CI) |

### The Generic Adapter Pattern

Every external system follows the same pattern:

1. **Observe** — watch workgraph events via `wg watch --json` or poll `wg list --json`
2. **Translate** — map external data into wg formats (evaluations, tasks, trace exports)
3. **Ingest** — call `wg` CLI commands to write data in
4. **React** — trigger external actions based on wg events

VX is one adapter. A CI integration is another. A Slack bot is another. They all use the same five ingestion points. The adapter translates vocabulary at the boundary — VX calls its scores "rewards," CI calls them "test results," a user calls them "feedback." Inside workgraph, they're all evaluations with a `source` field.

### What We Should Build (nikete-inspired improvements)

nikete's work highlighted real gaps in workgraph's introspection and extraction capabilities. These are genuinely valuable and we should keep building:

- **Better logging** — richer operation log entries, structured agent output capture
- **Deeper introspection** — `wg trace` is good but can show more (temporal viz, animate)
- **Function extraction** — `wg trace extract` / `wg trace instantiate` reify patterns from the log into reusable templates
- **Filtered trace export** — a sanitized, shareable view of work product for cross-boundary exchange
- **Replay with variation** — re-execute workflows with different models, data, agents
- **Three-zone visibility** — internal (full), public (sanitized), peer (richer for credentialed peers)

All of these came from or were accelerated by nikete's thinking. The disagreement is only about whether the core vocabulary should change to match one external system's terminology.

---

## 2. The Vocabulary Is Not the Problem

The `vx-adapter` branch renames:
- `agency` → `identity`
- `motivation` → `objective`
- `evaluate` → `reward`

These renames are driven by an RL mental model: agents have objectives, receive rewards, and optimize. But the system isn't RL, and the renames lose information rather than adding it.

### Agency ≠ Identity

An **agent** has an identity — that's the role × motivation pairing with its content-hash. The **agency** is the *system* of agents: the collective, the roster, the combinatorial identity space plus evaluation records plus the synergy matrix.

These sit at different abstraction levels. If you rename the system-level concept to "identity," you lose the collective noun. A system of identities is... what? The "identity system"? That's meaningless. "Agency" already does the work: it names the organizational layer that contains agents the way a bureaucracy contains bureaucrats. The agent's identity is a property of the agent. The agency is the structure they exist within.

### Evaluation ⊃ Reward

The evaluation system produces a weighted score across four dimensions — that score *is* the reward signal if you want to think in RL terms. But evaluation is richer:

- Dimensional breakdown (correctness, completeness, style, efficiency)
- Context IDs linking role and motivation performance
- Propagation to three levels (agent, role, motivation)
- Synergy analysis across the combinatorial space
- Trend indicators over time

Renaming this to "reward" is like renaming a medical examination to "temperature" because temperature is one thing the exam measures. The evaluation *includes* the scalar that RL calls a reward. It also includes the diagnostic information that makes evolution possible — which dimensions are weak, which pairings work, where the gaps are. "Reward" can't carry that.

### Motivation ≠ Objective

A motivation encodes *why* an agent acts and *how* it should behave — including acceptable and unacceptable tradeoffs. "Never skip tests" is a motivational constraint, not an objective. "Prefer correctness over speed" is a motivational stance. An objective is a target; a motivation is a reason. The system explicitly encodes tradeoff constraints that are motivational concepts, not objective-function concepts.

---

## 3. The System Is Broader Than RL

The organizational patterns document (docs/research/organizational-patterns.typ) maps the execute→evaluate→evolve loop onto:

- **Autopoiesis** — self-producing network (Maturana & Varela)
- **Double-loop learning** — questioning governing variables (Argyris & Schön)
- **Cybernetic regulation** — requisite variety (Ashby)
- **Viable System Model** — S3*/S4 intelligence (Beer)
- **Principal-agent theory** — monitoring and incentive alignment (Jensen & Meckling)
- **Stigmergy** — indirect coordination through shared medium (Grassé)

RL is one narrow lens you *could* apply to one slice (the score-drives-adaptation part). But the system also does things RL doesn't:

- Compositional identity through Cartesian products (role × motivation)
- Content-hash immutability with lineage tracking
- Synergy analysis across the combinatorial space
- Gap analysis for unmet capability needs
- Self-mutation with human oversight and budget controls

Reducing the vocabulary to RL terms (reward, objective) makes the system legible to people who only know RL, at the cost of making it illegible to organizational theorists, cyberneticians, and anyone thinking about viable systems. The vocabulary was chosen to sit at the intersection of all these frameworks, not inside any one.

---

## 4. Where VX Actually Plugs In: The Evolver

The evolver (`wg evolve`) is the component designed to consume performance signals and propose structural changes:

```
External signals (VX portfolio scores, market outcomes)
        │
        ▼
┌─────────────────────────────────────────┐
│              Performance Summary         │
│                                          │
│  Internal evaluations (4-dim scores)     │
│  + External scores (outcome-based)       │  ← VX enters here
│  + Trend indicators                      │
│  + Synergy matrix                        │
│  + Gap analysis                          │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│               Evolver                    │
│                                          │
│  Reads aggregate performance picture     │
│  Proposes: mutations, crossovers,        │
│    retirements, gap fills                │
│  Subject to: budget, human approval,     │
│    self-mutation guard                   │
└────────────────┬────────────────────────┘
                 │
                 ▼
        Modified agency definitions
        (new roles, tuned motivations,
         new agent pairings)
```

The architecture already has the slot. The evolver receives strategy-specific guidance from the evolver-skills directory. A VX integration is a new strategy — or enrichment to an existing one — where the evolver also considers external scoring data when deciding what to mutate.

### Concrete Example: VX Adapter in Action

Tuesday morning: an agent completes a portfolio construction task. The VX adapter is watching:

1. `wg watch --json` emits a task-completion event
2. The adapter pulls the realized Sharpe ratio from the portfolio data source: 0.72
3. It records: `wg evaluate record --task portfolio-q1 --source "outcome:sharpe" --score 0.72`
4. Meanwhile, the internal auto-evaluator scored the same task 0.91 (good code, clean tests)

Thursday: `wg evolve` runs. It reads the aggregate performance picture:
- Internal evaluation: 0.91 (high — the agent wrote clean code)
- Outcome evaluation: 0.72 (mediocre — the strategy didn't perform well in the market)

The evolver sees the gap. The agent is *technically competent* but the *domain strategy* is weak. It proposes a motivation mutation: add a constraint about backtesting against historical volatility regimes before committing to a strategy. Human reviews, approves.

Nothing about this required renaming anything. The `source` field on evaluations is the only thing that distinguishes "the LLM evaluator thought the code was good" from "the market said the portfolio was mediocre." The evolver reads both.

---

## 5. What the Rename Actually Broke

The rename obscured the architectural answer:

1. **Flattening evaluation to reward** makes you stop seeing the multi-level, cross-referenced diagnostic structure. You start seeing a scalar signal. And if you see a scalar signal, you can't see where VX enriches it — because you've already flattened the thing VX would enrich.

2. **Collapsing agency to identity** loses the system-level abstraction where evolution happens. The evolver operates on the agency — the collective — not on individual identities. Without the collective noun, the integration point becomes invisible.

3. **Replacing motivation with objective** loses the tradeoff constraints that make evolved agents safe. An objective says "maximize Sharpe." A motivation says "maximize Sharpe, but never take on overnight risk, and prefer strategies you can explain." The difference matters enormously when an evolver is autonomously proposing changes.

Ironically, cybernetics — your own frame — would diagnose this. Instead of increasing the system's variety to match the environment (plugging VX into the evolver), the rename *reduced* the system's variety to match one narrow subfield's vocabulary. That's Ashby's Law in reverse. Requisite poverty, not requisite variety.

---

## 6. Trace Exports Replace "Canon"

nikete's concept of a sanitized, shareable view of work product is valuable. But it doesn't need a new name or a new data structure. The trace *is* the organizational memory. Sharing parts of it is just a filtered export.

### Task Visibility

Every task gets a `visibility` field controlling what crosses organizational boundaries:

| Value | Meaning | What's shared |
|-------|---------|---------------|
| `internal` | Org-only (default) | Nothing crosses the boundary |
| `public` | Open sharing | Task description, status, structure — no agent output, no logs |
| `peer` | Credentialed sharing | Richer view for trusted peers — includes evaluations, patterns, lineage |

This is the same concept as GitHub repo visibility, OOP access modifiers, or network firewall rules. Everyone immediately understands `visibility: public`.

### Sharing = Trace Export with Visibility Filter

```
wg trace export --visibility public      # sanitized for open sharing
wg trace export --visibility peer        # richer view for trusted peers
wg trace export                          # full internal export (default)
```

The export takes the trace (operation log, task graph, evaluations) and filters it through the visibility field to produce a shareable artifact. The three zones from nikete's design map directly:

- **Internal zone** → `visibility: internal` tasks included (everything)
- **Public zone** → only `visibility: public` tasks, sanitized output
- **Credentialed zone** → `visibility: peer` tasks, with richer detail for authenticated peers

No separate "canon" command. No separate data store. The trace is the memory; visibility controls what subset of that memory crosses boundaries. The export format is the interchange format. A peer receives a trace export and imports it with `wg trace import`.

### No `wg veracity` Namespace

The proposed `wg veracity` subcommands all dissolve into existing commands:

| Proposed | Actually is | Why |
|----------|------------|-----|
| `veracity outcome` | `wg evaluate record --source "outcome:sharpe"` | It's an evaluation with a source tag |
| `veracity attribute` | Evaluation propagation (already exists) | Scores propagate to agent/role/motivation automatically |
| `veracity scores` | `wg evaluate show --source "outcome:*"` | Filter evaluations by source |
| `veracity check` | `wg check` (existing validation) | Integrity checking is generic |
| `veracity challenge` | External adapter | Peer protocol, not core wg |
| `veracity suggest` | External adapter | Peer protocol, not core wg |
| `veracity peers` | External adapter | Peer registry, not core wg |

Nothing unique remains. Every core function maps onto existing commands with the `Evaluation.source` field. The peer-to-peer protocol (challenges, suggestions, credibility tracking) is adapter-layer logic that belongs in the VX tool, not in workgraph core.

---

## 7. nikete's Code

The vx-adapter branch contains implementations (`canon.rs`, `trace.rs`, `distill.rs`) that parallel things we've already built (`wg trace`, `wg replay`, `wg trace extract`). This isn't duplication — nikete was working from the same ideas, and we were building in parallel. The right path forward is:

- **trace.rs** — our `wg trace` implementation covers this; nikete's version may have ideas worth merging into ours
- **canon.rs** — dissolves into `wg trace export --visibility <zone>` as described above
- **distill.rs** — maps onto `wg trace extract` / `wg trace instantiate` (trace functions); again, worth comparing implementations

We should compare side-by-side and merge any improvements. The implementations converge on the same concepts; the question is which code is more complete, not which vocabulary to use.

---

## 8. The Practical Proposal

Instead of renaming the core, build the VX integration as a thin adapter:

### What we'll do (in core wg):
- Add `Evaluation.source` field — `"llm"`, `"outcome:sharpe"`, `"vx:<peer-id>"`, etc.
- Add `Task.visibility` field — `internal` (default), `public`, `peer`
- Build `wg watch --json` — event stream for adapters to react to
- Build `wg trace export --visibility <zone>` — filtered, shareable trace exports
- Add serde aliases so nikete's file formats read into our types

### What the VX adapter does (external tool):
- Pulls portfolio outcomes from data sources
- Records them as evaluations with `source: "vx:outcome:<metric>"`
- The evolver consumes them alongside internal evaluations
- Handles peer exchange, credibility tracking, challenge posting
- Translates between VX protocol vocabulary and wg vocabulary at the boundary

### What doesn't change:
- The words agency, motivation, evaluation, agent, role
- The architecture
- The organizational patterns framework
- Any existing deployment

Two new fields: `Evaluation.source` and `Task.visibility`. Everything else is adapter-layer translation.

---

## 9. Summary

| Question | Answer |
|----------|--------|
| Where does VX plug in? | The evolver, via enriched evaluations — same as any external signal |
| What fields are needed? | `Evaluation.source` and `Task.visibility` (two new fields) |
| Does vocabulary need to change? | No — translate at the adapter boundary |
| What's the adapter's job? | Pull outcomes → record as evaluations → evolver consumes |
| How do peers share work? | `wg trace export --visibility <zone>` — a filtered view over existing trace data |
| Why not rename? | Loses information, obscures integration point, breaks ecosystem |
| What about nikete's code? | Parallel implementations of the same ideas — compare and merge improvements |
| Did nikete's ideas help? | Yes — trace, replay, filtered exports, function extraction, three-zone visibility all came from or were accelerated by his thinking |
| What's the generic pattern? | Five ingestion points (evaluation, task, context, state, observation) with adapters translating at boundaries |
| What should we keep building? | Better logging, deeper introspection, function extraction, trace export with visibility zones, temporal viz — all nikete-inspired |
