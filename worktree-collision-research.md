# Research: Worktree Collision Between wg and Claude Code

## Root Cause: Two Collision Mechanisms

### Mechanism 1: Agent-Initiated Worktree Escape (Primary)

Claude Code agents running inside wg worktrees call the `EnterWorktree` tool, creating a SECOND worktree in `.claude/worktrees/` and switching the session's working directory away from the wg worktree. This is the primary cause of "lost worktree" incidents.

**Evidence — every affected agent called EnterWorktree:**
- `agent-16720`: `EnterWorktree(name="nex-delegate")` → `.claude/worktrees/nex-delegate`
- `agent-16729`: `EnterWorktree(name="nex-config-bundles-recovery")` → `.claude/worktrees/nex-config-bundles-recovery`
- `agent-16733`: `EnterWorktree(name="agent-16733")` → `.claude/worktrees/agent-16733`
- `agent-16759`: `EnterWorktree(name="chat-endpoint-flag")` → `.claude/worktrees/chat-endpoint-flag`
- `agent-16763`: `EnterWorktree(name="fix-compaction-output-budget")` → `.claude/worktrees/fix-compaction-output-budget`

**Why Claude Code allows this:** The `EnterWorktree` tool's "Must not already be in a worktree" check only verifies Claude Code's own session state (was `EnterWorktree` already called in this session?), NOT whether the CWD is inside an existing git worktree. So agents already inside wg worktrees pass this check.

**Consequence chain:**
1. Agent starts in `.wg-worktrees/agent-XXXXX` on branch `wg/agent-XXXXX/task-id`
2. Agent calls `EnterWorktree` → session CWD moves to `.claude/worktrees/<name>` on branch `worktree-<name>`
3. All subsequent file edits and commits go to the wrong branch in the wrong directory
4. When agent exits, wg wrapper script checks for commits on `wg/agent-XXXXX/task-id` → finds none
5. Merge-back does nothing, work is stranded in `.claude/worktrees/<name>`
6. wg's cleanup removes the now-empty wg worktree directory (partially — `target/` may remain)

### Mechanism 2: Git Metadata Name Collision (agent-16733 only)

When the Claude Code worktree has the **same directory basename** as the wg worktree, git's internal worktree metadata gets corrupted.

**How git names worktree metadata:**
Git uses the worktree directory's basename as the key in `.git/worktrees/`:
- wg's `.wg-worktrees/agent-16733` → `.git/worktrees/agent-16733`
- Claude Code's `.claude/worktrees/agent-16733` → `.git/worktrees/agent-16733`

In testing, git auto-deduplicates by appending numbers (e.g., `agent-167331`). However, in agent-16733's case, the current state shows:
```
$ cat .git/worktrees/agent-16733/gitdir
/home/erik/workgraph/.claude/worktrees/agent-16733/.git
```

This points to Claude Code's worktree, meaning the wg worktree's `.git` pointer was severed. This may have happened through a sequence involving cleanup/prune between the two worktree creations.

## Additional Contributing Factors

### git gc --auto runs git worktree prune

Agent-16760 (which did NOT call `EnterWorktree`) showed `git gc --auto` running during its session:
```
Auto packing the repository in background for optimum performance.
```

Since git 2.15+, `git gc` includes `git worktree prune`, which removes entries whose directories no longer exist. If another agent's wg worktree directory was temporarily removed during cleanup, a concurrent `git gc --auto` would prune its metadata.

### wg's own cleanup calls git worktree prune

Every `remove_worktree` call in `src/commands/spawn/worktree.rs:107-111` runs `git worktree prune` after removing a specific worktree. This global prune affects ALL stale worktree entries, not just the one being removed. If any worktree directory was temporarily missing during this window, its entry would be pruned.

### Wrapper script cleanup is force-remove

The wg wrapper script (`src/commands/spawn/execution.rs:1251-1255`) runs:
```bash
git -C "$WG_PROJECT_ROOT" worktree remove --force "$WG_WORKTREE_PATH" 2>/dev/null
```
This is expected cleanup, but if the agent already moved to a Claude Code worktree, it removes the (now-abandoned) wg worktree. The `target/` directory may survive because it was recreated by cargo after the worktree content was removed.

