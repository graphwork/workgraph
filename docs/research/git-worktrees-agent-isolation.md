# Git Worktrees for Per-Agent Branch Isolation

**Task:** research-git-worktrees
**Date:** 2026-03-07
**Builds on:** docs/WORKTREE-ISOLATION.md (2026-02-28, Rust-specific), docs/agent-git-hygiene.md

---

## Executive Summary

Git worktrees are the pragmatic fast-path solution to shared-working-tree conflicts between concurrent agents. Each agent gets a full checkout on its own branch — complete filesystem isolation with zero dependency on external tools. This report covers the generic (language-agnostic) design, concrete run.sh integration, build isolation patterns, merge strategy, disk analysis, and edge cases.

**Verdict: Feasible and recommended.** Worktree creation is near-instant (~70ms measured), disk cost is the source tree size per agent (12MB for this repo), and the git primitives are mature and concurrent-safe. The main challenge is build artifact isolation, which requires per-language configuration but follows a generic pattern.

---

## 1. Git Worktree Mechanics

### Core Commands

| Operation | Command | Notes |
|-----------|---------|-------|
| Create | `git worktree add <path> -b <branch> [start-point]` | New branch from start-point (default HEAD) |
| Create (detached) | `git worktree add --detach <path> <commit>` | No branch, detached HEAD |
| List | `git worktree list [--porcelain]` | `--porcelain` for machine parsing |
| Remove | `git worktree remove <path>` | Fails if uncommitted changes |
| Force remove | `git worktree remove --force <path>` | Discards uncommitted changes |
| Lock | `git worktree lock <path> [--reason <text>]` | Prevents pruning |
| Unlock | `git worktree unlock <path>` | Re-enables pruning |
| Prune | `git worktree prune` | Removes stale admin entries |

### What Each Worktree Gets

- **Own working tree**: complete file checkout, independent of all other worktrees
- **Own HEAD**: tracks its own branch
- **Own index**: `git add`/`git commit` are fully isolated
- **Own per-worktree refs**: `refs/worktree/` namespace, `MERGE_HEAD`, `REBASE_HEAD`, etc.

### What Is Shared

- **Object store**: all commits, trees, blobs — no duplication
- **Refs** (branches, tags): visible from all worktrees
- **Config**: `.git/config` is shared
- **Hooks**: `.git/hooks/` is shared

### Hard Constraints

1. **One branch per worktree.** Git enforces this — `git worktree add` fails if the branch is already checked out elsewhere. This is a feature: it prevents two agents from accidentally working on the same branch.

2. **Worktree path must not exist.** The target directory is created by git; it must not pre-exist.

3. **Garbage collection.** `git worktree prune` removes admin entries for worktrees whose directories no longer exist. `git gc` respects worktree refs and won't collect reachable objects. No special GC configuration needed.

### Measured Performance (this repo, git 2.43.0)

| Metric | Value |
|--------|-------|
| Worktree creation time | 68ms |
| Worktree disk usage (source) | 12MB |
| Main repo source size | 16MB |
| Admin files per worktree | ~50KB in `.git/worktrees/<name>/` |

---

## 2. Build Tool Isolation (Generic Design)

### The Problem

Each worktree is a full source checkout, but build tools often use a project-relative output directory (`target/`, `node_modules/`, `build/`, `.venv/`, etc.). When multiple worktrees share a build directory, they fight over locks and invalidate each other's caches.

### Generic Pattern

Every build tool needs two things configured per worktree:

1. **Output directory**: where compiled/generated artifacts go
2. **Cache sharing**: whether and how to share cached artifacts across worktrees

The workgraph integration should:

