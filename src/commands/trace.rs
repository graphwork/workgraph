use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use workgraph::graph::{Status, Task, WorkGraph};
use workgraph::provenance::{self, OperationEntry};
use workgraph::query::build_reverse_index;

/// Output mode for the trace command
pub enum TraceMode {
    /// Human-readable summary (default)
    Summary,
    /// Full structured JSON output
    Json,
    /// Show complete agent conversation
    Full,
    /// Show only provenance log entries
    OpsOnly,
}

/// A single agent run archive entry
#[derive(Debug, Serialize)]
struct AgentRun {
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turns: Option<usize>,
}

/// Summary statistics
#[derive(Debug, Serialize)]
struct TraceSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_human: Option<String>,
    operation_count: usize,
    agent_run_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_tool_calls: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_turns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_output_bytes: Option<u64>,
}

/// Full structured trace output
#[derive(Debug, Serialize)]
struct TraceOutput {
    id: String,
    title: String,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    assigned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    operations: Vec<OperationEntry>,
    agent_runs: Vec<AgentRun>,
    summary: TraceSummary,
}

/// Parse Claude stream-json output to count tool calls and turns.
/// Returns (tool_call_count, turn_count).
fn parse_stream_json_stats(output: &str) -> (usize, usize) {
    let mut tool_calls = 0;
    let mut turns = 0;

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Try to parse as JSON
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            match val.get("type").and_then(|t| t.as_str()) {
                Some("assistant") => {
                    turns += 1;
                }
                Some("tool_use") | Some("tool_result") => {
                    if val.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        tool_calls += 1;
                    }
                }
                Some("result") => {
                    // Final result message, count as a turn if we haven't yet
                    if turns == 0 {
                        turns = 1;
                    }
                }
                _ => {}
            }
            // Also check for content_block with type "tool_use"
            if let Some(content_type) = val.get("content_block").and_then(|cb| cb.get("type")).and_then(|t| t.as_str())
                && content_type == "tool_use" {
                    tool_calls += 1;
                }
        }
    }

    (tool_calls, turns)
}

pub fn format_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {}m", h, m)
    }
}

fn load_agent_runs(dir: &Path, task_id: &str, include_content: bool) -> Vec<AgentRun> {
    let archive_base = dir.join("log").join("agents").join(task_id);
    if !archive_base.exists() {
        return Vec::new();
    }

    let mut attempts: Vec<_> = match fs::read_dir(&archive_base) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect(),
        Err(_) => return Vec::new(),
    };
    attempts.sort_by_key(|e| e.file_name());

    attempts
        .iter()
        .map(|attempt| {
            let path = attempt.path();
            let timestamp = attempt.file_name().to_string_lossy().to_string();

            let prompt_path = path.join("prompt.txt");
            let output_path = path.join("output.txt");

            let prompt_meta = fs::metadata(&prompt_path).ok();
            let output_meta = fs::metadata(&output_path).ok();

            let prompt_content = if include_content {
                fs::read_to_string(&prompt_path).ok()
            } else {
                None
            };

            let output_content = fs::read_to_string(&output_path).ok();
            let output_lines = output_content.as_ref().map(|c| c.lines().count());
            let prompt_lines = if include_content {
                prompt_content.as_ref().map(|c| c.lines().count())
            } else {
                fs::read_to_string(&prompt_path)
                    .ok()
                    .map(|c| c.lines().count())
            };

            let (tool_calls, turns) = output_content
                .as_ref()
                .map(|c| parse_stream_json_stats(c))
                .unwrap_or((0, 0));

            AgentRun {
                timestamp,
                prompt_bytes: prompt_meta.map(|m| m.len()),
                output_bytes: output_meta.map(|m| m.len()),
                prompt_lines,
                output_lines,
                prompt: if include_content { prompt_content } else { None },
                output: if include_content { output_content } else { None },
                tool_calls: if tool_calls > 0 { Some(tool_calls) } else { None },
                turns: if turns > 0 { Some(turns) } else { None },
            }
        })
        .collect()
}

pub fn run(dir: &Path, id: &str, mode: TraceMode) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;
    let task = graph.get_task_or_err(id)?;

    // Load operations for this task
    let all_ops = provenance::read_all_operations(dir)?;
    let task_ops: Vec<OperationEntry> = all_ops
        .into_iter()
        .filter(|e| e.task_id.as_deref() == Some(id))
        .collect();

    match mode {
        TraceMode::OpsOnly => {
            print_ops_only(id, &task_ops);
            Ok(())
        }
        TraceMode::Json => {
            let include_content = true;
            let agent_runs = load_agent_runs(dir, id, include_content);
            let summary = build_summary(task, &task_ops, &agent_runs);

            let output = TraceOutput {
                id: task.id.clone(),
                title: task.title.clone(),
                status: task.status,
                assigned: task.assigned.clone(),
                created_at: task.created_at.clone(),
                started_at: task.started_at.clone(),
                completed_at: task.completed_at.clone(),
                operations: task_ops,
                agent_runs,
                summary,
            };

            println!("{}", serde_json::to_string_pretty(&output)?);
            Ok(())
        }
        TraceMode::Full => {
            let agent_runs = load_agent_runs(dir, id, true);
            let summary = build_summary(task, &task_ops, &agent_runs);
            print_header(task);
            print_summary(&summary);
            println!();
            print_ops(id, &task_ops);
            println!();
            print_agent_runs_full(&agent_runs);
            Ok(())
        }
        TraceMode::Summary => {
            let agent_runs = load_agent_runs(dir, id, false);
            let summary = build_summary(task, &task_ops, &agent_runs);
            print_header(task);
            print_summary(&summary);
            println!();
            print_ops(id, &task_ops);
            println!();
            print_agent_runs_summary(&agent_runs);
            Ok(())
        }
    }
}

