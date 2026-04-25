# Executor Trial Synthesis: Recommendations

**Date:** 2026-04-05
**Sources:**
- Head-to-head trial: `terminal-bench/trials/executor-h2h-results.md` (84 trials, 4 conditions, 7 tasks, minimax-m2.7)
- Multi-model live trial: `.workgraph/research/multi-model-live-trial-summary.md` (5 models, real workgraph tasks)

---

## 1. Executive Summary

**Is the enhanced executor measurably stronger?** Yes — directionally, but not yet statistically proven.

The H2H trial compared four executor strategies across 84 trials on 7 hard tasks:

| Condition | Pass Rate | False-PASS | Efficiency (passes/MTok) |
|-----------|-----------|------------|--------------------------|
| A (bare agent) | 42.9% | 0% | **1.27** |
| C (skill injection + planning) | 52.4% | 0% | 1.01 |
| D (self-verify loop) | **66.7%** | 33.3% | 0.99 |
| E (org decomposition) | **66.7%** | 28.6% | 0.84 |

**The core tension:** Self-verification (D) and decomposition (E) boost raw pass rate by ~24pp over bare agents, but at the cost of 28–33% false-PASS rates — the agent declares success when it hasn't actually succeeded. The enhanced executor's skill injection (C) provides a modest +9.5pp with zero false-PASS, but doesn't match D/E's raw throughput.

**The missing piece:** The most important enhanced executor feature — separate-agent verification (`verify_mode = "separate"`) — was not testable in the TB adapter framework because it's a coordinator-level feature. This feature directly targets D/E's false-PASS problem. If it works, combining D's verification loops with separate-agent verification would yield the best of both worlds: high pass rate *and* honest self-assessment.

