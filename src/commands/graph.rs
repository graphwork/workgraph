use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use workgraph::graph::{Node, Status, Task};
use workgraph::parser::load_graph;

use super::graph_path;

fn archive_path(dir: &Path) -> std::path::PathBuf {
    dir.join("archive.jsonl")
}

/// Load archived tasks from the archive file
fn load_archive(archive_path: &Path) -> Result<Vec<Task>> {
    if !archive_path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(archive_path)
        .with_context(|| format!("Failed to open archive file: {:?}", archive_path))?;
    let reader = BufReader::new(file);
    let mut tasks = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let node: Node = serde_json::from_str(trimmed).with_context(|| {
            format!("Failed to parse archive line {}: {}", line_num + 1, trimmed)
        })?;
        if let Node::Task(task) = node {
            tasks.push(task);
        }
    }

    Ok(tasks)
}

/// Parse a date string (YYYY-MM-DD) to DateTime<Utc>
fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("Invalid date format '{}', expected YYYY-MM-DD", s))?;
    Ok(date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always valid")
        .and_utc())
}

/// Check if a task falls within the date range
fn in_date_range(
    task: &Task,
    since: Option<&DateTime<Utc>>,
    until: Option<&DateTime<Utc>>,
) -> bool {
    let completed = task.completed_at.as_ref().and_then(|s| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    });

    match (completed, since, until) {
        (Some(completed), Some(since), Some(until)) => completed >= *since && completed <= *until,
        (Some(completed), Some(since), None) => completed >= *since,
        (Some(completed), None, Some(until)) => completed <= *until,
        (None, Some(_), _) | (None, _, Some(_)) => false, // No timestamp, exclude if filtering
        (_, None, None) => true,                          // No filter, include
    }
}

/// Get the color for a task based on its status
fn status_color(status: &Status, is_archived: bool) -> &'static str {
    if is_archived {
        return "lightgray";
    }
    match status {
        Status::Done => "palegreen",
        Status::InProgress => "coral", // Red/orange - active work, draws attention
        Status::Blocked => "khaki",    // Yellow - waiting
        Status::Open => "white",       // Ready to pick up
        Status::Failed => "salmon",    // Red-ish - needs attention
        Status::Abandoned => "lightgray", // Grayed out
    }
}

/// Get the border style for special states
fn status_style(status: &Status, is_archived: bool) -> &'static str {
    if is_archived {
        return "filled,dashed";
    }
    match status {
        Status::InProgress => "filled,bold", // Bold border for active work
        Status::Failed => "filled,bold",     // Bold for attention
        _ => "filled",
    }
}

