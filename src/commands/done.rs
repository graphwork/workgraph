use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::graph::{
    LogEntry, Status, evaluate_cycle_iteration, parse_token_usage, parse_wg_tokens,
};
use workgraph::parser::save_graph;
use workgraph::query;
use workgraph::service::registry::AgentRegistry;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

/// Run a verify command in a shell, returning Ok(()) if it passes or an error if it fails/times out.
fn run_verify_command(verify_cmd: &str, project_root: &Path) -> Result<()> {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(verify_cmd)
        .current_dir(project_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn verify command: {}", verify_cmd))?;

    // Read stdout and stderr in background threads to prevent pipe buffer deadlock.
    // Without this, a child producing >64KB of output blocks on write and never exits.
    let stdout_handle = child.stdout.take().map(|s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::BufReader::new(s), &mut buf).ok();
            buf
        })
    });
    let stderr_handle = child.stderr.take().map(|s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::BufReader::new(s), &mut buf).ok();
            buf
        })
    });

    let timeout = Duration::from_secs(120);
    let start = Instant::now();

    // Poll with short sleeps to implement timeout without external crate
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!(
                        "Verify command timed out after {}s: {}",
                        timeout.as_secs(),
                        verify_cmd,
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                anyhow::bail!("Failed to wait on verify command: {}", e);
            }
        }
    };

    if status.success() {
        Ok(())
    } else {
        let stdout = stdout_handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr = stderr_handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        let mut combined = stderr;
        if !stdout.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&stdout);
        }
        let truncated: String = combined.chars().take(500).collect();
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        anyhow::bail!(
            "Verify command failed (exit code {}): {}\nOutput: {}",
            code,
            verify_cmd,
            truncated,
        );
    }
}

/// Check git hygiene when an agent marks a task as done.
/// Emits warnings for uncommitted changes and stash growth.
fn check_agent_git_hygiene(dir: &Path, task_id: &str) {
    use std::process::Command;
    let project_root = dir.parent().unwrap_or(dir);
    if let Ok(output) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(project_root)
        .output()
    {
        let status = String::from_utf8_lossy(&output.stdout);
        if !status.is_empty() {
            let changed: Vec<&str> = status.lines().take(10).collect();
            eprintln!(
                "Warning: git hygiene for '{}': uncommitted changes:\n{}",
                task_id,
                changed.join("\n")
            );
        }
    }
    if let Ok(output) = Command::new("git")
        .args(["stash", "list"])
        .current_dir(project_root)
        .output()
    {
        let count = String::from_utf8_lossy(&output.stdout).lines().count();
        if count > 0 {
            eprintln!(
                "Warning: git hygiene for '{}': {} stash(es) exist. Agents should never stash.",
                task_id, count
            );
        }
    }
}

pub fn run(dir: &Path, id: &str, converged: bool, skip_verify: bool) -> Result<()> {
    let is_agent = std::env::var("WG_AGENT_ID").is_ok();
    run_inner(dir, id, converged, skip_verify, is_agent)
}

