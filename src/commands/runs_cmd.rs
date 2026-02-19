//! `wg runs` — list, show, restore, and diff run snapshots.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use workgraph::config::Config;
use workgraph::parser::load_graph;
use workgraph::runs::{self, RunMeta};

/// List all run snapshots.
pub fn run_list(dir: &Path, json: bool) -> Result<()> {
    let ids = runs::list_runs(dir)?;
    if ids.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No run snapshots found.");
        }
        return Ok(());
    }

    let mut metas = Vec::new();
    for id in &ids {
        match runs::load_run_meta(dir, id) {
            Ok(meta) => metas.push(meta),
            Err(e) => {
                eprintln!("Warning: could not load metadata for {}: {}", id, e);
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&metas)?);
    } else {
        println!("Run snapshots:\n");
        for meta in &metas {
            println!("  {} ({})", meta.id, meta.timestamp);
            if let Some(ref model) = meta.model {
                println!("    Model: {}", model);
            }
            if let Some(ref filter) = meta.filter {
                println!("    Filter: {}", filter);
            }
            println!(
                "    Reset: {} task(s), Preserved: {} task(s)",
                meta.reset_tasks.len(),
                meta.preserved_tasks.len()
            );
        }
    }
    Ok(())
}

/// Show details of a specific run.
pub fn run_show(dir: &Path, run_id: &str, json: bool) -> Result<()> {
    let meta = runs::load_run_meta(dir, run_id)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&meta)?);
    } else {
        println!("Run: {}", meta.id);
        println!("  Timestamp: {}", meta.timestamp);
        if let Some(ref model) = meta.model {
            println!("  Model: {}", model);
        }
        if let Some(ref filter) = meta.filter {
            println!("  Filter: {}", filter);
        }
        println!("  Reset tasks ({}):", meta.reset_tasks.len());
        for id in &meta.reset_tasks {
            println!("    {}", id);
        }
        println!("  Preserved tasks ({}):", meta.preserved_tasks.len());
        for id in &meta.preserved_tasks {
            println!("    {}", id);
        }
    }
    Ok(())
}

/// Restore graph from a run snapshot.
pub fn run_restore(dir: &Path, run_id: &str, json: bool) -> Result<()> {
    // First verify the run exists
    let meta = runs::load_run_meta(dir, run_id)?;

    // Take a snapshot of current state before restoring (safety net)
    let safety_id = runs::next_run_id(dir);
    let safety_meta = RunMeta {
        id: safety_id.clone(),
        timestamp: Utc::now().to_rfc3339(),
        model: None,
        reset_tasks: vec![],
        preserved_tasks: vec![],
        filter: Some(format!("pre-restore safety snapshot (restoring {})", run_id)),
    };
    runs::snapshot(dir, &safety_id, &safety_meta)?;

    // Restore the graph
    runs::restore_graph(dir, run_id)?;
    super::notify_graph_changed(dir);

    // Record provenance
    let config = Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "restore",
        None,
        None,
        serde_json::json!({
            "restored_from": run_id,
            "safety_snapshot": safety_id,
        }),
        config.log.rotation_threshold,
    );

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "restored_from": run_id,
                "safety_snapshot": safety_id,
                "timestamp": meta.timestamp,
            }))?
        );
    } else {
        println!("Restored graph from {}", run_id);
        println!("  Safety snapshot: {} (in case you need to undo)", safety_id);
    }
    Ok(())
}

