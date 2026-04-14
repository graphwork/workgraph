//! Compactor: distills workgraph state into a 3-layer context artifact.
//!
//! Produces `.workgraph/compactor/context.md` with:
//! - Rolling Narrative (scaled to model context window)
//! - Persistent Facts (scaled to model context window)
//! - Evaluation Digest (scaled to model context window)
//!
//! Triggered by: graph lifecycle (`.compact-0` becomes ready when coordinator
//! marks done and cycle edge reactivates it), or manual `wg compact`.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{Config, DispatchRole};
use crate::graph::{Status, TokenUsage, WorkGraph};
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
    /// Duration of the last compaction LLM call in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_duration_ms: Option<u64>,
    /// Token usage from the last compaction LLM call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_tokens: Option<TokenUsage>,
    /// Byte size of context.md written during the last compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_context_bytes: Option<u64>,
    /// Number of consecutive compaction errors (persisted across daemon restarts).
    #[serde(default)]
    pub error_count: u64,
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
///
/// **Deprecated:** Compaction is now cycle-driven — it fires when `.compact-0` is
/// graph-ready (Open + all deps terminal). The daemon no longer calls this function.
/// Kept for backward compatibility; `compactor_interval` and `compactor_ops_threshold`
/// config values are no-ops in the new model.
pub fn should_compact(workgraph_dir: &Path, current_tick: u64, config: &Config) -> bool {
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

/// Resolve the context window size for the compactor's model.
///
/// Resolution order: model registry entry → endpoint config → 200k default.
fn resolve_compactor_context_window(config: &Config) -> u64 {
    let resolved = config.resolve_model_for_role(DispatchRole::Compactor);
    if let Some(ref entry) = resolved.registry_entry {
        if entry.context_window > 0 {
            return entry.context_window;
        }
    }
    if let Some(ref ep_name) = resolved.endpoint {
        if let Some(ep) = config.llm_endpoints.find_by_name(ep_name) {
            if let Some(cw) = ep.context_window {
                return cw;
            }
        }
    }
    200_000
}

/// Compute section token budgets for the compactor prompt based on the context window.
///
/// Uses 3% of the context window, clamped to [500, 6000], split 67/17/17 across
/// narrative / facts / evaluation sections.
fn compactor_section_budgets(context_window: u64) -> (u64, u64, u64) {
    let total_budget = (context_window as f64 * 0.03).round() as u64;
    let total_budget = total_budget.clamp(500, 6000);
    let narrative = (total_budget as f64 * 0.67).round() as u64;
    let facts = (total_budget as f64 * 0.17).round() as u64;
    let evaluation = total_budget.saturating_sub(narrative).saturating_sub(facts);
    (narrative.max(300), facts.max(100), evaluation.max(100))
}

/// Run compaction: gather graph state, call LLM, write context.md.
///
/// This is the main entry point for both `wg compact` and coordinator-triggered compaction.
/// Compaction is now cycle-driven — the daemon calls this only when `.compact-0` is
/// graph-ready. The old timer/ops gating has been removed from the call path.
pub fn run_compaction(workgraph_dir: &Path) -> Result<PathBuf> {
    let config = Config::load_or_default(workgraph_dir);
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&graph_path).context("Failed to load graph for compaction")?;

    // Gather the input data for the LLM
    let snapshot = build_graph_snapshot(&graph, workgraph_dir);

    // Resolve context window for dynamic budget scaling
    let context_window = resolve_compactor_context_window(&config);

    // Build the prompt
    let prompt = build_compactor_prompt(&snapshot, context_window);

    // Track duration of the LLM call
    let call_start = std::time::Instant::now();

    // Call the LLM
    let result = super::llm::run_lightweight_llm_call(
        &config,
        DispatchRole::Compactor,
        &prompt,
        120, // 2 minute timeout
    )
    .context("Compactor LLM call failed")?;

    let duration_ms = call_start.elapsed().as_millis() as u64;

    // Write context.md
    let output_path = context_md_path(workgraph_dir);
    let dir = compactor_dir(workgraph_dir);
    fs::create_dir_all(&dir)?;
    fs::write(&output_path, &result.text)?;
    let context_bytes = result.text.len() as u64;

    // Update compactor state with metrics
    let mut state = CompactorState::load(workgraph_dir);
    state.last_compaction = Some(Utc::now().to_rfc3339());
    state.last_ops_count = count_ops(workgraph_dir);
    state.compaction_count += 1;
    state.last_compaction_duration_ms = Some(duration_ms);
    state.last_compaction_tokens = result.token_usage;
    state.last_compaction_context_bytes = Some(context_bytes);
    state.error_count = 0;
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
            Status::PendingValidation => counts.waiting += 1,
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
            if entry.path().extension().is_some_and(|e| e == "json")
                && let Ok(content) = fs::read_to_string(entry.path())
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
            {
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

    if entries.is_empty() {
        "No evaluations recorded yet.".to_string()
    } else {
        entries.truncate(15);
        entries.join("\n")
    }
}

fn build_compactor_prompt(snapshot: &GraphSnapshot, context_window: u64) -> String {
    let c = &snapshot.status_counts;

    let mut prompt = format!(
        "You are a workgraph compactor. Distill the following project state into a context document \
         with EXACTLY three sections. Stay within the token budgets.\n\n\
         ## Input: Current Graph State\n\n\
         Total tasks: {}\n\
         - Open: {}, In-progress: {}, Done: {}, Failed: {}, Blocked: {}, Abandoned: {}, Waiting: {}\n\n",
        snapshot.total_tasks,
        c.open,
        c.in_progress,
        c.done,
        c.failed,
        c.blocked,
        c.abandoned,
        c.waiting,
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

    let (narrative_budget, facts_budget, eval_budget) = compactor_section_budgets(context_window);

    prompt.push_str(&format!(
        "## Output Format\n\n\
         Produce a markdown document with EXACTLY these three sections:\n\n\
         ### 1. Rolling Narrative (~{} tokens)\n\
         A coherent narrative of what the project has accomplished, what is currently happening, \
         and what remains. Focus on the story arc: what problems were identified, how they were \
         addressed, what patterns emerged. Write in present/past tense. Include task IDs where relevant.\n\n\
         ### 2. Persistent Facts (~{} tokens)\n\
         Bullet list of stable facts about the project: architecture decisions, conventions adopted, \
         key file paths, integration points, recurring patterns. These facts should remain useful \
         across many sessions.\n\n\
         ### 3. Evaluation Digest (~{} tokens)\n\
         Summary of evaluation outcomes: which tasks scored well, which struggled, any patterns \
         in agent performance. If no evaluations exist, note that and suggest what to evaluate.\n\n\
         IMPORTANT: Output ONLY the context document. No preamble, no explanation. \
         Start directly with '# Project Context'.",
        narrative_budget, facts_budget, eval_budget,
    ));

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
            ..Default::default()
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
        assert!(state.last_compaction_duration_ms.is_none());
        assert!(state.last_compaction_tokens.is_none());
        assert!(state.last_compaction_context_bytes.is_none());
        assert_eq!(state.error_count, 0);
    }

    #[test]
    fn test_compactor_state_metrics_roundtrip() {
        use crate::graph::TokenUsage;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("compactor")).unwrap();

        let state = CompactorState {
            last_compaction: Some("2026-01-01T00:00:00Z".to_string()),
            last_ops_count: 5,
            last_tick: 1,
            compaction_count: 2,
            last_compaction_duration_ms: Some(4500),
            last_compaction_tokens: Some(TokenUsage {
                cost_usd: 0.001,
                input_tokens: 800,
                output_tokens: 200,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            }),
            last_compaction_context_bytes: Some(1024),
            error_count: 0,
        };
        state.save(dir).unwrap();

        let loaded = CompactorState::load(dir);
        assert_eq!(loaded.last_compaction_duration_ms, Some(4500));
        assert_eq!(loaded.last_compaction_context_bytes, Some(1024));
        let tokens = loaded.last_compaction_tokens.expect("should have tokens");
        assert_eq!(tokens.input_tokens, 800);
        assert_eq!(tokens.output_tokens, 200);
        assert_eq!(loaded.error_count, 0);
    }

    #[test]
    fn test_compactor_state_error_count_persists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("compactor")).unwrap();

        let mut state = CompactorState::default();
        state.error_count = 3;
        state.save(dir).unwrap();

        let loaded = CompactorState::load(dir);
        assert_eq!(loaded.error_count, 3);
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

        let prompt = build_compactor_prompt(&snapshot, 200_000);
        assert!(prompt.contains("Total tasks: 10"));
        assert!(prompt.contains("Rolling Narrative"));
        assert!(prompt.contains("Persistent Facts"));
        assert!(prompt.contains("Evaluation Digest"));
        assert!(prompt.contains("[task-1] First task"));
        assert!(prompt.contains("[task-2] Active task"));
    }

    #[test]
    fn test_compactor_section_budgets_default_200k() {
        let (narrative, facts, eval) = compactor_section_budgets(200_000);
        // 200k * 0.03 = 6000 (at the cap)
        assert_eq!(narrative + facts + eval, 6000);
        assert!(narrative > facts);
        assert!(narrative > eval);
    }

    #[test]
    fn test_compactor_section_budgets_small_window() {
        let (narrative, facts, eval) = compactor_section_budgets(16_000);
        // 16k * 0.03 = 480 → clamped to 500, then section minimums apply
        assert!(narrative >= 300);
        assert!(facts >= 100);
        assert!(eval >= 100);
        // Total may exceed 500 slightly due to section floor clamping
        assert!(narrative + facts + eval >= 500);
        assert!(narrative + facts + eval <= 600);
    }

    #[test]
    fn test_compactor_section_budgets_medium_window() {
        let (narrative, facts, eval) = compactor_section_budgets(64_000);
        // 64k * 0.03 = 1920
        let total = narrative + facts + eval;
        assert!(total >= 1900 && total <= 1950);
    }

    #[test]
    fn test_compactor_section_budgets_very_large_window() {
        let (narrative, facts, eval) = compactor_section_budgets(1_000_000);
        // 1M * 0.03 = 30000 → clamped to 6000
        assert_eq!(narrative + facts + eval, 6000);
    }

    #[test]
    fn test_compactor_section_budgets_floor_clamp() {
        let (narrative, facts, eval) = compactor_section_budgets(1_000);
        // 1k * 0.03 = 30 → clamped to 500, then section minimums apply
        assert!(narrative >= 300);
        assert!(facts >= 100);
        assert!(eval >= 100);
        // Total may exceed 500 slightly due to section floor clamping
        assert!(narrative + facts + eval >= 500);
        assert!(narrative + facts + eval <= 600);
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
