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

fn last_batch_path(dir: &Path) -> std::path::PathBuf {
    dir.join("archive-last-batch.json")
}

/// Store batch metadata so we can undo the last archive operation
fn save_batch_metadata(dir: &Path, task_ids: &[String]) -> Result<()> {
    let metadata = serde_json::json!({
        "timestamp": Utc::now().to_rfc3339(),
        "task_ids": task_ids,
    });
    let path = last_batch_path(dir);
    let content = serde_json::to_string_pretty(&metadata)?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write batch metadata to {:?}", path))?;
    Ok(())
}

/// Load the last batch metadata for undo
fn load_batch_metadata(dir: &Path) -> Result<Vec<String>> {
    let path = last_batch_path(dir);
    if !path.exists() {
        anyhow::bail!("No archive batch to undo. No previous archive operation found.");
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read batch metadata from {:?}", path))?;
    let metadata: serde_json::Value =
        serde_json::from_str(&content).with_context(|| "Failed to parse batch metadata")?;
    let task_ids = metadata["task_ids"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid batch metadata: missing task_ids"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    Ok(task_ids)
}

/// Parse a duration string like "30d", "7d", "1w" into a chrono Duration
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("Empty duration string");
    }

    let (num_str, unit) = if let Some(n) = s.strip_suffix('d') {
        (n, 'd')
    } else if let Some(n) = s.strip_suffix('w') {
        (n, 'w')
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 'h')
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
        if let Some(completed_at) = &task.completed_at
            && let Ok(completed) = DateTime::parse_from_rfc3339(completed_at)
        {
            let age = Utc::now().signed_duration_since(completed);
            return age > *min_age;
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
            format!("Failed to parse archive line {}: {}", line_num + 1, trimmed)
        })?;
        if let Node::Task(task) = node {
            tasks.push(task);
        }
    }

    Ok(tasks)
}

/// Rewrite the archive file, excluding a specific task by ID.
fn remove_from_archive(archive_path: &Path, task_id: &str) -> Result<()> {
    let tasks = load_archive(archive_path)?;
    // Rewrite the file with all tasks except the one being restored
    let file = File::create(archive_path).with_context(|| {
        format!(
            "Failed to open archive file for writing: {:?}",
            archive_path
        )
    })?;
    let mut writer = std::io::BufWriter::new(file);
    for task in &tasks {
        if task.id != task_id {
            let node = Node::Task(task.clone());
            let json = serde_json::to_string(&node)
                .with_context(|| format!("Failed to serialize task: {}", task.id))?;
            writeln!(writer, "{}", json)?;
        }
    }
    Ok(())
}

/// Search archived tasks by title, description, and tags.
pub fn search(dir: &Path, query: &str, limit: usize, json: bool) -> Result<()> {
    let arch_path = archive_path(dir);
    let tasks = load_archive(&arch_path)?;
    let query_lower = query.to_lowercase();

    let matches: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            t.title.to_lowercase().contains(&query_lower)
                || t.description
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&query_lower)
                || t.tags
                    .iter()
                    .any(|tag| tag.to_lowercase().contains(&query_lower))
        })
        .take(limit)
        .collect();

    if json {
        let items: Vec<serde_json::Value> = matches
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "title": t.title,
                    "completed_at": t.completed_at,
                    "tags": t.tags,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if matches.is_empty() {
        println!("No archived tasks matching '{}'.", query);
    } else {
        println!(
            "Archived tasks matching '{}' ({} result{}):",
            query,
            matches.len(),
            if matches.len() == 1 { "" } else { "s" }
        );
        for task in &matches {
            let completed = task.completed_at.as_deref().unwrap_or("unknown");
            let tags = if task.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", task.tags.join(", "))
            };
            println!(
                "  {} - {} (completed: {}){}",
                task.id, task.title, completed, tags
            );
        }
    }

    Ok(())
}

