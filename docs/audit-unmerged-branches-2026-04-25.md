# Audit: Unmerged Agent Branches — 2026-04-25

## Summary

| Category | Count | Description |
|----------|-------|-------------|
| **(a) Content already in main** | 54 | Squash-merge audit ghosts — work landed under a different commit hash |
| **(b) Truly unmerged** | 20 | Content diverges from main, no squash-merge found on main |
| **(c) Empty / abandoned** | 6 | No commits or file changes beyond merge-base |
| **Total** | 80 | All `wg/agent-*` branches (excluding `audit-unmerged-branches` itself) |

### Category (b) branches needing attention

These 20 branches have work that never landed in main. Most are marked `done` in the task graph, meaning the agent completed the work but the squash-merge never happened.

**High-value unmerged work (large scope, done tasks):**

| Branch | Task Status | Files | Summary |
|--------|-------------|-------|---------|
| `wg/agent-138/fix-complete-verify-v2` | done | 49 | Remove --verify surface entirely |
| `wg/agent-137/rip-verify-surface-v2` | done | 43 | Rip --verify surface |
| `wg/agent-120/run-5-task-smoke-v2` | done | 18 | Harbor smoke test tasks |
| `wg/agent-337/agency-pipeline-should` | done | 10 | Constraint-fidelity lint for agency evaluation |
| `wg/agent-300/make-evaluate-the` | done | 7 | Make .evaluate the terminal status determinant |
| `wg/agent-16767/remove-verify-gates` | NOT_FOUND | 7 | Deprecate --verify gates |
| `wg/agent-220/migrate-docs-task` | done | 6 | Migrate verification guidance to LLM-first |
| `wg/agent-141/nex-ux-600s` | done | 5 | Nex UX for slow local models |
| `wg/agent-365/run-wg-gc` | failed | 5 | Fix archival + emergency_compact tests |

**Failed/abandoned tasks with unmerged code:**

| Branch | Task Status | Files | Summary |
|--------|-------------|-------|---------|
| `wg/agent-365/run-wg-gc` | failed | 5 | Archival fix + test corrections — work exists but task failed |

---

## Category (a): Content Already in Main

Squash-merge audit ghosts. The branch content was squash-merged to main under a different commit hash. Safe to prune.

