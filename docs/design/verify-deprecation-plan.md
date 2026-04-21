# --verify Deprecation Plan

**Date**: 2026-04-21
**Task**: design-verify-deprecation
**Input**: [verify-deprecation-survey.md](../research/verify-deprecation-survey.md)

---

## Executive summary

Hard removal of the `--verify` surface area (CLI flags, graph fields, gate
logic, separate-verify coordinator dispatch, prompt injection, lint module,
related tests). The `.evaluate` mechanism (already deputized via `wg_rescue`
and `auto_rescue_on_eval_fail`) is the post-removal validation path.
`PendingValidation` status is kept but narrowed to serve the
`validation="external"` use case only. One in-flight task needs its trivial
(`true`) gate cleared before the rip.

---

## Decisions

### 1. Deprecation strategy — **(a) Hard removal**

Picked over soft-deprecation (b) and silent-stub (c) for three reasons:

1. **Small blast radius**. The survey found 7 tasks historically with
   `verify` set, 6 are terminal, 1 in-flight has `verify="true"` (trivial).
   No user behavior relies on verify gates in practice right now.
2. **Dual codepaths have a cost**. Soft deprecation (b) means the gate logic,
   tests, lint module, and circuit breaker all stay live for a release while
   we print warnings. That's ~2000 lines of load-bearing code that needs
   maintenance during the warning window. The complexity isn't earning us
   anything because no one is relying on the gate.
3. **Silent-stub (c) is the worst option**. Agents currently set `--verify`
   in the decomp templates (`src/service/executor.rs:383-409`); if the flag
   is accepted-but-no-op, agents will think they have a safety net that
   isn't there. Failure is silent and delayed.

Rip happens in one coordinated change (Task.verify is referenced across
cli.rs, graph.rs, done.rs, coordinator.rs, executor.rs, config.rs — they all
have to move together to keep the tree green).

### 2. pending-validation status — **Keep, narrow to external-only**

The survey (§3) shows `PendingValidation` has two independent uses:

- **Use 1** (via verify): `verify_mode=separate` + `task.verify` set.
  Removed with the rip.
- **Use 2** (independent): `task.validation = "external"`. Human approval
  flow. Unrelated to `--verify` and in active use.

Removing the status entirely would break (2). Renaming it would churn the
status enum for no gain. Keep the status, keep `wg approve` / `wg reject`,
and just delete the verify-driven path that sets it. Comments/docs
should reflect the narrower purpose.

### 3. .evaluate deputization gap — **No capability build needed**

The survey (§2) and direct inspection of `src/commands/evaluate.rs:1412,1444`
confirm:

- `wg evaluate run <task>` operates on **Done** and **Failed** tasks (line
  123, 619). Done is the post-`wg done` state.
- `auto_rescue_on_eval_fail` calls `super::fail::run_eval_reject(...)` then
  `super::rescue::run(...)` with `Position::Parallel` (lines 1434-1484),
  injecting a corrective task at the same graph slot with successors
  rewired to it.
- Evaluator notes pass into the rescue description verbatim (line 1446-1452),
  so the rescue worker has full context.

The missing piece the research flagged (`wg_approve`/`wg_reject` native
tools) was only relevant if `.evaluate` needed to handle
`PendingValidation` targets — which it does not, since (a) we're removing
the verify path that produces `PendingValidation`, and (b) external
validation is explicitly human-driven.

No prerequisite capability task is needed. The final smoke-test task
exercises the existing `wg evaluate run` → `rescue::run` path on a `Done`
task.

### 4. ## Validation section in task descriptions — **Keep**

`## Validation` is agent-facing self-check guidance baked into the task
description. `.evaluate` is external third-party judgment. They serve
different layers:

- `## Validation` guides the agent **during** execution: "did I write the
  failing test first, does cargo build, are the acceptance criteria
  covered". It's a prompt-time checklist the agent consumes before calling
  `wg done`.
- `.evaluate` is a post-hoc audit by a different agent with different
  priors (evaluator role, evaluator tradeoff config), scoring the artifact
  against the desired outcome.

Removing `## Validation` would push the checklist into `.evaluate`'s scope,
which means every task needs an evaluation pass even for mechanical work.
Keeping it lets `.evaluate` be optional/selective while preserving
author-intended acceptance criteria at creation time. (The `CLAUDE.md`
template already prescribes this; no change needed there beyond dropping
`--verify` references.)

### 5. Migration for in-flight tasks — **Clear the trivial gate**

Only one non-terminal task has `verify` set: `run-5-task-smoke` with
`verify="true"`. Behavior is indistinguishable with or without the gate.

**Plan**: clear the `verify` field on `run-5-task-smoke` with
`wg edit run-5-task-smoke --verify ""` (or the equivalent JSONL edit)
**before** the rip lands. This keeps the rip PR internally consistent —
no task in the graph references the removed field.

### 6. Doc changes — **Five files**

