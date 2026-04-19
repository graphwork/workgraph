//! Manual cleanup commands for edge case recovery.
//!
//! Provides commands to manually clean up orphaned worktrees, recovery branches,
//! and other edge cases that may not be handled by automatic cleanup operations.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use clap::Parser;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use workgraph::graph::{Status, WorkGraph};

use super::load_workgraph;
use crate::commands::service::worktree::{WORKTREES_DIR, remove_worktree, verify_worktree_cleanup};

/// Parse an ISO 8601 timestamp string to SystemTime
#[allow(dead_code)]
fn parse_timestamp_to_systemtime(timestamp_opt: &Option<String>) -> Option<SystemTime> {
    let timestamp_str = timestamp_opt.as_ref()?;
    let dt = timestamp_str.parse::<DateTime<Utc>>().ok()?;
    let duration_since_epoch = dt.timestamp() as u64;
    Some(UNIX_EPOCH + Duration::from_secs(duration_since_epoch))
}

/// Manual cleanup commands for edge case recovery
#[derive(Parser, Debug)]
pub struct CleanupArgs {
    #[clap(subcommand)]
    pub subcmd: CleanupSubcommand,
}

#[derive(Parser, Debug)]
pub enum CleanupSubcommand {
    /// Clean up orphaned worktrees that have no corresponding agent metadata
    Orphaned(OrphanedArgs),
    /// Clean up old recovery branches
    RecoveryBranches(RecoveryBranchesArgs),
    /// Comprehensive nightly cleanup for task hygiene and system maintenance
    Nightly(NightlyArgs),
}

#[derive(Parser, Debug)]
pub struct OrphanedArgs {
    /// Actually perform the cleanup (dry-run by default)
    #[clap(long)]
    pub execute: bool,

    /// Force cleanup even if errors occur
    #[clap(long)]
    pub force: bool,

    /// Directory to search for orphaned worktrees (defaults to current directory)
    #[clap(long)]
    pub dir: Option<String>,
}

#[derive(Parser, Debug)]
pub struct RecoveryBranchesArgs {
    /// Maximum age of recovery branches to keep (in days)
    #[clap(long, default_value = "30")]
    pub max_age_days: u32,

    /// Actually perform the cleanup (dry-run by default)
    #[clap(long)]
    pub execute: bool,

    /// Force cleanup even if errors occur
    #[clap(long)]
    pub force: bool,

    /// Directory containing the git repository (defaults to current directory)
    #[clap(long)]
    pub dir: Option<String>,
}

#[derive(Parser, Debug)]
pub struct NightlyArgs {
    /// Actually perform the cleanup (dry-run by default)
    #[clap(long)]
    pub execute: bool,

    /// Force cleanup even if errors occur
    #[clap(long)]
    pub force: bool,

    /// Maximum age of abandoned tasks to archive (in days)
    #[clap(long, default_value = "7")]
    pub max_abandoned_age_days: u32,

    /// Maximum age of failed tasks to archive (in days)
    #[clap(long, default_value = "3")]
    pub max_failed_age_days: u32,

    /// Skip task cleanup (only do file system cleanup)
    #[clap(long)]
    pub skip_tasks: bool,

    /// Skip file system cleanup (only do task cleanup)
    #[clap(long)]
    pub skip_files: bool,

