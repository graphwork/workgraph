# TB Retest: Smart Fanout G vs Original G vs A

**Date:** 2026-04-14 06:51 UTC
**Model:** local:qwen3-coder-30b
**Endpoint:** http://lambda01:30000/v1
**Context window:** 32768 tokens
**Task count:** 10
**Tasks:** cobol-modernization, constraints-scheduling, multi-source-data-merger, algorithm, ml, fix-code-vulnerability, configure-git-webserver, debugging, data-processing, file-ops

## Summary Comparison

| Metric | Condition A | Condition G-orig | Condition G-smart |
|--------|------------|------------------|-------------------|
| Pass rate | 70% (7/10) | 70% (7/10) | 70% (7/10) |
| Median time (s) | 51.98 | 1203.75 | 163.02 |
| Decomposition rate | 0% | 100% | 10% |
| Avg subtasks/trial | 1.0 | 21.1 | 2.7 |
| Max agents | 1 | 4 | 4 |

## Per-Task Head-to-Head

| Task | Diff | A | G-orig | G-smart | A time | G-orig time | G-smart time | G-smart subtasks | G-smart decision |
|------|------|---|--------|---------|--------|-------------|--------------|------------------|------------------|
| cobol-modernization | hard | FAIL | FAIL | FAIL | 0s | 1204s | 202s | 1 |  |
| constraints-scheduling | hard | FAIL | PASS | FAIL | 0s | 1204s | 279s | 1 |  |
| multi-source-data-merger | hard | FAIL | PASS | PASS | 0s | 1204s | 179s | 1 |  |
| algorithm | hard | PASS | PASS | PASS | 0s | 176s | 163s | 1 | direct |
| ml | hard | PASS | PASS | PASS | 0s | 1205s | 43s | 1 | direct |
| fix-code-vulnerability | hard | PASS | PASS | PASS | 0s | 1204s | 2403s | 18 |  |
| configure-git-webserver | hard | PASS | FAIL | PASS | 0s | 1213s | 51s | 1 | direct |
| debugging | medium | PASS | PASS | PASS | 22s | 705s | 31s | 1 | direct |
| data-processing | medium | PASS | PASS | PASS | 52s | 903s | 71s | 1 |  |
| file-ops | easy | PASS | FAIL | FAIL | 82s | 602s | 16s | 1 | direct |

## Failure Mode Breakdown

**A:** {'context_overflow': 3, 'success': 7}

**G-original:** {'context_overflow': 1, 'success': 7, 'rate_limit': 2}

**G-smart:** {'status_done': 2, 'wrong_answer': 1, 'success': 7}

## G-smart Fanout Decisions (detailed)

**cobol-modernization** (reward=0.0):
  - No FANOUT_DECISION logged

**constraints-scheduling** (reward=0.0):
  - No FANOUT_DECISION logged

**multi-source-data-merger** (reward=1.0):
  - No FANOUT_DECISION logged

**algorithm** (reward=1.0):
  - Initial: FANOUT_DECISION: direct — The task is a single logical unit of work (implementing a key-value store with transaction support) and touches only one file (/tmp/kvstore.py). It's under 300 words.

**ml** (reward=1.0):
  - Initial: FANOUT_DECISION: direct — Task is a single logical unit of work with clear requirements, touching only one file (/tmp/kmeans.py)

**fix-code-vulnerability** (reward=1.0):
  - No FANOUT_DECISION logged

**configure-git-webserver** (reward=1.0):
  - Initial: FANOUT_DECISION: direct — The task is a single logical unit of work involving setting up a git server with post-receive hooks and HTTP serving, touching a few files but not requiring decomposition.

**debugging** (reward=1.0):
  - Initial: FANOUT_DECISION: direct — The task is straightforward with only one file to modify and 3 specific bugs to fix

**data-processing** (reward=1.0):
  - No FANOUT_DECISION logged

**file-ops** (reward=0.0):
  - Initial: FANOUT_DECISION: direct — The instruction is under 300 words and involves creating a specific file structure with 6 files total, which is manageable in a single implementation approach.

## Analysis & Verdict

### Pass Rate
- **All three conditions: 70% (7/10)** — identical pass rate
- G-smart vs A delta: +0%
- G-original vs A delta: +0%
- G-smart vs G-original delta: +0%

### Timing (the key differentiator)
- **Overhead reduction (G-smart vs G-original):** 86% (median 1204s → 163s)
- **G-smart overhead vs A:** +111s median (163s vs 52s)
- **G-original overhead vs A:** +1152s median (1204s vs 52s) — 23x slower

### Where Each Condition Wins

**Tasks only G-original solved (A and G-smart failed):**
- `constraints-scheduling`: Decomposition rescued a context-overflow task (A failed, G-smart failed going direct)

**Tasks only G-smart solved (G-original failed, A passed):**
- `configure-git-webserver`: Direct impl in 51s; G-original spawned 21+ subtasks and timed out

**Tasks all three solved (speed comparison):**
| Task | A | G-orig | G-smart | G-smart speedup vs G-orig |
|------|---|--------|---------|--------------------------|
| algorithm | — | 176s | 163s | 1.1x |
| ml | — | 1205s | 43s | **28x** |
| debugging | 22s | 705s | 31s | **23x** |
| data-processing | 52s | 903s | 71s | **13x** |
| multi-source-data-merger | — | 1204s | 179s | **6.7x** |
| fix-code-vulnerability | — | 1204s | 2403s | 0.5x (slower) |

### Smart Fanout Decision Quality
- **Decomposition rate:** G-smart decomposed 1/10 tasks (10%) vs G-original's 10/10 (100%)
- **FANOUT_DECISION logged:** 5/10 tasks (others acted without explicit logging)
- **All logged decisions were "direct"** — the model consistently preferred direct implementation
- **One task decomposed late:** fix-code-vulnerability (18 subtasks, 2403s) — the agent likely hit context pressure and switched to decompose mid-task, but no FANOUT_SWITCH was logged

### Failure Mode Shift
| Mode | A | G-original | G-smart |
|------|---|-----------|---------|
| success | 7 | 7 | 7 |
| context_overflow | 3 | 1 | 0 |
| rate_limit | 0 | 2 | 0 |
| wrong_answer | 0 | 0 | 1 |
| status_done (graph done, verify fail) | 0 | 0 | 2 |

G-smart eliminated both context_overflow and rate_limit failures but introduced wrong_answer failures — the agent completes quickly but sometimes produces incorrect solutions.

### Key Insight: The Decomposition Dilemma
G-smart's try-first approach **dramatically reduces overhead** (86% faster median) while maintaining the same pass rate. However, it trades away G-original's ability to rescue context-overflow tasks through decomposition:
- G-original rescued `constraints-scheduling` and `multi-source-data-merger` (both A-failed context-overflow tasks)
- G-smart only rescued `multi-source-data-merger` — it failed `constraints-scheduling` going direct

The ideal strategy would be a **hybrid**: try direct first, but with reliable context-pressure detection to trigger decomposition when the single-agent approach is clearly failing. The current FANOUT_SWITCH mechanism exists in the prompt but wasn't triggered in this test set.

### Verdict
**Smart fanout fixes the overhead problem.** The 86% median time reduction confirms the hypothesis: unconditional decomposition is wasteful for most tasks. However, the identical pass rate means smart fanout is a **lateral move on correctness** — it's much faster but doesn't unlock new capabilities. The remaining opportunity is improving the decomposition trigger so G-smart can also rescue the context-overflow tasks that G-original handles.