pub fn run(
    dir: &Path,
    include_archive: bool,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Parse date filters
    let since_dt = since.map(parse_date).transpose()?;
    let until_dt = until.map(parse_date).transpose()?;

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Collect tasks from main graph
    let mut all_tasks: Vec<(Task, bool)> = graph
        .tasks()
        .filter(|t| in_date_range(t, since_dt.as_ref(), until_dt.as_ref()))
        .map(|t| (t.clone(), false))
        .collect();

    // Load archived tasks if requested
    if include_archive {
        let arch_path = archive_path(dir);
        let archived = load_archive(&arch_path)?;
        for task in archived {
            if in_date_range(&task, since_dt.as_ref(), until_dt.as_ref()) {
                all_tasks.push((task, true));
            }
        }
    }

    // Print DOT format for visualization
    println!("digraph workgraph {{");
    println!("  rankdir=LR;");
    println!("  node [shape=box];");

    // Add legend
    println!();
    println!("  // Legend");
    println!("  subgraph cluster_legend {{");
    println!("    label=\"Legend\";");
    println!("    style=dashed;");
    println!("    fontsize=10;");
    println!("    legend_open [label=\"Open\", style=filled, fillcolor=white];");
    println!(
        "    legend_progress [label=\"In Progress\", style=\"filled,bold\", fillcolor=coral];"
    );
    println!("    legend_blocked [label=\"Blocked\", style=filled, fillcolor=khaki];");
    println!("    legend_done [label=\"Done\", style=filled, fillcolor=palegreen];");
    println!("    legend_failed [label=\"Failed\", style=\"filled,bold\", fillcolor=salmon];");
    if include_archive {
        println!(
            "    legend_archived [label=\"Archived\", style=\"filled,dashed\", fillcolor=lightgray];"
        );
    }
    println!(
        "    legend_open -> legend_progress -> legend_blocked -> legend_done -> legend_failed [style=invis];"
    );
    println!("  }}");
    println!();

    // Print task nodes
    for (task, is_archived) in &all_tasks {
        let color = status_color(&task.status, *is_archived);
        let style = status_style(&task.status, *is_archived);

        // Add assigned actor to label if claimed
        let label = if let Some(ref assigned) = task.assigned {
            format!("{}\\n{}\\n[{}]", task.id, task.title, assigned)
        } else {
            format!("{}\\n{}", task.id, task.title)
        };

        println!(
            "  \"{}\" [label=\"{}\", style=\"{}\", fillcolor={}];",
            task.id, label, style, color
        );
    }

    // Print resources
    for resource in graph.resources() {
        let name = resource.name.as_deref().unwrap_or(&resource.id);
        println!(
            "  \"{}\" [label=\"{}\", shape=diamond, style=filled, fillcolor=lightyellow];",
            resource.id, name
        );
    }

    println!();

    // Print edges (only for tasks in our filtered set)
    let task_ids: std::collections::HashSet<_> = all_tasks.iter().map(|(t, _)| &t.id).collect();

    for (task, _) in &all_tasks {
        for blocked in &task.blocked_by {
            // Only draw edge if both tasks are in our set
            if task_ids.contains(blocked) {
                println!("  \"{}\" -> \"{}\" [label=\"blocks\"];", blocked, task.id);
            }
        }
        if let Some(ref assigned) = task.assigned {
            println!(
                "  \"{}\" -> \"{}\" [style=dashed, label=\"assigned\"];",
                task.id, assigned
            );
        }
        for req in &task.requires {
            if task_ids.contains(req) {
                println!(
                    "  \"{}\" -> \"{}\" [style=dotted, label=\"requires\"];",
                    task.id, req
                );
            }
        }
    }

    println!("}}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Resource, Status, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
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
            paused: false,
        }
    }

    fn setup_graph(dir: &Path, graph: &WorkGraph) {
        std::fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        save_graph(graph, &path).unwrap();
    }

    // --- status_color tests ---

    #[test]
    fn test_status_color_by_status() {
        assert_eq!(status_color(&Status::Open, false), "white");
        assert_eq!(status_color(&Status::InProgress, false), "coral");
        assert_eq!(status_color(&Status::Done, false), "palegreen");
        assert_eq!(status_color(&Status::Blocked, false), "khaki");
        assert_eq!(status_color(&Status::Failed, false), "salmon");
        assert_eq!(status_color(&Status::Abandoned, false), "lightgray");
    }

    #[test]
    fn test_status_color_archived_always_lightgray() {
        assert_eq!(status_color(&Status::Open, true), "lightgray");
        assert_eq!(status_color(&Status::Done, true), "lightgray");
        assert_eq!(status_color(&Status::InProgress, true), "lightgray");
    }

    // --- status_style tests ---

    #[test]
    fn test_status_style_active_states_are_bold() {
        assert_eq!(status_style(&Status::InProgress, false), "filled,bold");
        assert_eq!(status_style(&Status::Failed, false), "filled,bold");
    }

    #[test]
    fn test_status_style_normal_states_are_filled() {
        assert_eq!(status_style(&Status::Open, false), "filled");
        assert_eq!(status_style(&Status::Done, false), "filled");
        assert_eq!(status_style(&Status::Blocked, false), "filled");
        assert_eq!(status_style(&Status::Abandoned, false), "filled");
    }

    #[test]
    fn test_status_style_archived_is_dashed() {
        assert_eq!(status_style(&Status::Open, true), "filled,dashed");
        assert_eq!(status_style(&Status::InProgress, true), "filled,dashed");
    }

    // --- parse_date tests ---

    #[test]
    fn test_parse_date_valid() {
        let dt = parse_date("2024-06-15").unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2024-06-15");
    }

    #[test]
    fn test_parse_date_invalid() {
        assert!(parse_date("not-a-date").is_err());
        assert!(parse_date("2024/06/15").is_err());
        assert!(parse_date("06-15-2024").is_err());
    }

    // --- in_date_range tests ---

    #[test]
    fn test_in_date_range_no_filters_always_included() {
        let task = make_task("t1", "T");
        assert!(in_date_range(&task, None, None));
    }

    #[test]
    fn test_in_date_range_no_completed_at_excluded_when_filtering() {
        let task = make_task("t1", "T");
        let since = parse_date("2024-01-01").unwrap();
        assert!(!in_date_range(&task, Some(&since), None));
    }

    #[test]
    fn test_in_date_range_since_filter() {
        let mut task = make_task("t1", "T");
        task.completed_at = Some("2024-06-15T10:00:00+00:00".to_string());

        let since_before = parse_date("2024-06-01").unwrap();
        assert!(in_date_range(&task, Some(&since_before), None));

        let since_after = parse_date("2024-07-01").unwrap();
        assert!(!in_date_range(&task, Some(&since_after), None));
    }

    #[test]
    fn test_in_date_range_until_filter() {
        let mut task = make_task("t1", "T");
        task.completed_at = Some("2024-06-15T10:00:00+00:00".to_string());

        let until_after = parse_date("2024-07-01").unwrap();
        assert!(in_date_range(&task, None, Some(&until_after)));

        let until_before = parse_date("2024-06-01").unwrap();
        assert!(!in_date_range(&task, None, Some(&until_before)));
    }

    #[test]
    fn test_in_date_range_both_filters() {
        let mut task = make_task("t1", "T");
        task.completed_at = Some("2024-06-15T10:00:00+00:00".to_string());

        let since = parse_date("2024-06-01").unwrap();
        let until = parse_date("2024-07-01").unwrap();
        assert!(in_date_range(&task, Some(&since), Some(&until)));

        let since_late = parse_date("2024-07-01").unwrap();
        assert!(!in_date_range(&task, Some(&since_late), Some(&until)));
    }

    // --- run / DOT output tests ---

    #[test]
    fn test_run_not_initialized() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        let result = run(&dir, false, None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_run_basic_dot_structure() {
        // Verifies run succeeds and doesn't error - actual DOT output goes to stdout
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "First task")));
        graph.add_node(Node::Task(make_task("t2", "Second task")));
        setup_graph(&dir, &graph);

        let result = run(&dir, false, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_with_edges() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "First");
        let mut t2 = make_task("t2", "Second");
        t2.blocked_by = vec!["t1".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        setup_graph(&dir, &graph);

        let result = run(&dir, false, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_with_resources() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task")));
        graph.add_node(Node::Resource(Resource {
            id: "gpu".to_string(),
            name: Some("GPU Cluster".to_string()),
            resource_type: None,
            available: Some(4.0),
            unit: Some("cards".to_string()),
        }));
        setup_graph(&dir, &graph);

        let result = run(&dir, false, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_with_date_filters() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Old task");
        t1.status = Status::Done;
        t1.completed_at = Some("2024-01-15T10:00:00+00:00".to_string());
        let mut t2 = make_task("t2", "New task");
        t2.status = Status::Done;
        t2.completed_at = Some("2024-06-15T10:00:00+00:00".to_string());
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        setup_graph(&dir, &graph);

        // Both should pass with --since filtering
        let result = run(&dir, false, Some("2024-06-01"), None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_include_archive_no_archive_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task")));
        setup_graph(&dir, &graph);

        // include_archive=true but no archive.jsonl exists — should still succeed
        let result = run(&dir, true, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_include_archive_with_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Active")));
        setup_graph(&dir, &graph);

        // Create an archive file with a task
        let archived_task = serde_json::json!({
            "kind": "task",
            "id": "old-t1",
            "title": "Archived task",
            "status": "done",
            "completed_at": "2024-03-01T00:00:00+00:00"
        });
        let arch = archive_path(&dir);
        std::fs::write(&arch, format!("{}\n", archived_task)).unwrap();

        let result = run(&dir, true, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_invalid_date_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task")));
        setup_graph(&dir, &graph);

        let result = run(&dir, false, Some("bad-date"), None);
        assert!(result.is_err());
    }

    // --- load_archive tests ---

    #[test]
    fn test_load_archive_missing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent.jsonl");
        let tasks = load_archive(&path).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn test_load_archive_with_tasks() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("archive.jsonl");
        let task_json = serde_json::json!({
            "kind": "task",
            "id": "archived-1",
            "title": "Archived",
            "status": "done"
        });
        std::fs::write(&path, format!("{}\n", task_json)).unwrap();

        let tasks = load_archive(&path).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "archived-1");
    }

    #[test]
    fn test_load_archive_skips_empty_and_comments() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("archive.jsonl");
        let task_json = serde_json::json!({
            "kind": "task",
            "id": "a1",
            "title": "A",
            "status": "done"
        });
        let content = format!("# comment\n\n{}\n\n", task_json);
        std::fs::write(&path, content).unwrap();

        let tasks = load_archive(&path).unwrap();
        assert_eq!(tasks.len(), 1);
    }
}
