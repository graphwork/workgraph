use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::graph::{LogEntry, Status, evaluate_loop_edges};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query;

use super::graph_path;

pub fn run(dir: &Path, id: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    if task.status == Status::Done {
        println!("Task '{}' is already done", id);
        return Ok(());
    }

    // Check for unresolved blockers
    let blockers = query::blocked_by(&graph, id);
    if !blockers.is_empty() {
        let blocker_list: Vec<String> = blockers
            .iter()
            .map(|t| format!("  - {} ({}): {:?}", t.id, t.title, t.status))
            .collect();
        anyhow::bail!(
            "Cannot mark '{}' as done: blocked by {} unresolved task(s):\n{}",
            id,
            blockers.len(),
            blocker_list.join("\n")
        );
    }

    // Re-acquire mutable reference after immutable borrow
    let task = graph.get_task_mut(id).unwrap();

    // Verified tasks must use submit -> approve workflow
    if task.verify.is_some() {
        anyhow::bail!(
            "Task '{}' requires verification. Use 'wg submit {}' instead of 'wg done'.\n\
             After submission, a reviewer must use 'wg approve {}' to complete it.",
            id,
            id,
            id
        );
    }

    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());

    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: task.assigned.clone(),
        message: "Task marked as done".to_string(),
    });

    // Evaluate loop edges: re-activate upstream tasks if conditions are met
    let id_owned = id.to_string();
    let reactivated = evaluate_loop_edges(&mut graph, &id_owned);

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    println!("Marked '{}' as done", id);

    for task_id in &reactivated {
        println!("  Loop: re-activated '{}'", task_id);
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{LoopEdge, Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
        }
    }

    fn setup_workgraph(dir: &Path, tasks: Vec<Task>) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
        path
    }

    #[test]
    fn test_done_open_task_transitions_to_done() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1");
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

        let result = run(dir_path, "t1");
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
        let result = run(dir_path, "t1");
        assert!(result.is_ok());
    }

    #[test]
    fn test_done_with_unresolved_blockers_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Blocker task", Status::Open);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.blocked_by = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked");
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
        blocked.blocked_by = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("blocked").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_verified_task_rejects_with_submit_hint() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Verified task", Status::InProgress);
        task.verify = Some("Check output quality".to_string());

        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("requires verification"));
        assert!(err.to_string().contains("wg submit"));
    }

    #[test]
    fn test_done_sets_completed_at_timestamp() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let before = Utc::now();
        let result = run(dir_path, "t1");
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

        let result = run(dir_path, "t1");
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
    fn test_done_evaluates_loop_edges_and_reactivates_target() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Create a loop: source -> loops_to -> target
        let mut source = make_task("source", "Source task", Status::InProgress);
        source.loops_to = vec![LoopEdge {
            target: "target".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];

        let mut target = make_task("target", "Target task", Status::Done);
        target.loop_iteration = 0;

        setup_workgraph(dir_path, vec![source, target]);

        let result = run(dir_path, "source");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();

        // Source should be re-opened (part of the cycle)
        let source = graph.get_task("source").unwrap();
        assert_eq!(source.status, Status::Open);
        assert_eq!(source.loop_iteration, 1);

        // Target should be re-activated (Open) with incremented loop_iteration
        let target = graph.get_task("target").unwrap();
        assert_eq!(target.status, Status::Open);
        assert_eq!(target.loop_iteration, 1);
    }

    #[test]
    fn test_done_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);

        let result = run(dir_path, "nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_done_uninitialized_workgraph_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Don't initialize workgraph

        let result = run(dir_path, "t1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[test]
    fn test_done_log_entry_without_assigned_has_none_actor() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.actor, None);
    }

    #[test]
    fn test_done_loop_edge_respects_max_iterations() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Source has a loop to target, but target is already at max_iterations
        let mut source = make_task("source", "Source task", Status::InProgress);
        source.loops_to = vec![LoopEdge {
            target: "target".to_string(),
            guard: None,
            max_iterations: 2,
            delay: None,
        }];

        let mut target = make_task("target", "Target task", Status::Done);
        target.loop_iteration = 2; // Already at max

        setup_workgraph(dir_path, vec![source, target]);

        let result = run(dir_path, "source");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();

        // Target should NOT be re-activated (still Done, iteration unchanged)
        let target = graph.get_task("target").unwrap();
        assert_eq!(target.status, Status::Done);
        assert_eq!(target.loop_iteration, 2);

        // Source should also stay Done (loop didn't fire)
        let source = graph.get_task("source").unwrap();
        assert_eq!(source.status, Status::Done);
    }

    #[test]
    fn test_done_loop_reopens_source_in_chain() {
        // Regression test: A→B→C chain with loop from C back to A.
        // When C completes, A (target), B (intermediate), and C (source)
        // should all be re-opened.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task_a = make_task("a", "Task A", Status::Done);
        task_a.loop_iteration = 0;

        let mut task_b = make_task("b", "Task B", Status::Done);
        task_b.blocked_by = vec!["a".to_string()];

        let mut task_c = make_task("c", "Task C", Status::InProgress);
        task_c.blocked_by = vec!["b".to_string()];
        task_c.loops_to = vec![LoopEdge {
            target: "a".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];

        setup_workgraph(dir_path, vec![task_a, task_b, task_c]);

        let result = run(dir_path, "c");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();

        // A (target) should be re-opened with incremented iteration
        let a = graph.get_task("a").unwrap();
        assert_eq!(a.status, Status::Open);
        assert_eq!(a.loop_iteration, 1);

        // B (intermediate) should be re-opened
        let b = graph.get_task("b").unwrap();
        assert_eq!(b.status, Status::Open);

        // C (source) should be re-opened with incremented iteration
        let c = graph.get_task("c").unwrap();
        assert_eq!(c.status, Status::Open);
        assert_eq!(c.loop_iteration, 1);
    }
}
