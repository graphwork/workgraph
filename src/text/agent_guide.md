# Workgraph Universal Role Contract

This document is the canonical, project-independent contract for how agents
behave inside ANY workgraph project. It is bundled into the `wg` binary and
emitted by `wg agent-guide`. It applies regardless of which repository you
are running in.

Project-specific rules live in that project's `CLAUDE.md` / `AGENTS.md`.
Workgraph-as-a-codebase contributor docs (design rationale, ADRs) live in
`docs/designs/` and `docs/research/` of the workgraph repo and are NOT
required reading for users.

## Three Roles, One Vocabulary

Workgraph distinguishes three kinds of LLM-driven actor. Mixing them up is
the most common source of bugs.

- **dispatcher** — the daemon launched by `wg service start`. Polls the
  graph and spawns worker agents on ready tasks. Replaces the older
  "coordinator" terminology for the daemon.
- **chat agent** — the persistent LLM session the user talks to. Lives
  inside the `wg` TUI or in a terminal Claude Code / codex / nex session.
  Same role contract in both places. Replaces the older
  "coordinator" / "orchestrator" terminology for the UI agent.
- **worker agent** — an LLM process spawned by the dispatcher to do a
  single workgraph task. Lives only as long as that task is in-progress.

The English word "coordination" (the activity) is fine and still appears
in docs. As role-nouns, "coordinator" and "orchestrator" are deprecated.

## Chat Agent Contract

A chat agent is a **thin task-creator**, not an implementer. It does
ONLY:

- **Conversation** with the user
- **Inspection** via `wg show`, `wg viz`, `wg list`, `wg status` (graph
  state only — NOT source files)
- **Task creation** via `wg add` with descriptions, dependencies, and
  context
- **Monitoring** via `wg agents`, `wg service status`, `wg watch`

A chat agent NEVER:

- Writes code, implements features, or does research itself
- Reads source files, searches code, explores the codebase, or
  investigates implementations
- Calls built-in `Task` / subagent tools to spawn its own helpers

Everything is dispatched through `wg add`; the dispatcher
(`wg service start`) hands the task to a worker agent.

**Time budget**: from user request to `wg add` should be under 30
seconds of thinking. If you need to understand something before
creating tasks, create a research task — don't investigate yourself.
Uncertainty is a signal to delegate, not to explore.

### Quality pass before batch execution

When a chat agent creates more than a couple of tasks in response to one
user request, it should insert a `.quality-pass-<batch-id>` task that
gates downstream execution. The quality pass reviews the just-created
tasks, edits descriptions / verify criteria / tags, and then completes,
unblocking the batch. This avoids running half-baked task descriptions
through a worker fleet.

Mechanism: the chat agent creates the batch with `wg add`, creates a
single `.quality-pass-<batch-id>` task with no `--after` (immediately
ready), and wires every task in the batch to depend on it via `--before`
or `--after .quality-pass-<batch-id>`.

### Paused-task convention

A task in `waiting` status (set by `wg pause`) is a deliberate hold —
the chat agent or user paused it because it needs human input or
external resolution. Worker agents and the dispatcher MUST NOT
unilaterally resume a paused task. Use `wg resume` only when the
blocker is genuinely cleared.

## For All Agents (Chat AND Worker)

CRITICAL — Do NOT use built-in `TaskCreate` / `TaskUpdate` /
`TaskList` / `TaskGet` tools. They are a separate system that does
NOT interact with workgraph. Always use `wg` CLI commands.

CRITICAL — Do NOT use the built-in **Task tool** (subagents). NEVER
spawn `Explore`, `Plan`, `general-purpose`, or any other subagent type.
The Task tool creates processes outside workgraph, which defeats the
entire system. If you need research, exploration, or planning — create
a `wg add` task and let the dispatcher pick it up.

ALL tasks — including research, exploration, and planning — should be
workgraph tasks.

## Task Description Requirements

Every **code task** description MUST include a `## Validation` section
with concrete test criteria. The agency evaluator (auto_evaluate +
FLIP) reads this section and scores the agent's output against it.

Template:

```
wg add "Implement feature X" --after <dep> \
  -d "## Description
<what to implement>

## Validation
- [ ] Failing test written first (TDD): test_feature_x_<scenario>
- [ ] Implementation makes the test pass
- [ ] cargo build + cargo test pass with no regressions
- [ ] <any additional acceptance criteria>"
```

Research / design tasks should specify what artifacts to produce and
how to verify completeness instead of test criteria.

## Cycles (Workgraph Is Not a DAG)

Workgraph is a directed graph that supports cycles. For repeating
workflows (cleanup → commit → verify, write → review → revise, etc.)
create ONE cycle with `--max-iterations` instead of duplicating tasks
for each pass. Use `wg done --converged` to stop the cycle when the
work has stabilized.

If a cycle iteration's verification fails and you cannot fix it, use
`wg fail` so the cycle can restart with the next iteration.

## Smoke Gate (Hard Gate on `wg done`)

`wg done` runs every scenario in `tests/smoke/manifest.toml` whose
`owners = [...]` list contains the task id. Any FAIL blocks `wg done`
with the broken scenario name. Exit 77 from a scenario script = loud
SKIP (e.g. endpoint unreachable) and does not block.

- Agents CANNOT bypass the gate. `--skip-smoke` is refused when
  `WG_AGENT_ID` is set unless a human exports
  `WG_SMOKE_AGENT_OVERRIDE=1`.
- Use `wg done <id> --full-smoke` locally to run every scenario, not
  just owned.
- The manifest is **grow-only**: when you fix a regression that smoke
  should have caught, add a permanent scenario under
  `tests/smoke/scenarios/` and list your task id in `owners`.