**Statistical caveat:** No pairwise comparison reached p < 0.05 (Fisher's exact). The trial was powered to detect ~25pp differences; the largest observed gap was 23.8pp (A vs D, p = 0.215). Per-task patterns are consistent and reproducible across pilot and H2H runs, lending confidence to directional findings.

---

## 2. Model Recommendations

The multi-model live trial tested 5 models on real workgraph tasks through the native executor. Combined with H2H data (all on minimax-m2.7), here are production recommendations:

### Tier 1: Production-Ready

| Model | Strengths | Weaknesses | Best For |
|-------|-----------|------------|----------|
| **claude-sonnet-4-latest** | Excellent tool use, cache-efficient (35k input), high quality output | Higher cost per token, requires Claude executor path | Quality-critical tasks, complex reasoning |
| **gemini-2.5-flash** | Fastest (15s completion), correct tool use, very cheap ($0.30/MTok input) | Slightly lower output quality than Claude/MiniMax | High-volume tasks, cost-sensitive workloads, simple-to-medium tasks |

### Tier 2: Viable with Caveats

| Model | Strengths | Weaknesses | Best For |
|-------|-----------|------------|----------|
| **minimax-m2.7** | Good reasoning (thinking tokens), structured output quality, well-tested (84 H2H trials) | First-turn tool format issues (XML in tool names), 3x tokens of Gemini | Research tasks, config audits, reasoning-heavy work |
| **deepseek-v3.2** | Thorough analysis, cheap input, correct tool use | Slow (5 min for simple tasks), verbose (32 turns, 518k input) | Exhaustive research/analysis where completeness > speed |

### Tier 3: Not Recommended for Agentic Work

| Model | Why Not |
|-------|---------|
| **qwen3-coder** | Broken tool use — malformed tool names ("", "w", "wg_msg"), high token waste from retries (422k input), string formatting issues in output. Not reliable enough for autonomous agent tasks. |

### Model Routing Recommendation

The multi-model trial validated that automatic model routing works end-to-end:
- Non-Anthropic models are auto-detected and routed to the `native` executor
- Context injection (task prompt, test files, agent workflow) works across all models
- Verify gates work for all models

**Default routing policy:**
- `claude-sonnet-4-latest` for complex tasks (reasoning, multi-step, quality-critical)
- `gemini-2.5-flash` for simple tasks (documentation, config, straightforward code changes)
- `minimax-m2.7` as a fallback/alternative for reasoning tasks where Claude is unavailable
- Avoid `qwen3-coder` entirely until tool use reliability improves

---

## 3. Feature Impact Analysis

### Ranked by Impact (Highest to Lowest)

#### 1. Self-Verification Loops — HIGH IMPACT, HIGH RISK
**Evidence:** D's +23.8pp over bare agents (42.9% → 66.7%). Most impactful single feature tested.

- **Where it helps:** Tasks with iterative debugging cycles — `regex-log` (D 67% vs A 33%, avg 10 verification iterations), `cancel-async-tasks` (D 67% vs A 33%), `nginx-request-logging` (D 100% vs A 33%)
- **Where it hurts:** Creates false confidence — 33.3% of all D trials declared success incorrectly. Worst on tasks with non-obvious verification criteria (`overfull-hbox`: 0 verification iterations, agent couldn't figure out what to check)
- **Verdict:** Ship, but only with separate-agent verification as a backstop

#### 2. Separate-Agent Verification — UNTESTED, HIGHEST PRIORITY
**Evidence:** Indirect only. D's 33% false-PASS rate and E's 29% false-PASS rate demonstrate the problem this feature solves. The H2H trial confirmed the problem is real and severe.

- **Design:** Fresh agent context verifies work, preventing the "rubber-stamp your own output" failure mode
- **Priority:** This is the single most important feature to validate next. It directly addresses the clearest signal in the data.
- **Risk:** May add latency and cost (spawning a second agent). Need to measure whether the false-PASS reduction justifies the overhead.

#### 3. Task Decomposition (Adaptive) — MEDIUM IMPACT, TASK-DEPENDENT
**Evidence:** E's decomposition helped on multi-step tasks (+67pp on `build-cython-ext`, +33pp on `merge-diff-arc-agi-task`) but catastrophically hurt on atomic tasks (-33pp on `cancel-async-tasks` and `regex-log`, both dropping to 0%).

- **The classifier matters more than the decomposition:** The value isn't in decomposing per se — it's in correctly identifying *when not to decompose*. Atomic tasks that get fragmented lose holistic context and fail.
- **Adaptive classification (`classify_task_complexity()`)** directly addresses this. It was designed for the coordinator but wasn't testable in the TB adapter.
- **Verdict:** Ship the classifier. Decompose multi-step tasks, protect atomic ones.

#### 4. Skill Injection + Planning Phase — LOW IMPACT
**Evidence:** C's +9.5pp over bare agents (42.9% → 52.4%), not statistically significant (p = 0.758).

- **Where it helps:** Well-structured tasks with clear steps (`nginx-request-logging`: C 100% vs A 33%)
- **Where it hurts:** Holistic reasoning tasks (`regex-log`: C 0% vs A 33% — planning overhead is counterproductive)
- **Verdict:** Keep but don't rely on. The planning phase adds modest value for structured tasks and doesn't substitute for verification.

#### 5. Pre-Task Test Discovery — NOT DIRECTLY TESTED
**Evidence:** Multi-model trial confirmed test discovery + auto-verify gates work end-to-end across all 5 models. However, the H2H trial couldn't isolate test discovery's impact from other features.

- **Mechanism:** Scanning for test files and injecting them into the agent prompt, plus auto-generating `--verify` gates
- **Expected value:** High for tasks where the agent wouldn't otherwise know which tests to run (e.g., `build-cython-ext` where the critical test is `test_ccomplexity`)
- **Verdict:** Ship as-is. Low cost, validated plumbing, fills an information gap that agents otherwise miss.

#### 6. Context Injection for Non-Claude Models — VALIDATED
**Evidence:** Multi-model trial confirmed that all 4 OpenRouter models received and used wg context correctly. All used `wg_log`, `wg_done`, and other wg tools (with varying success).

- **Verdict:** Ship as-is. Essential infrastructure that's already working.

---

## 4. Next Steps

### Ship As-Is (No Further Validation Needed)

1. **Model routing + auto-detection** — Working end-to-end, validated across 5 models
2. **Pre-task test discovery + auto-verify gates** — Plumbing validated, low risk
3. **Context injection for non-Claude models** — Working, essential
4. **Default model routing policy** — Claude for complex, Gemini for simple, MiniMax as fallback
5. **Qwen3-coder exclusion** — Add to model blocklist or warn on selection

### Validate Next (Highest Priority)

1. **Separate-agent verification (`verify_mode = "separate"`)** — Run a coordinator-level experiment, not a TB adapter trial. Design:
   - Same 7 tasks from H2H trial
   - Two conditions: `verify_mode = "inline"` vs `verify_mode = "separate"`
   - Same model (claude-sonnet-4-latest or opus)
   - Primary metric: false-PASS rate reduction
   - Secondary: pass rate, cost overhead, latency
   - Minimum 10 trials/task/condition (70 per condition) for adequate power

2. **Adaptive decomposition classifier** — Test `classify_task_complexity()` accuracy on the 7 H2H tasks:
   - Does it correctly classify `cancel-async-tasks` and `regex-log` as Atomic?
   - Does it correctly classify `build-cython-ext` and `merge-diff-arc-agi-task` as Pipeline/FanOut?
   - Can run as a unit test (no full trials needed)

### Improve Before Shipping

1. **Verification iteration guidance** — D had 0 verification iterations on `overfull-hbox` because the agent couldn't determine what to verify for LaTeX. The executor should provide task-type-specific verification hints (e.g., "for LaTeX tasks, verify compilation produces no warnings").

2. **Token budget controls** — E's `regex-log` trial consumed 3M tokens in a single trial from thrashing. Add per-trial token budget limits with graceful degradation (stop decomposing, simplify approach) rather than hard cutoffs.

3. **First-turn tool format recovery** — MiniMax-m2.7 and Qwen3-coder both had first-turn tool call format issues. The native executor should handle malformed first-turn tool calls more gracefully (retry with format hint rather than error).

### Cut / Deprioritize

1. **Planning phase as standalone feature** — C's planning phase showed minimal impact (+9.5pp, not significant) and actively hurt on holistic tasks. Don't invest more in planning-phase prompting. The value comes from verification and decomposition, not planning.

2. **Increasing H2H trial count** — Diminishing returns on the current TB adapter framework, which can't test coordinator-level features. Invest the trial budget in coordinator-level experiments instead.

3. **Organization generation (E-style)** — E's full org-gen approach is too expensive (1,197K tokens/pass, lowest efficiency) and has the same false-PASS problem as D. The adaptive classifier + separate verification captures E's benefits (decompose when appropriate) without E's costs.

---

## 5. Integrated Findings: What the Two Trials Tell Us Together

The H2H trial answered: **Which executor strategies work?**
The multi-model trial answered: **Which models can execute them?**

Together, they paint a clear picture:

1. **The executor architecture is sound.** Model routing, context injection, verify gates, and test discovery all work end-to-end. The infrastructure is production-ready.

2. **The biggest quality lever is verification, not planning or decomposition.** Self-verification loops (D) provided the largest pass-rate improvement. But they need an external check (separate-agent verification) to prevent the 33% false-PASS problem.

3. **The second biggest lever is knowing when NOT to decompose.** Decomposition is powerful for multi-step tasks and catastrophic for atomic ones. The adaptive classifier is the gate that makes decomposition safe to enable by default.

4. **Model choice matters less than executor strategy.** Within the H2H trial (same model, different strategies), pass rates ranged from 43% to 67% — a 24pp spread. Within the multi-model trial (same strategy, different models), all models completed their tasks successfully. Strategy dominates model selection for pass rate; model selection matters for cost and speed.

5. **The recommended production configuration is:**
   - Self-verification loops ON
   - Separate-agent verification ON (pending validation)
   - Adaptive decomposition classifier ON
   - Test discovery + auto-verify ON
   - Planning phase: included but not relied upon
   - Default model: claude-sonnet-4-latest (complex) / gemini-2.5-flash (simple)
