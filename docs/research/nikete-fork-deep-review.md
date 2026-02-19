# Deep Code Review: nikete/workgraph Fork

**Reviewer:** scout (analyst)
**Date:** 2026-02-19
**Fork:** https://github.com/nikete/workgraph (now 404 — source recovered from agent-535 output logs)
**Commits reviewed:** 3 (replay system + FORK.md documentation + VX design document)
**Source recovery:** All 10 new files + 4 patches fully recovered from prior agent stream-json logs. The `docs/design-veracity-exchange.md` (+737 lines, 3rd commit) is unrecoverable — no agent ever read it and the repo has been deleted.

---

## 1. Full Inventory

### New Files (10 files, ~4,084 lines)

| File | Lines | Purpose |
|------|-------|---------|
| `FORK.md` | 162 | Documentation of the fork: capture/distill/replay pipeline, CLI commands, configuration, storage layout, test coverage |
| `docs/design-replay-system.md` | 554 | Full design document: problem statement, architecture, 6 tradeoff analyses (A–F), implementation plan, future extensions |
| `docs/design-veracity-exchange.md` | ~737 | **UNRECOVERABLE** — VX design document. Per evaluator notes: outcome-based scoring, peer exchange, credibility accumulation. Typed Rust structs and implementation plan |
| `src/trace.rs` | 814 | Core trace module: `TraceEvent` enum, stream-json parser, JSONL I/O, metadata computation, filtering, trace extraction. 13 tests |
| `src/canon.rs` | 626 | Canon (distilled knowledge) module: `Canon` struct with spec/tests/interaction_patterns/quality_signals, versioned YAML persistence, prompt rendering, distill prompt builder. 14 tests |
| `src/runs.rs` | 698 | Run management: snapshots, run ID generation, recursive directory copy, task reset logic (selective, with keep-done threshold), graph restore. 16 tests |
| `src/commands/trace_cmd.rs` | 212 | CLI for `wg trace-extract <agent-id>` and `wg trace <task-id>` with filtering |
| `src/commands/distill.rs` | 231 | CLI for `wg distill <task-id>` and `wg distill --all`. Builds distill prompt. LLM call not wired up |
| `src/commands/canon_cmd.rs` | 201 | CLI for `wg canon <task-id>` (view) and `wg canon --list` |
| `src/commands/replay.rs` | 391 | CLI for `wg replay --model <model>` with --failed-only, --below-score, --tasks, --keep-done, --plan-only. 4 tests |
| `src/commands/runs_cmd.rs` | 195 | CLI for `wg runs list/show/restore`. 5 tests |

### Modified Files (4 files, ~350 lines changed)

| File | Changes |
|------|---------|
| `src/config.rs` | +91 lines: `DistillConfig` and `ReplayConfig` structs with sensible defaults |
| `src/lib.rs` | +3 lines: Exports `canon`, `runs`, and `trace` modules |
| `src/commands/mod.rs` | +5 lines: Registers 5 new command modules |
| `src/main.rs` | +180 lines: Registers 6 CLI subcommands (trace-extract, trace, distill, canon, replay, runs) with clap parsing. Adds `RunsCommands` enum |

### Unchanged
- `Cargo.lock` has +171/-164 changes (dependency updates for `serde_yaml`)
- No existing types, functions, or behaviors are modified

---

## 2. Struct Definitions

### TraceEvent (src/trace.rs:18-54)

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

pub struct ToolCall { pub name: String, pub args: String, pub id: String }
```

### TraceMeta (src/trace.rs:57-67)

```rust
pub struct TraceMeta {
    pub task_id: String,
    pub agent_id: String,
    pub model: Option<String>,
    pub duration_s: Option<f64>,
    pub turn_count: usize,
    pub user_intervention_count: usize,
    pub tool_call_count: usize,
    pub estimated_tokens: usize,
}
```

### Canon (src/canon.rs:14-26)

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

pub struct DistillSource { pub agent_id: String, pub model: Option<String>, pub iteration: u32 }
pub struct InteractionPatterns { pub corrections: Vec<Correction>, pub sticking_points: Vec<StickingPoint>, pub human_preferences: Vec<String> }
pub struct Correction { pub context: String, pub correction: String, pub lesson: String }
pub struct StickingPoint { pub description: String, pub resolution: String, pub iterations_to_resolve: u32 }
pub struct QualitySignals { pub evaluation_scores: Vec<f64>, pub convergence: bool, pub remaining_issues: Vec<String> }
```

