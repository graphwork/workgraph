# Research: Model Propagation Within Subgraphs

**Task:** research-model-propagation  
**Date:** 2026-04-13  
**Status:** Complete

## Executive Summary

Workgraph currently supports per-task model selection (`--model`) and a global coordinator model, but has **no mechanism for automatic model inheritance** within subgraphs. When an agent creates subtasks via `wg add --after parent`, the child tasks receive no model unless explicitly set with `--model`. This research investigates five approaches to enabling per-subgraph model propagation, where different branches of the task graph can run simultaneously on different models.

---

## Investigation Area 1: Current State of `--model`

### How `--model` works on tasks

Each task has an optional `model` field (`src/graph.rs:296`):
```rust
pub model: Option<String>,
```

When `wg add --model qwen3-local "Task X"` is run, the model string is stored directly on the task (`src/commands/add.rs:490`):
```rust
model: model.map(String::from),
```

### Does the coordinator respect per-task model?

**Yes.** The spawn system has a clear precedence hierarchy (`src/commands/spawn/execution.rs:1340-1366`):

```
task.model > agent.preferred_model > executor.model > role_model (config) > coordinator_model
```

When a task has `model` set, it takes absolute priority. The coordinator's `spawn_agents_for_ready_tasks` passes a `default_model` which is used only when the task doesn't have its own model. The effective model is then:
- Passed to the executor via `--model` CLI flag
- Set as `WG_MODEL` environment variable on the spawned agent process (`src/commands/spawn/execution.rs:532-533`)

### Do subtasks inherit the parent's model?

**No.** This is the core gap. When an agent creates subtasks via `wg add --after parent-task`:
- `wg add` does NOT read the `WG_MODEL` environment variable
- `wg add` does NOT look up the parent task's model
- The child task's `model` field is `None` unless explicitly set with `--model`
- The child will fall through to the coordinator's default model at spawn time

This means if you create a seed task with `--model qwen3-local`, any subtasks that agent fans out to will default back to the coordinator's model (typically claude-sonnet), not qwen3-local.

---

## Investigation Area 2: Automatic Propagation Mechanisms

### Current state: No propagation exists

There is no code path in `wg add` that reads from parent tasks. The `add::run()` function (`src/commands/add.rs:156`) takes `model: Option<&str>` directly from CLI args. It does not query the graph for parent task properties.

### Approach A: Environment-based propagation (Quick Win)

**Mechanism:** Have `wg add` auto-read `WG_MODEL` from the environment when no explicit `--model` is provided.

**Implementation:** In `src/commands/add.rs`, after line 252 where `resolved_model_str` is computed:
```rust
let resolved_model_str: Option<String> = if let Some(m) = model {
    Some(resolve_model_input(m, dir)?)
} else if let Ok(env_model) = std::env::var("WG_MODEL") {
    // Inherit model from parent agent's environment
    Some(env_model)
} else {
    None
};
```

**Pros:**
- ~5 lines of code change
- Works immediately because spawned agents already have `WG_MODEL` set
- Zero graph schema changes
- Opt-out: agents can override with explicit `--model` on subtasks

