# Agency: How It Currently Works

*Audit date: 2026-02-27*

This document describes the actual running state of the Agency system in workgraph, then compares it section-by-section against the specification at `~/agency/specification.md`.

---

## 1. Data Model

### Primitive Store (ground truth)

Three types of primitives, each stored as independent content-addressed YAML files:

| Primitive | Directory | Hash inputs | Key fields |
|-----------|-----------|-------------|------------|
| **RoleComponent** | `primitives/components/{hash}.yaml` | description, category, content | name, category (Translated/Enhanced/Novel), content (ContentRef: Name\|File\|Url\|Inline), performance, lineage, access_control |
| **DesiredOutcome** | `primitives/outcomes/{hash}.yaml` | description, success_criteria | name, success_criteria[], requires_human_oversight, performance, lineage, access_control |
| **TradeoffConfig** | `primitives/tradeoffs/{hash}.yaml` | description, acceptable/unacceptable tradeoffs | name, acceptable_tradeoffs[], unacceptable_tradeoffs[], performance, lineage, access_control |

All hashing uses SHA-256 on YAML serialization of content fields (not the ID itself). IDs are deterministic — identical content always produces the same hash.

### Composition Cache (derived, not ground truth)

| Entity | Directory | Composition |
|--------|-----------|-------------|
| **Role** | `cache/roles/{hash}.yaml` | Sorted component_ids[] + outcome_id |
| **Agent** | `cache/agents/{hash}.yaml` | role_id + tradeoff_id |

The Agent struct also carries: `attractor_weight` (0.0–1.0, how "conventional" this combination is), `staleness_flags`, `deployment_history`, `trust_level`, `executor`, and `capacity`.

### Shared Metadata on All Entities

- **PerformanceRecord**: task_count, avg_score, evaluations (Vec\<EvaluationRef\>)
- **Lineage**: parent_ids, generation, created_by, created_at
- **AccessControl**: owner (default "local"), policy (Private/Shared/Open)

### Source files

- `src/agency/types.rs` (582 lines) — all struct definitions
- `src/agency/hash.rs` (113 lines) — five content-hash functions
- `src/agency/store.rs` (258 lines) — LocalStore persistence (YAML/JSON)

### Comparison to spec

**Aligned.** The spec's central requirement — "the primitive store is the ground truth; the composition cache is a performance optimisation" — is structurally met. The three primitive types match exactly. The composition cache sits above them. Content-addressing via SHA-256 matches. The spec's `[Challenge to current implementation]` about pre-composed Identities conflating primitive store with composition cache has been resolved.

---

## 2. Bootstrap: What `wg agency init` Creates

Running `wg agency init` seeds (idempotently via `seed_starters()`):

**8 role components**: code-writing, testing, debugging, code-review, security-audit, technical-writing, system-design, dependency-analysis. All category: Translated, content: Name (resolved via skill system at prompt time).

**4 desired outcomes**: Working tested code, Review report with findings, Clear documentation, Design document with rationale.

**4 starter roles** (composed from the above):
- Programmer = [code-writing, testing, debugging] → "Working, tested code"
- Reviewer = [code-review, security-audit] → "Review report with findings"
- Documenter = [technical-writing] → "Clear documentation"
- Architect = [system-design, dependency-analysis] → "Design document with rationale"

**11 tradeoff configs**: 4 for regular work (Careful, Fast, Thorough, Balanced), plus special-agent tradeoffs for assigner, evaluator, evolver, and creator (4 proximity variants).

**1 default agent**: Careful Programmer (Programmer role + Careful tradeoff).

**Special agent component definitions** (seeded as primitives but NOT auto-composed into agents):
- Assigner: 7 components (task-to-component-matching, historical-performance-weighting, composition-cache-search, primitive-first-composition, task-clarification, etc.)
- Evaluator: 6 components (cardinal-scale-grading, rubric-interpretation, underspecification-detection, etc.)
- Evolver: 8 components (wording-mutation, component-substitution, randomisation, bizarre-ideation, gap-analysis, retirement-identification, etc.)
- Agent Creator: 6 components (research-literature-search, analogous-domain-identification, absorptive-capacity-assessment, federation-import, etc.)

### Comparison to spec

**Aligned on structure, gap on integration.** The spec describes four special agent types as "just agents with specific role components." The component definitions exist as seeded primitives, but the coordinator does not compose them into agents automatically, and prompt construction for special agents uses hardcoded templates rather than rendering from their role components. This is the Stage 2 → Stage 3 bootstrap gap.

---

