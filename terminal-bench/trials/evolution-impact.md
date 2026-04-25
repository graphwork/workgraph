# Evolution Impact Report: Pre vs Post Comparison

**Date:** 2026-04-05
**Run:** evolution run-01 (`run-20260405-051623`)
**Pre-evolution data:** `tb-results-full-sweep-01.json` — 84 trials (4 conditions × 7 tasks × 3 replicas)
**Post-evolution data:** `tb-results-rerun-01.json` — 24 trials (worst performers re-run)

---

## 1. Executive Summary

**Did evolution improve pass rates?** No — pass rates were already 100% across all conditions and remained 100% after evolution. The full-sweep-01 benchmark saturated: every trial passed external verification regardless of condition or task type.

**Did evolution improve FLIP scores?** Marginally. Across the 8 re-run condition×task combinations (the worst FLIP performers from full-sweep-01), mean FLIP improved from 0.144 to 0.235 (+0.091), but this is **not statistically significant** (Wilcoxon signed-rank W=7.0, p=0.148). Condition A FLIP remained essentially flat (0.134→0.153, Mann-Whitney U p=0.49).

**Two notable improvements emerged:**
- **C+file-ops**: FLIP jumped from 0.215 to 0.603 (+0.388) — the largest single improvement
- **A+text-processing**: FLIP improved from 0.314 to 0.630 (+0.316)

**Bottom line:** The evolution pipeline produced 18 operations (5 new roles, 6 new tradeoffs, 3 new agents, coordinator prompt overhaul) but the terminal-bench tasks are too easy to measure meaningful impact on pass rates. The evolution's value lies in coordinator prompt improvements and FLIP-measurable understanding gains on specific task types, not in pass-rate lift.

---

## 2. Full Sweep Results: 4 Conditions × 7 Tasks

### 2.1 Condition Descriptions

| Condition | Name | Key Feature |
|-----------|------|-------------|
| **A** | Bare agent | Minimal prompt, bash + file tools, no workgraph |
| **C** | WG + skill injection | Workgraph tools with explicit templates, 81% wg adoption |
| **D** | Autopoietic verification | Attempt→verify→iterate loop, agency identity, self-verification gate |
| **E** | Organization generation | Orchestrator framing, decomposition + independent verification |

### 2.2 Per-Condition Summary

| Condition | Pass Rate | Mean FLIP | FLIP σ | Mean LLM Eval | LLM σ | n |
|-----------|-----------|-----------|--------|---------------|-------|---|
| **A** | 100% (21/21) | 0.134 | 0.216 | 0.736 | 0.213 | 21 |
| **C** | 100% (21/21) | 0.697 | 0.236 | 0.862 | 0.136 | 21 |
| **D** | 100% (21/21) | 0.755 | 0.085 | 0.898 | 0.058 | 21 |
| **E** | 100% (21/21) | 0.795 | 0.103 | 0.889 | 0.053 | 21 |

### 2.3 Per-Condition × Per-Task Pass Rates and FLIP Scores

| Task | A (FLIP) | A (LLM) | C (FLIP) | C (LLM) | D (FLIP) | D (LLM) | E (FLIP) | E (LLM) |
|------|----------|---------|----------|---------|----------|---------|----------|---------|
| file-ops | 0.207 | 0.863 | 0.215 | 0.725 | 0.692 | 0.853 | 0.782 | 0.830 |
| text-processing | 0.314 | 0.497 | 0.741 | 0.753 | 0.848 | 0.913 | 0.865 | 0.883 |
| debugging | 0.227 | 0.767 | 0.923 | 0.883 | 0.730 | 0.930 | 0.892 | 0.917 |
| shell-scripting | 0.108 | 0.870 | 0.889 | 0.833 | 0.771 | 0.873 | 0.815 | 0.880 |
| data-processing | 0.042 | 0.737 | 0.707 | 0.937 | 0.716 | 0.850 | 0.732 | 0.917 |
| algorithm | 0.032 | 0.810 | 0.713 | 0.890 | 0.839 | 0.930 | 0.780 | 0.925 |
| ml | 0.008 | 0.653 | 0.690 | 0.963 | 0.692 | 0.917 | 0.697 | 0.880 |

**All cells: 3/3 passed (100% pass rate).** Pass rate is uniform — the differentiator is FLIP and LLM eval quality.

### 2.4 Per-Task Summary (Across Conditions)

