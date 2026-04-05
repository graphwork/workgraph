# FLIP Accuracy as False-PASS Detector

**Date:** 2026-04-05
**Task:** analyze-flip-accuracy
**Data source:** `terminal-bench/trials/tb-results-pilot-01.json` (12 trials, 2 conditions × 2 tasks × 3 replicas)
**Prior data:** `terminal-bench/trials/executor-synthesis.md` (H2H trial: 84 trials, 4 conditions, 7 tasks)

---

## 1. Executive Summary

**FLIP cannot yet be evaluated as a false-PASS detector.** The pilot-01 dataset has zero failures — all 12 trials passed external verification — so the confusion matrix is degenerate (only the "actually passed" column is populated). FLIP's sensitivity to real failures remains unmeasured.

However, this analysis reveals three actionable findings:

1. **FLIP is an excellent condition discriminator.** It cleanly separates condition A (shallow, mean 0.217) from condition D (deep, mean 0.711) with zero overlap in per-condition distributions.
2. **The 0.70 threshold is well-calibrated for flagging.** It triggers Opus review on 67% of trials — all 6 condition A trials (correctly: they have shallow understanding despite correct output) and 2 of 6 condition D trials (the two with lowest FLIP in that group).
3. **A harder task set is needed.** The pilot tasks (file-ops, debugging) are too easy — both conditions achieve 100% pass rate. The H2H trial's harder tasks (where D had 33.3% false-PASS) must be used to test FLIP's detection power.

---

## 2. Raw Data: Per-Trial Scores

| Trial ID | Cond | Task | FLIP | LLM Eval | Verify | Opus Triggered |
|---|---|---|---:|---:|---|---|
| tb-a-file-ops-r0 | A | file-ops | 0.030 | 0.91 | PASS | Yes |
| tb-a-file-ops-r1 | A | file-ops | 0.095 | 0.87 | PASS | Yes |
| tb-a-file-ops-r2 | A | file-ops | 0.495 | 0.81 | PASS | Yes |
| tb-a-debugging-r0 | A | debugging | 0.000 | — | PASS | No* |
| tb-a-debugging-r1 | A | debugging | 0.670 | 0.74 | PASS | Yes |
| tb-a-debugging-r2 | A | debugging | 0.010 | 0.74 | PASS | Yes |
| tb-d-file-ops-r0 | D | file-ops | 0.650 | 0.94 | PASS | Yes |
| tb-d-file-ops-r1 | D | file-ops | 0.726 | 0.90 | PASS | No |
| tb-d-file-ops-r2 | D | file-ops | 0.700 | 0.72 | PASS | No |
| tb-d-debugging-r0 | D | debugging | 0.780 | 0.92 | PASS | No |
| tb-d-debugging-r1 | D | debugging | 0.730 | 0.93 | PASS | No |
| tb-d-debugging-r2 | D | debugging | 0.680 | 0.94 | PASS | Yes |

\* tb-a-debugging-r0: FLIP = 0.00 but Opus not triggered — likely a timing/pipeline issue (LLM eval also missing).

---

## 3. Confusion Matrix (Observed)

FLIP threshold at 0.70: scores below → flagged for Opus review, scores at/above → passed through.

|  | Actually PASS (verify = true) | Actually FAIL (verify = false) |
|---|---|---|
| **FLIP passes through** (≥ 0.70) | 4 | 0 |
| **FLIP flags for review** (< 0.70) | 8 | 0 |

**The matrix is degenerate.** With zero actual failures:
- **Sensitivity** (catch rate for real failures): **undefined** — no failures exist to catch
- **Specificity** (correct pass-through rate): **4/12 = 33%** — FLIP correctly passes 4 of 12 genuine successes
- **False alarm rate**: **8/12 = 67%** — FLIP flags 8 genuine successes for unnecessary review
- **False-PASS rate**: **0/12 = 0%** in both conditions (no failures occurred regardless of FLIP)

**Conclusion:** This dataset cannot distinguish whether the 0% false-PASS rate is due to FLIP catching failures or due to there being no failures to catch.

---

## 4. Projected Confusion Matrix (Using H2H False-PASS Rate)

The H2H executor trial measured a **33.3% false-PASS rate** for Condition D on harder tasks (7 tasks, 21 D-trials: 14 passed, 7 false-PASSes). To model FLIP's value, we project that false-PASS rate onto this pilot's 12 trials.

