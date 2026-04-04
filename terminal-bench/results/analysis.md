# Terminal-Bench Results Analysis

**Date:** 2026-04-04
**Tasks:** 89 unique tasks × 3 trials each
**Model:** minimax/minimax-m2.7 via OpenRouter

## Executive Summary

**Null result:** The three experimental conditions — bare agent (A), stigmergic workgraph context (B), and enhanced planning + snapshots (C) — achieve statistically indistinguishable pass rates on Terminal-Bench (52.3%, 51.4%, 49.0%; all 95% CIs overlap broadly).

**Key findings:**
1. **No overall effect of workgraph scaffolding.** All conditions solve ~52% of tasks. Pairwise sign tests show no significant difference (p > 0.3 for all pairs).
2. **Tier-specific pattern.** B and C gain +9–10pp on medium-difficulty tasks but lose ~16pp on easy tasks, suggesting wg overhead harms simple tasks while providing marginal benefit on moderately complex ones.
3. **Hard tasks remain hard.** 24/34 hard tasks are never solved by any condition. Workgraph does not unlock new capabilities on tasks beyond the model's reach.
4. **Token efficiency is similar.** All conditions use ~1.2M tokens per solve. C is slightly more efficient (269K tokens/pass vs 310K for A), but the difference is modest.
5. **Decomposition is rare and low-impact.** Only 6–8% of trials use `wg_add`. TB tasks are typically single-scope, making decomposition overhead unjustified.
6. **WG overhead is modest.** WG tool calls consume ~9% of total tool calls — mostly `wg_log` and `wg_done` bookkeeping.

## 1. Overall Pass Rates

| Condition | Description | Valid Trials | Pass | Fail | Error | Pass Rate (trial) | Task Mean ± 95% CI |
|-----------|-------------|-------------|------|------|-------|-------------------|-------------------|
| **A** | Condition A (bare agent) | 225/267 | 121 | 104 | 42 | **53.8%** | 52.3% [43.4, 61.6] |
| **B** | Condition B (stigmergic wg context) | 227/267 | 121 | 106 | 40 | **53.3%** | 51.4% [42.0, 60.4] |
| **C** | Condition C (enhanced skill + planning + snapshots) | 227/263 | 118 | 109 | 36 | **52.0%** | 49.0% [39.4, 58.2] |

**Key finding:** All three conditions achieve similar trial-level pass rates (~51–53%). The task-level mean (averaging per-task pass rates) shows the same pattern.

### Pairwise Differences (task-level mean)

- B − A = -1.0 pp
- C − A = -3.3 pp
- C − B = -2.4 pp

### Paired Sign Test (per-task)

| Comparison | X wins | Y wins | Ties | p-value (two-sided) | Significant? |
|------------|--------|--------|------|--------------------:|-------------|
| A vs B | 17 | 14 | 53 | 0.720 | No |
| A vs C | 15 | 13 | 54 | 0.851 | No |
| B vs C | 14 | 13 | 54 | 1.000 | No |

> A task is an X-win if X's pass rate > Y's on that task (across 3 trials). Sign test excludes ties. None of the comparisons reach significance.

## 2. Pass Rate by Difficulty Tier

Tiers defined by Condition A pass rate: easy (≥67%), medium (33–66%), hard (<33%)

> **Note:** Easy-tier A rate is ~100% by construction (tasks classified as easy because A solves them).
> The interesting comparisons are B and C performance on each tier relative to A.

| Tier | # Tasks | A Rate [95% CI] | B Rate [95% CI] | C Rate [95% CI] | B−A | C−A |
|------|---------|-----------------|-----------------|-----------------|-----|-----|
| Easy | 33 | 100.0% [100, 100] | 83.8% [75, 92] | 83.3% [75, 91] | -16.2 | -16.7 |
| Medium | 22 | 54.5% [47, 61] | 63.6% [48, 77] | 64.4% [48, 80] | +9.1 | +9.8 |
| Hard | 34 | 0.0% [0, 0] | 6.7% [0, 17] | 2.2% [0, 6] | +6.7 | +2.2 |

