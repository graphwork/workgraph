//! Compactor: distills workgraph state into a 3-layer context artifact.
//!
//! Produces `.workgraph/compactor/context.md` with:
//! - Rolling Narrative (~2000 tokens)
//! - Persistent Facts (~500 tokens)
//! - Evaluation Digest (~500 tokens)
//!
//! Triggered by: coordinator tick interval, ops growth threshold, restart,
//! or manual `wg compact`.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{Config, DispatchRole};
use crate::graph::{Status, WorkGraph};
use crate::parser::load_graph;
use crate::provenance;

/// Directory for compactor artifacts
fn compactor_dir(workgraph_dir: &Path) -> PathBuf {
    workgraph_dir.join("compactor")
}

/// Path to the generated context.md
pub fn context_md_path(workgraph_dir: &Path) -> PathBuf {
    compactor_dir(workgraph_dir).join("context.md")
}

/// Path to the compactor state file (tracks last compaction metadata)
fn state_path(workgraph_dir: &Path) -> PathBuf {
    compactor_dir(workgraph_dir).join("state.json")
}

/// Persistent compactor state
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct CompactorState {
    pub last_compaction: Option<String>,
    pub last_ops_count: usize,
    pub last_tick: u64,
    pub compaction_count: u64,
}

impl CompactorState {
    pub fn load(workgraph_dir: &Path) -> Self {
        let path = state_path(workgraph_dir);
        if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self, workgraph_dir: &Path) -> Result<()> {
        let dir = compactor_dir(workgraph_dir);
        fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)?;
        fs::write(state_path(workgraph_dir), json)?;
        Ok(())
    }
}

/// Check whether compaction should run based on coordinator tick count and ops growth.
pub fn should_compact(
    workgraph_dir: &Path,
    current_tick: u64,
    config: &Config,
) -> bool {
    let interval = config.coordinator.compactor_interval;
    if interval == 0 {
        return false;
    }

    let state = CompactorState::load(workgraph_dir);

    // Check tick interval
    if current_tick.saturating_sub(state.last_tick) >= u64::from(interval) {
        return true;
    }

    // Check ops growth threshold
    let ops_threshold = config.coordinator.compactor_ops_threshold;
    if ops_threshold > 0 {
        let current_ops = count_ops(workgraph_dir);
        if current_ops.saturating_sub(state.last_ops_count) >= ops_threshold {
            return true;
        }
    }

    false
}

