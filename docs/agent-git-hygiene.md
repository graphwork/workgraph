# Agent Git Hygiene Guidelines

Rules for agents working in a shared repository. These exist because multiple agents operate concurrently in one working tree — careless git operations cause lost work.

## The Rules

### 1. Surgical staging only

**Never** use `git add -A`, `git add .`, or `git add --all`.

Always list specific files you modified:
```bash
git add src/foo.rs src/bar.rs
```

Before committing, verify your staged changes:
```bash
git diff --cached --name-only
```
Every file listed should be one YOU modified for YOUR task.

### 2. Commit early, commit often

Don't accumulate a large uncommitted delta. Commit after each logical unit of work:
- After implementing a function
- After fixing a test
- After completing a subtask

Small commits are recoverable. Large uncommitted deltas are not.

### 3. Never stash

**Do not run `git stash`** — ever. If you see uncommitted changes that aren't yours:
- Leave them alone
- Work around them
- If they conflict with your work, report via `wg log` and `wg fail`

Stashing other agents' work traps it in a stash that nobody will recover.

### 4. Never force push

No `git push --force` or `git push -f`. If push fails, diagnose and fix — don't force.

### 5. Don't touch others' changes

If `git status` shows modified files you didn't touch:
- **Do not stage them**
- **Do not commit them**
- **Do not stash them**
- **Do not reset them**

They belong to another agent. Leave them alone.

### 6. Handle locks gracefully

Concurrent agents cause lock contention:
- `.git/index.lock` — another git operation is running. Wait 2-3 seconds, retry.
- `target/` file locks — `cargo build` uses file locks. Concurrent builds are safe but slow. Retry on failure.
- `.workgraph/` locks — workgraph uses file locking internally. Retry after a brief wait.

Don't panic on lock errors. Don't delete lock files. Just retry.

### 7. Check before committing

Before every commit:
```bash
git diff --cached --name-only   # verify only your files are staged
git diff --cached               # review the actual changes
```

If you see files you didn't modify, unstage them:
```bash
git restore --staged <file>
```

## Post-Task Checklist

Before running `wg done`, verify:
- [ ] All your changes are committed (no relevant unstaged changes)
- [ ] You didn't stash anything (`git stash list` unchanged)
- [ ] You only committed files you modified
- [ ] Your commits have descriptive messages with the task ID
- [ ] You pushed to remote

## Root Causes This Prevents

Based on the [agent work integrity audit](audit/agent-work-integrity.md):
- **Stash accumulation**: 36 stashes from agents stashing each other's work
- **Cross-agent contamination**: Agents committing files they didn't modify
- **Lost work**: Large uncommitted deltas lost when another agent clobbers them
- **Detached HEAD work**: Commits on detached HEAD that can't be recovered
