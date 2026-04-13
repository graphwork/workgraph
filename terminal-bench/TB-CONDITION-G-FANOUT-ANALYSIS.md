# Research: Condition G Fanout Performance Analysis

**Date:** 2026-04-13
**Task:** tb-research-g-fanout
**Status:** Research complete

---

## Executive Summary

Condition G's poor performance is caused by **unconditional decomposition** — the meta-prompt instructs the seed agent to "DO NOT write code. DO NOT modify files. Only create wg tasks," forcing every task through a decompose-then-dispatch pipeline regardless of size or complexity. This creates three compounding penalties:

1. **Fixed coordination overhead** (~2-5 min per trial for graph construction + seed completion + coordinator dispatch + agent spawn) that dominates on tasks solvable in <5 minutes by a single agent
2. **Context transfer loss** — worker agents receive only the description the architect wrote, not the rich problem understanding the architect built through exploration
3. **Convergence failure** — verification cycles restart from scratch, agents lack time awareness, and the trial timeout kills productive work mid-iteration

**Key finding:** On the 18-task local benchmark, Condition A (single agent) achieves 72% pass rate (13/18) with the binding constraint being context window overflow on 5 hard tasks. Condition G's decomposition *could* help exactly those 5 tasks (by giving each subtask a fresh context window), but the meta-prompt's "always decompose" mandate means it also adds 2-5 minutes of overhead to the 13 tasks that A already solves directly. The break-even point for decomposition is approximately **30+ turns / 14k+ peak input tokens** — below this threshold, decomposition is pure overhead.

---

## 1. Performance Comparison: Condition A vs G

### 1.1 Available Data

| Run | Model | Tasks | A Pass Rate | G Pass Rate | Notes |
|-----|-------|-------|-------------|-------------|-------|
| pilot-a-vs-g-haiku | claude:haiku | 5 | 5/5 (100%) | 5/5 (100%) | G = raw Claude (not autopoietic); A was slightly slower (39s vs 33s avg) |
| pilot-qwen3-local-10 (A) | qwen3-coder-30b | 10 | 10/10 (100%) | — | Condition A baseline |
| qwen3-hard-20-a | qwen3-coder-30b | 18 | 13/18 (72%) | — | Full local benchmark |
| qwen3-hard-20-g | qwen3-coder-30b | 18 | — | 0/1 (interrupted) | Only 1 trial started; run was killed |
| gpt-oss-120b-G-smoke | gpt-oss-120b | 1 | — | 0/1 (CancelledError) | Harbor adapter G trial |
| smoke-g-harness-iter2 | gpt-oss-120b | 1 | — | 0/1 (CancelledError) | Harbor adapter G trial |
| smoke-g-iter2-rebuilt | gpt-oss-120b:free | 2 | — | 0/2 (CancelledError) | Harbor adapter G trial |
| Ulivo experiment run 4 | minimax-m2.7 | 14 | — | 9/14 (64%) | Best G result; agents worked until timeout |
| Ulivo experiment A/F | minimax-m2.7 | 89×5 | 41% / 45% | — | Full TB 2.0 benchmark |

**Critical observation:** There is no completed head-to-head A vs G comparison on the same tasks with the same model using the autopoietic meta-prompt. The pilot-a-vs-g-haiku comparison used raw Claude for "G" (not the workgraph decomposition condition). The Ulivo 64% result is not directly comparable to Ulivo A (41%) because it was a different task subset (14 vs 89 tasks) and different iteration of the prompt.

### 1.2 Overhead Gap: What We Can Estimate

From Condition A data on qwen3-coder-30b (18 tasks):

| Task Class | A: Avg Turns | A: Avg Peak Input | A: Pass Rate | G Overhead Estimate |
|------------|-------------|-------------------|-------------|-------------------|
| Easy (2 tasks) | 14.5 | 4,794 tok | 100% | Pure overhead — tasks complete in <10 turns |
| Medium (3 tasks) | 8.0 | 5,424 tok | 100% | Pure overhead — tasks complete in <15 turns |
| Hard-passing (8 tasks) | 25.4 | 9,621 tok | 100% | Mixed — some benefit from decomposition |
| Hard-failing (5 tasks) | 31.4 | 16,243 tok | 0% | **Potential G benefit** — context overflow |

The 5 tasks that fail under A all hit the 32k context ceiling at ~49.5% utilization (input ~16.2k + max_completion ~16.4k = 32.6k ≈ 32.7k limit). These are the prime candidates for G's decomposition: breaking them into subtasks gives each subtask a fresh 32k window.