### RunMeta (src/runs.rs:11-21)

```rust
pub struct RunMeta {
    pub id: String,
    pub model: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub parent_run: Option<String>,
    pub task_count: usize,
    pub tasks_reset: usize,
    pub tasks_kept: usize,
}
```

### Config additions (src/config.rs patch)

```rust
pub struct DistillConfig {
    pub auto_distill: bool,           // default: false
    pub model: String,                // default: "sonnet"
    pub keep_versions: bool,          // default: true
    pub include_tool_results: bool,   // default: false
    pub max_trace_tokens: usize,      // default: 50000
}

pub struct ReplayConfig {
    pub auto_distill_before: bool,    // default: true
    pub keep_done_threshold: f64,     // default: 0.9
    pub snapshot_traces: bool,        // default: true
    pub snapshot_canon: bool,         // default: true
}
```

---

## 3. The OrchestratorAdapter Trait

**Status: NOT FOUND in recovered source code.**

The `OrchestratorAdapter` trait was mentioned in the task description as a feature of the VX design. Since `docs/design-veracity-exchange.md` is unrecoverable, we cannot confirm its existence or definition. The recovered files (trace.rs, canon.rs, runs.rs, all commands, config patch, main patch) contain no trait named `OrchestratorAdapter` and no trait definitions at all. The fork's architecture is purely function-based, not trait-based.

**What we can infer:** If the VX design proposed an `OrchestratorAdapter` trait, it would logically need methods like:
- `submit_task(task_id, description, context) -> RunHandle`
- `get_task_status(task_id) -> Status`
- `get_task_artifacts(task_id) -> Vec<Artifact>`
- `replay_task(task_id, model, canon) -> RunHandle`
- `get_evaluation(task_id) -> Option<Evaluation>`

This maps to what our `TemplateVars` + `ExecutorConfig` + coordinator already provide, just not behind a formal trait interface.

---

## 4. The VX Design Document

**Status: UNRECOVERABLE.**

`docs/design-veracity-exchange.md` (+737 lines) was the largest new file in the fork and its 3rd (most recent) commit. No agent ever fetched its contents. The repository is now 404 on GitHub, and no web archive captured it.

### What we know (from evaluator notes + task descriptions):

The VX design document covered:
1. **Outcome-based scoring** — workgraph task sub-units have measurable real-world outcomes (portfolio P&L, prediction MSE) that become veracity scores
2. **Peer exchange** — public (non-sensitive) prompt sections posted to a market where others suggest improvements; good suggestions build credibility
3. **Credibility accumulation** — trust network built on demonstrated competence
4. **Typed Rust structs** — concrete type definitions for the VX protocol
5. **Implementation plan** — phased approach for integrating VX with workgraph

The evaluator described this as "a distinct conceptual contribution (outcome-based scoring, peer exchange, credibility accumulation) separate from the replay system."

**Recommendation:** Ask nikete directly for this document, or request he re-publish the fork.

---

## 5. Architecture: The Three-Stage Pipeline

```
CAPTURE → DISTILL → REPLAY
(traces)   (canons)   (runs)
```

### Stage 1: Capture (trace.rs, trace_cmd.rs)

**What it captures:** Post-hoc parsed Claude `--output-format stream-json` output from agent `output.log` files. Converts raw stream-json into typed `TraceEvent` JSONL.

**How it works:**
1. `wg trace-extract <agent-id>` reads `.workgraph/agents/{agent-id}/output.log`
2. `parse_stream_json()` converts stream-json events into `TraceEvent` variants
3. Events written to `.workgraph/traces/{agent-id}/trace.jsonl`
4. Summary metadata (turn count, tool calls, user interventions, token estimate) written to `trace-meta.json`
5. `wg trace <task-id>` looks up agents via registry, loads traces, displays with filtering (--full, --turns-only, --user-only, --json)

**Key functions:**
- `parse_stream_json(content: &str) -> Vec<TraceEvent>` (line 78-204)
- `write_trace(events, path)` / `read_trace(path)` (lines 207-235)
- `compute_meta(events, task_id, agent_id) -> TraceMeta` (lines 238-297)
- `extract_trace(workgraph_dir, agent_id, task_id) -> TraceMeta` (lines 320-352)
- `filter_events(events, user_only, turns_only)` (lines 358-380)

