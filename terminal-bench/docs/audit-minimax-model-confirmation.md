# Audit: MINIMAX M2.7 Model Confirmation Across All Pilot Traces

**Date:** 2026-04-07
**Task:** audit-confirm-minimax
**Conclusion:** **YES — MINIMAX M2.7 is the sole execution model for all pilot trial agents.**

---

## 1. Executive Summary

MINIMAX M2.7 (`openrouter:minimax/minimax-m2.7`, resolved to `minimax/minimax-m2.7` via OpenRouter) is confirmed as the **sole execution model** for task agent execution across both pilot conditions (A and F). No Claude Opus, Sonnet, or any other model was used for trial agent execution.

The workgraph system itself (coordinator, evaluations, assignments, FLIP scoring) uses different models (Opus, Sonnet, Haiku) — this is by design and does not affect the experiment's execution model.

---

## 2. Config Verification

### 2.1 Runner scripts (trial-level config)

Each runner script hardcodes MINIMAX M2.7:

| Script | Default model | Line |
|--------|--------------|------|
| `run_condition_a.py` | `openrouter:minimax/minimax-m2.7` | L34 |
| `run_pilot_f_89.py` | `openrouter:minimax/minimax-m2.7` | L47 |
| `run_full_a_prime_vs_f.py` | `openrouter:minimax/minimax-m2.7` | L37 |
| `run_scale_experiment.py` | `openrouter:minimax/minimax-m2.7` | L49 |

Each script writes an isolated `config.toml` per trial with the model locked:
```toml
[agent]
model = "openrouter:minimax/minimax-m2.7"

[coordinator]
model = "openrouter:minimax/minimax-m2.7"
```

And passes `--model openrouter:minimax/minimax-m2.7` explicitly to each `wg` spawn command.

### 2.2 Global workgraph config (NOT used for trials)

The project-level `.workgraph/config.toml` sets:
- `[agent] model = "claude:opus"`
- `[coordinator] model = "claude:opus"`
- `[models.task_agent] model = "claude:opus"`

**This config is irrelevant to the pilots** — each trial creates its own isolated `.workgraph/` directory with its own `config.toml`. The global config governs only the orchestrating coordinator that manages experiment tasks.

---

## 3. Executor Code Path Analysis

**File:** `src/commands/spawn/execution.rs`

