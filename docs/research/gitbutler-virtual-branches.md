# GitButler Virtual Branches for Concurrent Agent Isolation

**Task:** research-gitbutler-virtual
**Date:** 2026-03-07
**Builds on:** docs/research/git-worktrees-agent-isolation.md, docs/agent-git-hygiene.md, docs/research/file-locking-audit.md

---

## Executive Summary

GitButler virtual branches are a **change-sorting mechanism**, not a filesystem isolation mechanism. They allow multiple logical branches to coexist in a single working directory by assigning file-level changes to different branches at commit time. This is powerful for a single developer juggling features, but **fundamentally unsuitable as the primary isolation mechanism for concurrent AI agents** that need to compile, test, and modify files independently.

**Verdict: Skip as primary isolation. Consider as supplementary tooling.**

GitButler cannot replace git worktrees for agent isolation because it provides no filesystem, build, or process isolation — all agents would still share one working directory and fight over the same files. However, it offers interesting capabilities as a **commit-organization layer** on top of worktree-based isolation, and its MCP server integration could complement workgraph's agent spawning.

---

## 1. How Virtual Branches Work

### Core Mechanism

Virtual branches are a layer on top of git that maintains multiple logical branches within a single working directory. When GitButler is active:

1. **HEAD points to `gitbutler/workspace`** — a special merge commit that combines all applied virtual branches
2. **The git index reflects the union** of all committed states across applied branches
3. **Uncommitted changes are assigned to branches** by file or hunk (either manually via drag-and-drop in GUI, or via `but stage <file> <branch>` in CLI)
4. **Each commit is synthesized** — GitButler calculates what the branch would look like if only that branch's changes existed, and creates a commit with that tree

### What "Isolation" Actually Means

Virtual branches provide **logical separation at the commit level**, not filesystem separation:

| Layer | Isolated? | Details |
|-------|-----------|---------|
| Filesystem | **No** | All branches share one working directory |
| Git index | **Partial** | Each branch has its own staging area (managed by GitButler, not git) |
| Commits | **Yes** | Each branch gets clean commits with only its changes |
| Build artifacts | **No** | Single `target/`, `node_modules/`, etc. |
| Processes | **No** | All agents would run in same directory |

### How Conflict Prevention Works (Single User)

