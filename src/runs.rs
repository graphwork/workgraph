//! Run snapshot and restore logic for `wg replay`.
//!
//! Each replay creates a "run" — a point-in-time snapshot of the graph state
//! stored under `.workgraph/runs/<run-id>/`. Run IDs are auto-incrementing
//! (`run-001`, `run-002`, …).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Metadata for a single run snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    /// Run ID (e.g., "run-001")
    pub id: String,
    /// ISO 8601 timestamp when the snapshot was taken
    pub timestamp: String,
    /// Model override used for replay (if any)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// IDs of tasks that were reset in this replay
    pub reset_tasks: Vec<String>,
    /// IDs of tasks that were preserved (not reset)
    pub preserved_tasks: Vec<String>,
    /// Replay filter description (e.g., "--failed-only", "--tasks a,b")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

/// Directory where all run snapshots live.
pub fn runs_dir(workgraph_dir: &Path) -> PathBuf {
    workgraph_dir.join("runs")
}

/// Directory for a specific run.
pub fn run_dir(workgraph_dir: &Path, run_id: &str) -> PathBuf {
    runs_dir(workgraph_dir).join(run_id)
}

/// Generate the next run ID by scanning existing runs.
pub fn next_run_id(workgraph_dir: &Path) -> String {
    let dir = runs_dir(workgraph_dir);
    let mut max = 0u32;
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(num_str) = name.strip_prefix("run-") {
                if let Ok(num) = num_str.parse::<u32>() {
                    max = max.max(num);
                }
            }
        }
    }
    format!("run-{:03}", max + 1)
}

/// Take a snapshot of the current graph state.
///
/// Copies `graph.jsonl` and `config.toml` into `.workgraph/runs/<run-id>/`.
/// Writes `meta.json` with run metadata.
pub fn snapshot(
    workgraph_dir: &Path,
    run_id: &str,
    meta: &RunMeta,
) -> Result<PathBuf> {
    let dest = run_dir(workgraph_dir, run_id);
    fs::create_dir_all(&dest).context("Failed to create run directory")?;

    // Copy graph.jsonl
    let graph_src = workgraph_dir.join("graph.jsonl");
    if graph_src.exists() {
        fs::copy(&graph_src, dest.join("graph.jsonl"))
            .context("Failed to copy graph.jsonl to snapshot")?;
    }

    // Copy config.toml
    let config_src = workgraph_dir.join("config.toml");
    if config_src.exists() {
        fs::copy(&config_src, dest.join("config.toml"))
            .context("Failed to copy config.toml to snapshot")?;
    }

    // Write run metadata
    let meta_json = serde_json::to_string_pretty(meta)
        .context("Failed to serialize run metadata")?;
    fs::write(dest.join("meta.json"), meta_json)
        .context("Failed to write run metadata")?;

    Ok(dest)
}

/// Load metadata for a specific run.
pub fn load_run_meta(workgraph_dir: &Path, run_id: &str) -> Result<RunMeta> {
    let path = run_dir(workgraph_dir, run_id).join("meta.json");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read run metadata: {}", path.display()))?;
    let meta: RunMeta = serde_json::from_str(&content)
        .context("Failed to parse run metadata")?;
    Ok(meta)
}

/// List all run IDs (sorted ascending).
pub fn list_runs(workgraph_dir: &Path) -> Result<Vec<String>> {
    let dir = runs_dir(workgraph_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(&dir).context("Failed to read runs directory")? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("run-") {
                ids.push(name);
            }
        }
    }
    ids.sort();
    Ok(ids)
}

/// Restore graph.jsonl from a snapshot.
///
/// Replaces the current graph.jsonl with the snapshot's copy.
pub fn restore_graph(workgraph_dir: &Path, run_id: &str) -> Result<()> {
    let src = run_dir(workgraph_dir, run_id).join("graph.jsonl");
    if !src.exists() {
        anyhow::bail!("Snapshot graph.jsonl not found for run '{}'", run_id);
    }
    let dest = workgraph_dir.join("graph.jsonl");
    fs::copy(&src, &dest).context("Failed to restore graph.jsonl from snapshot")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    fn setup_wg(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("graph.jsonl"), "{\"kind\":\"task\",\"id\":\"t1\",\"title\":\"Test\"}\n").unwrap();
        fs::write(dir.join("config.toml"), "[agent]\nmodel = \"opus\"\n").unwrap();
    }

    #[test]
    fn test_next_run_id_empty() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(next_run_id(&dir), "run-001");
    }

    #[test]
    fn test_next_run_id_increments() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        let runs = dir.join("runs");
        fs::create_dir_all(runs.join("run-001")).unwrap();
        fs::create_dir_all(runs.join("run-003")).unwrap();
        assert_eq!(next_run_id(&dir), "run-004");
    }

    #[test]
    fn test_snapshot_and_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        setup_wg(&dir);

        let meta = RunMeta {
            id: "run-001".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            model: Some("sonnet".to_string()),
            reset_tasks: vec!["t1".to_string()],
            preserved_tasks: vec![],
            filter: Some("--failed-only".to_string()),
        };

        let snap_path = snapshot(&dir, "run-001", &meta).unwrap();
        assert!(snap_path.join("graph.jsonl").exists());
        assert!(snap_path.join("config.toml").exists());
        assert!(snap_path.join("meta.json").exists());

        let loaded = load_run_meta(&dir, "run-001").unwrap();
        assert_eq!(loaded.id, "run-001");
        assert_eq!(loaded.model, Some("sonnet".to_string()));
        assert_eq!(loaded.reset_tasks, vec!["t1"]);
    }

    #[test]
    fn test_list_runs() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        let runs = dir.join("runs");
        fs::create_dir_all(runs.join("run-001")).unwrap();
        fs::create_dir_all(runs.join("run-002")).unwrap();
        fs::create_dir_all(runs.join("not-a-run")).unwrap();

        let ids = list_runs(&dir).unwrap();
        assert_eq!(ids, vec!["run-001", "run-002"]);
    }

    #[test]
    fn test_restore_graph() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        setup_wg(&dir);

        let meta = RunMeta {
            id: "run-001".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            model: None,
            reset_tasks: vec![],
            preserved_tasks: vec!["t1".to_string()],
            filter: None,
        };
        snapshot(&dir, "run-001", &meta).unwrap();

        // Modify graph
        fs::write(dir.join("graph.jsonl"), "MODIFIED\n").unwrap();
        assert_eq!(fs::read_to_string(dir.join("graph.jsonl")).unwrap(), "MODIFIED\n");

        // Restore
        restore_graph(&dir, "run-001").unwrap();
        let content = fs::read_to_string(dir.join("graph.jsonl")).unwrap();
        assert!(content.contains("t1"));
    }
}
