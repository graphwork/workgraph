use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::graph::{LogEntry, Status, evaluate_cycle_iteration, parse_token_usage};
use workgraph::parser::save_graph;
use workgraph::query;
use workgraph::service::registry::AgentRegistry;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str, converged: bool) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    if task.status == Status::Done {
        println!("Task '{}' is already done", id);
        return Ok(());
    }

    // Check for unresolved blockers (cycle-aware: only exempt back-edge blockers,
    // not all same-cycle blockers).
    //
    // A "back-edge blocker" is a blocker that has cycle_config (the cycle's
    // iterator/validator task) and is in the same cycle as the task being
    // completed.  The auto-created dependency from the worker back to the
    // iterator is a loop-back edge; the iterator should not block the worker.
    let blockers = query::after(&graph, id);
    if !blockers.is_empty() {
        let cycle_analysis = graph.compute_cycle_analysis();
        let effective_blockers: Vec<_> = blockers
            .into_iter()
            .filter(|b| {
                // Exempt if the blocker is the cycle iterator in the same cycle
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

    // When --converged is passed, determine whether the task's cycle has a
    // non-trivial guard. If so, the guard is authoritative — ignore the
    // converged flag. This prevents workers from bypassing external
    // validation by self-declaring convergence.
    //
    // We do this check with immutable access before mutating the task.
    let converged_accepted = if converged {
        // Check 1: the task itself has a guarded cycle_config
        let own_guard = graph
            .get_task(id)
            .and_then(|t| t.cycle_config.as_ref())
            .and_then(|c| c.guard.as_ref())
            .map(|g| !matches!(g, workgraph::graph::LoopGuard::Always))
            .unwrap_or(false);

        // Check 2: the task is a non-header member of a cycle whose header
        // has a non-trivial guard. This covers workers trying to converge
        // a cycle they don't own.
        let cycle_guard = if !own_guard {
            let ca = graph.compute_cycle_analysis();
            ca.task_to_cycle.get(id).map(|&idx| {
                let cycle = &ca.cycles[idx];
                cycle.members.iter().any(|mid| {
                    graph
                        .get_task(mid)
                        .and_then(|t| t.cycle_config.as_ref())
                        .and_then(|c| c.guard.as_ref())
                        .map(|g| !matches!(g, workgraph::graph::LoopGuard::Always))
                        .unwrap_or(false)
                })
            }).unwrap_or(false)
        } else {
            false
        };

        let has_guard = own_guard || cycle_guard;

        if has_guard {
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
            "Task marked as done (--converged ignored, guard is authoritative)".to_string()
        } else {
            "Task marked as done".to_string()
        },
    });

    // Extract token usage from agent output.log if available
    if task.token_usage.is_none() {
        if let Ok(registry) = AgentRegistry::load(dir) {
            if let Some(agent) = registry.get_agent_by_task(id) {
                let output_path = std::path::Path::new(&agent.output_file);
                // output_file may be relative to the project root (parent of .workgraph)
                let abs_path = if output_path.is_absolute() {
                    output_path.to_path_buf()
                } else {
                    dir.parent().unwrap_or(dir).join(output_path)
                };
                if let Some(usage) = parse_token_usage(&abs_path) {
                    task.token_usage = Some(usage);
                }
            }
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
        let has_validation = task.log.iter().any(|entry| {
            entry.message.to_lowercase().contains("validat")
        });
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

        let result = run(dir_path, "t1", false);
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

        let result = run(dir_path, "t1", false);
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
        let result = run(dir_path, "t1", false);
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

        let result = run(dir_path, "blocked", false);
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

        let result = run(dir_path, "blocked", false);
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

        let result = run(dir_path, "blocked", false);
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

        let result = run(dir_path, "blocked", false);
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
        task.verify = Some("Check output quality".to_string());

        setup_workgraph(dir_path, vec![task]);

        // Verified tasks can now use wg done directly (submit is deprecated)
        let result = run(dir_path, "t1", false);
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
        let result = run(dir_path, "t1", false);
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

        let result = run(dir_path, "t1", false);
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

        let result = run(dir_path, "nonexistent", false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_done_uninitialized_workgraph_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Don't initialize workgraph

        let result = run(dir_path, "t1", false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[test]
    fn test_done_log_entry_without_assigned_has_none_actor() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false);
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

        let result = run(dir_path, "t1", true);
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
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true);
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
            "Task marked as done (--converged ignored, guard is authoritative)"
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
        });

        let mut worker = make_task("worker", "Worker in cycle", Status::Open);
        worker.after = vec!["header".to_string()];

        setup_workgraph(dir_path, vec![header, worker]);

        let result = run(dir_path, "worker", true);
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
            "Task marked as done (--converged ignored, guard is authoritative)"
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
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true);
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
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true);
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

        let result = run(dir_path, "t1", false);
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

        let result = run(dir_path, "t1", false);
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
}