For a **single developer** (GitButler's design target), conflicts are avoided because:

> "You're essentially starting from the merge product and extracting branches of work from it."

Since one human makes changes sequentially (even if assigned to different branches), there's no true concurrent modification. The developer sees all changes in one directory and can manually route them to branches.

### Why This Breaks With Concurrent Agents

Multiple agents writing to the same working directory simultaneously creates **real-time filesystem conflicts** that GitButler cannot prevent:

- Agent A writes `src/foo.rs` at time T
- Agent B writes `src/foo.rs` at time T+1ms
- Agent A's changes are lost — GitButler never saw them
- GitButler can only sort changes it can observe in `git diff`; it cannot mediate concurrent writes

This was confirmed in [GitHub issue #12224](https://github.com/gitbutlerapp/gitbutler/issues/12224):

> "If both agents modify the same file simultaneously, one will overwrite the other's changes before GitButler can assign the diffs."

The issue was closed by converting to a discussion — **no solution was implemented**.

---

## 2. GitButler CLI & Scriptability

### The `but` CLI

GitButler ships a CLI (`but`) that replicates most GUI functionality. Key commands for automation:

| Command | Purpose |
|---------|---------|
| `but setup` | Initialize GitButler in a repo |
| `but teardown` | Remove GitButler from a repo |
| `but branch new [name]` | Create a parallel branch |
| `but branch new --anchor [branch]` | Create a stacked (dependent) branch |
| `but branch apply/unapply` | Enable/disable a branch in workspace |
| `but commit -m "msg"` | Commit to current/specified branch |
| `but stage <file> <branch>` | Assign file to a branch |
| `but status` | Show workspace state |
| `but push [branch]` | Push branches |
| `but pr` | Create/update PRs |
| `but diff [branch]` | Branch-specific diff |
| `but undo` | Undo last operation |
| `but oplog` | Operation history |
| `but mark <branch>` | Auto-assign changes to branch |

All commands support `--json` / `-j` for structured output, making them scriptable.

### Installation

- **macOS**: `brew install gitbutler` or `curl -fsSL https://gitbutler.com/install.sh | sh`
- **Linux**: Install script or build from source (Rust/Tauri). Linux support exists but has [known compatibility issues](https://github.com/gitbutlerapp/gitbutler/issues/8411)
- **Headless/server**: CLI works standalone without GUI. No daemon required for basic operations.

### MCP Server

GitButler exposes an MCP server via `but mcp` that AI agents can use:

- **Primary tool**: `gitbutler_update_branches` — records changes and creates commits
- **Async processing**: Records changes immediately, processes commit messages in background
- **No GUI required**: MCP server runs from CLI only
- **Current scope**: Primarily commit recording; branch management via MCP is limited

### Scriptability Assessment

| Criterion | Rating | Notes |
|-----------|--------|-------|
| CLI completeness | Good | Most GUI features available |
| JSON output | Good | `--json` flag on all commands |
| Headless operation | Good | CLI works without GUI |
| Linux support | Fair | Works but not first-class; compatibility issues noted |
| Concurrency safety | **Poor** | No locking for concurrent `but` operations |
| Process isolation | **None** | Single working directory, no parallel execution model |

---

## 3. GitButler Stacks

### How Stacks Work

Stacks are ordered sequences of dependent branches where each branch builds on the previous one:

```
Stack "auth-feature":
  ├── auth-models      (bottom, targets main)
  ├── auth-endpoints   (targets auth-models)
  └── auth-tests       (top, targets auth-endpoints)
```

- **Commits propagate upward**: each branch contains all commits from branches below it
- **New commits land in the top branch** and automatically propagate down
- **PRs target the branch below**: `auth-endpoints` PR targets `auth-models`, not `main`
- **Merging must be bottom-up**: merge `auth-models` first, then `auth-endpoints`, etc.

### Could Agents Use Stacks?

Stacks model **sequential dependency**, not parallel isolation. They're useful for:

- Breaking a large feature into reviewable chunks
- Dependent PRs that must merge in order

They do **not** help with concurrent agent isolation because:

- All stacked branches share the same working directory
- Commits in upper branches include lower branch commits (no isolation)
- The dependency model is linear, not parallel

### Parallel Branches vs. Stacks

| Feature | Parallel Branches | Stacked Branches |
|---------|------------------|-----------------|
| Independence | Fully independent | Dependent on parent |
| Working directory | Shared | Shared |
| Commit isolation | Yes (per-branch) | No (cumulative) |
| Use case | Multiple unrelated features | Sequential dependent changes |
| Agent isolation | No (shared filesystem) | No (shared filesystem) |

---

## 4. Integration with Workgraph: Assessment

### The GitButler + Claude Code Blog Post

GitButler published a blog post titled ["Managing Multiple Claude Code Sessions Without Worktrees"](https://blog.gitbutler.com/parallel-claude-code) demonstrating their approach:

1. **Claude Code lifecycle hooks** notify GitButler when files are edited
2. GitButler **auto-creates a branch per session** and assigns changes to it
3. Result: one branch per session, one commit per chat round
4. **Limitation acknowledged**: this is **sorting**, not isolation

### Could This Work for Workgraph?

**The blog post scenario differs critically from workgraph's:**

| Dimension | Blog Post Scenario | Workgraph Scenario |
|-----------|-------------------|-------------------|
| Agents | 2-3 Claude Code sessions | 5-10 concurrent agents |
| File overlap | Carefully chosen non-overlapping tasks | Frequently overlapping (same codebase) |
| Build isolation | Not addressed (demo app) | Critical (cargo build, cargo test) |
| Duration | Short interactive sessions | Long autonomous tasks (10-30 min) |
| Failure mode | Human notices and intervenes | Unattended — failures must be automated |

### Why Virtual Branches Cannot Replace Worktrees for Workgraph

1. **No filesystem isolation**: Two agents writing `src/lib.rs` simultaneously will corrupt each other's work. GitButler sorts diffs after the fact — it cannot prevent concurrent writes.

2. **No build isolation**: All agents share one `target/` directory. Concurrent `cargo build` invocations fight over lock files and invalidate each other's incremental compilation. This is the #1 source of agent friction today.

3. **No test isolation**: `cargo test` in a shared directory uses shared test binaries. An agent running tests while another is mid-compilation gets spurious failures.

4. **Single-writer assumption**: GitButler's architecture assumes one writer (human or single agent) making changes, then sorting them into branches. The MCP `gitbutler_update_branches` tool is designed for one agent calling it after each edit — not N agents calling simultaneously.

5. **Hook-based assignment is fragile**: The Claude Code hook approach relies on each agent reporting which files it modified. If an agent modifies a file without triggering the hook (e.g., a subprocess, a build tool generating files), GitButler won't assign it correctly.

6. **No rollback isolation**: If one agent's changes break the build, all other agents in the same working directory are affected. With worktrees, a broken build in one worktree doesn't affect others.

### Where GitButler Could Add Value (Supplementary)

Even though virtual branches can't provide isolation, GitButler could supplement worktree-based isolation:

1. **Post-merge organization**: After agents complete work in worktrees and merge back, GitButler could help organize the merged changes into reviewable stacks for PR creation.

2. **MCP commit management**: Agents could use `but mcp` to auto-create well-structured commits within their worktrees (one `but setup` per worktree).

3. **Human review dashboard**: The GitButler GUI could provide visual overview of all agent branches and their changes — better than `git log --all --graph`.

4. **Conflict preview**: Before merging agent worktree branches, GitButler's conflict detection could identify problematic overlaps.

### Integration Sketch (If Supplementary)

```
coordinator claims task
  → create git worktree (isolation)
  → but setup in worktree (optional: commit management)
  → agent works in isolated worktree
  → agent uses but commit for structured commits
  → agent calls wg done
  → wrapper merges worktree branch to main
  → GitButler on main shows all merged branches for review
```

This adds value but also adds complexity:
- **Dependency**: GitButler CLI must be installed on all agent hosts
- **State management**: `but setup` / `but teardown` lifecycle per worktree
- **Failure modes**: GitButler bugs become agent failure modes
- **Marginal benefit**: Standard `git commit` in a worktree already works fine

---

## 5. Comparison: GitButler vs. Git Worktrees vs. Separate Clones

| Dimension | GitButler Virtual Branches | Git Worktrees | Separate Clones |
|-----------|--------------------------|---------------|-----------------|
| **Filesystem isolation** | None | Complete | Complete |
| **Build isolation** | None | Complete (with env vars) | Complete (natural) |
| **Test isolation** | None | Complete | Complete |
| **Disk cost (source)** | 0 extra | ~repo size per agent | ~repo size per agent |
| **Disk cost (build)** | 0 extra (shared) | Configurable (hardlinks) | Full duplicate |
| **Disk cost (.git)** | 0 extra | ~50KB per worktree | Full .git per clone |
| **Setup time** | ~instant | ~70ms (measured) | Seconds to minutes |
| **Dependencies** | GitButler CLI | Git >= 2.5 (built-in) | Git (built-in) |
| **Concurrent safety** | Poor (single-writer) | Excellent (separate dirs) | Excellent |
| **Merge model** | Virtual merge + PR | Standard git merge | Standard git merge |
| **Conflict detection** | Real-time (single user) | At merge time | At merge time |
| **Object dedup** | N/A (shared repo) | Shared object store | None (unless `--reference`) |
| **Branch constraint** | Multiple per workspace | One per worktree | No constraint |
| **Maturity** | Young (CLI in preview) | Mature (since git 2.5, 2015) | Mature |
| **Server/CI friendly** | CLI works, but young | Fully supported | Fully supported |
| **Recovery from agent crash** | Complex (shared state) | Simple (remove worktree) | Simple (delete clone) |

### When to Use Each

| Scenario | Best Choice |
|----------|-------------|
| Concurrent agents needing build/test isolation | **Git worktrees** |
| Single developer, multiple features | **GitButler virtual branches** |
| CI/CD with maximum isolation | **Separate clones** |
| Disk-constrained environments | **GitButler** (0 extra) or **worktrees** (minimal) |
| Maximum simplicity, no external deps | **Git worktrees** |
| Visual branch management for humans | **GitButler GUI** |

---

## 6. Recommendation

### Primary: Skip GitButler as isolation mechanism

GitButler virtual branches **cannot solve the shared-working-tree problem** for concurrent agents. The architecture fundamentally assumes a single writer making changes in one directory and sorting them into branches afterward. Multiple agents writing simultaneously will experience the same file conflicts, build lock contention, and test interference that plague the current system.

### Secondary: Adopt git worktrees (per sibling research)

Git worktrees provide the filesystem, build, and process isolation that concurrent agents need. See [docs/research/git-worktrees-agent-isolation.md](git-worktrees-agent-isolation.md) for the complete design.

### Tertiary: Revisit GitButler as supplementary tooling later

GitButler's MCP server, commit management, and visual review capabilities could add value **on top of** worktree isolation, but this is a nice-to-have, not a priority. The CLI is still in technical preview (as of early 2026), Linux support has known issues, and the marginal benefit over standard git commands doesn't justify the added dependency for now.

**Conditions for revisiting:**
- GitButler CLI reaches stable release
- Linux support matures (no known compatibility issues)
- MCP server gains full branch management (not just commit recording)
- Workgraph has worktree isolation working and stable

### Decision Matrix

| Option | Effort | Risk | Isolation | Recommendation |
|--------|--------|------|-----------|----------------|
| GitButler as primary isolation | Low | **Critical** — doesn't actually isolate | None | **Skip** |
| GitButler as supplementary to worktrees | Medium | Low — adds dependency | N/A (worktrees isolate) | **Defer** |
| Git worktrees only | Medium | Low — mature technology | Complete | **Adopt** |
| Separate clones | High | Low | Complete | **Fallback** if worktrees insufficient |

---

## Sources

- [GitButler Virtual Branches docs](https://docs.gitbutler.com/features/branch-management/virtual-branches)
- [GitButler CLI Overview](https://docs.gitbutler.com/cli-overview)
- [GitButler CLI Cheat Sheet](https://docs.gitbutler.com/cli/cheat)
- [Introducing the GitButler CLI (blog)](https://blog.gitbutler.com/but-cli)
- [GitButler Stacked Branches docs](https://docs.gitbutler.com/features/branch-management/stacked-branches)
- [GitButler Workspace Branch docs](https://docs.gitbutler.com/features/virtual-branches/integration-branch)
- [GitButler Rebasing and Conflicts docs](https://docs.gitbutler.com/features/branch-management/merging)
- [GitButler MCP Server docs](https://docs.gitbutler.com/features/ai-integration/mcp-server)
- [GitButler Installation docs](https://docs.gitbutler.com/cli-guides/installation)
- [GitHub Issue #12224: Parallel multi-agent workflows](https://github.com/gitbutlerapp/gitbutler/issues/12224)
- [Managing Multiple Claude Code Sessions Without Worktrees (blog)](https://blog.gitbutler.com/parallel-claude-code)
- [GitButler Linux compatibility issue #8411](https://github.com/gitbutlerapp/gitbutler/issues/8411)