/// Count total provenance operations (fast: just counts lines in operations.jsonl)
fn count_ops(workgraph_dir: &Path) -> usize {
    let path = provenance::operations_path(workgraph_dir);
    if !path.exists() {
        return 0;
    }
    match fs::read_to_string(&path) {
        Ok(content) => content.lines().filter(|l| !l.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

/// Run compaction: gather graph state, call LLM, write context.md.
///
/// This is the main entry point for both `wg compact` and coordinator-triggered compaction.
pub fn run_compaction(workgraph_dir: &Path, current_tick: u64) -> Result<PathBuf> {
    let config = Config::load_or_default(workgraph_dir);
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&graph_path).context("Failed to load graph for compaction")?;

    // Gather the input data for the LLM
    let snapshot = build_graph_snapshot(&graph, workgraph_dir);

    // Build the prompt
    let prompt = build_compactor_prompt(&snapshot);

    // Call the LLM
    let result = super::llm::run_lightweight_llm_call(
        &config,
        DispatchRole::Compactor,
        &prompt,
        120, // 2 minute timeout
    )
    .context("Compactor LLM call failed")?;

    // Write context.md
    let output_path = context_md_path(workgraph_dir);
    let dir = compactor_dir(workgraph_dir);
    fs::create_dir_all(&dir)?;
    fs::write(&output_path, &result.text)?;

    // Update compactor state
    let mut state = CompactorState::load(workgraph_dir);
    state.last_compaction = Some(Utc::now().to_rfc3339());
    state.last_ops_count = count_ops(workgraph_dir);
    state.last_tick = current_tick;
    state.compaction_count += 1;
    state.save(workgraph_dir)?;

    Ok(output_path)
}

/// Snapshot of graph state for the compactor prompt.
struct GraphSnapshot {
    total_tasks: usize,
    status_counts: StatusCounts,
    recent_completions: Vec<TaskSummary>,
    active_tasks: Vec<TaskSummary>,
    failed_tasks: Vec<TaskSummary>,
    blocked_tasks: Vec<TaskSummary>,
    recent_logs: Vec<String>,
    evaluation_digest: String,
}

struct StatusCounts {
    open: usize,
    in_progress: usize,
    done: usize,
    failed: usize,
    blocked: usize,
    abandoned: usize,
    waiting: usize,
}

struct TaskSummary {
    id: String,
    title: String,
    assigned: Option<String>,
}

fn build_graph_snapshot(graph: &WorkGraph, workgraph_dir: &Path) -> GraphSnapshot {
    let mut counts = StatusCounts {
        open: 0,
        in_progress: 0,
        done: 0,
        failed: 0,
        blocked: 0,
        abandoned: 0,
        waiting: 0,
    };

    let mut recent_completions = Vec::new();
    let mut active_tasks = Vec::new();
    let mut failed_tasks = Vec::new();
    let mut blocked_tasks = Vec::new();
    let mut recent_logs = Vec::new();

    for task in graph.tasks() {
        match task.status {
            Status::Open => counts.open += 1,
            Status::InProgress => counts.in_progress += 1,
            Status::Done => counts.done += 1,
            Status::Failed => counts.failed += 1,
            Status::Blocked => counts.blocked += 1,
            Status::Abandoned => counts.abandoned += 1,
            Status::Waiting => counts.waiting += 1,
        }

        let summary = TaskSummary {
            id: task.id.clone(),
            title: task.title.clone(),
            assigned: task.assigned.clone(),
        };

        match task.status {
            Status::Done => {
                recent_completions.push(summary);
            }
            Status::InProgress => {
                active_tasks.push(summary);
            }
            Status::Failed => {
                failed_tasks.push(summary);
            }
            Status::Blocked => {
                blocked_tasks.push(summary);
            }
            _ => {}
        }

        // Collect recent log entries (last 2 per task, up to 30 total)
        if recent_logs.len() < 30 {
            for entry in task.log.iter().rev().take(2) {
                recent_logs.push(format!(
                    "[{}] {}: {}",
                    entry.timestamp.chars().take(19).collect::<String>(),
                    task.id,
                    entry.message.chars().take(120).collect::<String>()
                ));
            }
        }
    }

    // Cap lists to keep prompt manageable
    recent_completions.truncate(20);
    failed_tasks.truncate(10);
    blocked_tasks.truncate(10);
    recent_logs.truncate(30);
    recent_logs.sort(); // chronological order

    let evaluation_digest = build_evaluation_digest(workgraph_dir);

    GraphSnapshot {
        total_tasks: graph.tasks().count(),
        status_counts: counts,
        recent_completions,
        active_tasks,
        failed_tasks,
        blocked_tasks,
        recent_logs,
        evaluation_digest,
    }
}

fn build_evaluation_digest(workgraph_dir: &Path) -> String {
    let eval_dir = workgraph_dir.join("agency").join("evaluations");
    if !eval_dir.exists() {
        return "No evaluations recorded yet.".to_string();
    }

    let mut entries = Vec::new();
    if let Ok(dir) = fs::read_dir(&eval_dir) {
        for entry in dir.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                        let task = val.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
                        let score = val.get("score").and_then(|v| v.as_f64());
                        let verdict = val.get("verdict").and_then(|v| v.as_str());
                        let line = match (score, verdict) {
                            (Some(s), Some(v)) => format!("- {}: score={:.1}, verdict={}", task, s, v),
                            (Some(s), None) => format!("- {}: score={:.1}", task, s),
                            _ => format!("- {}: evaluated", task),
                        };
                        entries.push(line);
                    }
                }
            }
        }
    }

    if entries.is_empty() {
        "No evaluations recorded yet.".to_string()
    } else {
        entries.truncate(15);
        entries.join("\n")
    }
}