**Design choice:** Capture is post-hoc (after agent completes), not real-time. Simpler but traces are lost if agent crashes before completion.

### Stage 2: Distill (canon.rs, distill.rs, canon_cmd.rs)

**What Canon is:** A normalized knowledge artifact — a YAML file containing refined spec, test expectations, interaction patterns, and quality signals distilled from conversation traces via LLM.

**How distill works:**
1. `wg distill <task-id>` loads task definition, finds agents via registry
2. Reads agent output logs as trace content
3. Loads evaluation scores from `agency/evaluations/`
4. Loads previous canon if exists (for refinement)
5. `build_distill_prompt()` constructs structured prompt with all context
6. **LLM call NOT YET WIRED UP** — prints "LLM integration not yet implemented"
7. `--dry-run` shows the prompt that would be sent

**Canon injection:** Designed for `{{task_canon}}` template variable in executor prompts, following existing `{{task_identity}}` pattern. `render_canon_for_prompt()` does priority-ordered section truncation within token budget.

**Relation to `wg evolve`:** Both are LLM-powered synthesis steps. Evolve mutates role/motivation definitions based on evaluations. Distill produces per-task knowledge artifacts from conversation traces. They operate at different levels: evolve refines *who does the work*, distill refines *what the work should produce*.

### Stage 3: Replay (runs.rs, replay.rs, runs_cmd.rs)

**What replay does:** Snapshot + in-place reset.
1. Generate run ID (`run-001`, `run-002`, ...)
2. Snapshot `.workgraph/` state (graph.jsonl, optionally traces/ and canon/) to `runs/{run-id}/`
3. Reset selected tasks to Open status (clearing assigned, started_at, completed_at, artifacts, loop_iteration — preserving log entries and blocked_by)
4. Save graph; coordinator dispatches with new model

**Selective replay options:**
- `--failed-only` — only reset Failed/Abandoned tasks
- `--below-score 0.8` — only reset tasks with eval score below threshold
- `--tasks task-1,task-3` — reset specific tasks + transitive dependents
- `--keep-done` — preserve high-scoring Done tasks (configurable threshold 0.9)
- `--plan-only` — dry run showing what would be reset

**Run management:** `wg runs list/show/restore`

**Does it support different models/agents on replay?** Yes — the `--model` flag sets the model for the replay run. The coordinator uses whatever agent assignment logic is configured. Canon from prior runs is injected into prompts to provide accumulated knowledge.

**How are runs tracked?** Each run gets a `RunMeta` JSON file in `runs/{run-id}/run-meta.json` with model, timestamps, task counts, and parent run reference.

---

## 6. Config Changes

Two new sections added to `config.toml`:

```toml
[distill]
auto_distill = false            # auto-distill on task completion
model = "sonnet"                # model for distillation LLM calls
keep_versions = true            # keep versioned canon history
include_tool_results = false    # include tool results in traces
max_trace_tokens = 50000        # max trace size for distill prompts

[replay]
auto_distill_before = true      # auto-distill before replay
keep_done_threshold = 0.9       # score threshold for --keep-done
snapshot_traces = true          # include traces in run snapshots
snapshot_canon = true           # include canons in run snapshots
```

Both use `#[serde(default)]` throughout, so existing configs work without modification.

---

## 7. Diff Analysis: Invasiveness

### Changes to wg core: **MINIMAL**

The fork is genuinely additive. The 4 modified files are:

1. **src/config.rs** (+91 lines) — Two new structs with defaults. No changes to existing `Config`, `CoordinatorConfig`, or `AgencyConfig` structs. New fields added to root `Config` with `#[serde(default)]`.

2. **src/lib.rs** (+3 lines) — Three `pub mod` declarations. No changes to existing exports.

3. **src/commands/mod.rs** (+5 lines) — Five `pub mod` declarations. No changes to existing modules.

4. **src/main.rs** (+180 lines) — New enum variants in `Commands`, new `RunsCommands` enum, match arms in the dispatch function. All additions at the end of existing match blocks.

**No existing types are modified.** No existing function signatures change. No existing behavior changes. This is the best possible outcome for merge compatibility.

### New dependency: `serde_yaml`

Canon files use YAML via `serde_yaml`. The rest of workgraph uses JSON/JSONL. This adds one dependency and introduces format inconsistency. However, YAML's multiline string support makes canon files more readable (spec and tests fields are typically multi-line).

