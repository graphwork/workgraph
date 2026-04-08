# Research: Condition G Status and Design

**Date:** 2026-04-08
**Task:** research-condition-g
**Author:** Generated from codebase analysis, git history, and experiment docs

---

## 1. What Is Condition G?

Condition G is the **autopoietic** treatment condition in the Terminal-Bench experiment. Unlike conditions A-F where the agent receives a task and works on it directly, Condition G gives the agent guidance (via a meta-prompt) to **build its own self-correcting workgraph** with verification cycles, sub-task decomposition, and iterative refinement.

### Definition (current, as of commit `a8fdb3f9`)

| Property | Value |
|----------|-------|
| `context_scope` | `graph` |
| `exec_mode` | `full` |
| WG tools | Full access (all tools) |
| `max_agents` | **8** (vs 1 for all other conditions) |
| Coordinator agent | **Active** (dispatches sub-tasks) |
| Meta-prompt | Autopoietic graph-building guidance prepended to task instruction |
| Agency | None (no role/tradeoff assignment) |
| `exclude_wg_tools` | `false` |
| `autopoietic` | `true` (triggers meta-prompt injection) |

### The Autopoietic Meta-Prompt

The agent receives `CONDITION_G_META_PROMPT` prepended to the task instruction (defined in `terminal-bench/wg/adapter.py:480-528`). It instructs the agent to:

1. **Read and understand** the problem (explore files, check `tests/`)
2. **Decompose** into parallel sub-tasks using `wg add`
3. **Build a verification cycle**: work -> verify -> fix -> loop back (with `--max-iterations 5`)
4. **Signal convergence**: `wg done --converged` when tests pass, plain `wg done` to iterate
5. **Fan out** for parallelism where possible (up to 8 agents run concurrently)
6. **Mark the seed task done** after building the graph (so worker agents can be dispatched)

The agent is **not forced** to follow this structure -- it has full tools and can solve problems directly (like Condition F). The meta-prompt is guidance, not constraint.

### Key Distinction From Other Conditions

Condition G emulates what a **human** does when using workgraph: reading the problem, breaking it down, building a plan, checking results, and iterating. The agent is both the worker and the consciousness that evaluates completeness.

---

## 2. How Condition G Differs From Conditions A-F

| Aspect | A (Control) | B | C | D | E | F | **G (Autopoietic)** |
|--------|------------|---|---|---|---|---|---------------------|
| `context_scope` | `clean` | `task` | `task` | `task` | `graph` | `graph` | `graph` |
| WG tools | Excluded | Yes | Yes | Yes | Yes | Yes | Yes |
| `max_agents` | 1 | 1 | 1 | 1 | 1 | 1 | **8** |
| Agency | None | None | None | programmer/careful | architect/thorough | None | None |
| Coordinator | No | No | No | No | No | No | **Yes** |
| Meta-prompt | None | None | Skills/planning | None | Org generation | Distilled memory | **Autopoietic graph-building** |
| Graph structure | Single task | Single task | Single task | Single task | Single task | Single task | **Agent-designed (cycles, sub-tasks)** |
| Iteration | None | None | None | None | None | None | **Encouraged via meta-prompt** |

### Evolution of the G definition

Condition G went through two distinct phases:

1. **Phase 1 (commit `47ed02d8`, 2026-04-07):** Originally formalized as "F without surveillance" -- context-only injection. After pilot data showed surveillance loops activated 0 times across 95 trials, G was defined as F with surveillance infrastructure stripped out. Prediction: G ~= F in pass rate at ~2x lower token cost.

2. **Phase 2 (commit `84c2d81b`, 2026-04-08):** Evolved into the autopoietic design after observing that the 89-task TB 2.0 benchmark was harder than the 18-task pilots, and that iterative self-correction could improve pass rates beyond single-shot context injection. Key additions: `max_agents=8`, active coordinator, autopoietic meta-prompt.

---

## 3. Implementation Status

**Status: Implemented but experimental -- iterating on prompt design**

### What exists

| Artifact | Path | Status |
|----------|------|--------|
| `CONDITION_CONFIG["G"]` | `terminal-bench/wg/adapter.py:127-134` | Implemented |
| `CONDITION_G_META_PROMPT` | `terminal-bench/wg/adapter.py:480-528` | Implemented |
| `ConditionGAgent` class | `terminal-bench/wg/adapter.py:1496-1507` | Implemented |
| `ARCHITECT_BUNDLE_TOML` | `terminal-bench/wg/adapter.py:535-541` | Implemented (but reverted from use) |
| `reproduce.sh` condition G support | `terminal-bench/reproduce.sh:46` | Implemented |
| RUNBOOK section for G | `terminal-bench/docs/RUNBOOK.md:630-680` | Documented |
| Experiment handoff for G | `terminal-bench/docs/experiment-handoff.md:655-699` | Documented |