fn build_summary(
    task: &workgraph::graph::Task,
    ops: &[OperationEntry],
    agent_runs: &[AgentRun],
) -> TraceSummary {
    let duration = match (task.started_at.as_ref(), task.completed_at.as_ref()) {
        (Some(s), Some(c)) => {
            let started: Option<DateTime<chrono::Utc>> =
                s.parse::<DateTime<chrono::FixedOffset>>().ok().map(|d| d.into());
            let completed: Option<DateTime<chrono::Utc>> =
                c.parse::<DateTime<chrono::FixedOffset>>().ok().map(|d| d.into());
            match (started, completed) {
                (Some(s), Some(c)) => Some((c - s).num_seconds()),
                _ => None,
            }
        }
        _ => None,
    };

    let total_tool_calls: usize = agent_runs
        .iter()
        .filter_map(|r| r.tool_calls)
        .sum();
    let total_turns: usize = agent_runs.iter().filter_map(|r| r.turns).sum();
    let total_output_bytes: u64 = agent_runs
        .iter()
        .filter_map(|r| r.output_bytes)
        .sum();

    TraceSummary {
        duration_secs: duration,
        duration_human: duration.map(format_duration),
        operation_count: ops.len(),
        agent_run_count: agent_runs.len(),
        total_tool_calls: if total_tool_calls > 0 {
            Some(total_tool_calls)
        } else {
            None
        },
        total_turns: if total_turns > 0 {
            Some(total_turns)
        } else {
            None
        },
        total_output_bytes: if total_output_bytes > 0 {
            Some(total_output_bytes)
        } else {
            None
        },
    }
}

fn print_header(task: &workgraph::graph::Task) {
    println!("Trace: {} ({})", task.id, task.status);
    println!("Title: {}", task.title);
    if let Some(ref assigned) = task.assigned {
        println!("Assigned: {}", assigned);
    }
    if let Some(ref created) = task.created_at {
        println!("Created: {}", created);
    }
    if let Some(ref started) = task.started_at {
        println!("Started: {}", started);
    }
    if let Some(ref completed) = task.completed_at {
        println!("Completed: {}", completed);
    }
}

fn print_summary(summary: &TraceSummary) {
    println!();
    println!("Summary:");
    if let Some(ref dur) = summary.duration_human {
        println!("  Duration: {}", dur);
    }
    println!("  Operations: {}", summary.operation_count);
    println!("  Agent runs: {}", summary.agent_run_count);
    if let Some(turns) = summary.total_turns {
        println!("  Total turns: {}", turns);
    }
    if let Some(tool_calls) = summary.total_tool_calls {
        println!("  Total tool calls: {}", tool_calls);
    }
    if let Some(bytes) = summary.total_output_bytes {
        let kb = bytes as f64 / 1024.0;
        if kb > 1024.0 {
            println!("  Total output: {:.1} MB", kb / 1024.0);
        } else {
            println!("  Total output: {:.1} KB", kb);
        }
    }
}

fn print_ops(_id: &str, ops: &[OperationEntry]) {
    if ops.is_empty() {
        println!("Operations: (none)");
        return;
    }

    println!("Operations ({}):", ops.len());
    for entry in ops {
        let actor_str = entry
            .actor
            .as_ref()
            .map(|a| format!(" ({})", a))
            .unwrap_or_default();
        println!("  {} {}{}", entry.timestamp, entry.op, actor_str);
        if !entry.detail.is_null() {
            // Print detail compactly
            let detail_str = serde_json::to_string(&entry.detail).unwrap_or_default();
            if detail_str.len() <= 120 {
                println!("    {}", detail_str);
            } else {
                println!("    {}...", &detail_str[..117]);
            }
        }
    }
}

fn print_ops_only(id: &str, ops: &[OperationEntry]) {
    if ops.is_empty() {
        println!("No operations recorded for task '{}'", id);
        return;
    }

    println!("Operations for '{}' ({} entries):", id, ops.len());
    println!();
    for entry in ops {
        let actor_str = entry
            .actor
            .as_ref()
            .map(|a| format!(" ({})", a))
            .unwrap_or_default();
        println!("  {} {}{}", entry.timestamp, entry.op, actor_str);
        if !entry.detail.is_null() {
            println!("    {}", entry.detail);
        }
    }
}

fn print_agent_runs_summary(runs: &[AgentRun]) {
    if runs.is_empty() {
        println!("Agent runs: (none)");
        return;
    }

    println!("Agent runs ({}):", runs.len());
    for (i, run) in runs.iter().enumerate() {
        println!("  Run {} [{}]", i + 1, run.timestamp);
        if let Some(bytes) = run.output_bytes {
            let kb = bytes as f64 / 1024.0;
            print!("    Output: {:.1} KB", kb);
            if let Some(lines) = run.output_lines {
                print!(" ({} lines)", lines);
            }
            println!();
        }
        if let Some(turns) = run.turns {
            print!("    Turns: {}", turns);
            if let Some(tc) = run.tool_calls {
                print!(", Tool calls: {}", tc);
            }
            println!();
        } else if let Some(tc) = run.tool_calls {
            println!("    Tool calls: {}", tc);
        }
    }
}

fn print_agent_runs_full(runs: &[AgentRun]) {
    if runs.is_empty() {
        println!("Agent runs: (none)");
        return;
    }

    println!("Agent runs ({}):", runs.len());
    for (i, run) in runs.iter().enumerate() {
        println!();
        println!("--- Run {} [{}] ---", i + 1, run.timestamp);

        if let Some(ref prompt) = run.prompt {
            println!();
            println!("  [Prompt] ({} bytes)", prompt.len());
            for line in prompt.lines() {
                println!("    {}", line);
            }
        }

        if let Some(ref output) = run.output {
            println!();
            println!("  [Output] ({} bytes)", output.len());
            for line in output.lines() {
                println!("    {}", line);
            }
        }
    }
}

// ── Recursive trace ─────────────────────────────────────────────────────

/// A human intervention detected from the provenance log.
#[derive(Debug, Serialize, Clone)]
pub struct HumanIntervention {
    pub timestamp: String,
    pub task_id: String,
    pub kind: String, // "fail", "retry", "add_task", "edit", "abandon"
    pub actor: Option<String>,
    pub detail: String,
}

/// Per-task summary used in recursive views.
#[derive(Debug, Serialize)]
struct RecursiveTaskInfo {
    id: String,
    title: String,
    status: Status,
    assigned: Option<String>,
    duration_secs: Option<i64>,
    duration_human: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    artifacts: Vec<String>,
    agent_runs: usize,
    interventions: Vec<HumanIntervention>,
}

/// Full recursive trace output (for JSON mode).
#[derive(Debug, Serialize)]
struct RecursiveTraceOutput {
    root_id: String,
    total_tasks: usize,
    wall_clock_secs: Option<i64>,
    wall_clock_human: Option<String>,
    tasks: Vec<RecursiveTaskInfo>,
    interventions: Vec<HumanIntervention>,
}