1. **Detect** the project type (look for `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, `Makefile`, etc.)
2. **Set environment variables** or config that redirects build output per worktree
3. **Optionally seed** the worktree's build directory from the main tree's cache

### Per-Language Notes

#### Rust (Cargo)

```bash
export CARGO_TARGET_DIR="$WORKTREE_PATH/.wg-target"
# Or copy main target for warm cache:
# cp -al "$PROJECT_ROOT/target" "$WORKTREE_PATH/.wg-target"  # hardlink = instant, 0 extra disk
```

- `CARGO_TARGET_DIR` fully isolates build output
- Cargo uses file locks internally — a shared target dir works but causes lock contention
- **Recommended**: per-worktree target, seeded with hardlinks (`cp -al`) from main for warm cache
- `sccache` can further deduplicate compilation across worktrees

#### Node.js (npm/yarn/pnpm)

```bash
# Option A: install per worktree (cleanest)
cd "$WORKTREE_PATH" && npm ci

# Option B: symlink node_modules (fast but fragile)
ln -s "$PROJECT_ROOT/node_modules" "$WORKTREE_PATH/node_modules"
```

- `node_modules` is project-relative by default — each worktree gets its own if you run `npm install`
- pnpm's content-addressable store already deduplicates across projects
- For monorepos with workspaces, each worktree needs its own install
- **Recommended**: `npm ci` per worktree for correctness; pnpm if disk is a concern

#### Python (pip/venv/poetry)

```bash
# venvs are naturally per-directory
cd "$WORKTREE_PATH" && python -m venv .venv && source .venv/bin/activate && pip install -e .

# Or share a single venv if deps don't change between agents:
ln -s "$PROJECT_ROOT/.venv" "$WORKTREE_PATH/.venv"
```

- Virtual environments are path-encoded — sharing is possible but brittle
- Poetry/pipenv lockfiles in the worktree ensure reproducibility
- **Recommended**: shared venv via symlink if deps are stable; per-worktree venv if agents modify dependencies

#### Go

```bash
# Go module cache is per-user ($GOPATH/pkg/mod), not per-project
# Build cache is per-user ($HOME/.cache/go-build)
# No special configuration needed!
```

- Go's design already handles this correctly
- Build output goes to `$GOPATH/bin` or a local `./bin`, not a project-relative dir
- **Recommended**: no action needed; Go just works with worktrees

#### C/C++ (Make/CMake)

```bash
# CMake: use out-of-source builds (already best practice)
mkdir "$WORKTREE_PATH/build" && cd "$WORKTREE_PATH/build" && cmake ..

# Make: set BUILD_DIR if the Makefile supports it
make BUILD_DIR="$WORKTREE_PATH/build"
```

- `ccache` deduplicates compilation across worktrees automatically
- **Recommended**: out-of-source builds + ccache

#### Java (Maven/Gradle)

```bash
# Maven: default target/ is project-relative — already isolated per worktree
# Gradle: build/ is project-relative — already isolated per worktree
# Both share ~/.m2/repository or ~/.gradle/caches (user-level cache)
```

- Build caches are user-level by default — natural deduplication
- **Recommended**: no special configuration needed

### Generic Integration Contract

The worktree setup hook should expose:

```bash
# Set by workgraph before agent starts
WG_WORKTREE_PATH=/path/to/worktree      # agent's working directory
WG_PROJECT_ROOT=/path/to/main/repo       # original repo root
WG_AGENT_ID=agent-XXXX                   # agent identifier
WG_BRANCH=wg/agent-XXXX/task-id          # agent's branch

# Optional: project-type-specific overrides
# These could be set by a .workgraph/worktree-setup.sh hook
CARGO_TARGET_DIR="$WG_WORKTREE_PATH/.wg-target"
```

Projects can provide a `.workgraph/worktree-setup.sh` script for custom build isolation:

```bash
#!/bin/bash
# .workgraph/worktree-setup.sh — run after worktree creation
# $1 = worktree path, $2 = project root

WORKTREE="$1"
ROOT="$2"

# Example: Rust project with warm cache
if [ -f "$ROOT/Cargo.toml" ]; then
    export CARGO_TARGET_DIR="$WORKTREE/.wg-target"
    if [ -d "$ROOT/target" ]; then
        cp -al "$ROOT/target" "$WORKTREE/.wg-target" 2>/dev/null || true
    fi
fi

# Example: Node project
if [ -f "$ROOT/package.json" ]; then
    (cd "$WORKTREE" && npm ci --silent 2>/dev/null) || true
fi
```

---

## 3. run.sh Integration Design

### Current Lifecycle (no worktrees)

```
coordinator claims task
  → spawn_agent_inner() writes run.sh
  → run.sh launches agent process in repo root
  → agent works in shared tree
  → agent calls wg done / wg fail (or wrapper detects exit)
  → cleanup
```

### Proposed Lifecycle (with worktrees)

```
coordinator claims task
  → spawn_agent_inner() creates worktree
  → spawn_agent_inner() writes run.sh with worktree env
  → run.sh launches agent process in WORKTREE directory
  → agent works in isolated worktree
  → agent commits on its branch, calls wg done
  → wrapper merges branch back to main
  → wrapper removes worktree
  → cleanup
```

### Concrete run.sh Sketch

```bash
#!/bin/bash
TASK_ID='implement-foo'
AGENT_ID='agent-7500'
OUTPUT_FILE='/path/to/.workgraph/agents/agent-7500/output.log'
PROJECT_ROOT='/home/user/myproject'

# --- Worktree Setup ---
WG_BRANCH="wg/${AGENT_ID}/${TASK_ID}"
WG_WORKTREE_PATH="${PROJECT_ROOT}/.wg-worktrees/${AGENT_ID}"

# Create worktree from current HEAD of main working tree
git -C "$PROJECT_ROOT" worktree add "$WG_WORKTREE_PATH" -b "$WG_BRANCH" HEAD 2>> "$OUTPUT_FILE"
if [ $? -ne 0 ]; then
    echo "[wrapper] Failed to create worktree" >> "$OUTPUT_FILE"
    wg fail "$TASK_ID" --reason "Worktree creation failed"
    exit 1
fi

# Symlink shared .workgraph so wg CLI works from worktree
ln -s "$PROJECT_ROOT/.workgraph" "$WG_WORKTREE_PATH/.workgraph"

# Run project-specific build setup if it exists
if [ -x "$PROJECT_ROOT/.workgraph/worktree-setup.sh" ]; then
    source "$PROJECT_ROOT/.workgraph/worktree-setup.sh" "$WG_WORKTREE_PATH" "$PROJECT_ROOT"
fi

# Export env vars for the agent
export WG_WORKTREE_PATH
export WG_BRANCH
export WG_PROJECT_ROOT="$PROJECT_ROOT"
export WG_AGENT_ID="$AGENT_ID"

# Allow nested Claude Code sessions
unset CLAUDECODE
unset CLAUDE_CODE_ENTRYPOINT

# --- Run Agent ---
cd "$WG_WORKTREE_PATH"
timeout --signal=TERM --kill-after=30 1800 \
    claude --print --verbose --output-format stream-json \
    --dangerously-skip-permissions --model opus \
    < "/path/to/prompt.txt" \
    > >(tee -a "$OUTPUT_FILE") 2>> "$OUTPUT_FILE"
EXIT_CODE=$?

# --- Post-Agent: Merge Back ---
TASK_STATUS=$(wg show "$TASK_ID" --json 2>/dev/null | grep -o '"status": *"[^"]*"' | head -1 | sed 's/.*"status": *"//;s/"//' || echo "unknown")

if [ "$TASK_STATUS" = "in-progress" ]; then
    if [ $EXIT_CODE -eq 0 ]; then
        wg done "$TASK_ID" 2>> "$OUTPUT_FILE"
    elif [ $EXIT_CODE -eq 124 ]; then
        wg fail "$TASK_ID" --reason "Agent exceeded hard timeout" 2>> "$OUTPUT_FILE"
    else
        wg fail "$TASK_ID" --reason "Agent exited with code $EXIT_CODE" 2>> "$OUTPUT_FILE"
    fi
fi

# Merge worktree branch to main (only if task succeeded)
TASK_STATUS=$(wg show "$TASK_ID" --json 2>/dev/null | grep -o '"status": *"[^"]*"' | head -1 | sed 's/.*"status": *"//;s/"//' || echo "unknown")

if [ "$TASK_STATUS" = "done" ]; then
    # Check if agent made any commits
    COMMITS=$(git -C "$PROJECT_ROOT" log --oneline "HEAD..$WG_BRANCH" 2>/dev/null | wc -l)
    if [ "$COMMITS" -gt 0 ]; then
        cd "$PROJECT_ROOT"
        git merge --squash "$WG_BRANCH" 2>> "$OUTPUT_FILE"
        MERGE_EXIT=$?

        if [ $MERGE_EXIT -ne 0 ]; then
            git merge --abort 2>/dev/null
            echo "[wrapper] Merge conflict — marking for retry" >> "$OUTPUT_FILE"
            # Re-open the task so coordinator can retry
            wg fail "$TASK_ID" --reason "Merge conflict on integration" 2>> "$OUTPUT_FILE"
        else
            git commit -m "$(cat <<EOF
feat: ${TASK_ID} (${AGENT_ID})

Squash-merged from branch ${WG_BRANCH}
EOF
)" 2>> "$OUTPUT_FILE"
            echo "[wrapper] Merged ${WG_BRANCH} to main" >> "$OUTPUT_FILE"
        fi
    else
        echo "[wrapper] No commits on ${WG_BRANCH}, nothing to merge" >> "$OUTPUT_FILE"
    fi