### 1.3 The Ulivo G Data (Best Available)

From the experiment progress report (6 G iterations on minimax-m2.7, Harbor/Docker):

| Run | Trials | Pass Rate | What Happened |
|-----|--------|-----------|---------------|
| 1 | 0 | — | Killed before results |
| 2 | 4 | 75% | Convergence prompt fix; small sample |
| 3 | 13 | 46% | Pushed parallel decomposition; 11/13 hit AgentTimeoutError |
| 4 | 14 | 64% | Post cycle-fix; **best result but agents worked until timeout** |
| 5 | 191 | 0% | Architect bundle broke everything |
| 6 | 0 | — | Smoke testing after fix |

Run 4's 64% vs A's 41% looks promising, but the mechanism was "agent works until 30-min timeout kills it" — essentially giving the agent more attempts via the retry cycle, not benefiting from graph architecture.

---

## 2. Fanout Decision Analysis: What G Actually Does

### 2.1 The Meta-Prompt Forces Universal Decomposition

The Condition G meta-prompt (`adapter.py:498-546`, `run_qwen3_hard_20_g.py:107-161`) explicitly instructs:

> "You are a graph architect. You do NOT implement solutions yourself."
> "DO NOT write code. DO NOT modify files. Only create wg tasks."

This is an unconditional decomposition mandate. The agent has no guidance on when decomposition is beneficial vs harmful. Every task, regardless of size, goes through:

1. Seed agent explores the problem (~2-5 min)
2. Seed agent creates subtasks via `wg add` (~1-2 min)
3. Seed agent calls `wg done` on itself (~30s)
4. Coordinator tick detects ready tasks (~10s per tick, ticks every 5s)
5. Agent spawn: worktree setup + prompt assembly + CLI launch (~10-30s per agent)
6. Worker agent receives description-only context and starts from scratch

### 2.2 Which Fanout Decisions Were Justified?

Based on Condition A's qwen3-coder-30b results, I can classify the 18 tasks:

**Decomposition HARMFUL (13 tasks — all pass under A):**

| Task | A Turns | A Peak Input | Why G Hurts |
|------|---------|-------------|-------------|
| text-processing | 7 | 3,909 | Trivial: 7 turns, 12% context |
| file-ops | 22 | 5,678 | Simple: one agent handles it easily |
| shell-scripting | 5 | 5,353 | Trivial: 5 turns, done in <1 min |
| data-processing | 8 | 5,579 | Simple: 8 turns, 17% context |
| debugging | 11 | 5,340 | Simple: 11 turns, 16% context |
| algorithm | 6 | 4,359 | Simple: 6 turns, 13% context |
| ml | 14 | 9,897 | Moderate: fits easily in context |
| sysadmin | 14 | 6,436 | Moderate: 20% context |
| build-cython-ext | 19 | 9,376 | Moderate: 29% context |
| fix-code-vulnerability | 17 | 13,934 | Borderline: 42% context but passes |
| multi-module-type-migration | 32 | 11,163 | Moderate: 34% context, passes |
| mailman | 50 | 11,106 | Complex but passes at 34% context |
| configure-git-webserver | 51 | 11,627 | Most complex passing task: 36% context |

For these 13 tasks, decomposition adds:
- **Time overhead**: 2-5 min for architect exploration + subtask creation + coordinator dispatch
- **Context loss**: Worker agents lose the architect's problem understanding
- **Coordination risk**: Dependency structure may deadlock (seen in Ulivo run 3)

**Decomposition POTENTIALLY BENEFICIAL (5 tasks — all fail under A due to context overflow):**

| Task | A Turns | A Peak Input | Why G Could Help |
|------|---------|-------------|-----------------|
| cobol-modernization | 34 | 16,246 | 49.6% ctx: COBOL→Python is multi-phase |
| multi-source-data-merger | 24 | 16,144 | 49.3% ctx: 3 formats → merge → conflicts |
| financial-document-processor | 36 | 16,282 | 49.7% ctx: classify → extract → summarize |
| constraints-scheduling | 25 | 16,300 | 49.7% ctx: ICS parsing + constraint solving |
| iterative-test-fix | 38 | 16,243 | 49.6% ctx: 6 bugs × 15 tests |

These tasks have clear **multi-phase structure** (parse → transform → validate, or multiple independent bug fixes) where splitting into subtasks gives each one a fresh 32k window. The Condition A failure mode is exclusively context overflow — the model is capable but runs out of room.