## 3. Assignment Flow

### How a task gets assigned

When `config.agency.auto_assign = true` (set by `wg agency init`), the coordinator's tick loop:

1. Finds ready tasks without an `agent` field
2. Calls `determine_assignment_path()` which routes probabilistically:
   - **ForcedExploration** if `task_count % exploration_interval == 0` (every 20 tasks)
   - **Learning** if `random() < max(run_mode, min_exploration_rate)` (default: 20% chance)
   - **Performance** otherwise (default: ~80% of tasks)
3. Creates a blocking `assign-{task_id}` task with:
   - Compact description (task ID, title, truncated description, available agents, run mode context)
   - `exec: "wg assign --auto {task_id}"` (triggers inline lightweight path)
   - `tags: ["assignment"]`
4. Original task gets `after: [assign-{task_id}]` dependency

### The assignment itself

When `assign-{task_id}` executes via the inline path (`spawn_eval_inline` in coordinator.rs):

1. Runs `wg assign --auto {task_id}` as a direct bash subprocess (~5-10k tokens)
2. The auto-assign logic:
   - **Performance mode**: `find_cached_agent()` returns highest-scored agent above `performance_threshold` (0.7) with no staleness flags
   - **Learning mode**: `design_experiment()` uses UCB1 to select a primitive to test, then composes a novel agent configuration
3. Records `TaskAssignmentRecord` in `assignments/{task_id}.yaml` with mode (CacheHit/CacheMiss/Learning/ForcedExploration) and experiment metadata
4. Updates the original task's `agent` field with the selected agent hash
5. Marks `assign-{task_id}` done

### UCB1 Primitive Selection (learning mode)

Score = `avg_score + C × sqrt(ln(N) / n_i) × novelty_factor`

Where:
- C = `ucb_exploration_constant` (default √2)
- N = total assignments, n_i = this primitive's assignments
- novelty_factor = `novelty_bonus_multiplier` (1.5) if attractor_weight < 0.5, else 1.0
- Unscored primitives get optimistic prior of 0.5

### Comparison to spec

**Partially aligned.** The spec describes the assigner as "an agent type whose role components include: matching task descriptions to agent configurations, evaluating closeness of fit..." The actual implementation does match this conceptually — it matches tasks to agents, weighs historical performance, and searches both cache and primitives. However:

- The assigner runs as a hardcoded prompt template, not as an agent composed from its role components
- Task clarification (spec: "when an assigner receives an underspecified task, it should request clarification") is defined as a component but not wired in
- The UCB1 selection + run mode continuum + experiment design are **more sophisticated than what the spec describes** — the spec outlines the concept of performance/learning modes, while the implementation has a full statistical framework

---

## 4. Evaluation Flow

### Auto-evaluate trigger

When `config.agency.auto_evaluate = true` and coordinator finds a completed task:

1. Creates blocking `evaluate-{task_id}` task
2. Renders evaluator prompt with: agent identity, task description, task output, artifacts diff, any explicit rubric
3. Calls evaluator agent (currently: hardcoded template via claude LLM)
4. Produces score (0.0–1.0) + rationale + optional dimension scores

### 6-step evaluation cascade (`record_evaluation` in eval.rs)

Score propagates through the entire hierarchy:

1. Save `Evaluation` JSON → `evaluations/eval-{task_id}-{timestamp}.json`
2. Update **Agent** performance (EvaluationRef with context_id=role_id)
3. Update **Role** performance (context_id=tradeoff_id)
4. Update **TradeoffConfig** performance (context_id=role_id)
5. Propagate to each **RoleComponent** in the role (context_id=role_id)
6. Propagate to the role's **DesiredOutcome** (context_id=agent_id)

Every entity's `avg_score` is recalculated after each evaluation.

### Retrospective inference (learning mode)

After evaluation for a learning/exploration assignment, `process_retrospective_inference()`:

1. Loads the `TaskAssignmentRecord` to find the experiment
2. Updates attractor weights on the base composition:
   - Score > base avg → weight += 0.1 (becoming more conventional)
   - Score < base avg → weight -= 0.1 (becoming less conventional)
3. Populates composition cache if score ≥ `cache_population_threshold` (0.8)

### Organizational-level concerns (folded into task evaluation)

The separate `OrgEvaluation` type has been removed. Organizational concerns are now handled as dimensions within the standard evaluator prompt:

| Dimension | Weight | Measures |
|-----------|--------|----------|
| downstream_usability | 15% | How useful output is to downstream tasks |
| coordination_overhead | 10% | Multi-agent overhead imposed |
| blocking_impact | 5% | Impact on blocking other agents |

