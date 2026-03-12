# Design: Verify-First Eval Pipeline

**Task:** design-verify-first
**Date:** 2026-03-09
**Status:** Proposed

## Problem Statement

The current evaluation pipeline runs FLIP inside the eval task's inline script, and verification tasks created from low FLIP scores have no relationship back to the eval. This means:

1. **Eval and FLIP are coupled** — both run in a single bash script within `.evaluate-<task>`
2. **Verify is orphaned** — `.verify-flip-<task>` doesn't feed back into `.evaluate-<task>`
3. **Eval ignores verify** — the evaluator prompt never sees verification findings
4. **Timing is wrong** — eval is already done before verify even exists

## Current Flow

```
task-X completes
    │
    └─→ .evaluate-task-X  (spawned inline, runs both eval + FLIP in one script)
             │
             ├─ wg evaluate run task-X          (haiku scoring → Evaluation record)
             ├─ wg evaluate run task-X --flip   (FLIP two-phase → Evaluation record, source:"flip")
             └─ marks .evaluate-task-X done
                    │
                    │  (next coordinator tick, Phase 4.5)
                    └─→ build_flip_verification_tasks() scans FLIP evals
                         │
                         └─ FLIP < threshold → .verify-flip-task-X  (standalone, Opus)
                                                    └─→ .evaluate-.verify-flip-task-X  (eval of verify)
```

**Key files:**
- `src/commands/eval_scaffold.rs` — creates `.evaluate-<task>` at publish time
- `src/commands/evaluate.rs` — `run()` (standard eval) and `run_flip()` (FLIP eval)
- `src/commands/service/coordinator.rs`:
  - `spawn_eval_inline()` (line ~1578) — forks eval+FLIP bash script
  - `build_flip_verification_tasks()` (line ~1213) — creates verify tasks from low FLIP scores
  - `spawn_agents_for_ready_tasks()` (line ~1940) — recognizes eval tasks by `"evaluation"` tag + `exec` field

## Proposed Flow

```
task-X completes
    │
    ├─ [FLIP disabled] ──→ .evaluate-task-X (after: [task-X])
    │                       Standard eval, same as today
    │
    └─ [FLIP enabled]  ──→ .flip-task-X (after: [task-X])
                             │
                             ├─ FLIP score >= threshold
                             │   └─→ .evaluate-task-X (after: [.flip-task-X])
                             │       Eval prompt includes FLIP score
                             │
                             └─ FLIP score < threshold
                                 └─→ .verify-flip-task-X created, added as dep
                                     │
                                     └─→ .evaluate-task-X (after: [.flip-task-X, .verify-flip-task-X])
                                         Eval prompt includes FLIP score + verify findings
```

### Dependency Graph Changes

**FLIP disabled (backwards compatible, no change):**
```
task-X → .evaluate-task-X
```

**FLIP enabled, high score:**
```
task-X → .flip-task-X → .evaluate-task-X
```

**FLIP enabled, low score (verify triggered):**
```
task-X → .flip-task-X ──→ .evaluate-task-X
              │                  ↑
              └→ .verify-flip-task-X ─┘
```

## Detailed Design

### 1. Scaffold Phase (publish time)

**File:** `src/commands/eval_scaffold.rs`

When a task is published, `scaffold_eval_task()` currently creates `.evaluate-<task>` with `after: [task-X]`.

New behavior:
- Add `scaffold_flip_task()` that creates `.flip-<task>` when FLIP is enabled
- `.flip-<task>` properties:
  - `after: [task-X]`
  - `exec: "wg evaluate run <task> --flip"`
  - `exec_mode: "bare"`
  - `tags: ["flip", "agency"]`
  - `model`: resolved from `DispatchRole::FlipInference` (or evaluator model)
  - `visibility: "internal"`
- When FLIP is enabled, `.evaluate-<task>` gets `after: [.flip-<task>]` instead of `after: [task-X]`
- When FLIP is disabled, `.evaluate-<task>` gets `after: [task-X]` as today

The `scaffold_eval_task()` function signature stays the same but checks FLIP config to determine `after` dependency.

### 2. FLIP Inline Spawning

**File:** `src/commands/service/coordinator.rs`

