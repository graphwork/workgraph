# TB Trial Data → Agency Evolution Pipeline: Gap Analysis

**Date:** 2026-04-05  
**Task:** research-tb-agency-pipeline  
**Author:** Agent (Documenter role)

---

## 1. Executive Summary

TB trials **already generate evaluation records** that land in `.workgraph/agency/evaluations/` and are **already consumed by `wg evolve run`**. The pipeline is connected — but the connection is lossy. Critical trial metadata (condition, task type, difficulty, verify pass/fail) is **not** encoded in evaluation records, which means `wg evolve` treats all TB evaluations identically regardless of experimental condition. This flattens the multi-condition signal that makes TB data uniquely valuable for evolution.

**Status: Pipeline exists but is metadata-blind.**

---

## 2. Current Data Flow

### 2.1 Where TB Trial Evaluations Land

TB trial tasks (e.g., `tb-a-file-ops-r0`) go through the standard workgraph lifecycle:

1. **Assignment** → `.workgraph/agency/assignments/tb-a-file-ops-r0.yaml`
   - Contains: `agent_id`, `composition_id`, `mode` (Learning/ForcedExploration), `assignment_source`
   - Does NOT contain: condition, task_type, replica index

2. **FLIP Evaluation** → `.workgraph/agency/evaluations/flip-tb-a-file-ops-r0-<timestamp>.json`
   - Source: `"flip"`
   - Contains: `score`, `dimensions` (hallucination_rate, semantic_match, specificity_match, requirement_coverage), `agent_id`, `role_id`, `tradeoff_id`
   - Does NOT contain: condition, task_type

3. **LLM Evaluation** → `.workgraph/agency/evaluations/eval-tb-a-file-ops-r0-<timestamp>.json`
   - Source: `"llm"`
   - Contains: `score`, `dimensions` (correctness, completeness, efficiency, style_adherence, intent_fidelity, downstream_usability, coordination_overhead, blocking_impact), `agent_id`, `role_id`, `tradeoff_id`
   - Does NOT contain: condition, task_type

4. **Performance propagation** — `record_evaluation()` in `src/agency/eval.rs` updates:
   - Agent performance record
   - Role performance record (context_id = tradeoff_id)
   - Tradeoff performance record (context_id = role_id)
   - Role component performance records
   - Desired outcome performance record

**Coverage check:** All 84 full-sweep tasks have both FLIP and LLM evaluation records (291 total TB eval records) and all 84 have assignment records (171 total TB assignment records, some from rerun).

### 2.2 TB Results JSON (Separate from Evaluations)

TB trial results are also collected in aggregate JSON files:

| File | Trials | Fields |
|------|--------|--------|
| `tb-results-pilot-01.json` | 12 | task_id, condition, task_type, replica, status, verify_passed, flip_score, llm_eval_score, duration_s, cost_usd, tokens_in/out, context_scope, agent |
| `tb-results-full-sweep-01.json` | 84 | task_id, condition, task_type, replica, status, verify_result, flip_score, flip_dimensions, llm_score, llm_dimensions |
| `tb-results-rerun-01.json` | 24 | task_id, condition, task_type, replica, status, verify_passed, flip_score, flip_dimensions, eval_score, eval_dimensions |

These JSON files are **external to the agency system** — they live in `terminal-bench/trials/` and are not read by any `wg` command.

---

## 3. What `wg evolve run` Consumes

### 3.1 Input Data

The evolution pipeline (`src/commands/evolve/mod.rs`) loads:

```
let all_evaluations = agency::load_all_evaluations(&evals_dir)  // ALL .json in agency/evaluations/
let agents = agency::load_all_agents_or_warn(&agents_dir)       // Filter out human agents
let roles = agency::load_all_roles(&roles_dir)
let tradeoffs = agency::load_all_tradeoffs(&tradeoffs_dir)
```