### Relevant git history (10 commits)

| Commit | Date | Description |
|--------|------|-------------|
| `47ed02d8` | Apr 7 | Formalize Condition G naming across docs |
| `84c2d81b` | Apr 8 | Initial implementation -- autopoietic self-correcting workgraph |
| `2e829ba7` | Apr 8 | Clarify meta-prompt for convergence signaling |
| `46f0f4a0` | Apr 8 | Bump max_agents to 8, push parallel decomposition |
| `b61a9e6b` | Apr 8 | Architect bundle for seed task (restrict tools to delegation-only) |
| `13315a3e` | Apr 8 | Use exec-mode bare (not architect) for seed task |
| `14ce8838` | Apr 8 | Revert: back to full exec_mode, no architect bundle |
| `a8fdb3f9` | Apr 8 | Update docs: G is autopoietic, not context-only |
| `0543df60` | Apr 8 | Fix cycle dispatch bug (Havlak header selection) found during G testing |
| `954e680a` | Apr 8 | Use Havlak DFS for cycle header detection (related fix) |

### Trial runs (from experiment-progress-report.md)

| Run | Trials | Pass Rate | Approach |
|-----|--------|-----------|----------|
| 1 | 0 | -- | Initial launch, killed before results |
| 2 | 4 | **75%** | Convergence prompt fix |
| 3 | 13 | **46%** | Parallel decomposition push |
| 4 | 14 | **64%** | Post cycle-fix in wg core (best result) |
| 5 | 191 | **0%** | Architect bundle -- `wg add --exec-mode architect` rejected |
| 6 | 0 | -- | Fix: use `--exec-mode bare`; smoke testing |

**Best result:** 64% pass rate (run 4) vs F's 45% on the same TB 2.0 89-task benchmark. However, this came from agents working until timeout rather than clean autopoietic iteration.

---

## 4. Comparison With Other Conditions: Results Summary

### Pilot results (18 custom tasks, 5 replicas)

| Condition | Pass Rate | Notes |
|-----------|-----------|-------|
| A (bare) | 41.6% (37/89 on TB 2.0) or 50% (4/8 matched) | Baseline |
| F (full wg) | 98.9% (89/90 on 18 custom) or 100% (40/40 matched) | Context injection drives all benefit |

### Full-scale TB 2.0 results (89 tasks, in progress)

| Condition | Completed | Passed | Pass Rate | Status |
|-----------|-----------|--------|-----------|--------|
| A (bare) | 178/445 | 74 | **41%** | Running |
| F (full wg) | 171/445 | 78 | **45%** | Running |
| G (autopoietic) | ~14 (best run) | ~9 | **~64%** | Experimental, iterating |

### Key observations

1. **Custom tasks vs TB 2.0:** The gap between A and F is dramatic on custom tasks (+50 pp on 8 matched tasks) but modest on the full TB 2.0 benchmark (+4 pp). Custom tasks were easier and more amenable to context-driven solutions.

2. **G's promise:** At 64% vs F's 45% on TB 2.0, G shows the potential of iterative self-correction. But the result is from only 14 trials and the agent often worked until timeout rather than cleanly iterating.

3. **Surveillance was zero-value:** Across 95 pilot trials, surveillance loops activated 0 times. All benefit came from context injection alone. This led to G being formalized as a distinct condition.

---

## 5. Core Technical Challenges and Blockers

### 5.1 Prompt competition (primary blocker)

The wg native executor injects a `REQUIRED_WORKFLOW` section into every agent's prompt (7-step mandatory workflow: log progress, validate, commit, `wg done`). This competes with the Condition G meta-prompt that says "build a graph, delegate, don't implement." M2.7 follows the louder, more structured `REQUIRED_WORKFLOW` and ignores the meta-prompt.

**Evidence:** In trials with artifacts, the seed agent typically solved the problem directly in the seed task rather than building a graph. When it did build graphs, it often created broken dependency structures (deadlocks). 11 of 13 trials in run 3 hit `AgentTimeoutError`.

### 5.2 Architect bundle approach failed

Attempted fix: use an "architect" bundle with `context_scope=clean` to eliminate the competing `REQUIRED_WORKFLOW`. Two problems:
- `wg add --exec-mode architect` was rejected (custom exec_mode names not supported, only `full`, `light`, `bare`, `shell`)
- Even after overriding `bare.toml` with architect content, M2.7 couldn't handle the indirection of delegation-only mode -- 0% pass rate on 191 trials (run 5)

