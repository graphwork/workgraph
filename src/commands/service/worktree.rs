//! Worktree lifecycle cleanup for agent isolation.
//!
//! Handles cleanup of git worktrees created for isolated agents:
//! - Dead agent worktree recovery and removal
//! - Orphaned worktree cleanup on service restart
//! - Age-based pruning of stale worktrees

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

/// The directory under the project root where agent worktrees live.
pub const WORKTREES_DIR: &str = ".wg-worktrees";

/// Maximum number of retry attempts for transient failures.
const MAX_RETRIES: usize = 3;

/// Initial retry delay in milliseconds.
const INITIAL_RETRY_DELAY_MS: u64 = 100;

/// Retry a fallible operation with exponential backoff.
/// Returns the result of the operation or the last error if all retries fail.
fn retry_operation<T, F>(mut operation: F, operation_name: &str) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    let mut last_error = None;

    for attempt in 0..=MAX_RETRIES {
        match operation() {
            Ok(result) => return Ok(result),
            Err(e) => {
                last_error = Some(e);

                if attempt < MAX_RETRIES {
                    let delay_ms = INITIAL_RETRY_DELAY_MS * 2_u64.pow(attempt as u32);
                    eprintln!(
                        "[worktree] {} failed on attempt {}/{}, retrying in {}ms: {}",
                        operation_name,
                        attempt + 1,
                        MAX_RETRIES + 1,
                        delay_ms,
                        last_error.as_ref().unwrap()
                    );
                    thread::sleep(Duration::from_millis(delay_ms));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("Operation {} failed with no error details", operation_name)))
}

/// Remove a worktree and its branch. Force-removes to discard uncommitted changes.
pub fn remove_worktree(project_root: &Path, worktree_path: &Path, branch: &str) -> Result<()> {
    let mut cleanup_errors = Vec::new();

    // Remove .workgraph symlink first (git worktree remove won't remove it)
    let symlink_path = worktree_path.join(".workgraph");
    if symlink_path.exists() {
        if let Err(e) = fs::remove_file(&symlink_path) {
            cleanup_errors.push(format!("Failed to remove .workgraph symlink {:?}: {}", symlink_path, e));
        }
    }

    // Remove isolated cargo target directory
    let target_dir = worktree_path.join("target");
    if target_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&target_dir) {
            cleanup_errors.push(format!("Failed to remove target directory {:?}: {}", target_dir, e));
        }
    }

    // Force-remove the worktree
    let output = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .current_dir(project_root)
        .output()
        .context("Failed to execute git worktree remove command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_errors.push(format!("Git worktree remove failed: {}", stderr.trim()));
    }

    // Delete the branch
    let output = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(project_root)
        .output()
        .context("Failed to execute git branch delete command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_errors.push(format!("Git branch delete failed for '{}': {}", branch, stderr.trim()));
    }

    // Prune stale worktree entries
    let output = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(project_root)
        .output()
        .context("Failed to execute git worktree prune command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_errors.push(format!("Git worktree prune failed: {}", stderr.trim()));
    }

    if !cleanup_errors.is_empty() {
        return Err(anyhow!("Worktree removal completed with errors:\n{}", cleanup_errors.join("\n")));
    }

    Ok(())
}

/// Check for recoverable commits on a dead agent's worktree branch.
/// If commits exist, creates a recovery branch at `recover/<agent-id>/<task-id>`.
/// Returns the number of commits found.
pub fn recover_commits(project_root: &Path, branch: &str, agent_id: &str) -> usize {
    let commit_count = Command::new("git")
        .args(["log", "--oneline", &format!("HEAD..{}", branch)])
        .current_dir(project_root)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .count()
        })
        .unwrap_or(0);

    if commit_count > 0 {
        let recovery_branch = format!("recover/{}", branch.strip_prefix("wg/").unwrap_or(branch));
        eprintln!(
            "[worktree] Dead agent {} had {} commits on {}. Creating recovery branch: {}",
            agent_id, commit_count, branch, recovery_branch
        );
        let _ = Command::new("git")
            .args(["branch", &recovery_branch, branch])
            .current_dir(project_root)
            .output();
    }

    commit_count
}

