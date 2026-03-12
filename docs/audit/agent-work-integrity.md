# Agent Work Integrity Audit: Stash Archaeology and Lost Work Recovery

**Date:** 2026-03-07
**Task:** audit-agent-work
**Scope:** All 36 git stashes, cross-referenced with task graph and git history

---

## Executive Summary

36 git stashes accumulated in the repository, many referencing tasks marked "done." This audit found:

- **Root cause: shared working tree without isolation.** All agents operate in a single git worktree. When multiple agents are dispatched concurrently, they encounter each other's uncommitted changes, leading to `git stash` as a conflict resolution mechanism.
- **No force pushes detected.** No evidence of destructive git operations.
- **7 unmerged feature branches** contain 139 collective commits never merged to main. The stashes are symptoms; the unmerged branches are the bigger risk.
- **~9 stashes contain substantial unique code** (1,000+ lines of insertions each), but most of this code was later re-implemented or committed through different paths.
- **3 stashes are pure Cargo.lock noise** (identical arboard crate addition).
- **2 stashes contain explicit evidence of cross-agent contamination** (stash messages: "other-agents-wip", "temp: stash other agents changes").

---

## Stash Inventory

### Legend

| Recovery Status | Meaning |
|---|---|
| SUPERSEDED | Code was later committed through different task/branch |
| IRRELEVANT | Cargo.lock noise or trivial changes |
| RECOVERABLE | Contains unique code not found in any branch |
| CONTAMINATION | Contains another agent's uncommitted work |

### Full Stash Table

