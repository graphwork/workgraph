# Review: nikete/workgraph Fork — Replay System

**Reviewer:** scout (analyst)
**Date:** 2026-02-18
**Fork:** https://github.com/nikete/workgraph
**Commits reviewed:** 2 (replay system + FORK.md documentation)

---

## 1. What Changed — File-by-File Summary

### New Files (9 files, ~3,544 lines added)

| File | Lines | Purpose |
|------|-------|---------|
| `FORK.md` | 163 | Comprehensive documentation of what the fork adds: the capture/distill/replay pipeline, CLI commands, configuration, storage layout, and test coverage |
| `docs/design-replay-system.md` | 555 | Full design document with problem statement, architecture, tradeoff analysis for 6 decision points (A–F), implementation plan, and future extensions |
| `src/trace.rs` | 815 | Core trace module: `TraceEvent` enum, stream-json parser, JSONL I/O, metadata computation, filtering, trace extraction from agent output logs. 13 tests |
| `src/canon.rs` | 627 | Canon (distilled knowledge) module: `Canon` struct with spec/tests/interaction_patterns/quality_signals, versioned YAML persistence, prompt rendering, distill prompt builder, output parser. 14 tests |
| `src/runs.rs` | 699 | Run management module: snapshots, run ID generation, recursive directory copy, task reset logic (selective, with keep-done threshold), graph restore. 16 tests |
| `src/commands/trace_cmd.rs` | 213 | CLI for `wg trace-extract <agent-id>` and `wg trace <task-id>` with filtering options (--full, --turns-only, --user-only, --json) |
| `src/commands/distill.rs` | 232 | CLI for `wg distill <task-id>` and `wg distill --all`. Builds distill prompt from traces + evaluations + previous canon. LLM call not yet wired up (--dry-run works) |
| `src/commands/canon_cmd.rs` | 202 | CLI for `wg canon <task-id>` (view) and `wg canon --list` |
| `src/commands/replay.rs` | 392 | CLI for `wg replay --model <model>` with --failed-only, --below-score, --tasks, --keep-done, --plan-only. 4 tests |
| `src/commands/runs_cmd.rs` | 196 | CLI for `wg runs list`, `wg runs show <id>`, `wg runs restore <id>`. 5 tests |

### Modified Files (4 files, ~350 lines changed)

| File | Change Summary |
|------|---------------|
| `src/config.rs` | +91 lines: Added `DistillConfig` and `ReplayConfig` structs with sensible defaults (auto_distill=false, keep_versions=true, max_trace_tokens=50000, keep_done_threshold=0.9, snapshot_traces/canon=true) |
| `src/lib.rs` | +3 lines: Exports `canon`, `runs`, and `trace` modules |
| `src/commands/mod.rs` | +5 lines: Registers `canon_cmd`, `distill`, `replay`, `runs_cmd`, `trace_cmd` command modules |
| `src/main.rs` | +180 lines: Registers 6 new CLI subcommands (`trace-extract`, `trace`, `distill`, `canon`, `replay`, `runs`) with full clap argument parsing. Adds `RunsCommands` subcommand enum. Wires all new commands to their handlers |

### Unchanged

`Cargo.lock` has +171/-164 changes (dependency updates, likely `serde_yaml` addition).

---

## 2. Logging/Replay Architecture

nikete's fork adds a **three-stage pipeline** for workflow replay:

```
  CAPTURE  →  DISTILL  →  REPLAY
  (traces)    (canons)    (runs)
```

### 2.1 Capture: Structured Conversation Traces

**Approach:** Post-hoc parsing of Claude's `--output-format stream-json` output from agent `output.log` files.

**Data model:** `TraceEvent` is a tagged enum with 6 variants:
- `System` — rendered prompt
- `Assistant` — model output with optional `Vec<ToolCall>`
- `ToolResult` — tool execution result linked by call_id
- `User` — human intervention (with optional source tag)
- `Error` — execution error (recoverable flag)
- `Outcome` — final result (status, exit_code, duration, artifacts)