    /// Cleanup mode: conservative (safe operations only) or aggressive (full cleanup)
    #[clap(long, default_value = "conservative", value_enum)]
    pub mode: CleanupMode,
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum CleanupMode {
    Conservative,
    Aggressive,
}

pub fn run(args: CleanupArgs) -> Result<()> {
    match args.subcmd {
        CleanupSubcommand::Orphaned(orphaned_args) => run_orphaned_cleanup(orphaned_args),
        CleanupSubcommand::RecoveryBranches(recovery_args) => {
            run_recovery_branches_cleanup(recovery_args)
        }
        CleanupSubcommand::Nightly(nightly_args) => run_nightly_cleanup(nightly_args),
    }
}

/// Clean up orphaned worktrees that have no corresponding agent metadata
fn run_orphaned_cleanup(args: OrphanedArgs) -> Result<()> {
    let project_root = if let Some(dir) = args.dir {
        std::path::PathBuf::from(dir)
    } else {
        std::env::current_dir().context("Failed to get current directory")?
    };

    println!(
        "Scanning for orphaned worktrees in: {}",
        project_root.display()
    );

    // Load workgraph to verify project structure
    let (_graph, _graph_path) = load_workgraph(&project_root)?;

    let worktrees_dir = project_root.join(WORKTREES_DIR);
    if !worktrees_dir.exists() {
        println!(
            "No worktrees directory found at: {}",
            worktrees_dir.display()
        );
        return Ok(());
    }

    let agents_dir = project_root.join(".workgraph").join("agents");

    // Get list of active agents from metadata
    let mut active_agents = HashSet::new();
    if agents_dir.exists() {
        for entry in fs::read_dir(&agents_dir).context("Failed to read agents directory")? {
            let entry = entry.context("Failed to read agent directory entry")?;
            if entry.path().is_dir() {
                let agent_id = entry.file_name().to_string_lossy().to_string();

                // Check if agent has valid metadata
                let metadata_path = entry.path().join("metadata.json");
                if metadata_path.exists()
                    && let Ok(metadata_content) = fs::read_to_string(&metadata_path)
                    && serde_json::from_str::<serde_json::Value>(&metadata_content).is_ok()
                {
                    active_agents.insert(agent_id);
                }
            }
        }
    }

    println!(
        "Found {} active agents with valid metadata",
        active_agents.len()
    );

    // Scan worktrees directory for orphaned entries
    let mut orphaned_worktrees = Vec::new();
    for entry in fs::read_dir(&worktrees_dir).context("Failed to read worktrees directory")? {
        let entry = entry.context("Failed to read worktree directory entry")?;
        if entry.path().is_dir() {
            let worktree_name = entry.file_name().to_string_lossy().to_string();

            // Check if this worktree has a corresponding active agent
            if !active_agents.contains(&worktree_name) {
                orphaned_worktrees.push((worktree_name.clone(), entry.path()));
                println!(
                    "Found orphaned worktree: {} -> {}",
                    worktree_name,
                    entry.path().display()
                );
            }
        }
    }

    if orphaned_worktrees.is_empty() {
        println!("No orphaned worktrees found.");
        return Ok(());
    }

    println!("Found {} orphaned worktree(s)", orphaned_worktrees.len());

    if !args.execute {
        println!("\nDry-run mode. Use --execute to actually perform cleanup.");
        println!("Use --force to continue cleanup even if individual operations fail.");
        return Ok(());
    }

    // Perform cleanup
    let mut cleanup_errors = Vec::new();
    let mut cleanup_successes = 0;

    for (agent_id, worktree_path) in orphaned_worktrees {
        println!("Cleaning up orphaned worktree: {}", agent_id);

        // Try to determine the branch name
        let branch = format!("wg/{}/task", agent_id);

        match cleanup_orphaned_worktree(&project_root, &worktree_path, &branch) {
            Ok(()) => {
                cleanup_successes += 1;
                println!("✓ Successfully cleaned up orphaned worktree: {}", agent_id);
            }
            Err(e) => {
                let error_msg = format!("Failed to clean up orphaned worktree {}: {}", agent_id, e);
                cleanup_errors.push(error_msg.clone());

                if args.force {
                    eprintln!("⚠ {}", error_msg);
                    eprintln!("  Continuing due to --force flag...");
                } else {
                    return Err(anyhow!(error_msg));
                }
            }
        }
    }

    println!("\nCleanup complete:");
    println!("  Successes: {}", cleanup_successes);
    println!("  Errors: {}", cleanup_errors.len());

    if !cleanup_errors.is_empty() && args.force {
        println!("\nErrors encountered (ignored due to --force):");
        for error in cleanup_errors {
            println!("  - {}", error);
        }
    }

    Ok(())
}

/// Clean up old recovery branches
fn run_recovery_branches_cleanup(args: RecoveryBranchesArgs) -> Result<()> {
    let project_root = if let Some(dir) = args.dir {
        std::path::PathBuf::from(dir)
    } else {
        std::env::current_dir().context("Failed to get current directory")?
    };

    println!(
        "Scanning for old recovery branches in: {}",
        project_root.display()
    );

    // Verify this is a git repository
    if !project_root.join(".git").exists() {
        return Err(anyhow!("Not a git repository: {}", project_root.display()));
    }

    // Get list of recovery branches
    let output = Command::new("git")
        .args(["branch", "-a"])
        .current_dir(&project_root)
        .output()
        .context("Failed to list git branches")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Git branch listing failed: {}", stderr));
    }