**Cons:**
- Implicit — the propagation is invisible in the graph (no field says "inherited from parent")
- Only propagates through the agent execution chain (if a human runs `wg add`, no `WG_MODEL` is set)
- System tasks (`.assign-*`, `.flip-*`, etc.) created by the coordinator would NOT inherit (they're created by the coordinator process, not the agent) — but this is actually desirable since system tasks use their own role-based models

### Approach B: Graph-based propagation via parent lookup

**Mechanism:** When `wg add --after parent-id` is used without `--model`, look up the parent task's model and copy it.

**Implementation:** In `src/commands/add.rs`, inside the `modify_graph` closure (after line 372), look up each parent's model:
```rust
let inherited_model = if model.is_none() {
    effective_after.iter()
        .filter_map(|parent_id| graph.get_task(parent_id))
        .find_map(|parent| parent.model.clone())
} else {
    None
};
// Use inherited_model when building the Task struct
```

**Pros:**
- Works even outside agent context (human `wg add` inherits too)
- Visible in the graph data (task.model is set)
- Deterministic — always inherits from the graph, not from runtime state

**Cons:**
- Ambiguity with multiple parents: which parent's model wins? (first? majority?)
- Surprises users who expect subtasks to use the default model
- No way to opt-out (would need `--model default` or `--no-inherit-model`)
- More invasive — changes behavior of `wg add` for all users

### Approach C: Explicit `--model inherit` flag

**Mechanism:** Add a special sentinel value `inherit` that tells `wg add` to look up the parent's model.

**Implementation:**
```rust
if model == Some("inherit") {
    // Look up parent model from --after deps
    ...
}
```

**Pros:**
- Explicit — no surprise behavior changes
- Works in both agent and human contexts
- Clear signal in task data

**Cons:**
- Requires agents to know to pass `--model inherit`
- More ceremony in task descriptions/prompts

---

## Investigation Area 3: External vs Internal Support

### External approach: Watcher script

A script could poll `wg list --json` and set models on new tasks that lack them:

```bash
while true; do
  for task in $(wg list --status open --json | jq -r '.[] | select(.model == null) | .id'); do
    parent_model=$(wg show $task --json | jq -r '.after[0]' | xargs -I{} wg show {} --json | jq -r '.model // empty')
    if [ -n "$parent_model" ]; then
      wg edit $task --model "$parent_model"
    fi
  done
  sleep 5
done
```

**Pros:**
- Zero code changes
- Can implement any arbitrarily complex propagation logic
- Can be tested independently

**Cons:**
- Race condition: task may be spawned before the watcher sets the model
- Additional process to manage
- Polling latency (5s gap between task creation and model assignment)
- Fragile — depends on `wg show --json` output format

### Internal approach: Native `model: inherit` semantics

The coordinator could implement model inheritance at spawn time rather than at task creation time. In `spawn_agents_for_ready_tasks` (`src/commands/service/coordinator.rs:3302`), when resolving the effective model for a task, it could walk up the dependency chain:

```rust
// In spawn_agents_for_ready_tasks, when task.model is None:
let inherited_model = task.after.iter()
    .filter_map(|dep_id| graph.get_task(dep_id))
    .find_map(|dep| dep.model.clone());
```

**Pros:**
- No race condition (resolved at spawn time)
- No external dependencies
- Works with existing task creation

**Cons:**
- Model isn't visible on the task until after spawning
- Doesn't compose well with the existing `resolve_model_and_provider` cascade (which already has 5 tiers)

**Verdict:** Internal support via Approach A (env-based) is the simplest path. The external watcher has too many race conditions to be reliable.

---

## Investigation Area 4: Coordinator-scoped Subgraphs

### Current coordinator architecture

The coordinator is a single daemon process (`wg service start`) that:
- Polls all ready tasks in the entire graph
- Spawns agents for any ready task up to `max_agents`
- Has a single `model` configuration (plus per-task overrides)

Multiple coordinators exist as numbered IDs (0, 1, 2...) with per-coordinator state (`CoordinatorState` in `src/commands/service/mod.rs:425`), but they all operate on the same graph and are designed for human-coordinator chat sessions, not for subgraph partitioning.

### Could coordinators be scoped to subgraphs?

**Theoretically possible but architecturally misaligned.** The multi-coordinator system is designed for parallel human conversations (each coordinator is a chat partner), not for subgraph isolation. Scoping would require:

1. A filter mechanism: e.g., coordinator-44 only manages tasks with tag `qwen3`
2. Mutual exclusion: tasks must not be double-spawned by two coordinators
3. Discovery: when a new subgraph appears, who creates the coordinator for it?

This is a much larger architectural change and doesn't align with the current coordinator design. It would essentially be building a task scheduler within a task scheduler.

### Alternative: Tag-based model routing

A simpler version of coordinator scoping: use tags to drive model selection without separate coordinators.

```toml
# In config.toml
[model_routing]
"qwen3" = "openrouter:qwen/qwen3-local"
"opus-branch" = "anthropic:claude-opus-4-latest"
```

When spawning, the coordinator checks if a task (or its ancestors) have a tag that maps to a model route. This is conceptually cleaner than coordinator scoping and achieves the same goal.

---

## Investigation Area 5: Edge Cases — Cross-Model Dependencies

### Scenario: Task A (qwen3) depends on Task B (opus)

This works fine today. The model is resolved per-task at spawn time. There's no issue with tasks having different models depending on each other — the dependency system is model-agnostic.

### Scenario: Two subgraphs with different models create tasks that depend on each other

This is the diamond merge problem:
```
seed-qwen3 ──► subtask-a (qwen3) ──┐
                                     ├──► merge-task (???)
seed-opus  ──► subtask-b (opus)  ──┘
```

**What model should `merge-task` use?**

Options:
1. **First parent wins**: Use the model from the first dependency listed in `--after`
2. **No inheritance for multi-parent**: If parents disagree, fall back to coordinator default
3. **Explicit override required**: Error or warn when parents have conflicting models

Option 2 is the safest default — conflicting inheritance should fall back to the global default rather than making an arbitrary choice. This is how most inheritance systems handle diamond problems.

### Scenario: System tasks (`.assign-*`, `.flip-*`) in model subgraphs

System tasks are created by the coordinator, not by agents. They use role-specific model resolution (`DispatchRole::Assigner`, `DispatchRole::Verification`, etc.) which is independent of the task's model. This is correct behavior — evaluation and assignment should use their configured models regardless of what model the task itself uses.

---

## Viable Approaches Compared

| Approach | Complexity | Reliability | Visibility | Breaking Changes |
|----------|-----------|-------------|------------|-----------------|
| **A: Env-based (`WG_MODEL` in `wg add`)** | ~5 LOC | High | Low (implicit) | None |
| **B: Parent lookup in `wg add`** | ~20 LOC | High | High (on task) | Behavior change |
| **C: `--model inherit` sentinel** | ~30 LOC | High | High (explicit) | None |
| **D: Coordinator spawn-time inheritance** | ~15 LOC | High | Low (late) | None |
| **E: Tag-based model routing** | ~80 LOC | High | High | Config schema change |
| **F: Coordinator scoping** | ~500+ LOC | Medium | High | Architectural change |
| **G: External watcher** | 0 LOC (script) | Low (races) | Low | None |

---

## Recommendation

### Simplest path: Approach A (env-based propagation)

**Implement `WG_MODEL` env var reading in `wg add`.** This is the lowest-risk, highest-value change:

1. **5 lines of code** in `src/commands/add.rs`
2. Works immediately because agents already have `WG_MODEL` set
3. No schema changes, no config changes, no breaking behavior
4. Natural opt-out: `wg add --model sonnet "Task"` overrides the inherited model
5. System tasks are unaffected (they don't go through `wg add`)

**Usage pattern:**
```bash
# User creates a seed task with a specific model
wg add "Explore qwen3 capabilities" --model openrouter:qwen/qwen3-235b

# Agent spawned with qwen3 creates subtasks — they auto-inherit qwen3
# (because WG_MODEL=openrouter:qwen/qwen3-235b is in the agent's env)
wg add "Subtask A" --after explore-qwen3-capabilities  # inherits qwen3
wg add "Subtask B" --after explore-qwen3-capabilities  # inherits qwen3

# Meanwhile, another branch uses opus
wg add "Deep analysis" --model opus
# Its subtasks would inherit opus
```

### Recommended follow-up: Approach C (`--model inherit`)

After env-based propagation ships, add `--model inherit` as an explicit mechanism for non-agent contexts (humans creating tasks, scripts). This gives full control without relying on environment state.

### Not recommended (for now): Approaches E and F

Tag-based routing and coordinator scoping are interesting but premature. The env-based approach solves the immediate need with near-zero risk. These can be revisited when the multi-model use case matures.

---

## Quick Win: Convention-Based Tag Propagation

Even without code changes, users can adopt a **convention** today:

1. Tag seed tasks with the model name: `wg add "Task" --model qwen3-local --tag model:qwen3-local`
2. Instruct agents in task descriptions to propagate the tag: "Subtasks should include `--tag model:qwen3-local --model qwen3-local`"
3. Use `wg list --tag model:qwen3-local` to view the subgraph

This is manual and error-prone, but works today with zero code changes. The env-based approach (Approach A) automates exactly this pattern.