/// Collect the subgraph rooted at `root_id`: the task itself plus all tasks
/// whose after chains trace back to it. Filters out internal tasks
/// (assignment/evaluation tags) for cleaner display.
pub fn collect_descendants<'a>(root_id: &str, graph: &'a WorkGraph) -> Vec<&'a Task> {
    let reverse_index = build_reverse_index(graph);
    let mut visited = HashSet::new();
    let mut queue = vec![root_id.to_string()];
    let mut result = Vec::new();

    while let Some(id) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some(task) = graph.get_task(&id) {
            // Skip internal agency tasks for cleaner output
            let is_internal = task.tags.iter().any(|t| t == "assignment" || t == "evaluation");
            if !is_internal {
                result.push(task);
            }
            // Follow reverse index: tasks that depend on this one
            if let Some(deps) = reverse_index.get(&id) {
                for dep_id in deps {
                    queue.push(dep_id.clone());
                }
            }
        }
    }

    // Sort by started_at for chronological ordering, falling back to created_at
    result.sort_by(|a, b| {
        let a_time = a.started_at.as_deref().or(a.created_at.as_deref()).unwrap_or("");
        let b_time = b.started_at.as_deref().or(b.created_at.as_deref()).unwrap_or("");
        a_time.cmp(b_time)
    });
    result
}

/// Detect human interventions from the provenance log for a set of task IDs.
fn detect_interventions(
    dir: &Path,
    task_ids: &HashSet<&str>,
) -> Vec<HumanIntervention> {
    let all_ops = provenance::read_all_operations(dir).unwrap_or_default();
    let mut interventions = Vec::new();

    // Ops that indicate human intervention: fail, retry, abandon, add_task (manual),
    // edit (manual changes)
    for op in &all_ops {
        let task_id = match op.task_id.as_deref() {
            Some(id) if task_ids.contains(id) => id,
            _ => continue,
        };

        // Skip operations by agents (actor starting with "agent-" is automated)
        let is_human = op.actor.as_ref()
            .map(|a| !a.starts_with("agent-") && !a.starts_with("coordinator"))
            .unwrap_or(true); // No actor = likely human CLI usage

        if !is_human {
            continue;
        }

        let (kind, detail) = match op.op.as_str() {
            "fail" => {
                let reason = op.detail.get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                ("fail".to_string(), format!("Task manually failed: {}", reason))
            }
            "retry" => {
                let attempt = op.detail.get("attempt")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                ("retry".to_string(), format!("Task retried (attempt {})", attempt))
            }
            "abandon" => {
                let reason = op.detail.get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                ("abandon".to_string(), format!("Task abandoned: {}", reason))
            }
            "add_task" => {
                ("add_task".to_string(), "Task manually added".to_string())
            }
            "edit" => {
                let fields = op.detail.get("fields")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|f| f.get("field").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                ("edit".to_string(), format!("Task manually edited: {}", fields))
            }
            _ => continue,
        };

        interventions.push(HumanIntervention {
            timestamp: op.timestamp.clone(),
            task_id: task_id.to_string(),
            kind,
            actor: op.actor.clone(),
            detail,
        });
    }

    interventions.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    interventions
}

fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    ts.parse::<DateTime<chrono::FixedOffset>>()
        .ok()
        .map(|d| d.into())
}

fn task_duration_secs(task: &Task) -> Option<i64> {
    let started = task.started_at.as_ref().and_then(|s| parse_timestamp(s))?;
    let completed = task.completed_at.as_ref().and_then(|s| parse_timestamp(s))?;
    Some((completed - started).num_seconds())
}

/// Compute wall-clock duration from the earliest start to the latest completion
/// across all tasks in the subgraph.
fn compute_wall_clock(tasks: &[&Task]) -> Option<i64> {
    let earliest = tasks.iter()
        .filter_map(|t| t.started_at.as_ref().and_then(|s| parse_timestamp(s)))
        .min()?;
    let latest = tasks.iter()
        .filter_map(|t| t.completed_at.as_ref().and_then(|s| parse_timestamp(s)))
        .max()?;
    Some((latest - earliest).num_seconds())
}

/// Build a RecursiveTaskInfo for a single task.
fn build_recursive_info(
    task: &Task,
    dir: &Path,
    task_interventions: &[HumanIntervention],
) -> RecursiveTaskInfo {
    let duration = task_duration_secs(task);
    let agent_runs = load_agent_runs(dir, &task.id, false).len();

    RecursiveTaskInfo {
        id: task.id.clone(),
        title: task.title.clone(),
        status: task.status,
        assigned: task.assigned.clone(),
        duration_secs: duration,
        duration_human: duration.map(format_duration),
        started_at: task.started_at.clone(),
        completed_at: task.completed_at.clone(),
        artifacts: task.artifacts.clone(),
        agent_runs,
        interventions: task_interventions.to_vec(),
    }
}

