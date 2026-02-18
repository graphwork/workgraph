use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};
use workgraph::graph::LogEntry;
use workgraph::parser::save_graph;

#[cfg(test)]
use super::graph_path;

/// Add a log entry to a task
pub fn run_add(dir: &Path, id: &str, message: &str, actor: Option<&str>) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    let entry = LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message: message.to_string(),
    };

    task.log.push(entry);

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    let actor_str = actor.map(|a| format!(" ({})", a)).unwrap_or_default();
    println!("Added log entry to '{}'{}", id, actor_str);
    Ok(())
}

/// List log entries for a task
pub fn run_list(dir: &Path, id: &str, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let task = graph.get_task_or_err(id)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&task.log)?);
        return Ok(());
    }

    if task.log.is_empty() {
        println!("No log entries for task '{}'", id);
        return Ok(());
    }

    println!("Log entries for '{}' ({}):", id, task.title);
    println!();

    for entry in &task.log {
        let actor_str = entry
            .actor
            .as_ref()
            .map(|a| format!(" [{}]", a))
            .unwrap_or_default();
        println!("  {} {}", entry.timestamp, actor_str);
        println!("    {}", entry.message);
        println!();
    }

    Ok(())
}

/// Archive directory for agent conversations: .workgraph/log/agents/<task-id>/
fn agent_archive_dir(dir: &Path, task_id: &str) -> PathBuf {
    dir.join("log").join("agents").join(task_id)
}

/// Archive an agent's prompt.txt and output.log for a completed task.
///
/// Copies from .workgraph/agents/<agent-id>/{prompt.txt,output.log}
/// to .workgraph/log/agents/<task-id>/<ISO-timestamp>/{prompt.txt,output.txt}
///
/// Each retry gets its own timestamped directory, preserving full history.
pub fn archive_agent(dir: &Path, task_id: &str, agent_id: &str) -> Result<PathBuf> {
    let agent_dir = dir.join("agents").join(agent_id);
    if !agent_dir.exists() {
        anyhow::bail!("Agent directory not found: {}", agent_dir.display());
    }

    let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let archive_dir = agent_archive_dir(dir, task_id).join(&timestamp);
    fs::create_dir_all(&archive_dir).with_context(|| {
        format!(
            "Failed to create archive directory: {}",
            archive_dir.display()
        )
    })?;

    // Copy prompt.txt if it exists
    let prompt_src = agent_dir.join("prompt.txt");
    if prompt_src.exists() {
        fs::copy(&prompt_src, archive_dir.join("prompt.txt"))
            .with_context(|| format!("Failed to copy prompt.txt from {}", prompt_src.display()))?;
    }

    // Copy output.log as output.txt
    let output_src = agent_dir.join("output.log");
    if output_src.exists() {
        fs::copy(&output_src, archive_dir.join("output.txt"))
            .with_context(|| format!("Failed to copy output.log from {}", output_src.display()))?;
    }

    Ok(archive_dir)
}