fn build_compactor_prompt(snapshot: &GraphSnapshot) -> String {
    let c = &snapshot.status_counts;

    let mut prompt = format!(
        "You are a workgraph compactor. Distill the following project state into a context document \
         with EXACTLY three sections. Stay within the token budgets.\n\n\
         ## Input: Current Graph State\n\n\
         Total tasks: {}\n\
         - Open: {}, In-progress: {}, Done: {}, Failed: {}, Blocked: {}, Abandoned: {}, Waiting: {}\n\n",
        snapshot.total_tasks,
        c.open, c.in_progress, c.done, c.failed, c.blocked, c.abandoned, c.waiting,
    );

    if !snapshot.active_tasks.is_empty() {
        prompt.push_str("### Active Tasks\n");
        for t in &snapshot.active_tasks {
            prompt.push_str(&format!(
                "- [{}] {} ({})\n",
                t.id,
                t.title,
                t.assigned.as_deref().unwrap_or("unassigned")
            ));
        }
        prompt.push('\n');
    }

    if !snapshot.recent_completions.is_empty() {
        prompt.push_str("### Recently Completed\n");
        for t in &snapshot.recent_completions {
            prompt.push_str(&format!("- [{}] {}\n", t.id, t.title));
        }
        prompt.push('\n');
    }

    if !snapshot.failed_tasks.is_empty() {
        prompt.push_str("### Failed Tasks\n");
        for t in &snapshot.failed_tasks {
            prompt.push_str(&format!("- [{}] {}\n", t.id, t.title));
        }
        prompt.push('\n');
    }

    if !snapshot.blocked_tasks.is_empty() {
        prompt.push_str("### Blocked Tasks\n");
        for t in &snapshot.blocked_tasks {
            prompt.push_str(&format!("- [{}] {}\n", t.id, t.title));
        }
        prompt.push('\n');
    }

    if !snapshot.recent_logs.is_empty() {
        prompt.push_str("### Recent Activity Log\n");
        for log in &snapshot.recent_logs {
            prompt.push_str(&format!("{}\n", log));
        }
        prompt.push('\n');
    }

    if !snapshot.evaluation_digest.is_empty() {
        prompt.push_str("### Evaluation Results\n");
        prompt.push_str(&snapshot.evaluation_digest);
        prompt.push_str("\n\n");
    }

    prompt.push_str(
        "## Output Format\n\n\
         Produce a markdown document with EXACTLY these three sections:\n\n\
         ### 1. Rolling Narrative (~2000 tokens)\n\
         A coherent narrative of what the project has accomplished, what is currently happening, \
         and what remains. Focus on the story arc: what problems were identified, how they were \
         addressed, what patterns emerged. Write in present/past tense. Include task IDs where relevant.\n\n\
         ### 2. Persistent Facts (~500 tokens)\n\
         Bullet list of stable facts about the project: architecture decisions, conventions adopted, \
         key file paths, integration points, recurring patterns. These facts should remain useful \
         across many sessions.\n\n\
         ### 3. Evaluation Digest (~500 tokens)\n\
         Summary of evaluation outcomes: which tasks scored well, which struggled, any patterns \
         in agent performance. If no evaluations exist, note that and suggest what to evaluate.\n\n\
         IMPORTANT: Output ONLY the context document. No preamble, no explanation. \
         Start directly with '# Project Context'."
    );

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compactor_state_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("compactor")).unwrap();

        let state = CompactorState {
            last_compaction: Some("2026-01-01T00:00:00Z".to_string()),
            last_ops_count: 42,
            last_tick: 5,
            compaction_count: 3,
        };
        state.save(dir).unwrap();

        let loaded = CompactorState::load(dir);
        assert_eq!(loaded.last_ops_count, 42);
        assert_eq!(loaded.last_tick, 5);
        assert_eq!(loaded.compaction_count, 3);
    }

    #[test]
    fn test_compactor_state_default_on_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = CompactorState::load(tmp.path());
        assert_eq!(state.last_ops_count, 0);
        assert_eq!(state.last_tick, 0);
        assert!(state.last_compaction.is_none());
    }

    #[test]
    fn test_should_compact_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.coordinator.compactor_interval = 0;
        assert!(!should_compact(tmp.path(), 100, &config));
    }

    #[test]
    fn test_should_compact_by_tick_interval() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("compactor")).unwrap();

        let mut config = Config::default();
        config.coordinator.compactor_interval = 5;

        // No state yet, tick 0 vs last_tick 0 → diff 0, not enough
        assert!(!should_compact(dir, 0, &config));

        // tick 5 vs last_tick 0 → diff 5 >= interval 5
        assert!(should_compact(dir, 5, &config));

        // Save state at tick 5
        let state = CompactorState {
            last_tick: 5,
            ..Default::default()
        };
        state.save(dir).unwrap();

        // tick 9 vs last_tick 5 → diff 4 < 5
        assert!(!should_compact(dir, 9, &config));

        // tick 10 vs last_tick 5 → diff 5 >= 5
        assert!(should_compact(dir, 10, &config));
    }

    #[test]
    fn test_should_compact_by_ops_growth() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("compactor")).unwrap();
        fs::create_dir_all(dir.join("log")).unwrap();

        let mut config = Config::default();
        config.coordinator.compactor_interval = 1000; // high tick interval so we only test ops
        config.coordinator.compactor_ops_threshold = 3;

        // Save state with 0 ops at tick 0
        let state = CompactorState {
            last_tick: 0,
            last_ops_count: 0,
            ..Default::default()
        };
        state.save(dir).unwrap();

        // Write 3 ops lines
        let ops_path = dir.join("log").join("operations.jsonl");
        fs::write(&ops_path, "{}\n{}\n{}\n").unwrap();

        // tick 1, diff from last_tick = 1 < 1000, but ops grew by 3 >= threshold 3
        assert!(should_compact(dir, 1, &config));
    }

    #[test]
    fn test_count_ops_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(count_ops(tmp.path()), 0);
    }

    #[test]
    fn test_count_ops_with_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("log")).unwrap();
        fs::write(
            dir.join("log").join("operations.jsonl"),
            "{\"op\":\"add\"}\n{\"op\":\"done\"}\n\n{\"op\":\"fail\"}\n",
        )
        .unwrap();
        assert_eq!(count_ops(dir), 3);
    }

    #[test]
    fn test_build_compactor_prompt_contains_sections() {
        let snapshot = GraphSnapshot {
            total_tasks: 10,
            status_counts: StatusCounts {
                open: 2,
                in_progress: 3,
                done: 4,
                failed: 1,
                blocked: 0,
                abandoned: 0,
                waiting: 0,
            },
            recent_completions: vec![TaskSummary {
                id: "task-1".into(),
                title: "First task".into(),
                assigned: None,
            }],
            active_tasks: vec![TaskSummary {
                id: "task-2".into(),
                title: "Active task".into(),
                assigned: Some("agent-1".into()),
            }],
            failed_tasks: vec![],
            blocked_tasks: vec![],
            recent_logs: vec!["[2026-01-01T00:00:00] task-1: completed work".into()],
            evaluation_digest: "No evaluations recorded yet.".into(),
        };

        let prompt = build_compactor_prompt(&snapshot);
        assert!(prompt.contains("Total tasks: 10"));
        assert!(prompt.contains("Rolling Narrative"));
        assert!(prompt.contains("Persistent Facts"));
        assert!(prompt.contains("Evaluation Digest"));
        assert!(prompt.contains("[task-1] First task"));
        assert!(prompt.contains("[task-2] Active task"));
    }

    #[test]
    fn test_build_evaluation_digest_no_evals() {
        let tmp = tempfile::TempDir::new().unwrap();
        let digest = build_evaluation_digest(tmp.path());
        assert_eq!(digest, "No evaluations recorded yet.");
    }

    #[test]
    fn test_build_evaluation_digest_with_evals() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let eval_dir = dir.join("agency").join("evaluations");
        fs::create_dir_all(&eval_dir).unwrap();

        let eval = serde_json::json!({
            "task_id": "my-task",
            "score": 8.5,
            "verdict": "pass"
        });
        fs::write(
            eval_dir.join("eval-1.json"),
            serde_json::to_string(&eval).unwrap(),
        )
        .unwrap();

        let digest = build_evaluation_digest(dir);
        assert!(digest.contains("my-task"));
        assert!(digest.contains("8.5"));
        assert!(digest.contains("pass"));
    }

    #[test]
    fn test_context_md_path() {
        let path = context_md_path(Path::new("/tmp/wg"));
        assert_eq!(path, PathBuf::from("/tmp/wg/compactor/context.md"));
    }
}