These are grouped as "Organizational Impact" (30% total weight) alongside "Individual Quality" (70%) in the evaluator rubric. When a task has downstream consumers, their context is rendered in the evaluator prompt automatically. No separate evaluation pass or storage is needed — org-level scoring flows through the standard 6-step evaluation cascade.

### Comparison to spec

**Mostly aligned.** The 6-step cascade matches the spec's requirement that "primitives can inherit evaluations from the agents they were elements of." The two-level reward signal (task-level + org-level) is now unified: org-level concerns (downstream_usability, coordination_overhead, blocking_impact) are integrated as weighted dimensions in the evaluator prompt rather than requiring a separate evaluation type. This is simpler than the spec's implied separate pass and achieves the same goal — org-level signal propagates through the standard cascade. Key gaps:

- The evaluator runs as a hardcoded prompt template, not as an agent composed from its role components
- The rubric specification spectrum (explicit → named → domain → natural language → none) from the spec is not formalized — the evaluator handles whatever rubric text is present but doesn't categorize the level of specification
- Proper scoring rules for evaluator incentives — agent implementation was lost in concurrent overwrites, needs re-implementation

---

## 5. Evolution

### Strategy types (9)

| Strategy | Description |
|----------|-------------|
| Mutation | Vary wording while preserving meaning |
| ComponentMutation | Swap one role component for a similar one |
| Crossover | Blend attributes from two parent primitives |
| Randomisation | Unconstrained recombination from existing pool |
| BizarreIdeation | Generate entirely novel primitives |
| GapAnalysis | Identify structural gaps in primitive pool |
| Retirement | Retire underperforming primitives |
| TradeoffTuning | Tune tradeoff configurations (the "GEPA" concept) |
| All | Apply any of above as appropriate |

### Level × Amount targeting

**Levels**: Primitives, Configurations, Agents, AgentConfigurations
**Amounts**: Minimal (single trait), Moderate (multiple related), Maximal (fundamental restructuring)

### Deferred operations (human oversight gate)

Operations that touch DesiredOutcomes with `requires_human_oversight=true` or use BizarreIdeation on outcomes are placed in `deferred/{id}.json` for human approval. This implements the spec's requirement that "objectives — what is worth pursuing — are meaningmaking decisions that must not be evolved without human oversight."

### How it runs

`wg evolve` invoked manually or scheduled. Loads performance data, renders evolver prompt with strategy instructions and skill documents, calls evolver agent (claude LLM), parses JSON output, applies operations (create/modify/retire roles/tradeoffs, mutations), persists results, queues deferred operations.

### Comparison to spec

**Well aligned.** The spec's three mutation classes (mutation, randomisation, bizarre ideation) are implemented, plus additional strategies (crossover, gap analysis, retirement) that extend beyond the spec. The Level × Amount targeting matches the spec's evolver desired outcomes along two independent dimensions. The deferred queue implements the spec's objective/trade-off configuration distinction — automatic evolution is constrained to trade-off configurations, while objective changes require human oversight.

**Gap**: The evolver runs as a hardcoded prompt template, not as an agent composed from its role components. ~~`wg evolve review` (for approving deferred operations) is not yet a CLI command.~~ **Now implemented** as `wg evolve review {list|approve|reject}`.

**New since audit**: Meta-agent evolution implemented — `meta_swap_role`, `meta_swap_tradeoff`, `meta_compose_agent` ops allow evolving assigner/evaluator/evolver configurations. Evolver self-mutation automatically deferred for human review.

---

## 6. Run Mode Continuum

### Configuration parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| run_mode | 0.2 | 0.0=pure performance, 1.0=pure learning |
| min_exploration_rate | 0.05 | Floor on learning probability |
| exploration_interval | 20 | Force learning every N tasks |
| cache_population_threshold | 0.8 | Score needed to promote to cache |
| ucb_exploration_constant | √2 | UCB1 exploration-exploitation balance |
| novelty_bonus_multiplier | 1.5 | Boost for low-attractor primitives |
| bizarre_ideation_interval | 10 | Force bizarre ideation every N learning assignments |
| performance_threshold | 0.7 | Minimum score for cache deployment |

### Routing logic

```
if task_count % exploration_interval == 0:  → ForcedExploration
elif random() < max(run_mode, min_exploration_rate):  → Learning
else:  → Performance
```

### Comparison to spec

