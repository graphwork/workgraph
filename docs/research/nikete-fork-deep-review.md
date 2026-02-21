# Deep Code Review: nikete/workgraph Fork

**Reviewer:** scout (analyst)
**Date:** 2026-02-20 (updated)
**Original review:** 2026-02-19 (based on code recovered from cached agent logs — repo was 404)
**This revision:** Based on actual source code via git remote `nikete` with branches `nikete/main` and `nikete/vx-adapter`
**Fork:** https://github.com/nikete/workgraph
**Companion report:** [nikete-fork-comparison-feb20.md](nikete-fork-comparison-feb20.md) — structural diff, feature table, merge feasibility

---

## 1. Full Inventory (Verified)

The previous review was reconstructed from agent output logs when the repo was 404. With direct access to the remote, here is the verified inventory.

### New Files in nikete/main (vs origin/main)

| File | Lines | Purpose | Verified |
|------|-------|---------|----------|
| `src/trace.rs` | 815 | Core trace module: `TraceEvent` enum, stream-json parser, JSONL I/O, metadata computation, filtering, extraction. 13 tests | Yes — matches prior review |
| `src/canon.rs` | 627 | Canon (distilled knowledge): `Canon` struct with spec/tests/interaction_patterns/quality_signals, versioned YAML, prompt rendering, distill prompt builder. 14 tests | Yes |
| `src/runs.rs` | 699 | Run management: snapshots, run ID generation, task reset logic (selective, with keep-done threshold), graph restore. 16 tests | Yes (was 698 in prior review — off by 1, likely trailing newline) |
| `src/commands/trace_cmd.rs` | ~212 | CLI for `wg trace-extract <agent-id>` and `wg trace <task-id>` | Yes |
| `src/commands/distill.rs` | ~231 | CLI for `wg distill <task-id>`. Builds distill prompt. LLM call not wired up | Yes |
| `src/commands/canon_cmd.rs` | ~201 | CLI for `wg canon <task-id>` (view) and `wg canon --list` | Yes |
| `src/commands/replay.rs` | 392 | CLI for `wg replay` with --failed-only, --below-score, --tasks, --keep-done, --plan-only | Yes |
| `src/commands/runs_cmd.rs` | ~195 | CLI for `wg runs list/show/restore` | Yes |
| `FORK.md` | 162 | Fork documentation | Yes |
| `docs/design-replay-system.md` | 554 | Design doc with 6 tradeoff analyses | Yes |
| `docs/design-veracity-exchange.md` | ~737 | VX design — **now recoverable** from vx-adapter branch research docs | See §3 |

### New Files in nikete/vx-adapter (vs nikete/main)

| File | Lines | Purpose |
|------|-------|---------|
| `src/provenance.rs` | 326 | Append-only JSONL operation log with zstd rotation — nearly identical design to our `provenance.rs` |
| `src/models.rs` | 414 | Model registry with tier/cost metadata — nearly identical to our `models.rs` |
| `src/identity.rs` | 3,637 | Renamed from `agency.rs` — agency→identity, motivation→objective, evaluation→reward |
| `src/commands/gc.rs` | 415 | Garbage collection — functionally identical to our `gc.rs` |
| `src/commands/models.rs` | 138 | CLI for `wg models list/add/set-default` |
| `src/commands/trace.rs` | 705 | New trace command built on provenance (replaces `trace_cmd.rs`) |
| `docs/design/provenance-system.md` | 327 | Comprehensive provenance design doc |
| `docs/research/veracity-exchange-deep-dive.md` | 723 | VX concept mapping and integration analysis |
| `docs/research/veracity-exchange-integration.md` | 489 | Gap analysis for VX integration |
| `docs/research/gepa-integration.md` | 426 | Gepa system integration analysis |
| `docs/research/logging-veracity-gap-analysis.md` | 222 | Logging coverage vs VX requirements |
| `docs/research/organizational-economics-review.md` | 765 | Organizational economics applied to workgraph |
| `docs/research/collaborators-and-perspectives.md` | 356 | External collaborator analysis |
| `docs/research/file-locking-audit.md` | 327 | File locking safety audit |
| `docs/IDENTITY.md` | — | Rewritten from AGENCY.md |
| `docs/LOGGING.md` | 174 | Logging system documentation |
| `tests/integration_cli_workflows.rs` | 611 | CLI workflow integration tests |
| `tests/integration_logging.rs` | 894 | Provenance logging tests |
| `tests/integration_trace_replay.rs` | 672 | End-to-end trace/replay tests |

