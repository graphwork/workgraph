# Agent Isolation Strategy Decision

**Task:** synthesis-agent-isolation
**Date:** 2026-03-07
**Inputs:** docs/research/git-worktrees-agent-isolation.md, docs/research/gitbutler-virtual-branches.md

---

## Research Summary

### Git Worktrees (docs/research/git-worktrees-agent-isolation.md)

Git worktrees provide complete filesystem isolation per agent via separate checkouts that share a single object store. Measured at 68ms creation time and 12MB disk per worktree (source only), they require no external dependencies (git >= 2.5, shipped since 2015). The report includes a concrete run.sh integration sketch with worktree lifecycle management, per-language build isolation patterns (Rust, Node, Python, Go, C/C++, Java), squash-merge-back strategy, and thorough edge case coverage (agent death, service restart, submodules, LFS).

### GitButler Virtual Branches (docs/research/gitbutler-virtual-branches.md)

GitButler virtual branches are a change-sorting mechanism that assigns file-level diffs to logical branches within a single working directory. They provide no filesystem, build, or process isolation — concurrent agents writing to the same directory will corrupt each other's work. The CLI (`but`) is scriptable with JSON output, and an MCP server exists for commit management. The report concludes GitButler is fundamentally unsuitable as primary isolation but could serve as supplementary tooling for commit organization and visual review once worktree isolation is stable.

---

## Viewpoint Deliberation

### Pragmatist: What ships fastest with least risk?

**Git worktrees, no contest.** The implementation touches one file (run.sh / spawn logic in the coordinator) and uses primitives that have been stable for 11 years. The worktree research report includes a near-complete run.sh sketch — most of the design work is already done. GitButler adds an external dependency, has a young CLI, known Linux issues, and doesn't even solve the core problem. The fastest path to "agents don't clobber each other" is: create worktree before agent starts, remove it after agent finishes.

The 80% solution is literally: `git worktree add`, `cd worktree`, run agent, `git worktree remove`. That's four lines of shell. Everything else (build isolation, merge-back, cleanup) is important but can be layered incrementally.

### Architect: What's the right long-term design?

Worktrees are the correct abstraction boundary. Each agent gets:
- Its own filesystem (working tree)
- Its own git state (HEAD, index, branch)
- Its own build artifacts (via env vars / setup hook)
- Its own process space (naturally, by running in a different cwd)

This maps cleanly to the conceptual model: **one task = one branch = one worktree = one agent**. The mapping is 1:1, which makes reasoning about the system trivial.

GitButler's model (N virtual branches in 1 working tree) breaks this clean mapping — it requires all agents to share state and then sorts changes after the fact. This is architecturally fragile and doesn't compose well as concurrency increases.

At 20+ concurrent agents, the worktree approach scales linearly: 20 worktrees, 20 branches, 20 independent build/test cycles. Disk cost is manageable with hardlink seeding for build caches. GitButler at 20 concurrent writers in one directory is chaos — the architecture literally cannot handle it.

The one architectural concern with worktrees is merge-back: when many agents complete near-simultaneously, sequential squash merges could bottleneck. But this is a queue, not a fundamental problem — the coordinator can serialize merges, and conflict-retry handles the occasional collision.

### Operator: What's easiest to debug when things go wrong?

Worktrees win on debuggability:
- **Agent stuck?** `ls .wg-worktrees/agent-XXXX/` — you can see exactly what files the agent modified.
- **Build broken?** `cd .wg-worktrees/agent-XXXX/ && cargo build` — reproduce it directly.
- **Merge conflict?** `git diff HEAD...wg/agent-XXXX/task-id` — see what diverged.
- **Agent died?** `git worktree list` shows orphaned worktrees. `git worktree remove --force` cleans up.
- **Everything broken?** `rm -rf .wg-worktrees/ && git worktree prune` — nuclear reset.

With GitButler, debugging concurrent issues means untangling which agent's changes ended up in which virtual branch, whether the hook fired correctly, whether the MCP server processed the commit — layers of indirection with a young tool.

**Failure modes are well-understood for worktrees:**
- Agent dies mid-work: worktree stays, uncommitted changes lost, branch may have partial commits. Cleanup is mechanical.
- Merge conflict: abort, re-dispatch from updated HEAD. Agent retries with fresh state.
- Disk full: worktree creation fails fast with clear error. No partial state.

**Failure modes for GitButler are poorly understood:**
- Agent dies mid-work in shared directory: other agents affected. Which changes belong to whom?
- GitButler daemon crashes: all virtual branch state is in limbo.
- `but` CLI bug: cascading failures across all agents in the workspace.

### Minimalist: Do we even need a full solution?

Good question. The current pain points are:
1. Agents clobbering each other's file changes
2. Build lock contention (`cargo` locks, `node_modules` state)
3. Test interference (one agent's compile breaks another's test run)

Could simpler measures get 80%?