## 3. Token Efficiency

| Condition | Mean Tokens (all) | Median | Mean (pass) | Mean (fail) | Total Tokens |
|-----------|-------------------|--------|-------------|-------------|-------------|
| **A** | 688,614 | 240,499 | 309,658 | 1,129,514 | 155,053,434 |
| **B** | 653,995 | 211,967 | 322,109 | 1,032,845 | 148,456,759 |
| **C** | 617,926 | 227,012 | 268,966 | 995,700 | 140,269,215 |

### Tokens per Solved Task

- **A**: 1,281,433 tokens/solve (121 solves)
- **B**: 1,226,915 tokens/solve (121 solves)
- **C**: 1,188,722 tokens/solve (118 solves)

## 4. Time Efficiency

| Condition | Mean Duration (all) | Median | Mean (pass) | Mean (fail) |
|-----------|--------------------:|-------:|------------:|------------:|
| **A** | 410s | 240s | 267s | 577s |
| **B** | 400s | 245s | 272s | 547s |
| **C** | 409s | 218s | 284s | 544s |

## 5. Decomposition Analysis (B + C)

| Condition | Valid Trials | Decomposed | Rate | Decomp Pass Rate | No-Decomp Pass Rate | Mean Subtasks |
|-----------|-------------|------------|------|------------------|---------------------|---------------|
| **B** | 227 | 17 | 7.5% | 52.9% | 53.3% | 3.4 |
| **C** | 227 | 13 | 5.7% | 61.5% | 51.4% | 3.5 |

## 6. Planning Analysis (B + C)

| Condition | Valid | With Planning | Direct | Decompose | Direct Pass Rate | Decompose Pass Rate |
|-----------|-------|--------------|--------|-----------|-----------------|---------------------|
| **B** | 227 | 227 (100%) | 225 | 2 | 52.9% | 100.0% |
| **C** | 227 | 227 (100%) | 226 | 1 | 51.8% | 100.0% |

## 7. Turn Analysis

| Condition | Mean Turns (all) | Mean (pass) | Median |
|-----------|-----------------|-------------|--------|
| **A** | 26.0 | 19.8 | 23 |
| **B** | 26.0 | 20.4 | 21 |
| **C** | 26.1 | 19.8 | 21 |

## 8. Workgraph Overhead (B + C)

| Condition | Total Tool Calls | WG Calls | WG Fraction | Trials Using WG |
|-----------|-----------------|----------|-------------|----------------|
| **B** | 7093 | 663 | 9.3% | 200/227 (88%) |
| **C** | 7239 | 627 | 8.7% | 194/227 (85%) |

### WG Tool Breakdown

**Condition B:**
  - `wg_log`: 357
  - `wg_done`: 186
  - `wg_artifact`: 61
  - `wg_add`: 57
  - `wg_show`: 1
  - `wg_list`: 1

**Condition C:**
  - `wg_log`: 357
  - `wg_done`: 178
  - `wg_add`: 46
  - `wg_artifact`: 46

## 9. Qualitative: Where Does Workgraph Help?

### Tasks where B or C improves ≥34pp over A

| Task | Tier | A Rate | B Rate | C Rate | Best Δ |
|------|------|--------|--------|--------|--------|
| fix-ocaml-gc | hard | 0% | 100% | 33% | +100pp |
| password-recovery | hard | 0% | 100% | 0% | +100pp |
| mailman | medium | 33% | 67% | 100% | +67pp |
| sqlite-with-gcov | medium | 33% | 67% | 100% | +67pp |
| tune-mjcf | medium | 33% | 100% | 50% | +67pp |

### Tasks where B or C degrades ≥34pp vs A