The FLIP task (`.flip-<task>`) uses the same inline spawning path as eval tasks. Since `spawn_eval_inline()` already reads the `exec` field and uses it directly, FLIP tasks are recognized by:
- Tag: `"flip"` (new) — to distinguish from evaluation tasks
- `exec` field: contains `"wg evaluate run <task> --flip"`

Option A (simple): Add `"flip"` to the set of tags that trigger inline spawning alongside `"evaluation"`. The existing `spawn_eval_inline()` function works for both since it uses the `exec` field.

Option B: Rename `spawn_eval_inline()` to something generic like `spawn_inline_task()` and have it handle both eval and FLIP tasks.

**Recommended: Option A** — minimal code change, just expand the tag check at line ~1940:
```rust
let is_inline_task = task.tags.iter().any(|t| t == "evaluation" || t == "flip") && task.exec.is_some();
```

### 3. Remove FLIP Fragment from Eval Script

**File:** `src/commands/service/coordinator.rs`, `spawn_eval_inline()`

Currently, the inline eval script includes a FLIP command fragment:
```bash
wg evaluate run '<task>' --flip >> output 2>&1 || true
```

This fragment is removed entirely. FLIP now runs in its own `.flip-<task>` task.

The `flip_cmd`, `flip_fragment`, and related code (lines ~1620-1676) are deleted from `spawn_eval_inline()`.

### 4. Dynamic Verify → Eval Dependency

**File:** `src/commands/service/coordinator.rs`, `build_flip_verification_tasks()`

When creating `.verify-flip-<task>`, also add it as an `after` dependency on `.evaluate-<task>`:

```rust
// After creating .verify-flip-<task>, add it as a dep on .evaluate-<task>
let eval_task_id = format!(".evaluate-{}", source_task_id);
if let Some(eval_task) = graph.get_task_mut(&eval_task_id) {
    if !eval_task.after.contains(&verify_task_id) {
        eval_task.after.push(verify_task_id.clone());
    }
}
```

This ensures `.evaluate-<task>` doesn't become ready until verify completes. The eval task already exists (created at scaffold time) and is blocked on `.flip-<task>`. Adding `.verify-flip-<task>` as an additional dependency keeps it blocked until both are done.

### 5. Verify Context in Eval Prompt

**File:** `src/commands/evaluate.rs` + `src/agency/prompt.rs`

When `wg evaluate run <task>` executes:

1. Check for `.verify-flip-<task>` in the graph
2. If it exists and is done, extract:
   - Verify task status (done/failed)
   - Verify task log entries (contain the verdict)
   - FLIP score (from the `.flip-<task>` evaluation record)
3. Add to `EvaluatorInput`:
   ```rust
   pub struct EvaluatorInput<'a> {
       // ... existing fields ...
       pub flip_score: Option<f64>,
       pub verify_status: Option<&'a str>,  // "passed" | "failed" | None
       pub verify_findings: Option<&'a str>,
   }
   ```
4. `render_evaluator_prompt()` includes a new section when verify data is available:
   ```
   ## FLIP Verification Results
   FLIP Score: 0.45 (below threshold 0.70)
   Verification Status: PASSED
   Verification Findings:
   <log entries from .verify-flip-<task>>

   NOTE: Verification is a strong signal. If verification failed, this should
   significantly reduce the overall score. If verification passed despite low
   FLIP score, the FLIP may have been a false alarm.
   ```

### 6. Coordinator Tick Phase Ordering

The existing phase ordering naturally handles the new flow:

- **Phase 3** (auto-assign): Assigns ready tasks including `.flip-*` tasks
- **Phase 4** (auto-evaluate): Scaffolds eval tasks via `build_auto_evaluate_tasks()`
- **Phase 4.5** (FLIP verification): Creates `.verify-flip-*` tasks, adds deps to `.evaluate-*`
- **Phase 5+** (spawn): Spawns agents for ready tasks (`.flip-*` tasks are inline-spawnable)

The dependency graph ensures correct ordering:
1. `.flip-<task>` becomes ready when source task completes → spawned inline
2. After FLIP completes, next tick Phase 4.5 creates `.verify-flip-<task>` if needed
3. `.evaluate-<task>` becomes ready when `.flip-<task>` (and optionally `.verify-flip-<task>`) complete
4. Eval spawns inline with verify context available

No changes to phase ordering required.

