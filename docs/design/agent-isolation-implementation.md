# Agent Isolation: Implementation Design

**Task:** design-agent-isolation
**Date:** 2026-03-07
**Input:** docs/design/agent-isolation-decision.md

---

## Overview

This document translates the git worktree isolation decision into concrete code changes, organized into three implementation groups that can be fanned out to parallel agents.

**Key principle:** Changes are scoped to minimize cross-group file conflicts. Each group modifies distinct files, enabling parallel implementation.

---

## 1. Agent Spawn Changes

### What happens today

`spawn_agent_inner()` in `src/commands/spawn/execution.rs` does:
1. Claims the task in the graph
2. Creates agent output directory at `.workgraph/agents/<agent-id>/`
3. Builds the inner command (executor-specific)
4. Writes `run.sh` wrapper script via `write_wrapper_script()`
5. Spawns `bash run.sh` with env vars (`WG_TASK_ID`, `WG_AGENT_ID`, `WG_EXECUTOR_TYPE`, `WG_MODEL`)
6. Registers agent in `AgentRegistry`

The agent process runs in the project root (or `settings.working_dir` if set). All agents share the same working tree.

### What changes

**New module:** `src/commands/spawn/worktree.rs`

This module handles worktree lifecycle and is called from `spawn_agent_inner()` when worktree isolation is enabled.

```rust
// src/commands/spawn/worktree.rs

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Worktree paths and metadata for an isolated agent workspace.
pub struct WorktreeInfo {
    /// Absolute path to the worktree directory
    pub path: PathBuf,
    /// Branch name: wg/<agent-id>/<task-id>
    pub branch: String,
    /// Absolute path to the main project root
    pub project_root: PathBuf,
}

/// Create a worktree for an agent.
///
/// 1. git worktree add .wg-worktrees/<agent-id> -b wg/<agent-id>/<task-id> HEAD
/// 2. Symlink .workgraph into the worktree
/// 3. Run worktree-setup.sh if it exists
pub fn create_worktree(
    project_root: &Path,
    workgraph_dir: &Path,
    agent_id: &str,
    task_id: &str,
) -> Result<WorktreeInfo> {
    let branch = format!("wg/{}/{}", agent_id, task_id);
    let worktree_dir = project_root.join(".wg-worktrees").join(agent_id);

    // Create worktree from HEAD
    let output = Command::new("git")
        .args(["worktree", "add"])
        .arg(&worktree_dir)
        .args(["-b", &branch, "HEAD"])
        .current_dir(project_root)
        .output()
        .context("Failed to run git worktree add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr.trim());
    }

    // Symlink .workgraph so wg CLI works from the worktree
    let symlink_target = workgraph_dir.canonicalize()
        .context("Failed to canonicalize .workgraph path")?;
    let symlink_path = worktree_dir.join(".workgraph");
    std::os::unix::fs::symlink(&symlink_target, &symlink_path)
        .context("Failed to symlink .workgraph into worktree")?;

    // Run worktree-setup.sh if it exists
    let setup_script = workgraph_dir.join("worktree-setup.sh");
    if setup_script.exists() {
        let _ = Command::new("bash")
            .arg(&setup_script)
            .arg(&worktree_dir)
            .arg(project_root)
            .current_dir(&worktree_dir)
            .output(); // Best-effort; don't fail spawn if setup hook fails
    }

    Ok(WorktreeInfo {
        path: worktree_dir,
        branch,
        project_root: project_root.to_path_buf(),
    })
}

/// Remove a worktree and its branch. Force-removes to discard uncommitted changes.
pub fn remove_worktree(project_root: &Path, worktree_path: &Path, branch: &str) -> Result<()> {
    // Remove the symlink first (git worktree remove won't remove it)
    let symlink_path = worktree_path.join(".workgraph");
    if symlink_path.exists() {
        let _ = std::fs::remove_file(&symlink_path);
    }

    // Force-remove the worktree
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .current_dir(project_root)
        .output();

    // Delete the branch
    let _ = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(project_root)
        .output();

    // Prune stale worktree entries
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(project_root)
        .output();

    Ok(())
}
```

### Modifications to `spawn_agent_inner()` in `src/commands/spawn/execution.rs`

After the agent output directory is created (line ~147) and before the wrapper script is written:

```rust
// After temp_agent_id and output_dir are set up...

// --- Worktree isolation ---
let worktree_info = if config.coordinator.worktree_isolation {
    let project_root = dir.parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine project root from {:?}", dir))?;
    match worktree::create_worktree(project_root, dir, &temp_agent_id, task_id) {
        Ok(info) => {
            eprintln!(
                "[spawn] Created worktree for {} at {:?} (branch: {})",
                temp_agent_id, info.path, info.branch
            );
            Some(info)
        }
        Err(e) => {
            // Worktree creation failed — bail out, don't spawn
            anyhow::bail!("Worktree creation failed for {}: {}", temp_agent_id, e);
        }
    }
} else {
    None
};
```

Then, when setting up the `Command`:

```rust
// Override working directory to worktree path
if let Some(ref wt) = worktree_info {
    cmd.current_dir(&wt.path);
    cmd.env("WG_WORKTREE_PATH", &wt.path);
    cmd.env("WG_BRANCH", &wt.branch);
    cmd.env("WG_PROJECT_ROOT", &wt.project_root);
} else if let Some(ref wd) = settings.working_dir {
    cmd.current_dir(wd);
}
```

The `worktree_info` is also stored in the agent metadata JSON so cleanup can find it:

```json
{
    "agent_id": "agent-7500",
    "worktree_path": "/home/user/project/.wg-worktrees/agent-7500",
    "worktree_branch": "wg/agent-7500/implement-foo"
}
```

### Environment variables set

| Variable | Value | Purpose |
|----------|-------|---------|
| `WG_WORKTREE_PATH` | `/abs/path/.wg-worktrees/agent-XXXX` | Agent's working directory |
| `WG_BRANCH` | `wg/agent-XXXX/task-id` | Agent's git branch |
| `WG_PROJECT_ROOT` | `/abs/path/to/project` | Main repo root |
| `WG_AGENT_ID` | `agent-XXXX` | Already exists, unchanged |
| `WG_TASK_ID` | `task-id` | Already exists, unchanged |

### How the agent knows it's in a worktree

The agent doesn't need to know. It works in its cwd as usual. The `wg` CLI works because `.workgraph` is symlinked. The env vars exist for the wrapper script and any tooling that needs to reference the main repo.

### Config addition

Add to `CoordinatorConfig` in `src/config.rs`:

```rust
/// Enable git worktree isolation for spawned agents.
/// When true, each agent gets its own worktree at .wg-worktrees/<agent-id>/.
#[serde(default)]
pub worktree_isolation: bool,
```

Default is `false` (opt-in for phase 1).

### `.gitignore` addition

Add `.wg-worktrees/` to the project `.gitignore`.

---

## 2. Merge/Completion Flow

### When the agent finishes

The merge-back happens in `write_wrapper_script()` output — additional shell code in `run.sh` after the agent process exits. This runs in the wrapper, not in Rust code, because:
1. The wrapper already handles post-agent logic (status check, `wg done`, `wg fail`)
2. Merge must happen after the agent exits and task status is determined
3. Shell-level merge is simpler and more debuggable than Rust git2 bindings

### Wrapper script additions (in `write_wrapper_script()`)

After the existing post-agent status handling, add a merge-back section:

```bash
# --- Merge Back (worktree isolation) ---
if [ -n "$WG_WORKTREE_PATH" ] && [ -n "$WG_BRANCH" ] && [ -n "$WG_PROJECT_ROOT" ]; then
    TASK_STATUS_FINAL=$(wg show "$TASK_ID" --json 2>/dev/null | grep -o '"status": *"[^"]*"' | head -1 | sed 's/.*"status": *"//;s/"//' || echo "unknown")

    if [ "$TASK_STATUS_FINAL" = "done" ]; then
        # Check if agent made any commits
        COMMITS=$(git -C "$WG_PROJECT_ROOT" log --oneline "HEAD..$WG_BRANCH" 2>/dev/null | wc -l | tr -d ' ')
        if [ "$COMMITS" -gt 0 ]; then
            cd "$WG_PROJECT_ROOT"

            # Acquire merge lock (serialize concurrent merges)
            MERGE_LOCK="$WG_PROJECT_ROOT/.wg-worktrees/.merge-lock"
            exec 9>"$MERGE_LOCK"
            flock 9

            git merge --squash "$WG_BRANCH" 2>> "$OUTPUT_FILE"
            MERGE_EXIT=$?

            if [ $MERGE_EXIT -ne 0 ]; then
                git merge --abort 2>/dev/null
                echo "[wrapper] Merge conflict on $WG_BRANCH — marking task failed for retry" >> "$OUTPUT_FILE"
                wg fail "$TASK_ID" --reason "Merge conflict integrating worktree branch $WG_BRANCH" 2>> "$OUTPUT_FILE"
            else
                git commit -m "$(cat <<COMMITEOF
feat: ${TASK_ID} (${WG_AGENT_ID})

Squash-merged from worktree branch ${WG_BRANCH}
COMMITEOF
)" 2>> "$OUTPUT_FILE"
                echo "[wrapper] Merged $WG_BRANCH to $(git rev-parse --abbrev-ref HEAD)" >> "$OUTPUT_FILE"
            fi

            # Release merge lock
            flock -u 9
        else
            echo "[wrapper] No commits on $WG_BRANCH, nothing to merge" >> "$OUTPUT_FILE"
        fi
    fi

    # Always clean up the worktree, regardless of task outcome
    # Remove .workgraph symlink first
    rm -f "$WG_WORKTREE_PATH/.workgraph" 2>/dev/null
    git -C "$WG_PROJECT_ROOT" worktree remove --force "$WG_WORKTREE_PATH" 2>/dev/null
    git -C "$WG_PROJECT_ROOT" branch -D "$WG_BRANCH" 2>/dev/null
    echo "[wrapper] Cleaned up worktree at $WG_WORKTREE_PATH" >> "$OUTPUT_FILE"
fi
```

### Conflict detection and resolution strategy

1. **File-lock serialization:** A flock-based merge lock at `.wg-worktrees/.merge-lock` ensures only one merge happens at a time. This prevents race conditions when multiple agents finish simultaneously.

2. **Squash merge:** `git merge --squash` combines all agent commits into one. This gives clean one-commit-per-task history on the main branch.

3. **Conflict = fail + retry:** If the squash merge has conflicts, the wrapper aborts and marks the task as failed with reason "Merge conflict". The coordinator will re-dispatch the task, and the new agent starts from updated HEAD (which includes all previously merged work). The conflict typically resolves itself because the new agent sees the latest state.

4. **No commits = no-op:** If the agent didn't make any commits on its branch (e.g., research/docs task that only modified files in `.workgraph/`), skip the merge.

### What if the merge fails?

| Scenario | Action |
|----------|--------|
| Clean merge | Commit squash merge, clean up worktree |
| Conflict | `git merge --abort`, `wg fail` with merge-conflict reason, clean up worktree |
| Retry exhausted | Task stays failed; user sees "Merge conflict" reason in `wg show` |

The coordinator's existing retry logic handles re-dispatch. The new agent will start from a fresh worktree branched from the current HEAD, which includes all previously merged work.

### Commit message conventions

```
feat: <task-id> (<agent-id>)

Squash-merged from worktree branch wg/<agent-id>/<task-id>
```

This format:
- Identifies which task produced the commit
- Identifies which agent did the work
- Indicates it was a squash merge from an isolated worktree

### Push behavior

The agent already pushes from within the worktree (as instructed by the prompt). After merge-back, the wrapper does NOT auto-push the squash commit to remote. The existing `git push` from the agent handles remote sync. If the agent's push was to its worktree branch, the squash commit on main will be pushed on the next agent's push or by the user.

**Future enhancement:** Add `auto_push_after_merge` config option to push main after successful merge.

---

## 3. Cleanup/Lifecycle

### Agent killed mid-work

When an agent dies (detected by `cleanup_dead_agents()` in `src/commands/service/triage.rs`):

1. **Detection:** Existing PID-liveness check finds the dead agent
2. **Worktree recovery:** Check if agent has a worktree by reading `metadata.json` for `worktree_path` and `worktree_branch`
3. **Partial work recovery:** Check if agent made commits: `git log --oneline HEAD..<branch>`
   - If commits exist: log them for potential recovery, but don't auto-merge (dead agent = incomplete work)
   - If no commits: nothing to recover
