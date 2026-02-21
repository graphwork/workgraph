# nikete Fork Integration Roadmap

**Date:** 2026-02-20
**Based on:** [nikete-fork-deep-review.md](../research/nikete-fork-deep-review.md), [veracity-exchange-deep-dive.md](../research/veracity-exchange-deep-dive.md), [nikete-fork-comparison-feb20.md](../research/nikete-fork-comparison-feb20.md)
**Remotes:** `nikete/main`, `nikete/vx-adapter`

---

## 1. What to Adopt from nikete

### 1.1 `parse_stream_json()` — Conversation-Level Trace Parser

**Source:** `nikete/main:src/trace.rs` (815 lines)
**Priority:** High
**Effort:** ~200 LOC to port (parser + TraceEvent types, skip the rest)

nikete's `TraceEvent` enum and `parse_stream_json()` parser convert Claude's `--output-format stream-json` into structured events (System, Assistant, ToolResult, User, Error, Outcome). Our `parse_stream_json_stats()` in `src/commands/trace.rs` only counts tokens and tool calls — his parser preserves the full conversation structure.

**What to port:**
- The `TraceEvent` enum with all 6 variants
- The `ToolCall` struct (name, args, call_id)
- The `parse_stream_json()` function — **with the timestamp bug fixed** (use per-event timestamps from stream-json, not a single `Utc::now()`)
- `TraceMeta` summary computation (token counts, duration, tool call stats)

**Where to put it:** New file `src/trace_parser.rs` alongside our existing `src/commands/trace.rs`. Keep our operation-level provenance separate from conversation-level parsing — they serve different purposes.

**Don't port:**
- `src/commands/trace_cmd.rs` — we have our own trace CLI
- JSONL I/O — use our existing patterns
- The 13 tests — rewrite against our integration test style

### 1.2 `Reward.source` / `Evaluation.source` — Pluggable Evaluation Sources

**Source:** `nikete/vx-adapter:src/identity.rs` (the `Reward` struct)
**Priority:** High
**Effort:** ~50 LOC

The single most impactful conceptual addition. Add a `source` field to our `Evaluation` struct in `src/agency.rs`:

```rust
pub struct Evaluation {
    // ... existing fields ...
    #[serde(default = "default_eval_source")]
    pub source: String,  // "llm", "outcome:<metric>", "manual", "vx:<peer-id>"
}

fn default_eval_source() -> String { "llm".to_string() }
```

This is backward-compatible via `#[serde(default)]` — existing evaluations deserialize with `source: "llm"`. Opens the door to outcome-based scoring, manual evaluation, and future VX integration without any breaking changes.

**Also add:** `#[serde(alias = "score")]` on the score field for forward-compatibility with nikete's `value` naming.

### 1.3 Run Snapshots with Traces/Provenance

**Source:** `nikete/main:src/runs.rs` (699 lines)
**Priority:** Medium
**Effort:** ~60 LOC

nikete's `snapshot()` optionally copies trace and canon directories into run snapshots. Our `src/runs.rs` (230 lines) only snapshots graph state. Extend our snapshot to include:
- `.workgraph/log/operations.jsonl` (provenance log)
- `.workgraph/agents/*/` archives (agent prompt/output)

This gives run snapshots a complete point-in-time picture — what the graph looked like AND what happened to get there.

### 1.4 VX Research Documents

**Source:** `nikete/vx-adapter` branch
**Priority:** Medium
**Effort:** File copy

Import these research documents that we don't have equivalents for:

| Document | Lines | Value |
|----------|-------|-------|
| `docs/research/veracity-exchange-integration.md` | 489 | Gap analysis: what exists vs what VX needs |
| `docs/research/gepa-integration.md` | 426 | GEPA prompt optimization framework integration |
| `docs/research/collaborators-and-perspectives.md` | 356 | External collaborator analysis |
| `docs/research/organizational-economics-review.md` | 765 | Organizational economics applied to workgraph |
| `docs/design/provenance-system.md` | 327 | Provenance coverage audit checklist |

These are forward-looking design research with no equivalent in our codebase. The provenance design doc is immediately actionable as an audit checklist for our `provenance.rs`.

### 1.5 4-Level Sensitivity Enum (Design Concept)

**Source:** nikete's design doc (`design-veracity-exchange.md`)
**Priority:** Low (design backlog — adopt when VX work begins)
**Effort:** ~100 LOC when implemented

nikete's `Sensitivity` enum (Public, Interface, Opaque, Private) is better than our proposed 3-level Visibility. The `Interface` level — share what a task does but not its data — is a genuinely useful middle ground. Don't implement now, but adopt this design when we add task visibility controls.

