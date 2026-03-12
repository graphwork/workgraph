# Validate: Core Dispatch — Self-Dispatch, Model Routing, Cycles

**Date:** 2026-03-07
**Task:** validate-core-dispatch
**Status:** ALL PASS

## Test 1: Self-Dispatch Loop

**Result: PASS**

Created a trivial probe task (`validation-probe-self`) via `wg add`. Observed:

1. Task created at `19:35:05`
2. Coordinator dispatched at `19:35:07` (~2s) — `--executor claude --model opus [agent-7326]`
3. Agent logged `PROBE_OK` and completed at `19:35:15` (~8s total)
4. `.evaluate-validation-probe-self` auto-created and dispatched with `eval inline --model haiku [agent-7327]`

The full self-dispatch loop — create, dispatch, execute, auto-evaluate — works end-to-end with no manual intervention.

## Test 2: Model Routing

**Result: PASS**

Sampled 12 tasks across 3 categories. Model routing is consistent and correct:

| Task Type | Executor | Model | Example Tasks |
|-----------|----------|-------|---------------|
| Code tasks | `claude` | `opus` | tui-liveness-display, infra-per-role-2, safety-mandatory-validation, agent-git-hygiene |
| Eval tasks | `eval inline` | `haiku` | .evaluate-tui-liveness-display, .evaluate-infra-per-role-2, .evaluate-safety-mandatory-validation, .evaluate-agent-git-hygiene |
| FLIP verify tasks | `claude` | `opus` | .verify-flip-tui-unified-markdown, .verify-flip-infra-per-role-2, .verify-flip-agency-executor-weight |

Configuration confirmed in `wg config --show`:
- Default executor: `claude`, model: `opus`
- `models.evaluator.model = "haiku"`
- `models.flip_inference.model = "sonnet"`
- `models.flip_comparison.model = "haiku"`

## Test 3: Cycle Test

**Result: PASS**

Created a 2-task cycle (`cycle-probe-step` -> `cycle-probe-step-2` -> back-edge with `--max-iterations 3`):

### Timeline

| Event | Time | Detail |
|-------|------|--------|
| Step A (original) dispatched | 19:36:08 | agent-7328, opus |
| Step A completed | 19:36:16 | CYCLE_A_OK logged |
| Step B dispatched | 19:36:37 | agent-7329, opus |
| Step B completed | 19:36:46 | Marked done |
| Step 3 (back-edge) dispatched | 19:40:27 | agent-7334, iteration 1/3 |
| Iteration 1 completed | 19:41:21 | Re-activated cycle |
| FLIP failure on Step B | 19:42:26 | Cycle failure restart 1/3 |
| Step B re-dispatched (iter 2) | 19:45:48 | agent-7341, CYCLE_B_OK logged |
| Step A re-dispatched (iter 2) | 19:46:02 | agent-7342, detected convergence |
| Cycle stopped | 19:46:28 | `--converged` at iteration 2/3 |

### Cycle Mechanics Verified
- Cycle detection: `wg cycles` shows the cycle as REDUCIBLE
- Iteration tracking: `Current iteration: 2/3` correctly maintained
- Re-activation: Tasks re-opened after each iteration
- Failure restart: FLIP verification failure triggered cycle restart
- Convergence: Agent used `--converged` to stop at iteration 2 (before max of 3)
- Tags: `converged` tag added to final task

## Summary

| Test | Status | Notes |
|------|--------|-------|
| Self-dispatch loop | PASS | ~2s dispatch latency, full loop including auto-eval |
| Model routing (2+ models) | PASS | 3 distinct model configs: opus (code), haiku (eval), sonnet (FLIP inference) |
| Cycle iterations + stop | PASS | 2/3 iterations ran, converged correctly |