**Storage:** `.workgraph/traces/<agent-id>/trace.jsonl` + `trace-meta.json`

**How it works:**
1. `wg trace-extract <agent-id>` reads the agent's `output.log`
2. `parse_stream_json()` converts Claude's stream-json events into `TraceEvent` variants
3. Events are written to JSONL; summary metadata (turn count, tool calls, user interventions, token estimate) is written to a companion JSON file
4. `wg trace <task-id>` looks up agents via the registry, loads their trace files, and displays with filtering

**Key design choice:** Capture is post-hoc (after agent completes), not real-time. This is simpler but means traces are lost if the agent crashes before completion.

### 2.2 Distill: LLM-Powered Knowledge Extraction

**Approach:** Build a structured prompt containing the task definition, all traces, artifacts, and evaluations. Send to an LLM that produces a **canon** — a YAML artifact with:
- `spec` — refined specification synthesized from conversations
- `tests` — expected test outcomes / acceptance criteria
- `interaction_patterns` — corrections, sticking points, human preferences
- `quality_signals` — evaluation scores, convergence status, remaining issues

**Storage:** `.workgraph/canon/<task-id>.yaml` with optional version history (`<task-id>.v1.yaml`, `.v2.yaml`, etc.)

**Current state:** The distill prompt builder works, `--dry-run` shows the prompt, but the actual LLM call is not yet wired up. The code prints "LLM integration not yet implemented."

**Template injection:** Canon content is designed to be injected into agent prompts via a `{{task_canon}}` template variable, following the existing `{{task_identity}}` pattern.

### 2.3 Replay: Re-execution with Upgraded Context

**Approach:** Snapshot + in-place reset.

1. Generate a run ID (`run-001`, `run-002`, etc.)
2. Snapshot current `.workgraph/` state (graph.jsonl, optionally traces/ and canon/) to `runs/<run-id>/`
3. Reset selected tasks to `open` status (clearing assigned, started_at, completed_at, artifacts, loop_iteration — but preserving log entries and blocked_by structure)
4. Save updated graph; the coordinator dispatches normally with the new model

**Selective replay options:**
- `--failed-only` — only reset Failed/Abandoned tasks
- `--below-score 0.8` — only reset tasks with evaluation score below threshold
- `--tasks task-1,task-3` — reset specific tasks + their transitive dependents
- `--keep-done` — preserve high-scoring Done tasks (uses configurable threshold)
- `--plan-only` — dry run showing what would be reset

**Run management:** `wg runs list/show/restore` for browsing and rolling back to previous run states.

---

## 3. Comparison to Our Provenance System Design

Our provenance system (designed in the `design-provenance-system` task, partially implemented in `src/provenance.rs`, informed by `docs/research/logging-gaps.md`) takes a fundamentally different approach from nikete's replay system. Here's how they compare:

### 3.1 Scope and Focus

| Dimension | Our Provenance System | nikete's Replay System |
|-----------|----------------------|----------------------|
| **Primary goal** | Full audit trail of all graph mutations | Re-running workflows with better models |
| **What it records** | Every graph operation (add, edit, done, fail, claim, etc.) | Agent conversation traces only |
| **Granularity** | Operation-level (every CLI command) | Task/agent-level (per-execution) |
| **Coverage** | All mutations to the graph, including non-agent changes | Only agent executions; no record of manual edits, dependency changes, etc. |
| **Reconstruction** | Can reconstruct graph state at any point in time | Can snapshot and restore full graph state |
| **LLM involvement** | None (pure logging) | Distillation requires LLM calls |

### 3.2 Architecture Comparison

**Our approach — append-only operation log:**
- `src/provenance.rs`: `OperationEntry { timestamp, op, task_id, actor, detail }` appended to `.workgraph/log/operations.jsonl`
- Log rotation with zstd compression when file exceeds threshold
- `read_all_operations()` reads across rotated + current files
- Every command that mutates the graph records an operation
- Content-addressed prompt archive planned