/// Diff current graph against a run snapshot.
pub fn run_diff(dir: &Path, run_id: &str, json: bool) -> Result<()> {
    let snap_graph_path = runs::run_dir(dir, run_id).join("graph.jsonl");
    if !snap_graph_path.exists() {
        anyhow::bail!("Snapshot graph.jsonl not found for run '{}'", run_id);
    }

    let snap_graph = load_graph(&snap_graph_path)
        .context("Failed to load snapshot graph")?;
    let (current_graph, _) = super::load_workgraph(dir)?;

    // Build maps: task_id -> status
    let snap_statuses: HashMap<String, String> = snap_graph
        .tasks()
        .map(|t| (t.id.clone(), t.status.to_string()))
        .collect();
    let current_statuses: HashMap<String, String> = current_graph
        .tasks()
        .map(|t| (t.id.clone(), t.status.to_string()))
        .collect();

    #[derive(Debug, Serialize)]
    struct TaskDiff {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot_status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_status: Option<String>,
        change: String,
    }

    let mut diffs = Vec::new();

    // Tasks in both
    let mut all_ids: Vec<String> = snap_statuses
        .keys()
        .chain(current_statuses.keys())
        .cloned()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    all_ids.sort();

    for id in &all_ids {
        let snap = snap_statuses.get(id);
        let current = current_statuses.get(id);
        match (snap, current) {
            (Some(s), Some(c)) if s != c => {
                diffs.push(TaskDiff {
                    id: id.clone(),
                    snapshot_status: Some(s.clone()),
                    current_status: Some(c.clone()),
                    change: "status_changed".to_string(),
                });
            }
            (Some(_), None) => {
                diffs.push(TaskDiff {
                    id: id.clone(),
                    snapshot_status: snap.cloned(),
                    current_status: None,
                    change: "removed".to_string(),
                });
            }
            (None, Some(_)) => {
                diffs.push(TaskDiff {
                    id: id.clone(),
                    snapshot_status: None,
                    current_status: current.cloned(),
                    change: "added".to_string(),
                });
            }
            _ => {} // No change
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "run_id": run_id,
                "changes": diffs,
                "total_changes": diffs.len(),
            }))?
        );
    } else {
        if diffs.is_empty() {
            println!("No differences between current graph and {}", run_id);
        } else {
            println!("Diff: current vs {} ({} change(s)):\n", run_id, diffs.len());
            for d in &diffs {
                match d.change.as_str() {
                    "status_changed" => {
                        println!(
                            "  ~ {} : {} -> {}",
                            d.id,
                            d.snapshot_status.as_deref().unwrap_or("?"),
                            d.current_status.as_deref().unwrap_or("?")
                        );
                    }
                    "removed" => {
                        println!(
                            "  - {} [{}]",
                            d.id,
                            d.snapshot_status.as_deref().unwrap_or("?")
                        );
                    }
                    "added" => {
                        println!(
                            "  + {} [{}]",
                            d.id,
                            d.current_status.as_deref().unwrap_or("?")
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::Status;
    use workgraph::parser::save_graph;
    use workgraph::test_helpers::{make_task, make_task_with_status, setup_workgraph};

    fn make_dir() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        (tmp, dir)
    }

    #[test]
    fn test_list_empty() {
        let (_tmp, dir) = make_dir();
        std::fs::create_dir_all(&dir).unwrap();
        run_list(&dir, false).unwrap();
    }

    #[test]
    fn test_show_run() {
        let (_tmp, dir) = make_dir();
        setup_workgraph(&dir, vec![make_task("t1", "Test")]);

        let meta = RunMeta {
            id: "run-001".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            model: Some("sonnet".to_string()),
            reset_tasks: vec!["t1".to_string()],
            preserved_tasks: vec![],
            filter: Some("--failed-only".to_string()),
        };
        runs::snapshot(&dir, "run-001", &meta).unwrap();

        run_show(&dir, "run-001", false).unwrap();
    }

    #[test]
    fn test_restore_creates_safety_snapshot() {
        let (_tmp, dir) = make_dir();
        let t1 = make_task_with_status("t1", "Test", Status::Done);
        setup_workgraph(&dir, vec![t1]);

        let meta = RunMeta {
            id: "run-001".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            model: None,
            reset_tasks: vec![],
            preserved_tasks: vec!["t1".to_string()],
            filter: None,
        };
        runs::snapshot(&dir, "run-001", &meta).unwrap();

        run_restore(&dir, "run-001", false).unwrap();

        // Should have run-001 + safety run-002
        let ids = runs::list_runs(&dir).unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[1], "run-002");
    }

    #[test]
    fn test_diff_shows_changes() {
        let (_tmp, dir) = make_dir();
        let t1 = make_task_with_status("t1", "Task 1", Status::Done);
        let t2 = make_task_with_status("t2", "Task 2", Status::Failed);
        setup_workgraph(&dir, vec![t1, t2]);

        // Snapshot current state
        let meta = RunMeta {
            id: "run-001".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            model: None,
            reset_tasks: vec![],
            preserved_tasks: vec!["t1".to_string(), "t2".to_string()],
            filter: None,
        };
        runs::snapshot(&dir, "run-001", &meta).unwrap();

        // Modify current graph: reset t2 to open
        let (mut graph, path) = super::super::load_workgraph_mut(&dir).unwrap();
        graph.get_task_mut("t2").unwrap().status = Status::Open;
        save_graph(&graph, &path).unwrap();

        // Diff should show t2 changed
        run_diff(&dir, "run-001", false).unwrap();
    }
}
