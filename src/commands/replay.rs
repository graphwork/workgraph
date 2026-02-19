//! `wg replay` — snapshot graph state, selectively reset tasks, and re-execute.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use workgraph::agency::{load_all_evaluations_or_warn, Evaluation};
use workgraph::config::Config;
use workgraph::graph::{Status, Task};
use workgraph::parser::save_graph;
use workgraph::runs::{self, RunMeta};

/// Options controlling which tasks to reset.
pub struct ReplayOptions {
    pub model: Option<String>,
    pub failed_only: bool,
    pub below_score: Option<f64>,
    pub tasks: Vec<String>,
    pub keep_done: Option<f64>,
    pub plan_only: bool,
    pub subgraph: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReplayOutput {
    run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    reset_tasks: Vec<String>,
    preserved_tasks: Vec<String>,
    plan_only: bool,
}

pub fn run(dir: &Path, opts: &ReplayOptions, json: bool) -> Result<()> {
    let (mut graph, graph_path) = super::load_workgraph_mut(dir)?;
    let config = Config::load_or_default(dir);

    // Determine keep_done threshold
    let keep_done_threshold = opts.keep_done.unwrap_or(config.replay.keep_done_threshold);

    // Load evaluations for --below-score and --keep-done filtering
    let agency_dir = dir.join("agency");
    let evaluations = if opts.below_score.is_some() || keep_done_threshold < 1.0 {
        load_all_evaluations_or_warn(&agency_dir)
    } else {
        Vec::new()
    };

    // Build a map of task_id -> best evaluation score
    let score_map = build_score_map(&evaluations);

    // Collect tasks in subgraph if --subgraph is specified
    let subgraph_ids: Option<HashSet<String>> = if let Some(root) = &opts.subgraph {
        Some(collect_subgraph(&graph, root)?)
    } else {
        None
    };

    // Phase 1: Determine which tasks to reset (seed set)
    let mut seeds: HashSet<String> = HashSet::new();

    for task in graph.tasks() {
        // If subgraph filter is active, skip tasks outside the subgraph
        if let Some(ref sg) = subgraph_ids {
            if !sg.contains(&task.id) {
                continue;
            }
        }

        if !opts.tasks.is_empty() {
            // Explicit task list: only seed listed tasks
            if opts.tasks.contains(&task.id) {
                seeds.insert(task.id.clone());
            }
            continue;
        }

        if opts.failed_only {
            if matches!(task.status, Status::Failed | Status::Abandoned) {
                seeds.insert(task.id.clone());
            }
            continue;
        }

        if let Some(threshold) = opts.below_score {
            let score = score_map.get(&task.id).copied();
            if let Some(s) = score {
                if s < threshold {
                    seeds.insert(task.id.clone());
                }
            } else if task.status.is_terminal() {
                // No eval score — reset if terminal (no evidence of quality)
                seeds.insert(task.id.clone());
            }
            continue;
        }

        // Default: reset all terminal tasks (unless kept by --keep-done)
        if task.status.is_terminal() {
            seeds.insert(task.id.clone());
        }
    }

    // Phase 2: Collect transitive dependents of seed tasks
    let reverse_index = build_reverse_index(&graph);
    let mut all_to_reset = seeds.clone();
    for seed in &seeds {
        super::collect_transitive_dependents(&reverse_index, seed, &mut all_to_reset);
    }

    // Phase 3: Apply --keep-done — remove Done tasks with score above threshold
    if keep_done_threshold < 1.0 {
        let mut to_keep = Vec::new();
        for task_id in &all_to_reset {
            if let Some(task) = graph.get_task(task_id) {
                if task.status == Status::Done {
                    if let Some(&score) = score_map.get(task_id) {
                        if score >= keep_done_threshold {
                            to_keep.push(task_id.clone());
                        }
                    }
                }
            }
        }
        for id in to_keep {
            all_to_reset.remove(&id);
        }
    }

    // Sort for deterministic output
    let mut reset_ids: Vec<String> = all_to_reset.into_iter().collect();
    reset_ids.sort();

    let mut preserved_ids: Vec<String> = graph
        .tasks()
        .filter(|t| !reset_ids.contains(&t.id))
        .map(|t| t.id.clone())
        .collect();
    preserved_ids.sort();

    if opts.plan_only {
        let output = ReplayOutput {
            run_id: "(dry run)".to_string(),
            model: opts.model.clone(),
            reset_tasks: reset_ids.clone(),
            preserved_tasks: preserved_ids.clone(),
            plan_only: true,
        };
        if json {
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("Replay plan (dry run — no changes will be made):\n");
            if let Some(ref m) = opts.model {
                println!("  Model override: {}", m);
            }
            println!("  Tasks to reset ({}):", reset_ids.len());
            for id in &reset_ids {
                let status = graph
                    .get_task(id)
                    .map(|t| t.status.to_string())
                    .unwrap_or_default();
                let score = score_map
                    .get(id)
                    .map(|s| format!(" (score: {:.2})", s))
                    .unwrap_or_default();
                println!("    {} [{}]{}", id, status, score);
            }
            println!("  Tasks preserved ({}):", preserved_ids.len());
            for id in &preserved_ids {
                let status = graph
                    .get_task(id)
                    .map(|t| t.status.to_string())
                    .unwrap_or_default();
                println!("    {} [{}]", id, status);
            }
        }
        return Ok(());
    }

    if reset_ids.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "message": "No tasks match the replay criteria",
                    "reset_tasks": [],
                }))?
            );
        } else {
            println!("No tasks match the replay criteria. Nothing to replay.");
        }
        return Ok(());
    }

    // Phase 4: Snapshot current state
    let run_id = runs::next_run_id(dir);
    let filter_desc = build_filter_desc(opts);
    let meta = RunMeta {
        id: run_id.clone(),
        timestamp: Utc::now().to_rfc3339(),
        model: opts.model.clone(),
        reset_tasks: reset_ids.clone(),
        preserved_tasks: preserved_ids.clone(),
        filter: Some(filter_desc),
    };
    runs::snapshot(dir, &run_id, &meta)?;

    // Phase 5: Reset selected tasks
    for task_id in &reset_ids {
        if let Some(task) = graph.get_task_mut(task_id) {
            reset_task(task);
            // Apply model override
            if let Some(ref model) = opts.model {
                task.model = Some(model.clone());
            }
        }
    }

    // Phase 6: Save graph
    save_graph(&graph, &graph_path).context("Failed to save graph after replay")?;
    super::notify_graph_changed(dir);

    // Phase 7: Record provenance
    let _ = workgraph::provenance::record(
        dir,
        "replay",
        None,
        None,
        serde_json::json!({
            "run_id": run_id,
            "model": opts.model,
            "reset_count": reset_ids.len(),
            "reset_tasks": reset_ids,
        }),
        config.log.rotation_threshold,
    );

    // Output
    let output = ReplayOutput {
        run_id: run_id.clone(),
        model: opts.model.clone(),
        reset_tasks: reset_ids.clone(),
        preserved_tasks: preserved_ids.clone(),
        plan_only: false,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Replay started: {}", run_id);
        println!("  Snapshot saved to .workgraph/runs/{}/", run_id);
        if let Some(ref m) = opts.model {
            println!("  Model override: {}", m);
        }
        println!("  Reset {} task(s):", reset_ids.len());
        for id in &reset_ids {
            println!("    {}", id);
        }
        println!("  Preserved {} task(s)", preserved_ids.len());
        super::print_service_hint(dir);
    }

    Ok(())
}

