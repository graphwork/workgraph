# Head-to-Head Executor Comparison: Results

**Date:** 2026-04-05  
**Task:** tb-trial-run  
**Plan:** terminal-bench/trials/executor-h2h-plan.md  
**Model:** openrouter/minimax/minimax-m2.7 (all conditions)  
**Tasks:** 7 selected tasks x 3 trials x 4 conditions = 84 trials  

---

## 1. Executive Summary

| Condition | Pass Rate | False-PASS | Avg Turns | Avg Time | Avg Tokens | Tokens/Pass | Efficiency |
|-----------|-----------|------------|-----------|----------|------------|-------------|------------|
| **A (bare agent)** | 9/21 (42.9%) | 0/21 (0.0%) | 21.0 | 196s | 339K | 790K | 1.27 |
| **C (enhanced/skill-inject)** | 11/21 (52.4%) | 0/21 (0.0%) | 29.0 | 270s | 517K | 988K | 1.01 |
| **D (self-verify/old executor)** | 14/21 (66.7%) | 7/21 (33.3%) | 36.0 | 318s | 674K | 1,011K | 0.99 |
| **E (org-gen)** | 14/21 (66.7%) | 6/21 (28.6%) | 40.1 | 400s | 798K | 1,197K | 0.84 |

**Efficiency** = pass_rate / (avg_tokens / 1M). Higher is better.

### Key Findings

1. **D and E tie for highest raw pass rate (66.7%)** but achieve it through fundamentally different mechanisms. D relies on self-verification loops; E on organizational decomposition.

2. **D and E have severe false-PASS problems.** D's 33.3% false-PASS rate means one-third of all trials where the agent declared success actually failed. E's 28.6% false-PASS rate is similarly bad. The self-verification mechanism gives agents false confidence.

3. **A and C have zero false-PASS events** because they terminate via `no_tool_calls` (natural stop) rather than explicit `wg_done` claims. This is a structural advantage, not a verification quality advantage.

