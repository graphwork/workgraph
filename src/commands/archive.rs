use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use workgraph::graph::{Node, Status, Task};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

fn archive_path(dir: &Path) -> std::path::PathBuf {
    dir.join("archive.jsonl")
}

/// Parse a duration string like "30d", "7d", "1w" into a chrono Duration
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("Empty duration string");
    }

    let (num_str, unit) = if s.ends_with('d') {
        (&s[..s.len() - 1], 'd')
    } else if s.ends_with('w') {
        (&s[..s.len() - 1], 'w')
    } else if s.ends_with('h') {
        (&s[..s.len() - 1], 'h')
    } else {
        // Default to days if no unit specified
        (s, 'd')
    };

    let num: i64 = num_str
        .parse()
        .with_context(|| format!("Invalid number in duration: '{}'", num_str))?;

    match unit {
        'd' => Ok(Duration::days(num)),
        'w' => Ok(Duration::weeks(num)),
        'h' => Ok(Duration::hours(num)),
        _ => anyhow::bail!("Unknown duration unit: {}", unit),
    }
}

/// Check if a task should be archived based on the --older filter
fn should_archive(task: &Task, older_than: Option<&Duration>) -> bool {
    if task.status != Status::Done {
        return false;
    }

    if let Some(min_age) = older_than {
        if let Some(completed_at) = &task.completed_at {
            if let Ok(completed) = DateTime::parse_from_rfc3339(completed_at) {
                let age = Utc::now().signed_duration_since(completed);
                return age > *min_age;
            }
        }
        // If no completion timestamp or can't parse, don't archive with --older filter
        return false;
    }

    true
}

/// Append tasks to the archive file
fn append_to_archive(tasks: &[Task], archive_path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(archive_path)
        .with_context(|| format!("Failed to open archive file: {:?}", archive_path))?;

    for task in tasks {
        let node = Node::Task(task.clone());
        let json = serde_json::to_string(&node)
            .with_context(|| format!("Failed to serialize task: {}", task.id))?;
        writeln!(file, "{}", json)?;
    }

    Ok(())
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
            format!(
                "Failed to parse archive line {}: {}",
                line_num + 1,
                trimmed
            )
        })?;
        if let Node::Task(task) = node {
            tasks.push(task);
        }
    }

    Ok(tasks)
}