### Files Removed in vx-adapter (vs nikete/main)

| File | Lines | Significance |
|------|-------|-------------|
| `src/canon.rs` | 627 | Canon/distill system abandoned |
| `src/trace.rs` | 815 | Stream-json parser removed, replaced by provenance-based trace |
| `src/commands/canon_cmd.rs` | ~201 | Removed with canon |
| `src/commands/distill.rs` | ~231 | Removed with canon |
| `src/commands/trace_cmd.rs` | ~212 | Replaced by new `trace.rs` |
| `docs/design-replay-system.md` | 554 | Replaced by provenance design |
| `docs/design-veracity-exchange.md` | ~737 | Superseded by research docs in `docs/research/` |

### Modified Files (both forks diverged)

| File | Nature of divergence |
|------|---------------------|
| `src/agency.rs` | Both ~3,600-3,800 lines. Functionally equivalent. Ours has ~165 more lines (federation-related). vx-adapter renames to `identity.rs`. |
| `src/config.rs` | Nikete/main: +91 lines (DistillConfig, ReplayConfig). Ours: significantly larger (1,038 lines) with log, models, and more sections. |
| `src/runs.rs` | Nikete/main: 699 lines (richer snapshots). Ours: 230 lines (focused). vx-adapter: simplified further. |
| `src/commands/replay.rs` | Nikete/main: 392 lines. Ours: 589 lines (richer). vx-adapter: 594 lines (ReplayOptions struct, reward terminology). |
| `src/commands/viz.rs` | Nikete: 1,659 lines. Ours: 2,418 lines (added 2D force-directed layout). |
| `src/main.rs` | Major structural differences — different CLI subcommand sets. |

---

## 2. Struct Definitions (Verified Against Actual Code)

All struct definitions from the prior review have been verified against `nikete/main` source. The prior cache-based recovery was accurate — no corrections needed.

