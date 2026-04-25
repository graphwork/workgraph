# Agent-Model Binding Design

**Task:** or-research-agent-model  
**Date:** 2026-03-13  
**Status:** Proposal

## Problem

The agency system's `Agent` struct has an `executor` field (e.g., `"claude"`, `"amplifier"`, `"shell"`) but no model preference. When the coordinator spawns an agent for a task, the model is resolved entirely from the config/task side — the agent identity has no voice in model selection. This means:

1. An agent proven to perform well with Opus cannot express that preference
2. Cost-sensitive agents (e.g., a "Triage" agent) cannot default to Haiku
3. OpenRouter agents cannot bind a preferred provider+model without task-level overrides

## Current State

### Agent Struct (`src/agency/types.rs:288-319`)

```rust
pub struct Agent {
    pub id: String,           // SHA-256(role_id + tradeoff_id)
    pub role_id: String,
    pub tradeoff_id: String,
    pub name: String,
    pub performance: PerformanceRecord,
    pub lineage: Lineage,
    pub capabilities: Vec<String>,
    pub rate: Option<f64>,
    pub capacity: Option<f64>,
    pub trust_level: TrustLevel,
    pub contact: Option<String>,
    pub executor: String,     // ← executor exists
    // NO model field          // ← gap
    pub deployment_history: Vec<DeploymentRef>,
    pub attractor_weight: f64,
    pub staleness_flags: Vec<StalenessFlag>,
}
```

### Current Model Resolution Chain

The model used when spawning a task agent is resolved in two stages:

**Stage 1: Coordinator tick** (`coordinator.rs:2841-2846`)
```
CLI --model > config.resolve_model_for_role(TaskAgent)
```

**Stage 2: spawn_agent_inner** (`execution.rs:133-137`)
```
task.model > executor_config.model > model_param (from stage 1)
```

**`resolve_model_for_role`** (`config.rs:1051-1175`) has its own 6-step cascade:
1. `models.<role>.model` — role-specific override in `[models]` section
2. Legacy per-role config (e.g., `agency.evaluator_model`)
3. `models.<role>.tier` — tier override → registry lookup
4. Role `default_tier()` → `tiers.<tier>` → registry lookup
5. `models.default.model` — default in `[models]` section
6. `agent.model` — global config fallback (`config.agent.model`)

**Combined effective chain (highest to lowest priority):**
```
task.model > executor_config.model > CLI --model > models.task_agent.model >
legacy config > models.task_agent.tier > role.default_tier > models.default.model >
config.agent.model
```

### How `wg assign` Works (`src/commands/assign.rs`)

`wg assign <task-id> <agent-hash>` sets `task.agent = Some(agent.id)`. At spawn time (`coordinator.rs:2449-2453`), the coordinator reads the agent hash to resolve the **executor**:

```rust
let effective_executor = task.agent
    .and_then(|hash| find_agent_by_prefix(&agents_dir, hash))
    .map(|agent| agent.executor)          // ← uses agent.executor
    .unwrap_or_else(|| executor.to_string());
```

But the model is resolved entirely from config — the agent entity is ignored for model selection.

## Proposed Design

### 1. Add `preferred_model` and `preferred_provider` to Agent

```rust
pub struct Agent {
    // ... existing fields ...
    pub executor: String,

    /// Preferred model for this agent (e.g., "opus", "sonnet", "haiku",
    /// or a full model ID like "claude-opus-4-latest").
    /// Used as a fallback when no task-level or role-level model is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,

    /// Preferred provider for this agent (e.g., "anthropic", "openrouter").
    /// Used alongside preferred_model for provider routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_provider: Option<String>,

    // ... rest of existing fields ...
}
```

**Why `Option<String>`:** Most agents won't need a model preference — they'll use whatever the config/tier system resolves. Making it optional keeps the common case zero-cost and preserves backward compatibility.

**Why not `preferred_tier`:** A tier (fast/standard/premium) is tempting but too coarse. An agent that works well with `claude-opus-4-latest` specifically shouldn't be silently downgraded when the tier mapping changes. Direct model IDs or registry short-names give precision. Tier-based preferences can be expressed via the existing tier config if needed.

### 2. Updated Precedence Chain

The agent's preference slots in **below task-level overrides but above config-level defaults**:

```
task.model                          # Highest: explicit per-task override
  > executor_config.model           # Executor-level default
    > CLI --model                   # Coordinator CLI override
      > models.task_agent.model     # Config role-specific model
        > agent.preferred_model     # ← NEW: agent identity preference
          > legacy per-role config  # Backward compat
            > tier resolution       # Tier system
              > models.default      # Config default
                > config.agent.model  # Global fallback (lowest)
```

**Rationale for this position:**

- **Below task.model:** A task that says `--model haiku` is an explicit human decision — the agent shouldn't override it. This respects the "task is the contract" principle.
- **Below CLI --model / models.task_agent:** These are system-wide policy decisions by the project owner. Agent preferences are per-identity suggestions, not mandates.
- **Above tier/default resolution:** When no explicit override exists, the agent's proven preference should win over generic tier defaults. This is the whole point — let performance-data-informed preferences bubble up.

### 3. Implementation: Spawn Path Change

In `spawn_agents_for_ready_tasks` (`coordinator.rs:2440-2472`), after resolving executor from the agent, also resolve the agent's model preference:

```rust
// Resolve executor: agent.executor > config.coordinator.executor
let effective_executor = /* ... existing logic ... */;

// Resolve agent model preference (NEW)
let agent_preferred_model = task.agent
    .as_ref()
    .and_then(|hash| agency::find_agent_by_prefix(&agents_dir, hash).ok())
    .and_then(|agent| agent.preferred_model.clone());
```

In `spawn_agent_inner` (`execution.rs:133-137`), insert the agent preference:

```rust
// Model resolution hierarchy:
//   task.model > executor.model > model_param > agent.preferred_model
let effective_model = task_model
    .or_else(|| executor_config.executor.model.clone())
    .or_else(|| model.map(ToString::to_string))
    .or_else(|| agent_preferred_model);  // ← NEW
```

This requires threading `agent_preferred_model` through to `spawn_agent_inner`. Options:
- **(A) Add parameter** to `spawn_agent_inner`: `agent_preferred_model: Option<String>`
- **(B) Pass the full Agent** and let `spawn_agent_inner` extract both executor and model
- **(C) Resolve in coordinator** and pass as the `model` param (conflates with CLI --model)

**Recommended: Option (A).** It's minimal, explicit, and doesn't change the existing `model` parameter semantics. Option (B) would require loading the agent twice (once in coordinator for executor, once in spawn). Option (C) would make it impossible to distinguish "coordinator resolved this from config" vs "agent prefers this."

### 4. Provider Resolution

Similarly, `agent.preferred_provider` should slot into the provider cascade. Currently provider resolution happens in two places:

- `resolve_model_for_role` (for config-level resolution)
- `spawn_agent_inner:200-208` (for native executor)

The agent's provider preference should be consulted when the task has no explicit provider:

```rust
let effective_provider = if settings.executor_type == "native" {
    task_provider
        .or_else(|| agent_preferred_provider.clone())  // ← NEW
        .or_else(|| resolved_task_agent.provider.clone())
} else {
    None
};
```

### 5. CLI Changes

#### `wg agent create`

Add `--model` and `--provider` flags:

```bash
wg agent create "Fast Reviewer" --role <hash> --tradeoff <hash> \
    --model haiku --provider anthropic
```

#### `wg agent update` (new or existing)

Allow updating model preference on existing agents:

```bash
wg agent update <hash> --model opus --provider openrouter
```

#### `wg show` / `wg agents`

Display preferred model when set:

```
Agent: Fast Reviewer (3ede50bb)
  Role:       Reviewer (a1b2c3d4)
  Tradeoff:   Thorough (e5f6g7h8)
  Executor:   claude
  Model:      haiku (preferred)       ← NEW
  Provider:   anthropic (preferred)   ← NEW
  Tasks: 12 | Avg: 0.87
```

### 6. Role-Level Model Defaults

The `Role` struct already has `default_exec_mode`. Consider adding `default_model`:

```rust
pub struct Role {
    // ... existing fields ...
    pub default_exec_mode: Option<String>,

    /// Default model preference for agents with this role.
    /// Agent.preferred_model overrides this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}
```

This allows role-based model routing without per-agent configuration. The precedence within the "agent preference" band becomes:

```
agent.preferred_model > role.default_model
```

**This is optional for the initial implementation** — it can be added later without breaking changes.

### 7. Interaction with Run-Mode Assignment

The assigner (LLM-based agent selection in `run_mode.rs`) currently selects agents based on role fit, tradeoff alignment, and performance history. With `preferred_model`, the assigner gains a new signal:

- When the task has no explicit model, prefer agents whose `preferred_model` matches the task's inferred complexity tier
- When cost is a concern (e.g., triage tasks), prefer agents with `preferred_model = "haiku"`
- Include `preferred_model` in the assigner prompt context so the LLM can reason about model fit

No structural changes needed — just include the field in the context rendered for the assigner.

## Migration Strategy

### Backward Compatibility

1. **`preferred_model` and `preferred_provider` are `Option<String>` with `#[serde(default)]`** — existing YAML files without these fields deserialize cleanly as `None`
2. **No change to agent ID computation** — `id = SHA-256(role_id + tradeoff_id)` is unchanged. Model preference is operational metadata, not identity
3. **No change to existing precedence** when `preferred_model` is `None` — the chain falls through to the same config resolution as today
4. **`skip_serializing_if = "Option::is_none"`** — existing agent YAML files won't grow until a preference is set

### Migration Steps

1. Add fields to `Agent` struct with serde defaults — **zero migration needed**, all existing agents load as-is
2. Update `spawn_agent_inner` signature — add `agent_preferred_model: Option<String>` parameter
3. Thread the preference through `spawn_agents_for_ready_tasks` → `spawn_agent_inner`
4. Add `--model` / `--provider` flags to `wg agent create` and display in `wg show`
5. (Optional) Add `default_model` to `Role` struct

### Rollback

If issues arise, setting `preferred_model: None` on affected agents restores previous behavior. No data loss, no schema incompatibility.

## Usage Examples

### Example 1: Cost-Optimized Triage Agent

```bash
# Create an agent that defaults to haiku for fast, cheap triage
wg agent create "Fast Triager" --role <triage-role> --tradeoff <speed-first> \
    --model haiku

# Assign to triage tasks — will use haiku unless task.model overrides
wg assign .triage-task-1 <fast-triager-hash>
```

### Example 2: Premium Research Agent

```bash
# Research tasks benefit from opus-level reasoning
wg agent create "Deep Researcher" --role <researcher> --tradeoff <thorough> \
    --model opus --provider openrouter

# When assigned, uses opus via openrouter unless task says otherwise
wg assign research-api-design <deep-researcher-hash>
```

### Example 3: Override Chain in Action

```bash
# Agent prefers sonnet
wg agent create "Implementer" --role <impl> --tradeoff <balanced> --model sonnet

# Task explicitly requests opus — task.model wins
wg add "Complex refactor" --model opus
wg assign complex-refactor <implementer-hash>
# → spawns with opus (task.model > agent.preferred_model)

# Task has no model preference — agent.preferred_model wins
wg add "Simple bugfix"
wg assign simple-bugfix <implementer-hash>
# → spawns with sonnet (agent.preferred_model > tier defaults)
```

### Example 4: Config-Level Policy Still Wins

```toml
# config.toml — project policy: all task agents use sonnet
[models]
task_agent = { model = "sonnet" }
```

```bash
# Agent prefers opus, but config policy says sonnet
wg agent create "Eager Architect" --role <arch> --tradeoff <thorough> --model opus
wg assign design-task <eager-architect-hash>
# → spawns with sonnet (models.task_agent.model > agent.preferred_model)
```

## Summary

| Change | Files | Breaking? |
|--------|-------|-----------|
| Add `preferred_model`, `preferred_provider` to `Agent` | `src/agency/types.rs` | No (Option + serde default) |
| Thread agent model through spawn | `src/commands/spawn/execution.rs`, `src/commands/service/coordinator.rs` | No (new optional param) |
| Add CLI flags | `src/commands/agent.rs`, `src/cli.rs` | No (new flags) |
| Display in `wg show` / `wg agents` | `src/commands/show.rs`, `src/commands/agents.rs` | No (additive) |
| (Optional) `Role.default_model` | `src/agency/types.rs` | No (Option + serde default) |

**All changes are additive. Zero migration required. Full backward compatibility.**