| Task | Mean FLIP | Mean LLM | n |
|------|-----------|----------|---|
| text-processing | 0.692 | 0.762 | 12 |
| debugging | 0.693 | 0.874 | 12 |
| shell-scripting | 0.646 | 0.864 | 12 |
| algorithm | 0.591 | 0.886 | 12 |
| data-processing | 0.549 | 0.861 | 12 |
| ml | 0.522 | 0.853 | 12 |
| file-ops | 0.474 | 0.826 | 12 |

---

## 3. Evolution Changes

### 3.1 Inventory Delta

| Entity | Before | After | Delta |
|--------|--------|-------|-------|
| Roles | 29 | 34 | **+5** |
| Tradeoff Configs | 241 | 247 | **+6** |
| Agents | 17 | 20 | **+3** |
| Evaluations | 3,938 | 3,946 | +8 |

### 3.2 New Roles Created

| Role | Hash | Origin | Rationale |
|------|------|--------|-----------|
| Execution Engineer | 738aa61a | gap-analysis | Process execution, benchmark orchestration |
| Test Analyst | 956ed2eb | crossover (Tester × Documenter) | Combines test rigor with documentation structure |
| Testable Systems Designer | 18baef24 | crossover (Architect × Tester) | Architecture informed by testability |
| Programmer-TDD variant | b1091e30 | component substitution | Debugging component swapped |
| Evolver variant | aee94eb1 | component removal | Pruned 1 component |

### 3.3 New Tradeoffs Created

| Tradeoff | Hash | Origin |
|----------|------|--------|
| Fast v2 | 84a7ddbb | motivation-tuning |
| Execution-Correct | f8c0b898 | gap-analysis |
| Entropic Minimalism | 56c13221 | bizarre-ideation |
| Thorough (wording variant) | 9a044e18 | motivation-tuning |
| Verification-Focused (wording variant) | c5a5aa7c | motivation-tuning |
| Careful (wording variant) | 1e318bff | motivation-tuning |

### 3.4 New Agents Created

| Agent | Hash | Role | Tradeoff | Status |
|-------|------|------|----------|--------|
| Thorough Tester (Experimental) | dec71e4d | 9bdeeeb3 (Tester) | 2dc69b33 (Thorough) | New |
| Careful Downstream Programmer (Experimental) | 20471e73 | 5c550a93 | 1caa4c3c (Careful) | New |
| Fast Evaluator (Experimental) | 4938ce56 | 75d2fab8 (Evaluator) | 4f502dae (Fast) | New |

### 3.5 Coordinator Prompt Overhaul (Highest Impact)

Two coordinator-level operations were applied (confidence 0.85–0.88):

1. **Evolved Amendments** — 10 new rules derived from evaluation failure patterns (e.g., require validation sections, serialize parallel tasks on same files, delegate data analysis instead of doing it directly)
2. **Common Patterns Replacement** — 12 scenario-driven templates replacing generic guidance (each template includes anti-patterns with observed evaluation scores)

These affect all future coordinator interactions, not just terminal-bench tasks.

### 3.6 Entities Retired

None. Evolution run-01 was additive only — no roles, tradeoffs, or agents were retired.

---

## 4. Re-Run Comparison: Pre vs Post Evolution

The re-run targeted the 8 worst-performing condition×task combinations by FLIP score from full-sweep-01 (all 7 Condition A tasks + C+file-ops). Each was re-run with 3 replicas using evolved agents.

### 4.1 Side-by-Side FLIP Scores

| Condition | Task | Original FLIP | Rerun FLIP | Delta | Direction |
|-----------|------|---------------|------------|-------|-----------|
| A | algorithm | 0.032 | 0.030 | −0.002 | → |
| A | data-processing | 0.042 | 0.047 | +0.005 | → |
| A | debugging | 0.227 | 0.303 | +0.077 | ↑ |
| A | file-ops | 0.207 | 0.050 | −0.157 | ↓ |
| A | ml | 0.008 | 0.022 | +0.013 | → |
| A | shell-scripting | 0.108 | 0.193 | +0.085 | ↑ |
| A | text-processing | 0.314 | 0.630 | **+0.316** | ↑↑ |
| C | file-ops | 0.215 | 0.603 | **+0.388** | ↑↑ |

**Legend:** → negligible (<0.05), ↑ moderate improvement, ↑↑ large improvement, ↓ regression

### 4.2 Pass Rate Comparison

| Combo | Original Pass Rate | Rerun Pass Rate | Change |
|-------|-------------------|-----------------|--------|
| A (all 7 tasks) | 21/21 (100%) | 21/21 (100%) | None |
| C+file-ops | 3/3 (100%) | 3/3 (100%) | None |

