# nikete/workgraph Fork Comparison (2026-02-20)

**Remotes compared:**
- `origin/main` (graphwork/workgraph) — our codebase
- `nikete/main` — nikete's primary branch
- `nikete/vx-adapter` — nikete's veracity exchange integration branch

**Overall diff:** 104 files changed, 71,234 insertions, 36,994 deletions (main vs nikete/main). The fork has diverged substantially.

---

## 1. Structural Diff

### Files only in nikete's fork (new modules)

| File | Lines | Purpose |
|------|-------|---------|
| `src/canon.rs` | 627 | Distilled knowledge artifacts from conversation traces (Canon struct, save/load/version, prompt rendering, distill prompt building) |
| `src/trace.rs` | 815 | Structured conversation recording — parses Claude's `stream-json` output into TraceEvent types (System, Assistant, ToolResult, User, Error, Outcome) |
| `src/commands/canon_cmd.rs` | ~100 | CLI for `wg canon <task-id>` — view canon artifacts |
| `src/commands/distill.rs` | ~150 | CLI for `wg distill <task-id>` — build distill prompts from traces |
| `src/commands/trace_cmd.rs` | ~150 | CLI for `wg trace <task-id>` and `wg trace-extract <agent-id>` |

### Files only in our fork (new modules)

| File | Lines | Purpose |
|------|-------|---------|
| `src/federation.rs` | 1,548 | Agency federation — cross-repo scanning, pulling, pushing, merging of roles/motivations/agents |
| `src/provenance.rs` | 326 | Append-only JSONL operation log with zstd-compressed rotation |
| `src/models.rs` | 414 | Model registry with cost, capability, tier metadata |
| `src/trace_function.rs` | 1,099 | Parameterized workflow templates — extract task patterns into reusable TraceFunctions with typed inputs/outputs |
| `src/commands/agency_merge.rs` | — | Federation merge command |
| `src/commands/agency_pull.rs` | — | Federation pull command |
| `src/commands/agency_push.rs` | — | Federation push command |
| `src/commands/agency_remote.rs` | — | Federation remote management |
| `src/commands/agency_scan.rs` | — | Federation scan for importable items |
| `src/commands/gc.rs` | — | Garbage collection command |
| `src/commands/models.rs` | — | Model registry CLI |
| `src/commands/setup.rs` | — | Setup wizard |
| `src/commands/trace.rs` | — | Our trace viewing command |
| `src/commands/trace_animate.rs` | — | Trace animation/visualization |
| `src/commands/trace_extract.rs` | — | Trace extraction from agent logs |
| `src/commands/trace_function_cmd.rs` | — | Trace function management CLI |
| `src/commands/trace_instantiate.rs` | — | Instantiate trace functions into new tasks |
| `src/commands/peer.rs` | — | Cross-repo peer communication (WIP) |

### Files in both but diverged

| File | Nature of divergence |
|------|---------------------|
| `src/agency.rs` | Both have the full agency system (~3,600-3,800 lines). Nikete's is slightly smaller (3,619 vs our 3,802). Our version has additional fields/methods. |
| `src/config.rs` | Nikete added `[distill]` and `[replay]` config sections (689 lines). Ours has `[log]` section for provenance rotation + more sections (1,038 lines). |
| `src/runs.rs` | Both have run management. Nikete's is larger (699 lines, includes `reset_tasks_for_replay` and trace/canon snapshots). Ours is 230 lines, focused on snapshot/restore. |
| `src/commands/replay.rs` | Both have replay. Nikete's is simpler (392 lines). Ours is richer (589 lines) with `--subgraph`, evaluation-aware keep-done. |
| `src/commands/viz.rs` | Nikete's is 1,659 lines. Ours is 2,418 lines — we added 2D graph layout (force-directed). |
| `src/graph.rs` | Minor differences — we have additional fields. |
| `src/main.rs` | Major structural differences — different CLI subcommand sets. Nikete's is 2,466 lines vs ours which is larger. |

