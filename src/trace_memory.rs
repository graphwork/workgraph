//! Trace memory for adaptive trace functions (Layer 3).
//!
//! Two storage strategies:
//! - **JSONL** (append-only): `.workgraph/functions/<func_id>.runs.jsonl` — used by
//!   `append_run_summary` / `load_run_summaries` for streaming writes.
//! - **Per-run JSON** (spec §3.4): `.workgraph/functions/<func_id>.memory/<timestamp>.json`
//!   — used by `save_run_summary` / `load_recent_summaries` for individual run files.
//!
//! Both formats are supported. The per-run JSON approach (`save_run_summary` /
//! `load_recent_summaries`) is the protocol-specified storage for §4.3 MAKE_ADAPTIVE.
//! The JSONL approach (`append_run_summary` / `load_run_summaries`) is the existing
//! integration point used by `trace_instantiate` and `trace_make_adaptive`.

use crate::agency::{load_all_evaluations, Evaluation};
use crate::graph::WorkGraph;
use crate::provenance::{read_all_operations, OperationEntry};
use crate::trace_function::{
    InterventionSummary, MemoryInclusions, RunSummary, TaskOutcome, TraceMemoryConfig,
    FUNCTIONS_DIR,
};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Per-run JSON storage (spec §3.4 / §4.3)
// ---------------------------------------------------------------------------

/// Return the memory directory for a function: `.workgraph/functions/<func_id>.memory/`
pub fn memory_dir(workgraph_dir: &Path, func_id: &str) -> PathBuf {
    workgraph_dir
        .join(FUNCTIONS_DIR)
        .join(format!("{}.memory", func_id))
}

/// Save a run summary as JSON to `.workgraph/functions/<func_id>.memory/<timestamp>.json`.
///
/// The timestamp is derived from `summary.instantiated_at`, sanitized for use as a filename
/// (colons replaced with dashes).
pub fn save_run_summary(
    func_id: &str,
    summary: &RunSummary,
    workgraph_dir: &Path,
) -> Result<PathBuf> {
    let dir = memory_dir(workgraph_dir, func_id);
    fs::create_dir_all(&dir).context("Failed to create memory directory")?;

    let safe_ts = summary.instantiated_at.replace(':', "-");
    let filename = format!("{}.json", safe_ts);
    let path = dir.join(filename);

    let json = serde_json::to_string_pretty(summary)
        .context("Failed to serialize RunSummary to JSON")?;
    fs::write(&path, json).context("Failed to write run summary file")?;

    Ok(path)
}

/// Load the most recent N run summaries for a function, sorted newest-first.
///
/// Reads all `.json` files from the `.memory/` directory, parses them, sorts by
/// `instantiated_at` descending, and returns at most `max_runs` entries.
pub fn load_recent_summaries(
    func_id: &str,
    max_runs: usize,
    workgraph_dir: &Path,
) -> Result<Vec<RunSummary>> {
    let dir = memory_dir(workgraph_dir, func_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut summaries = Vec::new();
    for entry in fs::read_dir(&dir).context("Failed to read memory directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            let summary: RunSummary = serde_json::from_str(&contents)
                .with_context(|| format!("Failed to parse {}", path.display()))?;
            summaries.push(summary);
        }
    }

    // Sort newest-first by instantiated_at (ISO 8601 strings sort lexicographically)
    summaries.sort_by(|a, b| b.instantiated_at.cmp(&a.instantiated_at));
    summaries.truncate(max_runs);

    Ok(summaries)
}

