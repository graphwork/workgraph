use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use std::process::Command;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Execute a task's shell command
///
/// This implements the "optional exec helper" part of the execution model:
/// - Claims the task if not already in progress
/// - Runs the task's exec command
/// - Marks done on success (exit 0), fail on error
pub fn run(dir: &Path, task_id: &str, actor: Option<&str>, dry_run: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Check task has an exec command
    let exec_cmd = task.exec.clone().ok_or_else(|| {
        anyhow::anyhow!("Task '{}' has no exec command defined", task_id)
    })?;

    // Check task status
    if task.status == Status::Done {
        anyhow::bail!("Task '{}' is already done", task_id);
    }

    if dry_run {
        println!("Would execute for task '{}':", task_id);
        println!("  Command: {}", exec_cmd);
        println!("  Status: {:?} -> InProgress -> Done/Failed", task.status);
        return Ok(());
    }

    // Claim the task if not already in progress
    let task = graph.get_task_mut(task_id).unwrap();
    let was_open = task.status == Status::Open;

    if was_open {
        task.status = Status::InProgress;
        task.started_at = Some(Utc::now().to_rfc3339());
        if let Some(actor_id) = actor {
            task.assigned = Some(actor_id.to_string());
        }
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: actor.map(String::from),
            message: format!("Started execution: {}", exec_cmd),
        });
        save_graph(&graph, &path).context("Failed to save graph")?;
        println!("Claimed task '{}' for execution", task_id);
    }

    // Run the command
    println!("Executing: {}", exec_cmd);
    let output = Command::new("sh")
        .arg("-c")
        .arg(&exec_cmd)
        .output()
        .context("Failed to execute command")?;

    let success = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Print output
    if !stdout.is_empty() {
        println!("{}", stdout);
    }
    if !stderr.is_empty() {
        eprintln!("{}", stderr);
    }

    // Reload graph and update status
    let mut graph = load_graph(&path).context("Failed to reload graph")?;
    let task = graph.get_task_mut(task_id).unwrap();

    if success {
        task.status = Status::Done;
        task.completed_at = Some(Utc::now().to_rfc3339());
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: actor.map(String::from),
            message: "Execution completed successfully".to_string(),
        });
        save_graph(&graph, &path).context("Failed to save graph")?;
        println!("Task '{}' completed successfully", task_id);
    } else {
        let exit_code = output.status.code().unwrap_or(-1);
        task.status = Status::Failed;
        task.retry_count += 1;
        task.failure_reason = Some(format!("Command exited with code {}", exit_code));
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: actor.map(String::from),
            message: format!("Execution failed with exit code {}", exit_code),
        });
        save_graph(&graph, &path).context("Failed to save graph")?;
        anyhow::bail!("Task '{}' failed with exit code {}", task_id, exit_code);
    }

    Ok(())
}

/// Set the exec command for a task
pub fn set_exec(dir: &Path, task_id: &str, command: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    task.exec = Some(command.to_string());

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Set exec command for '{}': {}", task_id, command);
    Ok(())
}

/// Clear the exec command for a task
pub fn clear_exec(dir: &Path, task_id: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    if task.exec.is_none() {
        println!("Task '{}' has no exec command to clear", task_id);
        return Ok(());
    }

    task.exec = None;

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Cleared exec command for '{}'", task_id);
    Ok(())
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
        }
    }

    fn setup_graph_with_exec() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        temp_dir
    }

    #[test]
    fn test_exec_success() {
        let temp_dir = setup_graph_with_exec();

        let result = run(temp_dir.path(), "t1", None, false);
        assert!(result.is_ok());

        // Verify task is done
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_exec_failure() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Failing Task");
        task.exec = Some("exit 1".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", None, false);
        assert!(result.is_err());

        // Verify task is failed
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
    }

    #[test]
    fn test_exec_no_command() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "No Exec Task");
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no exec command"));
    }

    #[test]
    fn test_exec_dry_run() {
        let temp_dir = setup_graph_with_exec();

        let result = run(temp_dir.path(), "t1", None, true);
        assert!(result.is_ok());

        // Verify task is still open (dry run doesn't execute)
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_set_exec() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Test Task");
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let result = set_exec(temp_dir.path(), "t1", "echo test");
        assert!(result.is_ok());

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.exec, Some("echo test".to_string()));
    }

    #[test]
    fn test_clear_exec() {
        let temp_dir = setup_graph_with_exec();

        let result = clear_exec(temp_dir.path(), "t1");
        assert!(result.is_ok());

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.exec.is_none());
    }
}
