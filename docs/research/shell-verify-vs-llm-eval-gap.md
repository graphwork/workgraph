# Shell `--verify` vs. Agency LLM Evaluation — Gap Analysis

**Date**: 2026-04-22
**Task**: research-shell-verify
**Downstream**: design-llm-based (Design: LLM-based task verification API and completion gate)

**Scope**: This report focuses on the four questions in the task brief.
For a near-exhaustive surface-area inventory of `--verify`, see the prior
[`verify-deprecation-survey.md`](./verify-deprecation-survey.md) and
[`docs/design/verify-deprecation-plan.md`](../design/verify-deprecation-plan.md).
This document does **not** re-list every CLI flag and field; it isolates the
two systems' completion-gate semantics and quantifies the verify-failure data.

---

## 1. Current `--verify` flow

### Where it is set, stored, executed

- **Set**: `wg add --verify <cmd>` (`src/cli.rs:232`), `wg edit --verify <cmd>` (`src/commands/edit.rs:37`).
  Auto-population is also possible via `coordinator.auto_test_discovery`
  (`src/commands/spawn/execution.rs:161-188,672-684`), which scans for test files
  and writes `task.verify` when the agent has not set one.
- **Stored**: `Task.verify: Option<String>`, `Task.verify_timeout: Option<String>`,
  `Task.verify_failures: u32` (`src/graph.rs:305,309,390`). Persisted to
  `.workgraph/graph.jsonl`.
- **Executed**: `src/commands/done.rs:891-1216` is the main gate.
  - `run_verify_command_with_retry` (200-330) handles flock contention.
  - `run_verify_command` (486-712) actually shells out via `sh -c`.
  - Three transparent wrappers fire here: scoped-verify
    (`generate_scoped_verify_command`, 333-369) reduces `cargo test` to
    affected files only; smart-verify (`is_free_text_verify_command`, 371-442
    + `run_llm_verify_evaluation`, 444-481) detects prose and reroutes to
    `wg evaluate run`; auto-correct (`verify_lint::auto_correct_verify_command`)
    strips trailing descriptive suffixes ("passes", "succeeds without errors",
    etc.) and retries once.
- **Lint**: `src/verify_lint.rs` (711 lines) — invoked at `wg add` / `wg edit`
  time to warn on prose-shaped commands.

### States it drives

```
InProgress ──wg done──▶ run verify ──pass──▶ Done
                            │
                            └──fail──▶ stay InProgress, verify_failures++ ;
                                       at threshold (max_verify_failures, default 3) → Failed
```

In `verify_mode = "separate"` (off by default; gated by both
`verify_mode=separate` and `verify_autospawn_enabled`):

```
InProgress ──wg done──▶ PendingValidation
                            │
                            ▼
        coordinator spawns .sep-verify-<id> agent in fresh context
                            │
                  ──pass──▶ wg approve → Done
                  ──fail──▶ wg reject  → Open (re-dispatched)
```

The deprecated `verify_autospawn_enabled` (default false since 2026-04-17,
`src/config.rs:2670`) used to also produce `.verify-deferred-<id>` shadow
tasks when a `--verify` task got decomposed into children
(`done.rs:776-889`).

### Common failure modes when verify is misspecified at task creation

Empirical (full data in §4): the dominant failure mode is **prose-shaped
commands**. Agents write `## Validation` checklists in human language and
mistakenly carry that style into `--verify`. Examples observed in the live
graph: `"cargo test test_x passes"`, `".wg-worktrees/ contains only ..."`,
`"chat log shows endpoint errors"`. These produce one of three outcomes:

1. **Bash syntax error** — bare words after the shell-parsed prefix:
   `cargo test foo passes` → `error: unexpected argument 'passes' found`.
2. **Smart-verify reroute** — `is_free_text_verify_command` matches and the
   command gets shipped to `wg evaluate run`. If the source task lacks the
   agent identity / output dir the evaluator expects, the eval itself
   fails (`stderr: LLM evaluation failed for '...'`).
3. **Auto-correct rescue** — `verify_lint::auto_correct_verify_command`
   strips known trailing words and retries. This works for the simple
   `cargo test foo passes` shape but not for sentences.

Other failure modes (less common but real):
- **Wrong CWD assumptions**: `python -c 'from wg.adapter import ...'`
  with no `cd terminal-bench/` prefix → exit 127.