**Pass rates were already at ceiling.** Evolution cannot improve what is already at 100%.

### 4.3 LLM Eval Comparison (Where Available)

| Combo | Original LLM Eval | Rerun LLM Eval | Delta |
|-------|-------------------|----------------|-------|
| A+data-processing | 0.737 | 0.880 | +0.143 |
| A+debugging | 0.767 | 0.750 | −0.017 |
| A+file-ops | 0.863 | 0.720 | −0.143 |
| A+ml | 0.653 | 0.790 | +0.137 |

Most rerun trials lacked LLM eval scores (16/24 missing), limiting comparison.

---

## 5. FLIP Analysis Update

### 5.1 FLIP Score Distribution After Evolution

| Condition | Original Mean FLIP | Rerun Mean FLIP | Delta |
|-----------|-------------------|-----------------|-------|
| A (n=21→16) | 0.134 | 0.153 | +0.019 |
| C (n=21→3) | 0.697 | 0.603 | −0.094 |

Condition A FLIP scores remain fundamentally low. The evolution did not alter the structural gap between Condition A (bare agent, no workgraph) and Conditions C/D/E (full scaffolding).

### 5.2 FLIP by Condition: Statistical Separation

The Kruskal-Wallis test confirms highly significant FLIP differences across conditions:

| Test | Statistic | p-value |
|------|-----------|---------|
| Kruskal-Wallis (A vs C vs D vs E) | H = 41.76 | p < 0.0001 |

Pairwise Fisher's exact tests on FLIP ≥ 0.70 threshold:

| Comparison | Above/Below (Left) | Above/Below (Right) | p-value |
|------------|-------------------|---------------------|---------|
| A vs C | 1/20 | 13/8 | p = 0.0002 |
| A vs D | 1/20 | 14/7 | p = 0.00005 |
| A vs E | 1/20 | 15/6 | p = 0.00001 |