**nikete's approach — trace + distill + replay pipeline:**
- Separate trace files per agent (not a unified log)
- LLM-powered distillation layer that produces structured knowledge artifacts
- Snapshot + reset mechanism for re-execution
- Run management with comparison capabilities

### 3.3 What Each Does Better

**Our provenance system does better:**
- **Completeness**: Records ALL graph mutations, not just agent executions. Manual `wg edit`, `wg add`, dependency changes, config changes — all captured.
- **Non-LLM operations**: Captures evolve prompts, evaluate prompts, assign decisions — operations nikete's system doesn't touch.
- **Deterministic reconstruction**: Given the operation log, you can rebuild exact graph state at any timestamp. No LLM interpretation needed.
- **Log rotation with compression**: Built-in zstd rotation keeps storage manageable for long-running projects.
- **No LLM cost**: Pure append-only logging has zero LLM cost. nikete's distillation requires LLM calls.

**nikete's system does better:**
- **Structured conversation traces**: Parses Claude's stream-json into typed events (System, Assistant, ToolResult, User, Error, Outcome). Our system stores raw output.log without structure.
- **Knowledge distillation**: The canon concept — extracting refined specs, test expectations, and interaction patterns from conversation history — is genuinely novel. Our system captures data but doesn't synthesize it.
- **Replay-as-a-feature**: Full workflow replay with model upgrades, selective re-execution, and run comparison. Our system provides the raw data for replay but no replay mechanism.
- **Canon-enriched re-execution**: Injecting distilled knowledge from prior runs into future agent prompts is a strong idea for iterative improvement.
- **Run management**: Snapshots, run IDs, restore capabilities. Our system has no equivalent.

### 3.4 Overlap and Gaps