### TraceEvent (src/trace.rs)

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceEvent {
    System { content: String, ts: String },
    Assistant { content: String, tool_calls: Vec<ToolCall>, ts: String },
    ToolResult { call_id: String, tool: String, result: String, truncated: bool, ts: String },
    User { content: String, source: Option<String>, ts: String },
    Error { content: String, recoverable: bool, ts: String },
    Outcome { status: String, exit_code: i32, duration_s: f64, artifacts_produced: Vec<String>, ts: String },
}
```

### Canon (src/canon.rs)

```rust
pub struct Canon {
    pub task_id: String,
    pub version: u32,
    pub distilled_from: Vec<DistillSource>,
    pub distilled_by: String,
    pub distilled_at: String,
    pub spec: String,
    pub tests: String,
    pub interaction_patterns: InteractionPatterns,
    pub quality_signals: QualitySignals,
}
```

### vx-adapter: Reward (src/identity.rs) — replaces Evaluation

```rust
pub struct Reward {
    pub task_id: String,
    pub agent_id: String,
    pub value: f64,                    // was "score", with #[serde(alias = "score")]
    pub dimensions: BTreeMap<String, f64>,
    pub reasoning: String,
    pub evaluated_by: String,          // unchanged name
    pub timestamp: String,
    #[serde(default = "default_reward_source")]
    pub source: String,                // NEW: "llm", "outcome:<metric>", "manual", etc.
}
```

The `source` field is the sole substantive addition in the identity rename — it enables external reward signals (specifically Veracity Exchange portfolio P&L) alongside LLM-generated evaluations.

---

## 3. The VX Design Document — Now Recoverable

The prior review flagged `docs/design-veracity-exchange.md` as "UNRECOVERABLE" since the repo was 404. With the vx-adapter branch accessible, the VX research is now available across multiple documents:

| Document | Lines | Key Content |
|----------|-------|-------------|
| `docs/research/veracity-exchange-deep-dive.md` | 723 | Detailed concept mapping: task → portfolio position, artifacts → position entries, reward score → internal quality metric, VX run-day score → external P&L/MSE metric. Analyzes 7 research questions. |
| `docs/research/veracity-exchange-integration.md` | 489 | Gap analysis: what workgraph already supports (provenance, rewards, content-hash identity, lineage) vs what's missing for VX. |
| `docs/research/gepa-integration.md` | 426 | Integration with the Gepa system. |
| `docs/research/logging-veracity-gap-analysis.md` | 222 | Provenance coverage requirements for VX compliance. |

### The OrchestratorAdapter Trait

The prior review noted this was "NOT FOUND in recovered source code" and speculated about its interface. With the actual code available: **there is no `OrchestratorAdapter` trait in either branch.** The VX integration surface is minimal — a single `Reward.source` field that allows external systems to contribute reward signals. The research docs describe the conceptual mapping but propose no trait interface. The prior review's speculation about trait methods was reasonable but unnecessary.

### Key VX Concepts (from actual docs)

1. **Outcome-based scoring:** Task evaluation scores map to VX "internal quality metrics." Real-world outcomes (portfolio P&L, prediction MSE) become VX "external quality metrics." The `Reward.source` field distinguishes these.
2. **Peer exchange:** Public (non-sensitive) portions of task specs and agent prompts can be shared on a VX marketplace where others suggest improvements. Good suggestions build credibility.
3. **Credibility accumulation:** Agent lineage + reward history maps to VX trust scores. The content-hash identity system provides the provenance chain.
4. **Implementation approach:** Incremental — add `source` to rewards first, then optional VX reporting, then full peer exchange. No big-bang rewrite.

---

## 4. Architecture Comparison: Two Evolutionary Paths

### nikete/main: Capture → Distill → Replay

```
CAPTURE → DISTILL → REPLAY
(traces)   (canons)   (runs)
```

The original three-stage pipeline. Captures raw LLM conversation, distills knowledge via LLM, replays with enriched context. The distill LLM call was never implemented.

### nikete/vx-adapter: Provenance → Reward → Replay

```
PROVENANCE → REWARD → REPLAY
(operations)  (external)  (runs)
```

The vx-adapter abandoned the distill pipeline. Instead:
- **Provenance** replaces conversation-level traces with operation-level logging (like our design)
- **Reward** replaces LLM-only evaluation with pluggable reward sources (outcome metrics, manual, VX)
- **Replay** retains snapshot-and-reset but drops canon injection

### Our fork: Provenance → Evaluate → Replay + Trace Functions + Federation

```
PROVENANCE → EVALUATE → REPLAY
(operations)  (LLM)      (runs)

TRACE FUNCTIONS → INSTANTIATE
(patterns)        (new subgraphs)

FEDERATION → PULL/PUSH/MERGE
(cross-repo)
```

We have:
- Operation-level provenance (like vx-adapter)
- LLM evaluation (same as nikete/main)
- Richer replay (--subgraph, evaluation-aware keep-done)
- **Trace functions** — extract structural patterns from completed task graphs into reusable templates
- **Federation** — share agency entities across repos
- **Loop convergence** — agents signal when iterative loops should stop
- **2D graph layout** — force-directed visualization in terminal
- **Model registry, GC, setup wizard**

---

## 5. Concept Mapping (Updated with vx-adapter Terminology)

| nikete/main Term | nikete/vx-adapter Term | Our Term | Notes |
|------------------|----------------------|----------|-------|
| `agency` | `identity` | `agency` | vx-adapter argues "identity" is more precise (role+objective+skills = who you are, not what power you have) |
| `motivation` | `objective` | `motivation` | vx-adapter: "objective" aligns with BDI agent architecture literature |
| `evaluation` / `evaluate` | `reward` | `evaluation` / `evaluate` | vx-adapter: "reward" aligns with RL literature and enables non-LLM sources |
| `Evaluation.score` | `Reward.value` | `Evaluation.score` | Field rename with `#[serde(alias = "score")]` for backward compat |
| — | `Reward.source` | — | **NEW in vx-adapter.** Distinguishes "llm", "outcome:\<metric\>", "manual", etc. We have no equivalent. |
| `TraceEvent` | — (removed) | `OperationEntry` | vx-adapter replaced conversation-level traces with operation-level provenance |
| `TraceMeta` | — (removed) | — | Summary statistics per agent trace. We have `AgentRun` stats in our trace commands. |
| `Canon` | — (removed) | — | No equivalent in either our codebase or vx-adapter |
| `distill` | — (removed) | — | No equivalent anywhere now |
| `trace` (conversation) | `trace` (provenance) | `trace` (provenance + agent archives) | Three different meanings of "trace" across branches |
| `parse_stream_json()` | — (removed) | `parse_stream_json_stats()` | We have a stats-only parser in `src/commands/trace.rs`; nikete/main had a full event parser |
| `replay` | `replay` | `replay` | Same concept. vx-adapter adds `ReplayOptions` struct, `subgraph` option, uses `below_reward` |
| `runs` | `runs` | `runs` | Same concept. vx-adapter simplified RunMeta. |
| `render_canon_for_prompt()` | — (removed) | — | No prompt injection of historical knowledge |
| `build_distill_prompt()` | — (removed) | — | No LLM knowledge extraction |
| — | — | `TraceFunction` | **Ours only.** Parameterized workflow templates extracted from task graphs. |
| — | — | `Federation` | **Ours only.** Cross-repo agency entity sharing. |
| — | — | `--converged` | **Ours only.** Loop termination signal. |