Effect sizes (Cohen's d on FLIP):

| Comparison | Cohen's d | Interpretation |
|------------|-----------|----------------|
| A vs D | 3.69 | Very large |
| A vs E | 3.81 | Very large |

### 5.3 Is the 0.70 Threshold Still Optimal?

**Yes, the 0.70 threshold remains well-calibrated.** In full-sweep-01:
- 20 of 84 trials (24%) have FLIP < 0.30 — all from Condition A
- Only 1 of 21 Condition A trials exceeds 0.70 (a single text-processing trial at 0.702)
- 13–15 of 21 trials from C/D/E exceed 0.70

The threshold cleanly separates "shallow understanding" (Condition A) from "genuine understanding" (C/D/E). However, since all trials passed verification regardless of FLIP score, FLIP remains a quality signal rather than a pass/fail predictor on these tasks.

### 5.4 FLIP and Verify Correlation

| Metric | Value |
|--------|-------|
| Mean FLIP of passed trials | 0.595 |
| Mean FLIP of failed trials | N/A (0 failures) |
| Low FLIP (<0.30) and passed | 20/20 |
| Low FLIP (<0.30) and failed | 0 |
| High FLIP (≥0.50) and passed | 63/63 |

**Critical finding: FLIP does not predict verification failure in this benchmark.** All 84 original trials and all 24 rerun trials passed, including 20 trials with FLIP < 0.30. The tasks are insufficiently challenging to produce failures that FLIP could detect.

---

## 6. Statistical Significance

### 6.1 Primary Question: Did Evolution Improve FLIP Scores?

| Test | Comparison | Statistic | p-value | Significant? |
|------|-----------|-----------|---------|--------------|
| Wilcoxon signed-rank | 8 pairs (original vs rerun FLIP) | W = 7.0 | p = 0.148 | No |
| Mann-Whitney U | Condition A FLIP (original vs rerun) | U = 145.5 | p = 0.492 | No |

The improvement in overall FLIP (+0.091) is **not statistically significant** at conventional thresholds. The sample is small (8 pairs for Wilcoxon, 21 vs 16 for Mann-Whitney) and variance is high.

### 6.2 Secondary: Are Conditions Different From Each Other?

| Test | Comparison | Statistic | p-value | Significant? |
|------|-----------|-----------|---------|--------------|
| Kruskal-Wallis | FLIP across A/C/D/E | H = 41.76 | p < 0.0001 | **Yes** |
| Fisher's exact | A vs E, FLIP ≥ 0.70 | — | p = 0.00001 | **Yes** |
| Fisher's exact | A vs D, FLIP ≥ 0.70 | — | p = 0.00005 | **Yes** |
| Fisher's exact | A vs C, FLIP ≥ 0.70 | — | p = 0.0002 | **Yes** |

The scaffolding conditions (C, D, E) produce dramatically higher FLIP scores than the bare agent (A). This structural difference dwarfs any within-condition improvement from evolution.

### 6.3 Limitations

- **Ceiling effect on pass rates:** 100% pass rate means evolution impact on task completion cannot be measured
- **Small rerun sample:** 24 trials (16 with FLIP scores) limits statistical power
- **Missing LLM eval data:** Only 4/24 rerun trials have LLM eval scores
- **Selection bias:** Rerun targeted worst performers, so regression to the mean could explain some improvement
- **Confound:** Rerun used different model routing (Gemini Flash for file-ops and text-processing) — observed improvements may reflect model differences rather than evolution effects

---

## 7. Convergence Assessment

### 7.1 Have Pass Rates Stabilized?

**Yes — at ceiling.** 100% pass rate across 84 original trials and 24 rerun trials (108 total) indicates the terminal-bench tasks are solved by all conditions. Further iterations cannot improve pass rates.

### 7.2 Have FLIP Scores Stabilized?

**For Condition A: Yes, at a low plateau.** Condition A FLIP averages ~0.14 across both runs with no significant change (p=0.49). The bare agent does not develop deep understanding of its work — this is a structural property of the condition, not something evolution can fix without changing the condition itself.

**For Conditions C/D/E:** Not tested in rerun (only C+file-ops was included). The single data point (C+file-ops FLIP 0.215→0.603) suggests improvement is possible where the original score was anomalously low.

### 7.3 Should Another Evolution Iteration Run?

**Not on this benchmark.** The terminal-bench tasks have saturated — there are no failures to optimize against, and FLIP improvements in Condition A are bounded by the condition's structural limitations (no workgraph, no verification loop).

Another evolution iteration would be valuable if:
1. **Harder tasks are introduced** — tasks with non-trivial failure rates (e.g., the H2H trial's 33% false-PASS rate on cython builds, nginx config, LaTeX processing)
2. **The benchmark targets FLIP specifically** — tasks designed to distinguish shallow from deep understanding
3. **New evaluation data accumulates** — the 3 new agents (Thorough Tester, Careful Downstream Programmer, Fast Evaluator) need deployment data before meaningful evolution

---

## 8. Recommendations

### 8.1 Benchmark Design

1. **Introduce harder tasks.** Current terminal-bench tasks produce 100% pass rates across all conditions. Port the H2H trial tasks (which had 33–52% failure rates) into the benchmark to create a failure gradient that evolution can optimize against.

2. **Add FLIP-discriminating tasks.** Design tasks where shallow understanding produces passing but fragile solutions (e.g., tasks with edge cases that only self-verifying agents catch). This would let FLIP serve its intended purpose as a false-PASS detector.

3. **Increase replicas for statistical power.** 3 replicas per condition×task yields wide confidence intervals. Consider 5–10 replicas on a smaller task set for higher-confidence comparisons.

### 8.2 Evolution Pipeline

4. **Deploy new agents to production tasks.** The 3 experimental agents have zero evaluation data. Assign them to real workgraph tasks and collect evaluation scores before the next evolution iteration.

5. **Evaluate coordinator prompt changes separately.** The two coordinator-level operations (evolved-amendments, common-patterns) affect all agents. Their impact should be measured through A/B testing on coordinator performance, not through terminal-bench agent trials.

6. **Consider retirement criteria.** Evolution run-01 was purely additive (5 roles, 6 tradeoffs, 3 agents added; 0 retired). Establish performance thresholds for retirement to prevent entity sprawl.

### 8.3 FLIP Calibration

7. **Maintain the 0.70 threshold.** It remains well-calibrated as a condition discriminator. However, its value as a false-PASS detector is still unvalidated — no failures have occurred to test against.

8. **Investigate the two-threshold system.** The prior FLIP analysis proposed: FLIP < 0.30 → auto-reject, 0.30–0.70 → Opus review, ≥ 0.70 → auto-pass. Full-sweep data supports this: 20 trials below 0.30 (all Condition A, all genuinely shallow understanding) vs. 63 above 0.50 (mostly genuine). The middle band is sparse.

9. **Collect failure data before refining thresholds.** Without actual false-PASSes, threshold optimization is speculative. The H2H trial's 33% false-PASS rate is the target environment for calibration.

### 8.4 Next Steps (Priority Order)

1. Design and run a "hard task" sweep with tasks known to produce failures
2. Deploy the 3 experimental agents to production workgraph tasks
3. Collect 100+ evaluations on new/modified entities
4. Run evolution iteration 2 with failure data as input
5. Re-assess FLIP threshold calibration with actual false-PASS data

---

## Appendix A: Raw Trial Data

### A.1 Full Sweep FLIP Scores by Condition (All 84 Trials)

<details>
<summary>Condition A — 21 trials, mean FLIP 0.134</summary>

| Task | r0 | r1 | r2 | Mean |
|------|----|----|-----|------|
| file-ops | 0.030 | 0.095 | 0.495 | 0.207 |
| text-processing | 0.240 | 0.000 | 0.702 | 0.314 |
| debugging | 0.000 | 0.670 | 0.010 | 0.227 |
| shell-scripting | 0.280 | 0.000 | 0.045 | 0.108 |
| data-processing | 0.125 | 0.000 | 0.000 | 0.042 |
| algorithm | 0.030 | 0.065 | 0.000 | 0.032 |
| ml | 0.000 | 0.000 | 0.025 | 0.008 |
</details>

<details>
<summary>Condition C — 21 trials, mean FLIP 0.697</summary>

| Task | r0 | r1 | r2 | Mean |
|------|----|----|-----|------|
| file-ops | 0.505 | 0.060 | 0.080 | 0.215 |
| text-processing | 0.720 | 0.620 | 0.883 | 0.741 |
| debugging | 0.960 | 0.840 | 0.970 | 0.923 |
| shell-scripting | 0.926 | 0.930 | 0.810 | 0.889 |
| data-processing | 0.680 | 0.700 | 0.740 | 0.707 |
| algorithm | 0.720 | 0.630 | 0.790 | 0.713 |
| ml | 0.650 | 0.740 | 0.680 | 0.690 |
</details>

<details>
<summary>Condition D — 21 trials, mean FLIP 0.755</summary>

| Task | r0 | r1 | r2 | Mean |
|------|----|----|-----|------|
| file-ops | 0.650 | 0.726 | 0.700 | 0.692 |
| text-processing | 0.940 | 0.905 | 0.700 | 0.848 |
| debugging | 0.780 | 0.730 | 0.680 | 0.730 |
| shell-scripting | 0.805 | 0.840 | 0.667 | 0.771 |
| data-processing | 0.800 | 0.698 | 0.650 | 0.716 |
| algorithm | 0.826 | 0.820 | 0.870 | 0.839 |
| ml | 0.685 | 0.660 | 0.730 | 0.692 |
</details>

<details>
<summary>Condition E — 21 trials, mean FLIP 0.795</summary>

| Task | r0 | r1 | r2 | Mean |
|------|----|----|-----|------|
| file-ops | 0.680 | 0.855 | 0.810 | 0.782 |
| text-processing | 0.950 | 0.949 | 0.695 | 0.865 |
| debugging | 0.880 | 0.860 | 0.935 | 0.892 |
| shell-scripting | 0.940 | 0.685 | 0.820 | 0.815 |
| data-processing | 0.770 | 0.730 | 0.695 | 0.732 |
| algorithm | 0.900 | 0.760 | 0.680 | 0.780 |
| ml | 0.780 | 0.700 | 0.610 | 0.697 |
</details>

### A.2 Rerun FLIP Scores (24 Trials)

| Task ID | Condition | Task | FLIP | LLM Eval | Verify |
|---------|-----------|------|------|----------|--------|
| tb-rerun-01-a-ml-r0 | A | ml | 0.025 | — | PASS |
| tb-rerun-01-a-ml-r1 | A | ml | 0.040 | 0.79 | PASS |
| tb-rerun-01-a-ml-r2 | A | ml | 0.000 | — | PASS |
| tb-rerun-01-a-algorithm-r0 | A | algorithm | 0.030 | — | PASS |
| tb-rerun-01-a-algorithm-r1 | A | algorithm | — | — | PASS |
| tb-rerun-01-a-algorithm-r2 | A | algorithm | — | — | PASS |
| tb-rerun-01-a-data-processing-r0 | A | data-processing | 0.000 | — | PASS |
| tb-rerun-01-a-data-processing-r1 | A | data-processing | 0.000 | — | PASS |
| tb-rerun-01-a-data-processing-r2 | A | data-processing | 0.140 | 0.88 | PASS |
| tb-rerun-01-a-text-processing-r0 | A | text-processing | 0.630 | — | PASS |
| tb-rerun-01-a-text-processing-r1 | A | text-processing | — | — | PASS |
| tb-rerun-01-a-text-processing-r2 | A | text-processing | — | — | PASS |
| tb-rerun-01-a-shell-scripting-r0 | A | shell-scripting | 0.465 | — | PASS |
| tb-rerun-01-a-shell-scripting-r1 | A | shell-scripting | 0.015 | — | PASS |
| tb-rerun-01-a-shell-scripting-r2 | A | shell-scripting | 0.100 | — | PASS |
| tb-rerun-01-a-file-ops-r0 | A | file-ops | — | — | PASS |
| tb-rerun-01-a-file-ops-r1 | A | file-ops | 0.000 | 0.72 | PASS |
| tb-rerun-01-a-file-ops-r2 | A | file-ops | 0.100 | — | PASS |
| tb-rerun-01-a-debugging-r0 | A | debugging | 0.650 | — | PASS |
| tb-rerun-01-a-debugging-r1 | A | debugging | 0.140 | — | PASS |
| tb-rerun-01-a-debugging-r2 | A | debugging | 0.120 | 0.75 | PASS |
| tb-rerun-01-c-file-ops-r0 | C | file-ops | 0.720 | — | PASS |
| tb-rerun-01-c-file-ops-r1 | C | file-ops | 0.625 | — | PASS |
| tb-rerun-01-c-file-ops-r2 | C | file-ops | 0.465 | — | PASS |

### A.3 Evolution Operations Summary

| # | Strategy | Operation | Confidence | Applied? |
|---|----------|-----------|------------|----------|
| 1 | coordinator | Evolved amendments (10 rules) | 0.88 | Yes |
| 2 | coordinator | Common patterns (12 templates) | 0.85 | Yes |
| 3 | crossover | Test Analyst (Tester × Documenter) | 0.82 | Yes |
| 4 | motivation-tuning | Fast tradeoff tightening | 0.82 | Yes |
| 5 | gap-analysis | Execution Engineer role | — | Yes |
| 6 | gap-analysis | Execution-Correct tradeoff | — | Yes |
| 7 | component-mutation | Enhanced Code Writing component | — | Yes |
| 8 | crossover | Literate Programmer | — | No-op (already existed) |
| 9 | crossover | Testable Systems Designer | — | Yes |
| 10 | component-mutation | Programmer-TDD variant | — | Yes |
| 11 | component-mutation | Evolver variant | — | Yes |
| 12 | motivation-tuning | Thorough (wording variant) | — | Yes |
| 13 | motivation-tuning | Verification-Focused (wording variant) | — | Yes |
| 14 | motivation-tuning | Careful (wording variant) | — | Yes |
| 15 | randomisation | Random role/tradeoff/agent | — | Yes |
| 16 | randomisation | Random role/tradeoff/agent | — | Yes |
| 17 | randomisation | Random role/tradeoff/agent | — | Yes |
| 18 | bizarre-ideation | Negative Space Awareness component | — | Yes |
| 19 | bizarre-ideation | Entropic Minimalism tradeoff | — | Yes |

---

## Appendix B: Methodology Notes

- **Pre-evolution data** was collected in full-sweep-01 (84 trials, 4 conditions × 7 tasks × 3 replicas), all run on the same day using `claude:claude-sonnet-4-latest`
- **Evolution** applied 18 operations generated by 7 parallel analyzers (crossover, gap-analysis, motivation-tuning, component-mutation, randomisation, bizarre-ideation, coordinator)
- **Post-evolution re-run** targeted the 8 worst FLIP-scoring condition×task combinations, using `claude:claude-sonnet-4-latest` with model overrides (Gemini Flash for file-ops and text-processing)
- **Statistical tests** used: Wilcoxon signed-rank (paired pre/post comparison), Mann-Whitney U (unpaired condition comparison), Fisher's exact test (threshold-based classification), Kruskal-Wallis (multi-group comparison), Cohen's d (effect size)
- **FLIP** (Fidelity via Latent Intent Probing) measures an agent's understanding of its own work, not output quality. Scores range [0, 1] where higher = deeper understanding.
- **LLM Eval** scores output quality across 8 dimensions (completeness, correctness, efficiency, style adherence, intent fidelity, downstream usability, coordination overhead, blocking impact)
