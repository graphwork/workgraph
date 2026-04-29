# Design: Post-Triage Quality Pass

> **Contributor doc — not required to USE workgraph.** The behavior described
> here is implemented. The authoritative description of when chat agents
> insert a quality-pass task lives in `wg agent-guide` (bundled with the `wg`
> binary). This document explains the rationale and design choices for
> people hacking on workgraph itself.

## Overview

The quality pass is a regular workgraph task that reviews and adjusts task metadata
after the coordinator creates tasks from a user request. It sits in the dependency
chain between task creation and task execution:

```
coordinator creates tasks → .quality-pass-<batch> reviews them → downstream tasks execute
```

The quality pass uses **only existing workgraph primitives**: `wg assign`, `wg edit`,
regular tasks, and dependency edges. No new task states, lifecycle phases, or special
coordinator logic.

## Design Questions — Answered

### 1. How does the coordinator insert the quality pass into the dependency chain?

**Answer: The coordinator inserts a `.quality-pass-<batch>` task after creating a batch of tasks, using `--before` edges to gate downstream execution.**

The coordinator already follows a pattern of creating system tasks (`.assign-*`,
`.evaluate-*`, `.flip-*`) that sit in the dependency chain. The quality pass uses
the same pattern:

1. Coordinator receives a user request and creates tasks via `wg add`.
2. Coordinator creates a single `.quality-pass-<batch-id>` task with no `--after`
   dependencies (immediately ready).
3. Coordinator wires each new task to depend on the quality pass:
   `wg edit <task-id> --add-after .quality-pass-<batch-id>` for each task in the batch.

The batch ID is a timestamp (matching the existing `.create-*` and `.evolve-*` convention):
`.quality-pass-20260402T143800`.

**Critical ordering with existing pipeline:** The quality pass must run BEFORE
`.assign-*` tasks because it may change the assignment. There are two options:

- **Option A (recommended):** The quality pass runs as the FIRST step, before
  `scaffold_full_pipeline`. The coordinator creates tasks with `--no-place` and
  `--paused`, then creates the quality pass task, then the quality pass agent
  un-pauses tasks after review (which triggers the normal scaffold pipeline at
  coordinator tick). This keeps the quality pass cleanly separated from the
  existing pipeline.

- **Option B:** The quality pass runs after `.assign-*` and overrides assignments.
  This wastes the initial assignment LLM call. Rejected.

**Chosen: Option A.** The coordinator creates tasks paused. The quality pass reviews
them, applies agency assignment + model selection + verify gates, then un-pauses
them (via `wg resume <task-id>`). The coordinator's normal tick then scaffolds
`.assign-*`, `.flip-*`, `.evaluate-*` as usual — but the `.assign-*` step sees
the pre-set agent and skips the LLM call (existing behavior: `build_auto_assign_tasks`
skips tasks that already have `.agent` set).

### 2. What data does the quality pass agent read?

The quality pass agent reads:

1. **Task descriptions** — via `wg show <task-id>` for each task in the batch.
   Task nature (research, implementation, fix, design, docs) is inferred from
   title keywords and description content.

2. **Agency profiles** — via `wg agency stats` for the role leaderboard (which
   roles perform best), plus reading individual agent YAML files from
   `.workgraph/agency/primitives/` when needed. The agent catalog is available
   via the same data the assigner uses.

3. **Evaluation history** — via scanning `.workgraph/agency/evaluations/` for
   scores segmented by:
   - Agent (role + tradeoff combination) — which agents do well on which task types
   - Model tier — which models succeed on which complexity levels
   - Task type — implementation vs. research vs. design vs. fix

4. **Existing batch context** — the batch's task IDs and their inter-dependencies,
   to understand the overall structure being reviewed.

### 3. How does the quality pass know which tasks to review?

**Answer: Explicit task IDs passed in the quality pass task description.**

The coordinator includes the list of task IDs in the quality pass task's description:

```
## Tasks to review
- implement-auth-endpoint
- auth-unit-tests
- auth-integration-test
- auth-docs-update
```

This is the simplest, most explicit approach. Alternatives considered:

- **Tags** (e.g., `batch-20260402T143800`): Requires a tag query mechanism that
  doesn't exist. Adds noise to task tags.
- **Graph inspection** ("review all paused tasks"): Fragile — other paused tasks
  from different batches could be mixed in.
- **Convention** ("review tasks created in the last N seconds"): Race-prone with
  concurrent coordinator operations.

Explicit IDs are unambiguous, trivially parseable, and match the pattern used by
`.create-needed-*` tasks (which list source task IDs in their description).

### 4. What's the minimal viable version?

**MVP: Agency assignment + model selection only.** Verification gates deferred to v2.

The MVP quality pass does two things per task:

1. **Select agent** — Match task nature to the best-performing agent identity:
   - `wg assign <task-id> <agent-hash>` — set the agent before the task un-pauses

2. **Select model** — Pick the right model tier based on task complexity:
   - `wg edit <task-id> --model <tier>` — set the model

