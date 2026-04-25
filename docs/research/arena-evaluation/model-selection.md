# Arena-Based Model Selection for Workgraph

Research doc covering how FLIP-style arena evaluation (Wang et al., 2025; arXiv:2602.13551) can drive model selection in workgraph.

## 1. Current Model Selection

Workgraph resolves which model runs a task through a fixed hierarchy (`src/commands/spawn.rs:209-213`):

```
task.model > executor.model > coordinator.model (CLI --model) > default
```

**Per-task override:** `wg add "title" --model sonnet` sets `task.model` on the Task struct (`src/graph.rs`). Highest priority.

**Executor-level default:** Each executor config (`.workgraph/executors/<name>.toml`) can specify a `model` field (`src/service/executor.rs:256-259`). Useful for giving the `amplifier` executor a different default than `claude`.

**Coordinator default:** `wg config coordinator.model <model>` or `wg service start --model <model>`. Stored in `CoordinatorConfig.model` (`src/config.rs:225`). Applied to all spawned agents unless overridden above.

**Model registry:** `wg models list/add/default` manages `.workgraph/models.yaml` (`src/models.rs`). The registry catalogs models with cost, tier (frontier/mid/budget), capabilities, and context window metadata. Currently informational — the coordinator doesn't query it when spawning. The `default_model` field exists but isn't wired into the spawn hierarchy.

**Key gap:** Model selection is entirely static. The user (or config) picks a model before seeing results. There's no mechanism to compare model outputs on the same task and pick the best one.

## 2. Arena for Model Selection

The FLIP method (§4.2 of the paper) enables cheap Best-of-N selection:

1. Given a task with description `x`, run it through N candidate models → responses `{y₁, ..., yₙ}`
2. For each response, run backward inference: `x'ᵢ = FLIP(yᵢ)` — ask a small model to infer what instruction would produce `yᵢ`
3. Score each: `rᵢ = F1(x, x'ᵢ)` — word-level F1 between original task description and inferred instruction
4. Select the response with the highest score: `y* = argmax rᵢ`

**Why this works for workgraph:** Task descriptions are explicit instructions. FLIP measures how faithfully a response follows its instruction — exactly the quality signal workgraph needs. A response that addresses the task description well will allow a small model to reconstruct that description from the response alone.

**Scoring is model-agnostic and training-free.** The FLIP evaluator can be any small model (1B-12B parameters). It doesn't need to understand code quality — it just needs to generate plausible instructions from responses. The F1 computation is pure string matching.

## 3. When to Use Arena Selection

Arena adds N model calls per task. It's worth the cost in specific scenarios:

### High-value task routing
Before dispatching expensive, long-running tasks (frontier model for 30+ minutes), run a short probe through 2-3 candidate models. The probe cost is small relative to the full task cost. Use model registry tier/cost metadata to calculate break-even.

### During evolution (`wg evolve`)
Evolution proposes new roles and motivations. Arena can validate whether a proposed agent configuration actually improves output quality — run the same benchmark task with old vs. new configuration, score with FLIP, keep the winner.

### New or unfamiliar task types
When a task has skills/tags the system hasn't seen before, arena selection avoids committing to a possibly wrong model. Run 2-3 candidates on a representative subtask, then use the winner for the full workload.

### Model onboarding
When adding a new model to the registry (`wg models add`), run it through arena against the current default on a few representative tasks. This builds an empirical win-rate before trusting it with production work.

### When NOT to use it
- Routine tasks where a model has a proven track record (high avg_score in `PerformanceRecord`)
- Budget-constrained runs where N× cost isn't justified
- Tasks where latency matters more than quality (arena adds sequential model calls)

## 4. Cost/Latency Tradeoffs

### Cost model

For an arena with N candidate models and a FLIP evaluator model `E`:

```
Arena cost = Σᵢ cost(modelᵢ, task) + N × cost(E, backward_inference)
Normal cost = cost(selected_model, task)
Overhead = (N-1) × cost(avg_model, task) + N × cost(E, backward_inference)
```

The FLIP evaluator call is cheap — backward inference prompts are short (the response `y` plus a fixed instruction). Using a budget-tier model (e.g., `deepseek/deepseek-chat-v3` at $0.30/1M input) makes the evaluation cost negligible.

### Concrete example

Task: 10k-token description, 50k-token response. Arena with 3 models:

| Component | Cost |
|-----------|------|
| 3× model runs (avg $3/1M in, $15/1M out) | ~$2.40 |
| 3× FLIP eval (budget model, ~50k in) | ~$0.05 |
| Normal single run | ~$0.80 |
| **Arena overhead** | **~$1.65** (~2× normal) |

Arena is cost-effective when the quality gain from selecting the best model saves downstream rework, retry costs, or evaluation failures. With workgraph's retry mechanism (`max_retries`), a single failed attempt at $0.80 + retry at $0.80 = $1.60 — comparable to arena's upfront cost.