- **Better `--after` sequencing**: Prevents agents from running in parallel when they shouldn't. But we already do this — the problem is agents on legitimately independent tasks that happen to share files (e.g., both modify `Cargo.toml` by adding different dependencies).
- **File-level locking**: The file-locking audit (docs/research/file-locking-audit.md) explored this. It prevents concurrent writes but doesn't solve build isolation — two agents can modify different files and still break each other's builds.
- **Sequential execution only**: Works but defeats the purpose of concurrent agents. We'd lose the parallelism that makes workgraph valuable.

**Verdict:** Simpler measures don't solve build isolation, which is the biggest pain point. You can sequence file writes, but you can't sequence `cargo build` across agents sharing one `target/` without serializing all work. Worktrees solve both problems simultaneously: file isolation AND build isolation.

That said, the minimalist has a valid point about **incremental adoption**: we don't need to ship the full worktree lifecycle on day one. The minimum viable version is worktree creation + cwd change + cleanup. Merge-back, build cache seeding, and setup hooks can come later.

---

## Decision

**We are building git worktree isolation.**

Git worktrees are the clear choice across all four viewpoints:
- **Pragmatist**: Fastest to ship, least risk, near-complete design exists
- **Architect**: Clean 1:1 mapping (task = branch = worktree = agent), scales linearly
- **Operator**: Transparent, debuggable, well-understood failure modes
- **Minimalist**: Solves both file AND build isolation in one mechanism; simpler alternatives don't

GitButler is explicitly **not adopted** — not because it's a bad tool, but because its design (change-sorting in a single working tree) doesn't solve the isolation problem. It may have supplementary value later, but only after worktree isolation is stable and the marginal benefit of commit organization justifies the added dependency.

---

## Scope

### Phase 1: Minimum Viable Worktree Isolation

The smallest change that eliminates agent interference:

1. **Worktree lifecycle in spawn logic**: Create worktree before agent starts, remove after agent finishes
   - `git worktree add .wg-worktrees/<agent-id> -b wg/<agent-id>/<task-id> HEAD`
   - Agent cwd set to worktree path
   - `git worktree remove --force` on completion/failure
2. **`.workgraph` symlink**: So `wg` CLI works from worktree
3. **Env vars**: `WG_WORKTREE_PATH`, `WG_BRANCH`, `WG_PROJECT_ROOT`
4. **Opt-in config**: `worktree_isolation = true` in workgraph config (default off initially)
5. **Basic cleanup**: On service restart, prune stale worktrees
6. **`.wg-worktrees/` in `.gitignore`**

**Not in phase 1:**
- Merge-back (agents push their branch; merge is manual or a follow-up)
- Build cache seeding (agents build from scratch — slower but correct)
- `worktree-setup.sh` hook (projects use env vars directly)
- GitButler integration (deferred indefinitely)

### Phase 2: Merge-Back and Build Optimization

Once phase 1 is stable and validated:

1. **Automatic squash merge**: Wrapper merges agent branch to main after task completion
2. **Conflict handling**: Abort + auto-retry (re-dispatch from updated HEAD)
3. **Build cache seeding**: `cp -al` for Rust `target/`, language-specific setup
4. **`worktree-setup.sh` hook**: Project-specific build isolation config
5. **Branch cleanup**: Delete merged branches automatically

### Phase 3: Operational Hardening

1. **Max worktree age enforcement**: Coordinator triage removes stale worktrees
2. **Disk usage monitoring**: Alert when worktree disk exceeds threshold
3. **`wg worktree` subcommand**: `list`, `clean`, `inspect` for operators
4. **Default on**: Flip `worktree_isolation` default to `true`

### Deferred (No Timeline)

- **GitButler supplementary integration**: Revisit when their CLI reaches stable and Linux support matures
- **Separate clones fallback**: Only if worktrees prove insufficient (unlikely)
- **Real-time conflict detection**: Could use `git diff --name-only` pre-dispatch to warn about file overlaps

---

## Hybrid Elements Worth Incorporating

While the decision is clearly worktrees, two ideas from the GitButler research are worth carrying forward:

1. **File-scope awareness in task descriptions**: GitButler's real-time conflict detection is appealing. We can approximate this cheaply by requiring task descriptions to list files they'll modify, and having the coordinator check for overlaps before dispatching parallel tasks. This is already partially addressed by the "same files = sequential edges" rule.

2. **Visual branch overview**: GitButler's GUI provides a nice multi-branch visualization. We don't need GitButler for this — `git log --all --oneline --graph` or TUI integration showing active worktree branches would serve the same purpose without the dependency.

---

## Summary

| Aspect | Decision |
|--------|----------|
| **Primary isolation** | Git worktrees |
| **GitButler** | Not adopted (deferred indefinitely) |
| **Merge strategy** | Squash merge (phase 2) |
| **Build isolation** | Per-worktree via env vars + setup hook (phase 2) |
| **Rollout** | Opt-in config flag, phased adoption |
| **First phase scope** | Worktree create/remove lifecycle, .workgraph symlink, env vars |

**We are building git worktree isolation as the agent isolation mechanism for workgraph.** Phase 1 delivers the core lifecycle (create, run, cleanup) behind an opt-in config flag. Phase 2 adds merge-back and build optimization. Phase 3 hardens operations. GitButler is not part of the plan.
