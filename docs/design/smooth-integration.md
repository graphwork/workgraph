# Smooth Integration Design

**Date:** 2026-02-20
**Status:** Draft for discussion with nikete
**Depends on:** [nikete-integration-roadmap.md](nikete-integration-roadmap.md), [nikete-fork-deep-review.md](../research/nikete-fork-deep-review.md), [veracity-exchange-deep-dive.md](../research/veracity-exchange-deep-dive.md)

---

## Core Principle

The question is not "how do we merge nikete's code" but **"what abstractions and interfaces make integration frictionless?"**

We want to make it as easy as possible for nikete — and any future VX system — to work with workgraph without requiring deep coupling, invasive merges, or synchronized releases. The ideal outcome: nikete can build his VX exchange client as an independent tool that talks to `wg` through stable, documented interfaces, while we continue evolving core features without breaking his tooling.

This means designing workgraph as a **platform** rather than a **monolith**. External systems should be able to observe, react to, and extend workgraph behavior through well-defined surfaces — not by forking and modifying internals.

---

## 1. Interface-First Design

### 1.1 The CLI as the Integration Surface

nikete's own recommendation is clear: **CLI bridge first, native integration deferred.** We agree. The `wg` CLI with `--json` output is the integration contract.

Currently, 47 of our commands support `--json`. This is already a broad surface. The design question is: which outputs need to be **stable**, and what's missing?

### 1.2 Stable JSON Contracts (Tier 1)

These commands are the ones VX (or any external system) would depend on. Their `--json` output schema should be treated as a public API — breaking changes require a deprecation cycle.

| Command | VX Use Case | Schema Status |
|---------|------------|---------------|
| `wg show <id> --json` | Build challenges from tasks (title, description, skills, deliverables, verify) | Exists, needs schema doc |
| `wg list --json` | Discover eligible tasks for public posting | Exists, needs schema doc |
| `wg agents --json` | Map agent identity to peer identity | Exists, needs schema doc |
| `wg evaluate show <agent-id> --json` | Read evaluation/reward history | Needs `--json` audit |
| `wg log --operations --json` | Audit trail for data flow events | Exists |
| `wg replay --plan-only --json` | Dry-run A/B comparison | Exists |
| `wg runs show <run-id> --json` | Historical outcome tracking | Exists |
| `wg status --json` | Service health and task summary | Exists |

**Action:** Document the JSON schema for each Tier 1 command in a `docs/api/` directory. Not a full OpenAPI spec — just example outputs with field descriptions and stability guarantees. A simple `wg schema <command>` that prints the JSON schema would be even better.

### 1.3 Missing Interface Points

These don't exist yet but are needed for VX integration:

| Command | Purpose | Design Notes |
|---------|---------|-------------|
| `wg veracity outcome <graph-root> --metric <m> --value <v> --json` | Record real-world outcome | New subcommand family. Returns outcome ID. |
| `wg veracity attribute <outcome-id> --method <m> --json` | Attribute outcome to sub-tasks | Returns per-task attribution scores. |
| `wg veracity scores --json` | Query per-task veracity scores | Aggregation view. |
| `wg veracity sensitivity <task-id> <level>` | Set task sensitivity | Requires new `sensitivity` field on Task. |
| `wg veracity check --json` | Validate sensitivity constraints | Graph constraint checker. |
| `wg capabilities --json` | Advertise what this workgraph instance can do | See §3.4 below. |

**Design principle:** Every `wg veracity` command should have `--json` from day one. These are integration-facing by nature.

### 1.4 Data Formats That Need to Be Stable

External systems touch these file formats directly or indirectly:

| Format | Location | Consumers | Stability |
|--------|----------|-----------|-----------|
| Graph JSONL | `.workgraph/graph.jsonl` | Federation peers, backup tools | **High** — append-only, line-per-task |
| Operations log | `.workgraph/log/operations.jsonl` | Audit tools, VX data flow events | **High** — append-only |
| Evaluation JSON | `.workgraph/agency/evaluations/*.json` | VX reward mapping | **Medium** — add fields with `serde(default)` |
| Agent YAML | `.workgraph/agency/agents/*.yaml` | Federation, peer identity | **Medium** |
| Federation config | `.workgraph/federation.yaml` | Cross-repo tools | **Low** — internal |

**Rule:** Never remove or rename fields in stable formats. New fields use `#[serde(default)]` or `#[serde(skip_serializing_if)]`. Old field names get `#[serde(alias)]` when renamed.

### 1.5 Events and Hooks

Currently workgraph has one event mechanism: the `IpcRequest::GraphChanged` notification sent over the Unix socket at `.workgraph/service.socket`. The coordinator uses this for fast scheduling after graph mutations.

For external integration, we need a richer event surface:

**Option A: `wg watch --json` (Recommended for Phase 1)**

A long-running process that tails the operations log and emits newline-delimited JSON events:

```bash
wg watch --json
{"ts":"2026-02-20T21:00:00Z","op":"task_done","task_id":"feature-eng","actor":"agent:scout","detail":{...}}
{"ts":"2026-02-20T21:00:01Z","op":"evaluation_recorded","task_id":"feature-eng","detail":{"score":0.87}}
```

This is trivially implementable — it's `tail -f` on `operations.jsonl` with optional filtering (`--op`, `--task`, `--actor`). The operations log already contains everything an external system needs to react to.

**Option B: Webhook callbacks (Phase 2, if needed)**

```toml
# .workgraph/config.toml
[hooks]
on_task_done = "curl -X POST http://localhost:8080/wg/task-done -d @-"
on_evaluation = "curl -X POST http://localhost:8080/wg/evaluation -d @-"
```

Config-driven hooks that pipe the operation JSON to an external command on specific events. Useful for VX: fire a hook when a task completes, the VX client checks if the task is public and auto-publishes an outcome.

**Recommendation:** Start with `wg watch --json`. It's zero-config, composable with Unix pipes, and sufficient for VX integration. Add config-driven hooks only if polling proves insufficient.

### 1.6 Should There Be a Plugin/Extension Architecture?

**No, not yet.** The current architecture already has the right extension seams:

- **Custom executors** (process-based, config-driven) — already exists
- **Custom evaluator/assigner/evolver agents** — already exists via `agency.evaluator_agent` config
- **Federation backends** (the `AgencyStore` trait) — already trait-based, extensible
- **CLI as API** — `wg` commands with `--json` serve as a process-level plugin interface

A formal plugin system (dynamic loading, trait objects, etc.) would add complexity without clear benefit at this stage. The CLI-as-API pattern is how nikete himself recommends integration. If we later need in-process extensibility, the `AgencyStore` trait is the natural starting point.

---

## 2. Canon as Interchange Format

### 2.1 The Problem

When workgraph instances want to share knowledge across organizational boundaries, what's the envelope? You can't send raw graph state (contains internal prompts, credentials, agent IDs). You can't send just task titles (too lossy). You need a **sanitized, structured view** of what was done, how it worked, and what was learned.

nikete's `Canon` concept is exactly this: a materialized view of work product, designed for sharing.

### 2.2 Three Zones of Sharing

| Zone | Contents | Audience | Use Case |
|------|----------|----------|----------|
| **Internal** (full log) | Complete graph, operations log, agent prompts, raw outputs, credentials | Internal team only | Debugging, audit, compliance |
| **Public** (sanitized) | Task structure, interfaces, verification criteria, aggregate scores — no data, no prompts | Anyone | Challenge postings, capability advertising, open benchmarks |
| **Credentialed** (richer for verified peers) | Public + redacted prompts, interaction patterns, quality signals, detailed scores | Peers with sufficient credibility | Suggestion exchange, collaborative improvement |

