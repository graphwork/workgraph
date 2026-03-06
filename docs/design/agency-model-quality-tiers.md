# Agency Model Quality Tiers: Strong Models for Meta-Layer Work

## Status: Design (March 2026)

## Problem

The agency system has a hierarchy of tasks with different quality requirements:

```
Layer 3 (meta-meta): Evaluating the evolver, evaluating the assigner
Layer 2 (meta):      .evolve-*, .assign-*, .evaluate-* for regular tasks
Layer 1 (work):      Regular implementation/research/design tasks
```

Currently, `DispatchRole::Evaluator` routes ALL evaluations to the same model (typically haiku for cost). But evaluating the evolver's output is fundamentally more important than evaluating a typo fix. If the evolver's evaluation is weak, the evolution signal is weak, and the entire agency degrades.

**Core insight:** The quality of the meta-layer sets the ceiling for everything below it. A cheap model evaluating the evolver produces noisy signal, which produces bad evolution decisions, which degrades all agents. System-level meta-tasks need strong (opus-class) models.

## Current State

### DispatchRole enum (`src/config.rs:393`)

```
Default, TaskAgent, Evaluator, FlipInference, FlipComparison,
Assigner, Evolver, Verification, Triage, Creator
```

### Resolution chain (`src/config.rs:587`)

```
role-specific [models] override
  -> legacy per-role config (agency.evaluator_model, etc.)
    -> tier defaults (triage=haiku, flip_comparison=haiku, flip_inference=sonnet, verification=opus)
      -> [models.default]
        -> agent.model
```

### Where meta-tasks are created

| Meta-task type | Created in | Model resolution | Current default |
|---|---|---|---|
| `.evaluate-*` | `build_auto_evaluate_tasks` (coordinator.rs:1109) | `DispatchRole::Evaluator` | Falls through to `models.default` or `agent.model` |
| `.assign-*` | `build_auto_assign_tasks` (coordinator.rs:718) | `DispatchRole::Assigner` | Falls through to `models.default` or `agent.model` |
| `.evolve-*` | auto-evolver (planned, see auto-evolver-loop.md) | `DispatchRole::Evolver` | Falls through to `models.default` or `agent.model` |

### The gap

There is one `Evaluator` role for all evaluations. The model assigned to `.evaluate-some-task` is the same whether `some-task` is a regular implementation task or `.evolve-20260305`. No distinction exists between evaluating work output vs. evaluating system-level meta-task output.

## Design: Automatic Escalation via Subject-Task Awareness

### Approach: Detect-and-escalate at task creation time

Rather than adding new `DispatchRole` variants (which would proliferate roles and config surface), the system **detects** when an evaluation/assignment targets a meta-task and **escalates** the model at creation time.

**Why this over new dispatch roles:**

| Option | Pros | Cons |
|---|---|---|
| New roles (`SystemEvaluator`, etc.) | Explicit config per role | Role proliferation; each new meta-task type needs a new role; config surface grows |
| Depth-based tiers | Automatically handles arbitrary depth | Over-engineered; real depth is 2-3 levels max |
| **Subject-aware escalation** | Zero new roles; works for any meta-task type; automatic | Slightly more logic at creation point |

The subject-aware approach uses a simple rule: **if the subject task is itself a system task (dot-prefixed), escalate the model.** This is generic because `is_system_task()` already identifies all meta-tasks (`.evaluate-*`, `.assign-*`, `.evolve-*`, and any future system task types).

### Tiering model

Two tiers, determined by what the meta-task operates on:

| Tier | Condition | Model | Rationale |
|---|---|---|---|
| **Standard** | Subject task is a regular (non-system) task | Role default (e.g., haiku for evaluator) | High volume, low stakes per task. Cheap model is fine. |
| **System** | Subject task is a system task (dot-prefixed) | Escalated (opus by default) | Low volume, high stakes. Quality of signal here governs the whole system. |

### What gets escalated

System-level meta-tasks (subject is a dot-prefixed task):