| Branch | Agent | Task ID | Task Status | Files Changed | Commits | Squash Commit |
|--------|-------|---------|-------------|---------------|---------|---------------|
| `wg/agent-8/research-agent-wg-awareness` | agent-8 | research-agent-wg-awareness | done | 1 | 1 | `d82d5f7d3` |
| `wg/agent-9/research-smoke-scope-criteria` | agent-9 | research-smoke-scope-criteria | done | 1 | 1 | `9fe4f3d37` |
| `wg/agent-10/research-tb-harness-wiring` | agent-10 | research-tb-harness-wiring | done | 1 | 1 | `0211627b6` |
| `wg/agent-11/research-wg-in-harbor` | agent-11 | research-wg-in-harbor | done | 1 | 1 | `5ef1d9bd0` |
| `wg/agent-22/fix-e-url` | agent-22 | fix-e-url | done | 1 | 1 | `201b69de6` |
| `wg/agent-25/synth-wg-nex-plan-of-attack` | agent-25 | synth-wg-nex-plan-of-attack | done | 1 | 1 | `78cd8de5f` |
| `wg/agent-44/write-harbor-config` | agent-44 | write-harbor-config | done | 1 | 1 | `9920dcf61` |
| `wg/agent-46/implement-nexevalagent-class` | agent-46 | implement-nexevalagent-class | done | 1 | 1 | `c979da53f` |
| `wg/agent-54/research-verify-deprecation` | agent-54 | research-verify-deprecation | done | 1 | 1 | `3b7a48a1c` |
| `wg/agent-57/design-verify-deprecation` | agent-57 | design-verify-deprecation | done | 1 | 1 | `443b17c75` |
| `wg/agent-112/fix-create-registry-v2` | agent-112 | fix-create-registry-v2 | done | 2 | 2 | `e6089d5ac` |
| `wg/agent-117/fix-wg-retry-clears-session` | agent-117 | fix-wg-retry-clears-session | done | 4 | 1 | `415a5fc9a` |
| `wg/agent-118/fix-expose-missing-v2` | agent-118 | fix-expose-missing-v2 | done | 2 | 1 | `f2576ae28` |
| `wg/agent-146/fix-log-pane` | agent-146 | fix-log-pane | done | 1 | 1 | `c08755d7f` |
| `wg/agent-148/make-wg-kill` | agent-148 | make-wg-kill | done | 5 | 1 | `bae7f5674` |
| `wg/agent-155/stop-auto-creating` | agent-155 | stop-auto-creating | done | 4 | 2 | `10658ee7d` |
| `wg/agent-158/surface-coordinator-errors` | agent-158 | surface-coordinator-errors | done | 6 | 1 | `3d8f56423` |
| `wg/agent-159/keep-mouse-mode` | agent-159 | keep-mouse-mode | done | 2 | 1 | `a41ee62d4` |
| `wg/agent-169/implement-wg-gc` | agent-169 | implement-wg-gc | done | 4 | 1 | `7b4274bf8` |
| `wg/agent-170/fix-atomic-worktree` | agent-170 | fix-atomic-worktree | done | 4 | 1 | `5cda52aa1` |
| `wg/agent-179/integrate-atomic-cleanup` | agent-179 | integrate-atomic-cleanup | done | 3 | 1 | `66bc71d23` |
| `wg/agent-211/research-shell-verify` | agent-211 | research-shell-verify | done | 1 | 1 | `000b4bd24` |
| `wg/agent-214/design-llm-based` | agent-214 | design-llm-based | done | 1 | 1 | `23da889b0` |
| `wg/agent-217/implement-llm-verification` | agent-217 | implement-llm-verification | done | 21 | 1 | `6e062aff6` |
| `wg/agent-234/fix-web-search` | agent-234 | fix-web-search | done | 1 | 1 | `adf1e2adc` |
| `wg/agent-237/fix-0-chat-panel` | agent-237 | fix-0-chat-panel | done | 2 | 1 | `9171d1e57` |
| `wg/agent-241/restore-msg-way` | agent-241 | restore-msg-way | done | 2 | 1 | `155f07ac7` |
| `wg/agent-243/fix-tui-tab` | agent-243 | fix-tui-tab | done | 2 | 1 | `e523a4f83` |
| `wg/agent-244/implement-model-endpoint` | agent-244 | implement-model-endpoint | done | 6 | 1 | `637b620b6` |
| `wg/agent-253/replace-new-coordinator` | agent-253 | replace-new-coordinator | done | 9 | 2 | `a51991f75` |
| `wg/agent-258/embedded-pty-chat` | agent-258 | embedded-pty-chat | done | 2 | 1 | `1cf794786` |
| `wg/agent-260/investigate-tui-coordinator` | agent-260 | investigate-tui-coordinator | done | 1 | 1 | `31d64231f` |
| `wg/agent-265/investigate-tui-pty` | agent-265 | investigate-tui-pty | done | 1 | 1 | `b3dd01c27` |
| `wg/agent-267/fix-paste-forwarding` | agent-267 | fix-paste-forwarding | failed | 3 | 2 | `93b81e0a4` |
| `wg/agent-270/fix-codex-resume` | agent-270 | fix-codex-resume | done | 1 | 1 | `e9a46c981` |
| `wg/agent-271/fix-claude-resume` | agent-271 | fix-claude-resume | done | 2 | 1 | `27a099641` |
| `wg/agent-277/fix-resume-all-executors` | agent-277 | fix-resume-all-executors | done | 4 | 2 | `426920029` |
| `wg/agent-285/fix-clap-endpoint-conflict` | agent-285 | fix-clap-endpoint-conflict | done | 1 | 1 | `b7159d9af` |
| `wg/agent-286/fix-paste-forwarding` | agent-286 | fix-paste-forwarding | failed | 1 | 1 | `93b81e0a4` |
| `wg/agent-288/add-incomplete-retryable` | agent-288 | add-incomplete-retryable | done | 33 | 1 | `6323d51a2` |
| `wg/agent-299/stronger-automatic-retry` | agent-299 | stronger-automatic-retry | done | 37 | 3 | `6323d51a2` |
| `wg/agent-16712/test-agent-worktree` | agent-16712 | test-agent-worktree | NOT_FOUND | 1 | 1 | `d5b3ab769` |
| `wg/agent-16722/tb-retest-smart-fanout` | agent-16722 | tb-retest-smart-fanout | NOT_FOUND | 4 | 1 | `d295965c4` |
| `wg/agent-16758/tb-research-fanout` | agent-16758 | tb-research-fanout | NOT_FOUND | 1 | 1 | `de0b72a24` |
| `wg/agent-16760/tb-impl-fanout-guidance` | agent-16760 | tb-impl-fanout-guidance | NOT_FOUND | 2 | 1 | `c98223898` |
| `wg/agent-16766/research-worktree-collision` | agent-16766 | research-worktree-collision | NOT_FOUND | 1 | 1 | `2521753f7` |
| `wg/agent-16770/fix-prevent-worktree` | agent-16770 | fix-prevent-worktree | NOT_FOUND | 6 | 1 | `58225803b` |
| `wg/agent-16772/fix-prevent-worktree` | agent-16772 | fix-prevent-worktree | NOT_FOUND | 5 | 1 | `58225803b` |
| `wg/agent-16773/fix-prevent-worktree` | agent-16773 | fix-prevent-worktree | NOT_FOUND | 4 | 1 | `58225803b` |
| `wg/agent-16774/fix-prevent-worktree` | agent-16774 | fix-prevent-worktree | NOT_FOUND | 4 | 1 | `58225803b` |
| `wg/agent-16777/fix-model-resolution` | agent-16777 | fix-model-resolution | NOT_FOUND | 1 | 1 | `e77d49d16` |
| `wg/agent-16779/implement-nex-interactive` | agent-16779 | implement-nex-interactive | NOT_FOUND | 6 | 1 | `d9ad4f34f` |
| `wg/agent-16784/implement-reprioritize-command` | agent-16784 | implement-reprioritize-command | NOT_FOUND | 4 | 1 | `adc0db3e3` |
| `wg/agent-16790/worktree-test-claude` | agent-16790 | worktree-test-claude | NOT_FOUND | 1 | 1 | `c7b083269` |