**Both systems capture:**
- Agent output (our system as operations, nikete's as structured traces)
- Task status transitions
- Evaluation scores (referenced by both)

**Neither system captures (yet):**
- Real-time human interaction during agent execution (both are post-hoc)
- External filesystem reads made by agents
- Cost/token tracking at the per-task level

**Gap in nikete's system (that ours addresses):**
- Graph mutations between agent executions (manual edits, dependency changes, config changes)
- Prompt archival for non-agent LLM calls (evolve, evaluate, assign)
- CLI invocation history

**Gap in our system (that nikete's addresses):**
- Structured parsing of agent output into conversation turns
- Knowledge synthesis from execution history
- Workflow replay mechanism
- Run comparison and rollback

---

## 4. Ideas We Should Adopt

### 4.1 Structured Conversation Traces (High Priority)

nikete's `TraceEvent` enum and `parse_stream_json()` function should be adopted. The stream-json parser converts raw `output.log` into typed, queryable events. This fills a gap identified in our logging-gaps.md (section 2.4: "No conversation transcript").

**What to port:**
- The `TraceEvent` enum (6 variants) and `ToolCall` struct
- The `parse_stream_json()` parser
- The `TraceMeta` summary computation
- The `wg trace-extract` and `wg trace` commands

**Why:** Our provenance system records that an agent ran, but not *what* it said or did. Structured traces make agent behavior queryable, debuggable, and comparable across model versions.

### 4.2 Workflow Replay Mechanism (High Priority)

The snapshot + reset + re-execute pattern is clean and practical. We should adopt:

- Graph snapshotting before replay
- Selective task reset (--failed-only, --below-score, --tasks, --keep-done)
- Run management (list, show, restore)
- Transitive dependent reset (if you re-run task A, also re-run everything that depends on A)

**Modification needed:** Wire replay into our provenance system so that replays are themselves recorded as operations in the event log.

### 4.3 Canon Concept (Medium Priority, After Provenance)

The idea of distilling conversation traces into reusable knowledge artifacts is compelling but depends on having structured traces first. Adopt this in sequence:
1. First: structured trace capture (4.1)
2. Then: provenance-aware replay (4.2)
3. Then: distillation pipeline (4.3)

The canon struct design (spec, tests, interaction_patterns, quality_signals) is well thought out. The prompt rendering and template injection (`{{task_canon}}`) integrate cleanly with our existing executor template system.

### 4.4 Run Comparison (Future)

The `wg runs` infrastructure enables comparing executions across model versions. This is valuable for:
- Benchmarking new models against known-good results
- Detecting regressions when models change
- Understanding which tasks benefit most from model upgrades

---

## 5. What Our Design Does Better or Differently

### 5.1 Comprehensive Event Log vs. Agent-Only Traces

Our provenance system records every graph mutation from every source (CLI, coordinator, agents). nikete's system only captures agent conversations. This means nikete's system can't answer questions like:
- "Who added this dependency edge?"
- "When was this task's description last edited?"
- "What config was the coordinator running with when this task was dispatched?"

### 5.2 No LLM Dependency for Core Logging

Our system is pure append-only logging with no LLM dependency. nikete's distillation requires LLM calls, which:
- Adds cost
- Introduces non-determinism (different distillation runs may produce different canons)
- Creates a chicken-and-egg problem for bootstrapping

### 5.3 Rotation and Long-Term Storage

Our `provenance.rs` already implements zstd-compressed log rotation with `read_all_operations()` that transparently reads across rotated files. nikete's system stores traces as flat JSONL with no rotation — trace files for long-running agents could grow very large.

### 5.4 Content-Addressed Prompt Archive (Planned)

Our design plans a content-addressed prompt store (`.workgraph/prompts/{sha256}.txt`) that enables exact prompt replay. nikete's system stores prompts within traces but doesn't deduplicate or hash them.

---

## 6. Specific Code Patterns Worth Porting

### 6.1 The `TraceEvent` Enum and Parser

From `src/trace.rs:14-60`:

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

The tagged enum with `serde(tag = "type")` is a clean pattern for serializing heterogeneous event types to JSONL. The `parse_stream_json()` function (lines 70-145) handles Claude's stream-json format with graceful fallback (skipping unparseable lines).

### 6.2 The `reset_tasks_for_replay()` Function

From `src/runs.rs:140-200`:

The selective reset logic is well-structured:
1. Determine which tasks to consider (specific IDs or all)
2. Build reverse dependency index for transitive dependent computation
3. Check keep-done threshold against evaluation scores
4. Reset selected tasks: `status → Open`, clear `assigned/started_at/completed_at/artifacts/loop_iteration`, preserve `log` and `blocked_by`

This function correctly handles the subtlety of resetting transitive dependents — if you re-run task A, you must also re-run tasks B and C that consumed A's output.

### 6.3 The `render_canon_for_prompt()` Priority Truncation

From `src/canon.rs:140-190`:

The priority-ordered section rendering with token budget is a useful pattern for context window management:
1. Spec (never truncated)
2. Tests (high priority)
3. Lessons header
4. Corrections
5. Sticking points
6. Human preferences (lowest priority)

Sections are greedily added while within budget. This hierarchical truncation approach could be generalized for our prompt builder.

### 6.4 The `snapshot()` + `restore_run()` Pattern

From `src/runs.rs:65-95, 230-245`:

Simple, reliable snapshot/restore using filesystem copy. The pattern of copying `graph.jsonl` → `graph.jsonl.snapshot` with optional recursive copy of `traces/` and `canon/` directories is straightforward and well-tested.

---

## 7. Concerns

### 7.1 Distillation Is Unfinished

The distill command builds the prompt but doesn't actually call an LLM. It prints "LLM integration not yet implemented." This means the capture → distill → replay pipeline is a 2/3 pipeline in practice. The most novel part (distillation) isn't functional yet.

### 7.2 No Integration with Our Operations Log

nikete's fork doesn't interact with our `provenance.rs` operations log at all. The two systems are completely independent. If we adopt nikete's replay system, we need to wire replay operations (snapshot, reset, restore) into the provenance log so there's a unified audit trail.

### 7.3 Duplicated `load_eval_scores()` Function

The `load_eval_scores()` function appears in both `src/runs.rs:203-230` and `src/commands/replay.rs:140-170` — identical implementations that read evaluation JSON files and extract the highest score per task. This should be factored into a shared utility (possibly in `agency.rs` alongside `load_all_evaluations_or_warn`).

### 7.4 `collect_transitive_dependents` Also Duplicated

Similarly, `collect_transitive_dependents()` is implemented in both `src/runs.rs` and `src/commands/replay.rs` (labeled `_local` in the command). This is a graph traversal utility that belongs in the core graph module.

### 7.5 Snapshot Creates Run Even for `--plan-only`

In `src/commands/replay.rs`, the `--plan-only` codepath still creates a snapshot before showing the plan. This is arguably a bug — a dry run shouldn't create side effects. The snapshot is created at line ~35 (before the `if plan_only` check at line ~75).

### 7.6 No Automatic Trace Extraction

Traces must be manually extracted with `wg trace-extract <agent-id>` after an agent completes. There's no integration with the spawn wrapper (`run.sh`) or coordinator to automatically extract traces on completion. This means traces won't exist unless someone remembers to run the extraction command.

### 7.7 `serde_yaml` Dependency for Canon

Canon files use YAML (via `serde_yaml`). The rest of workgraph uses JSON/JSONL throughout. Adding a YAML dependency for a single feature introduces format inconsistency. However, YAML's multiline string support makes it genuinely more readable for the spec/tests fields, so this is a reasonable tradeoff.

### 7.8 Timestamp Quality in `parse_stream_json()`

The parser uses `chrono::Utc::now().to_rfc3339()` as the timestamp for ALL events, meaning every event in a trace gets the same timestamp (the time of parsing, not the time of occurrence). Claude's stream-json includes timing information that could be used instead. This makes the `ts` field essentially useless for understanding timing within a conversation.

### 7.9 No Breaking Changes

The fork claims to be "purely additive" with no modifications to existing types or behaviors. Based on the diff, this is accurate — all changes are new modules, new commands, and new config sections with defaults that preserve existing behavior. Merging would not break any existing functionality.

### 7.10 Test Quality

52 new tests is solid coverage. Tests use `tempfile::TempDir` for isolation, test roundtrip serialization, edge cases (empty input, missing files, gaps in run IDs), and end-to-end command execution. The test helpers (`make_task`, `make_task_with_status`, `setup_workgraph`) suggest nikete's fork has our `test_helpers` module.

---

## 8. Recommendation Summary

| Category | Recommendation | Priority |
|----------|---------------|----------|
| Structured traces (`TraceEvent` + parser) | **Adopt** — fills a critical gap in our logging | High |
| Trace extraction CLI (`wg trace`, `wg trace-extract`) | **Adopt** — useful even without replay | High |
| Replay mechanism (snapshot + reset + re-execute) | **Adopt with integration** — wire into provenance log | High |
| Run management (`wg runs`) | **Adopt** — enables model comparison | Medium |
| Canon/distillation concept | **Adopt later** — depends on traces + replay being in place first | Medium |
| Canon YAML format | **Consider** — YAML readability vs. format consistency | Low |
| `{{task_canon}}` template variable | **Adopt with canon** — clean integration with executor templates | Medium |

The fork represents significant, well-designed work. The three-stage pipeline is architecturally sound, the code quality is good, and the 52 tests provide confidence. The main integration work would be: (1) wiring replay operations into our provenance event log, (2) automating trace extraction in the spawn wrapper, (3) fixing the duplicated utility functions, and (4) addressing the timestamp issue in the parser.
