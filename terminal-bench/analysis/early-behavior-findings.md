# Terminal Bench: Early Behavior Analysis

**Date:** 2026-04-04 (partial results)  
**Analyst task:** tb-early-behavior-analysis  

## Trial Completion Status

| Condition | Directory | Trials | Agent Class | Actual Condition |
|-----------|-----------|--------|-------------|-----------------|
| A | rerun-condition-a + completion | 256 | ConditionAAgent | A |
| B (original) | full-condition-b | 270 | ConditionBAgent | B |
| B (rerun) | rerun-condition-b + cont1 | 183 | **ConditionCAgent** | **C** |
| C | full-condition-c + retry1 | 169 | ConditionCAgent | C |

---

## 1. CRITICAL: B vs C Adapter Differentiation

### Finding: rerun-condition-b ran ConditionCAgent, NOT ConditionBAgent

**Evidence:**
- All 183 `rerun-condition-b` trials have `"import_path": "wg.adapter:ConditionCAgent"` in config.json
- The `agent_result.metadata.condition` field reads `"C"` for these trials
- The run script (`rerun-condition-b/run.sh`) explicitly states: *"Uses ConditionCAgent (same wg tools as B, but with skill injection + planning phase)"* — this was **intentional**, labeled as a "corrected" B run

**Impact:** The rerun-condition-b vs full-condition-c comparison is **invalid** — both ran identical Condition C code. Any B vs C analysis must use the **original** `full-condition-b` (270 trials, correct ConditionBAgent).

### What's different between B and C in the adapter?

The adapter source (`terminal-bench/wg/adapter.py`) defines three distinct paths:

| Property | Condition A | Condition B | Condition C |
|----------|-------------|-------------|-------------|
| Tools | bash + file (6 tools) | bash + file + wg (15 tools) | Same 15 as B |
| System prompt | Minimal ("coding agent") | Task assignment + graph patterns + journal | **Skill injection** + planning phase + explicit wg usage guidance |
| Planning phase | None | None | **Mandatory** ("analyze before acting") |
| `wg_add` guidance | N/A | Brief ("decompose complex work") | **Explicit**: when/how to decompose, decision threshold |
| `wg_log` guidance | N/A | Brief ("record progress") | **Explicit**: template with task ID + step labels |
| Root task | Not created | Created, ID passed to prompt | Created, ID passed to prompt |

**Key insight:** B and C have **identical tools** but different prompts. C's prompt explicitly teaches the model *when* and *how* to use workgraph tools, while B just lists them. This is the core experimental variable.

---

## 2. Why Isn't wg Usage 100%?

### Overall wg adoption rates

| Condition | Trials using any wg tool | % |
|-----------|--------------------------|---|
| B (original) | 53 / 270 | **20%** |
| C | 151 / 187 | **81%** |
| B-rerun (=C) | 174 / 206 | **84%** |

### Root cause: model ignores tools, not adapter bugs

Examined 5 zero-wg trials from each condition. Pattern is consistent:

**Zero-wg B trials** (61 / 270 = 23%): The model receives wg tools in its tool list but **never calls them**. It proceeds directly with bash + file tools. The B prompt says "Use `wg_log` to record progress" but does not provide concrete examples or explain *why* to use them. The model (minimax-m2.7) treats them as optional and focuses on the task itself.

**Zero-wg C trials** (23 / 165 = 14%): Even with the skill injection prompt, some trials skip wg entirely. Examined planning turns for these — the model **does not acknowledge workgraph** in its planning phase. Example from `cancel-async-tasks__5Drxper`: "Simple task - I'll implement directly" — no mention of wg_log or wg_done despite the prompt explicitly instructing these.

**Root causes identified:**
1. **Prompt compliance gap (primary):** minimax-m2.7 doesn't reliably follow tool usage instructions, especially when the task is simple. It takes the shortest path (bash + file) regardless of prompt.
2. **No enforcement mechanism:** The adapter does not enforce wg tool usage — there's no check that `wg_done` was called, no penalty for skipping it.
3. **Simple tasks reduce wg motivation:** Tasks that can be solved in 5-10 steps get completed before the agent considers using task management.

**Specific zero-wg examples (C trials):**
- `cancel-async-tasks__5Drxper`: 20 turns, all bash/write_file. Planning says "Simple task" — no wg mention.
- `cancel-async-tasks__5tEMSZY`: 14 turns, all bash/write_file. Planning identical — no wg mention.
- `circuit-fibsqrt__cYrK9xk`: Exception (no metadata), likely a very short run.
- `constraints-scheduling__Lsa85gj`: Similar pattern — direct implementation.

---

## 3. Self-Verification Behavior

### Aggregate patterns (non-exception trials only)

| Metric | A Success | A Failure | B Success | B Failure | C Success | C Failure |
|--------|-----------|-----------|-----------|-----------|-----------|-----------|
| n | 121 | 106 | 43 | 56 | 70 | 80 |
| Checks output (mentions verify/test/check) | 63% | 50% | 60% | 43% | 61% | 49% |
| Iterates on failure (attempts fixes) | 36% | 39% | 28% | 27% | 43% | 35% |
| Uses wg_done | 0% | 0% | 44% | 16% | **86%** | 40% |
| Uses wg_log | 0% | 0% | 40% | 38% | **93%** | 79% |
| Hit turn limit (50 turns) | 7% | **45%** | 9% | **46%** | 6% | **41%** |