**Notes:**
- Branches with task status `failed` but content in main (agent-267, agent-286) indicate the task failed on one attempt but was retried by a different agent or approach that succeeded.
- Branches with task status `NOT_FOUND` are from a previous graph state that was archived/GC'd. Their content is confirmed landed via squash commit.
- Multiple `fix-prevent-worktree` branches (agents 16770–16774) all point to the same squash commit `58225803b`, indicating repeated attempts at the same fix.

---

## Category (b): Truly Unmerged

These branches have content that does not appear in main via squash-merge. Needs human review to decide: merge, cherry-pick, or discard.

| Branch | Agent | Task ID | Task Status | Files | Commits | Changed Files | Commit Summary |
|--------|-------|---------|-------------|-------|---------|---------------|----------------|
| `wg/agent-120/run-5-task-smoke-v2` | agent-120 | run-5-task-smoke-v2 | done | 18 | 1 | `terminal-bench/tasks/harbor-smoke/**` | NexEvalAgent endpoint resolution + Harbor smoke tasks |
| `wg/agent-134/unpin-claude-model` | agent-134 | unpin-claude-model | done | 3 | 1 | `src/config.rs`, `src/executor/native/tools/{delegate,summarize}.rs` | Unpin Claude model versions after outage |
| `wg/agent-137/rip-verify-surface-v2` | agent-137 | rip-verify-surface-v2 | done | 43 | 1 | `src/agency/prompt.rs`, `src/cli.rs`, `src/commands/{add,done,edit,evaluate,...}.rs` + 30 more | Rip --verify surface |
| `wg/agent-138/fix-complete-verify-v2` | agent-138 | fix-complete-verify-v2 | done | 49 | 1 | `src/agency/{prompt,starters}.rs`, `src/cli.rs`, `src/commands/**` + 35 more | Remove verify surface (refactor) |
| `wg/agent-141/nex-ux-600s` | agent-141 | nex-ux-600s | done | 5 | 1 | `src/cli.rs`, `src/commands/nex.rs`, `src/executor/native/{agent,tools/mod}.rs`, `src/main.rs` | Nex UX for slow local models |
| `wg/agent-16762/tui-firehose-readable` | agent-16762 | tui-firehose-readable | NOT_FOUND | 3 | 1 | `src/tui/viz_viewer/{event,render,state}.rs` | Human-readable firehose with word-wrap |
| `wg/agent-16767/remove-verify-gates` | agent-16767 | remove-verify-gates | NOT_FOUND | 7 | 1 | `CLAUDE.md`, `src/commands/{add,quickstart}.rs`, `src/commands/spawn/{context,execution}.rs`, `src/service/executor.rs` | Deprecate --verify gates |
| `wg/agent-220/migrate-docs-task` | agent-220 | migrate-docs-task | done | 6 | 2 | `.claude/skills/wg/SKILL.md`, `CLAUDE.md`, `README.md`, `docs/AGENT-GUIDE.md`, `src/commands/quickstart.rs`, `src/service/executor.rs` | Migrate verification guidance to LLM-first |
| `wg/agent-232/fix-tui-native` | agent-232 | fix-tui-native | done | 3 | 4 | `scripts/smoke/tui_auto_pty.sh`, `src/commands/done.rs`, `src/tui/viz_viewer/state.rs` | Propagate executor type to PTY + smart-verify routing |
| `wg/agent-300/make-evaluate-the` | agent-300 | make-evaluate-the | done | 7 | 3 | `CLAUDE.md`, `src/agency/prompt.rs`, `src/commands/{done,evaluate,list,show,status}.rs` | Make .evaluate the terminal status determinant |
| `wg/agent-322/fix-or-remove` | agent-322 | fix-or-remove | done | 1 | 1 | `tests/integration_context_pressure.rs` | Fix keep_recent_tool_results param in test |
| `wg/agent-328/remove-stale-verify` | agent-328 | remove-stale-verify | done | 3 | 1 | `CLAUDE.md`, `src/commands/{quickstart,spawn/context}.rs` | Replace --verify with .evaluate guidance |
| `wg/agent-329/cascade-abandonment-to` | agent-329 | cascade-abandonment-to | done | 1 | 1 | `src/commands/abandon.rs` | Cascade abandon to .assign-* pipeline tasks |
| `wg/agent-330/coordinator-should-auto` | agent-330 | coordinator-should-auto | done | 1 | 1 | `src/commands/service/coordinator.rs` | Auto-dispatch shell-mode tasks |
| `wg/agent-332/hide-dot-prefixed` | agent-332 | hide-dot-prefixed | done | 4 | 1 | `src/cli.rs`, `src/commands/{list,status}.rs`, `src/main.rs` | Hide dot-prefixed tasks from default output |
| `wg/agent-337/agency-pipeline-should` | agent-337 | agency-pipeline-should | done | 10 | 1 | `src/agency/{constraint_fidelity,mod,prompt,types}.rs`, `src/commands/{evaluate,show}.rs`, `tests/{integration_agency,integration_verify_first,prompt_snapshots,test_prompt_from_components}.rs` | Constraint-fidelity lint for agency eval |
| `wg/agent-340/clarify-in-docs` | agent-340 | clarify-in-docs | done | 4 | 1 | `CLAUDE.md`, `src/commands/{add,quickstart}.rs`, `src/main.rs` | Clarify draft/publish as orchestrator signal |
| `wg/agent-364/skip-flip-evaluation` | agent-364 | skip-flip-evaluation | done | 1 | 1 | `src/commands/eval_scaffold.rs` | Skip FLIP/eval for skip-eval tagged tasks |
| `wg/agent-365/run-wg-gc` | agent-365 | run-wg-gc | failed | 5 | 3 | `src/commands/{archive,service/coordinator}.rs`, `tests/{integration_context_pressure,integration_logging,integration_multiple_compaction}.rs` | Fix archival + emergency_compact tests |
| `wg/agent-49/run-5-task-smoke` | agent-49 | run-5-task-smoke | NOT_FOUND | 2 | 4 | `terminal-bench/nex-eval-qwen3-coder-30b-condition-a-config.json`, `terminal-bench/wg/adapter.py` | NexEvalAgent endpoint + config for Docker eval |

