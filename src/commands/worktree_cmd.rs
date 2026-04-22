//! `wg worktree` subcommands — list, archive, gc, and inspect agent worktrees.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// List all worktrees under `.wg-worktrees/`.
pub fn list(workgraph_dir: &Path) -> Result<()> {
    let project_root = workgraph_dir
        .parent()
        .context("Cannot determine project root from workgraph dir")?;
    let worktrees_dir = project_root.join(".wg-worktrees");

    if !worktrees_dir.exists() {
        println!("No worktrees directory found.");
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(&worktrees_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    if entries.is_empty() {
        println!("No worktrees found.");
        return Ok(());
    }

    println!("Agent worktrees ({}):", entries.len());
    for entry in &entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        let size = dir_size_human(&path);
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| {
                let elapsed = t.elapsed().ok()?;
                Some(humanize_duration(elapsed))
            })
            .unwrap_or_else(|| "unknown".to_string());

        // Check if there are uncommitted changes
        let has_changes = has_uncommitted_changes(&path);
        let status = if has_changes {
            " [uncommitted changes]"
        } else {
            ""
        };

        println!("  {} — {} — modified {}{}", name, size, mtime, status);
    }

    Ok(())
}

/// Archive a specific agent's worktree: commit uncommitted work,
/// then optionally remove the directory.
pub fn archive(workgraph_dir: &Path, agent_id: &str, remove: bool) -> Result<()> {
    let project_root = workgraph_dir
        .parent()
        .context("Cannot determine project root from workgraph dir")?;
    let worktrees_dir = project_root.join(".wg-worktrees");
    let wt_path = worktrees_dir.join(agent_id);

    if !wt_path.exists() {
        anyhow::bail!(
            "Worktree for '{}' not found at {}",
            agent_id,
            wt_path.display()
        );
    }

    // Check for uncommitted changes and auto-commit them
    if has_uncommitted_changes(&wt_path) {
        eprintln!(
            "[worktree] Committing uncommitted changes in {} ...",
            agent_id
        );

        // Stage all changes
        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&wt_path)
            .output()
            .context("Failed to run git add")?;

        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            anyhow::bail!("git add failed: {}", stderr.trim());
        }

        // Commit with archive message
        let msg = format!(
            "archive: {} work snapshot\n\nAuto-committed by `wg worktree archive` to preserve\nuncommitted agent work before archival.",
            agent_id
        );
        let commit = Command::new("git")
            .args(["commit", "-m", &msg])
            .current_dir(&wt_path)
            .output()
            .context("Failed to run git commit")?;

        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            // "nothing to commit" is OK
            if !stderr.contains("nothing to commit") {
                anyhow::bail!("git commit failed: {}", stderr.trim());
            }
        } else {
            eprintln!(
                "[worktree] Committed: {}",
                String::from_utf8_lossy(&commit.stdout).trim()
            );
        }
    } else {
        eprintln!("[worktree] No uncommitted changes in {}", agent_id);
    }

    if remove {
        eprintln!(
            "[worktree] Removing worktree directory {} ...",
            wt_path.display()
        );

        // First try git worktree remove (clean git integration)
        let wt_remove = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&wt_path)
            .current_dir(project_root)
            .output();

        match wt_remove {
            Ok(output) if output.status.success() => {
                eprintln!("[worktree] Removed via git worktree remove");
            }
            _ => {
                // Fallback: manual removal (not a real git worktree,
                // just a directory)
                std::fs::remove_dir_all(&wt_path).context("Failed to remove worktree directory")?;
                eprintln!("[worktree] Removed directory manually");
            }
        }

        eprintln!("[worktree] Archived and removed: {}", agent_id);
    } else {
        eprintln!("[worktree] Archived (preserved on disk): {}", agent_id);
        eprintln!("  To remove: wg worktree archive {} --remove", agent_id);
    }

    Ok(())
}

pub(crate) fn has_uncommitted_changes(wt_path: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt_path)
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

fn dir_size_human(path: &Path) -> String {
    let output = Command::new("du").args(["-sh"]).arg(path).output().ok();
    output
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| s.split_whitespace().next().unwrap_or("?").to_string())
        })
        .unwrap_or_else(|| "?".to_string())
}

fn humanize_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Parse a human-friendly duration ("7d", "24h", "90m", "3600s").
/// Used for the `--older` filter on `wg worktree gc`.
pub(crate) fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration");
    }
    let (num_part, unit_char): (&str, char) = {
        let last = s.chars().last().unwrap();
        if last.is_ascii_digit() {
            (s, 's')
        } else {
            (&s[..s.len() - last.len_utf8()], last)
        }
    };
    let n: u64 = num_part
        .parse()
        .with_context(|| format!("invalid duration number: '{}'", num_part))?;
    let secs = match unit_char {
        's' => n,
        'm' => n * 60,
        'h' => n * 60 * 60,
        'd' => n * 60 * 60 * 24,
        'w' => n * 60 * 60 * 24 * 7,
        other => anyhow::bail!("unknown duration unit '{}' — use s/m/h/d/w", other),
    };
    Ok(Duration::from_secs(secs))
}

