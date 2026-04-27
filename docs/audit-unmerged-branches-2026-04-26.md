# Audit: Unmerged Agent Branches — 2026-04-26

Follow-up to `docs/audit-unmerged-branches-2026-04-25.md`. The previous audit was read-only; this one **executes the recommendations** (push, archive, delete) and captures the root-cause finding for "why merges aren't happening".

## TL;DR

- **Inputs:** `git branch -r --no-merged main` reports **132** branches (119 `wg/agent-*` + 13 non-agent).
- **Root cause for "unmerged but should be merged":** the stigmergic merge-back in `wg done` only does a *local* `git merge --squash` into the worktree's `main` ref. It does **not** push `main` to `origin` and does **not** delete the agent branch on `origin`. Local `main` is currently **40 commits ahead of `origin/main`** — every `wg done` since the last manual `git push origin main` has been accumulating in this worktree's local main and nowhere else.
- **Reconcile actions executed (this commit):**
  1. Push `main` → `origin/main` (40-commit fast-forward, all already-approved squash merges).
  2. Push every `origin/wg/agent-*` branch tip to `refs/archive/wg/agent-*` (preserve content).
  3. Delete every `origin/wg/agent-*` branch.
  4. Result: `git branch -r --no-merged main` drops from 132 → ~13 (only non-agent long-tail branches remain).
- **Follow-up tasks filed:** see "Follow-ups" at the bottom.

## Classification summary (119 `wg/agent-*` branches)

| Category | Count | Description | Action |
|----------|-------|-------------|--------|
| **(a) Content already in main** | 96 | Squash-merge by task-id found on local `main` | Archive + delete |
| **(b) Truly unmerged** | 23 | No squash on `main` for this task-id; content diverges | Archive + delete; valuable items have follow-up tasks |
| **(c) Empty / abandoned** | 0 | None this round (the 6 from 2026-04-25 either rolled into (a) or have new commits) | n/a |
| **Total** | **119** | | |

The 13 non-agent branches (`nikete/*`, `origin/fix-*`, `origin/tui-*`, `origin/show-live-token`, `origin/provenance-*`, `origin/worktree-*`) are upstream / pre-agent-system human work and are **out of scope** for this reconcile — they need maintainer review, not branch-listing surgery.

---

## Root-cause finding: why merges aren't happening

`wg done` runs the stigmergic merge-back in `src/commands/done.rs:1739-1792` (`attempt_worktree_merge`):

```rust
// runs locally inside the agent worktree
let merge_result = Command::new("git")
    .args(["merge", "--squash", &wt.branch])
    .current_dir(&wt.project_root)
    ...
```