- **Real test failures conflated with verify bugs**: when the verify
  command is correct but the code is broken, the task gets stuck in the
  `verify_failures++` retry loop until the circuit breaker fires.

---

## 2. Current agency evaluation flow

### `wg evaluate run`

`src/commands/evaluate.rs:103-595` (`run`) and `602-1000` (`run_flip`).

Inputs assembled into the evaluator prompt
(`EvaluatorInput`, lines 286-311):

| Source | Field |
|---|---|
| Task | `title`, `description`, `skills`, `verify` (verbatim, for context), `tags`, `started_at`, `completed_at` |
| Agent | `agent`, `role`, `tradeoff` resolved via `agency::find_agent_by_prefix` |
| Outcome | resolved `desired_outcome` for the role |
| Artifacts | `artifacts[]` plus a computed git diff over those paths from the commit at `started_at` to `HEAD` (`compute_artifact_diff`, line 45) — capped at 30 KB |
| Log | full task `log[]` |
| FLIP | optional pre-existing FLIP `intent_fidelity` score loaded from `.workgraph/agency/evaluations/` |
| Verify task | optional sibling `.verify-<id>` status + log entries |
| Downstream context | titles+descriptions of `task.before` (consumers) |
| Child tasks | titles+descriptions of tasks where `after.contains(task_id)` (decomposition signal) |
| Evaluator identity | rendered identity prompt for `config.agency.evaluator_agent`, if configured |

The prompt is rendered by `agency::render_evaluator_prompt` (`src/agency/prompt.rs`).
The LLM call happens through `workgraph::service::llm::run_lightweight_llm_call`
with up to 3 JSON-extraction retries.

### Primitives that feed scoring

- **Role** (`agency::types::Role`) — skills + outcome
- **Outcome** (`DesiredOutcome`) — what "done well" means for this role
- **Tradeoff** — Careful / Fast / etc.
- **FLIP** (Fidelity via Latent Intent Probing) — two-phase reverse-prompt
  evaluation: phase 1 reconstructs the prompt from artifacts,
  phase 2 compares reconstructed-vs-actual; produces dimensions
  `semantic_match`, `requirement_coverage`, `specificity_match`,
  `hallucination_rate` (`evaluate.rs:602-1000`).
- **Performance history** — `record_evaluation_with_inference` updates
  per-(role, tradeoff, task-type) running averages used by `wg agency stats`
  and the assigner.

### Output dimensions

Standard `EvalOutput`: `score: f64` (0..1), `dimensions: HashMap<String, f64>`,
`notes: String`. Recognized dimensions printed to terminal:
`correctness`, `completeness`, `efficiency`, `style_adherence`,
`downstream_usability`, `coordination_overhead`, `blocking_impact`,
`intent_fidelity` (folded in from FLIP).

### Artifacts produced

- `.workgraph/agency/evaluations/eval-<task_id>-<ts>.yaml`
  (or `flip-<task_id>-<ts>.yaml`) — full `Evaluation` record
- Performance-history updates inside `agency_dir`
- Token usage written back onto the `.evaluate-<task_id>` task
  (`evaluate.rs:521-538`)
- Optional auto-rescue task created via `super::rescue::run` when
  `auto_rescue_on_eval_fail` and the score is below `eval_gate_threshold`
  (default 0.7, `config.rs:2371-2376`)

---

## 3. Gap analysis

### Why are these two systems disjoint today?

They were designed for different jobs and bolted onto the lifecycle at
different points:

| | `--verify` | `wg evaluate run` |
|---|---|---|
| **Question answered** | Did this code execute correctly? | Did this artifact satisfy the role's desired outcome? |
| **Trigger** | `wg done` calls verify inline (or sets `PendingValidation` in separate mode) | `.evaluate-<id>` task spawned by coordinator after the source task is `Done` (or `Failed`) |
| **Cost** | One subprocess; deterministic | One or two LLM calls; ~100-300 ms × token cost |
| **Failure means** | Stay InProgress, retry, eventually Failed via circuit breaker | Score below `eval_gate_threshold` → `fail::run_eval_reject` + `auto_rescue` injects parallel rescue task |
| **Required state to act** | Source still `InProgress`, agent still alive | Source already `Done`/`Failed`; agent finished |
| **Where it transitions the source task** | Yes — gates `Done` | Optionally — only via auto-rescue path, which marks source `Failed` and inserts a sibling |

### Is agency evaluation post-hoc or a completion gate?