---

## 8. Concept Mapping: nikete → wg → What We'd Need to Change

| nikete Concept | Closest wg Concept | Gap / What We'd Need to Change |
|----------------|-------------------|-------------------------------|
| `TraceEvent` enum | `OperationEntry` in `provenance.rs` | Different granularity. OperationEntry records graph mutations. TraceEvent records conversation turns. **Complementary, not overlapping.** We'd add trace.rs alongside provenance.rs. |
| `TraceMeta` | Agent output in `.workgraph/output/{task_id}/` | Our `capture_task_output()` saves git diff + artifacts + log. TraceMeta adds structured conversation statistics. **We'd add TraceMeta to capture_task_output().** |
| `Canon` struct | `Role` + `Motivation` (agency.rs) | Different level. Canon captures per-task knowledge. Role/Motivation capture per-agent identity. **We'd add canon.rs as a new module — no overlap with agency.** |
| `Canon.spec` | Task `description` + `verify` fields | Canon.spec is a refined version informed by execution experience. **We'd inject it via `{{task_canon}}` in executor templates.** |
| `Canon.interaction_patterns` | Nothing equivalent | Novel concept. Corrections, sticking points, preferences have no wg equivalent. **New module needed.** |
| `Canon.quality_signals` | `Evaluation.score` + `Evaluation.dimensions` | evaluation_scores in quality_signals are just aggregated eval scores. **Simple mapping from existing evaluations.** |
| `DistillSource` | `EvaluationRef` in agency.rs | Similar provenance tracking (agent_id, model, iteration). **Could unify.** |
| `RunMeta` | Nothing equivalent | We have no run/snapshot management. **New module needed (runs.rs).** |
| `snapshot()` / `restore_run()` | Nothing equivalent | We have provenance log but no graph snapshots. **New functions needed.** |
| `reset_tasks_for_replay()` | `retry` command (retry.rs) | retry resets one task. reset_tasks_for_replay does selective batch reset with transitive dependents. **Extension of retry logic.** |
| `collect_transitive_dependents()` | Nothing in graph.rs | Our graph module has no reverse dependency traversal. **Add to graph.rs as utility.** |
| `load_eval_scores()` | `load_all_evaluations_or_warn()` in agency.rs | load_eval_scores reads the same eval JSON files but returns HashMap<task_id, max_score>. **Factor into shared utility.** |
| `parse_stream_json()` | Nothing equivalent | We store raw output.log with no parsing. **New parser needed.** |
| `render_canon_for_prompt()` | `render_identity_prompt()` in agency.rs | Same pattern: render structured data into prompt text with truncation. **Add `{{task_canon}}` to TemplateVars.** |
| `build_distill_prompt()` | `render_evaluator_prompt()` in agency.rs | Same pattern: build structured prompt for LLM call. **New prompt builder alongside evaluator.** |
| `DistillConfig` / `ReplayConfig` | `CoordinatorConfig` / `AgencyConfig` | Same config pattern. **Add to Config struct.** |
| `OrchestratorAdapter` trait (VX) | `TemplateVars` + `ExecutorConfig` | Our executor system is function-based. **An adapter trait would formalize this interface.** |

---

## 9. Bugs and Issues Found

### 9.1 Timestamp Bug in parse_stream_json() (CONFIRMED)

**Location:** `src/trace.rs:79`

```rust
let now = chrono::Utc::now().to_rfc3339();
```

This is called ONCE at the start of parsing. All events in the trace get the SAME timestamp (the time of parsing, not when events occurred). Claude's stream-json includes timing information (`duration_ms`, timestamps in message objects) that could be extracted instead. The `ts` field is essentially useless for understanding timing within a conversation.

**Severity:** Medium — doesn't break functionality but makes the trace data misleading.

### 9.2 --plan-only Creates Snapshot Side Effect (CONFIRMED)

**Location:** `src/commands/replay.rs:33-41`

The snapshot is created at line 36 (before the `if plan_only` check at line 95). A dry run shouldn't create side effects. The snapshot should be gated behind the plan_only check.

**Severity:** Low — creates an orphan run directory but doesn't modify the graph.

### 9.3 Duplicated load_eval_scores() Function

**Locations:** `src/runs.rs:277-306` and `src/commands/replay.rs:253-282`

Identical implementations that read evaluation JSON files and extract the highest score per task. Should be factored into a shared utility in agency.rs alongside `load_all_evaluations_or_warn`.