### 2.3 Canon Schema

Building on nikete's Canon struct and extending it for interchange:

```yaml
# canon-feature-engineering-v3.yaml
schema_version: "1.0"
kind: canon

# Identity
task_id: feature-engineering
title: "Feature Engineering for Alpha Signal"
version: 3
created_at: "2026-02-20T21:00:00Z"
created_by: "agent:scout"   # or "peer:<hash>" for external

# What was done (always shareable)
spec: |
  Build features from market microstructure data for alpha signal generation.
  Input: raw tick data (schema: {timestamp, price, volume, side})
  Output: feature matrix (schema: {timestamp, feature_1..feature_n, target})
deliverables:
  - "features/*.parquet"
  - "feature_importance.json"
verify: "python verify_features.py --check-schema --check-coverage"

# Interface definition (shareable at Interface sensitivity or above)
interface:
  inputs:
    - name: raw_data_path
      type: path
      schema: "tick_data_v2"
    - name: lookback_days
      type: integer
      default: 90
  outputs:
    - name: feature_matrix
      type: path
      schema: "feature_matrix_v1"
  skills_required: ["python", "pandas", "market-microstructure"]

# Quality signals (shareable at Credentialed level)
quality_signals:
  evaluation_score: 0.87
  outcome_scores:
    sharpe_contribution: 0.52
    attribution_method: ablation
  convergence: true
  iterations: 3

# Interaction patterns (shareable at Credentialed level)
interaction_patterns:
  common_errors:
    - "Feature leakage from future data — always use point-in-time joins"
  effective_approaches:
    - "Rolling window features outperformed expanding window by 15%"
  sticking_points:
    - "Volume features are noisy below 1-minute resolution"

# Provenance chain (always included, sanitized per zone)
provenance:
  source_graph: "sha256:abc123..."    # content hash of originating graph
  lineage: ["canon-data-prep-v2"]     # parent canons this built on
  outcome_verified: true               # backed by real-world outcome data
```

### 2.4 Export and Import Commands

```bash
# Export a canon from a completed task or subgraph
wg canon export <task-id-or-subgraph-root> \
  --zone public|credentialed|internal \
  --output feature-engineering.canon.yaml

# Export with automatic sensitivity enforcement
wg canon export portfolio-root --zone public
# → Only includes tasks with sensitivity=Public or sensitivity=Interface
# → Strips internal task IDs, agent IDs, raw prompts
# → Hashes data references instead of including data

# Import a canon (from a peer suggestion, or from another workgraph)
wg canon import suggestion-from-peer7f3a.canon.yaml
# → Creates a task (or task subgraph) pre-populated with canon context
# → Sets source: "canon:peer7f3a:sha256:def456"
# → Injected into agent prompt via {{task_canon}} template variable

# List available canons
wg canon list --json
```

### 2.5 Relationship to Trace Functions

Canons and trace functions serve different but complementary purposes:

| | Canon | Trace Function |
|---|-------|---------------|
| **Contains** | Knowledge — what worked, what didn't, quality signals | Structure — task graph pattern with typed parameters |
| **Granularity** | Single task or small subgraph | Arbitrary subgraph pattern |
| **Use case** | Enrich agent context for re-execution | Create new task subgraphs from templates |
| **Sharing** | Cross-organizational (VX exchange) | Cross-project (internal reuse) |
| **Injected via** | `{{task_canon}}` in agent prompts | `wg instantiate` creates tasks |

A canon can reference a trace function ("this knowledge was generated by applying pattern X"), and a trace function can bundle canons ("when instantiating this pattern, inject these canons into the generated tasks"). They compose naturally.

### 2.6 Canon as VX Suggestion Format

nikete's design treats Suggestions as Canons. This is elegant because:

1. The canon schema already contains everything a suggestion needs (spec, interface, quality signals)
2. Canons are versioned — suggestion iteration is natural
3. The content hash provides tamper-evidence for credibility tracking
4. `wg canon import` + `wg replay` = test a suggestion against your baseline