| Meta-task | Subject | Tier | Default model |
|---|---|---|---|
| `.evaluate-.evolve-*` | `.evolve-*` (evolution run) | System | opus |
| `.evaluate-.assign-*` | `.assign-*` (assignment decision) | System | opus |
| `.evaluate-.evaluate-*` | `.evaluate-*` (evaluation quality) | System | opus |
| `.evaluate-.creator-*` | `.creator-*` (agent creation) | System | opus |

Standard-tier meta-tasks (subject is a regular task):

| Meta-task | Subject | Tier | Default model |
|---|---|---|---|
| `.evaluate-implement-foo` | `implement-foo` | Standard | evaluator default (haiku) |
| `.evaluate-design-bar` | `design-bar` | Standard | evaluator default (haiku) |
| `.assign-implement-foo` | `implement-foo` | Standard | assigner default |

Already-routed system agents (no change needed):

| Agent type | DispatchRole | Recommended default |
|---|---|---|
| Evolver itself | `Evolver` | opus (set via `models.evolver.model`) |
| Assigner itself | `Assigner` | sonnet (cost-effective for assignment) |
| Creator | `Creator` | opus (creating good primitives matters) |
| Triage | `Triage` | haiku (lightweight summarization) |

### The escalation mechanism

A new `DispatchRole::SystemEvaluator` variant would be the most discoverable approach, but it only solves the evaluator case. Instead, a single new config field and resolution function handles all system-level meta-tasks:

#### New config field

```toml
# In [models] section of config.toml:
[models.system_evaluator]
model = "opus"
# provider = "anthropic"  # optional
```

#### New DispatchRole variant

```rust
pub enum DispatchRole {
    // ... existing variants ...
    /// Evaluator for system-level (dot-prefixed) tasks — stronger model
    SystemEvaluator,
}
```

This is the cleanest approach: a single new role that the model routing system already knows how to handle. It fits the existing pattern and requires minimal code change.

#### Tier default

In `resolve_model_for_role()`, add a tier default for `SystemEvaluator`:

```rust
let tier_default = match role {
    DispatchRole::Triage => Some("haiku"),
    DispatchRole::FlipComparison => Some("haiku"),
    DispatchRole::FlipInference => Some("sonnet"),
    DispatchRole::Verification => Some("opus"),
    DispatchRole::SystemEvaluator => Some("opus"),  // NEW
    _ => None,
};
```

This means `SystemEvaluator` defaults to opus with zero configuration. Users who want to override it can set `models.system_evaluator.model = "sonnet"`.

#### Resolution at task creation

In `build_auto_evaluate_tasks()` (coordinator.rs:1236), change the model resolution:

```rust
// Current:
model: Some(
    config.resolve_model_for_role(DispatchRole::Evaluator).model,
),

// New:
model: Some({
    let role = if is_system_task(task_id) {
        DispatchRole::SystemEvaluator
    } else {
        DispatchRole::Evaluator
    };
    config.resolve_model_for_role(role).model
}),
```

That's the entire behavioral change: one `if` at the point where eval tasks are created.

## Concrete Code Changes

### 1. Add `SystemEvaluator` to `DispatchRole` (`src/config.rs`)

```rust
// In DispatchRole enum (~line 393):
/// Evaluator for system-level meta-tasks (stronger model)
SystemEvaluator,

// In Display impl (~line 416):
Self::SystemEvaluator => write!(f, "system_evaluator"),

// In FromStr impl (~line 433):
"system_evaluator" => Ok(Self::SystemEvaluator),

// Update the error message to include the new role
```

### 2. Add field to `ModelRoutingConfig` (`src/config.rs`)

```rust
// In ModelRoutingConfig struct (~line 486):
#[serde(default, skip_serializing_if = "Option::is_none")]
pub system_evaluator: Option<RoleModelConfig>,

// In get_role() (~line 520):
DispatchRole::SystemEvaluator => self.system_evaluator.as_ref(),

// In get_role_mut() (~line 538):
DispatchRole::SystemEvaluator => &mut self.system_evaluator,
```