/// Restore an archived task back into the active graph.
pub fn restore(dir: &Path, task_id: &str, reopen: bool) -> Result<()> {
    let path = graph_path(dir);
    let arch_path = archive_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let tasks = load_archive(&arch_path)?;
    let task = tasks
        .iter()
        .find(|t| t.id == task_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found in archive", task_id))?;

    let mut restored_task = task;
    if reopen {
        restored_task.status = Status::Open;
        restored_task.completed_at = None;
        restored_task.assigned = None;
    }

    // Add back to graph
    let mut graph = load_graph(&path).context("Failed to load graph")?;
    if graph.get_task(&restored_task.id).is_some() {
        anyhow::bail!(
            "Task '{}' already exists in the active graph",
            restored_task.id
        );
    }
    graph.add_node(Node::Task(restored_task.clone()));
    save_graph(&graph, &path).context("Failed to save graph")?;

    // Remove from archive
    remove_from_archive(&arch_path, task_id)?;

    super::notify_graph_changed(dir);

    let status = if reopen { "open" } else { "done" };
    println!(
        "Restored task '{}' ({}) to active graph with status '{}'",
        task_id, restored_task.title, status
    );

    Ok(())
}

/// Undo the last archive operation by restoring all tasks from the last batch.
pub fn undo(dir: &Path) -> Result<()> {
    let path = graph_path(dir);
    let arch_path = archive_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let task_ids = load_batch_metadata(dir)?;
    if task_ids.is_empty() {
        anyhow::bail!("No tasks in the last archive batch to restore.");
    }

    let archived_tasks = load_archive(&arch_path)?;
    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let mut restored_count = 0;
    let mut skipped = Vec::new();

    for task_id in &task_ids {
        if let Some(task) = archived_tasks.iter().find(|t| &t.id == task_id) {
            if graph.get_task(task_id).is_some() {
                skipped.push(task_id.clone());
                continue;
            }
            graph.add_node(Node::Task(task.clone()));
            remove_from_archive(&arch_path, task_id)?;
            restored_count += 1;
        } else {
            skipped.push(task_id.clone());
        }
    }

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Remove the batch metadata file since undo is done
    let batch_path = last_batch_path(dir);
    if batch_path.exists() {
        std::fs::remove_file(&batch_path).ok();
    }

    println!("Restored {} tasks from last archive batch.", restored_count);
    if !skipped.is_empty() {
        println!(
            "Skipped {} tasks (not found in archive or already in graph): {}",
            skipped.len(),
            skipped.join(", ")
        );
    }

    Ok(())
}

pub fn run(
    dir: &Path,
    dry_run: bool,
    older: Option<&str>,
    list: bool,
    yes: bool,
    ids: &[String],
    json: bool,
) -> Result<()> {
    let path = graph_path(dir);
    let arch_path = archive_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Handle --list: show archived tasks
    if list {
        let tasks = load_archive(&arch_path)?;
        if json {
            let items: Vec<serde_json::Value> = tasks
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.id,
                        "title": t.title,
                        "completed_at": t.completed_at,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&items)?);
        } else if tasks.is_empty() {
            println!("No archived tasks.");
        } else {
            println!("Archived tasks ({}):", tasks.len());
            for task in &tasks {
                let completed = task.completed_at.as_deref().unwrap_or("unknown");
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
    let tasks_to_archive: Vec<Task> = if !ids.is_empty() {
        // Archive specific tasks by ID
        let mut tasks = Vec::new();
        for id in ids {
            if let Some(task) = graph.get_task(id) {
                if task.status != Status::Done {
                    anyhow::bail!(
                        "Task '{}' has status '{}' — only done tasks can be archived. \
                         Use `wg done {}` first.",
                        id,
                        task.status,
                        id
                    );
                }
                tasks.push(task.clone());
            } else {
                anyhow::bail!("Task '{}' not found in the graph.", id);
            }
        }
        tasks
    } else {
        graph
            .tasks()
            .filter(|t| should_archive(t, older_duration.as_ref()))
            .cloned()
            .collect()
    };

    if tasks_to_archive.is_empty() {
        println!("No tasks to archive.");
        return Ok(());
    }

    if dry_run {
        println!("Would archive {} tasks:", tasks_to_archive.len());
        for task in &tasks_to_archive {
            let completed = task.completed_at.as_deref().unwrap_or("unknown");
            println!("  {} - {} (completed: {})", task.id, task.title, completed);
        }
        return Ok(());
    }

    // For bulk operations (no explicit IDs), require --yes confirmation
    let is_bulk = ids.is_empty();
    if is_bulk && !yes {
        println!("Would archive {} tasks:", tasks_to_archive.len());
        for task in &tasks_to_archive {
            let completed = task.completed_at.as_deref().unwrap_or("unknown");
            println!("  {} - {} (completed: {})", task.id, task.title, completed);
        }
        println!();
        anyhow::bail!(
            "Use --yes to confirm, or specify task IDs explicitly: wg archive <id1> <id2> ..."
        );
    }

    // Perform the archive operation
    // 1. Append tasks to archive file
    append_to_archive(&tasks_to_archive, &arch_path)?;

    // 2. Save batch metadata for undo
    let archived_ids: Vec<String> = tasks_to_archive.iter().map(|t| t.id.clone()).collect();
    save_batch_metadata(dir, &archived_ids)?;

    // 3. Remove archived tasks from the main graph
    let mut modified_graph = graph;
    for task in &tasks_to_archive {
        modified_graph.remove_node(&task.id);
    }

    // 4. Save the modified graph
    save_graph(&modified_graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let task_ids: Vec<&str> = tasks_to_archive.iter().map(|t| t.id.as_str()).collect();
    let _ = workgraph::provenance::record(
        dir,
        "archive",
        None,
        None,
        serde_json::json!({ "task_ids": task_ids }),
        config.log.rotation_threshold,
    );

    println!(
        "Archived {} tasks. Use `wg archive --undo` to reverse.",
        tasks_to_archive.len(),
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
            status,
            completed_at: completed_at.map(String::from),
            ..Task::default()
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
        run(wg_dir, true, None, false, false, &[], false).unwrap();

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

        // Run archive (with --yes to skip confirmation)
        run(wg_dir, false, None, false, true, &[], false).unwrap();

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
        run(wg_dir, false, None, true, false, &[], false).unwrap();
    }

    #[test]
    fn test_run_list_json() {
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
        let tasks = vec![
            make_task(
                "t1",
                "First Archived",
                Status::Done,
                Some("2024-01-01T00:00:00Z"),
            ),
            make_task(
                "t2",
                "Second Archived",
                Status::Done,
                Some("2024-02-15T12:00:00Z"),
            ),
        ];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Run list with json=true (output goes to stdout, just verify no error)
        run(wg_dir, false, None, true, false, &[], true).unwrap();
    }

    fn make_task_with_tags(
        id: &str,
        title: &str,
        status: Status,
        completed_at: Option<&str>,
        description: Option<&str>,
        tags: Vec<&str>,
    ) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            completed_at: completed_at.map(String::from),
            description: description.map(String::from),
            tags: tags.into_iter().map(String::from).collect(),
            ..Task::default()
        }
    }

    #[test]
    fn test_search_by_title() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let arch_path = wg_dir.join("archive.jsonl");
        let tasks = vec![
            make_task(
                "t1",
                "Implement login feature",
                Status::Done,
                Some("2024-01-01T00:00:00Z"),
            ),
            make_task(
                "t2",
                "Fix database bug",
                Status::Done,
                Some("2024-01-02T00:00:00Z"),
            ),
            make_task(
                "t3",
                "Login page styling",
                Status::Done,
                Some("2024-01-03T00:00:00Z"),
            ),
        ];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Search should find tasks matching by title
        search(wg_dir, "login", 20, false).unwrap();

        // Verify by loading and filtering manually
        let loaded = load_archive(&arch_path).unwrap();
        let matches: Vec<_> = loaded
            .iter()
            .filter(|t| t.title.to_lowercase().contains("login"))
            .collect();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].id, "t1");
        assert_eq!(matches[1].id, "t3");
    }

    #[test]
    fn test_search_by_description() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let arch_path = wg_dir.join("archive.jsonl");
        let tasks = vec![
            make_task_with_tags(
                "t1",
                "Task A",
                Status::Done,
                Some("2024-01-01T00:00:00Z"),
                Some("Contains authentication logic"),
                vec![],
            ),
            make_task_with_tags(
                "t2",
                "Task B",
                Status::Done,
                Some("2024-01-02T00:00:00Z"),
                Some("Contains database logic"),
                vec![],
            ),
        ];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Should find by description content
        search(wg_dir, "authentication", 20, false).unwrap();

        let loaded = load_archive(&arch_path).unwrap();
        let matches: Vec<_> = loaded
            .iter()
            .filter(|t| {
                t.description
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("authentication")
            })
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "t1");
    }

    #[test]
    fn test_search_by_tags() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let arch_path = wg_dir.join("archive.jsonl");
        let tasks = vec![
            make_task_with_tags(
                "t1",
                "Task A",
                Status::Done,
                Some("2024-01-01T00:00:00Z"),
                None,
                vec!["frontend", "urgent"],
            ),
            make_task_with_tags(
                "t2",
                "Task B",
                Status::Done,
                Some("2024-01-02T00:00:00Z"),
                None,
                vec!["backend"],
            ),
        ];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Search by tag
        search(wg_dir, "frontend", 20, false).unwrap();

        let loaded = load_archive(&arch_path).unwrap();
        let matches: Vec<_> = loaded
            .iter()
            .filter(|t| {
                t.tags
                    .iter()
                    .any(|tag| tag.to_lowercase().contains("frontend"))
            })
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "t1");
    }

    #[test]
    fn test_search_case_insensitive() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let arch_path = wg_dir.join("archive.jsonl");
        let tasks = vec![make_task(
            "t1",
            "IMPORTANT Feature",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Case-insensitive search
        search(wg_dir, "important", 20, false).unwrap();

        let loaded = load_archive(&arch_path).unwrap();
        let matches: Vec<_> = loaded
            .iter()
            .filter(|t| t.title.to_lowercase().contains("important"))
            .collect();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_search_with_limit() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let arch_path = wg_dir.join("archive.jsonl");
        let tasks = vec![
            make_task(
                "t1",
                "Test task one",
                Status::Done,
                Some("2024-01-01T00:00:00Z"),
            ),
            make_task(
                "t2",
                "Test task two",
                Status::Done,
                Some("2024-01-02T00:00:00Z"),
            ),
            make_task(
                "t3",
                "Test task three",
                Status::Done,
                Some("2024-01-03T00:00:00Z"),
            ),
        ];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Search with limit=1 should not error
        search(wg_dir, "test", 1, false).unwrap();
    }

    #[test]
    fn test_search_json_output() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let arch_path = wg_dir.join("archive.jsonl");
        let tasks = vec![make_task(
            "t1",
            "Test task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )];
        append_to_archive(&tasks, &arch_path).unwrap();

        // JSON output should not error
        search(wg_dir, "test", 20, true).unwrap();
    }

    #[test]
    fn test_search_no_matches() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let arch_path = wg_dir.join("archive.jsonl");
        let tasks = vec![make_task(
            "t1",
            "Some task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )];
        append_to_archive(&tasks, &arch_path).unwrap();

        // No matches
        search(wg_dir, "nonexistent", 20, false).unwrap();
    }

    #[test]
    fn test_restore_as_done() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        let arch_path = wg_dir.join("archive.jsonl");

        // Create an empty graph
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        // Create archive with a task
        let tasks = vec![
            make_task(
                "t1",
                "Archived Task",
                Status::Done,
                Some("2024-01-01T00:00:00Z"),
            ),
            make_task(
                "t2",
                "Other Archived",
                Status::Done,
                Some("2024-01-02T00:00:00Z"),
            ),
        ];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Restore t1 without --reopen
        restore(wg_dir, "t1", false).unwrap();

        // Verify task is in graph with status Done
        let graph = load_graph(&graph_file).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
        assert_eq!(task.title, "Archived Task");

        // Verify task is removed from archive
        let archived = load_archive(&arch_path).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, "t2");
    }

    #[test]
    fn test_restore_with_reopen() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        let arch_path = wg_dir.join("archive.jsonl");

        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let tasks = vec![make_task(
            "t1",
            "Archived Task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Restore with --reopen
        restore(wg_dir, "t1", true).unwrap();

        // Verify task is in graph with status Open
        let graph = load_graph(&graph_file).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.completed_at.is_none());

        // Verify archive is now empty
        let archived = load_archive(&arch_path).unwrap();
        assert!(archived.is_empty());
    }

    #[test]
    fn test_restore_nonexistent_task() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        let arch_path = wg_dir.join("archive.jsonl");

        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let tasks = vec![make_task(
            "t1",
            "Archived Task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Restoring a nonexistent task should fail
        let result = restore(wg_dir, "nonexistent", false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found in archive")
        );
    }

    #[test]
    fn test_restore_duplicate_in_graph() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        let arch_path = wg_dir.join("archive.jsonl");

        // Create graph with existing task "t1"
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "t1",
            "Active Task",
            Status::Open,
            None,
        )));
        save_graph(&graph, &graph_file).unwrap();

        // Archive also has t1
        let tasks = vec![make_task(
            "t1",
            "Archived Task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Should fail because t1 already exists in graph
        let result = restore(wg_dir, "t1", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_remove_from_archive() {
        let dir = tempdir().unwrap();
        let arch_path = dir.path().join("archive.jsonl");

        let tasks = vec![
            make_task("t1", "Task 1", Status::Done, Some("2024-01-01T00:00:00Z")),
            make_task("t2", "Task 2", Status::Done, Some("2024-01-02T00:00:00Z")),
            make_task("t3", "Task 3", Status::Done, Some("2024-01-03T00:00:00Z")),
        ];
        append_to_archive(&tasks, &arch_path).unwrap();

        // Remove t2
        remove_from_archive(&arch_path, "t2").unwrap();

        let remaining = load_archive(&arch_path).unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].id, "t1");
        assert_eq!(remaining[1].id, "t3");
    }

    #[test]
    fn test_archive_specific_ids() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");

        // Create a graph with multiple done tasks
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "t1",
            "Done Task 1",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )));
        graph.add_node(Node::Task(make_task(
            "t2",
            "Done Task 2",
            Status::Done,
            Some("2024-01-02T00:00:00Z"),
        )));
        graph.add_node(Node::Task(make_task(
            "t3",
            "Done Task 3",
            Status::Done,
            Some("2024-01-03T00:00:00Z"),
        )));
        graph.add_node(Node::Task(make_task("t4", "Open Task", Status::Open, None)));
        save_graph(&graph, &graph_file).unwrap();

        // Archive only t1 and t3 by ID
        let ids = vec!["t1".to_string(), "t3".to_string()];
        run(wg_dir, false, None, false, false, &ids, false).unwrap();

        // Verify only t1 and t3 were archived
        let loaded = load_graph(&graph_file).unwrap();
        assert!(loaded.get_task("t1").is_none());
        assert!(loaded.get_task("t2").is_some());
        assert!(loaded.get_task("t3").is_none());
        assert!(loaded.get_task("t4").is_some());

        let arch_path = wg_dir.join("archive.jsonl");
        let archived = load_archive(&arch_path).unwrap();
        assert_eq!(archived.len(), 2);
        let archived_ids: Vec<&str> = archived.iter().map(|t| t.id.as_str()).collect();
        assert!(archived_ids.contains(&"t1"));
        assert!(archived_ids.contains(&"t3"));
    }

    #[test]
    fn test_archive_specific_ids_rejects_non_done() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Open Task", Status::Open, None)));
        save_graph(&graph, &graph_file).unwrap();

        // Trying to archive a non-done task should fail
        let ids = vec!["t1".to_string()];
        let result = run(wg_dir, false, None, false, false, &ids, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("only done tasks"));
    }

    #[test]
    fn test_archive_specific_ids_rejects_missing() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        let ids = vec!["nonexistent".to_string()];
        let result = run(wg_dir, false, None, false, false, &ids, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_archive_yes_flag() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "t1",
            "Done Task",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )));
        save_graph(&graph, &graph_file).unwrap();

        // With --yes, bulk archive proceeds without error
        run(wg_dir, false, None, false, true, &[], false).unwrap();

        let loaded = load_graph(&graph_file).unwrap();
        assert!(loaded.get_task("t1").is_none());
    }

    #[test]
    fn test_archive_undo() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");

        // Create graph with two done tasks
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "t1",
            "Done Task 1",
            Status::Done,
            Some("2024-01-01T00:00:00Z"),
        )));
        graph.add_node(Node::Task(make_task(
            "t2",
            "Done Task 2",
            Status::Done,
            Some("2024-01-02T00:00:00Z"),
        )));
        graph.add_node(Node::Task(make_task("t3", "Open Task", Status::Open, None)));
        save_graph(&graph, &graph_file).unwrap();

        // Archive with --yes
        run(wg_dir, false, None, false, true, &[], false).unwrap();

        // Verify tasks are archived
        let loaded = load_graph(&graph_file).unwrap();
        assert!(loaded.get_task("t1").is_none());
        assert!(loaded.get_task("t2").is_none());
        assert!(loaded.get_task("t3").is_some());

        // Undo the archive
        undo(wg_dir).unwrap();

        // Verify tasks are restored
        let loaded = load_graph(&graph_file).unwrap();
        assert!(loaded.get_task("t1").is_some());
        assert!(loaded.get_task("t2").is_some());
        assert!(loaded.get_task("t3").is_some());

        // Verify archive is now empty for those tasks
        let arch_path = wg_dir.join("archive.jsonl");
        let archived = load_archive(&arch_path).unwrap();
        assert!(archived.is_empty());

        // Verify batch metadata file is removed
        let batch_path = wg_dir.join("archive-last-batch.json");
        assert!(!batch_path.exists());
    }

    #[test]
    fn test_archive_undo_no_batch() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();
        let graph_file = wg_dir.join("graph.jsonl");
        save_graph(&WorkGraph::new(), &graph_file).unwrap();

        // Undo without a previous archive should fail
        let result = undo(wg_dir);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No archive batch to undo")
        );
    }

    #[test]
    fn test_archive_batch_metadata_roundtrip() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();

        let ids = vec!["t1".to_string(), "t2".to_string(), "t3".to_string()];
        save_batch_metadata(wg_dir, &ids).unwrap();

        let loaded = load_batch_metadata(wg_dir).unwrap();
        assert_eq!(loaded, ids);
    }
}