Then un-pause:
   - `wg resume <task-id>` — release the task for execution

**v2 additions (deferred):**
- Verification gate consistency: `wg edit <task-id> --verify "..."` for code tasks
  missing verify criteria.
- Cross-task dependency validation: check that parallel tasks don't modify the same files.
- Model budget optimization: distribute model tiers across a batch to stay within
  cost constraints.

### 5. Should the quality pass be a cycle task or one-shot per batch?

**Answer: One-shot per triage batch.**

Rationale:
- Each batch is a discrete unit of work from a single user request.
- The quality pass reviews a finite, known set of tasks and completes.
- A cycle would imply "keep reviewing as new tasks appear" — but the coordinator
  already handles new batches by creating new quality pass tasks.
- The existing `.assign-*` and `.evaluate-*` tasks are all one-shot. The quality
  pass should follow the same pattern.

If the coordinator receives another user request while the first batch is still
being reviewed, it creates a second `.quality-pass-<batch2>` task. No cycle needed.

## Proposed CLI Workflow

### What the coordinator does (exact commands)

```bash
# Step 1: User says "Add auth system with JWT tokens"
# Coordinator decomposes into tasks, created PAUSED:

wg add "Research: JWT library selection" --paused --no-place \
  -d "## Description
Evaluate JWT libraries for Rust...

## Validation
- [ ] Library comparison matrix produced
- [ ] Recommendation with rationale"

wg add "Implement: JWT auth middleware" --paused --no-place \
  --after research-jwt-library \
  -d "## Description
Implement JWT auth middleware...

## Validation
- [ ] Failing test written first
- [ ] Implementation makes the test pass
- [ ] cargo build + cargo test pass"

wg add "Test: auth integration tests" --paused --no-place \
  --after implement-jwt-auth \
  -d "## Description
Write integration tests...

## Validation
- [ ] Tests cover happy path + error cases
- [ ] cargo test passes"

# Step 2: Coordinator creates the quality pass task (NOT paused, immediately ready):

wg add ".quality-pass-20260402T143800" \
  --no-place \
  -d "## Quality Pass: Post-Triage Review

Review and optimize task metadata for a batch of newly created tasks.

## Tasks to review
- research-jwt-library
- implement-jwt-auth
- test-auth-integration

## Instructions
For each task:
1. Read the task description via \`wg show <task-id>\`
2. Select the best agent identity via \`wg assign <task-id> <agent-hash>\`
   - Use \`wg agency stats\` for role performance data
   - Match task type to role: research → Researcher, implementation → Programmer, etc.
3. Select model tier via \`wg edit <task-id> --model <tier>\`
   - Simple/mechanical tasks → haiku
   - Standard implementation → sonnet
   - Complex reasoning/design → opus
4. Un-pause each task: \`wg resume <task-id>\`

## Data sources
- Role performance: \`wg agency stats\`
- Task details: \`wg show <task-id>\`
- Evaluation history: \`.workgraph/agency/evaluations/\`"

# Step 3: Wire batch tasks to depend on the quality pass:

wg edit research-jwt-library --add-after .quality-pass-20260402T143800
wg edit implement-jwt-auth --add-after .quality-pass-20260402T143800
wg edit test-auth-integration --add-after .quality-pass-20260402T143800
```

### What the quality pass agent does (inside the task)

```bash
# Read the batch task list from own description
wg show .quality-pass-20260402T143800

# Review each task
wg show research-jwt-library
wg agency stats
# Decision: research task → Researcher role (66be1375, highest avg 0.85) + model haiku (simple research)
wg assign research-jwt-library 66be1375...
wg edit research-jwt-library --model haiku
wg resume research-jwt-library

wg show implement-jwt-auth
# Decision: implementation → Programmer role (52335de1, avg 0.84, 871 tasks) + model sonnet (standard impl)
wg assign implement-jwt-auth 52335de1...
wg edit implement-jwt-auth --model sonnet
wg resume implement-jwt-auth

wg show test-auth-integration
# Decision: testing → Programmer role + model sonnet
wg assign test-auth-integration 52335de1...
wg edit test-auth-integration --model sonnet
wg resume test-auth-integration

wg done .quality-pass-20260402T143800
```

## Proposed Prompt Template

This is the task description template for `.quality-pass-*` tasks. The coordinator
fills in `{TASK_LIST}` with the actual task IDs.