### 3. Add tier default (`src/config.rs`)

```rust
// In resolve_model_for_role() tier_default match (~line 640):
DispatchRole::SystemEvaluator => Some("opus"),
```

### 4. Escalate in `build_auto_evaluate_tasks` (`src/commands/service/coordinator.rs`)

```rust
// Around line 1236, change the model resolution:
let eval_role = if is_system_task(task_id) {
    workgraph::config::DispatchRole::SystemEvaluator
} else {
    workgraph::config::DispatchRole::Evaluator
};

// ... in the Task struct:
model: Some(config.resolve_model_for_role(eval_role).model),
```

### 5. Add `--system-evaluator-model` to CLI (`src/cli.rs`)

Wire `wg config --system-evaluator-model opus` to set `models.system_evaluator.model`.

### 6. No changes to `resolve_model_for_role()` resolution chain

The existing resolution chain already handles the new role correctly:
1. `models.system_evaluator.model` (explicit override)
2. No legacy config (new role, no backward compat needed)
3. Tier default: `opus`
4. Falls through to `models.default` then `agent.model` only if tier default is removed

## Default Model Assignments Per Tier

### Recommended defaults (all configurable via `wg config`)

| Role | Default Model | Cost tier | Rationale |
|---|---|---|---|
| **System tier** | | | |
| `SystemEvaluator` | opus | high | Quality of system evaluation governs evolution quality |
| `Evolver` | opus | high | Evolution decisions are high-leverage |
| `Creator` | opus | high | New primitives need to be well-designed |
| `Verification` | opus | high | FLIP verification catches critical errors |
| **Standard tier** | | | |
| `Evaluator` | haiku | low | High volume, individual evaluation noise is averaged out |
| `Assigner` | sonnet | medium | Assignment quality matters but volume is moderate |
| `TaskAgent` | (config default) | varies | Per-task routing via agent model |
| **Utility tier** | | | |
| `Triage` | haiku | low | Simple summarization |
| `FlipComparison` | haiku | low | Mechanical comparison |
| `FlipInference` | sonnet | medium | Reconstruction requires reasoning |

## Cost Analysis

### How many system evaluations happen?

System-level evaluations are rare compared to work evaluations:

| Event | Frequency | Evaluations generated |
|---|---|---|
| Regular task completion | ~5-20/day | 1 `.evaluate-*` each (standard tier) |
| Evolution cycle | ~1-3/day (every 10 evals) | 1 `.evaluate-.evolve-*` (system tier) |
| Assignment | ~5-20/day | 0 (assignments are not currently eval-scheduled) |
| Agent creation | Rare (~1/week) | 1 `.evaluate-.creator-*` (system tier) |

### Cost per 100 tasks

Assuming 100 regular tasks completing in a workday:

| Component | Count | Model | Cost each | Total |
|---|---|---|---|---|
| Standard evaluations | 100 | haiku | ~$0.01 | ~$1.00 |
| Evolution cycles | ~10 | opus (evolver) | ~$0.20 | ~$2.00 |
| System evaluations | ~10 | opus (system_evaluator) | ~$0.15 | ~$1.50 |
| **Total evaluation cost** | | | | **~$4.50** |

**Without this design** (all evaluations on haiku):

| Component | Count | Model | Cost each | Total |
|---|---|---|---|---|
| All evaluations | ~110 | haiku | ~$0.01 | ~$1.10 |
| Evolution cycles | ~10 | opus | ~$0.20 | ~$2.00 |
| **Total** | | | | **~$3.10** |

**Delta: ~$1.40/day for 100 tasks** (~$0.014 per task). This is a ~45% increase in evaluation cost, but evaluation is a small fraction of total spend. The total system spend for 100 tasks (including agent execution) is typically ~$50-200, making the evaluation cost increase <1% of total spend.

The quality improvement is high-leverage: a single bad evolution decision (from weak evaluation signal) can degrade all subsequent agents, costing far more than $1.40 in wasted agent runs.

## Configuration Surface