## Verify Architecture Question

Per user message #3, the `--verify` system compounds the worktree problem:
1. Worktree gets damaged/abandoned
2. Verify runs `cargo test` in the broken worktree
3. Test fails (of course — the code isn't there)
4. 3 failures = circuit breaker auto-fails the task

Additionally, concurrent agents running `cargo test` cause resource contention (ports, temp files, lockfiles), leading to spurious verify failures even without worktree issues.

**Recommendation:** If verification is retained, it should run AFTER merge-back into the main branch, not in the agent's worktree. This avoids both the broken-worktree and resource-contention issues. However, the FLIP/eval pipeline may be a better fit for quality gating, since it operates post-merge and is designed for multi-agent workflows.

## Proposed Fix Approaches

### Fix 1: Prevent EnterWorktree in wg worktrees (Best — address root cause)

Set an environment variable when spawning agents in wg worktrees:
```rust
cmd.env("CLAUDE_CODE_DISABLE_WORKTREES", "1");
```

Or add to the agent's CLAUDE.md / prompt instructions:
```
CRITICAL: Do NOT use EnterWorktree. You are already in an isolated worktree managed by workgraph.
```

The env var approach is more reliable since it prevents the tool at the system level rather than relying on the LLM following instructions.

**Check if Claude Code respects any env var to disable `EnterWorktree`.** If not, the prompt-based approach is the fallback.

### Fix 2: Prompt injection (immediate mitigation)

Add to the wg agent prompt template (in `src/commands/spawn/execution.rs` or the executor context injection):
```
NEVER use the EnterWorktree or ExitWorktree tools. Your working directory is already isolated by workgraph. Using EnterWorktree will cause you to lose your worktree and all uncommitted work.
```

This is less reliable than Fix 1 but can be deployed immediately.

### Fix 3: Detect and prevent at wg level

Before merge-back, check if the agent changed CWD away from the wg worktree:
```bash
if [ "$(pwd)" != "$WG_WORKTREE_PATH" ]; then
    echo "[wrapper] WARNING: Agent changed CWD from worktree, attempting recovery"
    cd "$WG_WORKTREE_PATH"
fi
```

Also, verify the `.git` file still exists in the worktree before running merge-back. If it's gone, log the issue and skip the merge (the work was done elsewhere).

### Fix 4: Remove global git worktree prune

Change `remove_worktree` in `src/commands/spawn/worktree.rs` and `src/commands/service/worktree.rs` to NOT run `git worktree prune` after each removal. Instead, let git's natural gc handle pruning. This prevents collateral damage to other agents' worktree entries during cleanup.

The current code (spawn/worktree.rs:107-111):
```rust
// Prune stale worktree entries
let _ = Command::new("git")
    .args(["worktree", "prune"])
    .current_dir(project_root)
    .output();
```

Should be removed or made conditional on a "no other agents alive" check.

## Affected Code Paths

| File | Line | Issue |
|------|------|-------|
| `src/commands/spawn/worktree.rs:107-111` | `git worktree prune` after every removal | Global prune can damage other agents' worktrees |
| `src/commands/spawn/worktree.rs:38` | Pre-create cleanup calls `remove_worktree` | Triggers global prune |
| `src/commands/service/worktree.rs:208-218` | `git worktree prune` in service cleanup | Same issue |
| `src/commands/service/worktree.rs:672-678` | `git worktree prune` in orphan cleanup | Same issue |
| `src/commands/spawn/execution.rs:1251-1255` | Wrapper script force-removes worktree | Expected, but races with Claude Code |
| `src/commands/cleanup.rs:839-858` | Nightly cleanup runs `git worktree prune` | Acceptable (not during active agents) |

## Recommended Fix Priority

1. **Immediate:** Add "never use EnterWorktree" to agent prompt (Fix 2) — deploy in minutes
2. **Short-term:** Set env var to disable Claude Code worktrees (Fix 1) — if supported
3. **Short-term:** Remove global `git worktree prune` from per-agent cleanup (Fix 4)
4. **Medium-term:** Add worktree integrity check to wrapper script (Fix 3)
5. **Consider:** Move verify to post-merge or replace with FLIP/eval