**Exceeds spec.** The spec describes a conceptual continuum between "performance mode" and "learning mode." The implementation provides a full statistical framework with UCB1 selection, attractor weight dynamics, experiment design, retrospective inference, and cache population. This is more sophisticated than what the spec requires. The spec's concern about "exploitation drift" (March, 1991) is directly addressed via `min_exploration_rate`, `exploration_interval`, and `bizarre_ideation_interval`.

---

## 7. Agent Creator

### Current state

Role component definitions exist as seeded primitives (6 components: research-literature-search, analogous-domain-identification, structural-similarity-recognition, absorptive-capacity-assessment, federation-import, primitive-candidate-specification).

A `creator_pipeline_function()` exists as a design template (creator → evolver → assigner workflow).

Config keys exist: `creator_agent`, `creator_model`, `auto_create`, `auto_create_threshold`.

### What's wired

- `src/commands/agency_create.rs` — creator logic, wired into `mod.rs` and `main.rs`
- `auto_create` config flag in coordinator — when enabled, coordinator invokes the creator agent when the primitive store needs expansion (threshold: `auto_create_threshold`, default 20 completed tasks)
- `wg agency create` CLI command available (supports `--model` and `--dry-run`)
- `wg config --auto-create true` CLI toggle for enabling/disabling
- Creator model configurable via `wg config --set-model creator <model>` (default: opus, premium tier)
- `auto_create` displayed in `wg config --show` output

### Comparison to spec

**Wired and functional.** The spec describes the agent creator as forming a pipeline with the evolver and assigner: "create new primitives → evolve and test configurations → assign to tasks." The creator module is now wired into the CLI and coordinator loop. The `auto_create` toggle and threshold control automated invocation. The creator agent is not yet automatically composed from its role components during bootstrap (still uses a hardcoded prompt template), but it is operationally accessible.

---

## 8. Special Agent Summary

| Agent Type | Components Defined | Agent Composed | Prompt Source | Auto-Triggered | Can Be Evolved |
|------------|-------------------|----------------|---------------|----------------|----------------|
| Assigner | 7 ✓ | ✗ | Hardcoded template | ✓ (via coordinator) | ✓ (meta_swap_role/tradeoff) |
| Evaluator | 6 ✓ | ✗ | Hardcoded template | ✓ (via coordinator) | ✓ (meta_swap_role/tradeoff) |
| Evolver | 8 ✓ | ✗ | Hardcoded template | ✗ (manual `wg evolve`) | ✓ (deferred for review) |
| Creator | 6 ✓ | ✗ | Module wired, hardcoded template | ✓ (via coordinator, `auto_create`) | ✓ (meta_swap_role/tradeoff) |

The spec says: "None are privileged system components. All accumulate performance history, can be evolved, and are subject to the same selection pressure as any other agent."

**Current reality**: All four are still partially privileged. They run via hardcoded prompt templates and do not accumulate individual performance history. However, **evolution of meta-agents is now implemented** — `meta_swap_role`, `meta_swap_tradeoff`, and `meta_compose_agent` operations can target assigner/evaluator/evolver slots. Evolver self-mutation is automatically deferred for human review via `wg evolve review`.

This is the Stage 2 → Stage 3 bootstrap gap. Stage 3 would mean: compose special agents from their role components, render prompts from those components, record evaluations against them, and evolve them like any other agent. Evolution targeting is done; composition and evaluation recording are not.

---

## 9. Federation

### What exists

- `AccessControl` metadata on all primitives (owner, policy: Private/Shared/Open)
- `federation.rs` (1,783 lines) with pull/push/merge/scan operations
- Named remotes stored in `.workgraph/federation.yaml`
- Transfer system with dry-run, entity filtering, conflict resolution
- Access policy enforcement (Private → reject, Shared → confirm, Open → allow)
- Peer workgraph discovery

### Commands

- `wg agency remote add/remove/list`
- `wg agency pull <source>` / `wg agency push <target>`
- `wg agency merge <source> <target>`
- `wg agency scan`

### Comparison to spec

**Partially aligned.** The spec treats federation as "optional and context-dependent" with access control at the primitive level. The implementation has the infrastructure but may not fully exercise push/pull for the new primitive types (components, outcomes, tradeoffs as separate entities). The spec's strategic framing (outsourced vs in-house agency, proprietary accumulation) is a conceptual layer, not an implementation requirement.

---

## 10. Summary: Spec vs Implementation