**Severity:** Low — code duplication, not a runtime bug.

### 9.4 Duplicated collect_transitive_dependents()

**Locations:** `src/runs.rs:261-273` and `src/commands/replay.rs:238-250` (labeled `_local`)

Identical graph traversal utility. Should be in the core graph module.

**Severity:** Low — code duplication.

### 9.5 No Automatic Trace Extraction

Traces must be manually extracted with `wg trace-extract <agent-id>` after an agent completes. There's no integration with the spawn wrapper (`run.sh`) or coordinator to automatically extract traces on completion. This means traces won't exist unless someone runs the extraction command.

**Severity:** Medium — breaks the capture → distill → replay pipeline in practice.

### 9.6 Distillation LLM Call Not Implemented

**Location:** `src/commands/distill.rs:140-168`

The `wg distill` command builds the prompt but prints "LLM integration not yet implemented." The capture → distill → replay pipeline is functionally 2/3 complete.

**Severity:** High for the distillation stage, but capture and replay work independently.

---

## 10. Test Quality Assessment

**52 new tests total**, well-distributed across modules:

| Module | Tests | Coverage Quality |
|--------|-------|-----------------|
| `src/trace.rs` | 13 | Excellent — parsing, roundtrip I/O, metadata, filtering, extraction from output.log |
| `src/canon.rs` | 14 | Excellent — serialization, persistence, versioning, prompt rendering, truncation, distill prompt building |
| `src/runs.rs` | 16 | Excellent — paths, IDs, snapshots, metadata, full/selective/preserving reset, restore |
| `src/commands/replay.rs` | 4 | Good — basic replay, failed-only, plan-only, specific tasks |
| `src/commands/runs_cmd.rs` | 5 | Good — list, show, restore, error handling |

Tests use `tempfile::TempDir` for isolation. Test helpers (`make_task`, `make_task_with_status`, `setup_workgraph`) match our `test_helpers` module. Edge cases covered include empty input, missing files, gaps in run IDs, and roundtrip serialization.

**Missing test coverage:**
- No tests for `distill.rs` (the unfinished LLM integration)
- No tests for `trace_cmd.rs` or `canon_cmd.rs` (CLI display functions)
- No integration test that runs the full capture → distill → replay pipeline

---

## 11. Design Document Analysis

The `docs/design-replay-system.md` (554 lines) is thorough and well-structured:

### Tradeoff Analyses (6 decision points)

| Tradeoff | Recommended | Implemented |
|----------|------------|-------------|
| **A. Capture method** | A1: Parse stream-json | Yes — `parse_stream_json()` in trace.rs |
| **B. Capture timing** | B1: Post-hoc | Yes — `extract_trace()` reads completed output.log |
| **C. Distill granularity** | C1: Per-task | Yes — one canon per task |
| **D. Distill timing** | D1+D2: Manual + auto | Partial — manual works, auto not wired |
| **E. Replay isolation** | E4: Snapshot + in-place | Yes — `snapshot()` + `reset_tasks_for_replay()` |
| **F. Canon injection** | F1: Template variable | Designed but not yet wired — `render_canon_for_prompt()` exists, `{{task_canon}}` not added to TemplateVars |

### Implementation vs. Design

The implementation closely follows the design. Most deviations are incomplete features (distill LLM call, auto-distill, `{{task_canon}}` injection) rather than design-implementation mismatches. The design is honest about what's phase 1 vs. future work.

---

## 12. Recommendations

### Adopt Wholesale (High Priority)

1. **TraceEvent enum + parse_stream_json() parser** — Fills a critical gap. Our provenance system records that an agent ran, but not what it said. Structured traces make agent behavior queryable and comparable across models. Port trace.rs with minimal modifications.

2. **Trace extraction CLI (wg trace-extract, wg trace)** — Useful immediately for debugging agent behavior, even without replay. Port trace_cmd.rs.

3. **Selective task reset logic (reset_tasks_for_replay)** — The --failed-only, --below-score, --tasks, --keep-done options + transitive dependent reset is well-designed. Port from runs.rs. Factor `collect_transitive_dependents()` into graph.rs.

### Adopt with Modifications (High Priority)

4. **Replay mechanism (snapshot + reset + re-execute)** — Adopt but wire into provenance log. Every snapshot, reset, and restore should be recorded as an OperationEntry. This gives us unified audit trail: "what happened" (provenance) + "what was said" (traces) + "what was reset" (replay ops).