### Thematic grouping of unmerged work

**--verify deprecation cluster** (related branches pursuing the same goal via different approaches):
- `agent-137/rip-verify-surface-v2` (43 files) — aggressive removal
- `agent-138/fix-complete-verify-v2` (49 files) — full refactor removal
- `agent-16767/remove-verify-gates` (7 files) — gate deprecation
- `agent-220/migrate-docs-task` (6 files) — docs migration to LLM-first
- `agent-300/make-evaluate-the` (7 files) — .evaluate as terminal determinant
- `agent-328/remove-stale-verify` (3 files) — replace guidance only

These represent ~6 parallel attempts at the same strategic initiative (deprecating --verify in favor of .evaluate). Most likely, the final approach was chosen and merged manually or via a different mechanism, making these branches obsolete.

**TUI improvements:**
- `agent-16762/tui-firehose-readable` — firehose readability
- `agent-232/fix-tui-native` — executor type propagation

**Terminal-bench:**
- `agent-120/run-5-task-smoke-v2` — Harbor smoke test definitions
- `agent-49/run-5-task-smoke` — NexEvalAgent config

**Small targeted fixes:**
- `agent-134/unpin-claude-model` — model unpinning
- `agent-141/nex-ux-600s` — nex UX for slow models
- `agent-322/fix-or-remove` — test param fix
- `agent-329/cascade-abandonment-to` — abandon cascade
- `agent-330/coordinator-should-auto` — auto-dispatch shell tasks
- `agent-332/hide-dot-prefixed` — hide system tasks from output
- `agent-337/agency-pipeline-should` — constraint-fidelity lint
- `agent-340/clarify-in-docs` — doc clarification
- `agent-364/skip-flip-evaluation` — skip-eval tag support
- `agent-365/run-wg-gc` — archival + test fixes (failed task)