    let branches_output = String::from_utf8_lossy(&output.stdout);
    let recovery_branches: Vec<&str> = branches_output
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("recover/"))
        .map(|line| line.trim_start_matches("* "))
        .collect();

    if recovery_branches.is_empty() {
        println!("No recovery branches found.");
        return Ok(());
    }

    println!("Found {} recovery branch(es)", recovery_branches.len());

    // Check age of recovery branches
    let mut old_branches = Vec::new();
    let max_age_seconds = args.max_age_days as i64 * 24 * 3600;

    for branch in recovery_branches {
        // Get branch creation time (last commit time on the branch)
        let output = Command::new("git")
            .args(["log", "-1", "--format=%ct", branch])
            .current_dir(&project_root)
            .output();

        if let Ok(output) = output
            && output.status.success()
        {
            let timestamp_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Ok(timestamp) = timestamp_str.parse::<i64>() {
                let age_seconds = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
                    - timestamp;

                if age_seconds > max_age_seconds {
                    let age_days = age_seconds / (24 * 3600);
                    old_branches.push((branch.to_string(), age_days));
                    println!(
                        "Found old recovery branch: {} (age: {} days)",
                        branch, age_days
                    );
                }
            }
        }
    }

    if old_branches.is_empty() {
        println!(
            "No recovery branches older than {} days found.",
            args.max_age_days
        );
        return Ok(());
    }

    println!(
        "Found {} recovery branch(es) older than {} days",
        old_branches.len(),
        args.max_age_days
    );

    if !args.execute {
        println!("\nDry-run mode. Use --execute to actually perform cleanup.");
        println!("Use --force to continue cleanup even if individual operations fail.");
        return Ok(());
    }

    // Perform cleanup
    let mut cleanup_errors = Vec::new();
    let mut cleanup_successes = 0;

    for (branch, age_days) in old_branches {
        println!(
            "Deleting recovery branch: {} (age: {} days)",
            branch, age_days
        );

        let output = Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(&project_root)
            .output()
            .context("Failed to execute git branch delete command")?;

        if output.status.success() {
            cleanup_successes += 1;
            println!("✓ Successfully deleted recovery branch: {}", branch);
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error_msg = format!(
                "Failed to delete recovery branch {}: {}",
                branch,
                stderr.trim()
            );
            cleanup_errors.push(error_msg.clone());

            if args.force {
                eprintln!("⚠ {}", error_msg);
                eprintln!("  Continuing due to --force flag...");
            } else {
                return Err(anyhow!(error_msg));
            }
        }
    }

    println!("\nCleanup complete:");
    println!("  Successes: {}", cleanup_successes);
    println!("  Errors: {}", cleanup_errors.len());

    if !cleanup_errors.is_empty() && args.force {
        println!("\nErrors encountered (ignored due to --force):");
        for error in cleanup_errors {
            println!("  - {}", error);
        }
    }

    Ok(())
}