| # | Branch | Task ID (from msg) | Task Status | Files | Key Content | Recovery Status |
|---|--------|-------------------|-------------|-------|-------------|----------------|
| 0 | (detached) | tui-viz-auto | Done | 10 | ExecMode enum, coordinator_agent refactor | SUPERSEDED (agency-executor-weight committed this) |
| 1 | safety-mandatory-validation | tui-dynamic-model-2 | Done | 4 | viz/state.rs +1264 lines | SUPERSEDED (extensive later TUI work) |
| 2 | fix-toctou-race | infra-compactor-mvp-2 | Done | 8 | graph.rs, native executor changes | SUPERSEDED (committed on fix-toctou-race) |
| 3 | fix-toctou-race | safety-self-healing | Done | 4 | provenance.rs, viz changes | SUPERSEDED (provenance.rs exists in HEAD, 326 lines) |
| 4 | fix-toctou-race | research-task-lifecycle | Done | 50 | cycle detection test, graph changes across 50 files | RECOVERABLE (massive diff, may contain unique patterns) |
| 5 | fix-toctou-race | human-telegram-notification-2 | Done | 4 | cli.rs, executor.rs additions | SUPERSEDED (small additions) |
| 6 | fix-toctou-race | infra-liveness-detection-2 | Done | 5 | done.rs, fail.rs refactors + Cargo.lock | SUPERSEDED |
| 7 | fix-toctou-race | (eval_timeout) | N/A | 4 | Cargo.lock + 5 lines cli/mod/main | IRRELEVANT |
| 8 | fix-toctou-race | (activity cmd WIP) | N/A | 3 | Cargo.lock + cli.rs, mod.rs | IRRELEVANT (self-described as unrelated) |
| 9 | fix-toctou-race | (eval_timeout) | N/A | 1 | Cargo.lock only | IRRELEVANT |
| 10 | fix-toctou-race | (eval_timeout) | N/A | 12 | fail/link/reschedule/resume refactors + viz | RECOVERABLE (command refactors may be unique) |
| 11 | fix-auto-task-edges | tui-detail-related | N/A | 2 | Cargo.lock + done.rs refactor | SUPERSEDED |
| 12 | fix-auto-task-edges | tui-detail-related | N/A | 1 | Cargo.lock only | IRRELEVANT |
| 13 | fix-toctou-race | tui-fix-insert | N/A | 8 | eval_timeout config + TUI state.rs +244 lines | RECOVERABLE (eval_timeout config, viz additions) |
| 14 | fix-auto-task-edges | fix-auto-task | N/A | 2 | Cargo.lock + resume.rs refactor | SUPERSEDED |
| 15 | fix-toctou-race | (eval_timeout) | N/A | 1 | Cargo.lock only | IRRELEVANT |
| 16 | fix-toctou-race | (eval_timeout) | N/A | 3 | Cargo.lock + fail.rs, coordinator.rs | SUPERSEDED |
| 17 | main | fix-missing-token | N/A | 3 | Cargo.lock + done.rs, coordinator.rs | SUPERSEDED |
| 18 | fix-auto-task-edges | fix-missing-token | N/A | 3 | Cargo.lock + done.rs, coordinator.rs | SUPERSEDED |
| 19 | fix-toctou-race | (eval_timeout) | N/A | 9 | add/done/edit/fail/link/reschedule/resume + coordinator | RECOVERABLE (massive refactors: 1434 insertions) |
| 20 | fix-toctou-race | tui-fix-insert | N/A | 13 | Same as 19 + eval_timeout config | RECOVERABLE (1335 insertions, overlaps with 19) |
| 21 | fix-toctou-race | toctou-phase1-core | Done | 2 | Cargo.lock + 1 line abandon.rs | IRRELEVANT |
| 22 | fix-toctou-race | (toctou-infra-wip) | N/A | 8 | parser.rs refactor, artifact/claim/wait changes | RECOVERABLE (parser.rs +37 lines unique) |
| 23 | tui-disable-fade | tui-disable-fade | N/A | 2 | Cargo.lock + editor_tests.rs +127 lines | SUPERSEDED |
| 24 | tui-disable-fade | tui-disable-fade | N/A | 24 | Massive parser + 20 command file refactors | RECOVERABLE (parser.rs +97 lines, systematic refactor) |
| 25 | tui-disable-fade | fix-missing-token | N/A | 3 | Cargo.lock + render.rs, state.rs | SUPERSEDED |
| 26 | fix-output-section | tui-disable-fade | N/A | 25 | stream_event.rs +400 lines, 20+ command files | RECOVERABLE (stream_event translator is unique) |
| 27 | fix-output-section | spark-v2-unified | N/A | 11 | eval.rs, config.rs, stream_event.rs, graph.rs | RECOVERABLE (notification dispatch, config additions) |
| 28 | fix-output-section | infra-unify-model | N/A | 11 | config.rs +110 lines, stream_event.rs, setup.rs | RECOVERABLE (config refactor is substantial) |
| 29 | fix-output-section | synthesis-report | N/A | 3 | Cargo.lock + render.rs, state.rs | SUPERSEDED |
| 30 | main | (cargo fmt) | N/A | 8 | graph.rs +262 lines (cycle restart logic) | SUPERSEDED (restart_on_failure exists in HEAD) |
| 31 | main | (other-agents-wip) | N/A | 7 | show.rs, spawn/execution.rs, viz changes | CONTAMINATION |
| 32 | coord-log-feature-v2 | (formatting) | N/A | 7 | add.rs, edit.rs, viz/render refactors | SUPERSEDED |
| 33 | coord-log-feature-v2 | (other agents) | N/A | 7 | cli.rs, list.rs, viz/ascii, viz/render | CONTAMINATION |
| 34 | coord-log-feature-v2 | (formatting) | N/A | 6 | edit.rs, list.rs, viz changes | SUPERSEDED |
| 35 | main | (mouse scroll) | N/A | 3 | viz_viewer event/render/state | SUPERSEDED |

### Summary Counts

| Category | Count | Stash IDs |
|----------|-------|-----------|
| SUPERSEDED | 16 | 0, 1, 2, 3, 5, 6, 11, 14, 16, 17, 18, 23, 25, 29, 30, 32, 34, 35 |
| IRRELEVANT | 6 | 7, 8, 9, 12, 15, 21 |
| RECOVERABLE | 9 | 4, 10, 13, 19, 20, 22, 24, 26, 27, 28 |
| CONTAMINATION | 2 | 31, 33 |

---

## Root Cause Analysis

### Root Cause 1: Shared Working Tree (PRIMARY)

**Evidence:**
- 16 of 36 stashes are on `fix-toctou-race`, a long-lived branch where multiple agents were dispatched concurrently
- Stash 33 message: "temp: stash other agents changes" -- agent explicitly describes stashing OTHER agents' work
- Stash 31 message: "other-agents-wip" -- same pattern on `main`
- No branch isolation or worktree separation in `src/commands/spawn/execution.rs` -- agents are spawned directly into the shared working directory