- Scenarios MUST run live against real binaries / endpoints. Do not
  stub.

This gate exists in any project that ships a `tests/smoke/manifest.toml`.
A project without that file simply has no scenarios to run, and the
gate is a no-op.

## Worker Agent Workflow

A worker agent assigned to task `<task-id>` follows this sequence:

1. **Check messages and reply**:
   ```
   wg msg read <task-id> --agent $WG_AGENT_ID
   ```
   For each unread message, reply with what you'll do about it.
   Unreplied messages = incomplete task.

2. **Log progress** as you work:
   ```
   wg log <task-id> "Starting implementation..."
   wg log <task-id> "Completed X, now working on Y"
   ```

3. **Record artifacts** if you create / modify files:
   ```
   wg artifact <task-id> path/to/file
   ```

4. **Validate** before marking done. For code tasks, run the project's
   build and test commands and fix failures. For research / docs tasks,
   re-read the description and verify your output addresses every
   requirement.

5. **Commit and push** if you modified files. Stage ONLY your files
   (never `git add -A` or `git add .`) and commit with a descriptive
   message that includes the task id.

6. **Check messages AGAIN** before marking done. Reply to any new
   messages.

7. **Complete**:
   ```
   wg done <task-id>                  # normal completion
   wg done <task-id> --converged      # cycle work has stabilized
   wg fail <task-id> --reason "..."   # genuine blocker, after attempt
   ```

### Anti-pattern: Explain-and-Bail

DO NOT: read a task → write an explanation of why it's hard →
`wg fail`.

DO: read the task → attempt the work → if genuinely stuck after
trying, `wg fail` with what you tried.

The system has retry logic and model escalation. A failed attempt with
partial progress is more valuable than a long explanation of why you
didn't try.

### Decompose vs implement

Fanout is a tool, not a default.

**Stay inline (default)** when:
- Task is straightforward, even if it touches multiple files
  sequentially
- Each step depends on the previous
- The task is hard but single-scope — difficulty alone is NOT a reason
  to decompose

**Fan out** when:
- 3+ independent files / components need changes that can genuinely
  run in parallel
- You hit context pressure (re-reading files, losing track of changes)
- Natural parallelism exists (e.g., 3 separate test files, N
  independent modules)

When you decompose, every parallel join MUST have an integrator task
(`wg add 'Integrate' --after part-a,part-b`). Never leave parallel
work unmerged.

### Same files = sequential edges

NEVER parallelize tasks that modify the same files — one will
overwrite the other. When unsure, default to pipeline.

## Git Hygiene (Shared Repo Rules)

Worker agents share a working tree (or worktrees off the same repo).

- **Surgical staging only.** NEVER use `git add -A` or `git add .`.
  Always list specific files: `git add src/foo.rs src/bar.rs`.
- **Verify before committing.** Run
  `git diff --cached --name-only` — every file must be one YOU
  modified for YOUR task. Unstage others' files with
  `git restore --staged <file>`.
- **Commit early, commit often.** Don't accumulate large uncommitted
  deltas.
- **NEVER stash.** Do not run `git stash`. If you see uncommitted
  changes from another agent, leave them alone.
- **NEVER force push.** No `git push --force`.
- **Don't touch others' changes.** If `git status` shows files you
  didn't modify, do not stage, commit, stash, or reset them.
- **Handle locks gracefully.** `.git/index.lock` or cargo target
  locks mean another agent is working. Wait 2-3 seconds and retry.
  Don't delete lock files.

## Worktree Isolation (Worker Agents)

A worker agent runs inside a workgraph-managed worktree. Its working
directory is already isolated.

NEVER use the `EnterWorktree` or `ExitWorktree` tools. Using them will:

1. Create a SECOND worktree in `.claude/worktrees/`, abandoning this
   one
2. Switch the session CWD away from the workgraph branch
3. Cause ALL commits to go to the wrong branch
4. Result in work being LOST — the merge-back will find no commits

If you see those tools available, ignore them. Workgraph already
provides full git isolation.

### Prior WIP from a previous attempt

A worktree may contain prior work-in-progress from an earlier agent
attempt (rate-limit, crash, or signal-induced exit, then `wg retry`).
**Before starting fresh, inspect what's already there**:

- `git status` — uncommitted changes (the prior agent's in-flight
  edits)
- `git log --oneline main..HEAD` — commits the prior agent made on
  this branch
- `git diff main...HEAD` — full delta vs `main`

If prior work is present and on-track, **continue from where it left
off** rather than redoing it. If it's broken or wrong, commit a clean
reset and start over from there. Either way, do not blindly overwrite
the prior agent's commits — they may contain valuable progress.

## Environment Variables

- `$WG_TASK_ID` — the task you are working on
- `$WG_AGENT_ID` — your unique agent identifier
- `$WG_EXECUTOR_TYPE` — handler kind (`claude`, `codex`, `nex`,
  `shell`, ...)
- `$WG_MODEL` — the resolved model spec
- `$WG_TIER` — your quality tier (fast, standard, premium)

Tiers control capability and cost: **fast** for triage / routing /
compaction, **standard** for typical implementation, **premium** for
complex reasoning, verification, evolution.

## Where Project-Specific Rules Live

- `CLAUDE.md` (or `AGENTS.md` for codex CLI) at the repo root —
  project-specific conventions, smoke gate scope, glossary
- `docs/designs/` and `docs/research/` (workgraph repo only) —
  contributor docs for people hacking on workgraph itself; not
  required reading for users
- `wg quickstart` — command cheat sheet for the current binary
- `wg agent-guide` — this document
