# FLIP Pipeline Ordering and TUI Integration

## What FLIP Is

FLIP (Fidelity via Latent Intent Probing) is a **roundtrip intent fidelity** metric. It does NOT check whether code compiles or tests pass. Instead:

1. **Inference phase**: An LLM reads only the agent's output (logs, artifacts, diffs) and reconstructs what the original task prompt must have been — without seeing the actual task description.
2. **Comparison phase**: A second LLM compares the inferred prompt to the actual task description, scoring similarity across four dimensions: semantic match, requirement coverage, specificity match, and hallucination rate.

The resulting `flip_score` (0.0–1.0) measures: **did the agent's output faithfully reflect what it was asked to do?** High FLIP = output clearly reflects the task. Low FLIP = agent went off-track or produced output that doesn't match the spec.

This is complementary to evaluation:
- **FLIP (fidelity)**: objective — does the output match the intent?
- **Eval (quality)**: subjective — was the approach good? Is the code clean?

### Current Pipeline

```
task completes
  -> .evaluate-* created (Phase 4 in coordinator tick)
     -> wg evaluate run <task>        [haiku grades quality]
     -> wg evaluate run <task> --flip  [sonnet infers + haiku compares, non-fatal]
  -> if FLIP < 0.70 (Phase 4.5)
     -> .verify-flip-* created         [opus independently checks the work]
```

Key observations from the code:
- FLIP runs as an **optional suffix** inside the `.evaluate-*` task script, appended with `|| true` (non-fatal)
- FLIP and standard eval share the same `.evaluate-*` task — FLIP isn't a separate task
- `.verify-flip-*` tasks (Opus verification) are triggered only after evaluation completes AND the FLIP score is below threshold
- Both run sequentially: standard eval first, then FLIP, then done/fail

Relevant code locations:
- `src/commands/service/coordinator.rs:1728-1814` — FLIP fragment construction and eval script
- `src/commands/service/coordinator.rs:1327-1506` — `.verify-flip-*` task creation
- `src/commands/evaluate.rs:499-849` — `run_flip()` implementation
- `src/agency/prompt.rs:567-780` — FLIP inference and comparison prompts
- `src/commands/viz/mod.rs:147-158` — phase annotation logic

---

## 1. Pipeline Ordering Recommendation

### Recommendation: Keep FLIP after eval, but separate into its own task

**Do NOT move FLIP before eval.** Instead, extract FLIP from the eval task script into its own `.flip-*` task that runs in parallel with `.evaluate-*`.

#### Rationale

**Against "FLIP first":**

1. **FLIP doesn't produce actionable data for the evaluator.** FLIP measures prompt-output fidelity — "did the output match the intent?" The evaluator already has the task description AND the output, so it can judge this directly. FLIP's value is as a *second opinion*, not as input data for the evaluator.

2. **Latency impact.** FLIP uses two LLM calls (inference with sonnet, comparison with haiku). Moving it before eval would add the full FLIP latency as a blocking prerequisite. Current pipeline: eval + FLIP run together in ~one task. Proposed "FLIP first" would serialize them.

3. **FLIP is not a build/test check.** The original intuition ("the evaluator grades blind without knowing if code compiles") conflates FLIP with verification. FLIP doesn't check compilation or test results — it checks fidelity. The evaluator already has access to artifacts and diffs; it doesn't need FLIP to tell it whether the output matches the spec.

**For parallel FLIP (recommended):**

1. **Independence.** FLIP and eval measure different things. Neither needs the other's output. They can run concurrently.

2. **Fault isolation.** Currently FLIP failure (`|| true`) is swallowed inside the eval task. A separate `.flip-*` task gives FLIP its own status, logs, token tracking, and failure handling.

3. **Selective execution.** Not all tasks need FLIP. Research/docs tasks, trivial fixes, and tasks without meaningful output logs gain little from FLIP. A separate task can be conditionally created based on task tags, type, or other criteria.

4. **Cost visibility.** Separate tasks make FLIP costs individually trackable without needing special token accounting logic.