/// Clean up a specific orphaned worktree with enhanced error handling
fn cleanup_orphaned_worktree(
    project_root: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<()> {
    // Try standard cleanup first
    match remove_worktree(project_root, worktree_path, branch) {
        Ok(()) => {
            // Verify cleanup was successful
            verify_worktree_cleanup(worktree_path, branch, project_root)?;
            return Ok(());
        }
        Err(e) => {
            eprintln!(
                "[cleanup] Standard removal failed for {:?}: {}",
                worktree_path, e
            );
            eprintln!("[cleanup] Attempting fallback cleanup...");
        }
    }

    // Fallback: manual cleanup with enhanced error handling
    attempt_manual_worktree_cleanup(project_root, worktree_path, branch)
}

/// Attempt manual cleanup of a worktree with permission-aware error handling
fn attempt_manual_worktree_cleanup(
    project_root: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<()> {
    let mut cleanup_errors = Vec::new();

    // Step 1: Clean up .workgraph symlink with permission handling
    let wg_symlink = worktree_path.join(".workgraph");
    if wg_symlink.exists() {
        match fs::remove_file(&wg_symlink) {
            Ok(()) => {
                eprintln!("[cleanup] Successfully removed .workgraph symlink");
            }
            Err(e) => {
                let error_msg = format!("Failed to remove .workgraph symlink: {}", e);
                cleanup_errors.push(error_msg.clone());
                eprintln!("[cleanup] {}", error_msg);

                // Try to fix permissions and retry
                if let Err(perm_err) = fix_permissions_and_retry_removal(&wg_symlink) {
                    cleanup_errors.push(format!("Permission fix also failed: {}", perm_err));
                } else {
                    eprintln!(
                        "[cleanup] Successfully removed .workgraph symlink after permission fix"
                    );
                }
            }
        }
    }

    // Step 2: Clean up target directory with permission handling
    let target_dir = worktree_path.join("target");
    if target_dir.exists() {
        match fs::remove_dir_all(&target_dir) {
            Ok(()) => {
                eprintln!("[cleanup] Successfully removed target directory");
            }
            Err(e) => {
                let error_msg = format!("Failed to remove target directory: {}", e);
                cleanup_errors.push(error_msg.clone());
                eprintln!("[cleanup] {}", error_msg);

                // Try to fix permissions and retry
                if let Err(perm_err) = fix_directory_permissions_and_retry(&target_dir) {
                    cleanup_errors.push(format!(
                        "Target directory permission fix failed: {}",
                        perm_err
                    ));
                } else {
                    eprintln!(
                        "[cleanup] Successfully removed target directory after permission fix"
                    );
                }
            }
        }
    }

    // Step 3: Try git worktree remove
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

    // Step 4: Try to remove the branch
    let output = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(project_root)
        .output()
        .context("Failed to execute git branch delete command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_errors.push(format!("Git branch delete failed: {}", stderr.trim()));
    }

    // Step 5: Final directory cleanup
    if worktree_path.exists() {
        match fs::remove_dir_all(worktree_path) {
            Ok(()) => {
                eprintln!("[cleanup] Successfully removed worktree directory");
            }
            Err(e) => {
                let error_msg = format!("Failed to remove worktree directory: {}", e);
                cleanup_errors.push(error_msg.clone());
                eprintln!("[cleanup] {}", error_msg);

                // Final attempt with permission fixes
                if let Err(perm_err) = fix_directory_permissions_and_retry(worktree_path) {
                    cleanup_errors.push(format!("Final directory cleanup failed: {}", perm_err));
                } else {
                    eprintln!(
                        "[cleanup] Successfully removed worktree directory after permission fix"
                    );
                }
            }
        }
    }

    if cleanup_errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "Manual cleanup completed with errors:\n{}",
            cleanup_errors.join("\n")
        ))
    }
}

/// Fix permissions on a file and retry removal
fn fix_permissions_and_retry_removal(file_path: &Path) -> Result<()> {
    // Try to make the file writable
    if let Ok(metadata) = fs::metadata(file_path) {
        let mut perms = metadata.permissions();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o644); // Read/write for owner, read for others
        }
        #[cfg(not(unix))]
        {
            perms.set_readonly(false);
        }

        if let Err(e) = fs::set_permissions(file_path, perms) {
            return Err(anyhow!("Failed to fix file permissions: {}", e));
        }

        // Retry removal
        fs::remove_file(file_path).context("Failed to remove file after permission fix")?;
    }

    Ok(())
}