```bash
# VX workflow: receive suggestion, test it, score it
wg canon import suggestion-peer7f3a.canon.yaml    # creates task with canon context
wg replay --subgraph feature-engineering           # re-run with suggestion applied
# ... wait for outcome period ...
wg veracity score suggestion-abc123                # compare against baseline
```

---

## 3. Adapter Pattern

### 3.1 Design Philosophy: Thin Adapter, Fat CLI

The VX exchange client should be a **thin adapter** that translates between VX protocol concepts and `wg` CLI calls. It should NOT duplicate workgraph logic. The adapter's job:

1. **Translate**: VX protocol messages ↔ `wg` CLI invocations
2. **Filter**: Apply sensitivity rules before data leaves the node
3. **Track**: Maintain peer credibility state (can be local to the adapter)
4. **Transport**: Handle network communication (HTTP, git, P2P — whatever VX uses)

```
┌─────────────────────────────────────────────────┐
│                    VX Adapter                     │
│                                                   │
│  ┌───────────┐  ┌──────────┐  ┌───────────────┐ │
│  │ VX Client │  │Sanitizer │  │ Credibility DB│ │
│  │ (network) │  │(redactor)│  │ (peer scores) │ │
│  └─────┬─────┘  └────┬─────┘  └───────┬───────┘ │
│        │              │                │          │
│        └──────────────┼────────────────┘          │
│                       │                           │
└───────────────────────┼───────────────────────────┘
                        │  calls wg CLI with --json
                        ▼
┌───────────────────────────────────────────────────┐
│                   workgraph (wg)                   │
│                                                    │
│  graph · agency · provenance · federation · runs   │
└────────────────────────────────────────────────────┘
```

### 3.2 Complete CLI API Surface for VX

The adapter needs these `wg` commands. All exist or are planned:

**Reading state (all have --json):**

| Command | VX Use | Exists? |
|---------|--------|---------|
| `wg show <id> --json` | Read task definition for challenge creation | Yes |
| `wg list --json --status done` | Find completed tasks for outcome recording | Yes |
| `wg evaluate show <agent> --json` | Map evaluations to reward history | Yes |
| `wg log --operations --json` | Audit trail, data flow events | Yes |
| `wg runs show <run> --json` | Historical outcomes, A/B comparison | Yes |
| `wg agents --json` | Agent identity for peer mapping | Yes |
| `wg trajectory <agent> --json` | Agent performance over time | Yes |
| `wg status --json` | Health check before exchange operations | Yes |

**Writing state:**

| Command | VX Use | Exists? |
|---------|--------|---------|
| `wg canon export <id> --zone <z>` | Create challenge content | Planned |
| `wg canon import <file>` | Apply received suggestion | Planned |
| `wg veracity outcome <root> --metric <m> --value <v>` | Record real-world outcome | Planned |
| `wg veracity attribute <outcome-id> --method <m>` | Attribute to sub-tasks | Planned |
| `wg veracity sensitivity <id> <level>` | Tag tasks for sharing | Planned |
| `wg replay --subgraph <id>` | Test suggestion against baseline | Yes |

**Monitoring:**

| Command | VX Use | Exists? |
|---------|--------|---------|
| `wg watch --json` | React to task completions, evaluations | Planned |
| `wg veracity check --json` | Validate before publishing | Planned |
| `wg capabilities --json` | Discover what this instance supports | Planned |

### 3.3 Event Stream: `wg watch --json`

The adapter needs to react to workgraph events without polling. `wg watch` tails the operations log:

```bash
# Stream all events
wg watch --json

# Filter to events VX cares about
wg watch --json --op task_done,evaluation_recorded,outcome_recorded

# Since a timestamp (for reconnection)
wg watch --json --since 2026-02-20T21:00:00Z
```