/// Run the recursive trace view.
pub fn run_recursive(dir: &Path, root_id: &str, timeline: bool, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;
    let _root = graph.get_task_or_err(root_id)?;

    let descendants = collect_descendants(root_id, &graph);
    let task_ids: HashSet<&str> = descendants.iter().map(|t| t.id.as_str()).collect();
    let interventions = detect_interventions(dir, &task_ids);

    // Build per-task intervention map
    let mut intervention_map: HashMap<&str, Vec<HumanIntervention>> = HashMap::new();
    for iv in &interventions {
        intervention_map.entry(iv.task_id.as_str()).or_default().push(iv.clone());
    }

    let wall_clock = compute_wall_clock(&descendants);

    if json {
        let task_infos: Vec<RecursiveTaskInfo> = descendants.iter()
            .map(|t| {
                let task_ivs = intervention_map.get(t.id.as_str())
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                build_recursive_info(t, dir, task_ivs)
            })
            .collect();

        let output = RecursiveTraceOutput {
            root_id: root_id.to_string(),
            total_tasks: descendants.len(),
            wall_clock_secs: wall_clock,
            wall_clock_human: wall_clock.map(format_duration),
            tasks: task_infos,
            interventions: interventions.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if timeline {
        print_timeline(&descendants, &interventions, dir, wall_clock);
    } else {
        print_recursive_tree(root_id, &graph, &descendants, &intervention_map, dir, wall_clock);
    }

    Ok(())
}

/// Print the recursive execution tree with ASCII rendering.
#[allow(clippy::only_used_in_recursion)]
fn print_recursive_tree(
    root_id: &str,
    graph: &WorkGraph,
    descendants: &[&Task],
    intervention_map: &HashMap<&str, Vec<HumanIntervention>>,
    dir: &Path,
    wall_clock: Option<i64>,
) {
    let use_color = std::io::stdout().is_terminal();
    let reset = if use_color { "\x1b[0m" } else { "" };
    let bold = if use_color { "\x1b[1m" } else { "" };
    let dim = if use_color { "\x1b[2m" } else { "" };
    let red = if use_color { "\x1b[31m" } else { "" };
    let green = if use_color { "\x1b[32m" } else { "" };
    let yellow = if use_color { "\x1b[33m" } else { "" };
    let magenta = if use_color { "\x1b[35m" } else { "" };

    // Header
    let root = graph.get_task(root_id);
    let root_title = root.map(|t| t.title.as_str()).unwrap_or(root_id);
    println!("{}Recursive Trace: {}{} ({})", bold, root_id, reset, root_title);
    println!("{}Tasks: {}{}", dim, descendants.len(), reset);
    if let Some(wc) = wall_clock {
        println!("{}Wall clock: {}{}", dim, format_duration(wc), reset);
    }
    println!();

    // Build adjacency within descendant set
    let desc_ids: HashSet<&str> = descendants.iter().map(|t| t.id.as_str()).collect();
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();

    for task in descendants {
        for blocker in &task.after {
            if desc_ids.contains(blocker.as_str()) {
                forward.entry(blocker.as_str()).or_default().push(task.id.as_str());
                reverse.entry(task.id.as_str()).or_default().push(blocker.as_str());
            }
        }
    }
    for v in forward.values_mut() { v.sort(); }
    for v in reverse.values_mut() { v.sort(); }

    // Find roots (no parents in descendant set)
    let mut roots: Vec<&str> = descendants.iter()
        .filter(|t| reverse.get(t.id.as_str()).map(Vec::is_empty).unwrap_or(true))
        .map(|t| t.id.as_str())
        .collect();
    roots.sort();

    let task_map: HashMap<&str, &&Task> = descendants.iter()
        .map(|t| (t.id.as_str(), t))
        .collect();

    let mut rendered: HashSet<&str> = HashSet::new();

    #[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)]
    fn render_tree_recursive<'a>(
        id: &'a str,
        prefix: &str,
        is_last: bool,
        is_root: bool,
        rendered: &mut HashSet<&'a str>,
        forward: &HashMap<&str, Vec<&'a str>>,
        reverse: &HashMap<&str, Vec<&'a str>>,
        task_map: &HashMap<&str, &&Task>,
        intervention_map: &HashMap<&str, Vec<HumanIntervention>>,
        dir: &Path,
        use_color: bool,
    ) -> Vec<String> {
        let mut lines = Vec::new();

        let connector = if is_root {
            String::new()
        } else if is_last {
            "└→ ".to_string()
        } else {
            "├→ ".to_string()
        };

        let reset = if use_color { "\x1b[0m" } else { "" };
        let dim = if use_color { "\x1b[2m" } else { "" };
        let cyan = if use_color { "\x1b[36m" } else { "" };
        let magenta = if use_color { "\x1b[35m" } else { "" };

        let status_color_fn = |s: &Status| -> &str {
            if !use_color { return ""; }
            match s {
                Status::Done => "\x1b[32m",
                Status::InProgress => "\x1b[33m",
                Status::Failed => "\x1b[31m",
                Status::Open => "\x1b[37m",
                Status::Blocked | Status::Abandoned => "\x1b[90m",
            }
        };

        let status_label_fn = |s: &Status| -> &str {
            match s {
                Status::Done => "done",
                Status::InProgress => "in-progress",
                Status::Failed => "failed",
                Status::Open => "open",
                Status::Blocked => "blocked",
                Status::Abandoned => "abandoned",
            }
        };

        // Back-reference for already-rendered nodes (fan-in)
        if rendered.contains(id) {
            if let Some(task) = task_map.get(id) {
                lines.push(format!(
                    "{}{}{}{}  ({}) ...{}",
                    prefix, connector,
                    status_color_fn(&task.status), id, status_label_fn(&task.status),
                    reset
                ));
            }
            return lines;
        }
        rendered.insert(id);

        // Fan-in annotation
        let parents = reverse.get(id).map(Vec::as_slice).unwrap_or(&[]);
        let fan_in = if parents.len() > 1 {
            format!("  {}(← {}){}", dim, parents.join(", "), reset)
        } else {
            String::new()
        };

        // Build node display
        if let Some(task) = task_map.get(id) {
            let dur = task_duration_secs(task)
                .map(|s| format!("  {}[{}]{}", dim, format_duration(s), reset))
                .unwrap_or_default();

            let assigned = task.assigned.as_ref()
                .map(|a| format!("  {}({}){}", cyan, a, reset))
                .unwrap_or_default();

            let artifacts_str = if !task.artifacts.is_empty() {
                format!("  {}→ {}{}", dim, task.artifacts.join(", "), reset)
            } else {
                String::new()
            };

            lines.push(format!(
                "{}{}{}{}{}  ({}){}{}{}{}",
                prefix, connector,
                status_color_fn(&task.status), id, reset,
                status_label_fn(&task.status),
                dur, assigned, artifacts_str, fan_in,
            ));

            // Show interventions on this task
            if let Some(ivs) = intervention_map.get(id) {
                let child_prefix = if is_root {
                    prefix.to_string()
                } else if is_last {
                    format!("{}  ", prefix)
                } else {
                    format!("{}│ ", prefix)
                };
                for iv in ivs {
                    lines.push(format!(
                        "{}{}⚠ {} — {}{}",
                        child_prefix, magenta, iv.kind, iv.detail, reset
                    ));
                }
            }
        } else {
            lines.push(format!("{}{}{}  (unknown){}", prefix, connector, id, fan_in));
        }

        // Recurse into children
        let child_prefix = if is_root {
            prefix.to_string()
        } else if is_last {
            format!("{}  ", prefix)
        } else {
            format!("{}│ ", prefix)
        };

        let children = forward.get(id).map(Vec::as_slice).unwrap_or(&[]);
        for (i, &child) in children.iter().enumerate() {
            let child_is_last = i == children.len() - 1;
            let child_lines = render_tree_recursive(
                child, &child_prefix, child_is_last, false,
                rendered, forward, reverse, task_map,
                intervention_map, dir, use_color,
            );
            lines.extend(child_lines);
        }

        lines
    }

    for (i, root_node) in roots.iter().enumerate() {
        if i > 0 { println!(); }
        let tree_lines = render_tree_recursive(
            root_node, "", true, true,
            &mut rendered, &forward, &reverse, &task_map,
            intervention_map, dir, use_color,
        );
        for line in &tree_lines {
            println!("{}", line);
        }
    }

    // Summary at bottom
    println!();

    // Count statuses
    let done_count = descendants.iter().filter(|t| t.status == Status::Done).count();
    let failed_count = descendants.iter().filter(|t| t.status == Status::Failed).count();
    let in_progress = descendants.iter().filter(|t| t.status == Status::InProgress).count();
    let open_count = descendants.iter().filter(|t| t.status == Status::Open).count();

    print!("{}Summary: ", dim);
    let mut parts = Vec::new();
    if done_count > 0 { parts.push(format!("{}{} done{}", green, done_count, reset)); }
    if in_progress > 0 { parts.push(format!("{}{} in-progress{}", yellow, in_progress, reset)); }
    if open_count > 0 { parts.push(format!("{} open", open_count)); }
    if failed_count > 0 { parts.push(format!("{}{} failed{}", red, failed_count, reset)); }
    println!("{}{}", parts.join(", "), reset);

    // Show interventions summary
    let total_interventions: usize = intervention_map.values().map(|v| v.len()).sum();
    if total_interventions > 0 {
        println!(
            "{}Human interventions: {}{}{}",
            dim, magenta, total_interventions, reset
        );
    }
}

/// Print the chronological timeline with parallel execution lanes.
fn print_timeline(
    descendants: &[&Task],
    interventions: &[HumanIntervention],
    _dir: &Path,
    wall_clock: Option<i64>,
) {
    let use_color = std::io::stdout().is_terminal();
    let reset = if use_color { "\x1b[0m" } else { "" };
    let bold = if use_color { "\x1b[1m" } else { "" };
    let dim = if use_color { "\x1b[2m" } else { "" };
    let magenta = if use_color { "\x1b[35m" } else { "" };

    let status_color = |s: &Status| -> &str {
        if !use_color { return ""; }
        match s {
            Status::Done => "\x1b[32m",
            Status::InProgress => "\x1b[33m",
            Status::Failed => "\x1b[31m",
            Status::Open => "\x1b[37m",
            Status::Blocked | Status::Abandoned => "\x1b[90m",
        }
    };

    println!("{}Execution Timeline{}", bold, reset);
    if let Some(wc) = wall_clock {
        println!("{}Total wall clock: {}{}", dim, format_duration(wc), reset);
    }
    println!();

    // Collect timeline events (start/end for each task + interventions)
    #[derive(Debug, Clone)]
    struct TimelineEvent {
        timestamp: DateTime<Utc>,
        kind: TimelineEventKind,
        task_id: String,
    }

    #[derive(Debug, Clone)]
    enum TimelineEventKind {
        Start,
        End(Status),
        Intervention(String), // detail string
    }

    let mut events: Vec<TimelineEvent> = Vec::new();

    for task in descendants {
        if let Some(ref started) = task.started_at
            && let Some(ts) = parse_timestamp(started) {
                events.push(TimelineEvent {
                    timestamp: ts,
                    kind: TimelineEventKind::Start,
                    task_id: task.id.clone(),
                });
            }
        if let Some(ref completed) = task.completed_at
            && let Some(ts) = parse_timestamp(completed) {
                events.push(TimelineEvent {
                    timestamp: ts,
                    kind: TimelineEventKind::End(task.status),
                    task_id: task.id.clone(),
                });
            }
    }

    for iv in interventions {
        if let Some(ts) = parse_timestamp(&iv.timestamp) {
            events.push(TimelineEvent {
                timestamp: ts,
                kind: TimelineEventKind::Intervention(iv.detail.clone()),
                task_id: iv.task_id.clone(),
            });
        }
    }

    events.sort_by_key(|e| e.timestamp);

    if events.is_empty() {
        println!("{}(no execution data available){}", dim, reset);
        return;
    }

    // Track active lanes (parallel execution)
    let mut active_lanes: Vec<String> = Vec::new(); // task_id per lane
    let mut task_lanes: HashMap<String, usize> = HashMap::new();

    let base_time = events.first().map(|e| e.timestamp).unwrap();

    for event in &events {
        let elapsed = (event.timestamp - base_time).num_seconds();
        let time_str = format!("+{}", format_duration(elapsed));

        match &event.kind {
            TimelineEventKind::Start => {
                // Find an empty lane or create a new one
                let lane = active_lanes.iter().position(|l| l.is_empty())
                    .unwrap_or_else(|| {
                        active_lanes.push(String::new());
                        active_lanes.len() - 1
                    });
                active_lanes[lane] = event.task_id.clone();
                task_lanes.insert(event.task_id.clone(), lane);

                // Render lane indicators
                let lanes_str = render_lanes(&active_lanes, Some(lane), "▶", use_color);
                println!(
                    "  {}{:>8}{}  {}  {} started",
                    dim, time_str, reset, lanes_str, event.task_id
                );
            }
            TimelineEventKind::End(status) => {
                let lane = task_lanes.get(&event.task_id).copied();
                let marker = match status {
                    Status::Done => "✓",
                    Status::Failed => "✗",
                    _ => "•",
                };
                let color = status_color(status);
                let lanes_str = render_lanes(&active_lanes, lane, marker, use_color);

                let dur = descendants.iter()
                    .find(|t| t.id == event.task_id)
                    .and_then(|t| task_duration_secs(t))
                    .map(|s| format!(" {}{}{}", dim, format_duration(s), reset))
                    .unwrap_or_default();

                println!(
                    "  {}{:>8}{}  {}  {}{} completed{}{}",
                    dim, time_str, reset, lanes_str,
                    color, event.task_id, reset, dur
                );

                // Free the lane
                if let Some(l) = lane
                    && l < active_lanes.len() {
                        active_lanes[l] = String::new();
                    }
            }
            TimelineEventKind::Intervention(detail) => {
                let lane = task_lanes.get(&event.task_id).copied();
                let lanes_str = render_lanes(&active_lanes, lane, "⚠", use_color);
                println!(
                    "  {}{:>8}{}  {}  {}⚠ {} — {}{}",
                    dim, time_str, reset, lanes_str,
                    magenta, event.task_id, detail, reset
                );
            }
        }
    }

    // Legend
    println!();
    let max_parallel = {
        let mut max = 0usize;
        let mut current = 0usize;
        for event in &events {
            match &event.kind {
                TimelineEventKind::Start => {
                    current += 1;
                    if current > max { max = current; }
                }
                TimelineEventKind::End(_) => {
                    current = current.saturating_sub(1);
                }
                _ => {}
            }
        }
        max
    };

    println!(
        "{}Max parallel: {} | Total tasks: {} | Events: {}{}",
        dim, max_parallel, descendants.len(), events.len(), reset
    );
}

/// Render lane indicators for the timeline view.
fn render_lanes(
    active_lanes: &[String],
    highlight_lane: Option<usize>,
    marker: &str,
    use_color: bool,
) -> String {
    let cyan = if use_color { "\x1b[36m" } else { "" };
    let dim = if use_color { "\x1b[2m" } else { "" };
    let reset = if use_color { "\x1b[0m" } else { "" };

    // Trim trailing empty lanes for display
    let effective_len = active_lanes.iter()
        .rposition(|l| !l.is_empty())
        .map(|p| p + 1)
        .unwrap_or(0)
        .max(highlight_lane.map(|l| l + 1).unwrap_or(0));

    let mut result = String::new();
    for (i, lane) in active_lanes.iter().enumerate().take(effective_len) {
        if Some(i) == highlight_lane {
            result.push_str(&format!("{}{}{}", cyan, marker, reset));
        } else if !lane.is_empty() {
            result.push_str(&format!("{}│{}", dim, reset));
        } else {
            result.push(' ');
        }
    }
    result
}

// ── Static graph visualization ───────────────────────────────────────────

/// Render the subgraph involved in a trace as a 2D box layout.
pub fn run_graph(dir: &Path, root_id: &str) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;
    let _root = graph.get_task_or_err(root_id)?;

    let descendants = collect_descendants(root_id, &graph);
    let task_ids: HashSet<&str> = descendants.iter().map(|t| t.id.as_str()).collect();
    let annotations = HashMap::new();

    let output = super::viz::generate_graph(&graph, &descendants, &task_ids, &annotations);
    println!("{}", output);
    Ok(())
}

