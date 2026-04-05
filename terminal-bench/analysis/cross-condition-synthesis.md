# Cross-Condition Comparative Analysis: Terminal Bench Experiment

**Date:** 2026-04-05
**Task:** tb-cross-condition-synthesis
**Data sources:** Full-sweep-01 (84 trials), Rerun-01 (24 trials), Condition F sweep (17/21 completed), Harbor pilot/full runs, prerequisite investigations

---

## Executive Summary

Five conditions (A/C/D/E/F) were compared across 7 task types on the Terminal Bench benchmark. Two key findings dominate:

1. **Model capability saturates these tasks.** With Claude Sonnet 4.6, all conditions achieve 100% pass rate. The tasks are too easy to differentiate conditions by correctness. Earlier Harbor runs with minimax m2.7 (a weaker model) showed meaningful differences: A 48%, B 38%, D 73%, E 75%.

2. **FLIP separates shallow from deep understanding.** Even at 100% pass rate, FLIP scores reveal a massive quality gap: Condition A (bare agent) averages 0.134 while D/E (structured verification + agency) average 0.755–0.794. This gap is statistically significant (Kruskal-Wallis H=41.76, p<0.0001) with very large effect sizes (Cohen's d=3.69–3.81).

**Recommendation:** For production use, Condition D (autopoietic verification) offers the best balance of understanding depth, cost efficiency, and behavioral discipline. Condition F (empirical-first verification) is the recommended design for weaker models, pending completion of its trial sweep. Condition E's mandatory decomposition adds overhead without proportional benefit on most tasks.

---

## 1. Condition Summary Table

### 1.1 Full Sweep Results (Sonnet 4.6, 7 tasks x 3 reps = 21 per condition)

| Condition | Description | Pass Rate | Mean FLIP | FLIP σ | Mean LLM Eval | LLM σ | n |
|-----------|-------------|-----------|-----------|--------|---------------|-------|---|
| **A** | Bare agent — minimal prompt, bash + file tools, no wg | 100% (21/21) | **0.134** | 0.221 | 0.736 | 0.218 | 21 |
| **C** | WG + skill injection — explicit templates, planning phase | 100% (21/21) | **0.697** | 0.242 | 0.862 | 0.140 | 21 |
| **D** | Autopoietic verification — attempt-verify-iterate loop, agency identity | 100% (21/21) | **0.755** | 0.087 | 0.897 | 0.060 | 21 |
| **E** | Organization generation — orchestrator framing, mandatory decomposition | 100% (21/21) | **0.794** | 0.106 | 0.888 | 0.055 | 21 |
| **F** | Empirical-first verification — test discovery, adaptive classification | 100% (17/17)* | pending | — | pending | — | 17* |

*Condition F: 17 of 21 trials completed at time of analysis. All 17 passed verification. FLIP/LLM eval scores not yet collected.

### 1.2 Harbor Pilot Results (minimax m2.7, Docker, external verification)

| Condition | Pass Rate | n | Key Observation |
|-----------|-----------|---|-----------------|
| **A** (50-turn cap) | 48% | ~256 | Turn cap kills ~25% of trials |
| **A'** (no turn cap) | 80% | 30 | Removing turn cap = biggest single intervention |
| **B** (passive wg) | 38% | ~270 | WG tools without guidance hurt performance |
| **C** (skill injection) | 41% | ~169 | Partially recovers B's overhead |
| **D** (verification loop) | 73% | 30 | Most cost-efficient (521K tokens/trial) |
| **E** (org generation) | 75% | 30 | Best multi-step, worst atomic tasks |

### 1.3 wg Tool Usage (Harbor data, minimax m2.7)

| Condition | Any wg Tool | wg_add (Decomposition) | --after (Dependencies) |
|-----------|-------------|----------------------|----------------------|
| **A** | 0% | 0% | N/A |
| **B** | 46% | 17% | 0% |
| **C** | 86% | 8% | 0% |
| **D** | 100% | 3% | 0% |
| **E** | 97% | 93% | 5.9% |
| **F** | N/A (no wg tools by design) | N/A | N/A |

---

## 2. Task Difficulty Analysis

### 2.1 Pass Rate by Difficulty (Full Sweep — Sonnet 4.6)

All conditions achieve 100% pass rate at all difficulty levels. **No differentiation is possible on pass rate with this model-task combination.**

### 2.2 FLIP by Difficulty (Full Sweep)

| Difficulty | Tasks | A | C | D | E |
|-----------|-------|---|---|---|---|
| **Easy** | file-ops, text-processing | 0.260±0.282 | 0.478±0.340 | 0.770±0.121 | 0.823±0.118 |
| **Medium** | debugging, shell-scripting, data-processing | 0.126±0.224 | 0.840±0.113 | 0.739±0.069 | 0.813±0.098 |
| **Hard** | algorithm, ml | 0.020±0.026 | 0.702±0.060 | 0.765±0.085 | 0.738±0.100 |

Key observations:
- **Condition A FLIP degrades with difficulty** (0.260 → 0.126 → 0.020). Harder tasks produce shallower understanding when the agent has no scaffolding.
- **Conditions C/D/E maintain FLIP above 0.70** across all difficulty levels. The scaffolding provides consistent understanding depth.
- **Condition C has a surprising dip on easy tasks** (0.478) driven by file-ops (mean FLIP=0.215). All other C scores are well above 0.70.

### 2.3 Per-Task FLIP Breakdown

| Task | Difficulty | A | C | D | E |
|------|-----------|---|---|---|---|
| file-ops | Easy | 0.207 | 0.215 | 0.692 | 0.782 |
| text-processing | Easy | 0.314 | 0.741 | 0.848 | 0.865 |
| debugging | Medium | 0.227 | 0.923 | 0.730 | 0.892 |
| shell-scripting | Medium | 0.108 | 0.889 | 0.771 | 0.815 |
| data-processing | Medium | 0.042 | 0.707 | 0.716 | 0.732 |
| algorithm | Hard | 0.032 | 0.713 | 0.839 | 0.780 |
| ml | Hard | 0.008 | 0.690 | 0.692 | 0.697 |

Notable patterns:
- **file-ops is the only task where C underperforms D/E.** C's skill injection doesn't help on the simplest structural tasks. This anomaly was the target of the rerun-01, where C's file-ops FLIP improved from 0.215 to 0.603 post-evolution.
- **ml has the lowest FLIP across all non-A conditions** (~0.69). ML implementation tasks produce shallow understanding even with full scaffolding.
- **debugging has the highest C FLIP** (0.923) — skill injection is particularly effective for structured debugging workflows.

### 2.4 Pass Rate by Difficulty (Harbor — minimax m2.7)

From the decomposition investigation (E only, Harbor pilot):

| Difficulty | E Pass Rate | E Decomposition Rate |
|-----------|-------------|---------------------|
| Easy | 60% (9/15) | 93% |
| Medium | 100% (6/6) | 100% |
| Hard | 78% (7/9) | 89% |

Forced decomposition (E) **hurts easy tasks** (60% vs A's 80%) but helps medium/hard tasks (100%/78%). The mismatch between decomposition strategy and task structure is the key failure mode.

---

## 3. Statistical Comparisons

### 3.1 FLIP Score Pairwise Tests (Mann-Whitney U)

| Comparison | U Statistic | p-value | Cohen's d | Significant? |
|------------|-------------|---------|-----------|-------------|
| A vs C | 26.0 | 0.000001 | 2.43 | **Yes** |
| A vs D | 13.0 | <0.000001 | 3.69 | **Yes** |
| A vs E | 8.0 | <0.000001 | 3.81 | **Yes** |
| C vs D | 206.5 | 0.734 | 0.32 | No |
| C vs E | 171.0 | 0.217 | 0.52 | No |
| D vs E | 172.0 | 0.227 | 0.40 | No |

### 3.2 FLIP Threshold Tests (Fisher's Exact, FLIP >= 0.70)

| Comparison | Left (above/total) | Right (above/total) | p-value | Significant? |
|------------|-------------------|---------------------|---------|-------------|
| A vs C | 1/21 | 13/21 | 0.0002 | **Yes** |
| A vs D | 1/21 | 14/21 | 0.00005 | **Yes** |
| A vs E | 1/21 | 15/21 | 0.00001 | **Yes** |
| C vs D | 13/21 | 14/21 | 1.000 | No |
| C vs E | 13/21 | 15/21 | 0.744 | No |
| D vs E | 14/21 | 15/21 | 1.000 | No |

### 3.3 LLM Eval Pairwise Tests (Mann-Whitney U)

| Comparison | U Statistic | p-value | Cohen's d | Significant? |
|------------|-------------|---------|-----------|-------------|
| A vs C | 103.5 | 0.009 | 0.68 | **Yes** |
| A vs D | 74.0 | 0.001 | 1.01 | **Yes** |
| A vs E | 88.0 | 0.002 | 0.96 | **Yes** |
| C vs D | 194.0 | 0.881 | 0.33 | No |
| C vs E | 209.5 | 0.807 | 0.25 | No |
| D vs E | 225.5 | 0.497 | -0.16 | No |

### 3.4 Multi-Group Test

| Test | Statistic | p-value |
|------|-----------|---------|
| Kruskal-Wallis (FLIP, A/C/D/E) | H = 41.76 | p < 0.0001 |

### 3.5 FLIP Score Distribution

| Condition | < 0.30 | 0.30–0.70 | >= 0.70 |
|-----------|--------|-----------|---------|
| A | 18 (86%) | 2 (10%) | 1 (5%) |
| C | 2 (10%) | 6 (29%) | 13 (62%) |
| D | 0 (0%) | 7 (33%) | 14 (67%) |
| E | 0 (0%) | 6 (29%) | 15 (71%) |

### 3.6 Interpretation

**The data reveals two tiers, not a gradient:**

- **Tier 1 (Condition A):** Shallow understanding despite correct output. FLIP < 0.30 in 86% of trials. The agent produces working code without structured reasoning about what it built.
- **Tier 2 (Conditions C/D/E):** Deep understanding. FLIP >= 0.70 in 62–71% of trials. Structured scaffolding (any form: skill injection, verification loops, or decomposition) produces agents that understand their own work.

Within Tier 2, the differences between C, D, and E are **not statistically significant** at this sample size (n=21 per condition). D and E trend higher than C on FLIP (0.755/0.794 vs 0.697), but the p-values range from 0.22 to 0.73. More data or harder tasks would be needed to distinguish them.

---

## 4. FLIP Validity Assessment

### 4.1 Model Mismatch Finding

The FLIP model investigation (tb-investigate-flip-model) established:

- **FLIP always uses fixed evaluator models**: Claude Sonnet (inference) + Claude Haiku (comparison), per `.workgraph/config.toml` configuration.
- **The task agent's model is recorded but never used** for FLIP probes (`src/commands/evaluate.rs:674-684`).
- **For the full-sweep data (Sonnet 4.6 agents), this is not a confound** — the FLIP inference model (also Sonnet) naturally understands Sonnet's output patterns.

### 4.2 Impact on Cross-Condition Comparison

Since all full-sweep conditions (A/C/D/E) used the same model (Sonnet 4.6), the FLIP scores are **directly comparable** across conditions. The model mismatch issue is latent — it only manifests when comparing agents using different models.

### 4.3 FLIP and Condition F

Condition F's trial config specified model overrides:
- **Algorithm, debugging, data-processing, ml, shell-scripting**: Claude Sonnet 4.6
- **File-ops, text-processing**: Gemini 2.5 Flash

For the Gemini trials, FLIP scores (when collected) will face the model mismatch problem: Sonnet's inference phase will try to reverse-engineer what prompt a Gemini model was working from. These scores should be interpreted with caution and flagged in any F vs A/C/D/E comparison.

### 4.4 FLIP Does Not Predict Verification Failure Here

All 84 full-sweep trials passed, including 18 with FLIP < 0.30. FLIP's theoretical role as a false-PASS detector is **unvalidated on this benchmark** — no failures exist to catch. The H2H trial data (33% false-PASS rate on harder tasks) suggests FLIP would be useful in a harder setting, but this remains a projection, not a measurement.

### 4.5 FLIP vs LLM Eval: Complementary Signals

| Metric | Condition Separation | Score Range | What It Measures |
|--------|---------------------|-------------|------------------|
| FLIP | 3.3x ratio (A vs D) | 0.00–0.78 | Latent understanding depth |
| LLM Eval | 1.1x ratio (A vs D) | 0.72–0.94 | Output quality |

FLIP provides far stronger discriminative power than LLM eval for separating shallow from deep understanding. LLM eval scores cluster in a narrow high range because all conditions produce correct output. The two metrics are complementary — FLIP detects understanding gaps that LLM eval cannot.

---

## 5. Behavioral Taxonomy

### 5.1 Condition A: Execute-and-Stop

- **Strategy:** Read task → implement → run → stop
- **wg usage:** None
- **Verification:** Ad-hoc (run the code, see if it works)
- **Termination:** Silent stop (`no_tool_calls`)
- **Strength:** No overhead, direct path to solution
- **Weakness:** No structured verification, no failure diagnostics, no understanding of *why* the solution works

### 5.2 Condition C: Plan-and-Execute

- **Strategy:** Planning phase ("analyze before acting") → implement → wg_log progress → wg_done
- **wg usage:** 86% any-wg, 8% decomposition
- **Verification:** Not gated — 10.5% call wg_done with zero verification
- **Termination:** wg_done (86% of successes)
- **Strength:** Structured planning, progress journaling, clean termination signaling
- **Weakness:** No verification gate — agents can declare done without testing

### 5.3 Condition D: Verify-and-Iterate

- **Strategy:** Implement → verify (run tests) → iterate if failed → wg_done after verification passes
- **wg usage:** 100% any-wg, 3% decomposition
- **Verification:** Self-authored tests, gated on wg_done. Average 3.4 verification iterations per trial.
- **Termination:** wg_done gated on verification (93%); wg_fail after 3 stuck iterations
- **Strength:** Most cost-efficient (521K tokens/trial, 37% less than A'), disciplined convergence
- **Weakness:** Self-verification blind spot — agent tests what it *conceives of testing*, not what the external verifier checks

### 5.4 Condition E: Decompose-and-Orchestrate

- **Strategy:** Analyze → decompose (wg_add subtasks) → implement each → "independent" verification → triage
- **wg usage:** 97% any-wg, 93% decomposition, 5.9% with --after dependencies
- **Verification:** "Independent" cognitive review (same context window) — 100% false-PASS rate on failures
- **Termination:** wg_done after PASS verdict; wg_fail after 6 iterations
- **Strength:** Excels on genuinely multi-step tasks (build pipelines, system configuration)
- **Weakness:** Counterproductive on atomic tasks (regex-log 0%, cancel-async 0%), "independent verification" is theater

### 5.5 Condition F: Discover-and-Verify

- **Strategy:** Find existing tests → classify task → implement → run discovered tests → iterate on failures
- **wg usage:** Uses wg in workgraph execution (logging, done) but not designed for Harbor wg tools
- **Verification:** Empirical — existing tests are authoritative, not self-authored tests
- **Termination:** wg_done (workgraph context)
- **Strength:** Test discovery bridges the verification gap; adaptive classification avoids E's decomposition trap
- **Weakness:** Falls back to A-like behavior when no tests exist; incomplete trial data

### 5.6 Behavioral Comparison Matrix

| Behavior | A | C | D | E | F |
|----------|---|---|---|---|---|
| Plans before coding | No | Yes | Implicit | Yes (full decomposition) | Yes (classification) |
| Runs verification tests | Ad-hoc | Ad-hoc | Self-authored | Self-authored ("independent") | **Discovers existing tests** |
| Iterates on failure | ~35% | ~40% | **Always** (loop) | Yes (triage phase) | Yes (5 iterations max) |
| Signals completion | Silent | wg_done | **wg_done gated on verify** | wg_done gated on PASS verdict | wg_done |
| Signals failure | Silent | Rare | wg_fail | wg_fail | Natural stop |
| Cost per trial | ~820K tokens | ~600K tokens | **~520K tokens** | ~680K tokens | ~600K tokens (est.) |

---

## 6. Autopoietic Decomposition Analysis

### 6.1 Decomposition Rates Track Prompt Framing

The decomposition investigation (tb-investigate-decomposition) established:

```
B (suggestion) → C (heuristic) → D (discouraged) → F (adaptive) → E (mandatory)
    17%              7-8%             3%               TBD              93%
```

The primary driver of decomposition is **prompt framing**, not task complexity or tool availability. The model (minimax m2.7 in Harbor, Sonnet 4.6 in full-sweep) decomposes when told to, not spontaneously.

### 6.2 When Decomposition Helps

| Task Structure | Decomposes? | Outcome |
|---------------|-------------|---------|
| Multi-step build (clone → patch → build → test) | Ideal when decomposed | E: 100% pass on build-cython-ext, nginx |
| Multi-file configuration | Benefits from decomposition | E: 100% pass on nginx-request-logging |
| Single function implementation | **Harmed by decomposition** | E: 0% pass on cancel-async, regex-log |
| Algorithm/regex design | **Harmed by decomposition** | Holistic reasoning required; fragmentation loses coherence |

### 6.3 The Decomposition Decision Matrix

|  | Agent Decomposes | Agent Solves Directly |
|--|------------------|-----------------------|
| **Multi-step task** | **Ideal**: E 100% on build/nginx | **Adequate**: D passes via verify-iterate |
| **Atomic task** | **Harmful**: E 0% on regex/async | **Ideal**: D passes via direct implementation |

### 6.4 Dependency Expression is Nearly Absent

Even in Condition E (93% decomposition, 101 wg_add calls), only 6 calls (5.9%) used `--after` dependencies. Agents create flat task lists, not graphs. This is because:

1. The tool schema doesn't return created task IDs, so agents must guess auto-generated IDs
2. In single-agent execution, dependencies don't affect execution order
3. Prompt examples rarely show dependency syntax

### 6.5 Condition F's Adaptive Approach

F's design teaches *when* to decompose rather than forcing it:
- **Atomic tasks** (single file, single function): Implement directly
- **Multi-step tasks** (multiple files, build pipeline): Plan steps, implement sequentially

This avoids E's failure mode (decomposing atomic tasks) while preserving the option for complex tasks. However, F's full-sweep trials use Sonnet 4.6 via workgraph (not Harbor with minimax), so a direct comparison of decomposition behavior is not yet available.

---

## 7. Known Confounds

### 7.1 Condition B Adapter Bug

The rerun-condition-b dataset (183 trials) ran **ConditionCAgent, not ConditionBAgent**. This was documented in the run script as a "corrected" B run. Any B vs C comparison must use the original full-condition-b data (270 trials). The early-behavior analysis (tb-early-behavior-analysis) identified this and flagged the rerun B data as invalid for B analysis.

**Impact:** Condition B data is limited to the original Harbor run (270 trials, 38.1% pass rate). No B full-sweep data exists with Sonnet 4.6.

### 7.2 FLIP Model Mismatch

FLIP uses fixed evaluator models (Sonnet + Haiku) regardless of the task agent's model. For the Sonnet 4.6 full-sweep, this is not a confound (evaluator matches agent model family). For Condition F's Gemini Flash trials (file-ops, text-processing), FLIP scores will conflate task quality with cross-model interpretability.

**Impact:** F's FLIP scores on file-ops and text-processing should not be directly compared with A/C/D/E scores on the same tasks without accounting for the model mismatch.

### 7.3 wg Availability vs. Adoption

The wg availability investigation (tb-investigate-wg-availability) confirmed wg tools are 100% reliable (0 errors across 1,166 calls). Low wg adoption in B (46%) and C (86%) is a prompt compliance issue, not a tool availability bug. The model ignores optional tools when the task appears simple.

### 7.4 Full-Sweep vs. Harbor: Different Experimental Contexts

| Dimension | Harbor Runs | Full-Sweep-01 |
|-----------|-------------|---------------|
| Model | minimax m2.7 | Claude Sonnet 4.6 |
| Execution | Docker containers, external verification | wg service, coordinated agents |
| Pass rates | Differentiated (38–80%) | Saturated (100%) |
| Differentiation | Pass rate | FLIP/LLM eval quality |

These are not the same experiment. Harbor data shows condition effects on task *completion*. Full-sweep data shows condition effects on *understanding quality*. Both are valid but measure different things.

### 7.5 Condition F Incomplete Data

At time of analysis, Condition F has 17/21 trials completed. Missing: algorithm-r0, data-processing-r0, data-processing-r2, shell-scripting-r2. No FLIP or LLM eval scores have been collected for any F trial. The comparison is therefore provisional and should be updated when tb-collect-condition-f completes.

### 7.6 Ceiling Effect

100% pass rate across all conditions means:
- Evolution impact on pass rates cannot be measured (evolution-impact finding)
- Condition differences cannot be detected by pass rate
- FLIP is the only discriminating metric, and it measures understanding rather than correctness
- Harder tasks are needed to produce a failure gradient

### 7.7 Sample Size

21 trials per condition (3 replicas × 7 tasks) provides limited statistical power for within-Tier-2 comparisons. The C vs D, C vs E, and D vs E differences (p=0.22–0.73) would require ~100 trials per condition to reach significance at current effect sizes.

---

## 8. Conclusions and Recommendations

### 8.1 The Two-Tier Structure

The clearest finding is that conditions form two distinct tiers:

- **Tier 1 (A):** Correct output, shallow understanding (FLIP ~0.13)
- **Tier 2 (C/D/E):** Correct output, deep understanding (FLIP ~0.70–0.80)

Any structured scaffolding — skill injection (C), verification loops (D), or decomposition protocols (E) — lifts agents from Tier 1 to Tier 2. The specific scaffolding type matters less than its presence.

### 8.2 Condition Recommendations

**For production use with capable models (Sonnet-class):**

| Rank | Condition | Rationale |
|------|-----------|-----------|
| 1 | **D** | Best cost efficiency (521K tokens/trial), lowest FLIP variance (σ=0.087), disciplined convergence, clean failure signaling. The verification loop is the most reliable behavioral pattern. |
| 2 | **E** | Highest mean FLIP (0.794), but 2-3x more wg overhead than D, counterproductive on atomic tasks, "independent verification" is unreliable. Best for genuinely multi-step tasks only. |
| 3 | **C** | Good baseline (FLIP 0.697), simple to implement, 86% wg adoption. Lacks verification gating — agents can declare done without testing. |
| 4 | **F** | Promising design (test discovery + adaptive classification) but incomplete data. Recommended for further evaluation, especially on harder tasks and weaker models. |

**For production use with weaker models (minimax-class):**

| Rank | Condition | Rationale |
|------|-----------|-----------|
| 1 | **F** | Addresses the critical verification gap (no condition tells agents where tests live). Test discovery is the highest-impact single intervention. All Harbor E failures would have been caught by running `/tests/test_outputs.py`. |
| 2 | **D** | Best Harbor pass rate among wg conditions (73%). Verification loop works model-independently. |
| 3 | **A'** (no turn cap) | Simplest, 80% pass rate. Removing the turn cap was more impactful than adding wg tools. |
| 4 | **E** | 75% pass rate but 100% false-PASS rate on failures. Mandatory decomposition actively harms atomic tasks. |

### 8.3 What F Gets Right (by Design)

Condition F synthesizes the key lessons from A through E:

1. **Test discovery** (from E's failure analysis): The #1 intervention. All false-PASSes across conditions stemmed from agents not knowing what the verifier tests.
2. **Empirical verification** (from D's blind spot): Trust test results, not self-assessment.
3. **Adaptive classification** (from E's decomposition data): Decompose when the task structure warrants it, not by default.
4. **No wg overhead** (from A' vs B/C comparison): In single-agent settings, wg tools add cognitive load without coordination benefit.
5. **Time awareness** (from E's timeout data): Explicit time budgets prevent unbounded iteration.

### 8.4 What Remains Unknown

1. **F's FLIP scores.** Until FLIP evaluations are collected for F, we cannot quantify whether F achieves Tier 2 understanding.
2. **F vs D on harder tasks.** The full-sweep tasks are too easy. The Harbor H2H tasks (33% false-PASS rate) are the right difficulty target.
3. **Multi-agent effects.** All data is single-agent. wg's coordination features (dependency dispatch, agent handoffs, concurrent execution) have never been tested in the benchmark.
4. **Model interaction effects.** Conditions may rank differently with different models. The only cross-model data point is minimax m2.7 (Harbor) vs Sonnet 4.6 (full-sweep), which confounds model capability with condition effects.

### 8.5 Actionable Next Steps

1. **Complete Condition F sweep.** 4 trials remain in-progress. Once done, collect FLIP/LLM eval via tb-collect-condition-f.
2. **Run harder tasks.** Port the H2H trial tasks (cancel-async-tasks, regex-log, build-cython-ext, overfull-hbox, etc. — known to produce failures) into a new sweep with conditions D, E, and F.
3. **Fix the FLIP model mismatch.** Implement per-task FLIP model override (`src/commands/evaluate.rs:674`) so F's Gemini trials are scored by a Gemini evaluator, not Sonnet.
4. **Test multi-agent scenarios.** Run the benchmark with `wg service --max-agents 4` to test whether wg's coordination features improve outcomes on tasks with genuine parallelism.
5. **Increase replicas.** 3 replicas per cell produces wide confidence intervals. 5–10 replicas on a focused task set would enable within-Tier-2 discrimination.

---

## Appendix A: Raw FLIP Scores by Condition and Task

### Condition A (n=21, mean=0.134)

| Task | r0 | r1 | r2 | Mean |
|------|----|----|-----|------|
| file-ops | 0.030 | 0.095 | 0.495 | 0.207 |
| text-processing | 0.240 | 0.000 | 0.702 | 0.314 |
| debugging | 0.000 | 0.670 | 0.010 | 0.227 |
| shell-scripting | 0.280 | 0.000 | 0.045 | 0.108 |
| data-processing | 0.125 | 0.000 | 0.000 | 0.042 |
| algorithm | 0.030 | 0.065 | 0.000 | 0.032 |
| ml | 0.000 | 0.000 | 0.025 | 0.008 |

### Condition C (n=21, mean=0.697)

| Task | r0 | r1 | r2 | Mean |
|------|----|----|-----|------|
| file-ops | 0.505 | 0.060 | 0.080 | 0.215 |
| text-processing | 0.720 | 0.620 | 0.883 | 0.741 |
| debugging | 0.960 | 0.840 | 0.970 | 0.923 |
| shell-scripting | 0.926 | 0.930 | 0.810 | 0.889 |
| data-processing | 0.680 | 0.700 | 0.740 | 0.707 |
| algorithm | 0.720 | 0.630 | 0.790 | 0.713 |
| ml | 0.650 | 0.740 | 0.680 | 0.690 |

### Condition D (n=21, mean=0.755)

| Task | r0 | r1 | r2 | Mean |
|------|----|----|-----|------|
| file-ops | 0.650 | 0.726 | 0.700 | 0.692 |
| text-processing | 0.940 | 0.905 | 0.700 | 0.848 |
| debugging | 0.780 | 0.730 | 0.680 | 0.730 |
| shell-scripting | 0.805 | 0.840 | 0.667 | 0.771 |
| data-processing | 0.800 | 0.698 | 0.650 | 0.716 |
| algorithm | 0.826 | 0.820 | 0.870 | 0.839 |
| ml | 0.685 | 0.660 | 0.730 | 0.692 |

### Condition E (n=21, mean=0.794)

| Task | r0 | r1 | r2 | Mean |
|------|----|----|-----|------|
| file-ops | 0.680 | 0.855 | 0.810 | 0.782 |
| text-processing | 0.950 | 0.949 | 0.695 | 0.865 |
| debugging | 0.880 | 0.860 | 0.935 | 0.892 |
| shell-scripting | 0.940 | 0.685 | 0.820 | 0.815 |
| data-processing | 0.770 | 0.730 | 0.695 | 0.732 |
| algorithm | 0.900 | 0.760 | 0.680 | 0.780 |
| ml | 0.780 | 0.700 | 0.610 | 0.697 |

### Condition F (n=17/21, FLIP pending)

All 17 completed trials passed verification. FLIP and LLM eval scores not yet collected.

---

## Appendix B: Statistical Methodology

- **Mann-Whitney U**: Non-parametric test for comparing two independent samples. Used because FLIP scores are not normally distributed (Condition A is heavily right-skewed).
- **Kruskal-Wallis H**: Non-parametric one-way ANOVA across 4+ groups.
- **Fisher's Exact Test**: Used for 2x2 contingency tables (above/below FLIP threshold by condition). Preferred over chi-squared due to small cell counts.
- **Cohen's d**: Effect size measure. Calculated with pooled standard deviation. Interpretation: small (0.2), medium (0.5), large (0.8), very large (>1.2).
- **Wilcoxon signed-rank**: Paired comparison for pre/post evolution FLIP scores (8 pairs). Not significant (W=7.0, p=0.148).
- All tests two-sided unless noted. Significance threshold: p < 0.05.
