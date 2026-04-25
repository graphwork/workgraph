# FLIP Model Mismatch Investigation

**Task:** tb-investigate-flip-model  
**Date:** 2026-04-05  
**Verdict:** YES, there is a model mismatch. FLIP always uses fixed evaluator models, never the task agent's model.

## Finding 1: FLIP uses hardcoded evaluator models, not the task agent's model

**Code evidence:**

The FLIP evaluation code in `src/commands/evaluate.rs:674-684` resolves models via the config dispatch role system:

```rust
// Line 674-680: Inference model
let inference_model = evaluator_model
    .map(std::string::ToString::to_string)
    .unwrap_or_else(|| {
        config
            .resolve_model_for_role(workgraph::config::DispatchRole::FlipInference)
            .model
    });

// Line 682-684: Comparison model
let comparison_model = config
    .resolve_model_for_role(workgraph::config::DispatchRole::FlipComparison)
    .model;
```

These resolve to the `[models]` section of `.workgraph/config.toml`:

```toml
[models.flip_inference]
model = "claude:sonnet"       # Line 118

[models.flip_comparison]
model = "claude:haiku"        # Line 121
```

The task agent's model is only recorded as metadata (line 823, 852):
```rust
let task_model = extract_spawn_model(&task.log).or_else(|| task.model.clone());
// ...stored in evaluation.model field, but never used for the actual FLIP LLM calls
```

## Finding 2: FLIP evaluates the outer TB trial task, not inner subtasks

The `.flip-<task-id>` task is scaffolded by `src/commands/eval_scaffold.rs:54-91` and executes:
```
wg evaluate run {task_id} --flip
```

This evaluates the task referenced by `task_id` directly (e.g., `tb-a-algorithm-r0`). In the current TB setup, trial tasks are atomic — they don't create subtasks. The trial agent writes code directly to /tmp/ paths. So FLIP evaluates artifacts and logs of the outer wrapper.

## Finding 3: Empirical confirmation from evaluation records

Checked 10+ TB evaluation records. Every FLIP evaluation shows the same evaluator pattern regardless of the task agent's model:

| Task | Source | Evaluator | Task Model | FLIP Score |
|------|--------|-----------|------------|------------|
| tb-a-algorithm-r0 | flip | flip:claude-sonnet-4-latest+claude-haiku-4-latest | claude-sonnet-4-latest | 0.03 |
| tb-a-algorithm-r1 | flip | flip:claude-sonnet-4-latest+claude-haiku-4-latest | claude-sonnet-4-latest | 0.065 |
| tb-a-algorithm-r2 | flip | flip:claude-sonnet-4-latest+claude-haiku-4-latest | claude-sonnet-4-latest | 0.0 |
| tb-d-algorithm-r0 | flip | flip:claude-sonnet-4-latest+claude-haiku-4-latest | claude-sonnet-4-latest | 0.826 |
| tb-d-algorithm-r1 | flip | flip:claude-sonnet-4-latest+claude-haiku-4-latest | claude-sonnet-4-latest | 0.82 |
| tb-e-algorithm-r0 | flip | flip:claude-sonnet-4-latest+claude-haiku-4-latest | claude-sonnet-4-latest | 0.9 |

**All FLIP evals use the same Sonnet+Haiku evaluator pair.** The task model (Sonnet 4.6) is recorded but not used for probing.

## Finding 4: No minimax m2.7 trials exist yet

The full-sweep-01 manifest (`terminal-bench/trials/manifest-full-sweep-01.json`) shows all conditions (A/C/D/E) used `model: "claude:claude-sonnet-4-latest"`. No minimax m2.7 trials have been run. The planned Condition F sweep (`tb-run-condition-f-sweep`) intends to use minimax m2.7.

## Analysis: Why FLIP scores are low for Condition A

The low Condition A FLIP scores (mean ~0.05) vs high Condition D/E scores (mean ~0.8) are NOT caused by a model mismatch — all trials used the same model (Sonnet 4.6). The low scores likely reflect the actual Condition A treatment (minimal context, no decomposition guidance) producing work that poorly matches what FLIP's inference model expects.

## Impact on Condition F (minimax m2.7)

When Condition F runs with minimax m2.7, there WILL be a genuine model mismatch:
- **FLIP Phase 1 (Inference):** Claude Sonnet tries to reverse-engineer a prompt from minimax m2.7's output
- **FLIP Phase 2 (Comparison):** Claude Haiku compares the inferred prompt to the actual prompt

This is problematic because:
1. Different models produce output with different patterns, verbosity, and reasoning styles
2. Sonnet may not accurately reverse-engineer what prompt a minimax model was working from
3. FLIP scores would conflate "task quality" with "evaluator-model alignment"

## Recommended Fix

Two options:

### Option A: Per-task FLIP model override (preferred)
Add a `--flip-model` flag or resolve FLIP inference model from the task's own model field:
```rust
// In run_flip(), before line 674:
let inference_model = task.model.clone()  // Use the task agent's model
    .or_else(|| evaluator_model.map(String::from))
    .unwrap_or_else(|| config.resolve_model_for_role(FlipInference).model);
```
This makes FLIP probe "the mind that did the work" for each task.

### Option B: Document the limitation
If FLIP is intentionally an external evaluator (testing output quality from a fixed perspective), document that cross-model FLIP scores are not comparable and should not be used for cross-model benchmarks like Condition F.

## Summary

| Question | Answer |
|----------|--------|
| Which model runs FLIP probes? | Always `claude:sonnet` (inference) + `claude:haiku` (comparison) per config |
| Does FLIP use the task agent's model? | No — task model is metadata only |
| Is there a model mismatch? | Not yet (all trials are Sonnet 4.6), but will be for Condition F (minimax m2.7) |
| Are current low Condition A scores caused by model mismatch? | No — caused by the condition treatment itself |
| Code evidence | `src/commands/evaluate.rs:674-684` (model selection), `src/commands/eval_scaffold.rs:54-91` (task creation) |