### Proposed Pipeline

```
task completes
  -> .evaluate-*  created (quality eval, haiku)     [parallel]
  -> .flip-*      created (fidelity eval, sonnet+haiku) [parallel]
  -> if FLIP < threshold (Phase 4.5)
       -> .verify-flip-* created (Opus verification)
```

Both `.evaluate-*` and `.flip-*` run independently. The coordinator's Phase 4.5 checks FLIP results to trigger verification tasks.

### When to skip FLIP

FLIP should be **opt-in by default** for now (matching current `flip_enabled` config), but when enabled:
- Skip for research/docs tasks (no meaningful code output to infer from)
- Skip for tasks with very short execution times (< 30 seconds — likely trivial)
- Skip for system/internal tasks (`.` prefixed, already skipped)
- Always run for tasks tagged `flip-eval`

---

## 2. FLIP-Eval Relationship

### Recommendation: Independent but reported together

FLIP and eval should remain **independent processes** that produce separate evaluation records, but their results should be **displayed together** in the TUI.

#### Design

```
Evaluation record (source: "llm")
  - score: 0.85 (quality)
  - dimensions: {correctness, clarity, spec_adherence, ...}
  - notes: "Good implementation..."

FLIP record (source: "flip")
  - score: 0.72 (fidelity)
  - dimensions: {semantic_match, requirement_coverage, specificity_match, hallucination_rate}
  - notes: "Output closely matches..."
```

**They should NOT be merged into a single score.** Quality and fidelity are orthogonal:
- High quality + low fidelity = well-crafted code that doesn't match the spec
- Low quality + high fidelity = sloppy code that does what was asked
- Both high = ideal
- Both low = needs rework

**The `.verify-flip-*` threshold should trigger on fidelity (FLIP score), not on a blended score.** An agent can produce excellent quality code (high eval) that doesn't match the task spec at all (low FLIP) — that should still trigger verification.

#### Combined Assessment

For the TUI inspector and summary views, display a combined badge:

```
Quality: 0.85 ★★★★☆  |  Fidelity: 0.72 ★★★☆☆
```

The combined assessment is informational, not a gate. Gating decisions should be made on individual scores:
- `eval_gate_threshold` gates on quality (eval score)
- `flip_verification_threshold` gates on fidelity (FLIP score)

---

## 3. Token Tracking Design for FLIP

### Current State

- Eval tokens are tracked via the `.evaluate-*` task's `token_usage` field
- FLIP tokens are currently **invisible** — they're consumed inside the eval task script and lumped into the eval task's token count
- The TUI shows `∴` for eval tokens in the viz tree

### Recommended Design

With FLIP extracted to its own `.flip-*` task:

1. **Per-task tracking is automatic.** The `.flip-*` task gets its own `token_usage` field like any other task. No special accounting needed.

2. **New TUI indicator: `✓` for FLIP tokens.** Alongside `∴` (eval) and `⊳` (assign):

   ```
   my-task  (done · →1.2M ←50k ◎800k ⊳12k ∴8k ✓15k) 12m
   ```

   The `✓` indicator is already partially anticipated — `compute_phase_annotation` returns `[✓ validating]` for `.verify-flip-*` tasks.

3. **Implementation in `format_token_display`** (`src/graph.rs:542`):

   Add a fourth optional parameter for FLIP token usage:

   ```rust
   pub fn format_token_display(
       usage: Option<&TokenUsage>,
       assign_usage: Option<&TokenUsage>,
       eval_usage: Option<&TokenUsage>,
       flip_usage: Option<&TokenUsage>,  // NEW
   ) -> Option<String> {
       // ... existing logic ...
       if let Some(f) = flip_usage {
           let ftok = f.total_input() + f.output_tokens;
           if ftok > 0 {
               s.push_str(&format!(" ✓{}", format_tokens(ftok)));
           }
       }
   }
   ```

