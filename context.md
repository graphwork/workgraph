# Project Context

## 1. Rolling Narrative

Workgraph is a lightweight task coordination graph system (Rust, MIT-licensed) enabling humans and AI agents to collaborate on complex multi-step projects. The project has progressed from core graph model through agency system integration toward production hardening. Key infrastructure: directed graph in `.workgraph/graph.jsonl`, daemon-based coordinator, git worktree isolation per agent, flock-based concurrency safety.

**Coordinator-21 Lifecycle (the compaction subject)**

Coordinator-21 was created 2026-04-09T13:42:00 as a persistent coordinator cycle alongside `.compact-21` and `.archive-21`. It operated normally through turns 1-8, then hit a critical failure: Turn 5 logged "wtf FLIP CALLS OPUS BY DEFAULT???????? they aren't configurable!!!??!?!?!??!?!?!" and the cloud account exhausted its credits. Turn 9 showed the executor failing with edit_file errors. The coordinator spent turns 6-17 trying to diagnose and recover, ultimately determining that context/token exhaustion was the root cause.

By Turn 17 (2026-04-09T15:14:01), coordinator-21 had logged 17 iterations with no convergence. The system responded by creating coordinator-22 (2026-04-09T15:17:09), which is now active and processing new work. Coordinator-21's daemon state shows `enabled=false`, `max_agents=0`, `accumulated_tokens=2,334,272` — confirming the exhaustion. Its cycle partners `.compact-21` and `.archive-21` remain open (waiting on coordinator-21 to complete before they can run).

**What this failure reveals**

The FLIP (Fidelity via Latent Intent Probing) evaluator hard-codes Opus as its model, bypassing executor/model configuration. When combined with high token usage from large context windows, this caused rapid credit depletion. The system needs: (1) FLIP model configurability, (2) better coordinator resource budgeting, (3) early warning when token accumulation approaches limits.

**Current State (2026-04-09 15:21)**

Coordinator-22 is active (in-progress, Turn 3 completed). Coordinator-21 remains in-progress but frozen (loop_iteration=17, no_converge=true). This compaction task (`manually-compact-coordinator-21`) was spawned to document coordinator-21's state before it is retired or restarted. The project has ~6319 tasks in graph.jsonl, 4467 evaluation files, and a daemon log of ~8.6MB. The evaluator infrastructure (four-dimensional scoring + FLIP) is mature and heavily used.

**Pattern: Coordinators as Recyclable Resources**

The system treats coordinators as ephemeral agents that can be discarded and replaced. Coordinator-22 replacing coordinator-21 demonstrates this pattern. Each coordinator gets its own numbered cycle (`.coordinator-N`, `.compact-N`, `.archive-N`). When a coordinator fails or exhausts resources, a new one is spun up rather than attempting recovery on the broken instance.

## 2. Persistent Facts

**Storage & State**
- Graph: `.workgraph/graph.jsonl` (~6319 tasks, ~11.7MB)
- Config: `.workgraph/config.toml`
- Agency: `.workgraph/agency/` (roles, tradeoffs, agents, 4467 evaluations)
- Service: `.workgraph/service/` (state.json, daemon.log ~8.6MB, coordinator-state-N.json)
- Coordinator prompt: `.workgraph/service/coordinator-prompt.txt`

**Coordinator Cycles**
- Each coordinator N has: `.coordinator-N` (main), `.compact-N` (introspection), `.archive-N` (cleanup)
- Dependencies: compact/archive wait on coordinator; coordinator waits on compact/archive to reset cycle
- CycleConfig: `max_iterations=0, no_converge=true` for uncoordinated coordinator loops
- Coordinator-21 state: enabled=false, accumulated_tokens=2,334,272, frozen
- Coordinator-22 state: active (Turn 3 complete)