/// Fix permissions on a directory and its contents, then retry removal
fn fix_directory_permissions_and_retry(dir_path: &Path) -> Result<()> {
    if !dir_path.exists() {
        return Ok(());
    }

    // Recursively fix permissions
    fn fix_permissions_recursive(path: &Path) -> Result<()> {
        if path.is_dir() {
            // Make directory executable/readable
            if let Ok(metadata) = fs::metadata(path) {
                let mut perms = metadata.permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    perms.set_mode(0o755);
                }
                #[cfg(not(unix))]
                {
                    perms.set_readonly(false);
                }
                let _ = fs::set_permissions(path, perms);
            }

            // Fix permissions for all entries
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    fix_permissions_recursive(&entry.path())?;
                }
            }
        } else {
            // Make file writable
            if let Ok(metadata) = fs::metadata(path) {
                let mut perms = metadata.permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    perms.set_mode(0o644);
                }
                #[cfg(not(unix))]
                {
                    perms.set_readonly(false);
                }
                let _ = fs::set_permissions(path, perms);
            }
        }
        Ok(())
    }

    fix_permissions_recursive(dir_path).context("Failed to fix directory permissions")?;

    // Retry removal
    fs::remove_dir_all(dir_path).context("Failed to remove directory after permission fix")?;

    Ok(())
}

/// Comprehensive nightly cleanup for task hygiene and system maintenance
fn run_nightly_cleanup(args: NightlyArgs) -> Result<()> {
    println!("Nightly cleanup requested (mode: {:?})", args.mode);

    let project_root = std::env::current_dir().context("Failed to get current directory")?;

    // Load workgraph for task cleanup
    let (graph, _graph_path) = load_workgraph(&project_root)?;

    // Initialize cleanup summary
    let mut summary = CleanupSummary::new();

    if !args.execute {
        println!("Dry-run mode - showing what would be cleaned:");
    }

    // Task cleanup (unless skipped)
    if !args.skip_tasks {
        cleanup_tasks(&graph, &args, &mut summary)?;
    }

    // File system cleanup (unless skipped)
    if !args.skip_files {
        cleanup_filesystem(&project_root, &args, &mut summary)?;
    }

    // Git cleanup (always run unless conservative mode and not execute)
    if args.mode == CleanupMode::Aggressive || args.execute {
        cleanup_git(&project_root, &args, &mut summary)?;
    }

    // Print summary of operations
    print_cleanup_summary(&summary);

    Ok(())
}

#[allow(dead_code)]
#[derive(Default)]
struct CleanupSummary {
    tasks_analyzed: usize,
    tasks_archived: usize,
    files_cleaned: usize,
    directories_cleaned: usize,
    git_operations: usize,
    errors: Vec<String>,
    disk_space_freed: u64,
}

impl CleanupSummary {
    #[allow(dead_code)]
    fn new() -> Self {
        Self::default()
    }
}

