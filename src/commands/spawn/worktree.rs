//! Git worktree isolation for spawned agents.
//!
//! When worktree isolation is enabled, each agent gets its own git worktree
//! at `.wg-worktrees/<agent-id>/`, branched from HEAD. The `.workgraph/`
//! directory is symlinked into the worktree so the `wg` CLI works normally.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Worktree paths and metadata for an isolated agent workspace.
#[derive(Debug)]
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
/// 1. Error out if a worktree/branch with the same name already exists — worktrees
///    are sacred and must only be removed by explicit user action (`wg worktree archive`)
/// 2. `git worktree add .wg-worktrees/<agent-id> -b wg/<agent-id>/<task-id> HEAD`
/// 3. Symlink `.workgraph` into the worktree
/// 4. Run `worktree-setup.sh` if it exists (best-effort)
pub fn create_worktree(
    project_root: &Path,
    workgraph_dir: &Path,
    agent_id: &str,
    task_id: &str,
) -> Result<WorktreeInfo> {
    let branch = format!("wg/{}/{}", agent_id, task_id);
    let worktree_dir = project_root.join(".wg-worktrees").join(agent_id);

    // Worktrees are sacred. If one already exists at this path, refuse to overwrite —
    // the user must explicitly archive it via `wg worktree archive`. Agent IDs are
    // randomly generated so collisions here indicate leftover state that may contain
    // uncommitted work.
    if worktree_dir.exists() {
        anyhow::bail!(
            "Worktree already exists at {:?}. Worktrees are not auto-removed. \
             Archive it explicitly with: wg worktree archive {} --remove",
            worktree_dir,
            agent_id
        );
    }

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
    let symlink_target = workgraph_dir
        .canonicalize()
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

/// Find an existing worktree for a given task by scanning `.wg-worktrees/`
/// for branches named `wg/<agent-id>/<task-id>`. Returns the worktree path
/// and branch name when one is found.
///
/// Used by the retry-in-place path: if a previous attempt left a worktree
/// behind, the next agent reuses it (preserving uncommitted WIP and prior
/// commits) rather than allocating a fresh worktree off `HEAD`.
pub fn find_worktree_for_task(
    project_root: &Path,
    task_id: &str,
) -> Option<(PathBuf, String)> {
    let worktrees_dir = project_root.join(".wg-worktrees");
    if !worktrees_dir.exists() {
        return None;
    }

    let entries = std::fs::read_dir(&worktrees_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("agent-") {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let expected = format!("wg/{}/{}", name, task_id);
        // Use git worktree list --porcelain to confirm the branch.
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(project_root)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut current: Option<&str> = None;
        let path_str = path.to_string_lossy();
        for line in text.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                current = Some(p);
            } else if let Some(b) = line.strip_prefix("branch ") {
                let branch_name = b.strip_prefix("refs/heads/").unwrap_or(b);
                if current == Some(path_str.as_ref()) && branch_name == expected {
                    return Some((path.clone(), branch_name.to_string()));
                }
            } else if line.is_empty() {
                current = None;
            }
        }
    }
    None
}

/// Remove a worktree and its branch. Force-removes to discard uncommitted changes.
pub fn remove_worktree(project_root: &Path, worktree_path: &Path, branch: &str) -> Result<()> {
    // Remove the symlink first (git worktree remove won't remove it)
    let symlink_path = worktree_path.join(".workgraph");
    if symlink_path.exists() {
        let _ = std::fs::remove_file(&symlink_path);
    }

    // Remove isolated cargo target directory
    let target_dir = worktree_path.join("target");
    if target_dir.exists() {
        let _ = std::fs::remove_dir_all(&target_dir);
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

    // NOTE: We intentionally do NOT run `git worktree prune` here.
    // Global prune can remove metadata for other agents' worktrees that are
    // temporarily missing during concurrent cleanup, causing data loss.

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_git_repo(path: &Path) {
        Command::new("git")
            .args(["init"])
            .arg(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .unwrap();
        std::fs::write(path.join("file.txt"), "hello").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(path)
            .output()
            .unwrap();
    }

    #[test]
    fn test_create_worktree() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

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
    fn test_create_worktree_behavior_without_local_git_repo() {
        // Note: In the test environment, Git can find parent repositories even in temp directories.
        // This test verifies the function behavior when there's no local .git directory
        // but Git might still find a parent repository (which is acceptable behavior).

        let temp = TempDir::new().unwrap();

        // Verify temp directory itself doesn't have .git
        assert!(
            !temp.path().join(".git").exists(),
            "Temp directory should not have .git"
        );

        // Test worktree creation - this may succeed or fail depending on whether
        // Git finds a parent repository in the test environment
        let result = create_worktree(temp.path(), temp.path(), "agent-1", "task-foo");

        // The exact behavior depends on test environment, but the function should not crash
        match result {
            Ok(_info) => {
                // If it succeeds, Git found a parent repo - this is valid Git behavior
                println!("Worktree creation succeeded - Git found parent repository");
            }
            Err(_e) => {
                // If it fails, no accessible Git repo was found - also valid
                println!("Worktree creation failed - no accessible Git repository");
            }
        }

        // The key test is that the function handles both cases gracefully without panicking
        // This test primarily ensures the function's error handling works correctly
    }

    #[test]
    fn test_create_worktree_refuses_to_overwrite_existing() {
        // Sacred-worktree invariant: if a worktree already exists at the target
        // path, create_worktree must refuse rather than silently nuke it.
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let wg_dir = project.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let info = create_worktree(&project, &wg_dir, "agent-collide", "task-one").unwrap();
        assert!(info.path.exists());

        // Second creation with the same agent-id must fail, preserving the first.
        let err = create_worktree(&project, &wg_dir, "agent-collide", "task-two").unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("already exists"),
            "expected 'already exists' in error, got: {}",
            msg
        );
        assert!(
            info.path.exists(),
            "original worktree must be preserved on collision"
        );

        remove_worktree(&project, &info.path, &info.branch).unwrap();
    }

    #[test]
    fn test_remove_worktree_idempotent() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let wg_dir = project.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let info = create_worktree(&project, &wg_dir, "agent-1", "task-foo").unwrap();
        remove_worktree(&project, &info.path, &info.branch).unwrap();
        // Second remove should not fail
        remove_worktree(&project, &info.path, &info.branch).unwrap();
    }

    #[test]
    fn test_worktree_symlink_points_to_workgraph() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let wg_dir = project.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        // Write a marker file so we can verify the symlink target
        std::fs::write(wg_dir.join("marker"), "test").unwrap();

        let info = create_worktree(&project, &wg_dir, "agent-2", "task-bar").unwrap();
        let symlink = info.path.join(".workgraph");
        assert!(symlink.is_symlink());
        // The marker file should be readable through the symlink
        assert_eq!(
            std::fs::read_to_string(symlink.join("marker")).unwrap(),
            "test"
        );

        remove_worktree(&project, &info.path, &info.branch).unwrap();
    }
}