**Mechanism:** When Agent B starts on a branch where Agent A left uncommitted changes, Agent B runs `git stash` to get a clean working tree. Agent A's work is now trapped in a stash that neither agent will recover.

### Root Cause 2: Cargo.lock Divergence

**Evidence:**
- 22 of 36 stashes contain Cargo.lock changes (always +384/-12 lines)
- The diff is consistently the `arboard` clipboard crate and its dependency tree
- This crate was added on one branch but not others, causing every agent on the other branch to see a modified Cargo.lock

**Mechanism:** When a dependency is added on one branch, agents on other branches that run `cargo build` generate a different Cargo.lock. This shows up as an uncommitted change, forcing stash/discard behavior.

### Root Cause 3: Branch Accumulation Without Merging

**Evidence:**
- 7 unmerged feature branches exist with 139 total unmerged commits:
  - `fix-toctou-race`: 59 commits ahead of main
  - `fix-output-section`: 30 commits ahead
  - `safety-mandatory-validation`: 22 commits ahead
  - `fix-before-edges`: 11 commits ahead
  - `show-live-token`: 10 commits ahead
  - `fix-auto-task-edges`: 4 commits ahead
  - `tui-disable-fade`: 1 commit ahead
- Only 2 branches have ever been merged to main: `coord-log-feature` and `coord-log-feature-v2`
- Tasks are marked "done" based on commits to feature branches, but the code never reaches main

**Impact:** Even without stash issues, the merge gap means "done" tasks have code that's potentially unreachable if branches are deleted or forgotten.

### Root Cause 4: No Post-Task Commit Verification

**Evidence:**
- No code in `spawn/execution.rs` or the completion flow checks for uncommitted changes
- Agents can run `wg done` with a dirty working tree
- The prompt instructs agents to "commit and push" but this is advisory, not enforced

### Root Cause 5: Detached HEAD State

**Evidence:**
- Stash 0 was created on a detached HEAD (commit 6102279 on fix-toctou-race)
- This happens when agents check out specific commits instead of branches
- Work done in detached HEAD is especially vulnerable to loss

---

## Unmerged Branch Analysis

| Branch | Commits Ahead | Behind Main | Task Commits | Risk |
|--------|--------------|-------------|--------------|------|
| fix-toctou-race | 59 | 0 | 6+ task commits | HIGH: contains TOCTOU phases, self-healing, compactor, liveness, telegram work |
| fix-output-section | 30 | 0 | 4+ task commits | HIGH: stream_event refactor, notification dispatch |
| safety-mandatory-validation | 22 | 0 | 10+ task commits | HIGH: current HEAD, lots of recent work |
| fix-before-edges | 11 | 0 | Unknown | MEDIUM |
| show-live-token | 10 | 0 | Unknown | MEDIUM |
| fix-auto-task-edges | 4 | 0 | 3+ task commits | LOW: small scope |
| tui-disable-fade | 1 | 0 | 1 commit | LOW |
| tui-pink-lifecycle | 2 | 0 | Unknown | LOW |

All unmerged branches are ahead of main with 0 commits behind, meaning main is an ancestor of each. No merge conflicts expected.

There is also a prunable worktree at `/tmp/wg-toctou` (branch `infra-fix-toctou`).

---

## Recoverable Stashes: Detailed Assessment

### High Priority (unique code, task-relevant)

**Stash 4** (research-task-lifecycle, 50 files, DONE task)
- Contains cycle detection integration test and graph changes across 50 files
- `tests/integration_cycle_detection.rs` exists in HEAD (5202 lines) but stash version may contain additional patterns
- **Action:** Compare stash content with HEAD version; cherry-pick any missing test cases

**Stash 26** (stream_event.rs on fix-output-section, 25 files)
- Contains `translate_claude_event()` function: a Claude CLI JSONL-to-StreamEvent translator (+400 lines)
- Not present in HEAD's `stream_event.rs` (720 lines)
- **Action:** Review and potentially apply the Claude event translator

**Stash 27** (fix-output-section, 11 files)
- Contains notification dispatch additions, config changes, graph changes
- **Action:** Review for notification infrastructure not yet committed

**Stash 28** (fix-output-section, 11 files)
- Contains `config.rs` +110 lines (possibly model provider config)
- Contains setup.rs refactor
- **Action:** Review config additions for unique content