/// Clean up old and abandoned tasks
#[allow(dead_code)]
fn cleanup_tasks(
    graph: &WorkGraph,
    args: &NightlyArgs,
    summary: &mut CleanupSummary,
) -> Result<()> {
    println!("Scanning tasks for cleanup opportunities...");

    let tasks: Vec<_> = graph.tasks().collect();
    summary.tasks_analyzed = tasks.len();

    let now = SystemTime::now();
    let max_abandoned_age = Duration::from_secs(args.max_abandoned_age_days as u64 * 24 * 3600);
    let max_failed_age = Duration::from_secs(args.max_failed_age_days as u64 * 24 * 3600);

    let mut archive_candidates = Vec::new();

    for task in tasks {
        let should_archive = match &task.status {
            Status::Abandoned => {
                if let Some(created_at) = parse_timestamp_to_systemtime(&task.created_at) {
                    if let Ok(age) = now.duration_since(created_at) {
                        age > max_abandoned_age
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            Status::Failed => {
                if let Some(created_at) = parse_timestamp_to_systemtime(&task.created_at) {
                    if let Ok(age) = now.duration_since(created_at) {
                        age > max_failed_age
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            Status::Done => {
                // Archive completed agency tasks immediately
                is_agency_task(&task.id) || is_old_coordinator_task(&task.id)
            }
            _ => false,
        };

        if should_archive {
            let age_days = if let Some(created_at) = parse_timestamp_to_systemtime(&task.created_at)
            {
                now.duration_since(created_at)
                    .map(|d| d.as_secs() / (24 * 3600))
                    .unwrap_or(0)
            } else {
                0
            };

            archive_candidates.push((task.id.clone(), task.status, age_days));
        }
    }

    if archive_candidates.is_empty() {
        println!("No tasks need archiving.");
        return Ok(());
    }

    println!(
        "Found {} tasks eligible for archiving:",
        archive_candidates.len()
    );
    for (task_id, status, age_days) in &archive_candidates {
        println!("  - {} ({:?}, {} days old)", task_id, status, age_days);
    }

    if args.execute {
        for (task_id, _status, _age_days) in archive_candidates {
            if let Err(e) = archive_task(&task_id) {
                let error_msg = format!("Failed to archive task {}: {}", task_id, e);
                if args.force {
                    eprintln!("⚠ {}", error_msg);
                    summary.errors.push(error_msg);
                } else {
                    return Err(anyhow!(error_msg));
                }
            } else {
                summary.tasks_archived += 1;
                println!("✓ Archived task: {}", task_id);
            }
        }
    }

    Ok(())
}

/// Clean up temporary files and build artifacts
fn cleanup_filesystem(
    project_root: &Path,
    args: &NightlyArgs,
    summary: &mut CleanupSummary,
) -> Result<()> {
    println!("Scanning file system for cleanup opportunities...");

    let cleanup_targets = vec![
        ("tmp", Duration::from_secs(24 * 3600)), // 1 day
        ("temp", Duration::from_secs(24 * 3600)),
        ("target", Duration::from_secs(24 * 3600)), // Rust build artifacts
        (".workgraph/logs", Duration::from_secs(30 * 24 * 3600)), // 30 days
    ];

    for (dir_name, max_age) in cleanup_targets {
        let target_path = project_root.join(dir_name);
        if target_path.exists() {
            match cleanup_directory(&target_path, max_age, args) {
                Ok((files_cleaned, dirs_cleaned, space_freed)) => {
                    summary.files_cleaned += files_cleaned;
                    summary.directories_cleaned += dirs_cleaned;
                    summary.disk_space_freed += space_freed;
                    if files_cleaned > 0 || dirs_cleaned > 0 {
                        println!(
                            "✓ Cleaned {}: {} files, {} dirs, {} bytes freed",
                            dir_name, files_cleaned, dirs_cleaned, space_freed
                        );
                    }
                }
                Err(e) => {
                    let error_msg = format!("Failed to clean directory {}: {}", dir_name, e);
                    if args.force {
                        eprintln!("⚠ {}", error_msg);
                        summary.errors.push(error_msg);
                    } else {
                        return Err(anyhow!(error_msg));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Clean up git-related artifacts
fn cleanup_git(
    project_root: &Path,
    args: &NightlyArgs,
    summary: &mut CleanupSummary,
) -> Result<()> {
    println!("Performing git cleanup operations...");

    // Git garbage collection
    if args.execute {
        let output = Command::new("git")
            .args(["gc", "--prune=now"])
            .current_dir(project_root)
            .output()
            .context("Failed to execute git gc")?;

        if output.status.success() {
            println!("✓ Git garbage collection completed");
            summary.git_operations += 1;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error_msg = format!("Git gc failed: {}", stderr.trim());
            if args.force {
                eprintln!("⚠ {}", error_msg);
                summary.errors.push(error_msg);
            } else {
                return Err(anyhow!(error_msg));
            }
        }
    }

    // Prune worktree references
    if args.execute {
        let output = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(project_root)
            .output()
            .context("Failed to execute git worktree prune")?;

        if output.status.success() {
            println!("✓ Git worktree pruning completed");
            summary.git_operations += 1;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error_msg = format!("Git worktree prune failed: {}", stderr.trim());
            if args.force {
                eprintln!("⚠ {}", error_msg);
                summary.errors.push(error_msg);
            } else {
                return Err(anyhow!(error_msg));
            }
        }
    }

    Ok(())
}

/// Check if a task is an agency-related task that should be archived when done
fn is_agency_task(task_id: &str) -> bool {
    task_id.starts_with(".evaluate-")
        || task_id.starts_with(".assign-")
        || task_id.starts_with(".flip-")
}

/// Check if a task is an old coordinator task that should be archived
fn is_old_coordinator_task(task_id: &str) -> bool {
    task_id.starts_with(".coordinator-")
        || task_id.starts_with(".archive-")
        || task_id.starts_with(".compact-")
}

/// Archive a specific task
fn archive_task(task_id: &str) -> Result<()> {
    // Use wg archive command
    let output = Command::new("wg")
        .args(["archive", task_id])
        .output()
        .context("Failed to execute wg archive command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Archive failed: {}", stderr.trim()));
    }

    Ok(())
}

/// Clean up a directory based on file age
fn cleanup_directory(
    dir_path: &Path,
    max_age: Duration,
    args: &NightlyArgs,
) -> Result<(usize, usize, u64)> {
    if !dir_path.exists() {
        return Ok((0, 0, 0));
    }

    let mut files_cleaned = 0;
    let mut dirs_cleaned = 0;
    let mut space_freed = 0u64;
    let now = SystemTime::now();

    fn cleanup_recursive(
        path: &Path,
        max_age: Duration,
        now: SystemTime,
        execute: bool,
        files: &mut usize,
        dirs: &mut usize,
        space: &mut u64,
    ) -> Result<()> {
        for entry in fs::read_dir(path).context("Failed to read directory")? {
            let entry = entry.context("Failed to read directory entry")?;
            let entry_path = entry.path();

            let metadata = entry.metadata().context("Failed to get file metadata")?;
            let modified_time = metadata.modified().unwrap_or(now);

            if let Ok(age) = now.duration_since(modified_time) {
                if age > max_age {
                    let file_size = metadata.len();

                    if entry_path.is_dir() {
                        if execute {
                            fs::remove_dir_all(&entry_path)
                                .context("Failed to remove directory")?;
                        }
                        *dirs += 1;
                        *space += file_size;
                    } else {
                        if execute {
                            fs::remove_file(&entry_path).context("Failed to remove file")?;
                        }
                        *files += 1;
                        *space += file_size;
                    }
                } else if entry_path.is_dir() {
                    // Recurse into subdirectories
                    cleanup_recursive(&entry_path, max_age, now, execute, files, dirs, space)?;
                }
            }
        }
        Ok(())
    }

    cleanup_recursive(
        dir_path,
        max_age,
        now,
        args.execute,
        &mut files_cleaned,
        &mut dirs_cleaned,
        &mut space_freed,
    )?;

    Ok((files_cleaned, dirs_cleaned, space_freed))
}

/// Print summary of cleanup operations
fn print_cleanup_summary(summary: &CleanupSummary) {
    println!("\n=== Cleanup Summary ===");
    println!("Tasks analyzed: {}", summary.tasks_analyzed);
    println!("Tasks archived: {}", summary.tasks_archived);
    println!("Files cleaned: {}", summary.files_cleaned);
    println!("Directories cleaned: {}", summary.directories_cleaned);
    println!("Git operations: {}", summary.git_operations);

    if summary.disk_space_freed > 0 {
        let freed_mb = summary.disk_space_freed / (1024 * 1024);
        println!("Disk space freed: {} MB", freed_mb);
    }

    if !summary.errors.is_empty() {
        println!("Errors encountered: {}", summary.errors.len());
        for error in &summary.errors {
            println!("  - {}", error);
        }
    }

    if summary.tasks_archived > 0 || summary.files_cleaned > 0 || summary.directories_cleaned > 0 {
        println!("✅ Cleanup completed successfully");
    } else {
        println!("✨ No cleanup needed - system is already clean");
    }
}