### Test files comparison

**Only in our fork (9 test files):**
- `integration_agency_federation.rs` (2,361 lines)
- `integration_global_config.rs` (763 lines)
- `integration_logging.rs` (894 lines)
- `integration_replay_exhaustive.rs` (2,195 lines)
- `integration_runs_exhaustive.rs` (1,348 lines)
- `integration_trace_exhaustive.rs` (1,650 lines)
- `integration_trace_functions.rs` (1,562 lines)
- `integration_trace_replay.rs` (672 lines)
- `integration_cross_repo_dispatch.rs` (WIP)

**Only in nikete's fork (0 test files):** All of nikete's test files have counterparts in our codebase.

**Total:** We have 23 test files. Nikete has 14 test files.

---

## 2. Feature Comparison Table

| Capability | Nikete's approach | Our approach | Verdict |
|-----------|-------------------|--------------|---------|
| **Trace/capture** | `src/trace.rs` (815 lines) + `src/commands/trace_cmd.rs`. Parses Claude `stream-json` into `TraceEvent` enum (System, Assistant, ToolResult, User, Error, Outcome). Stores in `.workgraph/traces/<agent-id>/trace.jsonl` + `trace-meta.json`. | `src/provenance.rs` (326 lines) — append-only operation log with zstd rotation. `src/commands/trace.rs` + `trace_extract.rs` + `trace_animate.rs`. We capture at the *operation* level, not *conversation* level. | **Different granularity.** Nikete captures raw LLM conversation turns. We capture graph operations. Both are useful — they're complementary, not competing. |
| **Canon/distill** | `src/canon.rs` (627 lines) + `src/commands/distill.rs` + `src/commands/canon_cmd.rs`. LLM-assisted distillation of traces into structured YAML canons (spec, tests, interaction_patterns, quality_signals). Versioned. Prompt-injectable via `{{task_canon}}`. | `src/trace_function.rs` (1,099 lines) + CLI commands. Parameterized workflow templates — extract *task graph patterns* into reusable functions with typed inputs/outputs. Focus on structural reuse, not knowledge distillation. | **Different goals.** Nikete distills *knowledge* from conversations. We extract *structural patterns* from task graphs. His canons feed context to re-executed agents. Our trace functions create new task subgraphs. |
| **Replay** | `src/commands/replay.rs` (392 lines). Snapshot, select tasks to reset (by status, score, or ID), reset them, re-run with new model. Canon injection into prompts during replay. | `src/commands/replay.rs` (589 lines). Same core concept but richer: `--subgraph` filtering, evaluation-aware keep-done, more detailed plan output. No canon injection (we don't have canons). | **Ours is richer operationally, but nikete's has the canon injection which is the key differentiator for replay.** Without distilled knowledge fed back into re-execution, replay is just "start over." |
| **Runs** | `src/runs.rs` (699 lines). Run snapshots include graph state + optional trace/canon snapshots. `reset_tasks_for_replay` with transitive dependent reset. Run restore. | `src/runs.rs` (230 lines). Simpler snapshot/restore focused on graph state. | **Nikete's is more complete** — trace and canon snapshots alongside graph state provides better point-in-time reconstruction. |
| **Logging/provenance** | No provenance module. Uses per-task `log` entries and agent output logs. No append-only operation log. | `src/provenance.rs` — append-only JSONL operation log with zstd-compressed rotation, `record()` calls from mutating commands. `.workgraph/log/operations.jsonl`. | **Ours is better for audit/compliance.** Nikete has no operation-level logging beyond per-task logs. |
| **Agency** | Full agency system in `src/agency.rs` (3,619 lines). Same core concepts: roles, motivations, agents, skills, evaluations, evolution, lineage. | Full agency system in `src/agency.rs` (3,802 lines). Same core + additional features. | **Functionally equivalent.** Both have the same agency architecture. Ours has ~180 more lines of additional capabilities. |
| **Federation** | No federation. No cross-repo agency sharing. | `src/federation.rs` (1,548 lines) + 5 command files + `integration_agency_federation.rs` (2,361 lines). Scan, pull, push, merge of roles/motivations/agents across repos. | **Ours only.** Nikete has nothing comparable. |
| **Loop convergence** | No `--converged` flag. No signal for loop termination based on quality. | `wg done <id> --converged` flag to signal that a loop has converged and should stop iterating. | **Ours only.** |
| **Viz (2D layout)** | `src/commands/viz.rs` (1,659 lines). Basic graph visualization. | `src/commands/viz.rs` (2,418 lines). Extended with force-directed 2D graph layout. | **Ours is richer.** |
| **Trace functions** | No trace functions. No structural pattern extraction. | `src/trace_function.rs` (1,099 lines) + CLI commands. Extract task graph patterns as parameterized templates, instantiate them into new task subgraphs. | **Ours only.** |
| **Model registry** | No model registry. | `src/models.rs` (414 lines). Catalog of AI models with cost/capability/tier metadata. | **Ours only.** |
| **GC** | No garbage collection command. | `src/commands/gc.rs`. Clean up completed/archived tasks. | **Ours only.** |
| **Setup wizard** | No setup wizard. | `src/commands/setup.rs`. Interactive setup for new users. | **Ours only.** |

### VX-Adapter Branch

The `nikete/vx-adapter` branch is a significant evolution of nikete's fork:

| Change | Description |
|--------|-------------|
| **agency → identity rename** | `src/agency.rs` → `src/identity.rs`, all "agency" terminology becomes "identity" throughout. `evaluate` → `reward`. `motivation` → `objective`. |
| **Canon/distill removed** | `src/canon.rs`, `src/commands/canon_cmd.rs`, `src/commands/distill.rs` are deleted. The replay-via-distillation approach is abandoned. |
| **Trace replaced** | `src/trace.rs` deleted, replaced by `src/commands/trace.rs` (different implementation). |
| **Provenance added** | `src/provenance.rs` — append-only JSONL operation log with zstd rotation. Very similar to our implementation! |
| **Models added** | `src/models.rs` — model registry with cost/tier metadata. Similar to our `models.rs`. |
| **GC added** | `src/commands/gc.rs` — garbage collection. Similar to ours. |
| **Veracity exchange research** | Extensive docs: `veracity-exchange-deep-dive.md`, `veracity-exchange-integration.md`, `gepa-integration.md`, `logging-veracity-gap-analysis.md`. Detailed analysis of how workgraph maps to Veracity Exchange's portfolio scoring API. |
| **Logging design** | `docs/design/provenance-system.md` — comprehensive design for complete operation log coverage, agent conversation capture, artifact archival with SHA-256 hashing. |

**Key insight:** The vx-adapter branch is converging toward our architecture. It independently added provenance logging, a model registry, and GC — features we already have. It also removed the canon/distill system, suggesting nikete concluded that the LLM distillation layer was premature or too complex. The branch instead focuses on the veracity exchange integration as the primary external feedback mechanism.

---

## 3. Concept Mapping

| Nikete's term | Our term | Notes |
|---------------|----------|-------|
| `trace` (in `src/trace.rs`) | No direct equivalent | We have operation-level provenance, not conversation-level traces. Our `trace` commands exist but focus on different data. |
| `capture` | `provenance::record()` | Both capture events, but at different granularities (conversation vs operation). |
| `distill` | No equivalent | We don't have LLM-assisted knowledge extraction from traces. |
| `canon` | No equivalent | We don't have structured knowledge artifacts. Our `trace_function.rs` serves a different purpose (structural patterns, not knowledge). |
| `replay` | `replay` | Same concept, same name. Both snapshot-and-reset. |
| `runs` | `runs` | Same concept, same name. Both manage snapshot directories. |
| `agency` | `agency` | Same system. vx-adapter renames to "identity." |
| `evaluation` | `evaluation` | Same system. vx-adapter renames to "reward." |
| `motivation` | `motivation` | Same concept. vx-adapter renames to "objective." |
| `TraceEvent` | `OperationEntry` | Nikete's captures LLM conversation turns. Ours captures graph mutations. |
| `TraceMeta` | — | Summary statistics per agent trace. No direct equivalent in our codebase. |
| `Canon.spec` | — | Refined task specification from distillation. |
| `Canon.interaction_patterns` | — | Corrections, sticking points, preferences from human review. |
| `build_distill_prompt()` | — | Constructs LLM prompt for knowledge extraction. |
| `render_canon_for_prompt()` | — | Injects canon into agent prompts as `{{task_canon}}`. |

---

## 4. Integration Assessment

### What from his fork should we pull in?

1. **Conversation-level trace parsing (`src/trace.rs`).** His `parse_stream_json()` function that converts Claude's `--output-format stream-json` into structured `TraceEvent` types is genuinely useful and complementary to our operation-level provenance. We already capture *what the system did* (operations); his traces capture *what the agent said and did* (conversation turns, tool calls, user interventions). **Recommendation: Port this.**

2. **The canon/distill concept (with caveats).** The idea of distilling conversation traces into structured knowledge artifacts that feed back into re-execution is powerful. However, nikete himself seems to have backed away from this in the vx-adapter branch. The `distill` command currently just builds a prompt — it doesn't actually call an LLM. **Recommendation: Take the concept, defer the implementation until we have a use case that demands it.**

3. **Veracity exchange research (from vx-adapter).** The research documents (`veracity-exchange-deep-dive.md`, `veracity-exchange-integration.md`) contain detailed analysis of how workgraph maps to Veracity Exchange's portfolio scoring API. This is forward-looking design work that we don't have. **Recommendation: Pull in the research docs.**

4. **Provenance system design doc (from vx-adapter).** `docs/design/provenance-system.md` is a thorough design for complete operation log coverage — instrumenting all 15+ graph-mutating commands, agent conversation archival at spawn time, artifact SHA-256 hashing. We have the provenance module but his design doc covers gaps in our coverage. **Recommendation: Pull in the design doc, implement its recommendations.**

### Where did he solve something differently and better?

1. **Trace events are richer than our operation entries.** His `TraceEvent` enum captures assistant messages, tool call details (name, args, ID), user interventions with source tracking, error recoverability, and detailed outcome metadata (exit code, duration, artifacts produced). Our `OperationEntry` is a flat `{timestamp, op, task_id, actor, detail}` where `detail` is untyped JSON. His approach gives you much better replay of *what happened during an execution*.

2. **Run snapshots include traces and canons.** His `runs::snapshot()` can optionally copy trace and canon directories into the run snapshot, giving a complete point-in-time picture. Our snapshots only include the graph state.

3. **The prompt injection pipeline.** His `render_canon_for_prompt()` → `{{task_canon}}` template variable is a clean mechanism for feeding historical knowledge into agent re-execution. We have no equivalent pathway for enriching agent prompts with prior execution context.

### Where did he solve something differently and worse?

1. **No operation-level provenance (on nikete/main).** His main branch has no append-only operation log. If you want to audit what happened to the graph over time, you have to reconstruct from per-task logs. The vx-adapter branch fixes this by adding provenance.rs.

2. **Simpler replay.** His replay is ~200 lines shorter than ours, missing `--subgraph` filtering and less sophisticated task selection logic.

3. **No federation.** No way to share agency artifacts across repos. For organizations running multiple workgraph instances, this is a significant gap.

4. **No loop convergence.** No way for agents to signal "this loop has converged, stop iterating." Loops just run to max iterations.

5. **No trace functions.** No structural pattern extraction. If you want to reuse a task graph pattern, you have to manually recreate it.

6. **Fewer tests.** 14 test files vs our 23. No exhaustive tests for trace, replay, runs, logging, or federation.

### What's on the vx-adapter branch that matters?

1. **The "identity" rename** — a considered terminology shift from "agency" to "identity" that may be worth discussing. The argument: agents have an *identity* (role + objective + skills + track record), not an *agency* (which implies autonomy/power). Similarly, "evaluate" → "reward" and "motivation" → "objective" are more precise terms.

2. **Veracity exchange integration research** — detailed mappings from workgraph concepts to Veracity's portfolio positions, scoring API, public/private boundaries, and trust networks. This is the most forward-looking work in the fork.

3. **The provenance system converging on our design** — independently arrived at append-only JSONL with zstd rotation, confirming our approach is sound.

---

## 5. Merge Feasibility

### How hard would it be to merge?

**Hard.** The forks have diverged substantially with 104 files changed and 100K+ lines in the diff. This is not a clean merge situation.

### Which files would conflict?

**Virtually all shared files would conflict**, since both forks have been independently adding features. Key conflict zones:

| File | Conflict severity | Why |
|------|-------------------|-----|
| `src/main.rs` | **Severe** | Completely different CLI subcommand sets. Both added many commands. |
| `src/config.rs` | **Severe** | Different config sections (distill/replay vs log/models/etc). |
| `src/commands/mod.rs` | **Severe** | Different module declarations and exports. |
| `src/agency.rs` | **Moderate** | Same core, but diverged on details. |
| `src/runs.rs` | **Moderate** | Different RunMeta fields, different helper functions. |
| `src/commands/replay.rs` | **Moderate** | Different function signatures and logic. |
| `src/graph.rs` | **Low** | Small differences. |
| `Cargo.toml` | **Moderate** | Different dependencies. |

### Recommended strategy

**Cherry-pick specific features.** Do NOT attempt a wholesale merge. Instead:

1. **Port `parse_stream_json()`** from his `src/trace.rs`. This is the most immediately useful code — a clean parser for Claude's stream-json format. Can be added as a function in our existing trace module or as a new `trace_parser.rs`. Minimal conflict risk.

2. **Import research docs** from the vx-adapter branch. No code conflicts — just new markdown files. Pull `docs/research/veracity-exchange-deep-dive.md`, `docs/research/veracity-exchange-integration.md`, `docs/design/provenance-system.md`.

3. **Consider the canon concept** as a future feature. Don't port the code now (nikete himself removed it in vx-adapter), but keep the design in mind for when we implement knowledge-enriched replay.

4. **Evaluate the identity/reward/objective terminology** from vx-adapter. This is a naming decision that affects the whole codebase — worth discussing but not urgent.

5. **Ignore the rest.** Most of his other changes are simplified versions of things we already have or features we've gone further with (federation, trace functions, viz, convergence, models, GC, tests).

### What NOT to merge

- His `src/canon.rs` / `src/commands/distill.rs` — he removed these in vx-adapter, suggesting they weren't ready.
- His simplified `src/runs.rs` — ours is more battle-tested.
- His `src/agency.rs` — functionally equivalent, ours has more features.
- His `src/config.rs` — incompatible config structures.
- Any renaming from vx-adapter (agency→identity, etc.) — this would touch every file and break all existing deployments.

---

## Summary

Nikete's fork adds a **capture → distill → replay** pipeline that we don't have. The most valuable novel contribution is the **conversation-level trace parsing** (his `src/trace.rs`) and the **veracity exchange research** (from vx-adapter). Our fork has gone much further on **federation, trace functions, provenance, models, visualization, loop convergence, and testing**. The vx-adapter branch independently converged on several of our architectural choices (provenance logging, model registry, GC), validating our direction.

**Bottom line:** Cherry-pick his trace parser, import his research docs, and keep the canon/distill concept in our design backlog. A wholesale merge is not feasible or desirable.