/// Show archived agent prompts and outputs for a task.
///
/// Lists all archived attempts with timestamps, showing prompt and output
/// content for each.
pub fn run_agent(dir: &Path, task_id: &str, json: bool) -> Result<()> {
    let archive_base = agent_archive_dir(dir, task_id);

    if !archive_base.exists() {
        if json {
            println!("[]");
        } else {
            println!("No agent archives for task '{}'", task_id);
        }
        return Ok(());
    }

    // Collect timestamped directories, sorted chronologically
    let mut attempts: Vec<_> = fs::read_dir(&archive_base)
        .with_context(|| {
            format!(
                "Failed to read archive directory: {}",
                archive_base.display()
            )
        })?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();

    attempts.sort_by_key(|e| e.file_name());

    if attempts.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No agent archives for task '{}'", task_id);
        }
        return Ok(());
    }

    if json {
        let mut entries = Vec::new();
        for attempt in &attempts {
            let path = attempt.path();
            let timestamp = attempt.file_name().to_string_lossy().to_string();
            let prompt = fs::read_to_string(path.join("prompt.txt")).ok();
            let output = fs::read_to_string(path.join("output.txt")).ok();
            entries.push(serde_json::json!({
                "timestamp": timestamp,
                "prompt": prompt,
                "output": output,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    println!(
        "Agent archives for '{}' ({} attempt{}):",
        task_id,
        attempts.len(),
        if attempts.len() == 1 { "" } else { "s" }
    );

    for (i, attempt) in attempts.iter().enumerate() {
        let path = attempt.path();
        let timestamp = attempt.file_name().to_string_lossy().to_string();

        println!();
        println!("--- Attempt {} [{}] ---", i + 1, timestamp);

        let prompt_path = path.join("prompt.txt");
        if prompt_path.exists() {
            let prompt = fs::read_to_string(&prompt_path)
                .with_context(|| format!("Failed to read {}", prompt_path.display()))?;
            let lines: Vec<&str> = prompt.lines().collect();
            let preview = if lines.len() > 10 {
                format!(
                    "{}\n    ... ({} more lines)",
                    lines[..10].join("\n"),
                    lines.len() - 10
                )
            } else {
                prompt.clone()
            };
            println!("  Prompt ({} bytes, {} lines):", prompt.len(), lines.len());
            for line in preview.lines() {
                println!("    {}", line);
            }
        } else {
            println!("  Prompt: (none)");
        }

        let output_path = path.join("output.txt");
        if output_path.exists() {
            let output = fs::read_to_string(&output_path)
                .with_context(|| format!("Failed to read {}", output_path.display()))?;
            let lines: Vec<&str> = output.lines().collect();
            let preview = if lines.len() > 20 {
                format!(
                    "{}\n    ... ({} more lines)",
                    lines[lines.len() - 20..].join("\n"),
                    lines.len() - 20
                )
            } else {
                output.clone()
            };
            println!("  Output ({} bytes, {} lines):", output.len(), lines.len());
            for line in preview.lines() {
                println!("    {}", line);
            }
        } else {
            println!("  Output: (none)");
        }
    }

    Ok(())
}

/// Show the operations log (current + rotated compressed files).
pub fn run_operations(dir: &Path, json: bool) -> Result<()> {
    let entries = workgraph::provenance::read_all_operations(dir)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("No operations recorded.");
        return Ok(());
    }

    println!("Operations log ({} entries):", entries.len());
    println!();

    for entry in &entries {
        let task_str = entry
            .task_id
            .as_ref()
            .map(|t| format!(" [{}]", t))
            .unwrap_or_default();
        let actor_str = entry
            .actor
            .as_ref()
            .map(|a| format!(" ({})", a))
            .unwrap_or_default();
        println!(
            "  {} {}{}{}",
            entry.timestamp, entry.op, task_str, actor_str
        );
        if !entry.detail.is_null() {
            println!("    {}", entry.detail);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_graph(dir: &Path, graph: &WorkGraph) {
        std::fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        save_graph(graph, &path).unwrap();
    }

    #[test]
    fn test_log_add_creates_entry_with_timestamp_and_message() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        run_add(&dir, "t1", "Started working on this", None).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.log.len(), 1);
        assert_eq!(task.log[0].message, "Started working on this");
        assert!(task.log[0].actor.is_none());
        // Timestamp should be a valid RFC 3339 string
        assert!(!task.log[0].timestamp.is_empty());
        chrono::DateTime::parse_from_rfc3339(&task.log[0].timestamp)
            .expect("timestamp should be valid RFC 3339");
    }

    #[test]
    fn test_log_add_with_actor() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        run_add(&dir, "t1", "Reviewed the PR", Some("alice")).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.log.len(), 1);
        assert_eq!(task.log[0].actor.as_deref(), Some("alice"));
        assert_eq!(task.log[0].message, "Reviewed the PR");
    }

    #[test]
    fn test_log_add_multiple_entries() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        run_add(&dir, "t1", "First entry", None).unwrap();
        run_add(&dir, "t1", "Second entry", Some("bot")).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.log.len(), 2);
        assert_eq!(task.log[0].message, "First entry");
        assert_eq!(task.log[1].message, "Second entry");
        assert_eq!(task.log[1].actor.as_deref(), Some("bot"));
    }

    #[test]
    fn test_log_add_empty_message() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        // Empty message is allowed — the function doesn't validate content
        run_add(&dir, "t1", "", None).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.log.len(), 1);
        assert_eq!(task.log[0].message, "");
    }

    #[test]
    fn test_log_add_task_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        let result = run_add(&dir, "nonexistent", "message", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_log_list_shows_entries() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        run_add(&dir, "t1", "Entry one", None).unwrap();
        run_add(&dir, "t1", "Entry two", Some("bob")).unwrap();

        // run_list should succeed without error
        let result = run_list(&dir, "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_log_list_empty_log() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        // Listing an empty log should succeed
        let result = run_list(&dir, "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_log_list_json_output() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        run_add(&dir, "t1", "JSON test entry", Some("agent")).unwrap();

        // JSON output should succeed
        let result = run_list(&dir, "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_log_list_json_format_is_valid() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        run_add(&dir, "t1", "Check JSON", Some("tester")).unwrap();

        // Verify the data that would be serialized is valid JSON
        let graph = load_graph(graph_path(&dir)).unwrap();
        let task = graph.get_task("t1").unwrap();
        let json_str = serde_json::to_string_pretty(&task.log).unwrap();
        let parsed: Vec<LogEntry> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].message, "Check JSON");
        assert_eq!(parsed[0].actor.as_deref(), Some("tester"));
    }

    #[test]
    fn test_log_list_task_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        let result = run_list(&dir, "nonexistent", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_log_fails_when_not_initialized() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let result = run_add(&dir, "t1", "message", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));

        let result = run_list(&dir, "t1", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_archive_agent_copies_prompt_and_output() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();

        // Create fake agent directory with prompt and output
        let agent_dir = dir.join("agents").join("agent-1");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("prompt.txt"), "Test prompt content").unwrap();
        fs::write(agent_dir.join("output.log"), "Test output content").unwrap();

        let result = archive_agent(&dir, "task-1", "agent-1");
        assert!(result.is_ok());

        let archive_dir = result.unwrap();
        assert!(archive_dir.exists());
        assert!(archive_dir.join("prompt.txt").exists());
        assert!(archive_dir.join("output.txt").exists());

        assert_eq!(
            fs::read_to_string(archive_dir.join("prompt.txt")).unwrap(),
            "Test prompt content"
        );
        assert_eq!(
            fs::read_to_string(archive_dir.join("output.txt")).unwrap(),
            "Test output content"
        );

        // Verify archive path structure: .workgraph/log/agents/<task-id>/<timestamp>/
        let log_agents_dir = dir.join("log").join("agents").join("task-1");
        assert!(log_agents_dir.exists());
        let entries: Vec<_> = fs::read_dir(&log_agents_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_archive_agent_missing_agent_dir_fails() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();

        let result = archive_agent(&dir, "task-1", "agent-nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_archive_agent_without_prompt() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();

        // Agent dir with only output (shell executor may not have prompt)
        let agent_dir = dir.join("agents").join("agent-2");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("output.log"), "Shell output").unwrap();

        let result = archive_agent(&dir, "task-2", "agent-2");
        assert!(result.is_ok());

        let archive_dir = result.unwrap();
        assert!(!archive_dir.join("prompt.txt").exists());
        assert!(archive_dir.join("output.txt").exists());
    }

    #[test]
    fn test_archive_agent_multiple_retries_get_separate_dirs() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();

        // First attempt
        let agent_dir1 = dir.join("agents").join("agent-1");
        fs::create_dir_all(&agent_dir1).unwrap();
        fs::write(agent_dir1.join("prompt.txt"), "Attempt 1 prompt").unwrap();
        fs::write(agent_dir1.join("output.log"), "Attempt 1 output").unwrap();

        let result1 = archive_agent(&dir, "task-1", "agent-1");
        assert!(result1.is_ok());

        // Small delay to ensure different timestamp
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Second attempt (different agent, same task)
        let agent_dir2 = dir.join("agents").join("agent-2");
        fs::create_dir_all(&agent_dir2).unwrap();
        fs::write(agent_dir2.join("prompt.txt"), "Attempt 2 prompt").unwrap();
        fs::write(agent_dir2.join("output.log"), "Attempt 2 output").unwrap();

        let result2 = archive_agent(&dir, "task-1", "agent-2");
        assert!(result2.is_ok());

        // Verify two separate timestamped directories
        let log_agents_dir = dir.join("log").join("agents").join("task-1");
        let entries: Vec<_> = fs::read_dir(&log_agents_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_run_agent_no_archives() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();

        // No archives exist — should print message and succeed
        let result = run_agent(&dir, "task-1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_agent_with_archives() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();

        // Create an archive
        let agent_dir = dir.join("agents").join("agent-1");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("prompt.txt"), "The prompt").unwrap();
        fs::write(agent_dir.join("output.log"), "The output").unwrap();

        archive_agent(&dir, "my-task", "agent-1").unwrap();

        // Show archives
        let result = run_agent(&dir, "my-task", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_agent_json_output() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();

        // Create an archive
        let agent_dir = dir.join("agents").join("agent-1");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("prompt.txt"), "JSON prompt").unwrap();
        fs::write(agent_dir.join("output.log"), "JSON output").unwrap();

        archive_agent(&dir, "json-task", "agent-1").unwrap();

        let result = run_agent(&dir, "json-task", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_agent_no_archives_json() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&dir).unwrap();

        let result = run_agent(&dir, "task-1", true);
        assert!(result.is_ok());
    }
}