// ── Temporal trace reconstruction ────────────────────────────────────────

/// A snapshot of the graph state at a point in time.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct GraphSnapshot {
    pub timestamp: DateTime<Utc>,
    /// task_id → status at this point in time
    pub statuses: HashMap<String, Status>,
    /// task_id → assigned agent at this point
    pub assignments: HashMap<String, String>,
    /// task IDs that exist at this point
    pub task_ids: HashSet<String>,
}

/// Reconstruct the graph state at each operation timestamp by replaying
/// the operation log. Returns a Vec of (timestamp, snapshot) pairs
/// for the tasks in the given subgraph.
pub fn reconstruct_temporal(
    dir: &Path,
    subgraph_ids: &HashSet<&str>,
) -> Result<Vec<GraphSnapshot>> {
    let all_ops = provenance::read_all_operations(dir)?;

    // Filter to operations relevant to our subgraph
    let relevant_ops: Vec<&OperationEntry> = all_ops
        .iter()
        .filter(|e| {
            e.task_id
                .as_deref()
                .map(|id| subgraph_ids.contains(id))
                .unwrap_or(false)
        })
        .collect();

    if relevant_ops.is_empty() {
        return Ok(Vec::new());
    }

    // Build snapshots by replaying operations
    let mut current_statuses: HashMap<String, Status> = HashMap::new();
    let mut current_assignments: HashMap<String, String> = HashMap::new();
    let mut current_task_ids: HashSet<String> = HashSet::new();
    let mut snapshots = Vec::new();

    // Initialize all subgraph tasks as existing but Open
    for &id in subgraph_ids {
        current_task_ids.insert(id.to_string());
    }

    for op in &relevant_ops {
        let task_id = match op.task_id.as_deref() {
            Some(id) => id.to_string(),
            None => continue,
        };

        let ts = match parse_timestamp(&op.timestamp) {
            Some(ts) => ts,
            None => continue,
        };

        // Apply the operation to our state
        match op.op.as_str() {
            "add_task" => {
                current_task_ids.insert(task_id.clone());
                current_statuses.insert(task_id.clone(), Status::Open);
            }
            "claim" => {
                current_statuses.insert(task_id.clone(), Status::InProgress);
                if let Some(actor) = &op.actor {
                    current_assignments.insert(task_id.clone(), actor.clone());
                }
            }
            "unclaim" => {
                current_statuses.insert(task_id.clone(), Status::Open);
                current_assignments.remove(&task_id);
            }
            "done" => {
                current_statuses.insert(task_id.clone(), Status::Done);
            }
            "fail" => {
                current_statuses.insert(task_id.clone(), Status::Failed);
            }
            "retry" => {
                current_statuses.insert(task_id.clone(), Status::Open);
                current_assignments.remove(&task_id);
            }
            "abandon" => {
                current_statuses.insert(task_id.clone(), Status::Abandoned);
            }
            "pause" => {
                current_statuses.insert(task_id.clone(), Status::Blocked);
            }
            "resume" => {
                // Resume restores to Open (or InProgress if was claimed)
                if current_assignments.contains_key(&task_id) {
                    current_statuses.insert(task_id.clone(), Status::InProgress);
                } else {
                    current_statuses.insert(task_id.clone(), Status::Open);
                }
            }
            _ => {
                // Other ops (edit, artifact_add, etc.) don't change status
                continue;
            }
        }

        snapshots.push(GraphSnapshot {
            timestamp: ts,
            statuses: current_statuses.clone(),
            assignments: current_assignments.clone(),
            task_ids: current_task_ids.clone(),
        });
    }

    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_graph(dir: &std::path::Path, graph: &WorkGraph) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("graph.jsonl");
        save_graph(graph, &path).unwrap();
    }

    #[test]
    fn test_trace_basic_task_summary() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", TraceMode::Summary);
        assert!(result.is_ok());
    }

    #[test]
    fn test_trace_basic_task_json() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", TraceMode::Json);
        assert!(result.is_ok());
    }

    #[test]
    fn test_trace_basic_task_full() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", TraceMode::Full);
        assert!(result.is_ok());
    }

    #[test]
    fn test_trace_ops_only() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", TraceMode::OpsOnly);
        assert!(result.is_ok());
    }

    #[test]
    fn test_trace_nonexistent_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        setup_graph(&dir, &graph);

        let result = run(&dir, "nonexistent", TraceMode::Summary);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_trace_with_operations() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        setup_graph(&dir, &graph);

        // Record some operations
        provenance::record(
            &dir,
            "add_task",
            Some("t1"),
            None,
            serde_json::json!({"title": "Test task"}),
            provenance::DEFAULT_ROTATION_THRESHOLD,
        )
        .unwrap();
        provenance::record(
            &dir,
            "claim",
            Some("t1"),
            Some("agent-1"),
            serde_json::Value::Null,
            provenance::DEFAULT_ROTATION_THRESHOLD,
        )
        .unwrap();
        provenance::record(
            &dir,
            "done",
            Some("t1"),
            None,
            serde_json::Value::Null,
            provenance::DEFAULT_ROTATION_THRESHOLD,
        )
        .unwrap();

        let result = run(&dir, "t1", TraceMode::Summary);
        assert!(result.is_ok());
    }

    #[test]
    fn test_trace_with_agent_archives() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        setup_graph(&dir, &graph);

        // Create an agent archive
        let archive_dir = dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-02-18T20:00:00Z");
        fs::create_dir_all(&archive_dir).unwrap();
        fs::write(archive_dir.join("prompt.txt"), "Test prompt").unwrap();
        fs::write(archive_dir.join("output.txt"), "Test output").unwrap();

        let result = run(&dir, "t1", TraceMode::Summary);
        assert!(result.is_ok());

        let result = run(&dir, "t1", TraceMode::Full);
        assert!(result.is_ok());
    }

    #[test]
    fn test_trace_json_with_agent_archives() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        setup_graph(&dir, &graph);

        // Create an agent archive
        let archive_dir = dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-02-18T20:00:00Z");
        fs::create_dir_all(&archive_dir).unwrap();
        fs::write(archive_dir.join("prompt.txt"), "Test prompt").unwrap();
        fs::write(archive_dir.join("output.txt"), "Test output data here").unwrap();

        let result = run(&dir, "t1", TraceMode::Json);
        assert!(result.is_ok());
    }

    #[test]
    fn test_trace_not_initialized() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let result = run(&dir, "t1", TraceMode::Summary);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_parse_stream_json_stats_empty() {
        let (tc, turns) = parse_stream_json_stats("");
        assert_eq!(tc, 0);
        assert_eq!(turns, 0);
    }

    #[test]
    fn test_parse_stream_json_stats_with_turns_and_tools() {
        let output = r#"{"type":"assistant","message":"hello"}
{"type":"tool_use","name":"Read","id":"123"}
{"type":"tool_result","tool_use_id":"123"}
{"type":"assistant","message":"done"}
{"type":"result","cost":{"input":100,"output":50}}
"#;
        let (tc, turns) = parse_stream_json_stats(output);
        assert_eq!(tc, 1);
        assert_eq!(turns, 2);
    }

    #[test]
    fn test_parse_stream_json_non_json_lines_ignored() {
        let output = "not json\nalso not json\n";
        let (tc, turns) = parse_stream_json_stats(output);
        assert_eq!(tc, 0);
        assert_eq!(turns, 0);
    }

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(45), "45s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(125), "2m 5s");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(3725), "1h 2m");
    }

    #[test]
    fn test_build_summary_no_timestamps() {
        let task = make_task("t1", "Test");
        let ops = vec![];
        let runs = vec![];
        let summary = build_summary(&task, &ops, &runs);
        assert!(summary.duration_secs.is_none());
        assert_eq!(summary.operation_count, 0);
        assert_eq!(summary.agent_run_count, 0);
    }

    #[test]
    fn test_load_agent_runs_no_archive_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        let runs = load_agent_runs(&dir, "nonexistent", false);
        assert!(runs.is_empty());
    }

    // ── Recursive trace tests ──

    fn make_done_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Done,
            started_at: Some("2026-02-20T10:00:00+00:00".to_string()),
            completed_at: Some("2026-02-20T10:05:00+00:00".to_string()),
            ..Task::default()
        }
    }

    #[test]
    fn test_collect_descendants_single_task() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("root", "Root")));
        let desc = collect_descendants("root", &graph);
        assert_eq!(desc.len(), 1);
        assert_eq!(desc[0].id, "root");
    }

    #[test]
    fn test_collect_descendants_chain() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("root", "Root")));
        let mut b = make_done_task("child", "Child");
        b.after = vec!["root".to_string()];
        graph.add_node(Node::Task(b));
        let mut c = make_done_task("grandchild", "Grandchild");
        c.after = vec!["child".to_string()];
        graph.add_node(Node::Task(c));

        let desc = collect_descendants("root", &graph);
        assert_eq!(desc.len(), 3);
    }

    #[test]
    fn test_collect_descendants_diamond() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("root", "Root")));
        let mut b = make_done_task("left", "Left");
        b.after = vec!["root".to_string()];
        let mut c = make_done_task("right", "Right");
        c.after = vec!["root".to_string()];
        let mut d = make_done_task("merge", "Merge");
        d.after = vec!["left".to_string(), "right".to_string()];
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let desc = collect_descendants("root", &graph);
        assert_eq!(desc.len(), 4);
    }

    #[test]
    fn test_collect_descendants_skips_internal_tasks() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("root", "Root")));
        let mut assign = make_done_task("assign-root", "Assign agent");
        assign.tags = vec!["assignment".to_string(), "agency".to_string()];
        assign.after = vec!["root".to_string()];
        graph.add_node(Node::Task(assign));

        let desc = collect_descendants("root", &graph);
        assert_eq!(desc.len(), 1);
        assert_eq!(desc[0].id, "root");
    }

    #[test]
    fn test_detect_interventions_finds_manual_fail() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&dir).unwrap();

        provenance::record(
            &dir,
            "fail",
            Some("task-1"),
            None, // no actor = human
            serde_json::json!({"reason": "wrong approach"}),
            provenance::DEFAULT_ROTATION_THRESHOLD,
        ).unwrap();

        let task_ids: HashSet<&str> = ["task-1"].into_iter().collect();
        let interventions = detect_interventions(&dir, &task_ids);
        assert_eq!(interventions.len(), 1);
        assert_eq!(interventions[0].kind, "fail");
        assert!(interventions[0].detail.contains("wrong approach"));
    }

    #[test]
    fn test_detect_interventions_skips_agent_ops() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&dir).unwrap();

        provenance::record(
            &dir,
            "fail",
            Some("task-1"),
            Some("agent-42"),
            serde_json::json!({"reason": "compile error"}),
            provenance::DEFAULT_ROTATION_THRESHOLD,
        ).unwrap();

        let task_ids: HashSet<&str> = ["task-1"].into_iter().collect();
        let interventions = detect_interventions(&dir, &task_ids);
        assert_eq!(interventions.len(), 0);
    }

    #[test]
    fn test_compute_wall_clock() {
        let t1 = Task {
            started_at: Some("2026-02-20T10:00:00+00:00".to_string()),
            completed_at: Some("2026-02-20T10:05:00+00:00".to_string()),
            ..make_task("t1", "T1")
        };
        let t2 = Task {
            started_at: Some("2026-02-20T10:02:00+00:00".to_string()),
            completed_at: Some("2026-02-20T10:10:00+00:00".to_string()),
            ..make_task("t2", "T2")
        };

        let wc = compute_wall_clock(&[&t1, &t2]);
        assert_eq!(wc, Some(600)); // 10 minutes from earliest start to latest end
    }

    #[test]
    fn test_run_recursive_basic() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("root", "Root task")));
        let mut child = make_done_task("child", "Child task");
        child.after = vec!["root".to_string()];
        child.assigned = Some("agent-1".to_string());
        child.artifacts = vec!["output.txt".to_string()];
        graph.add_node(Node::Task(child));
        setup_graph(&dir, &graph);

        // Should not panic
        let result = run_recursive(&dir, "root", false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_recursive_json() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("root", "Root task")));
        let mut child = make_done_task("child", "Child task");
        child.after = vec!["root".to_string()];
        graph.add_node(Node::Task(child));
        setup_graph(&dir, &graph);

        let result = run_recursive(&dir, "root", false, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_recursive_timeline() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let mut root = make_done_task("root", "Root");
        root.started_at = Some("2026-02-20T10:00:00+00:00".to_string());
        root.completed_at = Some("2026-02-20T10:01:00+00:00".to_string());
        graph.add_node(Node::Task(root));
        let mut child1 = make_done_task("c1", "Child 1");
        child1.after = vec!["root".to_string()];
        child1.started_at = Some("2026-02-20T10:01:00+00:00".to_string());
        child1.completed_at = Some("2026-02-20T10:03:00+00:00".to_string());
        graph.add_node(Node::Task(child1));
        let mut child2 = make_done_task("c2", "Child 2");
        child2.after = vec!["root".to_string()];
        child2.started_at = Some("2026-02-20T10:01:00+00:00".to_string());
        child2.completed_at = Some("2026-02-20T10:04:00+00:00".to_string());
        graph.add_node(Node::Task(child2));
        setup_graph(&dir, &graph);

        let result = run_recursive(&dir, "root", true, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_recursive_nonexistent_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test")));
        setup_graph(&dir, &graph);

        let result = run_recursive(&dir, "nonexistent", false, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_render_lanes_basic() {
        let lanes = vec!["t1".to_string(), "t2".to_string(), String::new()];
        let result = render_lanes(&lanes, Some(0), "▶", false);
        assert_eq!(result, "▶│");
    }

    #[test]
    fn test_render_lanes_empty() {
        let lanes: Vec<String> = vec![];
        let result = render_lanes(&lanes, None, "▶", false);
        assert_eq!(result, "");
    }

    #[test]
    fn test_task_duration_secs() {
        let task = Task {
            started_at: Some("2026-02-20T10:00:00+00:00".to_string()),
            completed_at: Some("2026-02-20T10:05:00+00:00".to_string()),
            ..make_task("t1", "Test")
        };
        assert_eq!(task_duration_secs(&task), Some(300));
    }

    #[test]
    fn test_task_duration_secs_no_timestamps() {
        let task = make_task("t1", "Test");
        assert_eq!(task_duration_secs(&task), None);
    }

    // ── Graph visualization tests ──

    #[test]
    fn test_run_graph_basic() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("root", "Root")));
        let mut child = make_done_task("child", "Child");
        child.after = vec!["root".to_string()];
        graph.add_node(Node::Task(child));
        setup_graph(&dir, &graph);

        let result = run_graph(&dir, "root");
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_graph_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test")));
        setup_graph(&dir, &graph);

        let result = run_graph(&dir, "nonexistent");
        assert!(result.is_err());
    }

    // ── Temporal reconstruction tests ──

    #[test]
    fn test_reconstruct_temporal_empty() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&dir).unwrap();

        let subgraph: HashSet<&str> = ["t1"].into_iter().collect();
        let snapshots = reconstruct_temporal(&dir, &subgraph).unwrap();
        assert!(snapshots.is_empty());
    }

    #[test]
    fn test_reconstruct_temporal_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&dir).unwrap();

        // Record a task lifecycle
        provenance::record(
            &dir, "add_task", Some("t1"), None,
            serde_json::json!({"title": "Test"}),
            provenance::DEFAULT_ROTATION_THRESHOLD,
        ).unwrap();
        provenance::record(
            &dir, "claim", Some("t1"), Some("agent-1"),
            serde_json::Value::Null,
            provenance::DEFAULT_ROTATION_THRESHOLD,
        ).unwrap();
        provenance::record(
            &dir, "done", Some("t1"), None,
            serde_json::Value::Null,
            provenance::DEFAULT_ROTATION_THRESHOLD,
        ).unwrap();

        let subgraph: HashSet<&str> = ["t1"].into_iter().collect();
        let snapshots = reconstruct_temporal(&dir, &subgraph).unwrap();

        assert_eq!(snapshots.len(), 3);
        // After add_task: Open
        assert_eq!(snapshots[0].statuses["t1"], Status::Open);
        // After claim: InProgress
        assert_eq!(snapshots[1].statuses["t1"], Status::InProgress);
        assert_eq!(snapshots[1].assignments["t1"], "agent-1");
        // After done: Done
        assert_eq!(snapshots[2].statuses["t1"], Status::Done);
    }

    #[test]
    fn test_reconstruct_temporal_filters_subgraph() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&dir).unwrap();

        // Record ops for two tasks
        provenance::record(
            &dir, "add_task", Some("t1"), None,
            serde_json::json!({"title": "T1"}),
            provenance::DEFAULT_ROTATION_THRESHOLD,
        ).unwrap();
        provenance::record(
            &dir, "add_task", Some("t2"), None,
            serde_json::json!({"title": "T2"}),
            provenance::DEFAULT_ROTATION_THRESHOLD,
        ).unwrap();

        // Only request t1
        let subgraph: HashSet<&str> = ["t1"].into_iter().collect();
        let snapshots = reconstruct_temporal(&dir, &subgraph).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert!(snapshots[0].statuses.contains_key("t1"));
        assert!(!snapshots[0].statuses.contains_key("t2"));
    }
}