| Task | Tier | A Rate | B Rate | C Rate | Worst Δ |
|------|------|--------|--------|--------|---------|
| constraints-scheduling | easy | 100% | 33% | 33% | -67pp |
| sanitize-git-repo | easy | 100% | 33% | 33% | -67pp |
| bn-fit-modify | medium | 67% | 0% | 0% | -67pp |

## 10. Cost Analysis

Model: minimax/minimax-m2.7 via OpenRouter. Pricing not available (cost_usd=0 in logs).
Estimated from token counts:

| Condition | Total Input Tokens | Total Output Tokens | Total Tokens |
|-----------|-------------------:|--------------------:|-------------:|
| **A** | 151,985,862 | 3,067,572 | 155,053,434 |
| **B** | 145,605,506 | 2,851,253 | 148,456,759 |
| **C** | 137,454,172 | 2,815,043 | 140,269,215 |

## 11. Error Analysis

| Condition | Total Errors | Error Rate | Timeout-like | Other |
|-----------|-------------|------------|-------------|-------|
| **A** | 42 | 15.7% | — | — |
| **B** | 40 | 15.0% | — | — |
| **C** | 36 | 13.7% | — | — |

**Tasks with no valid trials in any condition:** caffe-cifar-10, install-windows-3-11

## 12. Per-Task Comparison (All 89 Tasks)