/// Reset a task to Open status, clearing execution state but preserving structure.
fn reset_task(task: &mut Task) {
    task.status = Status::Open;
    task.assigned = None;
    task.started_at = None;
    task.completed_at = None;
    task.artifacts.clear();
    task.loop_iteration = 0;
    task.failure_reason = None;
    task.paused = false;
    // Preserve: log, blocked_by, blocks, description, tags, skills, etc.
}

/// Build a reverse dependency index: task_id -> list of tasks that depend on it.
fn build_reverse_index(
    graph: &workgraph::graph::WorkGraph,
) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for task in graph.tasks() {
        for dep in &task.blocked_by {
            index
                .entry(dep.clone())
                .or_default()
                .push(task.id.clone());
        }
    }
    index
}

/// Build a map of task_id -> best evaluation score.
fn build_score_map(evaluations: &[Evaluation]) -> HashMap<String, f64> {
    let mut map: HashMap<String, f64> = HashMap::new();
    for eval in evaluations {
        let entry = map.entry(eval.task_id.clone()).or_insert(0.0);
        if eval.score > *entry {
            *entry = eval.score;
        }
    }
    map
}

/// Collect all task IDs in the subgraph rooted at `root_id` (including root).
/// Follows `blocks` edges forward (root blocks children).
fn collect_subgraph(
    graph: &workgraph::graph::WorkGraph,
    root_id: &str,
) -> Result<HashSet<String>> {
    let _ = graph
        .get_task_or_err(root_id)
        .context("Subgraph root task not found")?;

    let mut result = HashSet::new();
    let mut queue = vec![root_id.to_string()];
    while let Some(id) = queue.pop() {
        if result.insert(id.clone()) {
            if let Some(task) = graph.get_task(&id) {
                for blocked in &task.blocks {
                    queue.push(blocked.clone());
                }
            }
        }
    }
    Ok(result)
}