Output is newline-delimited JSON (NDJSON), one event per line:

```json
{"ts":"...","op":"task_done","task_id":"feature-eng","actor":"agent:scout","detail":{"status":"done","duration_s":142}}
{"ts":"...","op":"evaluation_recorded","task_id":"feature-eng","detail":{"score":0.87,"agent":"scout","source":"llm"}}
```

The `--since` flag enables reconnection after adapter restart — no lost events as long as the operations log hasn't been rotated past the requested timestamp.

### 3.4 Capability Discovery: `wg capabilities --json`

An external tool (VX adapter, federation peer, or any integration) can ask "what can this workgraph do?"

```json
{
  "version": "0.9.0",
  "features": {
    "agency": true,
    "federation": true,
    "trace_functions": true,
    "veracity": false,
    "canon": false,
    "watch": true
  },
  "json_commands": ["show", "list", "status", "agents", "evaluate", "..."],
  "executor_types": ["claude", "shell", "amplifier"],
  "skills": ["rust", "python", "analysis", "..."],
  "federation": {
    "remotes": 2,
    "peers": 1
  }
}
```

This lets the VX adapter gracefully degrade: if `veracity` isn't available, fall back to using `evaluate` scores directly. If `canon` isn't available, export raw task definitions instead.

---

## 4. Naming Convergence

### 4.1 Principle: Don't Rename What Works

Both codebases are in production use. Wholesale renames break deployments, confuse users, and create merge hell. The goal is a **bridging vocabulary** that lets both sides understand each other without either side renaming their internals.

### 4.2 Bridging Vocabulary