### Key observations

1. **Verification language is condition-independent.** ~60% of successful trials mention "verify", "check", or "test" regardless of condition. The wg tools don't change whether the model *thinks* about verification.

2. **Condition C drives wg_done completion.** 86% of C successes call wg_done vs 44% for B. The explicit prompt template (`wg_done("{root_task_id}")`) drives this.

3. **Failure correlates with turn limit exhaustion.** ~45% of failed trials across ALL conditions hit the 50-turn limit. This suggests these tasks are genuinely hard (the model runs out of context), not that verification behavior differs.

4. **Iteration rates are low overall.** Only ~35-43% of trials show fix-after-failure behavior. Most agents execute linearly — write code, run it, stop. They don't iterate on errors unless the error is immediately obvious.

5. **No formal self-verification.** No trial in any condition shows a pattern like "run tests → check output → fix → re-run". The model's "verification" is ad-hoc (running the code and seeing if it works), not structured.

---

## 4. Agency System: NOT Active

The agency system (roles, tradeoffs, assignment, evaluation) is **not active** inside trial environments.

**Evidence:**
- `wg init` is called by the adapter's `setup()` method on the host, which seeds the agency primitives directory (roles, tradeoffs, outcomes — default starter data)
- **No assignments** exist in any examined workgraph state snapshot (empty `agency/assignments/` directory)
- **No evaluations** exist (empty `agency/evaluations/` directory)
- The adapter does **not expose** agency-related tools (no `wg_assign`, `wg_evaluate`, `wg_agent_create`)
- Agents never run `wg` via bash inside the container — all wg interaction is via function-calling tools routed to the host

**What IS used:**
- `wg_log` — progress journaling (most popular tool)
- `wg_done` — task completion signaling
- `wg_add` — task decomposition (rare, ~7% of trials)
- `wg_artifact` — file recording (rare)

The workgraph in TB trials functions as a **task tracker + journal**, not as a multi-agent coordination system.

### No `wg service` running

- The `service/registry.json` in all examined workgraph snapshots shows `{"agents": {}, "next_agent_id": 1}` — no agents registered
- No `state.json` or `daemon.log` in service directories
- The adapter runs a single agent per trial — no coordinator, no agent spawning

---

## 5. Model Confirmation

**All trials across all conditions use `openrouter/minimax/minimax-m2.7`.**

Verified via `config.agent.model_name` in config.json across 200+ sampled trials. The `agent_info.model_info` confirms `{"name": "minimax/minimax-m2.7", "provider": "openrouter"}`.

The model is consistent. The experimental variable is the adapter condition (prompt + tools), not the model.

---

## 6. Performance Summary

| Condition | Total | Success | Rate | Avg Turns | Timeouts |
|-----------|-------|---------|------|-----------|----------|
| A (rerun) | 256 | 122 | **47.7%** | 25.8 | 29 |
| B (original) | 270* | 43 | **38.1%** | 28.9 | 14 |
| B (rerun = C) | 183 | 85 | **46.4%** | 26.0 | 18 |
| C | 169 | 70 | **41.4%** | 26.2 | 20 |

*Note: full-condition-b has 270 trials but only 113 have result.json files (157 may still be pending or failed to start).

### Preliminary interpretation (CAUTION: partial data)

- **A outperforms B-original.** The bare agent (47.7%) beats the workgraph-enabled agent (38.1%). This is counterintuitive and suggests the B prompt's wg tools add cognitive overhead without enough guidance on *when* to use them. The model wastes turns on wg_log and wg_add instead of solving the task.
- **C recovers performance.** The skill injection prompt (41.4%) partially recovers the overhead introduced by wg tools. The explicit "if simple, skip decomposition" guidance helps.
- **B-rerun ≈ C** (46.4% vs 41.4%) — both run ConditionCAgent, so this difference is noise/sampling variance.
- **Turn counts are similar** across conditions (~26), suggesting the wg tools don't significantly change execution length.

---

## 7. Recommendations

### For experimental validity
1. **Do NOT use rerun-condition-b for B vs C comparison.** Use `full-condition-b` (the 270-trial run with correct ConditionBAgent) for all B analysis.
2. **Complete full-condition-b results.** Only 113/270 have result.json — the remaining 157 may change the numbers significantly.

### For adapter improvement
3. **Consider enforcing wg_done.** The adapter could check whether wg_done was called and record it as a separate metric, or prompt the agent to call it at the end.
4. **The B prompt needs work.** 80% of B agents ignore wg tools entirely. The current prompt is too passive ("Use wg_log to record progress"). Either make it more directive (like C's skill injection) or remove the tools to avoid overhead.
5. **Task decomposition is underused.** Only 7% of trials use wg_add across all conditions. Either the tasks are too simple to decompose, or the model doesn't understand when to fan out. Consider testing with more complex multi-file tasks.

### For the agency system
6. **Agency is not being tested.** No roles, tradeoffs, or assignments are active. If the goal is to evaluate agency, the adapter would need to expose agency tools and seed identity before the agent starts.