fi

# --- Cleanup Worktree ---
git -C "$PROJECT_ROOT" worktree remove --force "$WG_WORKTREE_PATH" 2>/dev/null
git -C "$PROJECT_ROOT" branch -D "$WG_BRANCH" 2>/dev/null
echo "[wrapper] Cleaned up worktree" >> "$OUTPUT_FILE"

exit $EXIT_CODE
```

### Key Design Decisions in the Sketch

1. **Worktree created BEFORE agent starts** — agent's cwd is set to the worktree
2. **`.workgraph` symlinked** — all wg CLI commands work transparently
3. **Merge happens in wrapper AFTER agent exits** — agent doesn't need to merge
4. **Squash merge by default** — clean one-commit-per-task history
5. **Conflict = auto-fail for retry** — coordinator re-dispatches from updated HEAD
6. **Force-remove on cleanup** — even if agent left uncommitted changes

### Interaction with WG_AGENT_ID

`WG_AGENT_ID` already exists and is set by the coordinator. The worktree integration adds:

| Env Var | Existing? | Purpose |
|---------|-----------|---------|
| `WG_AGENT_ID` | Yes | Agent identifier (unchanged) |
| `WG_WORKTREE_PATH` | New | Absolute path to agent's worktree |
| `WG_BRANCH` | New | Agent's git branch name |
| `WG_PROJECT_ROOT` | New | Main repo root (for merge-back) |

The agent itself doesn't need to know it's in a worktree — it just works in its cwd as usual. The env vars are for the wrapper script and any tooling that needs to reference the main repo.

---

## 4. Disk Space Analysis

### Per-Worktree Cost

| Component | Size | Notes |
|-----------|------|-------|
| Source files | ~repo size | Full checkout; 12MB for this repo |
| Git admin | ~50KB | `.git/worktrees/<name>/` |
| Build artifacts | Varies widely | See below |

### Build Artifact Cost by Language

| Language | Typical Build Dir Size | Strategy |
|----------|----------------------|----------|
| Rust | 1-5 GB (`target/`) | Hardlink seed (`cp -al`), or sccache |
| Node.js | 100MB-1GB (`node_modules/`) | pnpm (dedup), or fresh install |
| Python | 10-100MB (`.venv/`) | Shared symlink or per-worktree |
| Go | 0 (user-level cache) | No extra cost |
| Java | 50-500MB (`target/`/`build/`) | Per-worktree (user-level cache deduplicates) |
| C/C++ | 100MB-10GB | ccache (user-level dedup) |

### Scaling: N Concurrent Agents

For this repo (Rust, 12MB source, ~2GB target):

| Agents | Source Cost | Build Cost (naive) | Build Cost (hardlink seed) |
|--------|-----------|-------------------|---------------------------|
| 1 | 12 MB | 2 GB | 2 GB |
| 3 | 36 MB | 6 GB | ~2.1 GB (shared blocks) |
| 5 | 60 MB | 10 GB | ~2.3 GB |
| 10 | 120 MB | 20 GB | ~3 GB |

Hardlink seeding (`cp -al target/ worktree-target/`) creates zero-cost copies that only diverge as the agent modifies files. For most agents (modifying a few crates), the incremental disk cost is small.

### Cleanup Strategy

- **On task completion**: worktree removed immediately (run.sh wrapper)
- **On service restart**: `git worktree prune` removes stale entries; scan `.wg-worktrees/` for orphans
- **Periodic**: coordinator triage loop can check for worktrees older than N hours
- **Manual**: `wg worktree clean` command for operator use

---

## 5. Merge Strategy Recommendation

### Options

| Strategy | Command | History | Conflict Recovery |
|----------|---------|---------|-------------------|
| Squash merge | `git merge --squash` | 1 commit per task | Easy: abort + retry |
| Merge commit | `git merge --no-ff` | Preserves agent commits | Easy: revert merge |
| Rebase | `git rebase main` then fast-forward | Linear | Complex: rebase conflicts |

### Recommendation: Squash Merge (Default)

**Why squash:**

1. **Clean history**: one commit per task on main, easy to correlate with task graph
2. **Simple conflict handling**: `git merge --abort` cleanly undoes a failed squash
3. **Agent commits preserved on branch**: if debugging is needed, the branch exists until cleanup
4. **No rebase complexity**: rebase can fail mid-way with multiple conflict points

**When to use merge commit instead:**

- When preserving intermediate agent commits matters (audit trails)
- When tasks are large and the squash would be hard to review

**When to use rebase:**

- When linear history is a hard requirement (rare with automated agents)

### Conflict Handling Flow

```
Agent completes task → wrapper attempts squash merge
  ├─ Success → commit, clean up worktree, done
  └─ Conflict → abort merge, mark task failed with "merge conflict"
       → coordinator re-dispatches task
       → new agent starts from updated HEAD (includes other agents' merged work)
       → retry succeeds (conflict resolved by working from fresh state)
```

### Prevention: Graph-Level Conflict Avoidance

The best merge strategy is avoiding conflicts in the first place:

1. **Same files = sequential edges.** Tasks modifying the same files must have `--after` dependencies.
2. **File scope in task descriptions.** Include "Files: src/foo.rs, src/bar.rs" so the orchestrator can detect overlaps.
3. **Pre-merge check.** Before merging, `git diff --name-only HEAD...$BRANCH` reveals which files changed — if another merge just landed touching the same files, auto-retry immediately.

---

## 6. Edge Cases

### Agent Killed Mid-Work

- **Symptom**: worktree directory exists, agent process gone, task still "in-progress"
- **Detection**: triage loop checks PID liveness; if dead, worktree is orphaned
- **Recovery**:
  1. Check if agent made commits on its branch (`git log HEAD..$BRANCH`)
  2. If yes: optionally attempt merge (recovers partial work) or discard
  3. `git worktree remove --force` (discards uncommitted worktree changes)
  4. `git branch -D $BRANCH`
  5. Re-open task for retry

### Service Restart

- **On startup**: `git worktree list --porcelain` discovers all worktrees
- **For each worktree matching `.wg-worktrees/agent-*`**:
  - Check if the agent is still running (PID file or process check)
  - If not: clean up (same as killed-agent flow)
- **Also run**: `git worktree prune` to clean stale admin entries

### Worktree Left Behind (Disk Pressure)

- Coordinator triage can enforce a max worktree age (e.g., 2 hours)
- `wg worktree clean` command removes all worktrees not associated with running agents
- `.wg-worktrees/` should be in `.gitignore` so it doesn't pollute status

### The `.workgraph` Directory

**Should it be shared or per-worktree?** Shared, via symlink.

- `.workgraph/` contains task state, agent registry, config — all agents must see the same data
- Symlink from worktree to main repo's `.workgraph/` is the simplest solution
- Fallback: `WG_DIR` env var pointing to absolute path (requires minor CLI change)
- Since `.workgraph/` is in `.gitignore`, it doesn't appear in the worktree's git status

### Submodules

- `git worktree add` **does NOT initialize submodules** in the new worktree
- If the repo uses submodules, the worktree setup must run:
  ```bash
  cd "$WORKTREE_PATH" && git submodule update --init --recursive
  ```
- This can be slow for repos with many submodules
- Add to the generic `worktree-setup.sh` hook:
  ```bash
  if [ -f "$WORKTREE/.gitmodules" ]; then
      (cd "$WORKTREE" && git submodule update --init --recursive)
  fi
  ```

### Git LFS

- LFS objects are stored in `.git/lfs/` which is shared across worktrees
- Worktree checkout automatically smudges LFS pointers (downloads content)
- **Cost multiplier**: LFS files are checked out (smudged) per worktree, but the LFS cache is shared
- For repos with large LFS files, worktree creation is slower due to smudge filters
- Mitigation: `GIT_LFS_SKIP_SMUDGE=1 git worktree add ...` then selectively fetch needed files
- Most agent tasks won't need binary assets — lazy LFS fetch is fine

### Concurrent `git worktree add`

- Git uses lockfiles for worktree operations — concurrent adds are safe
- Tested: two worktree adds in sequence complete without error
- True parallel adds (same millisecond) might contend on `.git/worktrees/` lock — retry on failure

### Agent Creates Files Outside Worktree

- Agent runs in worktree cwd — relative paths resolve inside worktree
- `wg` commands use the symlinked `.workgraph` — artifacts are recorded relative to project root
- Risk: agent uses absolute paths from task description pointing to main repo
- Mitigation: agent prompt should not include absolute paths; use relative paths only

---

## 7. Comparison: Git Worktrees vs. GitButler Virtual Branches

This section is for synthesis with the sibling GitButler research task.

### Feature Comparison

| Dimension | Git Worktrees | GitButler Virtual Branches |
|-----------|--------------|---------------------------|
| **Mechanism** | Full filesystem checkout per branch | Single working tree, virtual branch tracking |
| **Isolation level** | Complete: separate files, index, HEAD | Partial: shared files, virtual ownership |
| **Disk cost** | Source tree size per worktree + build artifacts | Zero extra (one working tree) |
| **Build isolation** | Natural (separate directories) | None (shared working tree) |
| **Dependencies** | Git only (built-in since 2.5, 2015) | GitButler binary + Tauri runtime |
| **Merge model** | Standard git merge/rebase | GitButler-specific virtual merge |
| **Conflict detection** | At merge time (post-work) | Real-time (during work) |
| **Maturity** | Very mature, core git feature | Young project, evolving API |
| **Automation** | Fully scriptable (CLI) | GUI-first, CLI nascent |
| **Concurrent builds** | Yes (separate dirs) | No (single working tree) |

### When Worktrees Win

- **Build isolation required**: each agent needs to compile/test independently
- **No external dependencies**: works on any system with git >= 2.5
- **Simple mental model**: it's just another checkout — standard git applies
- **CI/server environments**: no GUI dependency, pure CLI
- **Long-running agents**: complete isolation means no interference at any point

### When GitButler Could Win

- **Disk-constrained environments**: zero extra disk for source files
- **Real-time conflict visibility**: know immediately if two agents touch the same hunk
- **Fast context switching**: no worktree creation/removal overhead
- **Human + agent collaboration**: user can see all virtual branches in the GUI

### Can They Complement Each Other?

Potentially:

1. **Worktrees for execution, GitButler for visibility**: agents work in worktrees, GitButler provides a dashboard of virtual branches over the main tree
2. **GitButler for planning, worktrees for doing**: use GitButler's conflict detection to inform task scheduling (which tasks can parallelize), then use worktrees for actual execution
3. **GitButler as worktree alternative for lightweight tasks**: research/doc tasks that don't need build isolation could use virtual branches; code tasks use worktrees

**Recommendation**: Worktrees are the right default for agent execution because build isolation is non-negotiable for code tasks. GitButler is worth evaluating as a supplementary tool for conflict visibility and lightweight tasks, but it should not be a required dependency.

---

## 8. Summary of Recommendations

| Decision | Recommendation |
|----------|---------------|
| Isolation mechanism | Git worktrees (native, no dependencies) |
| Worktree location | `.wg-worktrees/<agent-id>/` inside repo, added to `.gitignore` |
| Branch naming | `wg/<agent-id>/<task-id>` |
| Shared state | Symlink `.workgraph` from worktree to main repo |
| Build isolation | Generic `worktree-setup.sh` hook, per-language env vars |
| Merge strategy | Squash merge by default, configurable |
| Conflict handling | Abort + auto-retry (re-dispatch from updated HEAD) |
| Cleanup | Wrapper removes on completion; triage removes on agent death; prune on service restart |
| Default state | Opt-in via `worktree_isolation = true` in config |
| Submodules | Auto-init in worktree-setup if `.gitmodules` exists |
| LFS | Skip smudge on creation, lazy fetch; shared LFS cache |
| GitButler | Complementary tool for visibility, not a replacement |
