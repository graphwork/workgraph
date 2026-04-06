# Pilot Comparison: Condition A vs Condition F on M2.7

**Date:** 2026-04-06
**Model:** openrouter:minimax/minimax-m2.7 (MiniMax M2.7)
**Pilot size:** 5 tasks x 1 replicate each

## Condition Definitions

| Condition | Executor | WG Context | Surveillance Loop | Description |
|-----------|----------|------------|-------------------|-------------|
| **A** | native (wg executor) | None (clean scope) | No | Baseline: agent executes task directly, no workgraph context injected |
| **F** | native (wg executor) | Graph scope + WG Quick Guide | Yes (max 3 iterations, 1m delay) | Full wg-native: agent has wg tools, surveillance agent verifies work after completion |

## Model Verification

| Condition | Requested Model | Verified All M2.7 | Claude Fallback | Executor |
|-----------|-----------------|--------------------|-----------------|----------|
| A | openrouter:minimax/minimax-m2.7 | Yes (5/5) | None detected | native |
| F | openrouter:minimax/minimax-m2.7 | Yes (5/5) | None detected | native |

**Note on A's `all_correct_model=false`:** This is a documented false negative. The native executor strips the `openrouter:` prefix, recording `minimax/minimax-m2.7` in stream logs. The summary.json `note` field confirms this is M2.7. All 5 agents in both conditions used the correct model.

## Per-Task Comparison

### Task 1: file-ops (Easy)

| Metric | A | F |
|--------|---|---|
| **Status** | PASS | PASS |
| **Time (s)** | 22.1 | 51.1 |
| **Turns** | 6 | 13 |
| **Input tokens** | 17,240 | 83,040 |
| **Output tokens** | 1,542 | 2,013 |
| **Total tokens** | 18,782 | 85,053 |
| **Surveillance iterations** | N/A | 0 (converged first try) |
| **Surveillance issues** | N/A | None |

**Analysis:** Both passed. F took 2.3x longer and consumed 4.5x the tokens. The overhead comes from (a) wg context injection (graph scope adds task metadata to every prompt), (b) the surveillance agent running verification separately, and (c) 2 agents spawned vs 1. The surveillance agent confirmed validity but found nothing the built-in `--verify` gate in A wouldn't have caught.

### Task 2: text-processing (Easy)

| Metric | A | F |
|--------|---|---|
| **Status** | PASS | PASS |
| **Time (s)** | 26.7 | 41.8 |
| **Turns** | 7 | 11 |
| **Input tokens** | 18,877 | 65,780 |
| **Output tokens** | 1,661 | 1,708 |
| **Total tokens** | 20,538 | 67,488 |
| **Surveillance iterations** | N/A | 0 (converged first try) |
| **Surveillance issues** | N/A | None |

**Analysis:** Both passed. F took 1.6x longer and consumed 3.3x the tokens. Output token counts are nearly identical (~1.7k), indicating the actual work done by the model was equivalent. The input token difference is entirely context overhead.

### Task 3: debugging (Medium)

| Metric | A | F |
|--------|---|---|
| **Status** | PASS | PASS |
| **Time (s)** | 26.7 | 56.8 |
| **Turns** | 7 | 18 |
| **Input tokens** | 23,096 | 121,322 |
| **Output tokens** | 1,504 | 2,881 |
| **Total tokens** | 24,600 | 124,203 |
| **Surveillance iterations** | N/A | 0 (converged first try) |
| **Surveillance issues** | N/A | None |

**Analysis:** Both passed. F took 2.1x longer and consumed 5.0x the tokens. The debugging task (fixing merge sort bugs) is where surveillance might have the most value on harder problems — but at this difficulty level, M2.7 solved it correctly on the first attempt in both conditions.

### Task 4: data-processing (Medium)

| Metric | A | F |
|--------|---|---|
| **Status** | PASS | PASS |
| **Time (s)** | 46.8 | 71.9 |
| **Turns** | 5 | 17 |
| **Input tokens** | 16,986 | 125,793 |
| **Output tokens** | 2,056 | 3,784 |
| **Total tokens** | 19,042 | 129,577 |
| **Surveillance iterations** | N/A | 0 (converged first try) |
| **Surveillance issues** | N/A | None |

**Analysis:** Both passed. F took 1.5x longer and consumed 6.8x the tokens — the highest token ratio of any task pair. Data processing had the most turns in F (17 vs 5 in A), suggesting the wg context and surveillance setup added substantial per-turn overhead. Despite the overhead, the surveillance loop found no issues.

### Task 5: algorithm (Hard)

| Metric | A | F |
|--------|---|---|
| **Status** | PASS | PASS |
| **Time (s)** | 31.7 | 56.8 |
| **Turns** | 4 | 14 |
| **Input tokens** | 12,958 | 94,296 |
| **Output tokens** | 1,870 | 2,697 |
| **Total tokens** | 14,828 | 96,993 |
| **Surveillance iterations** | N/A | 0 (converged first try) |
| **Surveillance issues** | N/A | None |

**Analysis:** Both passed. The key-value store with transactions — the hardest task — was solved correctly on the first attempt by both conditions. F took 1.8x longer and consumed 6.5x the tokens. This was the task where surveillance loops would have been most valuable if M2.7 had struggled, but it didn't.

## Summary Metrics

| Metric | Condition A | Condition F | Ratio (F/A) |
|--------|-------------|-------------|-------------|
| **Pass rate** | 5/5 (100%) | 5/5 (100%) | 1.0x |
| **Mean time (s)** | 30.8 | 55.7 | 1.8x |
| **Total wall clock (s)** | 154.0 | ~278.4 | 1.8x |
| **Total input tokens** | 89,157 | 490,231 | 5.5x |
| **Total output tokens** | 8,633 | 13,083 | 1.5x |
| **Total tokens** | 97,790 | 503,314 | 5.1x |
| **Mean turns/task** | 5.8 | 14.6 | 2.5x |
| **Agents spawned** | 5 | 10 | 2.0x |
| **Model verified** | 5/5 | 5/5 | - |
| **Surveillance loops activated** | N/A | 0/5 | - |
| **Issues caught by surveillance** | N/A | 0 | - |

