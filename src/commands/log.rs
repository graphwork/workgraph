use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::LogEntry;
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Add a log entry to a task
pub fn run_add(dir: &Path, id: &str, message: &str, actor: Option<&str>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    let entry = LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message: message.to_string(),
    };

    task.log.push(entry);

    save_graph(&graph, &path).context("Failed to save graph")?;

    let actor_str = actor.map(|a| format!(" ({})", a)).unwrap_or_default();
    println!("Added log entry to '{}'{}", id, actor_str);
    Ok(())
}

/// List log entries for a task
pub fn run_list(dir: &Path, id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Status, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

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
}