---

## 6. Bugs and Issues (All 6 Verified Against Actual Code)

All bugs from the prior review have been verified against the actual source in `nikete/main`. All 6 remain present.

### 6.1 Timestamp Bug in parse_stream_json() — CONFIRMED

**Location:** `src/trace.rs`, `parse_stream_json()` function

```rust
let now = chrono::Utc::now().to_rfc3339();  // called ONCE
// ... all events use ts: now.clone() ...
```

All events get the identical timestamp — the moment of parsing, not when events occurred. Claude's stream-json includes timing information that could be extracted instead. The `ts` field is misleading.

**Severity:** Medium. Doesn't break functionality but makes timing analysis impossible.

**Status in vx-adapter:** N/A — `parse_stream_json()` was removed entirely. The vx-adapter's provenance system records real timestamps per operation.

### 6.2 --plan-only Creates Snapshot Side Effect — CONFIRMED

**Location:** `src/commands/replay.rs`

The snapshot is unconditionally created before the `plan_only` check. A dry run creates an orphan run directory. The test `test_run_replay_plan_only` explicitly asserts "snapshot should be created even for plan-only" — so this is intentional behavior, but surprising.

**Severity:** Low. Creates orphan directories but doesn't modify the graph.

**Status in vx-adapter:** Replay rewritten with `ReplayOptions` struct. Would need separate verification.

### 6.3 Duplicated load_eval_scores() — CONFIRMED

**Locations:** `src/runs.rs` (`load_evaluation_scores()`) and `src/commands/replay.rs` (`load_eval_scores()`).

Identical logic reading from `.workgraph/agency/evaluations/*.json`, returning `HashMap<String, f64>` of highest score per task.

**Severity:** Low — code duplication, not a runtime bug.

**Status in vx-adapter:** Both functions renamed to use `reward` terminology. Duplication persists but now uses `load_all_rewards_or_warn` from `identity` module in the replay command, partially addressing the issue.

### 6.4 Duplicated collect_transitive_dependents() — CONFIRMED

**Locations:** `src/runs.rs` (`collect_transitive_dependents()`) and `src/commands/replay.rs` (`collect_transitive_dependents_local()`). The `_local` suffix is a band-aid to avoid naming conflicts.

**Severity:** Low — code duplication.

**Status in vx-adapter:** Not verified separately, likely persists.

### 6.5 No Automatic Trace Extraction — CONFIRMED

The spawn wrapper in `src/commands/spawn.rs` handles post-agent completion by calling `wg done` or `wg fail` but does not invoke trace extraction. No integration with the coordinator either. Traces must be manually extracted via `wg trace-extract <agent-id>`.

**Severity:** Medium — breaks the capture → distill → replay pipeline in practice.

**Status in vx-adapter:** Moot — the stream-json trace system was removed. vx-adapter's provenance logging is automatic (inline `record()` calls in each mutating command).

### 6.6 Distillation LLM Call Not Implemented — CONFIRMED