**Today, primarily post-hoc.** The source task reaches `Done` via the
`wg done` path (which checks `--verify` if set, but does NOT call out to
the evaluator). Then the `.evaluate-<id>` task gets dispatched and the
evaluator scores the artifact.

It is *partially* a completion gate via two indirect paths:

1. **`auto_rescue_on_eval_fail` + `eval_gate_threshold`**
   (`src/commands/evaluate.rs:1369-1488`, `src/config.rs:2371-2393`):
   when the eval score is below the threshold (default 0.7) AND the task is
   gated (either `eval_gate_all` or has the `eval-gate` tag), the original
   task is **retroactively** failed and a rescue task is injected with
   `Position::Parallel, replace_edges=true`. Successors unblock from the
   rescue. So eval-gate is a "soft gate" — it doesn't block `wg done`
   completing, but it can revoke the `Done` after the fact and reroute
   downstream consumers.
2. **Smart-verify reroute** (`done.rs:506-509`): when the verify command is
   prose, `wg done` calls `evaluate::run` synchronously and uses its
   success/failure to decide whether the verify gate "passed". This is a
   real completion gate — but only for tasks whose verify command was
   prose-shaped enough to trigger the heuristic.

The two paths have different semantics (sync gate vs. async revoke) and
different scoring contracts. There is no unified "evaluation result decides
completion" path.

### What would it take to make LLM evaluation the primary completion gate?

The deprecation plan (`docs/design/verify-deprecation-plan.md`) already
chose a shape for this: hard-remove `--verify`, keep `## Validation` blocks
as agent-facing self-check, and rely on `auto_rescue_on_eval_fail` for the
actual gate. To make LLM-eval the **synchronous** completion gate (rather
than async revoke), the missing pieces are:

1. **A blocking call from `wg done` to the evaluator.** Today
   `done.rs:891-1216` runs the shell verify before transitioning to `Done`.
   The equivalent LLM-eval gate would be a new branch in `done.rs` that, for
   gated tasks, fires `evaluate::run` (or a lighter blocking variant)
   inline and returns its pass/fail to the same circuit-breaker logic.
   Smart-verify (`run_llm_verify_evaluation`) is essentially a prototype of
   this; it just isn't the primary path.
2. **A pass/fail contract on the eval, not just a score.** Today the
   evaluator returns a score plus dimensions. A gate needs a deterministic
   bool, derived either from a threshold (`eval_gate_threshold`) or from a
   structured `pass: bool` field. Options:
   - Reuse the threshold mechanism — but that's currently in
     `check_eval_gate`, not in `wg done`.
   - Add an explicit `pass: bool` to `EvalOutput` and let the prompt produce it.
3. **Latency budget.** A blocking eval call adds 5-30 s to `wg done`; smart-verify
   already does this and it's tolerable, but for shell-verifiable tasks
   (`cargo test`) it would be a regression. The design needs a per-task or
   per-tag opt-in (e.g., `validation = "llm"` parallel to `validation =
   "external"`).
4. **An `wg approve` / `wg reject` analogue for separate-context eval gating.**
   The existing separate-verify path uses `PendingValidation` + dispatched
   verify agent + `wg approve`/`wg reject`. An LLM-eval gate could reuse
   this exact dance: `wg done` sets `PendingValidation`, the coordinator
   dispatches the existing `.evaluate-<id>` task, the evaluator's auto-rescue
   logic translates "score below threshold" into reject. The plumbing exists;
   the missing wire is a way for `.evaluate-<id>` to call `wg approve` on
   pass (and in the gating mode, mark the source `Done` rather than the
   current "post-hoc evaluation only" behavior).
5. **Cost accounting.** LLM-as-gate is a recurring spend. The system
   already tracks token usage on `.evaluate-*` tasks; what's missing is a
   way to budget gating evals separately (e.g., per-task max), so a
   pathological retry loop doesn't burn the wallet.
6. **Determinism for re-runs.** Shell verify is deterministic; LLM eval is
   not. Re-running a flaky eval gate on retry could oscillate. A solution
   is caching: if `(task_id, artifact_diff_hash)` has already been
   evaluated, reuse the prior score. The evaluation file already keys on
   task_id+timestamp; adding an artifact-hash key is straightforward.

The combination of (1) + (2) + the per-task opt-in (3) gives a clean
"validation = llm" mode that mirrors `validation = external` and slots
into the existing `Status::PendingValidation` machinery — no new status
needed.

---

## 4. Failure data — survey of recent verify failures