---

## Category (c): Empty / Abandoned

No file changes or commits beyond the merge-base with main. These branches were created but no work was committed.

| Branch | Agent | Task ID | Task Status | Notes |
|--------|-------|---------|-------------|-------|
| `wg/agent-16728/nex-subtask` | agent-16728 | nex-subtask | NOT_FOUND | Branch created, no work done |
| `wg/agent-309/autopoietic-reflect` | agent-309 | autopoietic-reflect | done | Task marked done but no branch commits |
| `wg/agent-312/autopoietic-reflect` | agent-312 | autopoietic-reflect | done | Task marked done but no branch commits |
| `wg/agent-318/autopoietic-reflect` | agent-318 | autopoietic-reflect | done | Task marked done but no branch commits |
| `wg/agent-336/default-wg-add` | agent-336 | default-wg-add | abandoned | Branch created, task abandoned |
| `wg/agent-381/stigmergic-merge-on-done` | agent-381 | stigmergic-merge-on-done | in-progress | Currently in progress, no commits yet |

**Notes:**
- `autopoietic-reflect` (agents 309, 312, 318): Three attempts at the same reflection task, all marked done without branch commits. The work likely happened via graph state changes or was a no-op task.
- `stigmergic-merge-on-done` (agent-381): Currently in-progress — expected to have no commits yet.

---

## Methodology

For each `wg/agent-*` branch:
1. Computed merge-base with `main`
2. Counted files changed and commits beyond merge-base (`git diff --name-only` and `git log --oneline`)
3. Searched `main`'s commit log for the task ID to detect squash-merges (`git log main --grep=<task-id>`)
4. Cross-referenced with task status via `wg show <task-id>`

**Classification rules:**
- **(a)**: Task ID found in `main`'s commit log → squash-merged
- **(b)**: Task ID NOT found in `main`'s log AND branch has file changes → truly unmerged
- **(c)**: No file changes and no commits beyond merge-base → empty

**No branches were deleted.** This is a read-only audit.