/// Build a human-readable description of the replay filter.
fn build_filter_desc(opts: &ReplayOptions) -> String {
    let mut parts = Vec::new();
    if opts.failed_only {
        parts.push("--failed-only".to_string());
    }
    if let Some(t) = opts.below_score {
        parts.push(format!("--below-score {}", t));
    }
    if !opts.tasks.is_empty() {
        parts.push(format!("--tasks {}", opts.tasks.join(",")));
    }
    if let Some(ref m) = opts.model {
        parts.push(format!("--model {}", m));
    }
    if let Some(t) = opts.keep_done {
        parts.push(format!("--keep-done {}", t));
    }
    if let Some(ref s) = opts.subgraph {
        parts.push(format!("--subgraph {}", s));
    }
    if parts.is_empty() {
        "all tasks".to_string()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::test_helpers::{make_task, make_task_with_status, setup_workgraph};

    fn make_dir() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        (tmp, dir)
    }

    #[test]
    fn test_replay_failed_only() {
        let (_tmp, dir) = make_dir();
        let mut t1 = make_task_with_status("t1", "Task 1", Status::Done);
        t1.completed_at = Some(Utc::now().to_rfc3339());
        let mut t2 = make_task_with_status("t2", "Task 2", Status::Failed);
        t2.failure_reason = Some("broken".to_string());
        let t3 = make_task("t3", "Task 3");
        setup_workgraph(&dir, vec![t1, t2, t3]);

        let opts = ReplayOptions {
            model: None,
            failed_only: true,
            below_score: None,
            tasks: vec![],
            keep_done: None,
            plan_only: false,
            subgraph: None,
        };

        run(&dir, &opts, false).unwrap();

        // Verify: t2 should be reset to Open, t1 should still be Done
        let (graph, _) = super::super::load_workgraph(&dir).unwrap();
        assert_eq!(graph.get_task("t2").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("t1").unwrap().status, Status::Done);

        // Verify snapshot was created
        let runs = runs::list_runs(&dir).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0], "run-001");
    }

    #[test]
    fn test_replay_specific_tasks_with_dependents() {
        let (_tmp, dir) = make_dir();
        let mut t1 = make_task_with_status("t1", "Root", Status::Done);
        t1.blocks = vec!["t2".to_string()];
        let mut t2 = make_task_with_status("t2", "Middle", Status::Done);
        t2.blocked_by = vec!["t1".to_string()];
        t2.blocks = vec!["t3".to_string()];
        let mut t3 = make_task_with_status("t3", "Leaf", Status::Done);
        t3.blocked_by = vec!["t2".to_string()];
        setup_workgraph(&dir, vec![t1, t2, t3]);

        let opts = ReplayOptions {
            model: Some("sonnet".to_string()),
            failed_only: false,
            below_score: None,
            tasks: vec!["t1".to_string()],
            keep_done: None,
            plan_only: false,
            subgraph: None,
        };

        run(&dir, &opts, false).unwrap();

        let (graph, _) = super::super::load_workgraph(&dir).unwrap();
        // t1 was explicitly listed => reset
        assert_eq!(graph.get_task("t1").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("t1").unwrap().model, Some("sonnet".to_string()));
        // t2 and t3 are transitive dependents of t1 => also reset
        assert_eq!(graph.get_task("t2").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("t3").unwrap().status, Status::Open);
    }

    #[test]
    fn test_replay_plan_only() {
        let (_tmp, dir) = make_dir();
        let t1 = make_task_with_status("t1", "Task 1", Status::Failed);
        setup_workgraph(&dir, vec![t1]);

        let opts = ReplayOptions {
            model: None,
            failed_only: true,
            below_score: None,
            tasks: vec![],
            keep_done: None,
            plan_only: true,
            subgraph: None,
        };

        run(&dir, &opts, false).unwrap();

        // Verify no changes were made
        let (graph, _) = super::super::load_workgraph(&dir).unwrap();
        assert_eq!(graph.get_task("t1").unwrap().status, Status::Failed);

        // Verify no snapshot was created
        let runs = runs::list_runs(&dir).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn test_replay_subgraph() {
        let (_tmp, dir) = make_dir();
        let mut root = make_task_with_status("root", "Root", Status::Done);
        root.blocks = vec!["child".to_string()];
        let mut child = make_task_with_status("child", "Child", Status::Failed);
        child.blocked_by = vec!["root".to_string()];
        let other = make_task_with_status("other", "Unrelated", Status::Failed);
        setup_workgraph(&dir, vec![root, child, other]);

        let opts = ReplayOptions {
            model: None,
            failed_only: true,
            below_score: None,
            tasks: vec![],
            keep_done: None,
            plan_only: false,
            subgraph: Some("root".to_string()),
        };

        run(&dir, &opts, false).unwrap();

        let (graph, _) = super::super::load_workgraph(&dir).unwrap();
        // child is in subgraph and failed => reset
        assert_eq!(graph.get_task("child").unwrap().status, Status::Open);
        // other is not in subgraph => not reset
        assert_eq!(graph.get_task("other").unwrap().status, Status::Failed);
    }

    #[test]
    fn test_reset_task_clears_fields() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            status: Status::Done,
            assigned: Some("agent-1".to_string()),
            started_at: Some(Utc::now().to_rfc3339()),
            completed_at: Some(Utc::now().to_rfc3339()),
            artifacts: vec!["file.rs".to_string()],
            loop_iteration: 3,
            failure_reason: Some("err".to_string()),
            blocked_by: vec!["dep".to_string()],
            ..Task::default()
        };

        reset_task(&mut task);

        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert!(task.started_at.is_none());
        assert!(task.completed_at.is_none());
        assert!(task.artifacts.is_empty());
        assert_eq!(task.loop_iteration, 0);
        assert!(task.failure_reason.is_none());
        // Preserved:
        assert_eq!(task.blocked_by, vec!["dep"]);
        assert_eq!(task.title, "Test");
    }

    #[test]
    fn test_replay_no_matching_tasks() {
        let (_tmp, dir) = make_dir();
        let t1 = make_task("t1", "Open task");
        setup_workgraph(&dir, vec![t1]);

        let opts = ReplayOptions {
            model: None,
            failed_only: true,
            below_score: None,
            tasks: vec![],
            keep_done: None,
            plan_only: false,
            subgraph: None,
        };

        // Should succeed without error
        run(&dir, &opts, false).unwrap();

        // No snapshot created
        let runs = runs::list_runs(&dir).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn test_replay_all_terminal() {
        let (_tmp, dir) = make_dir();
        let t1 = make_task_with_status("t1", "Done", Status::Done);
        let t2 = make_task_with_status("t2", "Failed", Status::Failed);
        let t3 = make_task_with_status("t3", "Abandoned", Status::Abandoned);
        let t4 = make_task("t4", "Open");
        setup_workgraph(&dir, vec![t1, t2, t3, t4]);

        let opts = ReplayOptions {
            model: Some("haiku".to_string()),
            failed_only: false,
            below_score: None,
            tasks: vec![],
            keep_done: Some(1.0), // don't keep any done tasks (threshold unreachable)
            plan_only: false,
            subgraph: None,
        };

        run(&dir, &opts, false).unwrap();

        let (graph, _) = super::super::load_workgraph(&dir).unwrap();
        assert_eq!(graph.get_task("t1").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("t2").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("t3").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("t4").unwrap().status, Status::Open); // was already open
    }

    #[test]
    fn test_build_reverse_index() {
        let (_tmp, dir) = make_dir();
        let mut t1 = make_task("t1", "Root");
        t1.blocks = vec!["t2".to_string(), "t3".to_string()];
        let mut t2 = make_task("t2", "Mid");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Leaf");
        t3.blocked_by = vec!["t1".to_string()];
        setup_workgraph(&dir, vec![t1, t2, t3]);

        let (graph, _) = super::super::load_workgraph(&dir).unwrap();
        let index = build_reverse_index(&graph);

        assert!(index.get("t1").unwrap().contains(&"t2".to_string()));
        assert!(index.get("t1").unwrap().contains(&"t3".to_string()));
    }
}
