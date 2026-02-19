use anyhow::Result;
use chrono::DateTime;
use serde::Serialize;
use std::fs;
use std::path::Path;
use workgraph::graph::Status;
use workgraph::provenance::{self, OperationEntry};

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
            if let Some(content_type) = val.get("content_block").and_then(|cb| cb.get("type")).and_then(|t| t.as_str()) {
                if content_type == "tool_use" {
                    tool_calls += 1;
                }
            }
        }
    }

    (tool_calls, turns)
}

fn format_duration(secs: i64) -> String {
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
}