It filters out evaluations from human agents, then passes everything to either:
- **Single-shot mode** (< 50 evaluations): Builds a prompt with `build_performance_summary()` and calls an LLM directly
- **Fan-out mode** (≥ 50 evaluations): Partitions evaluations into per-strategy `AnalyzerSlice`s and creates a task graph

### 3.2 How Evaluations Are Summarized for the Evolver

`build_performance_summary()` in `src/commands/evolve/prompt.rs` produces:

1. **Per-role summary**: avg_score, task_count, generation, component_ids, aggregated dimensions
2. **Per-tradeoff summary**: avg_score, task_count, generation
3. **Synergy matrix**: (role_id × tradeoff_id) → avg_score, count

The evolver sees **aggregate scores per role and tradeoff** — it does NOT see:
- Which evaluations came from TB trials vs. normal workgraph tasks
- What experimental condition (A/C/D/E) a trial ran under
- What task type (file-ops, debugging, algorithm, etc.) was being tested
- Whether verify passed or failed
- Duration, cost, or token usage

### 3.3 Partition Strategy (Fan-out Mode)

`partition_evaluations()` in `src/commands/evolve/partition.rs` splits evaluations by strategy type (mutation, crossover, gap-analysis, retirement, etc.) with a MAX_EVALS_PER_SLICE=400 cap. Partitioning is by **strategy**, not by data source or condition.

### 3.4 Evolution Triggering

The auto-evolver (`src/agency/evolver.rs`) triggers on:
- **Threshold**: New evaluation count ≥ `evolution_threshold` (default: 10)
- **Reactive**: Recent average score < `evolution_reactive_threshold` (default: 0.4)
- **Interval**: Minimum time between cycles (`evolution_interval`, default: 7200s)

TB evaluations count toward these thresholds indistinguishably from normal workgraph evaluations.

---

## 4. Gap Analysis: What's Missing

### Gap 1: No Condition Metadata in Evaluation Records (CRITICAL)

The `Evaluation` struct has no field for experimental condition, task type, or any TB-specific metadata:

```rust
pub struct Evaluation {
    pub id: String,
    pub task_id: String,       // "tb-a-file-ops-r0" — condition is ENCODED in the ID but not parsed
    pub agent_id: String,
    pub role_id: String,
    pub tradeoff_id: String,
    pub score: f64,
    pub dimensions: HashMap<String, f64>,
    pub notes: String,
    pub evaluator: String,
    pub timestamp: String,
    pub model: Option<String>,
    pub source: String,        // "llm", "flip", etc.
}
```

The task_id `tb-a-file-ops-r0` encodes `condition=A`, `task_type=file-ops`, `replica=0`, but this is a naming convention, not a structured field. The evolution pipeline never parses it.

**Impact:** The evolver can't differentiate "this role scored 0.91 on file-ops under condition A" from "this role scored 0.68 on algorithm under condition D". All scores are averaged together.

### Gap 2: No Verify Pass/Fail in Evaluation Records (MODERATE)

TB trials track verify_passed/verify_result, but this doesn't flow into the `Evaluation` record. The evaluator prompt *does* receive FLIP score and verify findings, but these aren't persisted as first-class fields — only as dimensions (intent_fidelity) or embedded in notes.

**Impact:** Evolution can't weight "tasks where verify passed" differently from "tasks where verify failed" in aggregate analysis.

### Gap 3: TB Results JSON Disconnected from Agency System (LOW)

The `terminal-bench/trials/tb-results-*.json` files contain rich per-trial data (condition, task_type, replica, duration, cost, tokens) but are **completely external** to the `wg` tool. They're manually-collected aggregate reports, not inputs to any automated pipeline.

**Impact:** The richest cross-condition comparison data is only available through manual analysis, not through `wg evolve`.

### Gap 4: Evaluation Struct Has No Tags/Metadata Field (STRUCTURAL)