pub fn run(dir: &Path, dry_run: bool, older: Option<&str>, list: bool) -> Result<()> {
    let path = graph_path(dir);
    let arch_path = archive_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Handle --list: show archived tasks
    if list {
        let tasks = load_archive(&arch_path)?;
        if tasks.is_empty() {
            println!("No archived tasks.");
        } else {
            println!("Archived tasks ({}):", tasks.len());
            for task in &tasks {
                let completed = task
                    .completed_at
                    .as_deref()
                    .unwrap_or("unknown");
                println!("  {} - {} (completed: {})", task.id, task.title, completed);
            }
        }
        return Ok(());
    }

    // Parse --older duration if provided
    let older_duration = if let Some(older_str) = older {
        Some(parse_duration(older_str)?)
    } else {
        None
    };

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Find tasks to archive
    let tasks_to_archive: Vec<Task> = graph
        .tasks()
        .filter(|t| should_archive(t, older_duration.as_ref()))
        .cloned()
        .collect();

    if tasks_to_archive.is_empty() {
        println!("No tasks to archive.");
        return Ok(());
    }

    if dry_run {
        println!("Would archive {} tasks:", tasks_to_archive.len());
        for task in &tasks_to_archive {
            let completed = task
                .completed_at
                .as_deref()
                .unwrap_or("unknown");
            println!("  {} - {} (completed: {})", task.id, task.title, completed);
        }
        return Ok(());
    }

    // Perform the archive operation
    // 1. Append tasks to archive file
    append_to_archive(&tasks_to_archive, &arch_path)?;

    // 2. Remove archived tasks from the main graph
    let mut modified_graph = graph.clone();
    for task in &tasks_to_archive {
        modified_graph.remove_node(&task.id);
    }

    // 3. Save the modified graph
    save_graph(&modified_graph, &path).context("Failed to save graph")?;

    println!(
        "Archived {} tasks to {:?}",
        tasks_to_archive.len(),
        arch_path
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use workgraph::graph::WorkGraph;

    fn make_task(id: &str, title: &str, status: Status, completed_at: Option<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status,
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
            completed_at: completed_at.map(String::from),
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
        }
    }

    #[test]
    fn test_parse_duration_days() {
        let d = parse_duration("30d").unwrap();
        assert_eq!(d, Duration::days(30));
    }

    #[test]
    fn test_parse_duration_weeks() {
        let d = parse_duration("2w").unwrap();
        assert_eq!(d, Duration::weeks(2));
    }

    #[test]
    fn test_parse_duration_hours() {
        let d = parse_duration("24h").unwrap();
        assert_eq!(d, Duration::hours(24));
    }

    #[test]
    fn test_parse_duration_no_unit() {
        let d = parse_duration("7").unwrap();
        assert_eq!(d, Duration::days(7));
    }

    #[test]
    fn test_should_archive_done_task() {
        let task = make_task("t1", "Test", Status::Done, None);
        assert!(should_archive(&task, None));
    }

    #[test]
    fn test_should_not_archive_open_task() {
        let task = make_task("t1", "Test", Status::Open, None);
        assert!(!should_archive(&task, None));
    }

    #[test]
    fn test_should_archive_old_task() {
        // Task completed 40 days ago
        let completed_at = (Utc::now() - Duration::days(40)).to_rfc3339();
        let task = make_task("t1", "Test", Status::Done, Some(&completed_at));
        let min_age = Duration::days(30);
        assert!(should_archive(&task, Some(&min_age)));
    }

    #[test]
    fn test_should_not_archive_recent_task() {
        // Task completed 10 days ago
        let completed_at = (Utc::now() - Duration::days(10)).to_rfc3339();
        let task = make_task("t1", "Test", Status::Done, Some(&completed_at));
        let min_age = Duration::days(30);
        assert!(!should_archive(&task, Some(&min_age)));
    }

    #[test]
    fn test_archive_roundtrip() {
        let dir = tempdir().unwrap();
        let arch_path = dir.path().join("archive.jsonl");

        let tasks = vec![
            make_task("t1", "Task 1", Status::Done, Some("2024-01-01T00:00:00Z")),
            make_task("t2", "Task 2", Status::Done, Some("2024-01-02T00:00:00Z")),
        ];

        append_to_archive(&tasks, &arch_path).unwrap();

        let loaded = load_archive(&arch_path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "t1");
        assert_eq!(loaded[1].id, "t2");
    }

    #[test]
    fn test_run_dry_run() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();

        // Create .workgraph directory structure
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");

        // Create a graph with one done task
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "t1",
            "Done Task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )));
        graph.add_node(Node::Task(make_task("t2", "Open Task", Status::Open, None)));
        save_graph(&graph, &graph_file).unwrap();

        // Run in dry-run mode
        run(wg_dir, true, None, false).unwrap();

        // Verify graph is unchanged
        let loaded = load_graph(&graph_file).unwrap();
        assert_eq!(loaded.tasks().count(), 2);

        // Verify no archive file created
        let arch_path = wg_dir.join("archive.jsonl");
        assert!(!arch_path.exists());
    }

    #[test]
    fn test_run_archive() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();

        // Create .workgraph directory structure
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");

        // Create a graph with one done task and one open task
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "t1",
            "Done Task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )));
        graph.add_node(Node::Task(make_task("t2", "Open Task", Status::Open, None)));
        save_graph(&graph, &graph_file).unwrap();

        // Run archive
        run(wg_dir, false, None, false).unwrap();

        // Verify done task removed from graph
        let loaded = load_graph(&graph_file).unwrap();
        assert_eq!(loaded.tasks().count(), 1);
        assert!(loaded.get_task("t1").is_none());
        assert!(loaded.get_task("t2").is_some());

        // Verify done task is in archive
        let arch_path = wg_dir.join("archive.jsonl");
        let archived = load_archive(&arch_path).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, "t1");
    }

    #[test]
    fn test_run_list() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();

        // Create .workgraph directory structure
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        let arch_path = wg_dir.join("archive.jsonl");

        // Create empty graph
        let graph = WorkGraph::new();
        save_graph(&graph, &graph_file).unwrap();

        // Create archive with some tasks
        let tasks = vec![make_task(
            "t1",
            "Archived Task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Run list - should not error
        run(wg_dir, false, None, true).unwrap();
    }
}