### Automatic behavior (zero config)

Out of the box with this change:
- `.evaluate-<system-task>` uses opus (via `SystemEvaluator` tier default)
- `.evaluate-<regular-task>` uses whatever `Evaluator` resolves to
- No config needed

### Explicit overrides

```bash
# Override the system evaluator model
wg config --system-evaluator-model sonnet

# Override per-provider
wg config --system-evaluator-model anthropic/claude-sonnet-4-6

# Override the standard evaluator model (existing)
wg config --evaluator-model haiku

# Override the evolver model (existing)
wg config --evolver-model opus
```

### Config file

```toml
[models]
# System-level evaluations get opus
[models.system_evaluator]
model = "opus"

# Standard evaluations stay cheap
[models.evaluator]
model = "haiku"

# Evolver gets opus
[models.evolver]
model = "opus"
```

## Integration with Auto-Evolver Design

The auto-evolver design (`docs/design/auto-evolver-loop.md`) specifies:

1. `.evolve-*` tasks are created as meta-tasks with `tags: [agency, evolution, eval-scheduled]`
2. `.evolve-*` uses `DispatchRole::Evolver` for model routing (already supports opus)
3. `.evolve-*` tasks are tagged `eval-scheduled`, meaning they get auto-evaluated

**This design layers on top of the auto-evolver design without modifications:**

- When `.evolve-*` completes and gets auto-evaluated, `build_auto_evaluate_tasks` creates `.evaluate-.evolve-*`
- The `is_system_task(".evolve-*")` check returns `true` (dot-prefixed)
- The evaluation task gets `DispatchRole::SystemEvaluator` -> opus
- No changes to the auto-evolver design are needed

The auto-evolver design should note in its "Execution" section that the evaluator for `.evolve-*` tasks uses the `SystemEvaluator` role (this design), ensuring strong evaluation signal feeds back into the evolution loop.

## Genericity: Future Meta-Task Types

The mechanism is generic because it relies on `is_system_task()` (dot-prefix check), not on enumerating specific task types. If new system task types are added:

- `.verify-*` tasks -> their evaluations auto-escalate
- `.review-*` tasks -> their evaluations auto-escalate
- `.compact-*` tasks -> their evaluations auto-escalate

No code changes needed for new system task patterns. The only assumption is that system tasks use the dot-prefix convention, which is already enforced throughout the codebase.

## Open Questions

1. **Should `.assign-*` tasks also be eval-scheduled?** Currently they're created as Done immediately (inline LLM call). If assignment quality matters enough to evaluate, the evaluations of `.assign-*` would automatically get system-tier models via this design. Decision: defer to a separate task.

2. **Should the system evaluator use a different evaluator agent identity?** Currently all evaluations use the same `evaluator_agent` identity. A system-tier evaluator might benefit from a different prompt (e.g., "You are evaluating a system-level meta-task..."). Decision: the task description already contains the subject task context; the evaluator prompt renderer could include a note about meta-task evaluation. This is a small enhancement, not a design-level decision.

3. **Should there be a `SystemAssigner` role?** For assigning agents to system tasks. Currently system tasks skip assignment entirely (`is_system_task` check in `build_auto_assign_tasks`). If system tasks ever need assignment, this pattern extends naturally. Decision: not needed now.

## Summary

| Decision | Choice | Rationale |
|---|---|---|
| Mechanism | New `SystemEvaluator` dispatch role | Fits existing pattern, minimal code, discoverable |
| Detection | `is_system_task(subject_task_id)` at eval creation | Generic, works for all system task types |
| Default model | opus | System evaluations are low-volume, high-leverage |
| Cost impact | ~$1.40/day per 100 tasks (~1% of total) | Justified by quality improvement |
| Config surface | `wg config --system-evaluator-model` | Follows existing pattern |
| Auto-evolver interaction | Layers on top, no modifications needed | `.evolve-*` evaluations auto-escalate |

Total code changes: ~30 lines across 3 files (`config.rs`, `coordinator.rs`, `cli.rs`).