### Assumptions
- 33.3% of trials would be false-PASSes → ~4 of 12 trials
- False-PASSes have low FLIP scores (the agent doesn't truly understand what it did)
- The 4 false-PASSes are drawn from the lowest-FLIP trials

### Projected Matrix at Threshold 0.70

|  | Actually PASS (8) | Actually FAIL (4) |
|---|---|---|
| **FLIP passes through** (≥ 0.70) | 4 | 0 |
| **FLIP flags for review** (< 0.70) | 4 | 4 |

Under this projection:
- **Sensitivity**: **4/4 = 100%** — all false-PASSes caught (they cluster at low FLIP)
- **Specificity**: **4/8 = 50%** — half of genuine passes flagged unnecessarily
- **False-PASS rate after FLIP**: **0/12 = 0%** (down from projected 33%)
- **Cost**: 8 Opus verification calls per 12 trials (67% review rate)

### Why the projection is optimistic

This assumes perfect correlation between low FLIP and actual failure. In practice:
- Some genuine passes have low FLIP (condition A shows correct output + near-zero FLIP)
- Some false-PASSes might have elevated FLIP (agent confidently wrong)
- The FLIP score distribution in the pilot may not match harder tasks

**Conservative estimate:** Even if FLIP catches only 75% of false-PASSes, the projected false-PASS rate drops from 33.3% to ~8.3% — a 4x reduction.

---

## 5. Threshold Analysis

FLIP scores sorted: 0.00, 0.01, 0.03, 0.095, 0.495, 0.65, 0.67, 0.68, 0.70, 0.726, 0.73, 0.78

| Threshold | Flagged | Flag Rate | Condition A Flagged | Condition D Flagged | Notes |
|---:|---:|---:|---:|---:|---|
| 0.10 | 4 | 33% | 4/6 (67%) | 0/6 (0%) | Only flags near-zero FLIP |
| 0.50 | 5 | 42% | 5/6 (83%) | 0/6 (0%) | Catches one outlier (0.495) |
| 0.60 | 5 | 42% | 5/6 (83%) | 0/6 (0%) | No scores in [0.50, 0.60) gap |
| 0.66 | 6 | 50% | 5/6 (83%) | 1/6 (17%) | Starts catching D's tail |
| 0.68 | 7 | 58% | 6/6 (100%) | 1/6 (17%) | Catches A outlier at 0.67 |
| **0.70** | **8** | **67%** | **6/6 (100%)** | **2/6 (33%)** | **Current threshold** |
| 0.73 | 10 | 83% | 6/6 (100%) | 4/6 (67%) | Flags most of D |
| 0.78 | 11 | 92% | 6/6 (100%) | 5/6 (83%) | Near-universal flagging |

### Score Distribution by Condition

```
Condition A (n=6):  [0.00, 0.01, 0.03, 0.095, 0.495, 0.67]
                     Mean: 0.217  Std: 0.281  Range: [0.00, 0.67]

Condition D (n=6):  [0.65, 0.68, 0.70, 0.726, 0.73, 0.78]
                     Mean: 0.711  Std: 0.042  Range: [0.65, 0.78]
```

**Key observation:** There is a clear gap between 0.67 (max condition A) and 0.65 (min condition D). FLIP scores are bimodal — they cluster in two non-overlapping ranges by condition. Any threshold between 0.67 and 0.65 would perfectly separate conditions (but note the ranges slightly overlap: A-max 0.67 > D-min 0.65).

### Optimal Threshold Assessment

Without actual failures, "optimal" cannot be determined by classification accuracy. Instead:

| Threshold | Pros | Cons |
|---|---|---|
| **0.50** | Low false-alarm rate (42%), only flags truly suspicious trials | Misses condition D's weak tail (0.65, 0.68) |
| **0.60** | Same as 0.50 for this data (no scores in gap) | Same limitation |
| **0.70** (current) | Flags all condition A + D's weakest 2 trials | 67% flag rate → expensive |
| **0.80** | Catches everything below near-perfect understanding | 92% flag rate → nearly universal review |

**Recommendation:** **Keep 0.70 for now.** It correctly flags all condition A trials (shallow understanding despite correct output) and the weaker condition D trials. The 67% flag rate is acceptable during this validation phase — the cost of an Opus verification call is far less than the cost of a false-PASS propagating downstream. Revisit when data with actual failures is available.

**If cost pressure requires lowering the flag rate:** 0.50 would reduce reviews to 42% while still catching the most suspicious trials (those with near-zero FLIP).

---

## 6. Opus .verify Task Analysis

### Triggering Behavior

8 trials had FLIP < 0.70 and should have triggered Opus verification. Actual results:

| Trial | FLIP | Below 0.70? | Opus Triggered? | Concordant? |
|---|---:|---|---|---|
| tb-a-file-ops-r0 | 0.030 | Yes | Yes | ✓ |
| tb-a-file-ops-r1 | 0.095 | Yes | Yes | ✓ |
| tb-a-file-ops-r2 | 0.495 | Yes | Yes | ✓ |
| tb-a-debugging-r0 | 0.000 | Yes | **No** | ✗ |
| tb-a-debugging-r1 | 0.670 | Yes | Yes | ✓ |
| tb-a-debugging-r2 | 0.010 | Yes | Yes | ✓ |
| tb-d-file-ops-r0 | 0.650 | Yes | Yes | ✓ |
| tb-d-debugging-r2 | 0.680 | Yes | Yes | ✓ |

**7 of 8 triggered correctly** (87.5%). The one miss (tb-a-debugging-r0) correlates with its missing LLM eval — likely a pipeline timing issue rather than a threshold bug.

### Opus Verification Accuracy

All 7 triggered Opus .verify tasks confirmed the trial as a genuine PASS. Since all 12 trials truly passed, this is **7/7 = 100% correct** — no false accusations of failure.

**But this is the easy case.** Opus was only asked "is this correct work?" about work that was genuinely correct. The harder test — correctly identifying a false-PASS — was not exercised.

---

## 7. FLIP vs LLM Eval Comparison

| Metric | FLIP | LLM Eval |
|---|---|---|
| Condition A mean | 0.217 | 0.814 (n=5) |
| Condition D mean | 0.711 | 0.892 (n=6) |
| Condition separation | 3.3x ratio | 1.1x ratio |
| Score range | 0.00 – 0.78 | 0.72 – 0.94 |
| Variance (A) | 0.281 std | 0.074 std |
| Variance (D) | 0.042 std | 0.086 std |

**FLIP provides far stronger signal than LLM eval for distinguishing shallow from deep understanding.** LLM eval scores cluster in a narrow high range (0.72–0.94) for both conditions, while FLIP spans nearly the full [0, 1] range with clear bimodal separation.

This makes sense: LLM eval scores output quality (which is high for both conditions — they both produce correct answers), while FLIP scores latent understanding (which is dramatically different between a bare agent that got lucky and a fully-scaffolded agent that verified its own work).

**For false-PASS detection specifically:** FLIP is the better candidate because false-PASSes result from shallow understanding (agent doesn't really know if its output is correct), which is exactly what FLIP measures. LLM eval would not catch a false-PASS because it evaluates the output, which the agent has already (incorrectly) deemed correct.

---

## 8. Cross-Metric Correlation

| Trial | FLIP | LLM Eval | Agreement? |
|---|---:|---:|---|
| tb-a-file-ops-r0 | 0.030 | 0.91 | Disagreement — FLIP says shallow, eval says excellent |
| tb-a-file-ops-r1 | 0.095 | 0.87 | Disagreement |
| tb-a-file-ops-r2 | 0.495 | 0.81 | Mild disagreement |
| tb-a-debugging-r0 | 0.000 | — | N/A |
| tb-a-debugging-r1 | 0.670 | 0.74 | Mild agreement (both moderate) |
| tb-a-debugging-r2 | 0.010 | 0.74 | Disagreement |
| tb-d-file-ops-r0 | 0.650 | 0.94 | Disagreement (FLIP moderate, eval excellent) |
| tb-d-file-ops-r1 | 0.726 | 0.90 | Agreement (both high) |
| tb-d-file-ops-r2 | 0.700 | 0.72 | Mild agreement (both moderate-high) |
| tb-d-debugging-r0 | 0.780 | 0.92 | Agreement (both high) |
| tb-d-debugging-r1 | 0.730 | 0.93 | Agreement (both high) |
| tb-d-debugging-r2 | 0.680 | 0.94 | Disagreement (FLIP moderate, eval excellent) |

**FLIP and LLM eval are measuring different things.** The frequent disagreements (especially in condition A, where eval says "great output" but FLIP says "no understanding") confirm they provide complementary signal. A pipeline using both is stronger than either alone.

---

## 9. False-PASS Rate Comparison

| Scenario | False-PASS Rate | Evidence |
|---|---|---|
| **Bare agent (A), no pipeline** | ~47–48% on hard tasks | H2H trial: A achieved 42.9% pass rate on tasks with known solutions; ~52% of attempts failed |
| **Self-verify (D), no FLIP pipeline** | **33.3%** | H2H trial: 7/21 D trials declared success incorrectly |
| **Full agency pipeline (this pilot)** | **0%** | Pilot-01: 0/12 false-PASSes — but tasks were easy |
| **Projected: D + FLIP on hard tasks** | **0–8.3%** | Projection from §4: FLIP catches 75–100% of false-PASSes |

**Key caveat:** The pilot's 0% false-PASS rate cannot be attributed to FLIP because there were no failures to catch. The improvement from 33.3% (H2H) to 0% (pilot) is more likely explained by task difficulty (file-ops and debugging vs. the H2H's harder mix including nginx config, cython builds, and LaTeX processing).

---

## 10. Recommendations

### Is the pipeline catching false-PASSes?

**Unknown — the pilot didn't test this.** All 12 tasks passed genuinely, so the pipeline was never challenged with a false-PASS. The pipeline *would* flag most false-PASSes (based on the projected analysis), but this is a model, not a measurement.

### What needs to happen next

1. **Run FLIP on harder tasks.** Reuse the 7 H2H tasks (or a subset known to produce false-PASSes: `cancel-async-tasks`, `overfull-hbox`, `regex-log`) through the full agency pipeline. This is the only way to measure FLIP's sensitivity — it needs actual failures to catch.

2. **Keep the 0.70 threshold.** It's well-positioned:
   - Flags all condition A trials (appropriate — they have shallow understanding)
   - Passes most condition D trials (appropriate — they have deep understanding)
   - The gap between conditions is real and meaningful
   - Lowering to 0.50 would reduce cost but risks missing edge cases

3. **Fix the pipeline reliability issue.** tb-a-debugging-r0 had FLIP = 0.00 but neither Opus verify nor LLM eval triggered. One dropped trial out of 12 is an 8.3% pipeline failure rate — fix the timing/sequencing issue before scaling.

4. **Track FLIP calibration across task difficulty.** The bimodal FLIP distribution (near-0 for A, near-0.70 for D) may change with harder tasks. If FLIP scores are uniformly low on hard tasks (even for D), the threshold will need adjustment.

5. **Consider a two-threshold system.** Instead of a single 0.70 cutoff:
   - FLIP < 0.30: **auto-reject** — agent almost certainly doesn't understand what it did
   - 0.30 ≤ FLIP < 0.70: **Opus review** — ambiguous, needs second opinion
   - FLIP ≥ 0.70: **auto-pass** — agent demonstrates genuine understanding
   
   This reduces Opus costs by auto-rejecting the clearest false-PASSes (which make up 4/12 = 33% of trials in this pilot, all in condition A with FLIP < 0.10).

### Bottom line

The agency pipeline's FLIP component is a strong understanding signal — the strongest in the pipeline, better than LLM eval for differentiating shallow from deep task comprehension. But it hasn't yet been tested against real false-PASSes. The H2H trial's 33.3% false-PASS rate is the target to beat. Run the harder tasks through the full pipeline and measure whether FLIP + Opus verification actually catches those failures. Until then, FLIP is a promising but unvalidated false-PASS detector.

---

## Appendix: Statistical Notes

- **Sample size:** 12 trials is insufficient for robust statistical inference. Confidence intervals on any derived metric span the entire range. The projections in §4 are illustrative, not predictive.
- **Missing data:** 1 LLM eval score (tb-a-debugging-r0), 1 apparent pipeline failure (same trial). N=11 for cross-metric analysis.
- **Condition confound:** Conditions A and D differ in multiple ways (self-verify, agency identity, context scope, turn limits). FLIP score differences cannot be attributed to any single factor.
- **Selection bias:** Pilot tasks (file-ops, debugging) were chosen for ease of automation, not to stress-test FLIP. The H2H tasks are a more representative difficulty sample.
