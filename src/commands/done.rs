use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::graph::{LogEntry, Status, Task, evaluate_cycle_iteration};
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

pub fn run(dir: &Path, id: &str, converged: bool, skip_verify: bool) -> Result<()> {
    let is_agent = std::env::var("WG_AGENT_ID").is_ok();
    run_inner(dir, id, converged, skip_verify, is_agent)
}

struct DoneResult {
    cycle_reactivated: Vec<String>,
    agent_id: Option<String>,
    task_snapshot: Option<Task>,
}

fn run_inner(
    dir: &Path,
    id: &str,
    converged: bool,
    skip_verify: bool,
    is_agent: bool,
) -> Result<()> {
    // Run verify command gate BEFORE acquiring the lock (can take up to 120s).
    // Read-only peek to get the verify command string.
    if let Ok((peek_graph, _)) = super::load_workgraph(dir) {
        if let Some(verify_cmd) = peek_graph.get_task(id).and_then(|t| t.verify_cmd.clone()) {
            if skip_verify {
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
    }

    // Atomic graph mutation
    let result = super::mutate_workgraph(dir, |graph| {
        let task = graph.get_task_mut_or_err(id)?;

        if task.status == Status::Done {
            println!("Task '{}' is already done", id);
            return Ok(None);
        }

        // Check for unresolved blockers (cycle-aware)
        let blockers = query::after(graph, id);
        if !blockers.is_empty() {
            let cycle_analysis = graph.compute_cycle_analysis();
            let effective_blockers: Vec<_> = blockers
                .into_iter()
                .filter(|b| {
                    let blocker_is_cycle_iterator = b.cycle_config.is_some();
                    let in_same_cycle = blocker_is_cycle_iterator
                        && cycle_analysis
                            .task_to_cycle
                            .get(&b.id)
                            .is_some_and(|bc| {
                                cycle_analysis.task_to_cycle.get(id) == Some(bc)
                            });
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

        // Converged logic
        let converged_accepted = if converged {
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
            ..Default::default()
        });

        // Extract token usage from stream.jsonl (canonical source)
        if task.token_usage.is_none()
            && let Ok(registry) = AgentRegistry::load(dir)
            && let Some(agent) = registry.get_agent_by_task(id)
        {
            let output_path = std::path::Path::new(&agent.output_file);
            let agent_dir = if output_path.is_absolute() {
                output_path.parent().map(|p| p.to_path_buf())
            } else {
                output_path.parent().map(|p| dir.parent().unwrap_or(dir).join(p))
            };
            if let Some(agent_dir) = agent_dir {
                if let Some(usage) = workgraph::stream_event::parse_token_usage_from_stream(&agent_dir) {
                    task.token_usage = Some(usage);
                }
            }
        }

        // Evaluate structural cycle iteration
        let id_owned = id.to_string();
        let cycle_analysis = graph.compute_cycle_analysis();
        let cycle_reactivated = evaluate_cycle_iteration(graph, &id_owned, &cycle_analysis);

        let agent_id = graph.get_task(id).and_then(|t| t.assigned.clone());
        let task_snapshot = graph.get_task(id).cloned();

        Ok(Some(DoneResult {
            cycle_reactivated,
            agent_id,
            task_snapshot,
        }))
    })?;

    let Some(result) = result else {
        return Ok(());
    };

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

    for task_id in &result.cycle_reactivated {
        println!("  Cycle: re-activated '{}'", task_id);
    }

    // Archive agent conversation (prompt + output) for provenance
    if let Some(ref agent_id) = result.agent_id {
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
    if let Some(ref task) = result.task_snapshot {
        match capture_task_output(dir, task) {
            Ok(output_dir) => {
                eprintln!("Output captured to {}", output_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: output capture failed: {}", e);
            }
        }
    }

    // Soft validation nudge
    if let Some(ref task) = result.task_snapshot {
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
        task.verify_cmd = Some("true".to_string());

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

        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added when cycle guard is set"
        );

        let last_log = task.log.last().unwrap();
        assert_eq!(
            last_log.message,
            "Task marked as done (--converged ignored, cycle is forced)"
        );
    }

    #[test]
    fn test_done_converged_ignored_for_non_header_in_guarded_cycle() {
        use workgraph::graph::{CycleConfig, LoopGuard};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

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

        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added for non-header in guarded cycle"
        );

        let last_log = task.log.last().unwrap();
        assert_eq!(
            last_log.message,
            "Task marked as done (--converged ignored, cycle is forced)"
        );
    }

    #[test]
    fn test_done_converged_accepted_when_guard_is_always() {
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

        assert!(
            task.tags.contains(&"converged".to_string()),
            "converged tag should be added when guard is Always"
        );

        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.message, "Task marked as done (converged)");
    }

    #[test]
    fn test_done_converged_accepted_when_no_guard() {
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

        assert!(
            task.tags.contains(&"converged".to_string()),
            "converged tag should be added when no guard is set"
        );
    }

    #[test]
    fn test_done_without_validation_log_still_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);

        let has_validation = task
            .log
            .iter()
            .any(|e| e.message.to_lowercase().contains("validat"));
        assert!(!has_validation);
    }

    #[test]
    fn test_done_with_validation_log_suppresses_tip() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::Open);
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: "Validated: all tests pass".to_string(),
            ..Default::default()
        });
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);

        let has_validation = task
            .log
            .iter()
            .any(|e| e.message.to_lowercase().contains("validat"));
        assert!(has_validation);
    }

    #[test]
    fn test_done_converged_ignored_when_no_converge_set_on_self() {
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

        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added when no_converge is set"
        );

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

        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added for non-header in no-converge cycle"
        );

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
        task.verify_cmd = Some("exit 0".to_string());
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
        task.verify_cmd = Some("exit 1".to_string());
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
        task.verify_cmd = Some("echo 'test failed: expected 42 got 0' >&2; exit 1".to_string());
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
        task.verify_cmd = Some("exit 1".to_string());
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
        task.verify_cmd = Some("exit 1".to_string());
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
        assert!(task.verify_cmd.is_none());
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
        task.verify_cmd = Some("exit 1".to_string());
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
}