---

## 2. What nikete Should Adopt from Us

These are features we've built that his fork lacks entirely, or where our implementation is substantially ahead.

### 2.1 Agency Federation (`src/federation.rs` — 1,548 lines)

Cross-repo sharing of roles, motivations, and agents. Scan, pull, push, merge with referential integrity and performance record merging. nikete's VX research docs mention "peer exchange" conceptually — federation is the infrastructure that could carry it.

**Why it matters for VX:** If nikete's vision is agents with portable track records across organizational boundaries, federation is the mechanism that makes agent identity shareable. His peer identity model (`peer_id = SHA-256(public_key)`) parallels our agent content-hash identity — federation already handles the cross-repo identity transport layer.

### 2.2 Trace Functions (`src/trace_function.rs` — 1,099 lines)

Parameterized workflow templates extracted from completed task graphs. Where nikete's canon distills *knowledge* from conversations, our trace functions extract *structure* from task graphs. These are complementary, not competing.

**Related commands:** `src/commands/trace_extract.rs` (973 lines), `src/commands/trace_function_cmd.rs`, `src/commands/trace_instantiate.rs`

### 2.3 Loop Convergence (`--converged` flag)

`wg done <id> --converged` prevents loop edges from firing. Checked in `graph.rs:evaluate_loop_edges()`. nikete's loops run to max iterations with no early termination signal.

### 2.4 2D Graph Visualization (`src/commands/viz.rs` — 2,418 lines)

Force-directed 2D box layout in terminal with Unicode box-drawing, ANSI color, loop edge annotations. nikete's viz is 1,659 lines with basic layout.

### 2.5 Trace Animation (`src/commands/trace_animate.rs` — 330 lines)

Terminal TUI animation replaying historical execution as graph snapshots. No equivalent in nikete's fork.

### 2.6 Setup Wizard (`src/commands/setup.rs` — 463 lines)

Interactive first-time configuration via `dialoguer`. nikete has no onboarding flow.

### 2.7 Operation-Level Provenance (on nikete/main)

nikete's main branch has no `provenance.rs`. His vx-adapter branch adds one (converging on our design), but main still lacks operation-level audit logging.

### 2.8 Enhanced Test Suite (23 vs 14 test files)

9 test files totaling ~11,000+ lines that nikete doesn't have, including exhaustive tests for trace, replay, runs, logging, federation, and trace functions.

---

## 3. Terminology Resolution

Both forks use mostly the same terminology, with nikete's vx-adapter branch proposing renames. Here's the proposed resolution:

| Concept | Our current term | nikete's vx-adapter term | **Proposed resolution** | Rationale |
|---------|-----------------|------------------------|------------------------|-----------|
| Agent system | `agency` | `identity` | **Keep `agency`** | More intuitive. "Agency" describes the system of agents. "Identity" is what agents *have*, not what the system *is*. |
| Agent drive | `motivation` | `objective` | **Keep `motivation`** | "Objective" is more precise academically, but "motivation" is clearer to newcomers. Not worth a breaking rename. |
| Performance score | `evaluation` / `score` | `reward` / `value` | **Keep `evaluation` / `score`** | "Reward" implies RL which is misleading for LLM-evaluated quality. Add `#[serde(alias)]` for interop. |
| Conversation capture | `trace` (conversation) | `trace` (provenance) | **`trace` = provenance ops; `conversation` = raw LLM dialog** | Both branches overload "trace." Disambiguate: `wg trace` shows provenance operations (what already happens). New conversation-level parser uses "conversation" or "session" terminology. |
| Knowledge artifact | `canon` | *(removed)* | **Defer** | nikete removed canon from vx-adapter. If we adopt it later, keep the name "canon" — it's distinctive and the VX design doc still uses it as the suggestion format. |
| Workflow template | *(doesn't exist in nikete)* | — | **`trace function`** | Our term. No conflict. |
| Graph pattern capture | `capture` | `record` | **`record`** | We already use `provenance::record()`. Consistent. |

### Serde Aliases for Interop

Add `#[serde(alias)]` annotations so YAML files from either fork can be deserialized:

```rust
// In our Evaluation struct
#[serde(alias = "value")]
pub score: f64,

#[serde(alias = "reward")]
// on the struct-level rename_all if needed
```

This enables importing nikete's vx-adapter evaluation/reward files without data migration.

---

## 4. VX Interface Contract

What does the `wg` CLI need to expose for Veracity Exchange integration to work?

### 4.1 Current Interface Points

nikete's design proposes a **CLI bridge** (not native integration). The VX exchange client is external and talks to workgraph via CLI. These are the `wg` outputs that matter:

| Command | Output Format | VX Use |
|---------|--------------|--------|
| `wg show <task-id> --json` | Task JSON with status, skills, deliverables, verify | Building challenges from tasks |
| `wg list --json` | Array of task summaries | Identifying public-eligible tasks |
| `wg evaluate show <agent-id> --json` | Evaluation records | Mapping to reward/veracity scores |
| `wg agent show <agent-id> --json` | Agent definition + performance | Peer identity substrate |
| `wg log --operations --json` | Provenance entries | Audit trail for data flow events |
| `wg replay --plan-only --json` | Replay plan without executing | Dry-run A/B comparison planning |
| `wg runs show <run-id> --json` | Run metadata + snapshot info | Historical outcome tracking |

### 4.2 Missing Interface Points (needed for VX)

These don't exist yet and would need to be added:

| Command | Purpose | Effort |
|---------|---------|--------|
| `wg veracity outcome <graph-root> --metric <m> --value <v>` | Record real-world outcome | New command (~150 LOC) |
| `wg veracity attribute <outcome-id> --method <m>` | Attribute outcome to sub-tasks | New command (~200 LOC) |
| `wg veracity scores --json` | Per-task veracity scores | New command (~100 LOC) |
| `wg veracity sensitivity <task-id> <level>` | Set task sensitivity | Needs `sensitivity` field on Task (~50 LOC) |
| `wg veracity check` | Validate sensitivity constraints | Graph constraint check (~100 LOC) |

### 4.3 Formal Interface vs. CLI Calls

**There is no formal interface.** nikete explicitly recommends against one:

> "Do not build native integration until the exchange protocol is stable and proven. Premature coupling to an unstable protocol is worse than the friction of a CLI bridge."

The contract is: VX tools call `wg` subcommands and parse `--json` output. This is intentionally loose coupling. The `--json` output format of existing commands is the de facto interface.

**Implication:** We should stabilize the `--json` output schemas for the commands listed above. Any breaking changes to JSON output format would break VX tooling. Consider adding `--json` output to commands that don't have it yet (particularly `wg evaluate show`).

---

## 5. Merge Strategy

### 5.1 Guiding Principle

**Cherry-pick, don't merge.** 104 files changed, 100K+ lines in the diff. Virtually all shared files would conflict. A wholesale merge is not feasible.

### 5.2 Concrete Steps (ordered by priority)

#### Step 1: Add `Evaluation.source` field (1 hour)

**Files:** `src/agency.rs`
**Method:** Direct edit — add `source: String` with `#[serde(default)]` to the `Evaluation` struct. Update `record_evaluation()` to accept an optional source parameter. Add `#[serde(alias = "value")]` on `score` field.

This is the highest-value, lowest-effort change. Backward-compatible. No conflicts.

#### Step 2: Port `parse_stream_json()` conversation parser (half day)

**Source:** `nikete/main:src/trace.rs`
**Method:** Create new `src/trace_parser.rs`. Cherry-pick the `TraceEvent` enum, `ToolCall` struct, and `parse_stream_json()` function. Fix the timestamp bug. Write integration tests.

**Do NOT** cherry-pick the file — the surrounding code (JSONL I/O, metadata computation, filtering) is tangled with nikete's file layout assumptions. Extract the parser logic manually.

```bash
# View the source for reference
git show nikete/main:src/trace.rs > /tmp/nikete-trace-reference.rs
# Then manually port the parser into src/trace_parser.rs
```

#### Step 3: Extend run snapshots to include provenance (half day)

**Files:** `src/runs.rs`
**Method:** Direct edit — extend `snapshot()` to copy `operations.jsonl` and agent archives. Reference nikete's `runs.rs` for the pattern but implement against our simpler `runs.rs`.

```bash
# View nikete's approach for reference
git show nikete/main:src/runs.rs | grep -A 30 "snapshot"
```

#### Step 4: Import research documents (15 minutes)

**Method:** Direct file copy from vx-adapter branch.

```bash
git show nikete/vx-adapter:docs/research/veracity-exchange-integration.md > docs/research/veracity-exchange-integration.md
git show nikete/vx-adapter:docs/research/gepa-integration.md > docs/research/gepa-integration.md
git show nikete/vx-adapter:docs/research/collaborators-and-perspectives.md > docs/research/collaborators-and-perspectives.md
git show nikete/vx-adapter:docs/research/organizational-economics-review.md > docs/research/organizational-economics-review.md
```

We already have `docs/design/provenance-system.md` — compare with nikete's version for any gaps.

#### Step 5: Stabilize `--json` output for VX-relevant commands (1 day)

**Files:** Various `src/commands/*.rs`
**Method:** Audit each command listed in §4.1. Ensure `--json` flag exists and output is well-structured. Add `--json` to commands that lack it.

### 5.3 What NOT to Do

| Action | Why not |
|--------|---------|
| Cherry-pick `src/canon.rs` | nikete removed it in vx-adapter. Premature. |
| Cherry-pick `src/trace.rs` wholesale | File is tangled with nikete's I/O patterns. Extract parser only. |
| Apply vx-adapter renames (agency→identity, etc.) | Touches every file. Breaks all existing deployments. No functional benefit. |
| Merge nikete/main or nikete/vx-adapter branches | 104 files diverged. Conflict resolution would take longer than cherry-picking. |
| Port nikete's `src/runs.rs` | Ours is simpler and more battle-tested. Take the snapshot extension idea, not his code. |
| Port nikete's `src/config.rs` | Incompatible config structures. His has `[distill]`/`[replay]`, ours has `[log]`/`[models]`/etc. |

---

## 6. Open Questions for nikete

These are things we can't resolve without his input.

### 6.1 VX Protocol Status

**Question:** What's the current state of the Veracity Exchange protocol? Is there a spec, a reference implementation, or a running instance? The vx-adapter branch has extensive design docs but zero VX protocol code.

**Why it matters:** If the protocol is still being designed, building the local foundations (outcomes, sensitivity, attribution) makes sense. If there's a running exchange, we should understand its actual API.

### 6.2 Canon's Future

**Question:** You removed `canon.rs` from vx-adapter, but your VX design doc still treats Canon as the suggestion format. Is canon coming back in a different form? Should we implement the Canon struct as a shared data format for exchange suggestions?

**Why it matters:** If suggestions ARE canons, then the canon struct needs to exist somewhere. Either we both implement it, or we agree on a shared definition.

### 6.3 Terminology Preference

**Question:** How strongly do you feel about identity/objective/reward naming? We propose keeping agency/motivation/evaluation with `#[serde(alias)]` for interop. Would that work for your side too?

**Why it matters:** If both forks use aliases, YAML files are portable. If one fork hard-renames, portability breaks.

### 6.4 Federation Interest

**Question:** Is cross-repo agency sharing (our federation system) useful for your VX vision? Peer identity in VX maps naturally to federated agent identity. We could design the federation protocol to carry VX credibility data alongside agent definitions.

**Why it matters:** Federation could be the transport layer for VX peer exchange if we design it with that in mind.

### 6.5 Convergent Code

**Question:** We both independently built `provenance.rs`, `models.rs`, and `gc.rs` with nearly identical designs. Should we agree on a canonical version of these shared modules to reduce future drift? One option: extract them into a shared `workgraph-core` crate.

**Why it matters:** If we keep diverging on shared infrastructure, future cherry-picks get harder.

### 6.6 GEPA Integration

**Question:** How mature is the GEPA prompt optimization framework? Your `gepa-integration.md` proposes it as the inner loop for role evolution. Is this implemented somewhere? What's the dependency?

**Why it matters:** If GEPA is production-ready, using it as the `wg evolve` backend (instead of our single-shot LLM call) could be a major quality improvement for role evolution.

### 6.7 Outcome Data Sources

**Question:** For the VX prototype, what are the concrete outcome data sources you're targeting? The design doc mentions Alpaca and Databento APIs for portfolio P&L. Are there non-financial outcome sources planned?

**Why it matters:** The `Evaluation.source` field we're adding needs to accommodate whatever outcome formats VX will produce. Getting the source string format right now avoids migration later.

---

## Summary

**Immediate actions (this week):**
1. Add `Evaluation.source` field — 50 LOC, backward-compatible
2. Port conversation parser from nikete's `trace.rs` — 200 LOC, new file
3. Import VX research docs — file copy

**Short-term (this month):**
4. Extend run snapshots with provenance — 60 LOC
5. Stabilize `--json` output for VX-relevant commands — audit + small fixes

**When VX work begins:**
6. Implement `wg veracity outcome` / `attribute` / `scores` commands
7. Add `sensitivity` field to Task with 4-level enum
8. Design `wg veracity challenge` / `suggest` pipeline

**Deferred:**
- Terminology renames (use `#[serde(alias)]` instead)
- Canon implementation (wait for nikete's answer on its future)
- Native VX integration (CLI bridge first, per nikete's recommendation)