Unlike assignment records (which have `mode`, `assignment_source`), evaluations have no generic metadata or tags field. Adding condition/task-type support would require either:
- Adding new fields to the `Evaluation` struct (breaking change for existing records)
- Using `dimensions` as a vehicle for non-numeric metadata (abuse of type)
- Parsing the task_id convention (fragile, TB-specific)

### Gap 5: Same Role May Perform Differently Across Conditions (ANALYTICAL)

The core TB insight is that condition changes (prompt scaffolding, tool availability, verification loops) dramatically affect agent performance even with the same role+tradeoff. From the full-sweep data:

| Condition | Mean FLIP | Mean LLM |
|-----------|-----------|----------|
| A (bare)  | 0.134     | 0.736    |
| C (wg+skills) | 0.697 | 0.862   |
| D (autopoietic) | 0.755 | 0.898  |
| E (org-gen) | 0.795   | 0.889    |

The same role paired with condition D infrastructure scores 0.755 FLIP vs. 0.134 FLIP under condition A. But evolution can't see this — it sees one averaged score per role.

---

## 5. Proposed Pipeline Design: TB → Evolution

### 5.1 Approach: Structured Tags on Evaluations

Add an optional `tags` field to `Evaluation`:

```rust
pub struct Evaluation {
    // ... existing fields ...
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, String>,  // e.g., {"condition": "A", "task_type": "file-ops", "source_bench": "tb"}
}
```

This is backward-compatible (serde default) and generic enough for future use beyond TB.

### 5.2 Write Tags at Evaluation Time

When `wg evaluate` runs on a TB trial task, extract metadata from the task_id convention:

```
tb-{condition}-{task_type}-r{replica}
```

Or better: TB trial creation (`implement-tb-trials`) should tag tasks with metadata, and `wg evaluate` should copy task tags into the evaluation record.

### 5.3 Condition-Aware Evolution Prompt

Modify `build_performance_summary()` to include a per-condition breakdown when tagged evaluations are present:

```
### Role Performance by Condition

- **Programmer** (role c544fcb1):
  - Condition A: 3 evals, avg=0.83, FLIP=0.12
  - Condition C: 3 evals, avg=0.89, FLIP=0.71
  - Condition D: 3 evals, avg=0.91, FLIP=0.76
```

This lets the evolver make condition-aware decisions: "This role needs better prompt scaffolding (low FLIP under A) but performs well with verification infrastructure (high FLIP under D)."

### 5.4 Verify-Weighted Scoring

Given that FLIP is known to be unreliable (consistently low scores even on correct work), the evolution pipeline should:

1. **Deprioritize FLIP for aggregate scoring** — use `source: "llm"` evaluations as the primary signal
2. **Weight verify pass/fail heavily** — add a `verify_result` dimension (1.0 for pass, 0.0 for fail) that evolution can key on
3. **Use FLIP as a secondary signal** — only meaningful for comparing conditions, not absolute quality

### 5.5 TB Import Command (Optional)

For retrospective analysis, a `wg tb import <results.json>` command could:

1. Read a TB results JSON file
2. For each trial, find the existing evaluation records by task_id
3. Backfill tags (condition, task_type, replica) onto existing records
4. Or create new synthetic evaluation records tagged appropriately

This would unlock evolution on the existing 84-trial dataset without re-running experiments.

### 5.6 Multi-Condition Evolution Strategy

A new evolution strategy `condition-analysis` could:

1. Partition evaluations by condition tag
2. Identify roles/tradeoffs that perform well under some conditions but not others
3. Propose mutations targeted at the weak-condition gap
4. Generate roles specialized for specific execution environments

---

## 6. Immediate Recommendations

### For the Downstream `tb-evolution-cycle` Task

1. **The pipeline already works** — running `wg evolve run` will consume the 291 TB evaluation records alongside the 3,774 non-TB evaluations. No adapter is needed for basic functionality.

2. **For condition-aware evolution**, the simplest path is:
   - Parse task_id convention (`tb-{condition}-{task_type}-r{replica}`) in the evolve prompt builder
   - Add a "TB Condition Analysis" section to the performance summary
   - This requires zero struct changes — just prompt engineering