| Concept | wg term (keep) | nikete/VX term | Bridge | Implementation |
|---------|---------------|----------------|--------|----------------|
| Agent system | `agency` | `identity` | Both valid. Use "agency" in wg, "identity" in VX contexts | No code change |
| Agent drive | `motivation` | `objective` | Both valid. Use "motivation" in wg, "objective" in VX contexts | No code change |
| Performance score | `evaluation` / `score` | `reward` / `value` | `evaluation` internally, `reward` in VX interface | `#[serde(alias = "value")]` on score field |
| Score source | *(missing)* | `source` | **Adopt `source`** — this is new, not a rename | Add `Evaluation.source` field |
| Conversation capture | `trace` (provenance) | `trace` (conversation) | `trace` = provenance ops; `conversation` or `session` = raw LLM dialog | Naming convention in docs |
| Knowledge artifact | *(missing)* | `canon` | **Adopt `canon`** — distinctive, well-defined by nikete's design | New module when implemented |
| Workflow template | `trace function` | *(doesn't exist)* | Keep `trace function` | No conflict |
| Sensitivity | *(missing)* | `sensitivity` (4-level) | **Adopt `sensitivity`** — better than our proposed 3-level `visibility` | New field when implemented |

### 4.3 Serde Aliases for Format Portability

Add these aliases so YAML/JSON files from nikete's fork deserialize correctly in ours:

```rust
// In Evaluation struct
#[serde(alias = "value")]
pub score: f64,

#[serde(default = "default_eval_source")]
pub source: String,  // "llm", "outcome:<metric>", "manual", "vx:<peer-id>"

// On PerformanceRecord
#[serde(alias = "mean_reward")]
pub avg_score: Option<f64>,
```

This is a one-way bridge: their files work in our system. For full bidirectional portability, nikete would add the reverse aliases. Since both sides use `#[serde(default)]` for new fields, old files continue to work in both directions.

### 4.4 VX-Facing Vocabulary

When we build VX-specific commands (`wg veracity`), use nikete's vocabulary in that namespace:

- `wg veracity outcome` (not "evaluation" — outcomes are external, evaluations are internal)
- `wg veracity scores` (veracity scores, distinct from evaluation scores)
- `wg veracity sensitivity` (nikete's term, adopted)
- `wg veracity challenge` / `suggest` (nikete's exchange terminology)

This keeps the internal/external distinction clear: `wg evaluate` = internal LLM judgment, `wg veracity` = external ground-truth measurement.

---

## 5. Extension Points

### 5.1 Current Extension Points (Already Working)

| Extension Point | Mechanism | How External Systems Use It |
|----------------|-----------|---------------------------|
| **Custom executors** | `.workgraph/executors/{name}.toml` — process-based, template variables (`{{task_id}}`, `{{task_identity}}`, etc.) | VX could define a `vx-executor.toml` that wraps the default executor with outcome reporting |
| **Custom evaluator agents** | `agency.evaluator_agent` in config — an agent that produces evaluations | VX could plug in an evaluator that queries real-world outcome data instead of (or alongside) LLM judgment |
| **Custom assigner agents** | `agency.assigner_agent` in config | VX could factor in peer credibility when assigning agents to tasks |
| **Custom evolver agents** | `agency.evolver_agent` in config | VX could use GEPA-based evolution with outcome data as the fitness function |
| **Federation remotes** | `AgencyStore` trait in `federation.rs` | New backend implementations (HTTP, git-hosted) for remote agency stores |
| **CLI as API** | `wg <cmd> --json` | Any external tool can read/write workgraph state |

### 5.2 New Extension Points Needed

| Extension Point | Purpose | Design |
|----------------|---------|--------|
| **Event hooks** | React to graph mutations without polling | `wg watch --json` (Phase 1), config-driven hooks (Phase 2) |
| **Custom evaluators (source-aware)** | Record evaluations from non-LLM sources | Add `source` field to `Evaluation`; `record_evaluation()` accepts source param |
| **Canon generators** | Produce canons from completed tasks | `wg canon export` with pluggable redaction/sanitization |
| **Canon consumers** | Inject canon context into task execution | `{{task_canon}}` template variable in executor prompts |
| **Sensitivity enforcement** | Prevent information leakage across zones | `sensitivity` field on Task; `wg veracity check` validates DAG constraints |

### 5.3 What Should Stay in Core vs. External

**In core (part of `wg` binary):**
- `Evaluation.source` field — tiny change, enables everything
- `sensitivity` field on Task — foundational for all sharing
- `wg watch --json` — simple tail of existing log
- `wg canon export/import` — needs access to graph internals for sanitization
- `wg capabilities --json` — self-description

**External (separate tool or adapter):**
- VX exchange client (network protocol, peer discovery)
- Peer credibility database (separate from agency evaluations)
- Challenge marketplace UI
- Outcome data connectors (Alpaca, Databento API clients)
- GEPA integration for role evolution

**Migrate to core later if warranted:**
- Credibility-aware agent assignment (when trust scores inform task routing)
- Sensitivity enforcement in `build_task_context()` (when redaction needs enforcement, not convention)
- Outcome-weighted evolution (when veracity scores are reliable enough)

### 5.4 The `wg veracity` Subcommand Namespace

Rather than scattering VX-related commands across the CLI, group them under `wg veracity`:

```
wg veracity
├── outcome <root> --metric <m> --value <v>    # Record outcome
├── attribute <outcome-id> --method <m>         # Attribute to tasks
├── scores [--task <id>] [--json]               # View veracity scores
├── sensitivity <task-id> <level>               # Set task sensitivity
├── check [--json]                              # Validate constraints
├── challenge <task-id> [--reward ...]          # Post challenge
├── suggest <challenge-id> --canon <file>       # Submit suggestion
├── test <suggestion-id>                        # Replay with suggestion
├── score <suggestion-id>                       # Score vs baseline
├── peers [--topic <skill>] [--json]            # List peers
└── audit [--outbound] [--inbound] [--json]     # Data flow audit
```

This entire namespace can be feature-gated (`#[cfg(feature = "veracity")]`) so it's opt-in for builds that don't need it.

---

## 6. Migration Path

### 6.1 For Someone Running nikete's Fork

**Scenario:** A user is running `nikete/main` or `nikete/vx-adapter` and wants to switch to upstream + VX adapter.

**Step 1: Data compatibility check**

```bash
# From nikete's fork directory
wg list --json > /tmp/nikete-tasks.json
ls .workgraph/agency/  # Check for identity/ vs agency/ directory naming
```

**Step 2: Graph migration (zero data loss)**

The graph format (`.workgraph/graph.jsonl`) is compatible between forks. Both use the same `Task` struct with the same core fields. Copy it directly:

```bash
cp -r .workgraph/graph.jsonl /path/to/upstream-workgraph/.workgraph/
```

**Step 3: Agency migration**

If running `nikete/vx-adapter` (which renamed `agency/` → `identity/`):

```bash
# The directory structure is the same, just renamed
cp -r .workgraph/identity/roles/ /path/to/upstream/.workgraph/agency/roles/
cp -r .workgraph/identity/objectives/ /path/to/upstream/.workgraph/agency/motivations/
cp -r .workgraph/identity/agents/ /path/to/upstream/.workgraph/agency/agents/
```

Agent YAML files use `#[serde(alias)]`, so `objective_id` deserializes as `motivation_id`, and `reward` records deserialize as `evaluation` records. **No file editing required.**

If running `nikete/main` (same directory names as upstream): direct copy, no transformation needed.

**Step 4: Evaluation/Reward migration**

```bash
# Copy evaluation files — serde aliases handle field name differences
cp -r .workgraph/identity/evaluations/ /path/to/upstream/.workgraph/agency/evaluations/
# Or from nikete/main:
cp -r .workgraph/agency/evaluations/ /path/to/upstream/.workgraph/agency/evaluations/
```

Fields like `value` → `score` and `source` (new) are handled by `#[serde(alias)]` and `#[serde(default)]`.

**Step 5: Trace/Canon migration**

- nikete/main traces (`.workgraph/traces/`): These are conversation-level traces in a format we don't consume yet. Preserve them — once we port `parse_stream_json()`, they'll be readable.
- nikete/main canons (`.workgraph/canons/`): Preserve. When we implement `wg canon import`, these become usable.
- vx-adapter provenance (`.workgraph/log/operations.jsonl`): Same format as ours. Direct copy.

**Step 6: Config migration**

nikete's config has `[distill]` and `[replay]` sections we don't recognize. Our config has `[log]`, `[agency]`, and more sections he doesn't have. **Both systems ignore unknown config sections**, so you can merge the TOML files:

```bash
# Merge configs — unknown sections are silently ignored by both sides
cat nikete/.workgraph/config.toml upstream/.workgraph/config.toml > merged-config.toml
# Review and deduplicate
```

**Step 7: Install VX adapter (when available)**

```bash
# The VX adapter is a separate tool, not a wg fork
cargo install wg-vx-adapter  # hypothetical
wg-vx config set workgraph_dir /path/to/upstream/.workgraph
```

### 6.2 Migration Guarantees

| Data | Migration | Loss Risk |
|------|-----------|-----------|
| Task graph | Direct file copy | Zero — same format |
| Roles/motivations | Direct copy (directory rename if vx-adapter) | Zero — same YAML format |
| Agents | Direct copy + serde alias handling | Zero |
| Evaluations/rewards | Direct copy + serde alias handling | Zero — `source` defaults to "llm" |
| Traces (conversation) | Preserve; readable after parser port | Zero (deferred utility) |
| Canons | Preserve; usable after canon import | Zero (deferred utility) |
| Provenance log | Direct copy (if from vx-adapter) | Zero — same format |
| Run snapshots | Direct copy | Zero — same format |
| Config | Manual merge of TOML sections | Low — review needed |

---

## 7. Implementation Sequence

The changes above should be implemented in this order, each building on the last:

### Phase 0: Foundation (This Week, ~100 LOC)

1. **Add `Evaluation.source` field** with `#[serde(default)]` — enables non-LLM evaluation sources
2. **Add serde aliases** (`value`↔`score`, `mean_reward`↔`avg_score`) — enables file portability
3. **Document Tier 1 JSON schemas** — create `docs/api/` with example outputs

### Phase 1: Observability (This Month, ~200 LOC)

4. **`wg watch --json`** — tail operations log with filtering and `--since`
5. **`wg capabilities --json`** — self-description for external tools
6. **Port `parse_stream_json()` conversation parser** — new `src/trace_parser.rs`

### Phase 2: Canon System (When Needed, ~400 LOC)

7. **Canon struct and YAML schema** — the interchange format
8. **`wg canon export`** with zone-based sanitization
9. **`wg canon import`** with `{{task_canon}}` template injection
10. **Sensitivity field on Task** with `wg veracity sensitivity` command

### Phase 3: Outcome Scoring (When VX Work Begins, ~400 LOC)

11. **`wg veracity outcome`** — record real-world outcomes
12. **`wg veracity attribute`** — attribute to sub-tasks
13. **`wg veracity scores`** — query and display
14. **`wg veracity check`** — validate sensitivity DAG constraints

### Phase 4: Exchange (When VX Protocol Stabilizes)

15. **VX adapter as separate tool** — network, credibility, challenges
16. **Config-driven event hooks** — if `wg watch` proves insufficient
17. **Credibility-aware assignment** — migrate to core when proven

---

## 8. What We're Proposing to nikete

If we share this document with nikete, here's the implicit offer:

1. **We'll stabilize the CLI --json interface** so his tools can depend on it.
2. **We'll add `Evaluation.source`** so his outcome-based scoring works with our system.
3. **We'll adopt his canon schema** (with extensions) as the interchange format.
4. **We'll adopt his sensitivity enum** when we build sharing features.
5. **We'll build `wg watch --json`** so his adapter can react to events.
6. **We won't force renames** — serde aliases handle the vocabulary difference.
7. **VX stays external** — we provide the platform, he builds the exchange.

In return, we'd ask:
1. **Add reverse serde aliases** in his code so our files work there too.
2. **Converge on canon schema** — one format both sides produce and consume.
3. **Test against our `--json` output** — we'll maintain schema stability if he reports breakage.
4. **Share VX protocol design** early — so we can prepare the right extension points.

This is a **collaboration through interfaces**, not a merge. Both codebases continue to evolve independently, connected by stable data formats and CLI contracts.

---

## Appendix A: Comparison with Alternative Approaches

### Why Not: Merge the Forks

104 files diverged, 100K+ lines diff. Merge conflicts in every shared file. Both sides would spend weeks resolving conflicts instead of building features. The forks solve different problems at different time horizons — forcing them into one codebase creates governance overhead without functional benefit.

### Why Not: Native Rust Integration (Shared Crate)

A `workgraph-core` crate with shared types sounds clean but creates tight coupling. Both sides would need to coordinate releases, agree on type definitions, and handle version skew. The CLI-as-API approach gives the same integration surface with zero coordination overhead. If convergent evolution continues (provenance, models, GC), a shared crate becomes more natural — but don't force it.

### Why Not: gRPC/REST API Server

Adds deployment complexity (running a server), authentication concerns, and network latency for local operations. The CLI does everything a local API server would do, with better composability (pipes, scripts) and zero infrastructure. If workgraph eventually needs remote access (multi-machine deployment), a server makes sense — but that's a different problem from VX integration.

### Why Not: Full Plugin System

Dynamic loading, trait objects, plugin discovery — all engineering investment that's premature when we have exactly one integration target (VX). The current extension points (executors, evaluator agents, federation trait, CLI) handle the known use cases. Build a plugin system when there are three or more external systems that need different extension mechanisms.