The model resolution cascade (lines 211-261):
1. Per-task `model` field (highest priority)
2. Task provider
3. Agent preferred model (from agency assignment)
4. Agent preferred provider
5. Executor config model (`[agent].model` in trial's config.toml)
6. `[models.task_agent]` resolved model
7. Provider from role config
8. CLI `--model` flag
9. Coordinator provider

For pilot trials, the model is set at **both** level 5 (executor config) and passed via `--model` flag (level 8), making fallback impossible.

The effective model is then passed to the CLI command builder (lines 769-874) as `--model <effective_model>` for all executor types (`claude`, `amplifier`, `native`). There is **no hardcoded fallback** to Opus or any other model in the spawn path.

### 3.1 Model registry resolution

When the model string `openrouter:minimax/minimax-m2.7` is provided:
- `resolve_model_via_registry()` looks up registry entries
- `minimax-m2.7` is registered with `model = "minimax/minimax-m2.7"`, `provider = "openrouter"`, `endpoint = "openrouter"`
- The resolved model is `minimax/minimax-m2.7` via the OpenRouter endpoint

### 3.2 Built-in tier aliases

Built-in aliases (`haiku`, `sonnet`, `opus`) are preserved for Claude CLI compatibility. Since the trial model is `openrouter:minimax/minimax-m2.7` (not a built-in alias), it goes through registry resolution and is **not** remapped to a Claude model.

---

## 4. Trial-Level Model Verification

### 4.1 Pilot A-89 (Condition A — bare agent)

| Metric | Value |
|--------|-------|
| Total trials | 89 |
| Configured model | `openrouter:minimax/minimax-m2.7` |
| `all_agents_used_m2_7` | **true** |
| `no_claude_fallback` | **true** |
| Trials with `model_verified=False` | **0** |

**Sample of 25 verified trials:**

| # | Trial ID | model_verified | agent_model | provider |
|---|----------|---------------|-------------|----------|
| 1 | adaptive-rejection-sampler | true | minimax/minimax-m2.7 | openrouter |
| 2 | bn-fit-modify | true | minimax/minimax-m2.7 | openrouter |
| 3 | break-filter-js-from-html | true | minimax/minimax-m2.7 | openrouter |
| 4 | build-cython-ext | true | minimax/minimax-m2.7 | openrouter |
| 5 | build-pmars | true | minimax/minimax-m2.7 | openrouter |
| 6 | build-pov-ray | true | minimax/minimax-m2.7 | openrouter |
| 7 | caffe-cifar-10 | true | minimax/minimax-m2.7 | openrouter |
| 8 | cancel-async-tasks | true | minimax/minimax-m2.7 | openrouter |
| 9 | chess-best-move | true | minimax/minimax-m2.7 | openrouter |
| 10 | circuit-fibsqrt | true | minimax/minimax-m2.7 | openrouter |
| 11 | cobol-modernization | true | minimax/minimax-m2.7 | openrouter |
| 12 | code-from-image | true | minimax/minimax-m2.7 | openrouter |
| 13 | compile-compcert | true | minimax/minimax-m2.7 | openrouter |
| 14 | configure-git-webserver | true | minimax/minimax-m2.7 | openrouter |
| 15 | constraints-scheduling | true | minimax/minimax-m2.7 | openrouter |
| 16 | count-dataset-tokens | true | minimax/minimax-m2.7 | openrouter |
| 17 | crack-7z-hash | true | minimax/minimax-m2.7 | openrouter |
| 18 | custom-memory-heap-crash | true | minimax/minimax-m2.7 | openrouter |
| 19 | db-wal-recovery | true | minimax/minimax-m2.7 | openrouter |
| 20 | distribution-search | true | minimax/minimax-m2.7 | openrouter |
| 21 | dna-assembly | true | minimax/minimax-m2.7 | openrouter |
| 22 | dna-insert | true | minimax/minimax-m2.7 | openrouter |
| 23 | extract-elf | true | minimax/minimax-m2.7 | openrouter |
| 24 | extract-moves-from-video | true | minimax/minimax-m2.7 | openrouter |
| 25 | feal-differential-cryptanalysis | true | minimax/minimax-m2.7 | openrouter |

All 89/89 trials verified. Zero non-MINIMAX model usage.

### 4.2 Pilot F-89 (Condition F — full wg-native)

| Metric | Value |
|--------|-------|
| Total trials | 90 (18 tasks × 5 replicas) |
| Configured model | `openrouter:minimax/minimax-m2.7` |
| `model_verified_count` | **90** |
| `claude_fallback_detected` | **False** |
| Trials with `model_verified != True` | **0** |

**Sample of 25 verified trials:**

| # | Trial ID | model_verified | model_used |
|---|----------|---------------|------------|
| 1 | f-file-ops-r0 | true | minimax/minimax-m2.7 |
| 2 | f-file-ops-r1 | true | minimax/minimax-m2.7 |
| 3 | f-file-ops-r2 | true | minimax/minimax-m2.7 |
| 4 | f-file-ops-r3 | true | minimax/minimax-m2.7 |
| 5 | f-file-ops-r4 | true | minimax/minimax-m2.7 |
| 6 | f-text-processing-r0 | true | minimax/minimax-m2.7 |
| 7 | f-text-processing-r1 | true | minimax/minimax-m2.7 |
| 8 | f-text-processing-r2 | true | minimax/minimax-m2.7 |
| 9 | f-text-processing-r3 | true | minimax/minimax-m2.7 |
| 10 | f-text-processing-r4 | true | minimax/minimax-m2.7 |
| 11 | f-debugging-r0 | true | minimax/minimax-m2.7 |
| 12 | f-debugging-r1 | true | minimax/minimax-m2.7 |
| 13 | f-debugging-r2 | true | minimax/minimax-m2.7 |
| 14 | f-debugging-r3 | true | minimax/minimax-m2.7 |
| 15 | f-debugging-r4 | true | minimax/minimax-m2.7 |
| 16 | f-shell-scripting-r0 | true | minimax/minimax-m2.7 |
| 17 | f-shell-scripting-r1 | true | minimax/minimax-m2.7 |
| 18 | f-shell-scripting-r2 | true | minimax/minimax-m2.7 |
| 19 | f-shell-scripting-r3 | true | minimax/minimax-m2.7 |
| 20 | f-shell-scripting-r4 | true | minimax/minimax-m2.7 |
| 21 | f-data-processing-r0 | true | minimax/minimax-m2.7 |
| 22 | f-data-processing-r1 | true | minimax/minimax-m2.7 |
| 23 | f-data-processing-r2 | true | minimax/minimax-m2.7 |
| 24 | f-data-processing-r3 | true | minimax/minimax-m2.7 |
| 25 | f-data-processing-r4 | true | minimax/minimax-m2.7 |

All 90/90 trials verified. Zero non-MINIMAX model usage.

### 4.3 Full A' vs F run

| Metric | Value |
|--------|-------|
| Configured model | `openrouter:minimax/minimax-m2.7` |
| Both conditions | A' (clean context) and F (graph context) |

Uses the same model-locked isolation pattern as the pilots.

---

## 5. Evaluation Model vs Execution Model (Expected Distinction)

The workgraph system uses different models for different roles. This is **by design** and does not contaminate the experiment:

| Role | Model | Purpose | Affects trials? |
|------|-------|---------|----------------|
| **Task agent (trial execution)** | `minimax/minimax-m2.7` | Runs the actual benchmark task | **YES — this is what we're auditing** |
| Coordinator | `claude:opus` | Dispatches tasks, manages graph | No — orchestration only |
| Evaluator (.evaluate-*) | `claude:sonnet` | Scores task quality post-hoc | No — scoring only |
| FLIP inference (.flip-*) | `claude:sonnet` | Fidelity probing | No — scoring only |
| FLIP comparison | `claude:haiku` | Pairwise comparison | No — scoring only |
| Assigner | `claude:haiku` | Agent-task matching | No — orchestration only |
| Verification | `claude:opus` | Verify task completeness | No — post-hoc check only |

The registry shows 3932 total agent entries. Of these:
- **77 use `openrouter:minimax/minimax-m2.7`** — these are the actual trial execution agents
- **1623 use `claude-sonnet-4-latest`** — evaluation/FLIP agents
- **884 use `opus`** — coordinator agents
- Others — various system tasks

The non-minimax agents are all system/evaluation tasks, never trial execution agents.

---

## 6. Potential Override Mechanisms (None Active)

Checked for mechanisms that could silently substitute a different model:

| Mechanism | Status | Evidence |
|-----------|--------|----------|
| Per-task `model` field override | Not used in pilot trials | Trial configs write explicit minimax model |
| Agency `preferred_model` | Not applicable | Pilot trials don't use agency assignments |
| Executor fallback | No fallback exists | Code inspection: no hardcoded fallback model |
| Claude CLI default model | Overridden by `--model` flag | Flag is always passed (lines 769-874 in execution.rs) |
| Environment variable leak | Prevented | Runner scripts strip `WG_*` and `CLAUDECODE` env vars |
| `config.toml` global leak | Prevented | Each trial creates isolated `.workgraph/config.toml` |

---

## 7. Conclusion

**YES — MINIMAX M2.7 (`minimax/minimax-m2.7` via OpenRouter) is confirmed as the sole execution model for all pilot trial agents across both conditions.**

Evidence strength:
- **179 trials examined** (89 Condition A + 90 Condition F) — exceeds the 20-trial minimum
- **100% model verification** in both pilots (0 failures)
- **No Claude fallback detected** in either pilot
- **Three layers of confirmation**: config-level (runner scripts), code-level (executor spawn path), and trace-level (per-trial agent_info verification)
- **Isolation architecture** prevents global config from leaking into trial execution

The concern about Opus being invoked for spawned agents is unfounded: the runner scripts create fully isolated workgraph environments with explicit model configuration, and pass `--model` flags to every spawn command. The global `config.toml` (which does set `claude:opus`) is never read by trial agents.