**Task Lifecycle & State Machine**
- States: `open → in-progress → done/failed/abandoned/blocked/waiting/pending-validation`
- Cycle iteration reset: when all cycle members hit `done`, header + members reset to `open`, loop_iteration increments
- Pending-validation: for tasks with `--verify` machine-checkable gates

**Agency System**
- Agents = (role + tradeoff) pairs, bound to tasks via `wg assign`
- Roles: skills + desired outcomes (e.g., "Programmer" → tested code)
- Tradeoffs: constraints/priorities (e.g., "Careful" → reliability over speed)
- Evaluation: four-dimensional scoring + FLIP (fidelity via latent intent probing)
- FLIP threshold: 0.70; sub-threshold triggers Opus verification agent
- **CRITICAL BUG**: FLIP hard-codes `claude-opus-4-latest` model — not configurable through executor or per-task model settings. This caused coordinator-21 exhaustion.
- Evolution: performance data → role/tradeoff creation/retirement

**Executor & Model Selection**
- Executors: `claude` (default LLM), `amplifier` (delegation + bundles), `shell` (subprocess), `native`
- Model tiers: haiku (simple), sonnet (typical), opus (reasoning)
- Provider: openrouter with minimax/minimax-m2.7 used as fallback after exhaustion
- Model priority: per-task > executor config > coordinator.model > executor default

**Concurrency & Isolation**
- Daemon spawns agents up to `max_agents` parallel, each in isolated git worktree
- flock-based file locking for safe concurrent graph modification

**Conventions**
- Task IDs: kebab-case, auto-generated or explicit `--id`
- Context scope: `clean` (minimal) → `task` → `graph` → `full` (entire history)
- Visibility: `internal` (default), `public` (sanitized), `peer` (rich)
- Code task pattern: includes `## Validation` section with test criteria and `--verify` gates

## 3. Evaluation Digest

**Coordinator-21 Post-Mortem**

Coordinator-21 ran 17 turns over ~90 minutes (13:42 to 15:14). Final turn consumed 209,026 tokens (input=208,273). Accumulated token count reached 2,334,272 — confirming context exhaustion as the failure mode. Root cause chain: FLIP evaluator → Opus → high per-call cost → rapid credit depletion → context window overflow → coordinator failure.

**FLIP Model Hardcoding is the Critical Path**

The single highest-priority fix identified from coordinator-21's failure is making FLIP respect the executor's model configuration rather than hardcoding Opus. Currently: `wg evaluate run <task> --flip` spawns an Opus agent regardless of what model the task's executor is configured for. This is a systemic risk for any coordinator running on budget-constrained providers.

**Evaluation Infrastructure is Healthy**

Despite the coordinator failure, the evaluation system itself performed correctly. 4467 evaluation files exist, scoring in the 0.7–0.9 range for typical tasks. Low FLIP (< 0.70) correctly triggers verification tasks. The agency pipeline (`.place-*` → `.assign-*` → task → `.flip-*` → `.evaluate-*`) is functioning.

**Recommended Immediate Actions**

1. **Fix FLIP model configurability** — Allow `wg evaluate run --flip` to use the task's executor model rather than hardcoding Opus. This is the highest-leverage fix from coordinator-21's failure.
2. **Add coordinator resource budgeting** — Warn when accumulated_tokens approaches provider limits. Coordinator-22 should start with a clean token budget.
3. **Evaluate coordinator-21** — Once this compaction is complete, run `wg evaluate run .coordinator-21` to formally record the failure pattern. This feeds the evolution system.
4. **Consider coordinator restart rather than freeze** — Coordinator-21 is still `in-progress` with `loop_iteration=17`. Consider whether frozen coordinators should be explicitly failed or restarted rather than left in limbo.

**Evaluation Scores Distribution**
- Design tasks: 0.9
- Implementation tasks: 0.8–0.9
- Fix/audit tasks: 0.8
- Infrastructure/config: 0.7–0.9
- Safety/compliance: 0.7
- Website fade-tagline: 0.56 (below threshold, triggered Opus verification)