4. **Cleanup:** Force-remove worktree and delete branch (same as normal cleanup)
5. **Task re-open:** Existing triage logic handles this (marks task failed/re-opens)

### Modifications to `cleanup_dead_agents()` in `src/commands/service/triage.rs`

After marking the agent as dead and unclaiming the task, add worktree cleanup:

```rust
// After existing dead-agent handling...

// Clean up worktree if it exists
let agent_dir = std::path::Path::new(&output_file).parent();
if let Some(agent_dir) = agent_dir {
    let metadata_path = agent_dir.join("metadata.json");
    if let Ok(metadata_str) = fs::read_to_string(&metadata_path) {
        if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&metadata_str) {
            if let (Some(wt_path), Some(wt_branch)) = (
                metadata.get("worktree_path").and_then(|v| v.as_str()),
                metadata.get("worktree_branch").and_then(|v| v.as_str()),
            ) {
                let project_root = dir.parent().unwrap_or(dir);
                let wt_path = Path::new(wt_path);
                if wt_path.exists() {
                    eprintln!("[triage] Cleaning up worktree for dead agent {}: {:?}", agent_id, wt_path);
                    let _ = worktree::remove_worktree(project_root, wt_path, wt_branch);
                }
            }
        }
    }
}
```

### Service restart: discover and clean orphaned workspaces

On service startup (in `src/commands/service/mod.rs`), add a one-time cleanup:

```rust
/// Clean up orphaned worktrees from a previous service run.
/// Called once on service startup.
fn cleanup_orphaned_worktrees(dir: &Path) -> Result<()> {
    let project_root = dir.parent().ok_or_else(|| anyhow::anyhow!("No project root"))?;
    let worktrees_dir = project_root.join(".wg-worktrees");

    if !worktrees_dir.exists() {
        return Ok(());
    }

    let registry = AgentRegistry::load(dir)?;

    for entry in fs::read_dir(&worktrees_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip non-agent directories (e.g., .merge-lock)
        if !name.starts_with("agent-") {
            continue;
        }

        // Check if this agent is alive
        let is_alive = registry.agents.get(&name)
            .map(|a| a.is_alive() && is_process_alive(a.pid))
            .unwrap_or(false);

        if !is_alive {
            eprintln!("[service] Cleaning orphaned worktree: {}", name);
            let wt_path = entry.path();
            // Read branch from git
            let branch_output = Command::new("git")
                .args(["worktree", "list", "--porcelain"])
                .current_dir(project_root)
                .output();
            // Parse and find the branch for this worktree path, then remove
            let _ = worktree::remove_worktree(project_root, &wt_path, &format!("wg/{}/*", name));
        }
    }

    // Final prune
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(project_root)
        .output();

    Ok(())
}
```

### Disk space management

**When to prune:**
- Immediately after each agent completes (in wrapper script)
- On service startup (orphan cleanup)
- In coordinator triage loop: check for worktrees older than `max_worktree_age` (default: 2 hours)

**Size limits:**
- No hard size limit in phase 1
- Phase 3: configurable `max_worktree_disk_mb` in config; coordinator checks `du -s` in triage

**Prune command in triage (future):**

```rust
// In triage loop, after dead-agent cleanup
let worktrees_dir = project_root.join(".wg-worktrees");
if worktrees_dir.exists() {
    for entry in fs::read_dir(&worktrees_dir)? {
        let entry = entry?;
        if let Ok(meta) = entry.metadata() {
            if let Ok(age) = meta.modified().and_then(|t| t.elapsed()) {
                if age > Duration::from_secs(config.coordinator.max_worktree_age_secs) {
                    // Check if agent is still alive before removing
                    // ...
                }
            }
        }
    }
}
```

### How to recover work from a dead agent's workspace

Before removing an orphaned worktree, check for recoverable commits:

```bash
# List commits the dead agent made
git log --oneline HEAD..wg/<agent-id>/<task-id>

# If there are commits worth saving, create a recovery branch
git branch recover/<agent-id>/<task-id> wg/<agent-id>/<task-id>

# Then clean up the worktree
git worktree remove --force .wg-worktrees/<agent-id>
git branch -D wg/<agent-id>/<task-id>
# recovery branch is preserved for manual inspection
```

In the Rust code (triage.rs), this is:

```rust
// Check for commits before removing
let commit_count = Command::new("git")
    .args(["log", "--oneline", &format!("HEAD..{}", branch)])
    .current_dir(project_root)
    .output()
    .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
    .unwrap_or(0);

if commit_count > 0 {
    eprintln!(
        "[triage] Dead agent {} had {} commits on {}. Creating recovery branch.",
        agent_id, commit_count, branch
    );
    let recovery_branch = format!("recover/{}", branch.strip_prefix("wg/").unwrap_or(&branch));
    let _ = Command::new("git")
        .args(["branch", &recovery_branch, &branch])
        .current_dir(project_root)
        .output();
}
```

---

## 4. File Breakdown

### Group A: Spawn (worktree creation + agent CWD)

**Files to create:**
- `src/commands/spawn/worktree.rs` — new module with `create_worktree()` and `remove_worktree()`

**Files to modify:**
- `src/commands/spawn/mod.rs` — add `mod worktree;`
- `src/commands/spawn/execution.rs` — call `create_worktree()` when `config.coordinator.worktree_isolation` is true; set `cmd.current_dir()` to worktree path; add `WG_WORKTREE_PATH`, `WG_BRANCH`, `WG_PROJECT_ROOT` env vars; store worktree metadata in `metadata.json`
- `src/config.rs` — add `worktree_isolation: bool` to `CoordinatorConfig`
- `.gitignore` — add `.wg-worktrees/`

**Scope boundaries:** This group does NOT modify the wrapper script template or triage code. It creates the worktree and sets the CWD; the agent runs exactly as before but in the worktree directory.

### Group B: Merge-back (post-agent completion)

**Files to modify:**
- `src/commands/spawn/execution.rs` — modify `write_wrapper_script()` to add merge-back shell code when worktree env vars are set. The merge section is appended after existing post-agent handling. Also export `WG_WORKTREE_PATH`, `WG_BRANCH`, `WG_PROJECT_ROOT` in the wrapper header.

**What changes in the wrapper script template:**
1. Export worktree env vars at the top of run.sh
2. After the existing task-status check and `wg done`/`wg fail` block, add the merge-back section
3. After the merge-back section, add the worktree cleanup section

**Scope boundaries:** This group modifies ONLY `write_wrapper_script()` in `execution.rs`. It does not touch `spawn_agent_inner()` (that's Group A) or triage (that's Group C). The merge-back code is entirely self-contained shell script within the wrapper template.

**IMPORTANT: Coordination with Group A.** Group B depends on Group A having already added the worktree env vars to the Command. Group B reads these env vars in the wrapper script. Ensure Group A merges first, or coordinate: Group A adds env vars to both the Command AND the wrapper script header. Group B adds the post-agent merge/cleanup logic.

### Group C: Lifecycle cleanup and disk management

**Files to modify:**
- `src/commands/service/triage.rs` — in `cleanup_dead_agents()`, after marking agent dead, read `metadata.json` for worktree info and call cleanup. Create recovery branches for dead agents with commits.
- `src/commands/service/mod.rs` — add `cleanup_orphaned_worktrees()` call on service startup

**Files to potentially create:**
- None; the `worktree::remove_worktree()` function from Group A is reused. If Group C lands before Group A, it can inline the git commands directly.

**Scope boundaries:** This group modifies only triage and service startup code. No overlap with spawn or wrapper script changes.

### Dependency ordering

```
Group A (spawn) ──→ Group B (merge)
       └──────────→ Group C (cleanup)
```

Group A must land first because:
- Group B's wrapper changes reference env vars that Group A adds
- Group C's triage changes reference `worktree::remove_worktree()` from Group A
- Group C's triage reads `worktree_path`/`worktree_branch` from metadata.json, which Group A writes

Groups B and C are independent of each other and can run in parallel after Group A.

---

## 5. Testing Strategy

### Unit tests