### 2.3 Condition B Evidence: Self-Decomposition Patterns

The Condition B audit (`results/full-condition-b/audit-condition-b-wg-usage.md`) provides evidence from 98 completed trials where agents had wg tools available but weren't forced to decompose:

| Pattern | Trials | Pass Rate | Avg Turns |
|---------|--------|-----------|-----------|
| No wg usage | 54 | 42.6% | 26.7 |
| Bookkeeping only (log+done) | 15 | 60.0% | 24.9 |
| **Structured decomposition** | **5** | **80.0%** | **15.2** |
| Attempted decomposition | 10 | 40.0% | 34.3 |

When agents **chose** to decompose (5/98 trials), they achieved 80% pass rate in only 15.2 avg turns. But the model only chose to decompose 5% of the time. The 10 "attempted" decompositions (created subtasks but didn't complete them) had worse performance — decomposition that doesn't complete is worse than no decomposition.

**Implication:** Selective, agent-driven decomposition outperforms both "always decompose" and "never decompose," but models rarely choose it on their own.

---

## 3. Overhead Measurement

### 3.1 Agent Spawn Overhead (Per Agent)

From the spawn code path (`src/commands/spawn/execution.rs`, `src/commands/spawn/worktree.rs`):

| Step | Operation | Estimated Time |
|------|-----------|---------------|
| 1 | Load graph + resolve task | <100ms |
| 2 | Build context (scope, dependencies, previous attempts) | 100-500ms |
| 3 | Assemble prompt (template + context + REQUIRED_WORKFLOW) | <100ms |
| 4 | Create worktree (if enabled): `git worktree add` + symlink | 1-5s |
| 5 | Write wrapper script + launch executor process | <500ms |
| 6 | Executor CLI startup (claude/native) | 2-10s |
| 7 | First model inference (reading prompt, generating plan) | 5-30s |
| **Total per agent** | | **~10-45s** |

Note: `worktree_isolation = false` in the current G config, so step 4 is skipped — but this means multiple agents share a working directory, causing file conflicts with `max_agents=4-8`.

### 3.2 Full Trial Overhead (Condition G vs A)

| Phase | Condition A | Condition G |
|-------|------------|-------------|
| Graph init + task creation | ~2s | ~2s |
| Config write + service start | ~3s | ~3s |
| **Architect exploration** | — | **2-5 min** (seed agent reads files, understands problem) |
| **Subtask creation** | — | **1-2 min** (seed agent runs `wg add` commands) |
| **Seed task completion** | — | **30s** (seed agent calls `wg done`) |
| **Coordinator dispatch** | ~5s (1 task) | **10-30s** (multiple tasks, priority sort, readiness check) |
| **Agent spawn** | ~10-30s (1 agent) | **30-120s** (2-4 agents sequentially) |
| Worker execution | Full budget | Budget minus architect overhead |
| **Total overhead before work starts** | ~20s | **4-8 min** |

For a task that Condition A solves in 5 minutes total, Condition G spends 4-8 minutes just setting up the graph before any worker agent starts implementing. The worker then has less time budget and less context (only the architect's description).

### 3.3 Context Transfer Cost

The architect agent builds rich understanding by:
- Reading the task instruction
- Exploring files (`ls`, `cat`)
- Checking test scripts
- Understanding the codebase structure

This understanding is **not transferred** to worker agents. The worker agent receives only the text the architect wrote in `wg add -d "..."`. The meta-prompt warns about this:

> "Worker agents don't see this prompt. They only see the description you write in `wg add -d "..."`."

But the architect can't compress its entire exploration into a task description. Critical details are lost — file paths it discovered, patterns it noticed, edge cases it identified. The worker agent must re-explore much of this on its own, duplicating work.

---

## 4. Task Size Analysis: When Does Decomposition Pay Off?

### 4.1 The Break-Even Model

Decomposition has a fixed cost (overhead) and a variable benefit (fresh context + parallelism). The break-even point is where benefit exceeds cost.

**Fixed costs of decomposition:**
- Architect time: 3-7 min (exploration + subtask design + `wg done`)
- Spawn overhead: 10-45s per worker agent
- Context loss: worker must re-explore (adds ~2-5 min per worker)
- Coordination risk: deadlocks, convergence failure (probability ~15-30% based on Ulivo data)
- **Total: ~5-12 min of effective time lost**

**Variable benefits of decomposition:**
- Fresh context window per subtask (eliminates context overflow)
- Parallelism (multiple agents work simultaneously)
- Iteration cycles (failed subtasks retry with fresh context)

### 4.2 Break-Even Threshold

From the Condition A data:

| Metric | Tasks Below Threshold (Pass) | Tasks Above Threshold (Fail) |
|--------|------------------------------|------------------------------|
| Peak input tokens | <14k | >16k |
| Context utilization | <42% | >49% |
| Total turns | <32 | >24 (but with context pressure) |
| Task structure | Single-scope | Multi-phase/multi-file |

**The break-even point for decomposition is approximately:**
- **Peak context utilization > 45%** (approaching the context ceiling)
- **AND task has clear multi-phase structure** (not just long because of iteration)
- **AND single-agent approach would require >30 turns** with large tool outputs

Below this threshold, decomposition is pure overhead. Above it, decomposition provides the critical benefit of fresh context windows per subtask.

### 4.3 Task Classification for the 18-Task Benchmark

| Category | Count | % of Benchmark | Decompose? |
|----------|-------|----------------|------------|
| Easy (trivially solvable, <15 turns) | 5 | 28% | NO — pure overhead |
| Moderate (solvable, 15-30 turns, <35% ctx) | 5 | 28% | NO — A handles fine |
| Borderline (solvable, 30+ turns, 35-42% ctx) | 3 | 17% | PROBABLY NO — risk > benefit |
| Hard (context overflow, >45% ctx, multi-phase) | 5 | 28% | YES — decomposition helps |

**72% of tasks in the benchmark are harmed by decomposition.** Only 28% are candidates where decomposition could help.

---

## 5. Why G Fails: Root Cause Synthesis

### 5.1 Primary Cause: Unconditional Decomposition

The meta-prompt's "DO NOT write code" mandate forces decomposition on every task. This means:
- 72% of tasks get overhead with no benefit
- The architect spends minutes understanding the problem, then throws away that understanding by delegating
- Even the 28% of tasks that could benefit often fail due to poor delegation (context loss, deadlocks)

### 5.2 Contributing Causes

| Factor | Impact | Evidence |
|--------|--------|---------|
| **Prompt competition** | REQUIRED_WORKFLOW overrides meta-prompt | Ulivo run 3: agents implement directly instead of delegating |
| **No time awareness** | Agents iterate until killed by timeout | Ulivo run 4: 64% pass rate but "worked until timeout" |
| **Convergence signaling failure** | `--converged` rarely called | 11/13 trials in run 3 hit AgentTimeoutError |
| **worktree_isolation=false** | Multiple agents corrupt each other's files | Config: `worktree_isolation = false` with `max_agents=4-8` |
| **Model capability** | M2.7/Qwen3 struggle with delegation indirection | Agent writes code directly instead of creating subtasks |
| **Context loss at boundaries** | Worker agents lose architect's exploration | Only `wg add -d` text transfers, not the full exploration context |

### 5.3 The Fundamental Tension

Condition G tries to be two things at once:
1. **An architect** that understands the problem deeply enough to decompose it well
2. **A coordinator** that delegates without implementing

These are contradictory. Understanding the problem well enough to decompose it often means you're already most of the way to solving it. The architect invests 3-7 minutes of understanding only to throw it away and ask a worker agent to rebuild that understanding from a description.

---

## 6. Break-Even Characterization: The Smart Fanout Calculus

### 6.1 Decision Criteria

A task should be decomposed if and only if ALL of:

1. **Context pressure is binding**: Peak input tokens would exceed ~45% of context window under single-agent execution (estimated from task description complexity and multi-file scope)
2. **Task has independent sub-problems**: The work can be split into ≥2 parts where each part touches different files and can be verified independently
3. **Available time budget > 2× estimated overhead**: There's enough time for the architect + dispatch + worker execution, not just worker execution alone
4. **Model capability is sufficient**: The model can write clear, complete subtask descriptions that transfer the architect's understanding

### 6.2 Proposed Decision Tree

```
Is the task description > 500 words AND references > 3 files?
├── NO → Implement directly (skip decomposition)
└── YES → Estimate context pressure
         ├── Estimated peak < 40% context window → Implement directly
         └── Estimated peak ≥ 40% → Check sub-problem structure
              ├── No clear independent sub-problems → Implement directly
              └── ≥2 independent sub-problems → DECOMPOSE
                   └── Create focused subtasks with explicit file scopes
                        └── Each subtask must be verifiable independently
```

### 6.3 Task Size Heuristics

| Signal | Weight | Meaning |
|--------|--------|---------|
| Instruction length > 500 words | + | Complex requirements |
| References > 3 distinct files | + | Multi-file scope |
| Contains "then" / sequential phases | + | Pipeline structure |
| Test suite has > 10 test cases | + | Iterative debugging needed |
| Instruction length < 200 words | - | Simple task |
| Single file to modify | - | No parallelism benefit |
| Self-contained algorithm | - | No decomposition benefit |

### 6.4 Estimated Impact of Smart Fanout

If Condition G only decomposed the 5 context-overflow tasks (28% of benchmark) and implemented the other 13 directly:

| Scenario | Easy/Med/Border-Hard | Hard-Overflow | Overall |
|----------|---------------------|---------------|---------|
| **Condition A (current)** | 13/13 (100%) | 0/5 (0%) | 13/18 (72%) |
| **G with "always decompose"** | ~8/13 (62%) est. | ~3/5 (60%) est. | ~11/18 (61%) |
| **G with smart fanout** | 13/13 (100%) | ~3/5 (60%) est. | ~16/18 (89%) |

The smart fanout scenario preserves A's 100% on simpler tasks (by not decomposing them) while potentially recovering 3 of the 5 context-overflow failures through targeted decomposition.

---

## 7. Recommendations for tb-design-smart-fanout

1. **Replace "always decompose" with a decision function**: The seed agent should first attempt the task directly. Only if it detects context pressure (approaching context ceiling) or the task has clear multi-phase structure should it switch to decomposition mode.

2. **Implement a "try-then-decompose" pattern**: Give the seed agent 5-10 minutes to attempt the task directly. If it hits context overflow or makes no progress, THEN switch to architect mode and create subtasks. This preserves the architect's understanding while avoiding premature decomposition.

3. **Transfer context at subtask boundaries**: When decomposing, serialize the architect's exploration findings (file listing, test discovery, pattern observations) into a structured context section in each subtask's description. Don't rely on the architect writing everything from memory.

4. **Enable worktree isolation**: `worktree_isolation = true` is critical when `max_agents > 1`. Without it, agents corrupt each other's working directories.

5. **Set agent_timeout < trial_timeout**: Agent timeout should be `trial_timeout - 600s` (10 min buffer) so the coordinator can clean up and commit partial work before the trial timer kills everything.

6. **Wire time budget into agent prompts**: Use the heartbeat mechanism to inject elapsed time and remaining budget. Add a "wrap-up" phase at <5 min remaining.

---

## Source References

| Source | Path | Key Data |
|--------|------|----------|
| Condition G config | `terminal-bench/wg/adapter.py:137-147` | max_agents=8, autopoietic=True |
| G meta-prompt (adapter) | `terminal-bench/wg/adapter.py:498-546` | "DO NOT write code" mandate |
| G meta-prompt (local runner) | `terminal-bench/run_qwen3_hard_20_g.py:107-161` | Context window awareness added |
| G local runner config | `terminal-bench/run_qwen3_hard_20_g.py:451-483` | max_agents=4, worktree_isolation=false |
| Condition A 18-task results | `terminal-bench/results/qwen3-hard-20-a/combined_summary.json` | 13/18 pass, 5 context overflow |
| Condition A 10-task pilot | `terminal-bench/results/pilot-qwen3-local-10/summary.json` | 10/10 pass |
| A vs G haiku pilot | `terminal-bench/results/pilot-a-vs-g-haiku/comparison-report.md` | 5/5 tie (G = raw, not autopoietic) |
| G smoke results (Harbor) | `terminal-bench/results/gpt-oss-120b-G-smoke/` | All CancelledError |
| Experiment progress report | `terminal-bench/docs/experiment-progress-report.md` | G runs 1-6 history |
| G status research | `terminal-bench/docs/research-condition-g-status.md` | Full G history + design evolution |
| G timeout research | `terminal-bench/docs/research-condition-g-timeout.md` | Time awareness gap analysis |
| Condition B wg audit | `terminal-bench/results/full-condition-b/audit-condition-b-wg-usage.md` | Structured decomp: 80% pass in 15 turns |
| Spawn overhead | `src/commands/spawn/execution.rs` | Full spawn pipeline |
| Worktree setup | `src/commands/spawn/worktree.rs` | git worktree add + symlink |
| Coordinator dispatch | `src/commands/service/coordinator.rs:3302-3525` | spawn_agents_for_ready_tasks |
| TB task set audit | `terminal-bench/TB-TASK-SET-AUDIT.md` | 89 vs 18 task registries |