3. **Verify pass/fail** is already available via the `.verify-{task_id}` task status. The evaluator prompt already receives this. The gap is only in the aggregate evolution prompt.

4. **FLIP should be deprioritized** — the evolution impact report shows FLIP improvements are not statistically significant. Focus evolution on LLM eval scores and verify pass rates.

### Code Changes Needed

| Change | Effort | Impact |
|--------|--------|--------|
| Add `tags: HashMap<String, String>` to `Evaluation` struct | Small | Enables future structured metadata |
| Parse `tb-*` task_id pattern in `build_performance_summary()` | Small | Condition-aware evolution immediately |
| Add verify_result to evolution prompt summary | Small | Weight verify pass/fail in evolution |
| `wg tb import` command for backfilling tags | Medium | Retroactive condition tagging |
| New `condition-analysis` evolution strategy | Large | Full multi-condition evolution pipeline |

---

## 7. Data Inventory

| Metric | Count |
|--------|-------|
| Total evaluations in `.workgraph/agency/evaluations/` | ~4,356 |
| TB-specific evaluations (eval-tb-*) | 291 |
| TB FLIP evaluations | 146 |
| TB LLM evaluations | 145 |
| TB assignment records | 171 |
| TB trial tasks in full sweep | 84 |
| TB conditions tested | 4 (A, C, D, E) |
| TB task types tested | 7 (file-ops, text-processing, debugging, shell-scripting, data-processing, algorithm, ml) |
| Evolver state file | Does not exist (no prior auto-evolution) |

---

## Appendix A: Evaluation Record Format (Current)

```json
{
  "id": "eval-tb-a-file-ops-r0-2026-04-05T03-42-24.050203461+00-00",
  "task_id": "tb-a-file-ops-r0",
  "agent_id": "28f5ef63...",
  "role_id": "c544fcb1...",
  "tradeoff_id": "2dc69b33...",
  "score": 0.91,
  "dimensions": {
    "completeness": 1.0,
    "efficiency": 0.8,
    "style_adherence": 0.9,
    "intent_fidelity": 0.03,
    "downstream_usability": 1.0,
    "coordination_overhead": 0.9,
    "blocking_impact": 0.9,
    "correctness": 1.0
  },
  "notes": "...",
  "evaluator": "claude:haiku",
  "timestamp": "2026-04-05T03:42:24.050203461+00:00",
  "model": "claude-opus-4-latest",
  "source": "llm"
}
```

## Appendix B: TB Results JSON Format (External)

```json
{
  "task_id": "tb-a-file-ops-r0",
  "condition": "A",
  "task_type": "file-ops",
  "replica": 0,
  "status": "done",
  "verify_result": "passed",
  "flip_score": 0.03,
  "flip_dimensions": {
    "hallucination_rate": 0.95,
    "semantic_match": 0.0,
    "specificity_match": 0.1,
    "requirement_coverage": 0.0
  },
  "llm_score": 0.91,
  "llm_dimensions": {
    "completeness": 1.0,
    "efficiency": 0.8,
    "style_adherence": 0.9,
    "intent_fidelity": 0.03,
    "downstream_usability": 1.0,
    "coordination_overhead": 0.9,
    "blocking_impact": 0.9,
    "correctness": 1.0
  }
}
```

## Appendix C: Assignment Record Format

```yaml
task_id: tb-a-file-ops-r0
agent_id: 28f5ef63d156a3fd83e32b62332751747295e8b9e572eece0b92cc032b49c4d0
composition_id: 28f5ef63d156a3fd83e32b62332751747295e8b9e572eece0b92cc032b49c4d0
timestamp: 2026-04-05T03:34:16.241958366+00:00
mode:
  type: learning
  dimension:
    type: novel_composition
  bizarre_ideation: false
assignment_source:
  type: native
```