**What happens:** the agent's worktree branch is squash-merged into the *shared local* `main` ref. The squash commit is durable on whoever's machine the worktree lives on (here: erik's dev box).

**What does *not* happen:**
1. **No `git push origin main`** after the squash — local main accumulates indefinitely. Anyone who clones from `origin` sees neither the squash commit nor the agent's branch-tip work via `main`.
2. **No `git push origin :wg/agent-N/<task>`** — the agent's branch on `origin` is left forever, even after a clean local merge.
3. **No staleness signal** — `git branch -r --no-merged main` keeps reporting these branches because squash commits don't form a merge edge to the source branch tip.

**Net effect:** every clean `wg done` produces:
- ✓ a squash commit on local main (good)
- ✗ nothing on origin (bad — invisible to other clones / CI / the audit)
- ✗ a leftover branch on origin (bad — clutters `git branch -r`)

The 96 cat-(a) branches in this audit are 96 instances of this gap. `update-model-registry`, `restore-msg-inspector`, `nex-ux-600s`, etc. are not "merge bugs" so much as "we never pushed main, so everyone else still sees these as unmerged".

### Fix surface (filed as follow-up: `fix-done-merge-pushes-main`)

In `attempt_worktree_merge` after `WorktreeMergeResult::Merged { commit_sha }` succeeds, do (best-effort, non-fatal on failure):
```rust
let _ = Command::new("git").args(["push", "origin", "main"]).current_dir(&wt.project_root).output();
let _ = Command::new("git").args(["push", "origin", &format!(":refs/heads/{}", wt.branch)]).current_dir(&wt.project_root).output();
```
- Best-effort because offline / no-remote / ssh-unconfigured environments must not break `wg done`.
- The branch-delete step also serves as the cleanup signal — it makes the merged work *invisible* in `git branch -r`, which is the natural way other clones discover that work has landed.
- Non-fast-forward conflicts on `git push origin main` should: (a) `git fetch origin main`, (b) attempt `git merge origin/main` (FF only — fail loudly otherwise), (c) retry the push. If still failing, surface the error and leave the squash on local main as it does today.

---

## Merge contract (forward-looking)

When an agent calls `wg done` and the work is done:

1. **Default path (clean merge):** branch must land on `origin/main` within seconds.
   - Squash merge to local main (already implemented).
   - Push local main to `origin/main` (FF only). **MISSING.**
   - Delete branch on origin. **MISSING.**

2. **Conflict path:** if `git merge --squash <branch>` conflicts:
   - Without `--ignore-unmerged-worktree`: refuse, ask agent to resolve.
   - With `--ignore-unmerged-worktree`: create a `.merge-<task>` follow-up task. Branch on origin **stays** until the merge task resolves it.

3. **Failure path:** if the agent's work didn't pass evaluation:
   - Mark the agent task `Failed`, not `Done`. No merge happens, branch stays on origin until manual decision.

4. **Manual-review path:** if the work needs human review before landing:
   - Status `PendingValidation`, not `Done`. Mid-state until evaluator/human decides.

Today only step 1's *first* sub-step is implemented. Steps 1.b and 1.c are the "branch should land on main within X seconds" gap this audit identified.

---

## Category (a): Content already in main (96 branches)

Squash-merge audit ghosts. Local `main` already contains the work via a squash commit listed below. **Action: archive to `refs/archive/<branch>` then delete `origin/<branch>`.**

> Note: many of the listed squash commits are **on local `main` only** — i.e. not yet on `origin/main`. The first reconcile step (push origin main) makes them durable on origin; only then do the archive + delete steps preserve the work.

<!-- audit-table-cat-a-start -->
| Branch | Agent | Task ID | Files | Commits | Squash Commit |
|--------|-------|---------|-------|---------|---------------|
| `origin/wg/agent-100/spawn-single-source` | agent-100 | spawn-single-source | 4 | 1 | `a7d0ab022` |
| `origin/wg/agent-103/regression-test-for` | agent-103 | regression-test-for | 6 | 1 | `f0b739f04` |
| `origin/wg/agent-10/research-tb-harness-wiring` | agent-10 | research-tb-harness-wiring | 1 | 1 | `0211627b6` |
| `origin/wg/agent-112/fix-create-registry-v2` | agent-112 | fix-create-registry-v2 | 2 | 2 | `e6089d5ac` |
| `origin/wg/agent-117/fix-wg-retry-clears-session` | agent-117 | fix-wg-retry-clears-session | 4 | 1 | `415a5fc9a` |
| `origin/wg/agent-118/fix-expose-missing-v2` | agent-118 | fix-expose-missing-v2 | 2 | 1 | `f2576ae28` |
| `origin/wg/agent-11/research-wg-in-harbor` | agent-11 | research-wg-in-harbor | 1 | 1 | `5ef1d9bd0` |
| `origin/wg/agent-12/fix-pty-typing` | agent-12 | fix-pty-typing | 1 | 1 | `3a851cbd0` |
| `origin/wg/agent-146/fix-log-pane` | agent-146 | fix-log-pane | 1 | 1 | `c08755d7f` |
| `origin/wg/agent-148/make-wg-kill` | agent-148 | make-wg-kill | 5 | 1 | `bae7f5674` |
| `origin/wg/agent-14/fix-tui-scrollback` | agent-14 | fix-tui-scrollback | 3 | 1 | `bd8e77afe` |
| `origin/wg/agent-155/stop-auto-creating` | agent-155 | stop-auto-creating | 4 | 2 | `10658ee7d` |
| `origin/wg/agent-158/surface-coordinator-errors` | agent-158 | surface-coordinator-errors | 6 | 1 | `3d8f56423` |
| `origin/wg/agent-159/keep-mouse-mode` | agent-159 | keep-mouse-mode | 2 | 1 | `a41ee62d4` |
| `origin/wg/agent-16712/test-agent-worktree` | agent-16712 | test-agent-worktree | 1 | 1 | `d5b3ab769` |
| `origin/wg/agent-16722/tb-retest-smart-fanout` | agent-16722 | tb-retest-smart-fanout | 4 | 1 | `d295965c4` |
| `origin/wg/agent-16758/tb-research-fanout` | agent-16758 | tb-research-fanout | 1 | 1 | `de0b72a24` |
| `origin/wg/agent-16760/tb-impl-fanout-guidance` | agent-16760 | tb-impl-fanout-guidance | 2 | 1 | `c98223898` |
| `origin/wg/agent-16766/research-worktree-collision` | agent-16766 | research-worktree-collision | 1 | 1 | `2521753f7` |
| `origin/wg/agent-16770/fix-prevent-worktree` | agent-16770 | fix-prevent-worktree | 6 | 1 | `58225803b` |
| `origin/wg/agent-16772/fix-prevent-worktree` | agent-16772 | fix-prevent-worktree | 5 | 1 | `58225803b` |
| `origin/wg/agent-16773/fix-prevent-worktree` | agent-16773 | fix-prevent-worktree | 4 | 1 | `58225803b` |
| `origin/wg/agent-16774/fix-prevent-worktree` | agent-16774 | fix-prevent-worktree | 4 | 1 | `58225803b` |
| `origin/wg/agent-16777/fix-model-resolution` | agent-16777 | fix-model-resolution | 1 | 1 | `e77d49d16` |
| `origin/wg/agent-16779/implement-nex-interactive` | agent-16779 | implement-nex-interactive | 6 | 1 | `d9ad4f34f` |
| `origin/wg/agent-16784/implement-reprioritize-command` | agent-16784 | implement-reprioritize-command | 4 | 1 | `adc0db3e3` |
| `origin/wg/agent-16790/worktree-test-claude` | agent-16790 | worktree-test-claude | 1 | 1 | `c7b083269` |
| `origin/wg/agent-169/implement-wg-gc` | agent-169 | implement-wg-gc | 4 | 1 | `7b4274bf8` |
| `origin/wg/agent-170/fix-atomic-worktree` | agent-170 | fix-atomic-worktree | 4 | 1 | `5cda52aa1` |
| `origin/wg/agent-179/integrate-atomic-cleanup` | agent-179 | integrate-atomic-cleanup | 3 | 1 | `66bc71d23` |
| `origin/wg/agent-211/research-shell-verify` | agent-211 | research-shell-verify | 1 | 1 | `000b4bd24` |
| `origin/wg/agent-214/design-llm-based` | agent-214 | design-llm-based | 1 | 1 | `23da889b0` |
| `origin/wg/agent-217/implement-llm-verification` | agent-217 | implement-llm-verification | 21 | 1 | `6e062aff6` |
| `origin/wg/agent-22/fix-e-url` | agent-22 | fix-e-url | 1 | 1 | `201b69de6` |
| `origin/wg/agent-234/fix-web-search` | agent-234 | fix-web-search | 1 | 1 | `adf1e2adc` |
| `origin/wg/agent-237/fix-0-chat-panel` | agent-237 | fix-0-chat-panel | 2 | 1 | `9171d1e57` |
| `origin/wg/agent-241/restore-msg-way` | agent-241 | restore-msg-way | 2 | 1 | `155f07ac7` |
| `origin/wg/agent-243/fix-tui-tab` | agent-243 | fix-tui-tab | 2 | 1 | `e523a4f83` |
| `origin/wg/agent-244/implement-model-endpoint` | agent-244 | implement-model-endpoint | 6 | 1 | `637b620b6` |
| `origin/wg/agent-253/replace-new-coordinator` | agent-253 | replace-new-coordinator | 9 | 2 | `a51991f75` |
| `origin/wg/agent-258/embedded-pty-chat` | agent-258 | embedded-pty-chat | 2 | 1 | `1cf794786` |
| `origin/wg/agent-25/synth-wg-nex-plan-of-attack` | agent-25 | synth-wg-nex-plan-of-attack | 1 | 1 | `78cd8de5f` |
| `origin/wg/agent-260/investigate-tui-coordinator` | agent-260 | investigate-tui-coordinator | 1 | 1 | `31d64231f` |
| `origin/wg/agent-265/investigate-tui-pty` | agent-265 | investigate-tui-pty | 1 | 1 | `b3dd01c27` |
| `origin/wg/agent-267/fix-paste-forwarding` | agent-267 | fix-paste-forwarding | 3 | 2 | `93b81e0a4` |
| `origin/wg/agent-26/deprecate-or-remove` | agent-26 | deprecate-or-remove | 9 | 1 | `674aa5eeb` |
| `origin/wg/agent-270/fix-codex-resume` | agent-270 | fix-codex-resume | 1 | 1 | `e9a46c981` |
| `origin/wg/agent-271/fix-claude-resume` | agent-271 | fix-claude-resume | 2 | 1 | `27a099641` |
| `origin/wg/agent-277/fix-resume-all-executors` | agent-277 | fix-resume-all-executors | 4 | 2 | `426920029` |
| `origin/wg/agent-285/fix-clap-endpoint-conflict` | agent-285 | fix-clap-endpoint-conflict | 1 | 1 | `b7159d9af` |
| `origin/wg/agent-286/fix-paste-forwarding` | agent-286 | fix-paste-forwarding | 1 | 1 | `93b81e0a4` |
| `origin/wg/agent-288/add-incomplete-retryable` | agent-288 | add-incomplete-retryable | 33 | 1 | `6323d51a2` |
| `origin/wg/agent-28/force-executor-selection` | agent-28 | force-executor-selection | 6 | 1 | `9f46d43fe` |
| `origin/wg/agent-299/stronger-automatic-retry` | agent-299 | stronger-automatic-retry | 37 | 3 | `6323d51a2` |
| `origin/wg/agent-329/cascade-abandonment-to` | agent-329 | cascade-abandonment-to | 1 | 1 | `50e362dab` |
| `origin/wg/agent-381/stigmergic-merge-on-done` | agent-381 | stigmergic-merge-on-done | 7 | 1 | `d21491b84` |
| `origin/wg/agent-382/audit-unmerged-branches` | agent-382 | audit-unmerged-branches | 1 | 1 | `d5ead4798` |
| `origin/wg/agent-400/prompt-cycle-converge` | agent-400 | prompt-cycle-converge | 43 | 3 | `4fe1bb638` |
| `origin/wg/agent-404/research-cow-worktrees` | agent-404 | research-cow-worktrees | 44 | 4 | `777b03269` |
| `origin/wg/agent-408/fix-nex-embedded-input` | agent-408 | fix-nex-embedded-input | 3 | 1 | `0a373f159` |
| `origin/wg/agent-43/wire-priority-field` | agent-43 | wire-priority-field | 15 | 1 | `c7f1afdfe` |
| `origin/wg/agent-44/write-harbor-config` | agent-44 | write-harbor-config | 1 | 1 | `9920dcf61` |
| `origin/wg/agent-45/wg-nex-native` | agent-45 | wg-nex-native | 3 | 1 | `bcd17e1ab` |
| `origin/wg/agent-46/implement-nexevalagent-class` | agent-46 | implement-nexevalagent-class | 1 | 1 | `c979da53f` |
| `origin/wg/agent-46/stale-model-alias` | agent-46 | stale-model-alias | 8 | 1 | `6805f9c4e` |
| `origin/wg/agent-47/tui-add-coordinator` | agent-47 | tui-add-coordinator | 2 | 1 | `84afbc7ae` |
| `origin/wg/agent-47/tui-agent-activity` | agent-47 | tui-agent-activity | 2 | 1 | `fe0008ee8` |
| `origin/wg/agent-48/fix-clean-remaining` | agent-48 | fix-clean-remaining | 3 | 1 | `2737ba8ab` |
| `origin/wg/agent-48/tui-iteration-selector` | agent-48 | tui-iteration-selector | 1 | 1 | `87b66aabe` |
| `origin/wg/agent-49/model-is-not` | agent-49 | model-is-not | 2 | 1 | `3b8854059` |
| `origin/wg/agent-4/cheap-by-default` | agent-4 | cheap-by-default | 4 | 1 | `72cfe692e` |
| `origin/wg/agent-50/make-tiers-the` | agent-50 | make-tiers-the | 25 | 1 | `78bb9e2ce` |
| `origin/wg/agent-51/tui-close-coordinator` | agent-51 | tui-close-coordinator | 3 | 1 | `b5d4b5c5d` |
| `origin/wg/agent-51/wire-priority-field` | agent-51 | wire-priority-field | 22 | 1 | `c7f1afdfe` |
| `origin/wg/agent-52/inconsistent-agent-log` | agent-52 | inconsistent-agent-log | 2 | 1 | `400fd3a34` |
| `origin/wg/agent-54/research-verify-deprecation` | agent-54 | research-verify-deprecation | 1 | 1 | `3b7a48a1c` |
| `origin/wg/agent-57/design-verify-deprecation` | agent-57 | design-verify-deprecation | 1 | 1 | `443b17c75` |
| `origin/wg/agent-61/research-into-impl` | agent-61 | research-into-impl | 1 | 1 | `1f55153c1` |
| `origin/wg/agent-62/wg-nex-native` | agent-62 | wg-nex-native | 2 | 1 | `bcd17e1ab` |
| `origin/wg/agent-63/add-wg-archive` | agent-63 | add-wg-archive | 9 | 1 | `5bd4da104` |
| `origin/wg/agent-63/tui-purple-styled` | agent-63 | tui-purple-styled | 2 | 1 | `4e739d7c6` |
| `origin/wg/agent-66/agent-retry-with` | agent-66 | agent-retry-with | 2 | 1 | `d2668bd4c` |
| `origin/wg/agent-67/only-llm-evaluation` | agent-67 | only-llm-evaluation | 3 | 1 | `2082675fb` |
| `origin/wg/agent-70/merge-origin-wg` | agent-70 | merge-origin-wg | 3 | 2 | `60126d217` |
| `origin/wg/agent-70/tui-purple-styled` | agent-70 | tui-purple-styled | 2 | 1 | `4e739d7c6` |
| `origin/wg/agent-71/rename-dispatcher-daemon` | agent-71 | rename-dispatcher-daemon | 25 | 5 | `975a3356c` |
| `origin/wg/agent-72/tui-new-coordinator` | agent-72 | tui-new-coordinator | 3 | 1 | `555e3eea7` |
| `origin/wg/agent-72/wg-nex-native-2` | agent-72 | wg-nex-native-2 | 2 | 1 | `bcd17e1ab` |
| `origin/wg/agent-75/smoke-test-gap` | agent-75 | smoke-test-gap | 2 | 1 | `9450b8a34` |
| `origin/wg/agent-77/smoke-test-wg` | agent-77 | smoke-test-wg | 10 | 1 | `21ff70f40` |
| `origin/wg/agent-8/research-agent-wg-awareness` | agent-8 | research-agent-wg-awareness | 1 | 1 | `d82d5f7d3` |
| `origin/wg/agent-8/treat-wg-nex` | agent-8 | treat-wg-nex | 2 | 1 | `58eb7c751` |
| `origin/wg/agent-92/remove-validation-cli` | agent-92 | remove-validation-cli | 9 | 1 | `d7a7cf18f` |
| `origin/wg/agent-99/config-merge-recognize` | agent-99 | config-merge-recognize | 1 | 1 | `2270d4fb0` |
| `origin/wg/agent-9/research-smoke-scope-criteria` | agent-9 | research-smoke-scope-criteria | 1 | 1 | `9fe4f3d37` |
| `origin/wg/agent-9/wave-1-integration-smoke` | agent-9 | wave-1-integration-smoke | 2 | 1 | `163be0442` |
| `origin/wg/agent-9/wg-resume-on` | agent-9 | wg-resume-on | 1 | 1 | `ee295298d` |
<!-- audit-table-cat-a-end -->

**Recommendation:** archive + delete (executed in this commit). All work is preserved on `main` and on `refs/archive/<branch>`.

---

## Category (b): Truly unmerged (23 branches)

No squash commit found on `main` for these task IDs. Each branch is **conflict-clean** against current `main` (verified via `git merge-tree`), but the diff contents show several thematic clusters where the goal was achieved by a different path:

<!-- audit-table-cat-b-start -->
| Branch | Agent | Task ID | Files | Commits | Diff vs main | Disposition |
|--------|-------|---------|-------|---------|--------------|-------------|
| `origin/wg/agent-2/update-model-registry` | agent-2 | update-model-registry | 74 | 3 | +390/-389 | **superseded** — model registry has since been overhauled multiple times (`stale-model-alias`, `unpin-claude-model` already on main). Archive. |
| `origin/wg/agent-7/restore-msg-inspector` | agent-7 | restore-msg-inspector | 2 | 1 | +214/-27 | **superseded** — TUI Messages tab has since landed via different routes (`restore-msg-way` `155f07ac7`). Archive. |
| `origin/wg/agent-49/run-5-task-smoke` | agent-49 | run-5-task-smoke | 2 | 4 | +176/-15 | **superseded** — terminal-bench wiring landed via `wave-1-integration-smoke` and follow-ups. Archive. |
| `origin/wg/agent-69/thin-wrapper-impl` | agent-69 | thin-wrapper-impl | 5 | 1 | +429/-1 | **active candidate** — task is `pending-validation`. File follow-up to evaluate + merge if FLIP passes. |
| `origin/wg/agent-120/run-5-task-smoke-v2` | agent-120 | run-5-task-smoke-v2 | 18 | 1 | +418/-18 | **superseded** by harbor smoke evolution. Archive. |
| `origin/wg/agent-134/unpin-claude-model` | agent-134 | unpin-claude-model | 3 | 1 | +4/-4 | **superseded** — Claude model unpin already in main. Archive. |
| `origin/wg/agent-137/rip-verify-surface-v2` | agent-137 | rip-verify-surface-v2 | 43 | 1 | +114/-5042 | **superseded** by `remove-validation-cli` (agent-92, on main). Archive. |
| `origin/wg/agent-138/fix-complete-verify-v2` | agent-138 | fix-complete-verify-v2 | 49 | 1 | +86/-5378 | **superseded** by `remove-validation-cli` (agent-92, on main). Archive. |
| `origin/wg/agent-141/nex-ux-600s` | agent-141 | nex-ux-600s | 5 | 1 | +60/-4 | **valuable, isolated** — nex UX for slow models. File follow-up to cherry-pick. |
| `origin/wg/agent-16762/tui-firehose-readable` | agent-16762 | tui-firehose-readable | 3 | 1 | +293/-46 | **valuable, isolated** — firehose readability + word-wrap. File follow-up to cherry-pick. |
| `origin/wg/agent-16767/remove-verify-gates` | agent-16767 | remove-verify-gates | 7 | 1 | +45/-87 | **superseded** by `remove-validation-cli`. Archive. |
| `origin/wg/agent-220/migrate-docs-task` | agent-220 | migrate-docs-task | 6 | 2 | +163/-59 | **superseded** — docs migrated via the verify-removal cluster. Archive. |
| `origin/wg/agent-232/fix-tui-native` | agent-232 | fix-tui-native | 3 | 4 | +18/-36 | **valuable, isolated** — propagate executor type to PTY child env. File follow-up to cherry-pick. |
| `origin/wg/agent-300/make-evaluate-the` | agent-300 | make-evaluate-the | 7 | 3 | +190/-1095 | **superseded** by the broader verify-removal effort already on main. Archive. |
| `origin/wg/agent-322/fix-or-remove` | agent-322 | fix-or-remove | 1 | 1 | +7/-2 | **trivial** — `keep_recent_tool_results` test param fix. File follow-up to cherry-pick. |
| `origin/wg/agent-328/remove-stale-verify` | agent-328 | remove-stale-verify | 3 | 1 | +27/-28 | **superseded** by `remove-validation-cli`. Archive. |
| `origin/wg/agent-330/coordinator-should-auto` | agent-330 | coordinator-should-auto | 1 | 1 | +157 | **valuable, isolated** — auto-dispatch shell-mode tasks in coordinator. File follow-up. |
| `origin/wg/agent-332/hide-dot-prefixed` | agent-332 | hide-dot-prefixed | 4 | 1 | +191/-41 | **valuable, isolated** — hide `.` tasks from default `wg list`/`status`. File follow-up. |
| `origin/wg/agent-336/default-wg-add` | agent-336 | default-wg-add | 2 | 1 | +8/-18 | **superseded** by current `wg add` defaults. Archive. |
| `origin/wg/agent-337/agency-pipeline-should` | agent-337 | agency-pipeline-should | 10 | 1 | +762 | **possibly valuable** — constraint-fidelity lint for agency eval. File follow-up to evaluate (large new module — needs design review, not blind cherry-pick). |
| `origin/wg/agent-340/clarify-in-docs` | agent-340 | clarify-in-docs | 4 | 1 | +35/-1 | **superseded** — docs already clarify draft/publish on main. Archive. |
| `origin/wg/agent-364/skip-flip-evaluation` | agent-364 | skip-flip-evaluation | 1 | 1 | +155/-5 | **valuable, isolated** — skip-eval tag support. File follow-up to cherry-pick. |
| `origin/wg/agent-365/run-wg-gc` | agent-365 | run-wg-gc | 5 | 3 | +144/-46 | **failed task** — archival fix + test corrections. Status was `failed`. Archive (work was abandoned for a reason). |
<!-- audit-table-cat-b-end -->

### Disposition rule
For all 23 branches: **archive to `refs/archive/wg/agent-N/<task>` then delete on origin**. Branch tips remain reachable forever via the archive ref. The "valuable, isolated" / "possibly valuable" entries get follow-up tasks to selectively cherry-pick from archive — no work is lost, just deferred.

---

## Category (c): Empty / abandoned (0 branches)

Empty in the prior audit (autopoietic-reflect 309/312/318, nex-subtask 16728) — these have rolled into category (a) or no longer exist as remote branches.

---

## Non-agent branches (13, out-of-scope)

Left alone. These are upstream / human / pre-agent-system branches; reconcile decisions belong to the maintainers, not this audit.

```
nikete/main
nikete/vx-adapter
origin/fix-auto-task-edges
origin/fix-before-edges
origin/fix-output-section
origin/fix-toctou-race
origin/infra-fix-toctou
origin/provenance-and-executor-generalization
origin/show-live-token
origin/tui-disable-fade
origin/tui-pink-lifecycle
origin/worktree-chat-endpoint-flag
origin/worktree-fix-compaction-output-budget
```

---

## Reconcile actions executed (this commit)

Recorded for reproducibility. The script that performs these is captured inline in the commit body; recovery is `git push origin refs/archive/wg/agent-N/<task>:refs/heads/wg/agent-N/<task>`.

1. `git push origin main` — fast-forward; 40 squash-merge commits land on `origin/main`.
2. For each `origin/wg/agent-*` branch (119 total):
   - `git push origin <sha>:refs/archive/wg/agent-N/<task>` — preserve branch tip.
   - `git push origin :refs/heads/wg/agent-N/<task>` — delete from active listing.

After: `git branch -r --no-merged main` → 13 (non-agent only).

---

## Follow-ups (filed)

| Task ID | Title | Why |
|---------|-------|-----|
| `fix-wg-done` | Fix `wg done` auto-push origin main + delete origin agent branch on clean merge | **Load-bearing.** Closes the structural gap that produced 96 cat-(a) ghosts. Without this, the next audit round has the same shape. |
| `cherry-pick-valuable` | Cherry-pick valuable cat-(b) work from `refs/archive/*` (audit-2026-04-26) | Selectively land the 8 "valuable, isolated" cat-(b) branches (plus evaluate `agent-69/thin-wrapper-impl` and `agent-337/agency-pipeline-should`) from archive refs into main. |
| `fix-pre-existing` | Fix pre-existing test failure: `assigner_to_creator_signal_path_exists` | Test asserts Creator tier == Standard but config returns Premium. Pre-existing on main; surfaced during this audit's validation step. |

---

## Methodology

For each `origin/wg/agent-*` branch:
1. `merge-base = git merge-base origin/<branch> main`
2. `files = git diff --name-only $merge-base origin/<branch> | wc -l`
3. `commits = git rev-list --count $merge-base..origin/<branch>`
4. `squash = git log main --oneline --grep=<task_id>` (first match)
5. Conflict probe: `git merge-tree --no-messages main origin/<branch> | grep '<<<<<<<'`
6. Diff size: `git diff --shortstat main...origin/<branch>`
7. Cross-reference `wg show <task_id>` for task status (most are `NOT_FOUND` because the graph has rotated past these tasks).

**Classification rules** (same as 2026-04-25 audit):
- (a): squash commit by task-id found on `main` → already merged.
- (b): no squash on `main` AND files > 0 → truly unmerged.
- (c): files == 0 AND commits == 0 → empty.

**Reconcile rules** (new this audit):
- (a) → archive + delete (work preserved on main, branch ref preserved at `refs/archive/*`).
- (b) → archive + delete; valuable-isolated entries get follow-up cherry-pick tasks.
- (c) → delete (n/a this round).
- Non-agent branches → leave alone.