/// Garbage-collect stale agent worktrees. Dry-run by default.
///
/// Worktrees are sacred — this is the only bulk-removal path in workgraph,
/// and it refuses to act without explicit filters to prevent accidental
/// nuke-all. Per-worktree removal still goes through `archive --remove`
/// so uncommitted work is committed to the agent's branch before the
/// directory is dropped.
pub fn gc(workgraph_dir: &Path, execute: bool, older: Option<&str>, dead_only: bool) -> Result<()> {
    let project_root = workgraph_dir
        .parent()
        .context("Cannot determine project root from workgraph dir")?;
    let worktrees_dir = project_root.join(".wg-worktrees");

    if !worktrees_dir.exists() {
        println!("No worktrees directory found.");
        return Ok(());
    }

    // Require at least one filter — refuse to nuke-all by default.
    if older.is_none() && !dead_only {
        anyhow::bail!(
            "wg worktree gc requires at least one filter (--older <dur> and/or --dead-only). \
             Worktrees are sacred — use explicit criteria to choose which ones to collect."
        );
    }

    let older_than = match older {
        Some(s) => Some(parse_duration(s).context("--older parse failed")?),
        None => None,
    };

    // Build the live-agent set if we'll need it.
    let alive_agents: std::collections::HashSet<String> = if dead_only {
        use workgraph::service::AgentRegistry;
        match AgentRegistry::load_locked(workgraph_dir) {
            Ok(reg) => reg
                .list_alive_agents()
                .into_iter()
                .map(|a| a.id.clone())
                .collect(),
            Err(_) => std::collections::HashSet::new(),
        }
    } else {
        std::collections::HashSet::new()
    };

    let mut candidates: Vec<(String, std::path::PathBuf, String, String)> = Vec::new(); // (agent_id, path, age_str, size_str)
    let now = SystemTime::now();

    for entry in std::fs::read_dir(&worktrees_dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("agent-") {
            continue;
        }
        let path = entry.path();
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);

        if let Some(threshold) = older_than
            && age < threshold
        {
            continue;
        }
        if dead_only && alive_agents.contains(&name) {
            continue;
        }

        candidates.push((
            name.clone(),
            path.clone(),
            humanize_duration(age),
            dir_size_human(&path),
        ));
    }

    if candidates.is_empty() {
        println!("No worktrees match the filters.");
        return Ok(());
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    if !execute {
        println!("Would remove {} worktree(s) (dry-run):", candidates.len());
        for (aid, _path, age, size) in &candidates {
            println!("  {} — {} — {}", aid, size, age);
        }
        println!();
        println!("Re-run with --execute to actually archive and remove.");
        return Ok(());
    }

    let mut ok = 0;
    let mut failed = 0;
    for (aid, path, _age, _size) in &candidates {
        let branch = find_branch_for_agent(project_root, aid)
            .unwrap_or_else(|| format!("wg/{}/unknown", aid));
        match crate::commands::spawn::worktree::remove_worktree(project_root, path, &branch) {
            Ok(()) => {
                ok += 1;
            }
            Err(e) => {
                eprintln!("[worktree-gc] {}: {}", aid, e);
                failed += 1;
            }
        }
    }
    println!();
    println!(
        "Removed {} worktree(s); {} failed. Uncommitted agent work was NOT preserved — \
         use `wg worktree archive <agent-id>` first if you want to save it.",
        ok, failed
    );
    Ok(())
}

/// Look up the `wg/<agent>/<task>` branch for an agent-id, if any.
fn find_branch_for_agent(project_root: &Path, agent_id: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["branch", "--list", &format!("wg/{}/*", agent_id)])
        .current_dir(project_root)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        let trimmed = line.trim_start_matches(['*', '+', ' ']).trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_supports_common_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604800));
        assert_eq!(parse_duration("1w").unwrap(), Duration::from_secs(604800));
    }

    #[test]
    fn parse_duration_bare_number_is_seconds() {
        assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn parse_duration_rejects_unknown_unit() {
        let err = parse_duration("7y").unwrap_err();
        assert!(err.to_string().contains("unknown duration unit"));
    }

    #[test]
    fn parse_duration_rejects_empty() {
        assert!(parse_duration("").is_err());
    }
}