/// Build a RunSummary from a set of completed task IDs.
///
/// Reads task statuses and timestamps from the graph, evaluation scores from the
/// evaluations directory, and detects interventions (retries, edits) from provenance ops.
///
/// `task_ids` should be the task IDs created by a single instantiation (e.g. all tasks
/// under a common prefix). The `instantiated_at` and `prefix` fields must be provided
/// since they cannot be reliably inferred from task data alone.
pub fn build_run_summary(
    task_ids: &[String],
    graph: &WorkGraph,
    evaluations_dir: &Path,
    provenance_path: &Path,
    instantiated_at: &str,
    prefix: &str,
) -> Result<RunSummary> {
    // Load evaluations keyed by task_id (use most recent per task)
    let evals = load_all_evaluations(evaluations_dir).unwrap_or_default();
    let mut eval_by_task: HashMap<&str, &Evaluation> = HashMap::new();
    for eval in &evals {
        eval_by_task
            .entry(eval.task_id.as_str())
            .and_modify(|existing| {
                if eval.timestamp > existing.timestamp {
                    *existing = eval;
                }
            })
            .or_insert(eval);
    }

    // Load provenance operations filtered to our task_ids
    let task_id_set: std::collections::HashSet<&str> =
        task_ids.iter().map(|s| s.as_str()).collect();
    let all_ops = read_all_operations(provenance_path).unwrap_or_default();
    let relevant_ops: Vec<&OperationEntry> = all_ops
        .iter()
        .filter(|op| {
            op.task_id
                .as_ref()
                .is_some_and(|tid| task_id_set.contains(tid.as_str()))
        })
        .collect();

    // Build task outcomes
    let mut task_outcomes = Vec::new();
    let mut all_succeeded = true;
    let mut scores = Vec::new();

    for task_id in task_ids {
        let task = match graph.get_task(task_id) {
            Some(t) => t,
            None => continue,
        };

        let status_str = format!("{:?}", task.status);
        if status_str != "Done" {
            all_succeeded = false;
        }

        // Compute duration from started_at to completed_at
        let duration_secs = compute_duration(&task.started_at, &task.completed_at);

        // Look up evaluation score
        let score = eval_by_task.get(task_id.as_str()).map(|e| e.score);
        if let Some(s) = score {
            scores.push(s);
        }

        // Derive template_id by stripping the prefix from task_id
        let template_id = if !prefix.is_empty() && task_id.starts_with(prefix) {
            task_id[prefix.len()..].to_string()
        } else {
            task_id.clone()
        };

        task_outcomes.push(TaskOutcome {
            template_id,
            task_id: task_id.clone(),
            status: status_str,
            score,
            duration_secs,
            retry_count: task.retry_count,
        });
    }

    // Detect interventions from provenance ops
    let interventions = detect_interventions(&relevant_ops);

    // Compute wall-clock time: earliest started_at to latest completed_at
    let wall_clock_secs = compute_wall_clock(task_ids, graph);

    // Average score
    let avg_score = if scores.is_empty() {
        None
    } else {
        Some(scores.iter().sum::<f64>() / scores.len() as f64)
    };

    Ok(RunSummary {
        instantiated_at: instantiated_at.to_string(),
        inputs: HashMap::new(), // Caller should set inputs if available
        prefix: prefix.to_string(),
        task_outcomes,
        interventions,
        wall_clock_secs,
        all_succeeded,
        avg_score,
    })
}