| File | Action |
|------|--------|
| `CLAUDE.md` (lines 55, 60) | Remove `--verify` hard-gate guidance. Replace with pointer to `## Validation` self-check + optional `wg evaluate run <task>` post-hoc audit. |
| `docs/AGENT-LIFECYCLE.md` (lines 24, 36, 54, 73) | Remove `verify` field doc, remove verify-driven path from pending-validation. Keep external-validation path. |
| `src/executor/native/tools/wg.rs` quickstart (720-767) | Strip `--verify` guidance from the agent guide embedded in the native executor. Add evaluate-pattern guidance. |
| `src/service/executor.rs` decomp templates (383-409) | Strip `--verify 'cargo test ...'` from pipeline/fan-out examples. Keep template structure. |
| `docs/research/verify-cycle-interaction.md` | Prepend a "**HISTORICAL**: --verify was deprecated in 2026-04, see design/verify-deprecation-plan.md" banner. Content preserved for context. |

---

## Implementation task plan

Six impl tasks, then one final verify. Pipeline shape; a couple of parallel
splits. Task IDs are suggested; the placement step will use what `wg add`
accepts.

```
design-verify-deprecation (this task)
        │
        ├─► migrate-inflight-verify       [T1]
        │        │
        │        ▼
        └─► rip-verify-surface             [T2]  (after T1)
                 │
                 ├─► narrow-pending-validation   [T3]  (after T2)
                 ├─► update-agent-docs           [T4]  (after T2)
                 │
                 ▼
        smoke-test-evaluate-replaces-verify [T5 / final verify]
                 (after T2, T3, T4)
```

Each impl task modifies a **disjoint file set** so they can't overwrite
each other's work. T3 and T4 are genuinely parallel (different files) and
fan out from T2.

### T1 · migrate-inflight-verify

**Scope**: One graph edit.

**Files**: `.workgraph/graph.jsonl`

Clear `verify` on `run-5-task-smoke` via `wg edit` (or direct JSONL patch
using the standard JSONL-append semantics). Post-condition:
`wg show run-5-task-smoke` shows no verify command.

**Validation**:
- [ ] `wg show run-5-task-smoke` reports no verify command
- [ ] `graph.jsonl` scan: no non-terminal task has `verify` set

### T2 · rip-verify-surface

**Scope**: The coordinated removal. Everything has to move together for
`cargo build` to stay green across the change.

**Files**:
- `src/cli.rs` (remove `--verify`, `--verify-timeout` on `wg add`; remove
  `--skip-verify` on `wg done`; clean up `--also-strip-meta` references)
- `src/commands/edit.rs` (remove `--verify` flag + handling)
- `src/graph.rs` (remove `Task.verify`, `Task.verify_timeout`,
  `Task.verify_failures`; update deserialization helper at 949-1002)
- `src/commands/done.rs` (remove gate logic 721-927, keep external
  validation path 1275-1321; remove autospawn block 776-860; remove
  `run_verify_command*`, `generate_scoped_verify_command`,
  `resolve_verify_timeout`, `is_free_text_verify_command`,
  `run_llm_verify_evaluation`; remove verify unit tests)
- `src/commands/service/coordinator.rs` (remove `build_separate_verify_tasks`
  1974-2169 and its callsite in the coordinator main loop; **keep** FLIP
  injection 1692-1957 — FLIP produces `.verify-<id>` tasks that are
  ordinary open tasks and don't use the verify gate mechanism; adjust
  FLIP naming only if the name now misleads)
- `src/config.rs` (remove `verify_mode`, `verify_autospawn_enabled`,
  `max_verify_failures`, `verify_default_timeout`, `verify_triage_enabled`,
  `verify_progress_timeout`, `scoped_verify_enabled`,
  `auto_test_discovery` — the last two were verify-coupled per 2718, 2726)
- `src/service/executor.rs` (remove verify block at 934-939; remove
  `task_verify` from `TemplateVars` and `{{task_verify}}` substitutions at
  1103, 1177, 1180, 1313)
- `src/verify_lint.rs` (delete module entirely)
- `src/lib.rs` / `src/main.rs` (drop `mod verify_lint;` and related imports)
- `tests/integration_verify_first.rs` (delete)
- `tests/test_verify_lint_integration.rs` (delete)
- `tests/test_verify_timeout_basic.rs` (delete)
- `tests/test_verify_timeout_functionality.rs` (delete)
- `tests/test_prompt_logging_debug.rs` (lines 137, 228 — delete verify-prompt
  assertions; keep the rest of the file)

**Notes**:
- `PendingValidation` status remains (used by external validation path).
- `wg approve` / `wg reject` commands remain (used by external validation).
- `wg reset --also-strip-meta`: remove references to `.verify-*` and
  `.verify-deferred-*` from its docs; keep the flag for FLIP `.verify-<id>`
  and other meta tasks.
- Agent FLIP `.verify-<id>` tasks are **not** affected — they are ordinary
  open tasks.

**Validation**:
- [ ] `cargo build` passes
- [ ] `cargo test` passes (verify-removed suites deleted, all remaining
      tests green)