**Location:** `src/commands/distill.rs`

```rust
println!("\nLLM integration is not yet implemented.");
println!("Use `wg distill {} --dry-run` to see the prompt that would be sent.", task_id);
```

The prompt construction works. The LLM call is stubbed.

**Severity:** High for the distillation concept, but capture and replay work independently.

**Status in vx-adapter:** Moot — `distill.rs` was removed entirely. The canon/distill approach was abandoned.

### Bug Summary

| # | Bug | nikete/main | vx-adapter |
|---|-----|-------------|------------|
| 1 | Timestamp bug in parse_stream_json | **Present** | N/A (removed) |
| 2 | --plan-only snapshot side effect | **Present** | Needs verification |
| 3 | Duplicated load_eval_scores | **Present** | Partially addressed |
| 4 | Duplicated collect_transitive_dependents | **Present** | Likely present |
| 5 | No automatic trace extraction | **Present** | N/A (replaced by inline provenance) |
| 6 | Distill LLM call not implemented | **Present** | N/A (removed) |

**Assessment:** Three of the six bugs (1, 5, 6) are addressed in vx-adapter by removing the problematic code entirely rather than fixing it. This is a legitimate resolution — the vx-adapter's provenance-based approach doesn't have these issues by design.

---

## 7. What We've Built Since the Last Review

Since the prior review (2026-02-19), our codebase has continued to diverge. Here's what we have that nikete doesn't, and how it relates to his work.

### Trace Functions (src/trace_function.rs — 1,099 lines)

Parameterized workflow templates extracted from completed task graphs. Key structs: `TraceFunction`, `FunctionInput` (8 typed input types with validation), `TaskTemplate`, `FunctionOutput`. Storage in `.workgraph/functions/` as YAML.

**Relation to nikete's work:** Where nikete's canon distills *knowledge* from conversations ("what did the agent learn"), our trace functions extract *structure* from task graphs ("what pattern of tasks solved this problem"). Complementary goals — canon is about context enrichment for re-execution, trace functions are about workflow reuse.

### Federation (src/federation.rs — 1,548 lines)

Cross-repo agency entity sharing: scan, pull, push, merge of roles/motivations/agents. Supports referential integrity (agents carry role+motivation deps), performance record merging, lineage merging. Also includes peer workgraph communication via Unix socket IPC and direct file access fallback.

**Relation to nikete's work:** No equivalent in either nikete branch. Federation enables organizational-scale workgraph deployments where teams share proven agent configurations. nikete's VX research docs touch on "peer exchange" at a conceptual level — federation could be the infrastructure layer for that.

### Loop Convergence (--converged flag)

`wg done <id> --converged` adds a `"converged"` tag that prevents loop edges from firing. Checked in `graph.rs:evaluate_loop_edges()`. Cleared by `wg retry`.

**Relation to nikete's work:** nikete has no loop termination signal. His loops run to max iterations. Our convergence flag gives agents agency over when to stop iterating.

### 2D Graph Visualization (src/commands/viz.rs — 2,418 lines)

Force-directed 2D box layout in terminal with Unicode box-drawing, ANSI color by status, loop edge annotations, phase annotations. BFS layer assignment with within-layer ordering by average parent position to minimize edge crossings.

**Relation to nikete's work:** nikete's viz is 1,659 lines with basic graph visualization. Our 2D layout is a significant extension that makes large graphs navigable in the terminal.

### Trace Animation (src/commands/trace_animate.rs — 330 lines)

Terminal TUI animation that replays historical execution as a series of graph snapshots. Uses crossterm for raw terminal mode. Controls: pause, step, speed adjustment.

**Relation to nikete's work:** No equivalent. This builds on our provenance system to provide visual replay of execution history.

### Trace Extraction to Functions (src/commands/trace_extract.rs — 973 lines)

Extracts completed task subgraphs into reusable `TraceFunction` templates. Detects parameters heuristically (file paths, URLs, commands, numbers). Validates extracted functions for internal consistency.

**Relation to nikete's work:** No equivalent. nikete's trace system captures conversation content; our extraction captures task graph structure.

### Model Registry (src/models.rs — 414 lines)

Catalog of 12+ AI models with provider, cost, context window, capabilities, and tier (Frontier/Mid/Budget). Persistent storage in `.workgraph/models.yaml`.

