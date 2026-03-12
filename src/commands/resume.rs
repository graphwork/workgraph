use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use workgraph::graph::{LogEntry, WorkGraph};
use workgraph::parser::save_graph;

use super::eval_scaffold;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str, only: bool) -> Result<()> {
    run_inner(dir, id, only, false)
}

/// Publish a draft task (alias for resume with validation messaging).
pub fn publish(dir: &Path, id: &str, only: bool) -> Result<()> {
    run_inner(dir, id, only, true)
}

fn run_inner(dir: &Path, id: &str, only: bool, is_publish: bool) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    // Verify seed task exists and is paused
    let task = graph.get_task_or_err(id)?;
    if !task.paused {
        anyhow::bail!("Task '{}' is not paused", id);
    }

    if only {
        // Single-task mode: validate just this task's deps, then unpause
        validate_task_deps(&graph, id, is_publish)?;
        let action = if is_publish { "published" } else { "resumed" };
        unpause_task(&mut graph, id, action);

        // Eagerly scaffold eval task at publish time
        if is_publish {
            scaffold_eval_for_published(dir, &mut graph, &[id.to_string()]);
        }

        save_graph(&graph, &path).context("Failed to save graph")?;
        super::notify_graph_changed(dir);
        record_provenance(dir, id, is_publish);
        if is_publish {
            println!("Published '{}' — task is now available for dispatch", id);
        } else {
            println!("Resumed '{}'", id);
        }
    } else {
        // Propagating mode: discover subgraph, validate all, unpause all
        let subgraph = discover_downstream(&graph, id);

        // Validate the entire subgraph structure
        validate_subgraph(&graph, &subgraph, is_publish)?;

        // Atomic unpause: all paused tasks in the subgraph
        let action = if is_publish { "published" } else { "resumed" };
        let mut unpaused = Vec::new();
        for task_id in &subgraph {
            let t = graph.get_task(task_id).unwrap();
            if t.paused {
                unpaused.push(task_id.clone());
            }
        }
        for task_id in &unpaused {
            unpause_task(&mut graph, task_id, action);
        }

        // Eagerly scaffold eval tasks at publish time
        if is_publish {
            scaffold_eval_for_published(dir, &mut graph, &unpaused);
        }

        save_graph(&graph, &path).context("Failed to save graph")?;
        super::notify_graph_changed(dir);
        record_provenance(dir, id, is_publish);

        if is_publish {
            println!(
                "Published '{}' and {} downstream task(s)",
                id,
                unpaused.len().saturating_sub(1)
            );
        } else {
            println!(
                "Resumed '{}' and {} downstream task(s)",
                id,
                unpaused.len().saturating_sub(1)
            );
        }
    }

    Ok(())
}

