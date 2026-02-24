use std::fs;
use std::path::{Path, PathBuf};

use super::store::AgencyError;
use super::types::ArtifactEntry;

/// Capture a snapshot of what an agent produced for a completed task.
///
/// Writes three files into `{wg_dir}/output/{task_id}/`:
///   - `changes.patch` — git diff from claim time (`started_at`) to now
///   - `artifacts.json` — JSON list of registered artifacts with file sizes
///   - `log.json` — full progress log entries
///
/// This is a mechanical operation — no LLM is involved. The coordinator calls
/// this after marking a task done but before creating any evaluation task.
/// The evaluator reads from `{wg_dir}/output/{task_id}/` to assess work.
///
/// Errors are non-fatal: individual capture steps log warnings and continue.
pub fn capture_task_output(
    wg_dir: &Path,
    task: &crate::graph::Task,
) -> Result<PathBuf, AgencyError> {
    let output_dir = wg_dir.join("output").join(&task.id);
    fs::create_dir_all(&output_dir)?;

    // 1. Git diff capture
    capture_git_diff(&output_dir, task);

    // 2. Artifact manifest
    capture_artifact_manifest(&output_dir, task);

    // 3. Log snapshot
    capture_log_snapshot(&output_dir, task);

    Ok(output_dir)
}

/// Capture git diff from task claim time to now, saved as changes.patch.
///
/// Uses `started_at` as the since-timestamp for `git diff`. If the project
/// is not a git repo or the diff fails, writes an empty patch with a comment.
fn capture_git_diff(output_dir: &Path, task: &crate::graph::Task) {
    let patch_path = output_dir.join("changes.patch");

    // Find the project root by walking up from the .workgraph dir
    let project_root = output_dir.ancestors().find(|p| p.join(".git").exists());

    let project_root = match project_root {
        Some(root) => root.to_path_buf(),
        None => {
            // Not a git repo — write an empty patch with explanation
            if let Err(e) = fs::write(&patch_path, "# Not a git repository — no diff captured\n")
            {
                eprintln!(
                    "Warning: failed to write patch file {}: {}",
                    patch_path.display(),
                    e
                );
            }
            return;
        }
    };

    // Build the git diff command.
    // If we have a started_at timestamp, find the commit closest to that time
    // and diff from there to the current working tree (including uncommitted).
    let output = if let Some(ref started_at) = task.started_at {
        // Find the last commit before the task was claimed
        let rev_result = std::process::Command::new("git")
            .args([
                "rev-list",
                "-1",
                &format!("--before={}", started_at),
                "HEAD",
            ])
            .current_dir(&project_root)
            .output();

        match rev_result {
            Ok(rev_output) if rev_output.status.success() => {
                let base_rev = String::from_utf8_lossy(&rev_output.stdout)
                    .trim()
                    .to_string();

                if base_rev.is_empty() {
                    // No commit before started_at — diff entire working tree
                    std::process::Command::new("git")
                        .args(["diff", "HEAD"])
                        .current_dir(&project_root)
                        .output()
                } else {
                    // Diff from base revision to current working tree
                    std::process::Command::new("git")
                        .args(["diff", &base_rev])
                        .current_dir(&project_root)
                        .output()
                }
            }
            _ => {
                // rev-list failed — fall back to uncommitted changes only
                std::process::Command::new("git")
                    .args(["diff", "HEAD"])
                    .current_dir(&project_root)
                    .output()
            }
        }
    } else {
        // No started_at — just capture uncommitted changes
        std::process::Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(&project_root)
            .output()
    };

    match output {
        Ok(out) if out.status.success() => {
            let diff = String::from_utf8_lossy(&out.stdout);
            if diff.is_empty() {
                if let Err(e) = fs::write(&patch_path, "# No changes detected in git diff\n") {
                    eprintln!(
                        "Warning: failed to write patch file {}: {}",
                        patch_path.display(),
                        e
                    );
                }
            } else if let Err(e) = fs::write(&patch_path, diff.as_bytes()) {
                eprintln!(
                    "Warning: failed to write patch file {}: {}",
                    patch_path.display(),
                    e
                );
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if let Err(e) = fs::write(
                &patch_path,
                format!("# git diff failed: {}\n", stderr.trim()),
            ) {
                eprintln!(
                    "Warning: failed to write patch file {}: {}",
                    patch_path.display(),
                    e
                );
            }
        }
        Err(e) => {
            if let Err(write_err) = fs::write(&patch_path, format!("# git diff failed: {}\n", e)) {
                eprintln!(
                    "Warning: failed to write patch file {}: {}",
                    patch_path.display(),
                    write_err
                );
            }
        }
    }
}

/// Capture artifact manifest as artifacts.json — a JSON list of registered
/// artifacts with their file paths and sizes.
fn capture_artifact_manifest(output_dir: &Path, task: &crate::graph::Task) {
    let manifest_path = output_dir.join("artifacts.json");

    // Find project root for resolving relative artifact paths
    let project_root = output_dir
        .ancestors()
        .find(|p| p.join(".git").exists())
        .map(std::path::Path::to_path_buf);

    let entries: Vec<ArtifactEntry> = task
        .artifacts
        .iter()
        .map(|artifact_path| {
            // Try to get file size — resolve relative paths from project root
            let full_path = if Path::new(artifact_path).is_absolute() {
                PathBuf::from(artifact_path)
            } else if let Some(ref root) = project_root {
                root.join(artifact_path)
            } else {
                PathBuf::from(artifact_path)
            };

            let size = fs::metadata(&full_path).ok().map(|m| m.len());

            ArtifactEntry {
                path: artifact_path.clone(),
                size,
            }
        })
        .collect();

    match serde_json::to_string_pretty(&entries) {
        Ok(json) => {
            if let Err(e) = fs::write(&manifest_path, json) {
                eprintln!(
                    "Warning: failed to write artifact manifest {}: {}",
                    manifest_path.display(),
                    e
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to serialize artifact manifest: {}", e);
        }
    }
}

/// Capture the full progress log as log.json.
fn capture_log_snapshot(output_dir: &Path, task: &crate::graph::Task) {
    let log_path = output_dir.join("log.json");

    match serde_json::to_string_pretty(&task.log) {
        Ok(json) => {
            if let Err(e) = fs::write(&log_path, json) {
                eprintln!(
                    "Warning: failed to write log snapshot {}: {}",
                    log_path.display(),
                    e
                );
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to serialize log snapshot: {}", e);
        }
    }
}