Source: `.workgraph/graph.jsonl` (147 nodes, scanned 2026-04-22).
15 tasks have a `verify` field set.

### Aggregate

| Metric | Count |
|---|---|
| Tasks with `verify` field | 15 |
| Total `Verify FAILED` log entries | 18 |
| Tasks with non-zero `verify_failures` counter at scan time | 1 |
| Circuit-breaker trips (`verify-circuit-breaker` actor) | 0 |
| Auto-correction events (`verify-autocorrect` actor) | 0 |

The low circuit-breaker / auto-correct counts are because most failed
verify commands were either fixed by the agent on the next attempt or the
task ultimately reached `Done` after agents revised the verify field.

### Per-failure classification

Of 18 `Verify FAILED` log entries, classified by root cause:

| Cause | Count | Examples |
|---|---|---|
| **Verify command is descriptive prose / malformed** | 13 | `"cargo test test_x passes AND no leaks"` (bash chokes on `passes`); `".wg-worktrees/ contains only ..."` (no shell metachar, fell through to LLM eval which also failed); `"chat log shows endpoint errors..."`; `"wg nex ... 'list files in cwd' exits 0 and prints valid JSON"` |
| **Real code/test failure** | 5 | `cargo test agency_skips_system_tasks` (exit 101 = panic); `cargo build && cargo test` (real compile error); `cargo test --test integration_worktree --no-run` (warning + build error); `cargo test test_retry_clears_session && cargo build && cargo test` (real test fail on second attempt, after first attempt was malformed) |

**~72% of verify failures observed in the live graph were due to a buggy
verify command, not buggy code.** This matches the design rationale for
the verify-deprecation plan: the current `--verify` interface, which asks
agents to write executable shell commands, is mismatched with how agents
naturally express acceptance criteria (prose checklists). The smart-verify
LLM fallback exists precisely to plaster over this gap, but the data shows
it doesn't fully work — when smart-verify reroutes prose to
`wg evaluate run`, the eval often itself fails to extract structured
output from a context that wasn't designed for it (`"LLM evaluation
failed for ..."`).

### Tasks where the agent eventually self-corrected

Several tasks (e.g., `cleanup-surgically-remove`, `verify-worktree-gc`,
`integrate-atomic-cleanup`) appear with multiple `Verify FAILED` entries
followed by `Done`. The agent's loop was: write a prose verify → fail →
read the failure → re-do the work → eventually rewrite verify to a real
shell command (often `true` or a narrowly scoped `cargo test`). This is
the system functioning as designed — the gate held, the agent learned —
but it cost 1-3 retry cycles each and burned coordinator dispatch budget.

---

## 5. Integration points where LLM verification could plug in

Listed in dispatch-order (earliest hook first):