fn run_inner(
    dir: &Path,
    id: &str,
    converged: bool,
    skip_verify: bool,
    is_agent: bool,
) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    if task.status == Status::Done {
        println!("Task '{}' is already done", id);
        return Ok(());
    }

    // Check for unresolved blockers (cycle-aware: only exempt back-edge blockers,
    // not all same-cycle blockers).
    //
    // Any blocker that is in the same cycle (SCC) as the task being completed
    // is exempted — both header and non-header members.  The mutual dependency
    // between cycle members is a structural back-edge; blocking on it would
    // deadlock the cycle.
    let blockers = query::after(&graph, id);
    if !blockers.is_empty() {
        let cycle_analysis = graph.compute_cycle_analysis();
        let effective_blockers: Vec<_> = blockers
            .into_iter()
            .filter(|b| {
                // Exempt any blocker in the same cycle (SCC) as this task
                let in_same_cycle = cycle_analysis
                    .task_to_cycle
                    .get(&b.id)
                    .is_some_and(|bc| cycle_analysis.task_to_cycle.get(id) == Some(bc));
                !in_same_cycle
            })
            .collect();
        if !effective_blockers.is_empty() {
            let blocker_list: Vec<String> = effective_blockers
                .iter()
                .map(|t| format!("  - {} ({}): {:?}", t.id, t.title, t.status))
                .collect();
            anyhow::bail!(
                "Cannot mark '{}' as done: blocked by {} unresolved task(s):\n{}",
                id,
                effective_blockers.len(),
                blocker_list.join("\n")
            );
        }
    }

    // Git hygiene check for agents: warn about uncommitted changes
    if is_agent {
        check_agent_git_hygiene(dir, id);
    }

    // Run verify command gate (if task has a verify field)
    if let Some(verify_cmd) = graph.get_task(id).and_then(|t| t.verify.clone()) {
        if skip_verify {
            // Block agents from using --skip-verify
            if is_agent {
                anyhow::bail!(
                    "Agents cannot use --skip-verify. The verify command must pass:\n  {}",
                    verify_cmd,
                );
            }
            eprintln!("Warning: skipping verify command: {}", verify_cmd);
        } else {
            let project_root = dir.parent().unwrap_or(dir);
            eprintln!("Running verify command: {}", verify_cmd);
            run_verify_command(&verify_cmd, project_root)?;
            eprintln!("Verify command passed");
        }
    }

    // Determine validation mode for this task.
    // Resolution: task.validation > "none" (default, backward compatible).
    let validation_mode = graph
        .get_task(id)
        .and_then(|t| t.validation.clone())
        .unwrap_or_else(|| "none".to_string());

    // Integrated validation: enforce log check + run validation_commands
    if validation_mode == "integrated" {
        let task_ref = graph.get_task(id).unwrap();
        let has_validation_log = task_ref
            .log
            .iter()
            .any(|entry| entry.message.to_lowercase().contains("validat"));
        if !has_validation_log {
            anyhow::bail!(
                "Cannot mark '{}' as done: integrated validation requires a validation log entry.\n\
                 Add one with: wg log {} \"Validated: <what you checked>\"",
                id,
                id
            );
        }
        let commands = task_ref.validation_commands.clone();
        if !commands.is_empty() {
            let project_root = dir.parent().unwrap_or(dir);
            for cmd in &commands {
                eprintln!("Running validation command: {}", cmd);
                run_verify_command(cmd, project_root).with_context(|| {
                    format!(
                        "Integrated validation failed for '{}': command failed: {}",
                        id, cmd
                    )
                })?;
            }
            eprintln!("All validation commands passed");
        }
    }

    // External validation: transition to PendingValidation instead of Done
    if validation_mode == "external" {
        let task = graph
            .get_task_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Task '{}' disappeared from graph", id))?;
        task.status = Status::PendingValidation;
        task.completed_at = Some(Utc::now().to_rfc3339());
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: task.assigned.clone(),
            message: "Task pending external validation".to_string(),
        });

        save_graph(&graph, &path).context("Failed to save graph")?;
        super::notify_graph_changed(dir);

        let config = workgraph::config::Config::load_or_default(dir);
        let _ = workgraph::provenance::record(
            dir,
            "done",
            Some(id),
            None,
            serde_json::json!({ "validation": "external", "status": "pending-validation" }),
            config.log.rotation_threshold,
        );

        println!("Task '{}' is pending external validation", id);

        // Archive agent conversation for provenance
        if let Some(task) = graph.get_task(id)
            && let Some(ref agent_id) = task.assigned
        {
            match super::log::archive_agent(dir, id, agent_id) {
                Ok(archive_dir) => {
                    eprintln!("Agent archived to {}", archive_dir.display());
                }
                Err(e) => {
                    eprintln!("Warning: agent archive failed: {}", e);
                }
            }
        }

        // Capture task output for validation
        if let Some(task) = graph.get_task(id) {
            match capture_task_output(dir, task) {
                Ok(output_dir) => {
                    eprintln!("Output captured to {}", output_dir.display());
                }
                Err(e) => {
                    eprintln!("Warning: output capture failed: {}", e);
                }
            }
        }

        return Ok(());
    }

    // When --converged is passed, determine whether the task's cycle has a
    // non-trivial guard or no_converge flag. If so, ignore the converged flag.
    // This prevents workers from bypassing external validation by
    // self-declaring convergence, and enforces forced cycles.
    //
    // We do this check with immutable access before mutating the task.
    let converged_accepted = if converged {
        // Check 1: the task itself has no_converge or a guarded cycle_config
        let own_no_converge = graph
            .get_task(id)
            .and_then(|t| t.cycle_config.as_ref())
            .map(|c| c.no_converge)
            .unwrap_or(false);

        let own_guard = graph
            .get_task(id)
            .and_then(|t| t.cycle_config.as_ref())
            .and_then(|c| c.guard.as_ref())
            .map(|g| !matches!(g, workgraph::graph::LoopGuard::Always))
            .unwrap_or(false);

        // Check 2: the task is a non-header member of a cycle whose header
        // has a non-trivial guard or no_converge. This covers workers trying
        // to converge a cycle they don't own.
        let (cycle_guard, cycle_no_converge) = if !own_guard && !own_no_converge {
            let ca = graph.compute_cycle_analysis();
            ca.task_to_cycle
                .get(id)
                .map(|&idx| {
                    let cycle = &ca.cycles[idx];
                    let guard = cycle.members.iter().any(|mid| {
                        graph
                            .get_task(mid)
                            .and_then(|t| t.cycle_config.as_ref())
                            .and_then(|c| c.guard.as_ref())
                            .map(|g| !matches!(g, workgraph::graph::LoopGuard::Always))
                            .unwrap_or(false)
                    });
                    let no_conv = cycle.members.iter().any(|mid| {
                        graph
                            .get_task(mid)
                            .and_then(|t| t.cycle_config.as_ref())
                            .map(|c| c.no_converge)
                            .unwrap_or(false)
                    });
                    (guard, no_conv)
                })
                .unwrap_or((false, false))
        } else {
            (false, false)
        };

        let has_guard = own_guard || cycle_guard;
        let has_no_converge = own_no_converge || cycle_no_converge;

        if has_no_converge {
            eprintln!(
                "Warning: --converged ignored for '{}' because the cycle is configured with --no-converge.\n         \
                 All iterations must run.",
                id
            );
            false
        } else if has_guard {
            eprintln!(
                "Warning: --converged ignored for '{}' because a cycle guard is set.\n         \
                 Only the guard condition determines convergence.",
                id
            );
            false
        } else {
            true
        }
    } else {
        false
    };

    // Now mutate the task
    let task = graph
        .get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' disappeared from graph", id))?;

    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());

    if converged_accepted && !task.tags.contains(&"converged".to_string()) {
        task.tags.push("converged".to_string());
    }

    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: task.assigned.clone(),
        message: if converged_accepted {
            "Task marked as done (converged)".to_string()
        } else if converged {
            "Task marked as done (--converged ignored, cycle is forced)".to_string()
        } else {
            "Task marked as done".to_string()
        },
    });

    // Extract token usage from agent output.log if available
    if task.token_usage.is_none()
        && let Ok(registry) = AgentRegistry::load(dir)
        && let Some(agent) = registry.get_agent_by_task(id)
    {
        let output_path = std::path::Path::new(&agent.output_file);
        // output_file may be relative to the project root (parent of .workgraph)
        let abs_path = if output_path.is_absolute() {
            output_path.to_path_buf()
        } else {
            dir.parent().unwrap_or(dir).join(output_path)
        };
        if let Some(usage) = parse_token_usage(&abs_path) {
            task.token_usage = Some(usage);
        } else if let Some(usage) = parse_wg_tokens(&abs_path) {
            // Eval agents (.evaluate-*, .flip-*) emit __WG_TOKENS__ lines
            // instead of Claude CLI type=result JSON.
            task.token_usage = Some(usage);
        }
    }

    // Evaluate structural cycle iteration
    let id_owned = id.to_string();
    let cycle_analysis = graph.compute_cycle_analysis();
    let cycle_reactivated = evaluate_cycle_iteration(&mut graph, &id_owned, &cycle_analysis);

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "done",
        Some(id),
        None,
        serde_json::Value::Null,
        config.log.rotation_threshold,
    );

    println!("Marked '{}' as done", id);

    for task_id in &cycle_reactivated {
        println!("  Cycle: re-activated '{}'", task_id);
    }

    // Archive agent conversation (prompt + output) for provenance
    if let Some(task) = graph.get_task(id)
        && let Some(ref agent_id) = task.assigned
    {
        match super::log::archive_agent(dir, id, agent_id) {
            Ok(archive_dir) => {
                eprintln!("Agent archived to {}", archive_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: agent archive failed: {}", e);
            }
        }
    }

    // Capture task output (git diff, artifacts, log) for evaluation.
    // When auto_evaluate is enabled, the coordinator creates an evaluation task
    // in the graph that becomes ready once this task is done; the captured output
    // feeds that evaluator.
    if let Some(task) = graph.get_task(id) {
        match capture_task_output(dir, task) {
            Ok(output_dir) => {
                eprintln!("Output captured to {}", output_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: output capture failed: {}", e);
            }
        }
    }

    // Soft validation nudge: if no log entry mentions validation, print a tip.
    if let Some(task) = graph.get_task(id) {
        let has_validation = task
            .log
            .iter()
            .any(|entry| entry.message.to_lowercase().contains("validat"));
        if !has_validation {
            eprintln!(
                "Tip: Log validation steps before wg done (e.g., wg log {} \"Validated: tests pass\")",
                id
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use workgraph::test_helpers::{make_task_with_status as make_task, setup_workgraph};

    #[test]
    fn test_done_open_task_transitions_to_done() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_in_progress_task_transitions_to_done() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::InProgress)],
        );

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_already_done_returns_ok() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Done)]);

        // Should return Ok (idempotent) rather than error
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_done_with_unresolved_blockers_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Blocker task", Status::Open);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("blocked by"));
        assert!(err.to_string().contains("unresolved"));
    }

    #[test]
    fn test_done_with_resolved_blockers_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Blocker task", Status::Done);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("blocked").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_with_failed_blocker_succeeds() {
        // Failed blockers are terminal — they should not block dependents
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Failed blocker", Status::Failed);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("blocked").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_with_abandoned_blocker_succeeds() {
        // Abandoned blockers are terminal — they should not block dependents
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Abandoned blocker", Status::Abandoned);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("blocked").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_verified_task_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Verified task", Status::InProgress);
        task.verify = Some("true".to_string());

        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_sets_completed_at_timestamp() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let before = Utc::now();
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.completed_at.is_some());

        // Parse the timestamp and verify it's recent
        let completed_at: chrono::DateTime<Utc> =
            task.completed_at.as_ref().unwrap().parse().unwrap();
        assert!(completed_at >= before);
    }

    #[test]
    fn test_done_creates_log_entry() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        assert!(!task.log.is_empty());
        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.message, "Task marked as done");
        assert_eq!(last_log.actor, Some("agent-1".to_string()));
    }

    #[test]
    fn test_done_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);

        let result = run(dir_path, "nonexistent", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_done_uninitialized_workgraph_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Don't initialize workgraph

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[test]
    fn test_done_log_entry_without_assigned_has_none_actor() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.actor, None);
    }

    #[test]
    fn test_done_converged_log_message() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.message, "Task marked as done (converged)");
    }

    #[test]
    fn test_done_converged_ignored_when_cycle_guard_set_on_self() {
        // When the task itself has a cycle guard, --converged should be ignored.
        // The guard is authoritative — the agent cannot self-converge.
        use workgraph::graph::{CycleConfig, LoopGuard};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Cycle header", Status::Open);
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: Some(LoopGuard::TaskStatus {
                task: "validator".to_string(),
                status: Status::Failed,
            }),
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("header").unwrap();

        // Converged tag should NOT be present
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added when cycle guard is set"
        );

        // Log should reflect that --converged was ignored
        let last_log = task.log.last().unwrap();
        assert_eq!(
            last_log.message,
            "Task marked as done (--converged ignored, cycle is forced)"
        );
    }

    #[test]
    fn test_done_converged_ignored_for_non_header_in_guarded_cycle() {
        // When a task is a non-header member of a cycle whose header has a guard,
        // --converged should also be ignored.
        use workgraph::graph::{CycleConfig, LoopGuard};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Create cycle: header ↔ worker (both depend on each other)
        let mut header = make_task("header", "Cycle header", Status::Done);
        header.after = vec!["worker".to_string()];
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: Some(LoopGuard::TaskStatus {
                task: "validator".to_string(),
                status: Status::Failed,
            }),
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let mut worker = make_task("worker", "Worker in cycle", Status::Open);
        worker.after = vec!["header".to_string()];

        setup_workgraph(dir_path, vec![header, worker]);

        let result = run(dir_path, "worker", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("worker").unwrap();

        // Converged tag should NOT be present
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added for non-header in guarded cycle"
        );

        // Log should reflect that --converged was ignored
        let last_log = task.log.last().unwrap();
        assert_eq!(
            last_log.message,
            "Task marked as done (--converged ignored, cycle is forced)"
        );
    }

    #[test]
    fn test_done_converged_accepted_when_guard_is_always() {
        // When cycle_config has guard = Always (trivial), --converged should work.
        use workgraph::graph::{CycleConfig, LoopGuard};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Cycle header", Status::Open);
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: Some(LoopGuard::Always),
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("header").unwrap();

        // Converged tag SHOULD be present (Always guard is trivial)
        assert!(
            task.tags.contains(&"converged".to_string()),
            "converged tag should be added when guard is Always"
        );

        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.message, "Task marked as done (converged)");
    }

    #[test]
    fn test_done_converged_accepted_when_no_guard() {
        // When cycle_config has no guard, --converged should work.
        use workgraph::graph::CycleConfig;

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Cycle header", Status::Open);
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("header").unwrap();

        // Converged tag SHOULD be present
        assert!(
            task.tags.contains(&"converged".to_string()),
            "converged tag should be added when no guard is set"
        );
    }

    #[test]
    fn test_done_without_validation_log_still_succeeds() {
        // The soft validation tip should never block completion.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);

        // No log entry contains "validat" — the tip would fire, but must not block
        let has_validation = task
            .log
            .iter()
            .any(|e| e.message.to_lowercase().contains("validat"));
        assert!(!has_validation);
    }

    #[test]
    fn test_done_with_validation_log_suppresses_tip() {
        // When a log entry contains a validation mention, no tip should fire.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::Open);
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: "Validated: all tests pass".to_string(),
        });
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);

        // Log contains "Validated" — tip should be suppressed
        let has_validation = task
            .log
            .iter()
            .any(|e| e.message.to_lowercase().contains("validat"));
        assert!(has_validation);
    }

    #[test]
    fn test_done_converged_ignored_when_no_converge_set_on_self() {
        // When the task itself has no_converge, --converged should be ignored.
        use workgraph::graph::CycleConfig;

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Forced cycle header", Status::Open);
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: true,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("header").unwrap();

        // Converged tag should NOT be present
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added when no_converge is set"
        );

        // Log should contain the forced-ignore message (may not be last due to reactivation)
        let has_forced_msg = task
            .log
            .iter()
            .any(|e| e.message == "Task marked as done (--converged ignored, cycle is forced)");
        assert!(
            has_forced_msg,
            "Log should contain forced-ignore message, got: {:?}",
            task.log.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_done_converged_ignored_for_non_header_in_no_converge_cycle() {
        // When a task is a non-header member of a cycle with no_converge,
        // --converged should also be ignored.
        use workgraph::graph::CycleConfig;

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Forced cycle header", Status::Done);
        header.after = vec!["worker".to_string()];
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: true,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let mut worker = make_task("worker", "Worker in forced cycle", Status::Open);
        worker.after = vec!["header".to_string()];

        setup_workgraph(dir_path, vec![header, worker]);

        let result = run(dir_path, "worker", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("worker").unwrap();

        // Converged tag should NOT be present
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added for non-header in no-converge cycle"
        );

        // Log should contain the forced-ignore message (may not be last due to reactivation)
        let has_forced_msg = task
            .log
            .iter()
            .any(|e| e.message == "Task marked as done (--converged ignored, cycle is forced)");
        assert!(
            has_forced_msg,
            "Log should contain forced-ignore message, got: {:?}",
            task.log.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_done_verify_passing_allows_transition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with passing verify", Status::InProgress);
        task.verify = Some("exit 0".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_verify_failing_blocks_transition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Verify command failed"), "got: {}", err);
        assert!(err.contains("exit 1"), "got: {}", err);

        // Task should still be in-progress
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_done_verify_failing_includes_output() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("echo 'test failed: expected 42 got 0' >&2; exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("test failed: expected 42 got 0"),
            "error should include command output, got: {}",
            err
        );
    }

    #[test]
    fn test_done_skip_verify_bypasses_gate() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Use run_inner with is_agent=false to simulate human usage
        let result = super::run_inner(dir_path, "t1", false, true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_skip_verify_blocked_for_agents() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Use run_inner with is_agent=true to simulate agent context
        let result = super::run_inner(dir_path, "t1", false, true, true);

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Agents cannot use --skip-verify"),
            "got: {}",
            err
        );

        // Task should not have transitioned
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_done_no_verify_field_works_as_before() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let task = make_task("t1", "Task without verify", Status::InProgress);
        assert!(task.verify.is_none());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_converged_also_runs_verify() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", true, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Verify command failed"), "got: {}", err);

        // Task should still be in-progress
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_done_external_validation_transitions_to_pending() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "External validation task", Status::InProgress);
        task.validation = Some("external".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::PendingValidation);
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn test_done_external_validation_adds_log() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "External validation task", Status::InProgress);
        task.validation = Some("external".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false, false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        let last_log = task.log.last().unwrap();
        assert!(last_log.message.contains("pending external validation"));
    }

    #[test]
    fn test_done_integrated_validation_requires_log_entry() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Integrated validation task", Status::InProgress);
        task.validation = Some("integrated".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Should fail: no validation log entry
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("validation log entry"));
    }

    #[test]
    fn test_done_integrated_validation_with_log_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Integrated validation task", Status::InProgress);
        task.validation = Some("integrated".to_string());
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: "Validated: all tests pass".to_string(),
        });
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_integrated_validation_runs_commands() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Integrated with commands", Status::InProgress);
        task.validation = Some("integrated".to_string());
        task.validation_commands = vec!["exit 1".to_string()]; // will fail
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: "Validated: ready".to_string(),
        });
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("validation failed"));
    }

    #[test]
    fn test_done_none_validation_is_default() {
        // validation=None (default) should behave like "none" — direct to Done
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let task = make_task("t1", "Default task", Status::InProgress);
        assert!(task.validation.is_none());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }
}