/// Render run summaries as human-readable text for injection into planner prompts.
///
/// Produces a concise multi-line text block summarizing each past run.
/// This is the simple variant; use `render_run_summaries` for config-aware rendering.
pub fn render_summaries_text(summaries: &[RunSummary]) -> String {
    if summaries.is_empty() {
        return "No previous runs recorded.".to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!("=== Past Runs ({} total) ===", summaries.len()));

    for (i, summary) in summaries.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!(
            "--- Run {} ({}){} ---",
            i + 1,
            summary.instantiated_at,
            if summary.all_succeeded {
                " [SUCCESS]"
            } else {
                " [ISSUES]"
            }
        ));

        if let Some(avg) = summary.avg_score {
            lines.push(format!("  Avg score: {:.2}", avg));
        }
        if let Some(wall) = summary.wall_clock_secs {
            lines.push(format!(
                "  Wall clock: {}",
                crate::format_duration(wall, false)
            ));
        }

        // Task outcomes
        if !summary.task_outcomes.is_empty() {
            lines.push("  Tasks:".to_string());
            for outcome in &summary.task_outcomes {
                let mut parts = vec![format!(
                    "    - {} ({})",
                    outcome.template_id, outcome.status
                )];
                if let Some(s) = outcome.score {
                    parts.push(format!("score={:.2}", s));
                }
                if let Some(d) = outcome.duration_secs {
                    parts.push(format!(
                        "duration={}",
                        crate::format_duration(d, false)
                    ));
                }
                if outcome.retry_count > 0 {
                    parts.push(format!("retries={}", outcome.retry_count));
                }
                lines.push(parts.join(" "));
            }
        }

        // Interventions
        if !summary.interventions.is_empty() {
            lines.push("  Interventions:".to_string());
            for intervention in &summary.interventions {
                let desc = intervention
                    .description
                    .as_deref()
                    .unwrap_or("(no details)");
                lines.push(format!(
                    "    - {} on {}: {}",
                    intervention.kind, intervention.task_id, desc
                ));
            }
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse two ISO 8601 timestamps and return the difference in seconds.
fn compute_duration(started: &Option<String>, completed: &Option<String>) -> Option<i64> {
    let start = started.as_ref()?;
    let end = completed.as_ref()?;
    let start_dt = chrono::DateTime::parse_from_rfc3339(start).ok()?;
    let end_dt = chrono::DateTime::parse_from_rfc3339(end).ok()?;
    Some(end_dt.signed_duration_since(start_dt).num_seconds())
}

/// Compute wall-clock time from earliest started_at to latest completed_at across tasks.
fn compute_wall_clock(task_ids: &[String], graph: &WorkGraph) -> Option<i64> {
    let mut earliest_start: Option<&str> = None;
    let mut latest_end: Option<&str> = None;

    for task_id in task_ids {
        if let Some(task) = graph.get_task(task_id) {
            if let Some(ref s) = task.started_at
                && earliest_start.is_none_or(|es| s.as_str() < es) {
                    earliest_start = Some(s.as_str());
                }
            if let Some(ref c) = task.completed_at
                && latest_end.is_none_or(|le| c.as_str() > le) {
                    latest_end = Some(c.as_str());
                }
        }
    }

    let start = earliest_start.map(String::from);
    let end = latest_end.map(String::from);
    compute_duration(&start, &end)
}

/// Detect interventions from provenance operations.
///
/// Looks for `retry`, `edit`, `reassign`, and `manual_override` operations
/// as indicators of human or system intervention during a run.
fn detect_interventions(ops: &[&OperationEntry]) -> Vec<InterventionSummary> {
    const INTERVENTION_OPS: &[&str] = &["retry", "edit", "reassign", "manual_override"];
    let mut interventions = Vec::new();

    for op in ops {
        if INTERVENTION_OPS.contains(&op.op.as_str()) {
            let description = if op.detail.is_null() {
                None
            } else {
                op.detail
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| {
                        op.detail
                            .get("detail")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .or_else(|| Some(op.detail.to_string()))
            };

            interventions.push(InterventionSummary {
                task_id: op.task_id.clone().unwrap_or_default(),
                kind: op.op.clone(),
                description,
                timestamp: op.timestamp.clone(),
            });
        }
    }

    interventions
}

// ---------------------------------------------------------------------------
// JSONL storage (existing integration)
// ---------------------------------------------------------------------------

/// Load run summaries for a function from its `.runs.jsonl` file.
///
/// Returns at most `max_runs` most recent summaries (the file is append-only,
/// so the last lines are the most recent).
pub fn load_run_summaries(
    workgraph_dir: &Path,
    function_id: &str,
    config: &TraceMemoryConfig,
) -> Vec<RunSummary> {
    let runs_path = if let Some(ref storage) = config.storage_path {
        workgraph_dir.join(storage)
    } else {
        workgraph_dir
            .join("functions")
            .join(format!("{}.runs.jsonl", function_id))
    };

    if !runs_path.exists() {
        return Vec::new();
    }

    let contents = match std::fs::read_to_string(&runs_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut summaries: Vec<RunSummary> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    // Keep only the most recent N
    let max = config.max_runs as usize;
    if summaries.len() > max {
        summaries = summaries.split_off(summaries.len() - max);
    }

    summaries
}

/// Return the path to the runs.jsonl file for a function.
pub fn runs_path(workgraph_dir: &Path, function_id: &str) -> PathBuf {
    workgraph_dir
        .join("functions")
        .join(format!("{}.runs.jsonl", function_id))
}

/// Append a run summary to the function's `.runs.jsonl` file.
pub fn append_run_summary(
    workgraph_dir: &Path,
    function_id: &str,
    summary: &RunSummary,
) -> Result<PathBuf, std::io::Error> {
    let path = runs_path(workgraph_dir, function_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(summary)
        .map_err(std::io::Error::other)?;
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{}", line)?;
    Ok(path)
}

/// Render run summaries into human-readable text for prompt injection.
pub fn render_run_summaries(summaries: &[RunSummary], inclusions: &MemoryInclusions) -> String {
    if summaries.is_empty() {
        return "No previous runs recorded.".to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!("=== {} Previous Run(s) ===", summaries.len()));

    for (i, summary) in summaries.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!("--- Run {} (prefix: {}) ---", i + 1, summary.prefix));
        lines.push(format!("  Instantiated: {}", summary.instantiated_at));

        if !summary.inputs.is_empty() {
            let input_strs: Vec<String> = summary
                .inputs
                .iter()
                .map(|(k, v)| {
                    let rendered = crate::trace_function::render_value(v);
                    let truncated = if rendered.len() > 80 {
                        format!("{}...", &rendered[..77])
                    } else {
                        rendered
                    };
                    format!("{}={}", k, truncated)
                })
                .collect();
            lines.push(format!("  Inputs: {}", input_strs.join(", ")));
        }

        if inclusions.outcomes {
            let succeeded = summary
                .task_outcomes
                .iter()
                .filter(|t| t.status == "Done")
                .count();
            let failed = summary
                .task_outcomes
                .iter()
                .filter(|t| t.status == "Failed")
                .count();
            let total = summary.task_outcomes.len();
            lines.push(format!(
                "  Outcome: {} ({}/{} succeeded, {} failed)",
                if summary.all_succeeded {
                    "SUCCESS"
                } else {
                    "PARTIAL"
                },
                succeeded,
                total,
                failed
            ));
        }

        if inclusions.scores
            && let Some(avg) = summary.avg_score {
                lines.push(format!("  Avg Score: {:.2}", avg));
            }

        if inclusions.duration
            && let Some(secs) = summary.wall_clock_secs {
                lines.push(format!(
                    "  Duration: {}",
                    crate::format_duration(secs, false)
                ));
            }

        if inclusions.interventions && !summary.interventions.is_empty() {
            lines.push(format!(
                "  Interventions: {}",
                summary.interventions.len()
            ));
            for intervention in &summary.interventions {
                let desc = intervention
                    .description
                    .as_deref()
                    .unwrap_or("(no description)");
                lines.push(format!(
                    "    - {} on {}: {}",
                    intervention.kind, intervention.task_id, desc
                ));
            }
        }

        if inclusions.retries {
            let total_retries: u32 = summary
                .task_outcomes
                .iter()
                .map(|t| t.retry_count)
                .sum();
            if total_retries > 0 {
                lines.push(format!("  Total Retries: {}", total_retries));
            }
        }

        if inclusions.artifacts {
            let task_ids: Vec<&str> = summary
                .task_outcomes
                .iter()
                .map(|t| t.task_id.as_str())
                .collect();
            if !task_ids.is_empty() {
                lines.push(format!("  Tasks: {}", task_ids.join(", ")));
            }
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_function::{
        InterventionSummary, MemoryInclusions, RunSummary, TaskOutcome, TraceMemoryConfig,
    };
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn sample_run_summary() -> RunSummary {
        RunSummary {
            instantiated_at: "2026-02-20T12:00:00Z".to_string(),
            inputs: {
                let mut m = HashMap::new();
                m.insert(
                    "feature_name".to_string(),
                    serde_yaml::Value::String("auth".to_string()),
                );
                m
            },
            prefix: "auth".to_string(),
            task_outcomes: vec![
                TaskOutcome {
                    template_id: "plan".to_string(),
                    task_id: "auth-plan".to_string(),
                    status: "Done".to_string(),
                    score: Some(0.9),
                    duration_secs: Some(60),
                    retry_count: 0,
                },
                TaskOutcome {
                    template_id: "implement".to_string(),
                    task_id: "auth-implement".to_string(),
                    status: "Done".to_string(),
                    score: Some(0.85),
                    duration_secs: Some(300),
                    retry_count: 1,
                },
            ],
            interventions: vec![InterventionSummary {
                task_id: "auth-implement".to_string(),
                kind: "manual-retry".to_string(),
                description: Some("Flaky test, retried".to_string()),
                timestamp: "2026-02-20T12:05:00Z".to_string(),
            }],
            wall_clock_secs: Some(360),
            all_succeeded: true,
            avg_score: Some(0.875),
        }
    }

    fn default_inclusions() -> MemoryInclusions {
        MemoryInclusions {
            outcomes: true,
            scores: true,
            interventions: true,
            duration: true,
            retries: false,
            artifacts: false,
        }
    }

    // ===================================================================
    // JSONL storage tests (existing)
    // ===================================================================

    #[test]
    fn load_empty_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let config = TraceMemoryConfig {
            max_runs: 10,
            include: default_inclusions(),
            storage_path: None,
        };
        let summaries = load_run_summaries(tmp.path(), "nonexistent", &config);
        assert!(summaries.is_empty());
    }

    #[test]
    fn load_and_parse_jsonl() {
        let tmp = TempDir::new().unwrap();
        let func_dir = tmp.path().join("functions");
        std::fs::create_dir_all(&func_dir).unwrap();

        let summary = sample_run_summary();
        let line = serde_json::to_string(&summary).unwrap();
        std::fs::write(
            func_dir.join("test-func.runs.jsonl"),
            format!("{}\n", line),
        )
        .unwrap();

        let config = TraceMemoryConfig {
            max_runs: 10,
            include: default_inclusions(),
            storage_path: None,
        };
        let summaries = load_run_summaries(tmp.path(), "test-func", &config);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].prefix, "auth");
        assert!(summaries[0].all_succeeded);
    }

    #[test]
    fn load_respects_max_runs() {
        let tmp = TempDir::new().unwrap();
        let func_dir = tmp.path().join("functions");
        std::fs::create_dir_all(&func_dir).unwrap();

        let summary = sample_run_summary();
        let line = serde_json::to_string(&summary).unwrap();
        let content: String = (0..5).map(|_| format!("{}\n", line)).collect();
        std::fs::write(func_dir.join("test-func.runs.jsonl"), content).unwrap();

        let config = TraceMemoryConfig {
            max_runs: 2,
            include: default_inclusions(),
            storage_path: None,
        };
        let summaries = load_run_summaries(tmp.path(), "test-func", &config);
        assert_eq!(summaries.len(), 2);
    }

    #[test]
    fn render_empty_summaries_jsonl() {
        let text = render_run_summaries(&[], &default_inclusions());
        assert_eq!(text, "No previous runs recorded.");
    }

    #[test]
    fn render_with_outcomes_and_scores() {
        let summaries = vec![sample_run_summary()];
        let text = render_run_summaries(&summaries, &default_inclusions());
        assert!(text.contains("1 Previous Run"));
        assert!(text.contains("SUCCESS"));
        assert!(text.contains("2/2 succeeded"));
        assert!(text.contains("0.88")); // avg score
        assert!(text.contains("6m")); // duration
        assert!(text.contains("manual-retry")); // intervention
    }

    #[test]
    fn render_with_retries() {
        let summaries = vec![sample_run_summary()];
        let inclusions = MemoryInclusions {
            retries: true,
            ..default_inclusions()
        };
        let text = render_run_summaries(&summaries, &inclusions);
        assert!(text.contains("Total Retries: 1"));
    }

    #[test]
    fn render_without_optional_fields() {
        let summaries = vec![sample_run_summary()];
        let inclusions = MemoryInclusions {
            outcomes: false,
            scores: false,
            interventions: false,
            duration: false,
            retries: false,
            artifacts: false,
        };
        let text = render_run_summaries(&summaries, &inclusions);
        assert!(!text.contains("SUCCESS"));
        assert!(!text.contains("Avg Score"));
        assert!(!text.contains("Duration"));
        assert!(!text.contains("Interventions"));
    }

    #[test]
    fn load_with_custom_storage_path() {
        let tmp = TempDir::new().unwrap();
        let custom_dir = tmp.path().join("custom");
        std::fs::create_dir_all(&custom_dir).unwrap();

        let summary = sample_run_summary();
        let line = serde_json::to_string(&summary).unwrap();
        std::fs::write(
            tmp.path().join("custom").join("runs.jsonl"),
            format!("{}\n", line),
        )
        .unwrap();

        let config = TraceMemoryConfig {
            max_runs: 10,
            include: default_inclusions(),
            storage_path: Some("custom/runs.jsonl".to_string()),
        };
        let summaries = load_run_summaries(tmp.path(), "ignored", &config);
        assert_eq!(summaries.len(), 1);
    }

    #[test]
    fn append_and_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let summary = sample_run_summary();
        append_run_summary(tmp.path(), "my-func", &summary).unwrap();

        let config = TraceMemoryConfig {
            max_runs: 10,
            include: default_inclusions(),
            storage_path: None,
        };
        let loaded = load_run_summaries(tmp.path(), "my-func", &config);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].prefix, "auth");
    }

    // ===================================================================
    // Per-run JSON storage tests (spec §3.4 / §4.3)
    // ===================================================================

    #[test]
    fn save_creates_json_file() {
        let dir = TempDir::new().unwrap();
        let summary = sample_run_summary();
        let path = save_run_summary("deploy-prod", &summary, dir.path()).unwrap();

        assert!(path.exists());
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "2026-02-20T12-00-00Z.json"
        );

        // Verify it's valid JSON and round-trips
        let contents = fs::read_to_string(&path).unwrap();
        let loaded: RunSummary = serde_json::from_str(&contents).unwrap();
        assert_eq!(loaded.prefix, "auth");
        assert!(loaded.all_succeeded);
        assert_eq!(loaded.task_outcomes.len(), 2);
    }

    #[test]
    fn save_creates_memory_directory() {
        let dir = TempDir::new().unwrap();
        let summary = sample_run_summary();
        save_run_summary("my-func", &summary, dir.path()).unwrap();

        let mem_dir = dir.path().join("functions").join("my-func.memory");
        assert!(mem_dir.is_dir());
    }

    #[test]
    fn load_recent_returns_empty_for_nonexistent() {
        let result =
            load_recent_summaries("nonexistent", 10, Path::new("/tmp/no-such-wg")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn load_recent_sorted_newest_first() {
        let dir = TempDir::new().unwrap();

        let mut s1 = sample_run_summary();
        s1.instantiated_at = "2026-02-18T10:00:00Z".to_string();
        save_run_summary("func-a", &s1, dir.path()).unwrap();

        let mut s2 = sample_run_summary();
        s2.instantiated_at = "2026-02-20T12:00:00Z".to_string();
        save_run_summary("func-a", &s2, dir.path()).unwrap();

        let mut s3 = sample_run_summary();
        s3.instantiated_at = "2026-02-19T08:00:00Z".to_string();
        save_run_summary("func-a", &s3, dir.path()).unwrap();

        let loaded = load_recent_summaries("func-a", 10, dir.path()).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].instantiated_at, "2026-02-20T12:00:00Z");
        assert_eq!(loaded[1].instantiated_at, "2026-02-19T08:00:00Z");
        assert_eq!(loaded[2].instantiated_at, "2026-02-18T10:00:00Z");
    }

    #[test]
    fn load_recent_respects_max_runs() {
        let dir = TempDir::new().unwrap();
        for i in 0..5u32 {
            let mut s = sample_run_summary();
            s.instantiated_at = format!("2026-02-{:02}T12:00:00Z", 15 + i);
            save_run_summary("func-b", &s, dir.path()).unwrap();
        }

        let loaded = load_recent_summaries("func-b", 3, dir.path()).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].instantiated_at, "2026-02-19T12:00:00Z");
    }

    #[test]
    fn save_load_recent_round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample_run_summary();
        save_run_summary("func-rt", &original, dir.path()).unwrap();

        let loaded = load_recent_summaries("func-rt", 10, dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);

        let s = &loaded[0];
        assert_eq!(s.instantiated_at, original.instantiated_at);
        assert_eq!(s.prefix, original.prefix);
        assert_eq!(s.all_succeeded, original.all_succeeded);
        assert_eq!(s.avg_score, original.avg_score);
        assert_eq!(s.wall_clock_secs, original.wall_clock_secs);
        assert_eq!(s.task_outcomes.len(), original.task_outcomes.len());
        assert_eq!(s.interventions.len(), original.interventions.len());
    }

    // ===================================================================
    // build_run_summary tests
    // ===================================================================

    #[test]
    fn build_summary_from_graph() {
        use crate::graph::{Node, Status, Task};

        let mut graph = WorkGraph::new();

        graph.add_node(Node::Task(Task {
            id: "pfx/build".to_string(),
            title: "Build".to_string(),
            status: Status::Done,
            started_at: Some("2026-02-20T12:00:00Z".to_string()),
            completed_at: Some("2026-02-20T12:02:00Z".to_string()),
            retry_count: 0,
            ..Task::default()
        }));

        graph.add_node(Node::Task(Task {
            id: "pfx/test".to_string(),
            title: "Test".to_string(),
            status: Status::Failed,
            started_at: Some("2026-02-20T12:02:00Z".to_string()),
            completed_at: Some("2026-02-20T12:03:00Z".to_string()),
            retry_count: 1,
            ..Task::default()
        }));

        let tmp = TempDir::new().unwrap();
        let eval_dir = tmp.path().join("evaluations");
        fs::create_dir_all(&eval_dir).unwrap();

        let task_ids = vec!["pfx/build".to_string(), "pfx/test".to_string()];
        let summary = build_run_summary(
            &task_ids,
            &graph,
            &eval_dir,
            tmp.path(),
            "2026-02-20T12:00:00Z",
            "pfx/",
        )
        .unwrap();

        assert_eq!(summary.instantiated_at, "2026-02-20T12:00:00Z");
        assert_eq!(summary.prefix, "pfx/");
        assert!(!summary.all_succeeded);
        assert_eq!(summary.task_outcomes.len(), 2);

        let build_outcome = &summary.task_outcomes[0];
        assert_eq!(build_outcome.template_id, "build");
        assert_eq!(build_outcome.status, "Done");
        assert_eq!(build_outcome.duration_secs, Some(120));

        let test_outcome = &summary.task_outcomes[1];
        assert_eq!(test_outcome.template_id, "test");
        assert_eq!(test_outcome.status, "Failed");
        assert_eq!(test_outcome.retry_count, 1);
        assert_eq!(test_outcome.duration_secs, Some(60));

        // Wall clock: 12:00:00 to 12:03:00 = 180s
        assert_eq!(summary.wall_clock_secs, Some(180));
        assert!(summary.avg_score.is_none()); // No evaluations loaded
    }

    #[test]
    fn build_summary_with_evaluations() {
        use crate::graph::{Node, Status, Task};

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "pfx/build".to_string(),
            title: "Build".to_string(),
            status: Status::Done,
            started_at: Some("2026-02-20T12:00:00Z".to_string()),
            completed_at: Some("2026-02-20T12:02:00Z".to_string()),
            ..Task::default()
        }));

        let tmp = TempDir::new().unwrap();
        let eval_dir = tmp.path().join("evaluations");
        fs::create_dir_all(&eval_dir).unwrap();

        let eval = serde_json::json!({
            "id": "eval-001",
            "task_id": "pfx/build",
            "agent_id": "agent-1",
            "role_id": "role-1",
            "motivation_id": "mot-1",
            "score": 0.9,
            "notes": "Good work",
            "evaluator": "auto",
            "timestamp": "2026-02-20T12:05:00Z",
            "source": "llm"
        });
        fs::write(
            eval_dir.join("eval-001.json"),
            serde_json::to_string(&eval).unwrap(),
        )
        .unwrap();

        let task_ids = vec!["pfx/build".to_string()];
        let summary = build_run_summary(
            &task_ids,
            &graph,
            &eval_dir,
            tmp.path(),
            "2026-02-20T12:00:00Z",
            "pfx/",
        )
        .unwrap();

        assert!(summary.all_succeeded);
        assert_eq!(summary.task_outcomes[0].score, Some(0.9));
        assert_eq!(summary.avg_score, Some(0.9));
    }

    #[test]
    fn build_summary_skips_missing_tasks() {
        let graph = WorkGraph::new();
        let tmp = TempDir::new().unwrap();
        let eval_dir = tmp.path().join("evaluations");
        fs::create_dir_all(&eval_dir).unwrap();

        let task_ids = vec!["nonexistent/task".to_string()];
        let summary = build_run_summary(
            &task_ids,
            &graph,
            &eval_dir,
            tmp.path(),
            "2026-02-20T12:00:00Z",
            "nonexistent/",
        )
        .unwrap();

        assert!(summary.task_outcomes.is_empty());
        assert!(summary.all_succeeded); // vacuously true
    }

    // ===================================================================
    // detect_interventions tests
    // ===================================================================

    #[test]
    fn detect_interventions_from_ops() {
        let ops = vec![
            OperationEntry {
                timestamp: "2026-02-20T12:01:00Z".to_string(),
                op: "done".to_string(),
                task_id: Some("pfx/build".to_string()),
                actor: Some("agent".to_string()),
                detail: serde_json::Value::Null,
            },
            OperationEntry {
                timestamp: "2026-02-20T12:02:00Z".to_string(),
                op: "retry".to_string(),
                task_id: Some("pfx/test".to_string()),
                actor: Some("human".to_string()),
                detail: serde_json::json!({"reason": "Flaky test"}),
            },
            OperationEntry {
                timestamp: "2026-02-20T12:03:00Z".to_string(),
                op: "edit".to_string(),
                task_id: Some("pfx/test".to_string()),
                actor: Some("human".to_string()),
                detail: serde_json::json!({"detail": "Updated description"}),
            },
        ];

        let op_refs: Vec<&OperationEntry> = ops.iter().collect();
        let interventions = detect_interventions(&op_refs);

        assert_eq!(interventions.len(), 2);
        assert_eq!(interventions[0].kind, "retry");
        assert_eq!(
            interventions[0].description,
            Some("Flaky test".to_string())
        );
        assert_eq!(interventions[1].kind, "edit");
        assert_eq!(
            interventions[1].description,
            Some("Updated description".to_string())
        );
    }

    #[test]
    fn detect_interventions_ignores_normal_ops() {
        let ops = vec![
            OperationEntry {
                timestamp: "2026-02-20T12:01:00Z".to_string(),
                op: "add_task".to_string(),
                task_id: Some("task-1".to_string()),
                actor: None,
                detail: serde_json::Value::Null,
            },
            OperationEntry {
                timestamp: "2026-02-20T12:02:00Z".to_string(),
                op: "done".to_string(),
                task_id: Some("task-1".to_string()),
                actor: None,
                detail: serde_json::Value::Null,
            },
        ];

        let op_refs: Vec<&OperationEntry> = ops.iter().collect();
        let interventions = detect_interventions(&op_refs);
        assert!(interventions.is_empty());
    }

    #[test]
    fn detect_interventions_null_detail() {
        let ops = vec![OperationEntry {
            timestamp: "2026-02-20T12:01:00Z".to_string(),
            op: "retry".to_string(),
            task_id: Some("task-1".to_string()),
            actor: None,
            detail: serde_json::Value::Null,
        }];

        let op_refs: Vec<&OperationEntry> = ops.iter().collect();
        let interventions = detect_interventions(&op_refs);
        assert_eq!(interventions.len(), 1);
        assert!(interventions[0].description.is_none());
    }

    // ===================================================================
    // render_summaries_text tests
    // ===================================================================

    #[test]
    fn render_text_empty_summaries() {
        let text = render_summaries_text(&[]);
        assert_eq!(text, "No previous runs recorded.");
    }

    #[test]
    fn render_text_single_summary() {
        let summary = sample_run_summary();
        let text = render_summaries_text(&[summary]);

        assert!(text.contains("Past Runs (1 total)"));
        assert!(text.contains("Run 1"));
        assert!(text.contains("2026-02-20T12:00:00Z"));
        assert!(text.contains("[SUCCESS]"));
        assert!(text.contains("Avg score: 0.88"));
        assert!(text.contains("Wall clock: 6m"));
        assert!(text.contains("plan (Done)"));
        assert!(text.contains("implement (Done)"));
        assert!(text.contains("score=0.85"));
        assert!(text.contains("retries=1"));
        assert!(text.contains("manual-retry on auth-implement: Flaky test"));
    }

    #[test]
    fn render_text_multiple_summaries() {
        let s1 = sample_run_summary();
        let mut s2 = sample_run_summary();
        s2.instantiated_at = "2026-02-19T08:00:00Z".to_string();
        s2.all_succeeded = false;
        s2.task_outcomes[1].status = "Failed".to_string();

        let text = render_summaries_text(&[s1, s2]);
        assert!(text.contains("Past Runs (2 total)"));
        assert!(text.contains("Run 1"));
        assert!(text.contains("Run 2"));
        assert!(text.contains("[SUCCESS]"));
        assert!(text.contains("[ISSUES]"));
    }

    #[test]
    fn render_text_without_optional_fields() {
        let summary = RunSummary {
            instantiated_at: "2026-02-20T12:00:00Z".to_string(),
            inputs: HashMap::new(),
            prefix: "test/".to_string(),
            task_outcomes: vec![TaskOutcome {
                template_id: "build".to_string(),
                task_id: "test/build".to_string(),
                status: "Done".to_string(),
                score: None,
                duration_secs: None,
                retry_count: 0,
            }],
            interventions: vec![],
            wall_clock_secs: None,
            all_succeeded: true,
            avg_score: None,
        };

        let text = render_summaries_text(&[summary]);
        assert!(text.contains("[SUCCESS]"));
        assert!(text.contains("build (Done)"));
        assert!(!text.contains("score="));
        assert!(!text.contains("duration="));
        assert!(!text.contains("Avg score"));
        assert!(!text.contains("Wall clock"));
        assert!(!text.contains("Interventions"));
    }

    // ===================================================================
    // compute_duration tests
    // ===================================================================

    #[test]
    fn compute_duration_valid() {
        let start = Some("2026-02-20T12:00:00Z".to_string());
        let end = Some("2026-02-20T12:02:30Z".to_string());
        assert_eq!(compute_duration(&start, &end), Some(150));
    }

    #[test]
    fn compute_duration_none_start() {
        let end = Some("2026-02-20T12:00:00Z".to_string());
        assert_eq!(compute_duration(&None, &end), None);
    }

    #[test]
    fn compute_duration_none_end() {
        let start = Some("2026-02-20T12:00:00Z".to_string());
        assert_eq!(compute_duration(&start, &None), None);
    }

    #[test]
    fn compute_duration_both_none() {
        assert_eq!(compute_duration(&None, &None), None);
    }

    // ===================================================================
    // memory_dir tests
    // ===================================================================

    #[test]
    fn memory_dir_path() {
        let wg = Path::new("/tmp/.workgraph");
        let dir = memory_dir(wg, "deploy-prod");
        assert_eq!(
            dir,
            PathBuf::from("/tmp/.workgraph/functions/deploy-prod.memory")
        );
    }
}