### Medium Priority (refactors that may improve code quality)

**Stash 19/20** (eval_timeout base, 9-13 files, ~1400 lines each)
- Systematic refactors of add/done/edit/fail/link/reschedule/resume commands
- These appear to be the same refactor at different stages
- **Action:** Check if refactors were later applied differently; may be obsolete

**Stash 24** (tui-disable-fade, 24 files)
- Parser.rs +97 lines, systematic command refactors
- **Action:** Check if parser improvements were applied through different task

**Stash 22** (toctou-infra-wip, 8 files)
- Parser.rs changes, artifact/claim/wait command changes
- **Action:** Review parser changes for TOCTOU improvements

### Low Priority

**Stash 10, 13** - Smaller subsets of the refactors in 19/20/24

---

## Recommendations

### 1. Enforce Worktree Isolation (Critical)

**Problem:** All agents share one working directory, causing mutual interference.

**Solution:** Modify `spawn_agent_inner()` in `src/commands/spawn/execution.rs` to:
1. Create a git worktree per agent: `git worktree add /tmp/wg-agent-{id} HEAD`
2. Run the agent process in the worktree directory
3. After task completion, merge the worktree branch and clean up

**Alternative (lighter):** At minimum, each agent should work on a dedicated branch created at spawn time, and commit before marking done.

### 2. Add Pre-Done Dirty Tree Check (High Priority)

**Problem:** Agents can mark tasks done with uncommitted changes.

**Solution:** In the `wg done` command handler (`src/commands/done.rs`):
```rust
// Before marking done, check for uncommitted changes
let output = Command::new("git")
    .args(["status", "--porcelain"])
    .output()?;
if !output.stdout.is_empty() {
    anyhow::bail!(
        "Cannot mark task done: uncommitted changes detected. \
         Commit your work first with 'git add -A && git commit'."
    );
}
```

### 3. Add Anti-Stash Instruction to Agent Prompts (High Priority)

**Problem:** Agents use `git stash` as a default conflict resolution mechanism.

**Solution:** Add to agent system prompt / CLAUDE.md:
```
NEVER run `git stash`. If you find uncommitted changes from another agent:
1. Check if changes conflict with your work
2. If no conflict: commit them with message "chore: commit prior agent WIP"
3. If conflict: report via `wg log` and `wg fail` with reason
```

### 4. Merge Accumulated Branches (Immediate)

**Problem:** 139 commits across 7 branches are unmerged to main.

**Action:** Since all branches are ahead of main with 0 behind:
```bash
git checkout main
git merge safety-mandatory-validation  # includes most recent work
# Then merge other branches or rebase them onto new main
```

### 5. Clean Up Stashes (After Recovery)

**Action:** After recovering any useful content from RECOVERABLE stashes:
```bash
git stash drop stash@{N}  # for each reviewed stash
# Or bulk: git stash clear  # after full review
```

### 6. Add Branch-Per-Task Convention

**Problem:** Multiple tasks commit to the same long-lived branch.

**Solution:** Each agent should create a branch named after its task ID:
```bash
git checkout -b task/{task_id}
# ... do work ...
git push -u origin task/{task_id}
```
The coordinator or a merge agent then integrates task branches.

### 7. Post-Task Commit Audit Hook

Add a post-completion hook that verifies:
- [ ] Agent's task ID appears in at least one commit message
- [ ] No uncommitted changes remain
- [ ] Branch has been pushed to remote
- [ ] Stash list hasn't grown during the task

---

## Appendix: Branch Topology

```
main (f0c7ec7)
  |
  +-- safety-mandatory-validation (90f3ce2, 22 ahead) <-- current HEAD
  |
  +-- fix-toctou-race (59 ahead, shares f0c7ec7 as merge-base)
  |     Contains: TOCTOU phases, self-healing, compactor, liveness, telegram
  |
  +-- fix-output-section (30 ahead)
  |     Contains: stream_event refactor, notification dispatch
  |
  +-- fix-before-edges (11 ahead)
  +-- show-live-token (10 ahead)
  +-- fix-auto-task-edges (4 ahead)
  +-- tui-disable-fade (1 ahead)
  +-- tui-pink-lifecycle (2 ahead)
```

All branches diverge from main at or near `f0c7ec7`. No branch is behind main.