## Implementation Tasks

### Task 1: `scaffold-flip-phase` — Extract FLIP into separate scaffolded task
**Files:** `src/commands/eval_scaffold.rs`
- Add `scaffold_flip_task()` function
- Create `.flip-<task>` with `exec: "wg evaluate run <task> --flip"`, `exec_mode: "bare"`, tags `["flip", "agency"]`
- Modify `.evaluate-<task>` to depend on `.flip-<task>` when FLIP is enabled
- Add `"flip"` tag to `DOMINATED_TAGS` to prevent eval-of-flip tasks
- **Validation:** Unit tests for scaffold_flip_task, verify .evaluate-* depends on .flip-* when FLIP enabled

### Task 2: `flip-inline-spawn` — Inline spawning for FLIP tasks
**Files:** `src/commands/service/coordinator.rs`
- Expand inline task recognition: `"evaluation" || "flip"` tag check
- Remove FLIP fragment from `spawn_eval_inline()` (delete lines ~1620-1676 FLIP-related code)
- **Validation:** Eval inline script no longer contains `--flip`, FLIP tasks spawn independently

### Task 3: `dynamic-verify-dep` — Wire verify as dynamic eval dependency
**Files:** `src/commands/service/coordinator.rs`
- In `build_flip_verification_tasks()`, after creating `.verify-flip-<task>`, add it to `.evaluate-<task>.after`
- **Validation:** After verify task creation, .evaluate-<task>.after contains .verify-flip-<task>

### Task 4: `verify-context-eval-prompt` — Pass verify findings into eval
**Files:** `src/commands/evaluate.rs`, `src/agency/prompt.rs`, `src/agency/types.rs`
- Add `flip_score`, `verify_status`, `verify_findings` to `EvaluatorInput`
- Load `.verify-flip-<task>` log entries in `evaluate::run()`
- Load FLIP evaluation record for the source task
- Render verify section in evaluator prompt
- **Validation:** Eval prompt includes verify findings when available; eval works normally when no verify exists

### Task 5: `verify-first-integration-tests` — End-to-end tests
**Files:** `tests/integration_verify_first.rs` (new)
- Test: FLIP disabled → eval depends on source task directly (backwards compatible)
- Test: FLIP enabled, high score → eval depends on .flip-<task>, runs with FLIP score
- Test: FLIP enabled, low score → verify created, eval blocked on verify, runs with findings
- Test: scaffold idempotency for .flip-* tasks
- **Validation:** All tests pass

### Dependency Graph Between Implementation Tasks

```
scaffold-flip-phase ──→ flip-inline-spawn ──→ verify-first-integration-tests
                    ──→ dynamic-verify-dep ──→ verify-first-integration-tests
                    ──→ verify-context-eval-prompt ──→ verify-first-integration-tests
```

`scaffold-flip-phase` is the foundation — the other three tasks can proceed in parallel after it, with the integration test task depending on all of them.

## Backwards Compatibility

- **FLIP disabled** (default): No change. `.evaluate-<task>` depends on `task-X` directly.
- **FLIP enabled, no threshold**: `.flip-<task>` runs, `.evaluate-<task>` depends on it. No verify created.
- **FLIP enabled + threshold**: Full pipeline: FLIP → (optional verify) → eval with context.
- **Existing evaluations**: Unaffected. Only new tasks published after the change use the new pipeline.
- **Config**: No new config keys needed. Existing `flip_enabled`, `flip_verification_threshold` control behavior.

## Open Questions

1. **Should `.flip-<task>` get its own eval?** Currently `DOMINATED_TAGS` includes `"evaluation"` to prevent eval-of-eval. Adding `"flip"` to `DOMINATED_TAGS` prevents eval-of-flip, which seems correct — FLIP is infrastructure.

2. **FLIP task model**: Should `.flip-<task>` use the FLIP-specific models (`flip_inference_model`, `flip_comparison_model`) or a single model? Current behavior uses separate models for each phase. Since `.flip-<task>` runs `wg evaluate run <task> --flip`, which internally uses the FLIP-specific models, the task's `model` field is informational only.

3. **Verify findings format**: Should verify findings be structured (JSON) or freeform (log entries)? Log entries are simpler and already available. Recommend log entries for v1, structured later if needed.
