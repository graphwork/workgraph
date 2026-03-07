use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use std::process::Command;
use workgraph::graph::{LogEntry, Status};

#[cfg(test)]
use super::graph_path;

/// Execute a task's shell command
///
/// This implements the "optional exec helper" part of the execution model:
/// - Claims the task if not already in progress
/// - Runs the task's exec command
/// - Marks done on success (exit 0), fail on error
pub fn run(dir: &Path, task_id: &str, actor: Option<&str>, dry_run: bool) -> Result<()> {
    // Read-only check: verify task exists, has exec command, and isn't done
    let (graph, _path) = super::load_workgraph(dir)?;
    let task = graph.get_task_or_err(task_id)?;

    let exec_cmd = task
        .exec
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Task '{}' has no exec command defined", task_id))?;

    if task.status == Status::Done {
        anyhow::bail!("Task '{}' is already done", task_id);
    }

    if dry_run {
        println!("Would execute for task '{}':", task_id);
        println!("  Command: {}", exec_cmd);
        println!("  Status: {:?} -> InProgress -> Done/Failed", task.status);
        return Ok(());
    }

    let was_open = task.status == Status::Open;
    drop(graph);

    // Claim the task atomically if it was open
    if was_open {
        let actor_owned = actor.map(String::from);
        let exec_cmd_clone = exec_cmd.clone();
        let task_id_owned = task_id.to_string();
        super::mutate_workgraph(dir, |graph| {
            let task = graph.get_task_mut_or_err(&task_id_owned)?;
            task.status = Status::InProgress;
            task.started_at = Some(Utc::now().to_rfc3339());
            if let Some(ref actor_id) = actor_owned {
                task.assigned = Some(actor_id.clone());
            }
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: actor_owned,
                message: format!("Started execution: {}", exec_cmd_clone),
            });
            Ok(())
        })?;
        super::notify_graph_changed(dir);
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

    if !stdout.is_empty() {
        println!("{}", stdout);
    }
    if !stderr.is_empty() {
        eprintln!("{}", stderr);
    }

    // Atomically update status (task may have been modified by exec command)
    let task_id_owned = task_id.to_string();
    let actor_owned = actor.map(String::from);
    if success {
        super::mutate_workgraph(dir, |graph| {
            let task = graph.get_task_mut_or_err(&task_id_owned)?;
            task.status = Status::Done;
            task.completed_at = Some(Utc::now().to_rfc3339());
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: actor_owned,
                message: "Execution completed successfully".to_string(),
            });
            Ok(())
        })?;
        super::notify_graph_changed(dir);
        println!("Task '{}' completed successfully", task_id);
    } else {
        let exit_code = output.status.code().unwrap_or(-1);
        super::mutate_workgraph(dir, |graph| {
            let task = graph.get_task_mut_or_err(&task_id_owned)?;
            task.status = Status::Failed;
            task.retry_count += 1;
            task.failure_reason = Some(format!("Command exited with code {}", exit_code));
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: actor_owned,
                message: format!("Execution failed with exit code {}", exit_code),
            });
            Ok(())
        })?;
        super::notify_graph_changed(dir);
        anyhow::bail!("Task '{}' failed with exit code {}", task_id, exit_code);
    }

    Ok(())
}

/// Set the exec command for a task
pub fn set_exec(dir: &Path, task_id: &str, command: &str) -> Result<()> {
    let task_id = task_id.to_string();
    let command = command.to_string();
    super::mutate_workgraph(dir, |graph| {
        let task = graph.get_task_mut_or_err(&task_id)?;
        task.exec = Some(command.clone());
        Ok(())
    })?;
    super::notify_graph_changed(dir);

    println!("Set exec command for '{}': {}", task_id, command);
    Ok(())
}

/// Clear the exec command for a task
pub fn clear_exec(dir: &Path, task_id: &str) -> Result<()> {
    let task_id = task_id.to_string();
    let cleared = super::mutate_workgraph(dir, |graph| {
        let task = graph.get_task_mut_or_err(&task_id)?;
        if task.exec.is_none() {
            return Ok(false);
        }
        task.exec = None;
        Ok(true)
    })?;

    if cleared {
        super::notify_graph_changed(dir);
        println!("Cleared exec command for '{}'", task_id);
    } else {
        println!("Task '{}' has no exec command to clear", task_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::{load_graph, save_graph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
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
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
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
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
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
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
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

        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.exec, Some("echo test".to_string()));
    }

    #[test]
    fn test_clear_exec() {
        let temp_dir = setup_graph_with_exec();

        let result = clear_exec(temp_dir.path(), "t1");
        assert!(result.is_ok());

        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.exec.is_none());
    }
}