4. **Build FLIP token usage map** in `src/commands/viz/mod.rs`, parallel to `eval_token_usage`:

   ```rust
   let mut flip_token_usage: HashMap<String, TokenUsage> = HashMap::new();
   // Collect from .flip-* tasks, keyed by parent task ID
   ```

5. **Cumulative FLIP spend**: The TUI status bar already aggregates `total_usage` across all visible tasks. FLIP tasks are normal tasks — their tokens automatically flow into the aggregate.

### Cost Estimates

FLIP uses two LLM calls per task:
- Inference: sonnet (moderate cost)
- Comparison: haiku (low cost)

Typical per-task FLIP cost: ~$0.01-0.05 depending on output size.

Making this visible per-task prevents surprise cost accumulation.

---

## 4. TUI Inspector Integration for FLIP Results

### Current Inspector State

The TUI inspector (`src/tui/viz_viewer/state.rs`) has an Agency tab showing:
- Task Lifecycle: Assignment -> Execution -> Evaluation
- Each phase shows: status, agent, token usage, runtime
- Evaluation phase shows: score, notes

FLIP results are **not displayed** anywhere in the inspector.

### Recommended Design

#### 4.1 Extend AgencyLifecycle with a FLIP Phase

```rust
pub struct AgencyLifecycle {
    pub task_id: String,
    pub assignment: Option<LifecyclePhase>,
    pub execution: Option<LifecyclePhase>,
    pub evaluation: Option<LifecyclePhase>,
    pub flip: Option<FlipPhase>,          // NEW
    pub verification: Option<LifecyclePhase>,  // NEW (for .verify-flip-*)
}

pub struct FlipPhase {
    pub task_id: String,        // .flip-{task_id}
    pub status: Status,
    pub flip_score: Option<f64>,
    pub dimensions: Option<FlipDimensions>,
    pub inferred_prompt: Option<String>,  // What the LLM thought the task was
    pub token_usage: Option<TokenUsage>,
    pub runtime_secs: Option<i64>,
}

pub struct FlipDimensions {
    pub semantic_match: f64,
    pub requirement_coverage: f64,
    pub specificity_match: f64,
    pub hallucination_rate: f64,
}
```

#### 4.2 Agency Tab Rendering

Extend the lifecycle display in `src/tui/viz_viewer/render.rs:3233`:

```
-- Task Lifecycle --
  [*] ⊳ Assignment       [a]   agent-1234   12k tokens  $0.002   3s
  [*] ▸ Execution                agent-1235   1.2M tokens $0.45    12m
  [*] ∴ Evaluation        [e]   agent-1236   8k tokens   $0.001   5s
      Score: 0.85
      Good implementation, clean code...
  [*] ✓ FLIP Fidelity     [f]   agent-1237   15k tokens  $0.02    8s
      Fidelity: 0.72
      Dimensions: semantic=0.80 coverage=0.65 specificity=0.75 halluc=0.10
      Inferred prompt: "Implement the widget parser..."
  [ ] ⚠ Verification            (pending — FLIP 0.72 < 0.70 threshold)
```

Key features:
- **`[f]` keybinding** to navigate to the `.flip-*` task (parallel to `[a]` for assign, `[e]` for eval)
- **Fidelity score prominently displayed** with same formatting as eval score
- **Dimension breakdown** showing the four FLIP dimensions
- **Inferred prompt preview** (truncated) — this is often the most useful output from FLIP, as it reveals what the LLM "thought" the task was from the output alone
- **Verification status** when a `.verify-flip-*` task exists

#### 4.3 HUD Detail Integration

In the existing HUD detail view (`src/tui/viz_viewer/state.rs:3271`), add FLIP info:

```
-- Tokens --
  →1.2M ←50k +800k cached  ($0.45)
  Assignment:  ⊳12k   ($0.002)
  Evaluation:  ∴8k    ($0.001)
  FLIP:        ✓15k   ($0.02)
  Total: $0.473

-- Evaluation --
  Score: 0.85 (Exceptional)
  Notes: Good implementation...

-- FLIP Fidelity --
  Score: 0.72
  Semantic Match:       0.80
  Requirement Coverage: 0.65
  Specificity Match:    0.75
  Hallucination Rate:   0.10
```