| Task | Tier | A (p/v) | B (p/v) | C (p/v) | A% | B% | C% | B−A | C−A |
|------|------|---------|---------|---------|-----|-----|-----|-----|-----|
| adaptive-rejection-sampler | easy | 1/1 | 0/2 | 0/0 | 100 | 0 | — | -100 | — |
| cobol-modernization | easy | 2/2 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| code-from-image | easy | 2/2 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| compile-compcert | easy | 2/2 | 2/2 | 2/2 | 100 | 100 | 100 | +0 | +0 |
| configure-git-webserver | easy | 3/3 | 2/3 | 3/3 | 100 | 67 | 100 | -33 | +0 |
| constraints-scheduling | easy | 3/3 | 1/3 | 1/3 | 100 | 33 | 33 | -67 | -67 |
| crack-7z-hash | easy | 3/3 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| distribution-search | easy | 3/3 | 2/3 | 2/3 | 100 | 67 | 67 | -33 | -33 |
| extract-elf | easy | 3/3 | 2/3 | 1/3 | 100 | 67 | 33 | -33 | -67 |
| financial-document-processor | easy | 2/2 | 3/3 | 1/3 | 100 | 100 | 33 | +0 | -67 |
| fix-code-vulnerability | easy | 3/3 | 2/2 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| git-leak-recovery | easy | 3/3 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| git-multibranch | easy | 3/3 | 3/3 | 2/3 | 100 | 100 | 67 | +0 | -33 |
| headless-terminal | easy | 3/3 | 3/3 | 1/2 | 100 | 100 | 50 | +0 | -50 |
| hf-model-inference | easy | 3/3 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| kv-store-grpc | easy | 3/3 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| large-scale-text-editing | easy | 1/1 | 2/3 | 2/3 | 100 | 67 | 67 | -33 | -33 |
| llm-inference-batching-scheduler | easy | 3/3 | 2/3 | 3/3 | 100 | 67 | 100 | -33 | +0 |
| mcmc-sampling-stan | easy | 3/3 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| modernize-scientific-stack | easy | 3/3 | 2/3 | 3/3 | 100 | 67 | 100 | -33 | +0 |
| nginx-request-logging | easy | 1/1 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| openssl-selfsigned-cert | easy | 3/3 | 2/3 | 3/3 | 100 | 67 | 100 | -33 | +0 |
| portfolio-optimization | easy | 3/3 | 3/3 | 2/3 | 100 | 100 | 67 | +0 | -33 |
| prove-plus-comm | easy | 3/3 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| pypi-server | easy | 3/3 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| pytorch-model-cli | easy | 3/3 | 3/3 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| pytorch-model-recovery | easy | 2/2 | 2/2 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| qemu-alpine-ssh | easy | 1/1 | 1/1 | 0/0 | 100 | 100 | — | +0 | — |
| query-optimize | easy | 2/2 | 1/3 | 2/3 | 100 | 33 | 67 | -67 | -33 |
| reshard-c4-data | easy | 3/3 | 3/3 | 2/3 | 100 | 100 | 67 | +0 | -33 |
| rstan-to-pystan | easy | 3/3 | 2/2 | 3/3 | 100 | 100 | 100 | +0 | +0 |
| sanitize-git-repo | easy | 3/3 | 1/3 | 1/3 | 100 | 33 | 33 | -67 | -67 |
| sqlite-db-truncate | easy | 3/3 | 3/3 | 2/2 | 100 | 100 | 100 | +0 | +0 |
| bn-fit-modify | medium | 2/3 | 0/3 | 0/3 | 67 | 0 | 0 | -67 | -67 |
| break-filter-js-from-html | medium | 2/3 | 1/3 | 2/3 | 67 | 33 | 67 | -33 | +0 |
| build-cython-ext | medium | 1/3 | 2/3 | 2/3 | 33 | 67 | 67 | +33 | +33 |
| build-pmars | medium | 2/3 | 3/3 | 3/3 | 67 | 100 | 100 | +33 | +33 |
| build-pov-ray | medium | 2/3 | 1/3 | 3/3 | 67 | 33 | 100 | -33 | +33 |
| cancel-async-tasks | medium | 1/3 | 1/3 | 1/3 | 33 | 33 | 33 | +0 | +0 |
| count-dataset-tokens | medium | 2/3 | 3/3 | 2/3 | 67 | 100 | 67 | +33 | +0 |
| custom-memory-heap-crash | medium | 2/3 | 3/3 | 2/3 | 67 | 100 | 67 | +33 | +0 |
| fix-git | medium | 2/3 | 3/3 | 3/3 | 67 | 100 | 100 | +33 | +33 |
| largest-eigenval | medium | 2/3 | 1/3 | 0/3 | 67 | 33 | 0 | -33 | -67 |
| log-summary-date-ranges | medium | 2/3 | 3/3 | 3/3 | 67 | 100 | 100 | +33 | +33 |
| mailman | medium | 1/3 | 2/3 | 3/3 | 33 | 67 | 100 | +33 | +67 |
| merge-diff-arc-agi-task | medium | 2/3 | 3/3 | 2/3 | 67 | 100 | 67 | +33 | +0 |
| multi-source-data-merger | medium | 2/3 | 3/3 | 3/3 | 67 | 100 | 100 | +33 | +33 |
| overfull-hbox | medium | 1/3 | 0/3 | 1/3 | 33 | 0 | 33 | -33 | +0 |
| qemu-startup | medium | 2/3 | 2/3 | 2/2 | 67 | 67 | 100 | +0 | +33 |
| regex-log | medium | 1/3 | 2/3 | 0/3 | 33 | 67 | 0 | +33 | -33 |
| sparql-university | medium | 2/3 | 2/3 | 2/3 | 67 | 67 | 67 | +0 | +0 |
| sqlite-with-gcov | medium | 1/3 | 2/3 | 3/3 | 33 | 67 | 100 | +33 | +67 |
| tune-mjcf | medium | 1/3 | 2/2 | 1/2 | 33 | 100 | 50 | +67 | +17 |
| vulnerable-secret | medium | 2/3 | 2/3 | 3/3 | 67 | 67 | 100 | +0 | +33 |
| winning-avg-corewars | medium | 1/3 | 0/3 | 0/3 | 33 | 0 | 0 | -33 | -33 |
| caffe-cifar-10 | hard | 0/0 | 0/0 | 0/0 | — | — | — | — | — |
| chess-best-move | hard | 0/1 | 0/2 | 0/2 | 0 | 0 | 0 | +0 | +0 |
| circuit-fibsqrt | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| db-wal-recovery | hard | 0/3 | 0/2 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| dna-assembly | hard | 0/3 | 0/2 | 0/2 | 0 | 0 | 0 | +0 | +0 |
| dna-insert | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| extract-moves-from-video | hard | 0/1 | 0/2 | 0/2 | 0 | 0 | 0 | +0 | +0 |
| feal-differential-cryptanalysis | hard | 0/1 | 0/1 | 0/1 | 0 | 0 | 0 | +0 | +0 |
| feal-linear-cryptanalysis | hard | 0/1 | 0/0 | 0/1 | 0 | — | 0 | — | +0 |
| filter-js-from-html | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| fix-ocaml-gc | hard | 0/2 | 2/2 | 1/3 | 0 | 100 | 33 | +100 | +33 |
| gcode-to-text | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| gpt2-codegolf | hard | 0/2 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| install-windows-3-11 | hard | 0/0 | 0/0 | 0/0 | — | — | — | — | — |
| make-doom-for-mips | hard | 0/3 | 0/3 | 0/2 | 0 | 0 | 0 | +0 | +0 |
| make-mips-interpreter | hard | 0/3 | 0/1 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| model-extraction-relu-logits | hard | 0/1 | 0/0 | 0/1 | 0 | — | 0 | — | +0 |
| mteb-leaderboard | hard | 0/2 | 0/2 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| mteb-retrieve | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| password-recovery | hard | 0/3 | 1/1 | 0/2 | 0 | 100 | 0 | +100 | +0 |
| path-tracing | hard | 0/3 | 0/3 | 0/2 | 0 | 0 | 0 | +0 | +0 |
| path-tracing-reverse | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| polyglot-c-py | hard | 0/2 | 0/1 | 0/1 | 0 | 0 | 0 | +0 | +0 |
| polyglot-rust-c | hard | 0/2 | 0/2 | 0/0 | 0 | 0 | — | +0 | — |
| protein-assembly | hard | 0/2 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| raman-fitting | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| regex-chess | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| sam-cell-seg | hard | 0/3 | 0/2 | 0/2 | 0 | 0 | 0 | +0 | +0 |
| schemelike-metacircular-eval | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| torch-pipeline-parallelism | hard | 0/2 | 0/3 | 1/3 | 0 | 0 | 33 | +0 | +33 |
| torch-tensor-parallelism | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| train-fasttext | hard | 0/0 | 0/1 | 0/1 | — | 0 | 0 | — | — |
| video-processing | hard | 0/3 | 0/3 | 0/3 | 0 | 0 | 0 | +0 | +0 |
| write-compressor | hard | 0/1 | 0/1 | 0/0 | 0 | 0 | — | +0 | — |

## 13. Consistency Analysis

- **Always pass (100% in all conditions):** 16 tasks
  cobol-modernization, code-from-image, compile-compcert, crack-7z-hash, fix-code-vulnerability, git-leak-recovery, hf-model-inference, kv-store-grpc, mcmc-sampling-stan, nginx-request-logging, prove-plus-comm, pypi-server, pytorch-model-cli, pytorch-model-recovery, rstan-to-pystan, sqlite-db-truncate
- **Never pass (0% in all conditions):** 24 tasks
  chess-best-move, circuit-fibsqrt, db-wal-recovery, dna-assembly, dna-insert, extract-moves-from-video, feal-differential-cryptanalysis, filter-js-from-html, gcode-to-text, gpt2-codegolf, make-doom-for-mips, make-mips-interpreter, mteb-leaderboard, mteb-retrieve, path-tracing, path-tracing-reverse, polyglot-c-py, protein-assembly, raman-fitting, regex-chess, sam-cell-seg, schemelike-metacircular-eval, torch-tensor-parallelism, video-processing

- **Variable (differs across conditions or trials):** 49 tasks