**Relation to nikete's work:** vx-adapter independently added an identical module. Same structs, same default catalog, same storage format. Strong convergent evolution.

### GC (src/commands/gc.rs — 415 lines)

Garbage collection of terminal tasks. Cascades to internal assign-/evaluate- tasks. Safety checks prevent removing tasks with non-terminal dependents.

**Relation to nikete's work:** vx-adapter independently added a functionally identical module.

### Setup Wizard (src/commands/setup.rs — 463 lines)

Interactive first-time configuration using `dialoguer`. Walks through executor, model, agency, and parallelism settings.

**Relation to nikete's work:** No equivalent in either nikete branch.

### Enhanced Testing (23 test files vs nikete's 14)

9 test files only in our fork, including exhaustive tests for trace, replay, runs, logging, federation, and trace functions. Total of ~11,000+ additional test lines.

---

## 8. Convergence and Divergence

### Where the codebases are converging naturally

These areas show independent convergent evolution — both forks arrived at the same (or very similar) solutions without coordination.

| Area | Evidence | Interpretation |
|------|----------|---------------|
| **Provenance logging** | Both added `provenance.rs` with append-only JSONL and zstd rotation. Same `OperationEntry` structure (timestamp, op, task_id, actor, detail). | This is the "right" design for operation auditing in a task graph system. The convergence validates our architecture. |
| **Model registry** | Both added `models.rs` with `ModelEntry` structs, tier classification, cost metadata, YAML persistence, default catalogs of the same models. | Multi-model support is a natural evolution once you're running diverse agents. |
| **Garbage collection** | Both added `gc.rs` with the same logic: remove terminal tasks, cascade to internal tasks, safety-check against non-terminal dependents. | Natural need once graphs grow large. |
| **Replay with subgraph filtering** | Our replay has `--subgraph`. vx-adapter's replay added a `subgraph` option in `ReplayOptions`. | Replay of the entire graph is too coarse — subgraph selection is a natural refinement. |
| **Operation-level over conversation-level** | vx-adapter removed stream-json trace parsing in favor of operation-level provenance. We never implemented conversation-level parsing (beyond stats), going straight to operation provenance. | Operation-level is more reliable (inline recording vs post-hoc parsing), more useful for audit, and doesn't depend on a specific LLM output format. |

### Where the codebases have genuinely diverged

These represent different philosophical choices, not one being "ahead" of the other.

| Area | Our approach | nikete's approach | Why they diverge |
|------|-------------|-------------------|------------------|
| **Knowledge reuse** | **Structural** — extract task graph patterns as `TraceFunction` templates, instantiate into new subgraphs. Reuse the *shape* of work. | **Contextual** — distill conversation traces into `Canon` knowledge artifacts, inject into agent prompts via `{{task_canon}}`. Reuse the *learnings* from work. (Abandoned in vx-adapter.) | Different theories of what's reusable. We bet on graph structure; nikete bet on conversation knowledge, then pivoted away. |
| **External feedback** | **None yet** — evaluations are purely LLM-generated. No pluggable reward sources. | **Pluggable** — `Reward.source` field supports "llm", "outcome:\<metric\>", "manual", and custom sources. VX integration as primary external feedback mechanism. | nikete is building toward a broader ecosystem (Veracity Exchange). We're focused on making the standalone system robust. |
| **Terminology** | `agency` / `motivation` / `evaluation` / `score` | `identity` / `objective` / `reward` / `value` | nikete aligns with RL and multi-agent systems literature. Our terms are more intuitive for newcomers but less precise academically. |
| **Cross-repo sharing** | **Federation** — full pull/push/merge of agency entities across repos, peer IPC, cross-repo task status queries. | **None** — no cross-repo features. VX peer exchange is conceptual, not implemented. | We're solving the organizational deployment problem. nikete is focused on single-repo with external market integration. |
| **Visualization** | **Rich terminal UI** — 2D force-directed layout, trace animation, ANSI colors, Unicode box-drawing. | **Basic graph output** — DOT, Mermaid, ASCII. | Different priorities: we invest in developer experience; nikete invests in research docs and integration design. |
| **Loop control** | **Agent-driven** — `--converged` flag lets agents signal when loops should stop. | **Iteration-based** — loops run to `max_iterations`. No early termination signal. | We trust agents to know when they're done. nikete's design doesn't give agents that agency. |
| **Documentation** | Code-focused — CLAUDE.md, AGENCY.md, inline comments. | Research-heavy — 7+ research documents totaling ~4,000 lines analyzing VX, organizational economics, Gepa integration, logging gaps. | nikete is doing design research for a future where workgraph integrates with external systems. We're building features for the present. |