- [ ] `rg --no-heading 'task\.verify|--verify|verify_mode|verify_autospawn|verify_lint|verify_failures' src/ tests/` returns no hits in live code (matches only in historical comments or design docs if any)
- [ ] `wg add "smoke" --verify "cargo test"` fails with "unrecognized argument"
- [ ] `wg edit <task> --verify "..."` fails with "unrecognized argument"
- [ ] `wg add "smoke" && wg done smoke` transitions directly to Done
      without touching PendingValidation

### T3 · narrow-pending-validation

**Scope**: Semantic narrowing + doc strings. No behavior change beyond
what T2 already removed.

**Files**:
- `src/graph.rs` (update doc comment for `Status::PendingValidation` to
  say "external manual hold; set by `wg done` when `task.validation =
  external`")
- `src/query.rs:99` (confirm counting semantics still sensible post-rip —
  adjust comment only)
- `src/service/compactor.rs:267` (comment)
- `src/commands/approve.rs` / `src/commands/reject.rs` (update help text
  if it mentions verify)

**Validation**:
- [ ] `rg "verify_mode=separate|separate.verify" src/ tests/` returns no hits
- [ ] `wg approve --help` and `wg reject --help` mention external validation,
      not verify gates
- [ ] `cargo build` + `cargo test` green

### T4 · update-agent-docs

**Scope**: Documentation only. No source code changes beyond doc
comments already handled in T3.

**Files**:
- `CLAUDE.md` (strip `--verify` guidance at lines 55, 60; add pointer to
  `## Validation` self-check + `wg evaluate run` post-hoc audit)
- `docs/AGENT-LIFECYCLE.md` (remove `verify` field doc; narrow
  `PendingValidation` to external-only)
- `src/executor/native/tools/wg.rs` quickstart (720-767: replace `--verify`
  sections with "write a `## Validation` block and let `.evaluate` audit
  after `wg done`")
- `src/service/executor.rs` decomp templates (383-409: strip `--verify`
  from pipeline and fan-out example template strings)
- `docs/research/verify-cycle-interaction.md` (prepend HISTORICAL banner)

**Validation**:
- [ ] `rg --no-heading '\-\-verify' CLAUDE.md docs/ src/executor/ src/service/executor.rs` returns no hits in agent-facing guidance (historical research banners OK)
- [ ] `wg nex --help` (or whatever surfaces the agent quickstart) does not
      mention `--verify`
- [ ] AGENT-LIFECYCLE.md reads coherently with pending-validation
      described only as the external-hold state

### T5 · final-verify (smoke-test-evaluate-replaces-verify)

**Scope**: End-to-end smoke test of the post-deprecation path.

**Steps**:
1. `wg add smoke-body "Write a file that says 'wrong' into /tmp/smoke-out.txt"
   -d "## Description\nWrite 'wrong' to /tmp/smoke-out.txt. ## Validation\n- [ ]
   /tmp/smoke-out.txt contains exactly 'right'"` (note: ## Validation
   intentionally mismatches the body — an honest agent should catch this
   and do the right thing; for smoke we want to observe what happens when
   the agent completes anyway)
2. Dispatch via `wg service` or by assigning; agent does something
   reasonable and calls `wg done`.
3. `wg evaluate run smoke-body` → evaluator scores low because the acceptance
   criterion in `## Validation` is violated → `auto_rescue_on_eval_fail`
   injects a `.evaluate-smoke-body`-backed rescue task with
   `Position::Parallel`.
4. Coordinator dispatches the rescue task; agent fixes the file; rescue
   completes.
5. Graph state shows: original task Failed (eval-rejected), rescue task
   Done, successors of original now unblock from the rescue.

**Validation**:
- [ ] Step 3 produces a `wg list` entry for the rescue task
- [ ] Step 4 completes without manual intervention (the coordinator picks
      it up like any other task)
- [ ] `wg show <original-task>` shows status `failed` with
      reject-reason mentioning the evaluation score
- [ ] `wg show <rescue-task>` shows status `done`
- [ ] No `PendingValidation` state appears anywhere in the run
- [ ] No `--verify` flag used in any command in the run

If the smoke fails, capture which of (2),(3),(4) broke and file a rescue
task against T5 to address.

---

## Rollout sequencing

1. `design-verify-deprecation` completes, places T1..T5 with
   dependency chain as above.
2. Coordinator picks up T1 (trivial single-file edit).
3. Coordinator picks up T2 (the rip — will need a capable agent, possibly
   Careful Programmer + opus given its breadth).
4. T3 and T4 fan out in parallel from T2.
5. T5 runs after T3 and T4. On success, deprecation is complete. On
   failure, T5's failure mode drives the next corrective cycle.

## Risk and rollback

- **Risk**: downstream projects/scripts call `wg add --verify ...`.
  Rollback: keep a git tag pre-rip so `git revert` is a clean path.
- **Risk**: FLIP's `.verify-<id>` naming gets confusing post-removal.
  Mitigation: T2 Notes section; if needed, a follow-up rename to
  `.flip-verify-<id>` — not in scope for this plan.
- **Risk**: the smoke test exposes a previously-latent bug in the
  evaluate→rescue path. Mitigation: T5 is deliberately the last task and
  exists to catch exactly this. Failure produces a rescue task and the
  coordinator resumes the cycle.