```markdown
## Quality Pass: Post-Triage Review

Review and optimize task metadata for newly created tasks before they enter execution.

## Tasks to review
{TASK_LIST}

## What to do

For EACH task listed above:

### 1. Classify task type
Read the task via `wg show <task-id>`. Classify as one of:
- **research** — Investigation, analysis, library evaluation
- **implementation** — New code, features, endpoints
- **fix** — Bug fixes, error corrections
- **design** — Architecture, API design, planning
- **test** — Test writing, test infrastructure
- **docs** — Documentation, comments, guides
- **refactor** — Code restructuring without behavior change

### 2. Assign agent identity
Use `wg agency stats` to see role performance data. Select the best agent:

| Task type | Preferred role (by past performance) | Fallback |
|-----------|--------------------------------------|----------|
| research | Highest-scoring role on research tasks | Researcher role |
| implementation | Highest-scoring role on impl tasks | Programmer role |
| fix | Highest-scoring role on fix tasks | Programmer role |
| design | Highest-scoring role on design tasks | Architect role |
| test | Highest-scoring role on test tasks | Programmer role |
| docs | Highest-scoring role on docs tasks | Documenter role |
| refactor | Highest-scoring role on refactor tasks | Programmer role |

Apply: `wg assign <task-id> <agent-hash>`

If evaluation data is sparse (< 5 evaluations for a role+task-type combination),
use the overall role leaderboard from `wg agency stats` instead.

### 3. Select model tier
Pick based on task complexity signals:

| Signal | Model |
|--------|-------|
| Simple, mechanical, well-defined (e.g., "add a flag", "rename X") | haiku |
| Standard implementation, testing, research | sonnet |
| Complex design, multi-system reasoning, novel architecture | opus |
| Task has failed before (check status history) | escalate one tier |

Apply: `wg edit <task-id> --model <tier>`

### 4. Release for execution
After assigning agent and model: `wg resume <task-id>`

## Validation
- Every listed task has an agent assigned (check via `wg show`)
- Every listed task has a model set
- Every listed task is un-paused (status: open, not paused)
- Assignments are justified by evaluation data, not arbitrary
```

## Feedback Loop: Evaluations → Quality Pass

The quality pass reads historical evaluation data to make informed decisions. Here's
the concrete feedback loop:

```
1. Task executes → completes (Done/Failed)
2. .evaluate-<task> runs → produces Evaluation record:
   {
     "task_id": "implement-jwt-auth",
     "agent_id": "a4724ba7...",
     "role_id": "52335de1...",
     "tradeoff_id": "2dc69b33...",
     "score": 0.88,
     "model": "sonnet",
     "dimensions": { ... }
   }
3. Evaluation accumulates in .workgraph/agency/evaluations/
4. `wg agency stats` aggregates into role leaderboard
5. NEXT quality pass reads this data → better assignment decisions

Cross-cutting analysis the quality pass can do:
- Role X scores 0.85 avg on implementation but 0.72 on research → don't assign it to research
- Model tier "haiku" has 0.65 avg on complex tasks but 0.82 on simple → use haiku only for simple
- Agent Y (role+tradeoff) has declining trend → try a different tradeoff config
```

The quality pass does NOT modify the evaluation system. It is a pure consumer of
evaluation data and a producer of better initial assignments, creating a positive
feedback loop:

```
better assignments → higher eval scores → better data → even better assignments
```

## Integration with Existing Pipeline

The quality pass slots into the existing lifecycle without modifying it:

```
BEFORE (current):
  wg add → [.assign-* (LLM)] → task execution → .flip-* → .evaluate-*

AFTER (with quality pass):
  coordinator creates tasks (paused)
  → .quality-pass-* (assigns agent, model, resumes)
  → [.assign-* sees pre-set agent, skips LLM call]
  → task execution
  → .flip-* → .evaluate-*
```

Key compatibility points:
- **`.assign-*` is still scaffolded** by `scaffold_full_pipeline` at un-pause time.
  But `build_auto_assign_tasks` Phase 2 skips tasks that already have `.agent` set
  (line 839: `filter(|t| t.agent.is_none() && t.assigned.is_none())`).
- **`.flip-*` and `.evaluate-*` are unaffected** — they run after task completion as before.
- **Model selection** via `wg edit --model` is already supported and respected by
  the coordinator's spawn logic.
- **No new coordinator logic needed** — the quality pass is just a regular task that
  happens to run `wg assign` and `wg edit` commands.

## Coordinator Changes Required

The coordinator needs ONE behavioral change: when creating tasks from a user request,
create them paused and add a quality pass task. This is a change to the **coordinator
prompt** (the system instructions for the coordinator agent), not to coordinator code.

The coordinator prompt update adds:

```
When creating tasks from a user request:
1. Create all tasks with --paused --no-place
2. Create a .quality-pass-<timestamp> task listing the new task IDs
3. Wire each new task to depend on the quality pass via --add-after
```

No Rust code changes are needed for the MVP. The quality pass is a regular task
executed by a regular agent using existing CLI commands.

## Future Extensions (Not in MVP)

1. **Verify gate consistency**: The quality pass checks code tasks for `--verify`
   and adds missing gates based on task type templates.
2. **File conflict detection**: Cross-reference task artifacts/descriptions to detect
   parallel tasks that might modify the same files, and serialize them.
3. **Budget awareness**: Distribute model tiers across a batch to stay within a
   per-batch cost envelope.
4. **Quality pass as coordinator function**: Extract the quality pass pattern into
   a `wg func` template for reuse across projects.