**In `src/commands/spawn/worktree.rs` (Group A):**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_create_worktree() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        // git init, add a file, commit
        Command::new("git").args(["init"]).arg(&project).output().unwrap();
        std::fs::write(project.join("file.txt"), "hello").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&project).output().unwrap();
        Command::new("git").args(["commit", "-m", "init"]).current_dir(&project).output().unwrap();

        let wg_dir = project.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let info = create_worktree(&project, &wg_dir, "agent-1", "task-foo").unwrap();
        assert!(info.path.exists());
        assert_eq!(info.branch, "wg/agent-1/task-foo");
        assert!(info.path.join(".workgraph").exists()); // symlink
        assert!(info.path.join("file.txt").exists()); // source checked out

        // Cleanup
        remove_worktree(&project, &info.path, &info.branch).unwrap();
        assert!(!info.path.exists());
    }

    #[test]
    fn test_create_worktree_fails_gracefully() {
        let temp = TempDir::new().unwrap();
        // No git repo — should fail
        let result = create_worktree(temp.path(), temp.path(), "agent-1", "task-foo");
        assert!(result.is_err());
    }
}
```

**In `execution.rs` wrapper script tests:**

- Existing tests like `test_wrapper_script_generation_success` can be extended to verify worktree env vars appear in the script when `worktree_isolation = true`
- Test that merge-back section is present in wrapper when worktree vars are set
- Test that merge-back section is absent when worktree vars are not set

### Integration tests

**Smoke test: spawn 2 agents on conflicting tasks (Group A):**

```rust
#[test]
fn test_two_agents_isolated_worktrees() {
    // Setup: git repo with one file
    // Task A: modify file line 1
    // Task B: modify file line 2
    // Spawn both with shell executor + worktree_isolation=true
    // Verify: both agents get separate worktrees
    // Verify: both agents' worktrees have the original file content
    // Verify: worktree paths differ
}
```

**Merge-back test (Group B):**

```rust
#[test]
fn test_merge_back_after_agent_completes() {
    // Setup: git repo, worktree created
    // Agent makes a commit in worktree
    // Wrapper runs merge-back logic
    // Verify: commit appears on main branch as squash merge
    // Verify: worktree is cleaned up
}
```

**Merge conflict test (Group B):**

```rust
#[test]
fn test_merge_conflict_fails_gracefully() {
    // Setup: git repo, worktree created
    // Modify same file on main AND in worktree
    // Attempt merge
    // Verify: merge aborts cleanly
    // Verify: task is marked failed with "merge conflict" reason
    // Verify: worktree is still cleaned up
}
```

**Dead agent cleanup test (Group C):**

```rust
#[test]
fn test_dead_agent_worktree_cleanup() {
    // Setup: git repo, worktree created, agent registered
    // Kill agent process
    // Run cleanup_dead_agents()
    // Verify: worktree removed
    // Verify: branch deleted
    // Verify: if agent had commits, recovery branch exists
}
```

### Manual/E2E test procedure

1. `wg config --worktree-isolation true`
2. Create two tasks that modify different files:
   ```
   wg add "Modify src/foo.rs" --verify "cargo test"
   wg add "Modify src/bar.rs" --verify "cargo test"
   ```
3. `wg service start`
4. Watch with `wg watch` — verify both agents spawn in separate worktrees
5. Verify both complete and their changes appear as squash merges on the main branch
6. Verify `.wg-worktrees/` is empty after both complete

### Edge case tests

| Scenario | How to test | Expected result |
|----------|-------------|-----------------|
| Agent killed mid-work | `kill -9` agent PID | Triage cleans up worktree, task re-opens |
| Service restart with active worktrees | `wg service stop && wg service start` | Orphaned worktrees cleaned on startup |
| Worktree creation fails (disk full) | Mock by making `.wg-worktrees` read-only | Spawn fails cleanly, task not claimed |
| Agent makes no commits | Research task, only reads files | Merge skipped, worktree cleaned up |
| Concurrent merges | Two agents finish at same millisecond | Flock serializes; second agent merges cleanly or retries |
| Agent creates files outside worktree | Agent writes to absolute path | Not prevented; documented as a known limitation |

---

## Summary

| Group | Files | Task |
|-------|-------|------|
| A (Spawn) | `spawn/worktree.rs` (new), `spawn/mod.rs`, `spawn/execution.rs`, `config.rs`, `.gitignore` | Create worktree, set CWD, set env vars |
| B (Merge) | `spawn/execution.rs` (`write_wrapper_script()` only) | Merge-back shell code in wrapper script |
| C (Cleanup) | `service/triage.rs`, `service/mod.rs` | Dead-agent worktree cleanup, orphan cleanup on startup |

Groups B and C can run in parallel after Group A lands.