| Spec Requirement | Status | Notes |
|-----------------|--------|-------|
| Primitive store as ground truth | **DONE** | 3 types, content-addressed, independent storage |
| Composition cache above primitives | **DONE** | Role = components + outcome, Agent = role + tradeoff |
| Evaluation cascade to primitives | **DONE** | 6-step cascade, scores propagate to all entities |
| Two-level reward signal | **DONE** | Org-level concerns folded into evaluator rubric as weighted dimensions (30% org / 70% individual) — no separate OrgEvaluation type |
| Performance/learning continuum | **EXCEEDS** | UCB1, attractor weights, experiments, retrospective inference |
| Evolver mutation operations | **DONE** | 9+ strategies, Level×Amount, deferred queue |
| Human oversight on objectives | **DONE** | Deferred queue for DesiredOutcome changes |
| Meta-agent evolution | **DONE** | meta_swap_role, meta_swap_tradeoff, meta_compose_agent ops in evolve.rs; evolver self-mutation auto-deferred |
| `wg evolve review` CLI | **DONE** | list/approve/reject subcommands for deferred operations |

| Special agents as regular agents | **20%** | Components defined, not composed/rendered/evaluated as agents |
| Assigner from primitives | **PARTIAL** | Functional but hardcoded prompt, not composed from components |
| Evaluator from primitives | **PARTIAL** | Functional but hardcoded prompt |
| Evolver from primitives | **PARTIAL** | Functional but hardcoded prompt |
| Agent creator pipeline | **WIRED** | agency_create.rs module wired into mod.rs/main.rs; `auto_create` config flag in coordinator; `wg agency create` CLI available |
| Federation at primitive level | **PARTIAL** | Infrastructure exists, primitive-level transfer needs testing |
| Task clarification by assigner | **NOT DONE** | Component defined, not wired (agent changes lost) |
| Rubric specification spectrum | **NOT DONE** | Agent changes lost — needs re-implementation |
| Proper scoring rules for evaluator | **NOT DONE** | Agent changes lost — needs re-implementation |
| Record evaluations on special agents | **NOT DONE** | Agent changes lost — needs re-implementation |
| ~~Auto-trigger org evaluation~~ | **REMOVED** | No longer needed — org concerns integrated into standard evaluator dimensions |

### Validation note (2026-02-27)

Twelve dependency tasks were dispatched concurrently to implement audit gaps. Due to all agents working in the same directory without isolation, changes in 9 of 12 tasks were overwritten by concurrent file modifications. Only evolve.rs and main.rs changes survived (3 tasks: enable-evolution-of, evolver-consumes-org, implement-wg-evolve).

**Follow-up task `re-implement-missing` created** to re-do the 9 lost items. Must use sequential pipeline pattern or worktree isolation to prevent recurrence.

### What works end-to-end

Build: clean. Tests: 2825 passed, 0 failed. The following flows work:
1. Bootstrap (`wg agency init`) → seeds primitives + special agent components
2. Assignment (coordinator auto-assign with UCB1/learning modes)
3. Evaluation (6-step cascade with integrated org-level dimensions)
4. Evolution (9+ strategies, meta-agent targeting, deferred queue, review CLI)
5. Federation infrastructure (push/pull/merge with access control)

### Remaining critical path

1. Re-implement 9 lost audit items (see `re-implement-missing` task)
2. Compose special agents from their seeded role components during `wg agency init`
3. Wire prompt construction to render from role components instead of templates
4. Complete agent creator wiring (mod.rs + main.rs + config flags)

---

## Source file inventory

| File | Lines | Role |
|------|-------|------|
| `src/agency/types.rs` | 582 | All type definitions |
| `src/agency/store.rs` | 258 | Persistence layer |
| `src/agency/hash.rs` | 113 | Content-addressable hashing |
| `src/agency/eval.rs` | ~700 | 6-step evaluation cascade |
| `src/agency/prompt.rs` | 1,358 | Prompt rendering from primitives |
| `src/agency/lineage.rs` | 366 | BFS ancestry walkers |
| `src/agency/run_mode.rs` | 883 | Assignment routing, UCB1, experiments |
| `src/agency/starters.rs` | 2,238 | Bootstrap definitions |
| `src/agency/output.rs` | 226 | Task output capture |
| `src/commands/evolve.rs` | 4,510 | Full evolver system |
| `src/commands/evaluate.rs` | ~1,200 | Evaluation commands |
| `src/commands/assign.rs` | ~600 | Assignment commands |
| `src/commands/service/coordinator.rs` | ~900 | Coordinator auto-assign/eval |
| `src/config.rs` | 1,167 | All configuration |
| `src/federation.rs` | 1,783 | Federation infrastructure |
| **Total** | **~16,000** | |