---

## 5. Unified Result View

### Combined Evaluation + Validation Card

For the inspector detail view, show a combined card when both eval and FLIP results exist:

```
┌─────────────────────────────────────────┐
│  Quality: 0.85 ████████░░  Exceptional  │
│  Fidelity: 0.72 ███████░░░             │
│                                         │
│  Eval Dimensions:                       │
│    correctness    0.90 █████████░       │
│    clarity        0.85 ████████░░       │
│    spec_adherence 0.80 ████████░░       │
│                                         │
│  FLIP Dimensions:                       │
│    semantic_match       0.80            │
│    requirement_coverage 0.65            │
│    specificity_match    0.75            │
│    hallucination_rate   0.10            │
│                                         │
│  Costs: ∴ $0.001  ✓ $0.02              │
│  Verification: pending (score < 0.70)   │
└─────────────────────────────────────────┘
```

### Alert States

Color-code based on results:
- Both green: eval >= 0.60, FLIP >= 0.70 — task is solid
- Yellow warning: eval < 0.60 OR FLIP < 0.70 — review recommended
- Red alert: FLIP < 0.50 — agent likely went off-track, verification needed
- Verification in progress: pink pulsing (matching existing agency phase styling)

---

## 6. Implementation Plan

### Phase 1: Extract FLIP into Separate Task (Medium)

**Files:** `src/commands/service/coordinator.rs`

1. Create `build_auto_flip_tasks()` function parallel to `build_auto_evaluate_tasks()`
2. Generate `.flip-{task_id}` tasks with `exec: "wg evaluate run '{task_id}' --flip"`
3. Remove FLIP fragment from eval task script (`flip_fragment` construction)
4. Add Phase 4.1 in coordinator tick between eval and FLIP verification
5. Update `filter_internal_tasks` to recognize `.flip-*` prefix
6. Add phase annotation: `.flip-*` -> `[✓ fidelity]`

### Phase 2: FLIP Token Display (Small)

**Files:** `src/graph.rs`, `src/commands/viz/mod.rs`, `src/commands/viz/ascii.rs`

1. Add `flip_usage` parameter to `format_token_display()`
2. Build `flip_token_usage` map in viz module (keyed by parent task ID from `.flip-*` tasks)
3. Pass through to `generate_ascii()` and `generate_graph()`
4. Display `✓{tokens}` indicator

### Phase 3: TUI Lifecycle Extension (Medium)

**Files:** `src/tui/viz_viewer/state.rs`, `src/tui/viz_viewer/render.rs`

1. Add `FlipPhase` and `verification` fields to `AgencyLifecycle`
2. Load FLIP evaluation data from `agency/evaluations/` (filter by `source: "flip"`)
3. Add `[f]` keybinding for FLIP task navigation
4. Render FLIP phase in Agency tab lifecycle view
5. Show FLIP dimensions and inferred prompt in detail view

### Phase 4: HUD and Inspector Integration (Small)

**Files:** `src/tui/viz_viewer/state.rs` (HUD detail builder), `src/tui/viz_viewer/render.rs`

1. Add FLIP section to HUD detail (`build_hud_lines()`)
2. Show combined eval + FLIP token costs
3. Add FLIP score to compact task metadata

### Phase 5: Unified Result View (Small)

**Files:** `src/tui/viz_viewer/render.rs`

1. Build combined evaluation+fidelity card widget
2. Add color-coded alert states
3. Show verification status when `.verify-flip-*` exists

### Dependencies

```
Phase 1 (extract FLIP task)
  -> Phase 2 (token display) [needs .flip-* tasks to exist]
  -> Phase 3 (lifecycle)     [needs .flip-* tasks and data]
     -> Phase 4 (HUD)        [needs lifecycle data]
     -> Phase 5 (unified)    [needs lifecycle data]
```

Phase 2 and Phase 3 can run in parallel after Phase 1.
Phases 4 and 5 can run in parallel after Phase 3.