**Resolution (commit `14ce8838`):** Reverted to full exec_mode with meta-prompt as guidance, not constraint. The agent gets full tools and the meta-prompt tells it to build a graph, but doesn't force it.

### 5.3 Cycle dispatch bug (found and fixed)

Discovered during G testing: when a user creates a cycle with `wg edit X --add-after Y --max-iterations N`, Havlak's algorithm sometimes picked a different header based on DFS traversal order, causing cycles to start from the wrong task or deadlock. Fixed in commit `0543df60` (then refined in `954e680a`).

### 5.4 Convergence signaling

Agents weren't calling `wg done --converged`, causing trials to run until Harbor's timeout killed them. Addressed by making the meta-prompt more explicit about when to use `--converged` vs plain `wg done`.

### 5.5 Model capability ceiling

M2.7 (the benchmark model at $0.30/M input, $1.20/M output) struggles with the indirection required by autopoietic mode. It can solve problems directly but has difficulty:
- Writing clear sub-task descriptions that transfer intent to worker agents
- Building correct dependency structures (cycles, fan-out-merge)
- Evaluating whether work is complete vs needs another iteration

A more capable model (e.g., Claude Sonnet/Opus) might handle the autopoietic pattern better, but would change the cost equation significantly.

---

## 6. Open Questions

1. **Is the autopoietic approach fundamentally better, or does it just add more compute (retry) budget?** The 64% result (run 4) came from agents working until timeout, suggesting the benefit may be from retries rather than graph architecture.

2. **Would G work better with a stronger model?** M2.7's inability to reliably delegate and evaluate is the core issue. A model with better planning/meta-cognition might unlock the autopoietic pattern.

3. **Can the prompt competition issue be solved without code changes?** The `REQUIRED_WORKFLOW` injection is baked into the native executor (`src/commands/service/coordinator.rs`). A potential fix: add a config flag to suppress `REQUIRED_WORKFLOW` for specific exec_modes or when `autopoietic: true`.

4. **Should G target the 89-task TB 2.0 benchmark, the 18 custom tasks, or both?** The custom tasks showed much larger A-vs-F gaps, suggesting they may also show larger G effects.

5. **What is the cost multiplier for G?** With 8 agents and retry loops, G likely uses 5-10x more tokens than A. Is the pass-rate improvement worth the cost?

6. **Full-scale run timing:** Conditions A and F are still running (~12-15h remaining as of the progress report). G needs a stable prompt/config before launching a full run.

---

## 7. Workgraph Tasks Related to Condition G

From `wg list`, the following tasks reference Condition G:

| Task | Status | Description |
|------|--------|-------------|
| `formalize-condition-g` | Done | Rename "F without surveillance" to Condition G |
| `.flip-formalize-condition-g` | Done | FLIP evaluation |
| `research-condition-g` | In-progress | This research task |

No open implementation or execution tasks for G exist in the current graph. A full-scale G run has not been scheduled as a workgraph task.

---

## 8. Recommendations

1. **Stabilize the meta-prompt** before launching a full-scale run. The current design (full tools + guidance meta-prompt) is the best approach found so far, but only 14 trials have been run with it.

2. **Run a focused pilot** on a subset of TB 2.0 tasks (e.g., the 8 matched tasks from the pilot comparison) to validate G's performance before committing to a full 89-task run.

3. **Consider a code-level fix for prompt competition:** Add a config flag (e.g., `suppress_required_workflow = true`) that the autopoietic condition can use to prevent the native executor from injecting the `REQUIRED_WORKFLOW` section.

4. **Track cost carefully:** G's multi-agent, multi-iteration approach will be significantly more expensive than A or F. Establish a cost budget before the full run.

5. **Compare against the right baseline:** Given that F shows only +4 pp over A on TB 2.0 (vs +50 pp on custom tasks), G's value proposition should be evaluated against the harder benchmark where the marginal improvement matters more.

---

## Source References

| Document | Path |
|----------|------|
| Adapter implementation | `terminal-bench/wg/adapter.py` |
| Experiment handoff | `terminal-bench/docs/experiment-handoff.md` |
| Reproduction runbook | `terminal-bench/docs/RUNBOOK.md` |
| Scale experiment design | `terminal-bench/docs/scale-experiment-design.md` |
| Progress report | `terminal-bench/docs/experiment-progress-report.md` |
| Pilot results synthesis | `terminal-bench/docs/pilot-results-synthesis.md` |
| Surveillance audit | `terminal-bench/docs/surveillance-audit.md` |
| Reproduce script | `terminal-bench/reproduce.sh` |