### Latency

Arena runs are inherently sequential if models share rate limits, or parallel if using different providers. With workgraph's multi-provider model registry, parallel arena runs are possible. The FLIP scoring step is fast (~1-2s per evaluation with a small model).

## 5. Integration with Model Registry and Per-Task Override

### Registry-driven candidate selection

The model registry (`src/models.rs`) already has the metadata needed for arena candidate selection:

- **Tier filtering:** Arena across tiers tests whether a budget model suffices (e.g., run `haiku` vs `sonnet` vs `opus` — if `haiku` wins, save 90% on that task type)
- **Capability matching:** Filter candidates by task skills (e.g., only arena models with `"coding"` capability for implementation tasks)
- **Cost budgeting:** Set a max arena budget; registry cost data determines how many candidates fit

### Per-task model override interaction

The existing `task.model` field takes priority in the spawn hierarchy. Arena should respect this:

- If `task.model` is set → skip arena, user made an explicit choice
- If `task.model` is unset → arena candidates come from registry, filtered by task skills
- Arena winner gets recorded as a recommendation (not forced), or optionally written to `task.model` for the actual dispatch

### Win-rate tracking

Arena results should feed back into the registry or a separate stats file:

```yaml
# .workgraph/arena-stats.yaml
win_rates:
  anthropic/claude-sonnet-4-latest:
    tasks_entered: 42
    wins: 28
    avg_flip_score: 0.73
  anthropic/claude-haiku-4-latest:
    tasks_entered: 42
    wins: 8
    avg_flip_score: 0.61
```

This data feeds into automatic model recommendation — over time, the system learns which model tier works for which task types without explicit configuration.

## 6. Implementation Sketch

### Option A: `wg arena-select` command

A standalone command that runs arena selection before dispatch:

```bash
# Run arena with 3 models from registry, pick the best
wg arena-select <task-id> --candidates 3

# Explicit model list
wg arena-select <task-id> --models sonnet,haiku,gpt-4o

# Filter by tier
wg arena-select <task-id> --tier mid

# Dry run — show scores without dispatching
wg arena-select <task-id> --dry-run
```

**Implementation path:**
1. New file: `src/commands/arena.rs`
2. For each candidate model, run a truncated version of the task (first N tokens of response, or a "plan only" probe)
3. Run FLIP evaluation on each response
4. Print ranking with scores; optionally set `task.model` to winner
5. Requires a FLIP evaluator — either call an API model or shell out to a local model

### Option B: Automatic arena in coordinator

Add `agency.arena_select: true` to config. When the coordinator encounters a task without `task.model` set:

1. Check if the task type has enough win-rate history → if yes, use the historically best model
2. If not, run a quick arena (2-3 candidates from different tiers)
3. Set `task.model` to winner, then proceed with normal spawn

**Integration point:** `spawn_agents_for_ready_tasks()` in `src/commands/service.rs:904`. Before the spawn loop, insert an arena selection phase for unassigned-model tasks.

### Option C: Arena as a probe task

Create a short-lived probe task that runs before the real task:

```bash
wg add "arena-probe-{task-id}" --after dependencies --before {task-id} \
  --exec "wg arena-select {task-id} --candidates 3"
```

This fits naturally into workgraph's graph model — the probe task completes, sets the model, then the real task dispatches with the selected model.

### Recommended approach

Start with **Option A** (`wg arena-select` command) because:
- No changes to coordinator logic
- Users opt in explicitly
- Easy to test and validate FLIP scoring accuracy on real workgraph tasks
- Win-rate data accumulates and informs whether Option B (automatic) is worth building

Then graduate to **Option B** once win-rate data shows meaningful quality differences between models for different task types.

### Key implementation details

**FLIP evaluator prompt** (from paper §3):
```
Given the following response, infer a single instruction that would most
plausibly generate this response. Output only the instruction, nothing else.

Response:
{response_text}
```

**F1 scoring** (from paper §3, pure Rust implementation):
```rust
fn flip_f1(original: &str, inferred: &str) -> f64 {
    let orig_tokens: HashSet<_> = original.split_whitespace().collect();
    let inf_tokens: HashSet<_> = inferred.split_whitespace().collect();
    let overlap = orig_tokens.intersection(&inf_tokens).count() as f64;
    let precision = overlap / inf_tokens.len().max(1) as f64;
    let recall = overlap / orig_tokens.len().max(1) as f64;
    if precision + recall == 0.0 { 0.0 }
    else { 2.0 * precision * recall / (precision + recall) }
}
```

**Probe strategy:** For cost efficiency, don't run the full task. Instead, ask each candidate model to produce a plan or outline (first 500 tokens). FLIP scoring works on partial responses — if the plan captures the task intent, FLIP will reconstruct the instruction. This reduces arena cost by ~10× compared to full task execution.