5. **Run management (wg runs list/show/restore)** — Adopt but add `wg runs diff <a> <b>` for comparing evaluation scores across runs.

6. **Auto trace extraction** — Fix the gap in nikete's implementation. Wire trace extraction into the spawn wrapper (run.sh) or coordinator completion handler so traces are always captured. Without this, the pipeline is manual and unreliable.

### Adopt Later (Medium Priority)

7. **Canon/distillation concept** — The idea is compelling but the implementation is incomplete (no LLM call). Depends on traces and replay being in place first. Sequence: traces → replay → distill.

8. **Canon struct design** — The spec/tests/interaction_patterns/quality_signals structure is well-thought-out. When we implement distillation, use this struct design.

9. **`{{task_canon}}` template variable** — Clean integration with our executor template system. Add when canon is implemented.

10. **Priority truncation in render_canon_for_prompt()** — Generalizable pattern for context window management. Could be applied to our prompt builder more broadly.

### Skip / Reconsider

11. **serde_yaml dependency** — Consider using JSON instead of YAML for canon files. The multiline readability benefit is real but format consistency matters. Alternatively, use YAML only for human-facing canon display and JSON for storage.

12. **load_eval_scores() duplication** — Don't port the duplicated function. Factor into a shared utility from the start.

### Must Investigate (Blocked)

13. **VX design document and OrchestratorAdapter trait** — Cannot evaluate without the document. Request from nikete. The evaluator described it as "a distinct conceptual contribution separate from the replay system" — it may define the interface that makes workgraph pluggable into external scoring/exchange systems.

---

## 13. Integration Effort Estimate

### Minimal integration (capture + replay only)
- Port `trace.rs` → `src/trace.rs` (adapted to our module structure)
- Port `runs.rs` → `src/runs.rs` (wire into provenance)
- Port trace/replay/runs CLI commands
- Add `{{task_canon}}` placeholder to `TemplateVars` (empty string for now)
- Wire trace extraction into coordinator completion
- Add `DistillConfig` + `ReplayConfig` to config.rs
- ~1,500 lines of new code + ~200 lines of integration

### Full integration (capture + distill + replay)
- All of the above, plus:
- Port `canon.rs` → `src/canon.rs`
- Wire distill LLM call (follow the evaluator pattern from agency.rs)
- Implement `{{task_canon}}` injection in `TemplateVars::from_task()`
- Add auto-distill to coordinator tick
- ~2,500 lines of new code + ~400 lines of integration

### What we'd need to change in wg core
- `src/config.rs`: +91 lines (2 new config structs)
- `src/lib.rs`: +3 lines (module exports)
- `src/commands/mod.rs`: +5 lines (command registrations)
- `src/main.rs`: +180 lines (CLI arg parsing + dispatch)
- `src/service/executor.rs`: +20 lines (add `{{task_canon}}` to TemplateVars)
- `src/service/coordinator.rs`: +30 lines (auto trace extraction on completion)
- `src/graph.rs`: +20 lines (add `collect_transitive_dependents()`)
- `src/provenance.rs`: +10 lines (add replay operation types)

**Total core changes: ~360 lines across 8 existing files.** Non-invasive.

---

## 14. Summary

nikete's fork is substantial, well-designed, and architecturally sound. The three-stage pipeline (capture → distill → replay) addresses real gaps in workgraph's ability to learn from past executions. The code quality is good (52 tests, clean Rust patterns, proper error handling), and the changes are genuinely additive with no breaking changes.

**Key strengths:**
- Structured trace parsing fills a critical observability gap
- Selective replay with transitive dependent reset is well-engineered
- Design document shows careful tradeoff analysis
- Canon concept provides a novel knowledge synthesis layer

**Key weaknesses:**
- Distillation LLM call is not implemented (the most novel part)
- No automatic trace extraction (manual step breaks the pipeline)
- Timestamp bug makes trace timing data useless
- --plan-only creates snapshot side effect
- Code duplication (load_eval_scores, collect_transitive_dependents)
- VX design document is lost (repo deleted)

**Bottom line:** Adopt the capture and replay systems now. They're production-ready and fill real gaps. Add distillation later once we've validated the trace + replay workflow. Investigate the VX design separately — it represents a different (potentially more ambitious) vision for workgraph's role in a broader ecosystem.