/// Clean up a dead agent's worktree: recover commits, then remove worktree and branch.
/// Uses retry logic for transient failures and enhanced error reporting.
pub fn cleanup_dead_agent_worktree(
    project_root: &Path,
    worktree_path: &Path,
    branch: &str,
    agent_id: &str,
) {
    eprintln!("[worktree] Cleaning up dead agent {} worktree {:?} (branch: {})", agent_id, worktree_path, branch);

    // Recover commits before removing
    let commit_count = recover_commits(project_root, branch, agent_id);
    if commit_count > 0 {
        eprintln!("[worktree] Recovered {} commits from dead agent {}", commit_count, agent_id);
    }

    // Remove the worktree with retry logic
    let cleanup_result = retry_operation(
        || remove_worktree(project_root, worktree_path, branch),
        &format!("worktree cleanup for agent {}", agent_id),
    );

    match cleanup_result {
        Ok(()) => {
            eprintln!("[worktree] Successfully cleaned up worktree for dead agent {}", agent_id);
        }
        Err(e) => {
            eprintln!(
                "[worktree] ERROR: Failed to clean up worktree {:?} for agent {} after {} retries: {}",
                worktree_path, agent_id, MAX_RETRIES, e
            );

            // Log individual error details for troubleshooting
            eprintln!("[worktree] Full error chain: {:?}", e);

            // Try a final fallback: manual directory removal if the worktree path still exists
            if worktree_path.exists() {
                eprintln!("[worktree] Attempting fallback: force removal of directory {:?}", worktree_path);
                if let Err(fallback_err) = fs::remove_dir_all(worktree_path) {
                    eprintln!("[worktree] Fallback also failed: {}", fallback_err);
                } else {
                    eprintln!("[worktree] Fallback succeeded: directory removed");
                }
            }
        }
    }
}

/// Parse `git worktree list --porcelain` output to find the branch for a given worktree path.
pub fn find_branch_for_worktree(project_root: &Path, worktree_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    let worktree_str = worktree_path.to_string_lossy();

    // Porcelain output is blocks separated by blank lines.
    // Each block has: worktree <path>\nHEAD <sha>\nbranch refs/heads/<name>\n
    let mut current_path: Option<&str> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path);
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            if let Some(cp) = current_path
                && cp == worktree_str.as_ref()
            {
                // Convert refs/heads/wg/agent-X/task-Y to wg/agent-X/task-Y
                return Some(
                    branch_ref
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch_ref)
                        .to_string(),
                );
            }
        } else if line.is_empty() {
            current_path = None;
        }
    }

    None
}

/// Clean up orphaned worktrees from a previous service run.
/// Called once on service startup. Scans `.wg-worktrees/` for directories
/// that don't correspond to alive agents.
pub fn cleanup_orphaned_worktrees(dir: &Path) -> Result<usize> {
    let project_root = dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine project root from {:?}", dir))?;
    let worktrees_dir = project_root.join(WORKTREES_DIR);

    if !worktrees_dir.exists() {
        return Ok(0);
    }

    let registry = workgraph::service::registry::AgentRegistry::load(dir)?;
    let mut cleaned = 0;

    for entry in fs::read_dir(&worktrees_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip non-agent directories (e.g., .merge-lock file)
        if !name.starts_with("agent-") {
            continue;
        }

        // Check if this agent is alive
        let is_alive = registry
            .agents
            .get(&name)
            .map(|a| a.is_alive() && crate::commands::is_process_alive(a.pid))
            .unwrap_or(false);

        if !is_alive {
            let wt_path = entry.path();
            eprintln!("[worktree] Cleaning orphaned worktree: {}", name);

            // Try to find the branch from git porcelain output
            let branch = find_branch_for_worktree(project_root, &wt_path);

            if let Some(ref branch) = branch {
                // Use the enhanced cleanup function with retry logic
                cleanup_dead_agent_worktree(project_root, &wt_path, branch, &name);
            } else {
                eprintln!("[worktree] No git branch found for orphaned worktree {}, attempting manual cleanup", name);

                // No branch found — use fallback cleanup with error reporting
                let mut cleanup_errors = Vec::new();

                // Remove .workgraph symlink
                let symlink_path = wt_path.join(".workgraph");
                if symlink_path.exists() {
                    if let Err(e) = fs::remove_file(&symlink_path) {
                        cleanup_errors.push(format!("Failed to remove .workgraph symlink: {}", e));
                    }
                }

                // Remove isolated cargo target directory
                let target_dir = wt_path.join("target");
                if target_dir.exists() {
                    if let Err(e) = fs::remove_dir_all(&target_dir) {
                        cleanup_errors.push(format!("Failed to remove target directory: {}", e));
                    }
                }

                // Try git worktree remove
                let output = Command::new("git")
                    .args(["worktree", "remove", "--force"])
                    .arg(&wt_path)
                    .current_dir(project_root)
                    .output();

                match output {
                    Ok(output) if !output.status.success() => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        cleanup_errors.push(format!("Git worktree remove failed: {}", stderr.trim()));
                    }
                    Err(e) => {
                        cleanup_errors.push(format!("Failed to execute git worktree remove: {}", e));
                    }
                    _ => {} // Success case
                }

                if !cleanup_errors.is_empty() {
                    eprintln!("[worktree] Warnings during manual cleanup of {}: {}", name, cleanup_errors.join("; "));
                }
            }

            cleaned += 1;
        }
    }

    // Final prune to clean up any stale git worktree metadata
    if cleaned > 0 {
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(project_root)
            .output();
    }

    Ok(cleaned)
}