### Difficulty Breakdown

| Difficulty | A Pass Rate | A Mean Time | F Pass Rate | F Mean Time | Time Ratio |
|-----------|------------|-------------|------------|-------------|------------|
| Easy (2 tasks) | 100% | 24.4s | 100% | 46.4s | 1.9x |
| Medium (2 tasks) | 100% | 36.7s | 100% | 64.4s | 1.8x |
| Hard (1 task) | 100% | 31.7s | 100% | 56.8s | 1.8x |

## Surveillance Loop Analysis

### Value Assessment

The surveillance loop in condition F added **zero value** across all 5 trials:

- **Loops created:** 5/5 (infrastructure worked correctly)
- **Cycle edges created:** 5/5 (work -> surv -> work back-edges properly formed)
- **Total surveillance iterations across all trials:** 0
- **Trials converged on first try:** 5/5
- **Trials needing retry:** 0/5
- **Issues detected by surveillance:** 0

### Why Surveillance Added No Value Here

1. **M2.7 was too capable for these tasks.** All 5 benchmark problems (easy to hard) were within M2.7's reliable solve range. The model produced correct solutions on the first attempt every time.
2. **The `--verify` gate in condition A already provides automated verification.** The surveillance agent in F essentially re-ran the same verify command. For these tasks, there's no gap between what `--verify` checks and what the surveillance agent checks.
3. **Surveillance value is latent, not absent.** The loop infrastructure works correctly — it would catch errors on harder tasks or with weaker models where the first attempt fails. This pilot doesn't prove surveillance is useless, only that it wasn't needed at this difficulty/model combination.

### Token Cost of Surveillance

The surveillance overhead is significant:

| Component | Estimated Tokens |
|-----------|-----------------|
| WG context injection (per turn) | ~3,000-5,000 extra input tokens |
| Surveillance agent prompts | ~15,000-25,000 input tokens per trial |
| Surveillance agent responses | ~500-1,000 output tokens per trial |
| **Total surveillance overhead** | **~405,000 tokens (5.1x over baseline)** |

## Quality Assessment

Both conditions produced functionally equivalent solutions. Since both used the same model (M2.7) on identical problems with identical verify commands, and all trials passed, there is no quality differentiation. A deeper code-quality analysis would require examining the actual generated files, which were written to `/tmp` and cleaned between trials.

Key observation: **Output token counts are similar** (A: 8,633 total vs F: 13,083 total, only 1.5x). The extra output in F comes from wg commands (`wg log`, `wg done --converged`, etc.) rather than from qualitatively different solutions. The 5.1x total token ratio is almost entirely driven by input token inflation from context injection.

## Threats to Validity

1. **Small sample size (n=1 per task).** With no replicates, we cannot distinguish signal from noise in timing differences. A single outlier could skew the mean.
2. **Easy-to-hard range may not be hard enough.** M2.7's 100% pass rate on "hard" tasks suggests the benchmark ceiling is too low to stress-test surveillance value.
3. **Cost reported as $0.00.** OpenRouter's M2.7 endpoint reported zero cost, so we cannot compare dollar costs.
4. **Sequential execution.** Both pilots ran tasks sequentially (max_agents=1 for A, effective serialization for F). Concurrent execution could change the time ratios.
5. **Surveillance agent uses same verify command.** In a real deployment, the surveillance agent might apply deeper checks (code review, edge case testing). This pilot only tested re-running the verify command, which is the lowest-value surveillance mode.

## Go/No-Go Recommendation for Full 89x3 Experiment

### Recommendation: **CONDITIONAL GO**

**Rationale:**

The pilot establishes that the benchmark infrastructure works correctly for both conditions:
- Model routing is verified: M2.7 is used in both, no Claude fallback
- Environment isolation works: clean `/tmp` between trials, separate wg graphs per trial
- Surveillance loop infrastructure functions: cycles created, edges formed, convergence signaling works
- Data collection pipeline captures all required metrics

**Conditions for proceeding to 89x3:**

1. **Include harder tasks.** The current 5 tasks are too easy for M2.7. The full 89-task set should include tasks where M2.7 is expected to fail ~20-40% of the time — this is where surveillance loops will show their value (or lack thereof).

2. **Consider token cost budget.** At 5.1x token overhead, running 89 tasks x 3 replicates under condition F will cost approximately 5x what condition A costs. Ensure the budget accounts for ~500k tokens/task for F vs ~100k for A (rough estimates from this pilot). Total projected: A ~27M tokens, F ~133M tokens for 89x3.

3. **Add a condition between A and F.** The 5.1x token gap is large. A condition with wg context but *without* the surveillance loop would isolate the cost of context injection vs surveillance. This would clarify whether the overhead is worth the safety net.

4. **Accept that this pilot provides no evidence of surveillance value.** The full experiment needs harder tasks to test the hypothesis. A negative result (surveillance catches nothing even on hard tasks) would still be informative — it would suggest M2.7 is reliable enough to not need a supervisor.

**What could block the go:**
- If the token budget cannot accommodate F's 5x overhead across 89x3
- If the task suite doesn't include problems hard enough to produce failures (rendering surveillance comparison moot)

---

*Generated by pilot-compare-a-vs-f agent. Source data: terminal-bench/results/pilot-a-5x1/summary.json, terminal-bench/results/pilot-f-5x1/summary.json*