| # | Hook point | File:line | What an LLM gate would do here |
|---|---|---|---|
| 1 | **`wg add` / `wg edit` create-time lint** | `src/verify_lint.rs:91` | Already exists for shell verify (warns on prose). For LLM-gate mode: validate that the task's `## Validation` block is well-formed and parseable into success criteria. Cheap, fast, no LLM call. |
| 2 | **Agent prompt injection** | `src/service/executor.rs:934-939` | Today injects `## Verification Required: <verify cmd>`. LLM-gate analogue: inject "Your work will be evaluated against `## Validation` by an independent reviewer before completion." Sets agent expectation. |
| 3 | **`wg done` inline gate (synchronous)** | `src/commands/done.rs:891-1216` | The current sync-gate location. Insert a branch: if `task.validation == "llm"`, call `evaluate::run` with a minimal prompt that returns `{ pass: bool, reason: string }`. Reuse circuit-breaker semantics for retries. |
| 4 | **`wg done` PendingValidation transition (async)** | `src/commands/done.rs:902-960` | The current verify_mode=separate transition. Mirror it: `validation = "llm"` → `PendingValidation` → coordinator dispatches the existing `.evaluate-<id>` task (already exists per `eval_scaffold.rs:439-537`), and the evaluator calls `wg approve` / `wg reject` based on score. |
| 5 | **Coordinator dispatcher** | `src/commands/service/coordinator.rs:3878-3915` | Where `build_separate_verify_tasks` runs today. Already runs `build_auto_evaluate_tasks` and FLIP injection here. Adding a "gate-mode" eval dispatcher is one more `modified \|=` line. |
| 6 | **`.evaluate-<id>` task itself (post-hoc audit)** | `src/commands/evaluate.rs:103-595` | Where `auto_rescue_on_eval_fail` already gates retroactively. To strengthen this into a proper completion gate: when the source is in `PendingValidation` (not `Done`), call `wg approve <source>` on pass and `wg reject <source>` on fail, instead of (or in addition to) the rescue path. |
| 7 | **FLIP injection** | `src/commands/service/coordinator.rs:1692-1957` | Already creates `.verify-<id>` "verify by independent agent" tasks when FLIP score is low. These could become the primary path, with FLIP itself acting as the gating signal. |
| 8 | **`wg approve` / `wg reject` CLI** | `src/commands/approve.rs:30`, `src/commands/reject.rs:34` | Existing transitions. No change needed; the LLM gate just needs to be able to reach them, either as a CLI subprocess (already possible in claude executor) or via a new `wg_approve`/`wg_reject` native tool (gap #1 in `verify-deprecation-survey.md` §2). |

The cleanest minimal integration: hook (4) + hook (6). `wg done` sets
`PendingValidation` for `validation="llm"` tasks; the existing
`.evaluate-<id>` task is taught to terminate the source via approve/reject.
No new status, no new coordinator phase, ~50 LOC of glue.

---

## 6. Recommendation

**Make LLM-eval a per-task opt-in via a new `validation = "llm"` mode,
parallel to the existing `validation = "external"`.** Do not replace
`--verify` outright; let the deprecation plan run its course (which
removes the field entirely from agent-facing surface) and re-introduce
LLM gating as a typed `validation` mode for tasks that explicitly want it.

### Why per-task, not global

- **Cost**: a global LLM gate would add 5-30 s + token cost to every
  `wg done`. Most tasks (smoke tests, shell scripts, simple refactors)
  don't need it.
- **Determinism**: deterministic shell tasks should stay shell-verified
  when a real shell command is appropriate. The deprecation plan
  preserves the `## Validation` checklist as agent self-check; that's
  enough for the deterministic case.
- **Selective**: research / design / docs / "fix the prompt" tasks —
  exactly the cases where shell verify fails today (per §4) — are where
  LLM eval shines. Those tasks should opt in.
- **Existing precedent**: `validation = "external"` is already a typed,
  per-task validation mode (`done.rs:1218-1348`). Adding `"llm"` as a
  third value reuses all of the `PendingValidation` machinery,
  `wg approve` / `wg reject`, and the agent registry handling.

### What changes

1. **Add `validation = "llm"` value** with the same `PendingValidation`
   transition as `"external"` (`done.rs:1275-1321`).
2. **Wire `.evaluate-<id>` to call `wg approve` / `wg reject`** when the
   source is `PendingValidation` and the score crosses
   `eval_gate_threshold` (modify `evaluate.rs:1444-1488`).
3. **Default `validation = "llm"` for tasks tagged `eval-gate`** — already
   the existing per-task opt-in tag.
4. **Keep smart-verify reroute** as a backstop for stragglers but mark it
   for removal once `--verify` is hard-removed (per the deprecation plan).
5. **Don't add LLM-gate to `wg done` synchronously.** The async
   PendingValidation path is cheaper to implement, easier to reason
   about, and already used by `external` validation.

### What stays in the existing deprecation plan

- Hard-remove `--verify` field, CLI flags, lint module, scoped-verify,
  smart-verify, separate-verify coordinator dispatch (per
  `verify-deprecation-plan.md` §T2).
- Keep `PendingValidation` status; narrow comments to "external manual
  hold OR LLM gate" once this design lands.
- Keep `## Validation` blocks as agent-facing self-check (per plan §4).
- Keep `auto_rescue_on_eval_fail` for post-hoc revoke-and-replace; the
  new `validation = "llm"` mode is for prospective gating.

### What does NOT replace what

- `## Validation` block ≠ LLM gate. Validation block is agent self-check
  during execution. LLM gate is third-party post-execution audit.
- `auto_rescue_on_eval_fail` ≠ LLM gate. Auto-rescue is async revoke;
  LLM gate is synchronous (or via `PendingValidation`) blocking.
- LLM gate ≠ `--verify`. The shell verify model assumed deterministic
  executable criteria; the LLM gate model assumes natural-language
  acceptance criteria evaluated by an independent reviewer.

The three are complementary, not substitutes, and a clean design treats
them as three layers (self-check → post-hoc audit → optional gate)
selectable per-task.