/// Discover all tasks reachable downstream from the seed task.
/// "Downstream" means: tasks whose `after` list includes a member of the subgraph,
/// plus tasks reachable via `before` edges from the subgraph.
fn discover_downstream(graph: &WorkGraph, seed_id: &str) -> Vec<String> {
    // Build a reverse index: for each task, which tasks depend on it (have it in `after`)?
    let mut dependents: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for task in graph.tasks() {
        for dep_id in &task.after {
            dependents
                .entry(dep_id.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    // Also include `before` edges: if A has B in `before`, B depends on A,
    // so B is downstream of A.
    for task in graph.tasks() {
        for downstream_id in &task.before {
            dependents
                .entry(task.id.clone())
                .or_default()
                .push(downstream_id.clone());
        }
    }

    // BFS from seed
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(seed_id.to_string());
    queue.push_back(seed_id.to_string());

    while let Some(current) = queue.pop_front() {
        if let Some(deps) = dependents.get(&current) {
            for dep in deps {
                // Only include actual tasks (not resources, not missing)
                if graph.get_task(dep).is_some() && visited.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    let mut result: Vec<String> = visited.into_iter().collect();
    result.sort(); // deterministic order
    result
}

/// Validate a single task's `after` dependencies exist.
fn validate_task_deps(graph: &WorkGraph, task_id: &str, is_publish: bool) -> Result<()> {
    let task = graph.get_task_or_err(task_id)?;
    let mut missing = Vec::new();
    for dep_id in &task.after {
        if workgraph::federation::parse_remote_ref(dep_id).is_some() {
            continue;
        }
        if graph.get_node(dep_id).is_none() {
            let mut msg = format!("'{}'", dep_id);
            let all_ids: Vec<&str> = graph.tasks().map(|t| t.id.as_str()).collect();
            if let Some((suggestion, _)) =
                workgraph::check::fuzzy_match_task_id(dep_id, all_ids.iter().copied(), 3)
            {
                msg.push_str(&format!(" (did you mean '{}'?)", suggestion));
            }
            missing.push(msg);
        }
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "Cannot {} task '{}': dangling dependencies:\n  {}",
            if is_publish { "publish" } else { "resume" },
            task_id,
            missing.join("\n  ")
        );
    }
    Ok(())
}

/// Validate the entire subgraph structure before unpausing.
fn validate_subgraph(graph: &WorkGraph, subgraph: &[String], is_publish: bool) -> Result<()> {
    let action = if is_publish { "publish" } else { "resume" };
    let mut errors = Vec::new();

    for task_id in subgraph {
        let task = graph.get_task(task_id).unwrap();

        // Check for dangling after-dependencies
        for dep_id in &task.after {
            if workgraph::federation::parse_remote_ref(dep_id).is_some() {
                continue;
            }
            if graph.get_node(dep_id).is_none() {
                let mut msg = format!("Task '{}': dangling dependency '{}'", task_id, dep_id);
                let all_ids: Vec<&str> = graph.tasks().map(|t| t.id.as_str()).collect();
                if let Some((suggestion, _)) =
                    workgraph::check::fuzzy_match_task_id(dep_id, all_ids.iter().copied(), 3)
                {
                    msg.push_str(&format!(" (did you mean '{}'?)", suggestion));
                }
                errors.push(msg);
            }
        }
    }

    // Check cycle validity: any cycle in the subgraph must have max_iterations configured
    let subgraph_set: HashSet<&str> = subgraph.iter().map(|s| s.as_str()).collect();
    let cycle_analysis = workgraph::graph::CycleAnalysis::from_graph(graph);
    for cycle in &cycle_analysis.cycles {
        // Check if this cycle intersects with our subgraph
        let members_in_subgraph: Vec<&str> = cycle
            .members
            .iter()
            .filter(|id| subgraph_set.contains(id.as_str()))
            .map(|s| s.as_str())
            .collect();
        if members_in_subgraph.len() > 1 {
            // This is a real cycle — check if any task has cycle_config
            let has_config = members_in_subgraph.iter().any(|id| {
                graph
                    .get_task(id)
                    .map(|t| t.cycle_config.is_some())
                    .unwrap_or(false)
            });
            if !has_config {
                errors.push(format!(
                    "Cycle without --max-iterations: [{}]",
                    members_in_subgraph.join(", ")
                ));
            }
        }
    }

    if !errors.is_empty() {
        anyhow::bail!(
            "Cannot {} subgraph — structural errors:\n  {}",
            action,
            errors.join("\n  ")
        );
    }

    Ok(())
}

fn unpause_task(graph: &mut WorkGraph, task_id: &str, action: &str) {
    let task = graph.get_task_mut(task_id).unwrap();
    task.paused = false;
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: None,
        message: format!("Task {}", action),
    });
}

/// Create lifecycle tasks (`.assign-*`, `.evaluate-*`, `.flip-*`) for each
/// published task. Skips system tasks (dot-prefixed) and dominated tags.
fn scaffold_eval_for_published(dir: &Path, graph: &mut WorkGraph, task_ids: &[String]) {
    let config = workgraph::config::Config::load_or_default(dir);

    // Collect (id, title) pairs, filtering out system tasks
    let candidates: Vec<(String, String)> = task_ids
        .iter()
        .filter(|id| !workgraph::graph::is_system_task(id))
        .filter_map(|id| graph.get_task(id).map(|t| (id.clone(), t.title.clone())))
        .collect();

    // Scaffold .assign-* tasks (blocking edges) when auto_assign is enabled
    if config.agency.auto_assign {
        let assign_count = eval_scaffold::scaffold_assign_tasks_batch(graph, &candidates);
        if assign_count > 0 {
            eprintln!(
                "[publish] Eagerly scaffolded {} assignment task(s)",
                assign_count
            );
        }
    }

    // Scaffold .evaluate-* and .flip-* tasks
    if config.agency.auto_evaluate {
        let eval_count = eval_scaffold::scaffold_eval_tasks_batch(dir, graph, &candidates, &config);
        if eval_count > 0 {
            eprintln!(
                "[publish] Eagerly scaffolded {} evaluation task(s)",
                eval_count
            );
        }
    }
}

fn record_provenance(dir: &Path, id: &str, is_publish: bool) {
    let config = workgraph::config::Config::load_or_default(dir);
    let op = if is_publish { "publish" } else { "resume" };
    let _ = workgraph::provenance::record(
        dir,
        op,
        Some(id),
        None,
        serde_json::json!({}),
        config.log.rotation_threshold,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{CycleConfig, Node, Status, Task, WorkGraph};

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Task::default()
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

    // --- Single-task (--only) tests ---

    #[test]
    fn test_resume_paused_task_only() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1", true);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.paused);
    }

    #[test]
    fn test_resume_not_paused_fails() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Test", Status::Open)]);

        let result = run(dir.path(), "t1", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not paused"));
    }

    #[test]
    fn test_resume_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![]);

        let result = run(dir.path(), "nonexistent", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_resume_only_adds_log_entry() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", true).unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.log.len(), 1);
        assert!(task.log[0].message.contains("resumed"));
    }

    #[test]
    fn test_resume_only_with_dangling_dep_fails() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["nonexistent-dep".to_string()];
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1", true);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("dangling dependencies"), "got: {msg}");
        assert!(msg.contains("nonexistent-dep"), "got: {msg}");
    }

    #[test]
    fn test_resume_only_with_valid_deps_succeeds() {
        let dir = tempdir().unwrap();
        let dep = make_task("dep1", "Dependency", Status::Open);
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["dep1".to_string()];
        setup_workgraph(dir.path(), vec![dep, task]);

        let result = run(dir.path(), "t1", true);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.paused);
    }

    // --- Propagating resume tests ---

    #[test]
    fn test_propagating_resume_unpauses_chain() {
        let dir = tempdir().unwrap();
        let mut t1 = make_task("research", "Research X", Status::Open);
        t1.paused = true;
        let mut t2 = make_task("implement", "Implement X", Status::Open);
        t2.paused = true;
        t2.after = vec!["research".to_string()];
        let mut t3 = make_task("test-x", "Test X", Status::Open);
        t3.paused = true;
        t3.after = vec!["implement".to_string()];
        setup_workgraph(dir.path(), vec![t1, t2, t3]);

        let result = run(dir.path(), "research", false);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        assert!(!graph.get_task("research").unwrap().paused);
        assert!(!graph.get_task("implement").unwrap().paused);
        assert!(!graph.get_task("test-x").unwrap().paused);
    }

    #[test]
    fn test_propagating_resume_only_flag_unpauses_single() {
        let dir = tempdir().unwrap();
        let mut t1 = make_task("research", "Research X", Status::Open);
        t1.paused = true;
        let mut t2 = make_task("implement", "Implement X", Status::Open);
        t2.paused = true;
        t2.after = vec!["research".to_string()];
        setup_workgraph(dir.path(), vec![t1, t2]);

        let result = run(dir.path(), "research", true);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        assert!(!graph.get_task("research").unwrap().paused);
        // Downstream task should still be paused
        assert!(graph.get_task("implement").unwrap().paused);
    }

    #[test]
    fn test_propagating_resume_dangling_dep_in_subgraph_fails() {
        let dir = tempdir().unwrap();
        let mut t1 = make_task("research", "Research X", Status::Open);
        t1.paused = true;
        let mut t2 = make_task("implement", "Implement X", Status::Open);
        t2.paused = true;
        t2.after = vec!["research".to_string(), "missing-task".to_string()];
        setup_workgraph(dir.path(), vec![t1, t2]);

        let result = run(dir.path(), "research", false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("structural errors"), "got: {msg}");
        assert!(msg.contains("missing-task"), "got: {msg}");

        // Nothing should have been unpaused (atomic)
        let graph = load_graph(graph_path(dir.path())).unwrap();
        assert!(graph.get_task("research").unwrap().paused);
        assert!(graph.get_task("implement").unwrap().paused);
    }

    #[test]
    fn test_propagating_resume_does_not_affect_unrelated_tasks() {
        let dir = tempdir().unwrap();
        let mut t1 = make_task("a", "Task A", Status::Open);
        t1.paused = true;
        let mut t2 = make_task("b", "Task B", Status::Open);
        t2.paused = true;
        t2.after = vec!["a".to_string()];
        let mut t3 = make_task("unrelated", "Unrelated", Status::Open);
        t3.paused = true;
        setup_workgraph(dir.path(), vec![t1, t2, t3]);

        run(dir.path(), "a", false).unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        assert!(!graph.get_task("a").unwrap().paused);
        assert!(!graph.get_task("b").unwrap().paused);
        // Unrelated task should still be paused
        assert!(graph.get_task("unrelated").unwrap().paused);
    }

    #[test]
    fn test_propagating_resume_skips_already_unpaused() {
        let dir = tempdir().unwrap();
        let mut t1 = make_task("a", "Task A", Status::Open);
        t1.paused = true;
        let mut t2 = make_task("b", "Task B", Status::Open);
        // t2 is NOT paused, but is downstream
        t2.after = vec!["a".to_string()];
        setup_workgraph(dir.path(), vec![t1, t2]);

        let result = run(dir.path(), "a", false);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        assert!(!graph.get_task("a").unwrap().paused);
        assert!(!graph.get_task("b").unwrap().paused);
        // b should have no log entry since it wasn't paused
        assert!(graph.get_task("b").unwrap().log.is_empty());
    }

    #[test]
    fn test_propagating_resume_diamond_shape() {
        let dir = tempdir().unwrap();
        let mut root = make_task("root", "Root", Status::Open);
        root.paused = true;
        let mut left = make_task("left", "Left", Status::Open);
        left.paused = true;
        left.after = vec!["root".to_string()];
        let mut right = make_task("right", "Right", Status::Open);
        right.paused = true;
        right.after = vec!["root".to_string()];
        let mut join = make_task("join", "Join", Status::Open);
        join.paused = true;
        join.after = vec!["left".to_string(), "right".to_string()];
        setup_workgraph(dir.path(), vec![root, left, right, join]);

        run(dir.path(), "root", false).unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        for id in &["root", "left", "right", "join"] {
            assert!(
                !graph.get_task(id).unwrap().paused,
                "{} should be unpaused",
                id
            );
        }
    }

    // --- Publish tests ---

    #[test]
    fn test_publish_with_dangling_dep_fails() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["missing-task".to_string()];
        setup_workgraph(dir.path(), vec![task]);

        let result = publish(dir.path(), "t1", false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("structural errors"), "got: {msg}");
        assert!(msg.contains("dangling"), "got: {msg}");
    }

    #[test]
    fn test_publish_with_valid_deps_succeeds() {
        let dir = tempdir().unwrap();
        let dep = make_task("dep1", "Dependency", Status::Open);
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["dep1".to_string()];
        setup_workgraph(dir.path(), vec![dep, task]);

        let result = publish(dir.path(), "t1", false);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.paused);
        assert!(task.log.last().unwrap().message.contains("published"));
    }

    #[test]
    fn test_publish_no_deps_succeeds() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        setup_workgraph(dir.path(), vec![task]);

        let result = publish(dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_resume_with_multiple_dangling_deps_lists_all() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["missing-a".to_string(), "missing-b".to_string()];
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1", false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("missing-a"), "got: {msg}");
        assert!(msg.contains("missing-b"), "got: {msg}");
    }

    #[test]
    fn test_propagating_resume_with_before_edges() {
        // Test that `before` edges are followed for downstream discovery
        let dir = tempdir().unwrap();
        let mut t1 = make_task("seed", "Seed", Status::Open);
        t1.paused = true;
        t1.before = vec!["downstream".to_string()];
        let mut t2 = make_task("downstream", "Downstream", Status::Open);
        t2.paused = true;
        setup_workgraph(dir.path(), vec![t1, t2]);

        run(dir.path(), "seed", false).unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        assert!(!graph.get_task("seed").unwrap().paused);
        assert!(!graph.get_task("downstream").unwrap().paused);
    }

    #[test]
    fn test_propagating_resume_cycle_without_max_iterations_fails() {
        let dir = tempdir().unwrap();
        let mut t1 = make_task("a", "Task A", Status::Open);
        t1.paused = true;
        t1.after = vec!["b".to_string()];
        let mut t2 = make_task("b", "Task B", Status::Open);
        t2.paused = true;
        t2.after = vec!["a".to_string()];
        setup_workgraph(dir.path(), vec![t1, t2]);

        let result = run(dir.path(), "a", false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Cycle without --max-iterations"), "got: {msg}");

        // Atomic: nothing unpaused
        let graph = load_graph(graph_path(dir.path())).unwrap();
        assert!(graph.get_task("a").unwrap().paused);
        assert!(graph.get_task("b").unwrap().paused);
    }

    #[test]
    fn test_propagating_resume_cycle_with_max_iterations_succeeds() {
        let dir = tempdir().unwrap();
        let mut t1 = make_task("a", "Task A", Status::Open);
        t1.paused = true;
        t1.after = vec!["b".to_string()];
        t1.cycle_config = Some(CycleConfig {
            max_iterations: 3,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        let mut t2 = make_task("b", "Task B", Status::Open);
        t2.paused = true;
        t2.after = vec!["a".to_string()];
        setup_workgraph(dir.path(), vec![t1, t2]);

        let result = run(dir.path(), "a", false);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        assert!(!graph.get_task("a").unwrap().paused);
        assert!(!graph.get_task("b").unwrap().paused);
    }
}