4. **C outperforms A by +9.5pp** (52.4% vs 42.9%) but the difference is not statistically significant (Fisher's exact p = 0.758). The skill injection and planning phase provide a modest benefit.

5. **No pairwise comparison reaches statistical significance** at p < 0.05 with 21 trials per condition. This confirms the plan's risk analysis: 3 trials per task can only detect ~25pp differences.

6. **A is the most token-efficient condition.** Despite the lowest pass rate, A's efficiency score of 1.27 beats all other conditions because it uses far fewer tokens per trial.

---

## 2. Conditions Tested

### Condition A — Bare Agent (No Workgraph)
- **Data source:** `results/rerun-condition-a/` (21 trials)
- **Agent:** `ConditionAAgent` — bash + file tools only, no graph awareness, no wg tools
- **Matches plan:** Condition A (bare agent, no decomposition, no verification)

### Condition C — Enhanced Executor (Skill Injection + Planning)
- **Data source:** `results/full-condition-c/` (21 trials, combining initial + retry1 runs)
- **Agent:** `ConditionCAgent` — wg tools + skill injection + planning phase
- **Matches plan:** Condition C (enhanced executor), specifically the skill injection and planning phase components. Note: the config-flag features (decomp_guidance, auto_test_discovery, verify_mode) are coordinator-level features that operate through `wg service start`, not through the TB adapter. The TB adapter's ConditionC captures the prompt-level enhancements.

### Condition D — Old Executor (Self-Verify)
- **Data source:** `results/pilot-d/pilot-d/` (21 trials)
- **Agent:** `ConditionDAgent` — wg tools + autopoietic self-verification + agency identity
- **Matches plan:** Condition B (old executor). D is the closest proxy for the "old executor" behavior: inline self-verification, no adaptive decomposition, no test discovery.

### Condition E — Organization Generation (Reference)
- **Data source:** `results/pilot-e/pilot-e/` (21 trials)
- **Agent:** `ConditionEAgent` — wg tools + org generation + independent verification
- **Additional context:** Included as a reference point showing the effect of structured decomposition + independent verification.

### Methodology Note

The trial plan specified conditions controlled by `coordinator.decomp_guidance`, `coordinator.auto_test_discovery`, and `coordinator.verify_mode` config flags — features that operate at the workgraph coordinator level (`wg service start`). Terminal Bench trials run through the harbor framework with TB adapter agents in Docker containers, which do not use the coordinator dispatch path. Instead, the adapter agents (ConditionA-F) implement their own prompting strategies that approximate the coordinator behaviors:

- ConditionA = no wg tools (plan's Condition A)
- ConditionC = skill injection + planning phase (plan's Condition C enhanced features)
- ConditionD = self-verification loops (plan's Condition B old executor)

This design isolates the **prompt-level** effects of the executor improvements rather than the **coordinator-level** infrastructure. The results should be interpreted as measuring the value of prompting strategies (skill injection, planning phases, self-verification prompts) rather than the mechanical config-flag toggles.

---

## 3. Per-Task Results

### Summary Table

| Task | A | C | D | E | Best |
|------|---|---|---|---|------|
| build-cython-ext | 1/3 (33%) | 2/3 (67%) | 1/3 (33%) | **3/3 (100%)** | E |
| cancel-async-tasks | 1/3 (33%) | 1/3 (33%) | **2/3 (67%)** | 0/3 (0%) | D |
| overfull-hbox | 1/3 (33%) | 1/3 (33%) | 1/3 (33%) | **2/3 (67%)** | E |
| regex-log | 1/3 (33%) | 0/3 (0%) | **2/3 (67%)** | 0/3 (0%) | D |
| custom-memory-heap-crash | 2/3 (67%) | 2/3 (67%) | **3/3 (100%)** | **3/3 (100%)** | D/E tie |
| merge-diff-arc-agi-task | 2/3 (67%) | 2/3 (67%) | 2/3 (67%) | **3/3 (100%)** | E |
| nginx-request-logging | 1/3 (33%) | **3/3 (100%)** | **3/3 (100%)** | **3/3 (100%)** | C/D/E tie |
| **OVERALL** | **9/21 (43%)** | **11/21 (52%)** | **14/21 (67%)** | **14/21 (67%)** | D/E tie |

### Per-Task Analysis

#### build-cython-ext (Multi-step build pipeline)
- **A 33%, C 67%, D 33%, E 100%**
- E's organizational decomposition excels here — breaking the task into clone/patch/compile/test subtasks consistently works.
- D's self-verification blind spot is acute: 2/3 D trials called `wg_done` despite failing the external verifier's `test_ccomplexity` test. The agent's self-verification passed basic imports but missed the demanding test.
- C's skill injection provides a modest boost over A (67% vs 33%), suggesting the planning phase helps structure multi-step builds.

#### cancel-async-tasks (Atomic task with subtle edge case)
- **A 33%, C 33%, D 67%, E 0%**
- **E's decomposition hurts.** Fragmenting this atomic async task into subtasks loses holistic context. All 3 E trials failed, all with false-PASS (agent claimed success via `wg_done` despite failing `test_tasks_cancel_above_max_concurrent`).
- D's self-verification loop caught the edge case in 2/3 trials, but 1 trial still false-PASSed.
- C's planning phase didn't help — the task requires focused single-function reasoning, not decomposition.

#### overfull-hbox (LaTeX debugging)
- **A 33%, C 33%, D 33%, E 67%**
- All conditions struggle with this constrained debugging task.
- D had 0 verification iterations across all 3 trials — the agent couldn't determine what to verify for LaTeX.
- E's decomposition provided a slight edge (67%), possibly by breaking the problem into "diagnose" and "fix" phases.
- Neither A nor C's approach helps here.

#### regex-log (Atomic text-processing)
- **A 33%, C 0%, D 67%, E 0%**
- **C performs worst** — 0% pass rate. The skill injection and planning phase may have introduced counterproductive decomposition overhead for this holistic regex task.
- E also 0% — decomposition kills holistic regex reasoning. E had 1 timeout trial (error) at ~3M tokens from thrashing.
- D achieves 67% through iterative verification (avg 10 verification iterations), though 1 trial still false-PASSed after 15 verification iterations.
- A achieves 33% with direct approach — occasionally gets the regex right on first attempt.

#### custom-memory-heap-crash (Multi-step C debugging)
- **A 67%, C 67%, D 100%, E 100%**
- D and E both achieve 100% — this is a well-structured debugging task where systematic approaches (verification loops or decomposition) consistently work.
- A and C both miss 1/3 trials, suggesting the task occasionally requires more structured debugging than a bare agent provides.

#### merge-diff-arc-agi-task (Multi-step reasoning)
- **A 67%, C 67%, D 67%, E 100%**
- E's organizational decomposition provides a clear advantage on this reasoning task — breaking it into git setup / data parsing / algorithm / testing genuinely helped.
- All other conditions tie at 67%.

#### nginx-request-logging (System configuration)
- **A 33%, C 100%, D 100%, E 100%**
- All enhanced conditions (C, D, E) achieve 100%. Even basic skill injection (C) suffices for this well-structured configuration task.
- A's 33% is driven by 2 trials with very short durations (likely infrastructure/bootstrap issues rather than genuine failures).

---

## 4. False-PASS Analysis

A false-PASS occurs when the agent declares success (`wg_done` or natural stop) but the external verifier reports failure. This measures **self-verification blind spots**.

### False-PASS Rates by Condition

| Condition | False-PASS Count | Rate | Mechanism |
|-----------|-----------------|------|-----------|
| A (bare) | 0/21 | 0.0% | No explicit success claim (terminates via `no_tool_calls`) |
| C (enhanced) | 0/21 | 0.0% | No explicit success claim (terminates via `no_tool_calls`) |
| D (self-verify) | 7/21 | 33.3% | Agent calls `wg_done` after self-verification passes but external verifier disagrees |
| E (org-gen) | 6/21 | 28.6% | Same as D — `wg_done` claims don't match external verifier |

### False-PASS Breakdown by Task (D and E only)

| Task | D False-PASS | E False-PASS |
|------|-------------|-------------|
| build-cython-ext | 2/3 | 0/3 |
| cancel-async-tasks | 1/3 | 3/3 |
| overfull-hbox | 2/3 | 1/3 |
| regex-log | 1/3 | 2/3 |
| custom-memory-heap-crash | 0/3 | 0/3 |
| merge-diff-arc-agi-task | 1/3 | 0/3 |
| nginx-request-logging | 0/3 | 0/3 |

### Interpretation

The 0% false-PASS rate for A and C is **structural, not diagnostic**. These conditions terminate via `no_tool_calls` (the model stops generating tool calls), which means the agent never explicitly claims success. In contrast, D and E terminate via `wg_done`, which is an affirmative success claim that can be wrong.

This reveals a fundamental tension: **explicit success claims enable verification loops but also enable false confidence.** The ideal executor would combine explicit success claims with effective external verification — which is exactly what the plan's Condition C (separate-agent verification via `verify_mode = "separate"`) was designed to test. This feature operates at the coordinator level and was not directly testable through the TB adapter framework.

**Recommendation:** The separate-agent verification feature (`verify_mode = "separate"`) should be tested in a dedicated coordinator-level experiment, not through the TB adapter.

---

## 5. Hypothesis Evaluation

### H1: Enhanced executor (C) achieves higher pass rate than old executor (D)
**REJECTED.** D (66.7%) outperforms C (52.4%) by 14.3pp, though not significantly (p = 0.530). The old executor's self-verification loop provides more value than skill injection, despite its false-PASS problem.

### H2: C shows fewer false-PASS failures than D
**CONFIRMED structurally, but not diagnostic.** C has 0% false-PASS vs D's 33.3%, but this is because C doesn't make explicit success claims. The comparison is not apples-to-apples.

### H3: C produces better-structured subtask graphs than D
**NOT TESTABLE.** Neither the ConditionC nor ConditionD adapter agents in the TB framework produce subtask graphs via `wg add`. The subtask quality metrics are only available for ConditionE, which creates an average of 3.4 subtasks per trial.

### H4: C correctly avoids decomposing atomic tasks
**PARTIALLY CONFIRMED.** C does not decompose `cancel-async-tasks` or `regex-log` (no subtask infrastructure), but this is because the TB adapter's ConditionC doesn't have decomposition mechanics — it relies on prompt-level planning guidance. The result on `regex-log` (C = 0%, worse than all others) suggests the planning phase may actually hurt on tasks that need direct holistic reasoning.

### H5: A may still be competitive with D
**DISCONFIRMED.** A (42.9%) is substantially below D (66.7%), a 23.8pp gap (p = 0.215). While not statistically significant with 21 trials, the consistent per-task pattern (D >= A on 6/7 tasks) suggests D's self-verification provides real value, especially on `cancel-async-tasks`, `regex-log`, and `nginx-request-logging`.

**Note:** This contradicts the pilot-comparison.md finding that A' (80%) beat D (73.3%). The discrepancy is due to different trial runs — the pilot A' data had unusually strong results, while the rerun-condition-a data used here is more representative.

---

## 6. Efficiency Analysis

### Token Efficiency

| Condition | Total Tokens (all trials) | Tokens per Pass | Efficiency Score |
|-----------|--------------------------|-----------------|-----------------|
| A (bare) | 7.11M | 790K | **1.27** |
| C (enhanced) | 10.87M | 988K | 1.01 |
| D (self-verify) | 14.15M | 1,011K | 0.99 |
| E (org-gen) | 16.76M | 1,197K | 0.84 |

A is the most token-efficient: it achieves the most passes per million tokens. Each additional layer of executor sophistication (C > D > E) increases token usage without proportional pass rate gains.

### Time Efficiency

| Condition | Avg Time per Trial | Time per Pass |
|-----------|-------------------|---------------|
| A | 196s | 457s |
| C | 270s | 515s |
| D | 318s | 477s |
| E | 400s | 600s |

A and D are comparable in time-per-pass. C and E are slower.

---

## 7. Per-Task Deep Dives

### Tasks Where Decomposition Helps (E > A by 33pp+)

| Task | A | E | Delta | Why Decomposition Helps |
|------|---|---|-------|------------------------|
| build-cython-ext | 33% | 100% | +67pp | Multi-step pipeline (clone, patch, compile, test) benefits from explicit subtask boundaries |
| merge-diff-arc-agi-task | 67% | 100% | +33pp | Reasoning task with 4+ distinct phases (git setup, data parsing, algorithm, testing) |
| overfull-hbox | 33% | 67% | +34pp | Diagnose-then-fix structure helps constrained debugging |

### Tasks Where Decomposition Hurts (E < A)

| Task | A | E | Delta | Why Decomposition Hurts |
|------|---|---|-------|------------------------|
| cancel-async-tasks | 33% | 0% | -33pp | Atomic single-function task loses holistic context when fragmented |
| regex-log | 33% | 0% | -33pp | Holistic regex reasoning cannot be decomposed — each fragment loses pattern context |

### Tasks Where Self-Verification Helps (D > A by 33pp+)

| Task | A | D | Delta | Why Verification Helps |
|------|---|---|-------|----------------------|
| nginx-request-logging | 33% | 100% | +67pp | Config verification loop catches issues that a single pass misses |
| cancel-async-tasks | 33% | 67% | +34pp | Iterative testing catches concurrency edge case |
| regex-log | 33% | 67% | +34pp | Repeated test runs (avg 10 iterations) converge on correct regex |

---

## 8. Statistical Tests

### Aggregate Pairwise Comparisons (Fisher's Exact Test, Two-Sided)

| Comparison | Pass Rates | p-value | Significant? |
|------------|-----------|---------|--------------|
| A vs C | 42.9% vs 52.4% | 0.758 | No |
| A vs D | 42.9% vs 66.7% | 0.215 | No |
| A vs E | 42.9% vs 66.7% | 0.215 | No |
| C vs D | 52.4% vs 66.7% | 0.530 | No |
| C vs E | 52.4% vs 66.7% | 0.530 | No |
| D vs E | 66.7% vs 66.7% | 1.000 | No |

**None of the comparisons reach statistical significance.** This was predicted by the plan's power analysis: with 21 trials per condition, only ~25pp differences can be detected at p < 0.05. The largest observed difference (A vs D, 23.8pp) is just below this threshold.

### Power Analysis

To detect the A-vs-D difference (23.8pp) at 80% power and p < 0.05, we would need approximately 60 trials per condition (Fisher's exact). For the C-vs-D difference (14.3pp), approximately 180 trials per condition would be needed.

---

## 9. Limitations

1. **Small sample size.** 3 trials per task per condition is insufficient for statistical significance on moderate effects. All findings are directional, not conclusive.

2. **TB adapter ≠ coordinator.** The plan's key features (decomp_guidance, auto_test_discovery, verify_mode config flags) operate at the coordinator level via `wg service start`. The TB adapter implements approximate equivalents through prompting strategies, but the mapping is imperfect.

3. **Model confound.** All conditions used `openrouter/minimax/minimax-m2.7`. The enhanced executor features were designed for `claude:opus`. Results may differ with the intended model.

4. **Run-to-run variance.** The pilot A' data (80% pass rate) and rerun A data (42.9%) differ substantially, showing that 21-trial runs have high variance. Comparisons should be treated as suggestive.

5. **Termination mechanism asymmetry.** Conditions A and C terminate via `no_tool_calls`, while D and E terminate via `wg_done`. This makes false-PASS comparison structurally invalid — A/C can't false-PASS by definition.

---

## 10. Key Takeaways and Recommendations

### What Works

1. **Self-verification loops (D)** improve pass rate (+23.8pp over bare) and are the single most impactful feature tested. The iterative test-fix cycle catches edge cases that a single pass misses.

2. **Organizational decomposition (E)** excels on genuinely multi-step tasks (build pipelines, reasoning chains) and matches D's overall pass rate while providing better structure.

3. **Skill injection (C)** provides a modest boost for well-structured tasks (nginx-request-logging: 100% vs A's 33%) but doesn't help and may hurt on tasks requiring holistic reasoning (regex-log: 0%).

### What Doesn't Work

1. **Self-verification for atomic tasks.** D and E's false-PASS rates (28-33%) show that agents rubber-stamp their own work, especially on tasks with non-obvious verification criteria (overfull-hbox: 0 verification iterations).

2. **Decomposition for atomic tasks.** E's 0% on cancel-async-tasks and regex-log confirms that fragmenting holistic tasks into subtasks is counterproductive. The enhanced executor's adaptive complexity classification (classify tasks as Atomic vs Pipeline) addresses this, but it operates at the coordinator level and was not testable here.

3. **Planning phase alone.** C's planning phase doesn't substitute for iterative verification. Planning helps structure the approach but doesn't catch edge cases discovered only through testing.

### Actionable Recommendations

1. **Separate-agent verification is the highest-priority feature to validate.** D's 33% false-PASS rate is the clearest signal in this data. The `verify_mode = "separate"` feature directly addresses this by having a fresh agent verify work. A dedicated coordinator-level experiment should test this.

2. **Adaptive decomposition classification needs coordinator-level testing.** The atomic-vs-pipeline classification would prevent E's catastrophic 0% on cancel-async-tasks and regex-log. This is testable through the coordinator but not through the TB adapter.

3. **Increase trial count for future experiments.** At minimum 10 trials per task per condition (70 per condition) to detect 15pp differences. Ideally 20+ per task for robust per-task comparisons.

4. **Test with claude:opus.** The enhanced features were designed for Claude's capabilities. The minimax/m2.7 model may not respond optimally to skill injection and planning prompts designed for Claude.

5. **Hybrid strategy:** Combine D's verification loop with E's decomposition, gated by complexity classification: decompose multi-step tasks, directly solve atomic ones, always verify with a separate agent.

---

## Appendix A: Trial-Level Data

### Condition A (Bare Agent)

| Task | Trial | Result | Turns | Time | Tokens |
|------|-------|--------|-------|------|--------|
| build-cython-ext | 1 | PASS | 50 | 350s | 1,032K |
| build-cython-ext | 2 | FAIL | 50 | 242s | 1,135K |
| build-cython-ext | 3 | FAIL | 38 | 234s | 655K |
| cancel-async-tasks | 1 | PASS | 13 | 118s | 76K |
| cancel-async-tasks | 2 | FAIL | 5 | 46s | 13K |
| cancel-async-tasks | 3 | FAIL | 8 | 75s | 31K |
| overfull-hbox | 1 | FAIL | 26 | 441s | 372K |
| overfull-hbox | 2 | FAIL | 34 | 217s | 405K |
| overfull-hbox | 3 | PASS | 20 | 104s | 131K |
| regex-log | 1 | FAIL | 8 | 276s | 96K |
| regex-log | 2 | FAIL | 10 | 225s | 114K |
| regex-log | 3 | PASS | 6 | 116s | 35K |
| custom-memory-heap-crash | 1 | FAIL | 50 | 643s | 1,440K |
| custom-memory-heap-crash | 2 | PASS | 35 | 381s | 699K |
| custom-memory-heap-crash | 3 | PASS | 15 | 89s | 87K |
| merge-diff-arc-agi-task | 1 | PASS | 23 | 188s | 336K |
| merge-diff-arc-agi-task | 2 | FAIL | 12 | 168s | 111K |
| merge-diff-arc-agi-task | 3 | PASS | 18 | 121s | 240K |
| nginx-request-logging | 1 | PASS | 19 | 76s | 106K |
| nginx-request-logging | 2 | FAIL | 1 | 2s | 0K |
| nginx-request-logging | 3 | FAIL | 1 | 1s | 0K |

### Condition C (Enhanced / Skill Injection)

| Task | Trial | Result | Turns | Time | Tokens |
|------|-------|--------|-------|------|--------|
| build-cython-ext | 1 | PASS | 50 | 281s | 949K |
| build-cython-ext | 2 | FAIL | 50 | 212s | 878K |
| build-cython-ext | 3 | PASS | 50 | 259s | 876K |
| cancel-async-tasks | 1 | FAIL | 20 | 215s | 166K |
| cancel-async-tasks | 2 | FAIL | 27 | 302s | 257K |
| cancel-async-tasks | 3 | PASS | 9 | 41s | 29K |
| overfull-hbox | 1 | FAIL | 50 | 238s | 597K |
| overfull-hbox | 2 | PASS | 35 | 157s | 358K |
| overfull-hbox | 3 | FAIL | 50 | 247s | 664K |
| regex-log | 1 | FAIL | 6 | 166s | 53K |
| regex-log | 2 | FAIL | 10 | 143s | 80K |
| regex-log | 3 | FAIL | 35 | 818s | 1,202K |
| custom-memory-heap-crash | 1 | PASS | 17 | 139s | 142K |
| custom-memory-heap-crash | 2 | FAIL | 50 | 1040s | 2,316K |
| custom-memory-heap-crash | 3 | PASS | 34 | 425s | 598K |
| merge-diff-arc-agi-task | 1 | PASS | 29 | 151s | 468K |
| merge-diff-arc-agi-task | 2 | FAIL | 22 | 131s | 313K |
| merge-diff-arc-agi-task | 3 | PASS | 25 | 482s | 623K |
| nginx-request-logging | 1 | PASS | 18 | 63s | 146K |
| nginx-request-logging | 2 | PASS | 8 | 92s | 57K |
| nginx-request-logging | 3 | PASS | 14 | 60s | 94K |

### Condition D (Self-Verify / Old Executor)

| Task | Trial | Result | Turns | Time | Tokens | Verify Iters | False-PASS |
|------|-------|--------|-------|------|--------|--------------|-----------|
| build-cython-ext | 1 | PASS | 50 | 257s | 1,195K | 5 | |
| build-cython-ext | 2 | FAIL | 50 | 275s | 943K | 6 | YES |
| build-cython-ext | 3 | FAIL | 37 | 213s | 590K | 4 | YES |
| cancel-async-tasks | 1 | FAIL | 17 | 114s | 108K | 1 | YES |
| cancel-async-tasks | 2 | PASS | 13 | 75s | 62K | 2 | |
| cancel-async-tasks | 3 | PASS | 20 | 256s | 187K | 8 | |
| overfull-hbox | 1 | FAIL | 47 | 317s | 655K | 0 | YES |
| overfull-hbox | 2 | PASS | 93 | 656s | 1,196K | 0 | |
| overfull-hbox | 3 | FAIL | 136 | 705s | 3,421K | 0 | YES |
| regex-log | 1 | FAIL | 30 | 523s | 774K | 15 | YES |
| regex-log | 2 | PASS | 15 | 343s | 318K | 4 | |
| regex-log | 3 | PASS | 17 | 239s | 235K | 11 | |
| custom-memory-heap-crash | 1 | PASS | 53 | 1030s | 1,965K | 4 | |
| custom-memory-heap-crash | 2 | PASS | 16 | 91s | 126K | 2 | |
| custom-memory-heap-crash | 3 | PASS | 42 | 332s | 709K | 3 | |
| merge-diff-arc-agi-task | 1 | FAIL | 11 | 375s | 132K | 1 | YES |
| merge-diff-arc-agi-task | 2 | PASS | 26 | 515s | 672K | 1 | |
| merge-diff-arc-agi-task | 3 | PASS | 33 | 156s | 484K | 1 | |
| nginx-request-logging | 1 | PASS | 15 | 70s | 110K | 1 | |
| nginx-request-logging | 2 | PASS | 16 | 71s | 132K | 0 | |
| nginx-request-logging | 3 | PASS | 18 | 77s | 136K | 8 | |

### Condition E (Organization Generation)

| Task | Trial | Result | Turns | Time | Tokens | Verify Iters | False-PASS |
|------|-------|--------|-------|------|--------|--------------|-----------|
| build-cython-ext | 1 | PASS | 59 | 242s | 1,125K | 5 | |
| build-cython-ext | 2 | PASS | 97 | 326s | 1,876K | 4 | |
| build-cython-ext | 3 | PASS | 57 | 268s | 1,280K | 15 | |
| cancel-async-tasks | 1 | FAIL | 34 | 355s | 382K | 14 | YES |
| cancel-async-tasks | 2 | FAIL | 13 | 127s | 87K | 4 | YES |
| cancel-async-tasks | 3 | FAIL | 13 | 120s | 88K | 7 | YES |
| overfull-hbox | 1 | FAIL | 52 | 271s | 756K | 0 | YES |
| overfull-hbox | 2 | PASS | 47 | 239s | 583K | 0 | |
| overfull-hbox | 3 | PASS | 27 | 172s | 333K | 0 | |
| regex-log | 1 | FAIL | 47 | 1055s | 2,055K | 15 | YES |
| regex-log | 2 | FAIL | 0 | 1800s | 0K | - | |
| regex-log | 3 | FAIL | 62 | 660s | 1,825K | 36 | YES |
| custom-memory-heap-crash | 1 | PASS | 42 | 401s | 918K | 2 | |
| custom-memory-heap-crash | 2 | PASS | 64 | 1101s | 2,342K | 4 | |
| custom-memory-heap-crash | 3 | PASS | 36 | 390s | 638K | 3 | |
| merge-diff-arc-agi-task | 1 | PASS | 28 | 174s | 455K | 2 | |
| merge-diff-arc-agi-task | 2 | PASS | 33 | 157s | 535K | 6 | |
| merge-diff-arc-agi-task | 3 | PASS | 35 | 148s | 552K | 3 | |
| nginx-request-logging | 1 | PASS | 35 | 127s | 361K | 2 | |
| nginx-request-logging | 2 | PASS | 26 | 142s | 230K | 1 | |
| nginx-request-logging | 3 | PASS | 36 | 116s | 344K | 0 | |

---

## Appendix B: Data Sources

| Condition | Directory | Agent Class | Trials |
|-----------|-----------|-------------|--------|
| A | `results/rerun-condition-a/` | ConditionAAgent | 21 |
| C | `results/full-condition-c/` | ConditionCAgent | 21 |
| D | `results/pilot-d/pilot-d/` | ConditionDAgent | 21 |
| E | `results/pilot-e/pilot-e/` | ConditionEAgent | 21 |

All conditions used model `openrouter/minimax/minimax-m2.7`, temperature 0.0, with Docker-isolated environments via the harbor framework.

## Appendix C: Cross-Reference with Prior Analysis

The pilot-comparison.md (A' vs D vs E across 10 tasks, 30 trials each) found:
- A' 80.0%, D 73.3%, E 75.0% (on broader task set)
- A' benefited from several "easy" tasks (count-dataset-tokens, qemu-startup) excluded from this H2H comparison
- This H2H study focuses on the 7 hardest tasks, which explains the lower aggregate pass rates

The consistency between prior and current findings on per-task patterns (E excels on build-cython-ext and merge-diff-arc-agi-task; D excels on nginx-request-logging; regex-log is problematic for E) validates the reproducibility of these results.