### Assessment

The two forks are solving different problems at different time horizons:

- **Our fork** is building a complete, self-contained task orchestration system with strong developer experience (viz, animation, setup wizard), operational robustness (federation, convergence, GC, exhaustive tests), and workflow reuse (trace functions).

- **nikete's fork** is positioning workgraph as a node in a larger ecosystem — specifically one where agent performance is scored by real-world outcomes (not just LLM judgments) and where agents' track records are portable and verifiable across organizational boundaries.

The convergent evolution in provenance, models, and GC suggests the standalone-system problems are well-defined and have obvious solutions. The divergence in knowledge reuse, external feedback, and cross-repo sharing reflects genuinely different visions for what workgraph becomes.

---

## 9. Updated Recommendations

### From nikete/main — Still Valuable

1. **`parse_stream_json()` parser** — Despite the timestamp bug, the structured `TraceEvent` enum and parser are the most immediately useful novel code. Our `parse_stream_json_stats()` in `src/commands/trace.rs` only extracts counts; his parser extracts full conversation structure. **Port with timestamp fix.**

2. **Run snapshot with traces** — His `runs.rs` snapshots optionally copy trace and canon directories. Our snapshots only include graph state. **Add trace/provenance snapshots to our runs system.**

### From nikete/vx-adapter — High Value

3. **`Reward.source` field** — The single most impactful conceptual addition. Enabling non-LLM reward sources (outcome metrics, manual scores, external systems) is a clean, backward-compatible extension. **Adopt the concept: add a `source` field to our `Evaluation` struct.** No need to rename evaluation→reward.

4. **Research documents** — The VX deep-dive (723 lines), integration analysis (489 lines), and provenance design doc (327 lines) are substantial forward-looking research we don't have. **Import the docs to our `docs/research/` directory.**

5. **Provenance design doc** — `docs/design/provenance-system.md` identifies gaps in operation log coverage (which commands record and which don't). **Use as audit checklist for our provenance system.**

### Terminology — Consider but Don't Rush

6. **identity/objective/reward naming** — The RL-aligned terminology is more precise, and the `#[serde(alias)]` backward compatibility is well-done. But renaming touches every file and breaks all existing deployments. **Defer until a major version bump, if ever.** The current names work.

### Skip

7. **Canon/distill system** — nikete himself removed it in vx-adapter. The LLM distillation call was never implemented. The concept is interesting but premature. **Keep in design backlog; don't port.**

8. **Wholesale merge** — 104 files changed, 100K+ lines in the diff. **Cherry-pick only.** See [comparison report](nikete-fork-comparison-feb20.md) §5 for detailed merge feasibility analysis.

---

## 10. Summary

The prior review was accurate despite being reconstructed from cached agent logs — all struct definitions, architecture descriptions, and bug reports have been verified against the actual source. The six identified bugs are all confirmed present on `nikete/main`.

The major new finding is the **vx-adapter branch**, which represents a significant evolution: it removes the incomplete canon/distill system, adds provenance and models (converging with our design), introduces pluggable reward sources for external feedback, and contains extensive research documents on Veracity Exchange integration. The `Reward.source` field is the most important conceptual contribution — it's a minimal change that opens workgraph to external evaluation sources.

Our codebase has grown substantially since the last review with trace functions, federation, loop convergence, 2D visualization, trace animation, and a setup wizard. These features make our fork more complete as a standalone system, while nikete's work positions workgraph for ecosystem integration.

**Bottom line:** The two forks are complementary, not competing. Cherry-pick the `parse_stream_json()` parser (with timestamp fix), add `Evaluation.source` for pluggable rewards, import the VX research docs, and use the provenance design doc as an audit checklist. A wholesale merge remains infeasible and undesirable.