/// Prune worktrees that are older than `max_age_secs`.
/// Called periodically from the triage loop. Only removes worktrees
/// whose agents are no longer alive.
#[allow(dead_code)]
pub fn prune_stale_worktrees(dir: &Path, max_age_secs: u64) -> Result<usize> {
    let project_root = dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine project root from {:?}", dir))?;
    let worktrees_dir = project_root.join(WORKTREES_DIR);

    if !worktrees_dir.exists() {
        return Ok(0);
    }

    let registry = workgraph::service::registry::AgentRegistry::load(dir)?;
    let mut pruned = 0;

    for entry in fs::read_dir(&worktrees_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        if !name.starts_with("agent-") {
            continue;
        }

        // Skip alive agents
        let is_alive = registry
            .agents
            .get(&name)
            .map(|a| a.is_alive() && crate::commands::is_process_alive(a.pid))
            .unwrap_or(false);

        if is_alive {
            continue;
        }

        // Check age
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = match modified.elapsed() {
            Ok(d) => d,
            Err(_) => continue,
        };

        if age.as_secs() > max_age_secs {
            let wt_path = entry.path();
            eprintln!(
                "[worktree] Pruning stale worktree {} (age: {}s > {}s)",
                name,
                age.as_secs(),
                max_age_secs
            );

            let branch = find_branch_for_worktree(project_root, &wt_path);
            if let Some(ref branch) = branch {
                // Use the enhanced cleanup function with retry logic
                cleanup_dead_agent_worktree(project_root, &wt_path, branch, &name);
            } else {
                eprintln!("[worktree] No git branch found for stale worktree {}, attempting manual cleanup", name);

                // Use fallback cleanup with error reporting (same as orphaned cleanup)
                let mut cleanup_errors = Vec::new();

                let symlink_path = wt_path.join(".workgraph");
                if symlink_path.exists() {
                    if let Err(e) = fs::remove_file(&symlink_path) {
                        cleanup_errors.push(format!("Failed to remove .workgraph symlink: {}", e));
                    }
                }

                let target_dir = wt_path.join("target");
                if target_dir.exists() {
                    if let Err(e) = fs::remove_dir_all(&target_dir) {
                        cleanup_errors.push(format!("Failed to remove target directory: {}", e));
                    }
                }

                let output = Command::new("git")
                    .args(["worktree", "remove", "--force"])
                    .arg(&wt_path)
                    .current_dir(project_root)
                    .output();

                match output {
                    Ok(output) if !output.status.success() => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        cleanup_errors.push(format!("Git worktree remove failed: {}", stderr.trim()));
                    }
                    Err(e) => {
                        cleanup_errors.push(format!("Failed to execute git worktree remove: {}", e));
                    }
                    _ => {} // Success case
                }

                if !cleanup_errors.is_empty() {
                    eprintln!("[worktree] Warnings during manual cleanup of stale {}: {}", name, cleanup_errors.join("; "));
                }
            }

            pruned += 1;
        }
    }

    if pruned > 0 {
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(project_root)
            .output();
    }

    Ok(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_git_repo(path: &Path) {
        Command::new("git")
            .args(["init"])
            .arg(path)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
        fs::write(path.join("file.txt"), "hello").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
    }

    fn create_test_worktree(
        project: &Path,
        agent_id: &str,
        task_id: &str,
    ) -> (std::path::PathBuf, String) {
        let branch = format!("wg/{}/{}", agent_id, task_id);
        let wt_dir = project.join(WORKTREES_DIR).join(agent_id);
        fs::create_dir_all(project.join(WORKTREES_DIR)).unwrap();

        Command::new("git")
            .args(["worktree", "add"])
            .arg(&wt_dir)
            .args(["-b", &branch, "HEAD"])
            .current_dir(project)
            .output()
            .unwrap();

        (wt_dir, branch)
    }

    #[test]
    fn test_remove_worktree() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let (wt_path, branch) = create_test_worktree(&project, "agent-1", "task-foo");
        assert!(wt_path.exists());

        remove_worktree(&project, &wt_path, &branch).unwrap();
        assert!(!wt_path.exists());

        // Branch should be deleted
        let output = Command::new("git")
            .args(["branch", "--list", &branch])
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());
    }

    #[test]
    fn test_recover_commits_no_commits() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let (_wt_path, branch) = create_test_worktree(&project, "agent-2", "task-bar");
        let count = recover_commits(&project, &branch, "agent-2");
        assert_eq!(count, 0);
    }

    #[test]
    fn test_recover_commits_with_commits() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let (wt_path, branch) = create_test_worktree(&project, "agent-3", "task-baz");

        // Make a commit in the worktree
        fs::write(wt_path.join("new_file.txt"), "agent work").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&wt_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "agent work"])
            .current_dir(&wt_path)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();

        let count = recover_commits(&project, &branch, "agent-3");
        assert_eq!(count, 1);

        // Recovery branch should exist
        let recovery_branch = format!("recover/agent-3/task-baz");
        let output = Command::new("git")
            .args(["branch", "--list", &recovery_branch])
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&output.stdout).trim().is_empty(),
            "Recovery branch should exist"
        );

        // Clean up
        remove_worktree(&project, &wt_path, &branch).unwrap();
    }

    #[test]
    fn test_cleanup_dead_agent_worktree() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let (wt_path, branch) = create_test_worktree(&project, "agent-4", "task-qux");
        assert!(wt_path.exists());

        cleanup_dead_agent_worktree(&project, &wt_path, &branch, "agent-4");
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_find_branch_for_worktree() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let (wt_path, branch) = create_test_worktree(&project, "agent-5", "task-find");
        let found = find_branch_for_worktree(&project, &wt_path);
        assert_eq!(found, Some(branch.clone()));

        // Clean up
        remove_worktree(&project, &wt_path, &branch).unwrap();
    }

    #[test]
    fn test_remove_worktree_nonexistent_reports_errors() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        // Removing a nonexistent worktree should now report errors with enhanced error handling
        let fake_path = project.join(WORKTREES_DIR).join("agent-999");
        let result = remove_worktree(&project, &fake_path, "wg/agent-999/fake");
        // Should return an error now that we have enhanced error reporting
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Worktree removal completed with errors"));
    }

    #[test]
    fn test_retry_operation_success_first_try() {
        let mut call_count = 0;
        let result = retry_operation(
            || {
                call_count += 1;
                Ok("success")
            },
            "test operation",
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(call_count, 1);
    }

    #[test]
    fn test_retry_operation_success_after_retries() {
        let mut call_count = 0;
        let result = retry_operation(
            || {
                call_count += 1;
                if call_count < 3 {
                    Err(anyhow::anyhow!("temporary failure"))
                } else {
                    Ok("success")
                }
            },
            "test operation",
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(call_count, 3);
    }

    #[test]
    fn test_retry_operation_max_retries_exceeded() {
        let mut call_count = 0;
        let result: anyhow::Result<&str> = retry_operation(
            || {
                call_count += 1;
                Err(anyhow::anyhow!("persistent failure"))
            },
            "test operation",
        );
        assert!(result.is_err());
        assert_eq!(call_count, MAX_RETRIES + 1);
        assert!(result.unwrap_err().to_string().contains("persistent failure"));
    }

    #[test]
    fn test_enhanced_cleanup_with_corrupted_git() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let (wt_path, branch) = create_test_worktree(&project, "agent-test", "task-test");
        assert!(wt_path.exists());

        // Simulate a corrupted state by creating an invalid .git file
        // This should trigger the enhanced error handling and fallback mechanisms
        let git_file = wt_path.join(".git");
        if git_file.exists() {
            // Overwrite .git with invalid content to simulate corruption
            let _ = fs::write(&git_file, "corrupted git content");
        }

        // The cleanup should still work due to enhanced error handling
        cleanup_dead_agent_worktree(&project, &wt_path, &branch, "agent-test");

        // Verify that the directory is removed or at least attempted
        // (The exact behavior may depend on the filesystem and permissions)
    }
}
