use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status, WaitCondition, WaitSpec, parse_delay};
use workgraph::parser::save_graph;
use workgraph::service::registry::{AgentRegistry, AgentStatus};

/// Parse a condition string into a WaitCondition.
///
/// Supported formats:
/// - `task:<id>=<status>` — wait for a task to reach a status
/// - `timer:<duration>` — wait for a duration (e.g. 5m, 2h, 30s)
/// - `human-input` — wait for a human message
/// - `message` — wait for any message
/// - `file:<path>` — wait for a file to change
fn parse_condition(s: &str, graph: &workgraph::graph::WorkGraph) -> Result<WaitCondition> {
    let s = s.trim();

    if s == "human-input" {
        return Ok(WaitCondition::HumanInput);
    }
    if s == "message" {
        return Ok(WaitCondition::Message);
    }

    if let Some(rest) = s.strip_prefix("task:") {
        // Format: task:<id>=<status>
        let parts: Vec<&str> = rest.splitn(2, '=').collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "Invalid task condition '{}'. Expected format: task:<id>=<status>",
                s
            );
        }
        let task_id = parts[0];
        let status_str = parts[1];

        // Validate the referenced task exists
        if graph.get_task(task_id).is_none() {
            anyhow::bail!("Task '{}' referenced in condition does not exist", task_id);
        }

        let status = match status_str {
            "open" => Status::Open,
            "in-progress" => Status::InProgress,
            "waiting" => Status::Waiting,
            "done" => Status::Done,
            "blocked" => Status::Blocked,
            "failed" => Status::Failed,
            "abandoned" => Status::Abandoned,
            other => anyhow::bail!("Unknown status '{}' in condition", other),
        };

        return Ok(WaitCondition::TaskStatus {
            task_id: task_id.to_string(),
            status,
        });
    }

    if let Some(rest) = s.strip_prefix("timer:") {
        let secs = parse_delay(rest).ok_or_else(|| {
            anyhow::anyhow!("Invalid timer duration '{}'. Use e.g. 5m, 2h, 30s", rest)
        })?;
        let resume_after = Utc::now() + chrono::Duration::seconds(secs as i64);
        return Ok(WaitCondition::Timer {
            resume_after: resume_after.to_rfc3339(),
        });
    }

    if let Some(rest) = s.strip_prefix("file:") {
        let path = rest.trim();
        if path.is_empty() {
            anyhow::bail!("Empty file path in condition");
        }
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            })
            .unwrap_or(0);
        return Ok(WaitCondition::FileChanged {
            path: path.to_string(),
            mtime_at_wait: mtime,
        });
    }

    anyhow::bail!(
        "Unknown condition '{}'. Supported: task:<id>=<status>, timer:<dur>, human-input, message, file:<path>",
        s
    );
}

/// Parse a composite condition string into a WaitSpec.
///
/// Comma-separated = AND (All), pipe-separated = OR (Any).
/// Cannot mix AND and OR in one expression.
fn parse_wait_spec(s: &str, graph: &workgraph::graph::WorkGraph) -> Result<WaitSpec> {
    let has_comma = s.contains(',');
    let has_pipe = s.contains('|');

    if has_comma && has_pipe {
        anyhow::bail!(
            "Cannot mix AND (,) and OR (|) in a single --until expression. \
             Use all commas or all pipes."
        );
    }

    if has_pipe {
        let conditions: Vec<WaitCondition> = s
            .split('|')
            .map(|part| parse_condition(part, graph))
            .collect::<Result<Vec<_>>>()?;
        Ok(WaitSpec::Any(conditions))
    } else if has_comma {
        let conditions: Vec<WaitCondition> = s
            .split(',')
            .map(|part| parse_condition(part, graph))
            .collect::<Result<Vec<_>>>()?;
        Ok(WaitSpec::All(conditions))
    } else {
        // Single condition — wrap as All with one element
        let condition = parse_condition(s, graph)?;
        Ok(WaitSpec::All(vec![condition]))
    }
}

pub fn run(dir: &Path, id: &str, until: &str, checkpoint: Option<&str>) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_or_err(id)?;

    // Validate task is InProgress
    if task.status != Status::InProgress {
        anyhow::bail!(
            "Cannot wait on task '{}': status is '{}', expected 'in-progress'",
            id,
            task.status
        );
    }

    // Parse and validate the condition
    let wait_spec = parse_wait_spec(until, &graph)?;

    // Now mutate
    let task = graph
        .get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    task.status = Status::Waiting;
    task.wait_condition = Some(wait_spec);

    if let Some(cp) = checkpoint {
        task.checkpoint = Some(cp.to_string());
    }

    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: task.assigned.clone(),
        message: format!("Agent parked. Waiting for: {}", until),
        ..Default::default()
    });

    // Update agent status to Parked if there's an assigned agent
    if let Some(ref assigned) = task.assigned.clone()
        && let Ok(mut registry) = AgentRegistry::load_locked(dir)
    {
        if let Some(agent) = registry.registry.get_agent_mut(assigned) {
            agent.status = AgentStatus::Parked;
            agent.completed_at = Some(Utc::now().to_rfc3339());
        }
        // Also try to find by task_id if assigned is not an agent registry key
        for agent in registry.registry.agents.values_mut() {
            if agent.task_id == id && agent.is_alive() {
                agent.status = AgentStatus::Parked;
                if agent.completed_at.is_none() {
                    agent.completed_at = Some(Utc::now().to_rfc3339());
                }
            }
        }
        let _ = registry.save();
    }

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    println!("Parked task '{}'. Condition: {}", id, until);
    println!("Checkpoint saved. You should now exit cleanly.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use workgraph::graph::{Status, WaitCondition, WaitSpec};
    use workgraph::parser::load_graph;
    use workgraph::test_helpers::{make_task_with_status as make_task, setup_workgraph};

    fn graph_path(dir: &Path) -> std::path::PathBuf {
        dir.join("graph.jsonl")
    }

    #[test]
    fn test_wg_wait_basic_task_condition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let dep = make_task("dep-a", "Dependency A", Status::Open);
        let mut main_task = make_task("main", "Main task", Status::InProgress);
        main_task.assigned = Some("agent-1".to_string());

        setup_workgraph(dir_path, vec![dep, main_task]);

        let result = run(
            dir_path,
            "main",
            "task:dep-a=done",
            Some("Phase 1 complete"),
        );
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();

        assert_eq!(task.status, Status::Waiting);
        assert!(task.wait_condition.is_some());
        assert_eq!(task.checkpoint.as_deref(), Some("Phase 1 complete"));

        // Check wait condition contents
        if let Some(WaitSpec::All(conditions)) = &task.wait_condition {
            assert_eq!(conditions.len(), 1);
            match &conditions[0] {
                WaitCondition::TaskStatus { task_id, status } => {
                    assert_eq!(task_id, "dep-a");
                    assert_eq!(*status, Status::Done);
                }
                _ => panic!("Expected TaskStatus condition"),
            }
        } else {
            panic!("Expected WaitSpec::All");
        }
    }

    #[test]
    fn test_wg_wait_rejects_non_in_progress() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(dir_path, vec![make_task("t1", "Test", Status::Open)]);

        let result = run(dir_path, "t1", "message", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("in-progress"));
    }

    #[test]
    fn test_wg_wait_rejects_nonexistent_task_in_condition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("main", "Main", Status::InProgress)],
        );

        let result = run(dir_path, "main", "task:nonexistent=done", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_wg_wait_timer_condition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("main", "Main", Status::InProgress)],
        );

        let result = run(dir_path, "main", "timer:5m", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();

        assert_eq!(task.status, Status::Waiting);
        if let Some(WaitSpec::All(conditions)) = &task.wait_condition {
            match &conditions[0] {
                WaitCondition::Timer { resume_after } => {
                    // Should be parseable as RFC3339
                    assert!(resume_after.parse::<chrono::DateTime<Utc>>().is_ok());
                }
                _ => panic!("Expected Timer condition"),
            }
        } else {
            panic!("Expected WaitSpec::All");
        }
    }

    #[test]
    fn test_wg_wait_message_condition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("main", "Main", Status::InProgress)],
        );

        let result = run(dir_path, "main", "message", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();
        assert_eq!(task.status, Status::Waiting);
    }

    #[test]
    fn test_wg_wait_human_input_condition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("main", "Main", Status::InProgress)],
        );

        let result = run(dir_path, "main", "human-input", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();
        assert_eq!(task.status, Status::Waiting);
    }

    #[test]
    fn test_wg_wait_and_conditions() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let dep_a = make_task("dep-a", "Dep A", Status::Open);
        let dep_b = make_task("dep-b", "Dep B", Status::Open);
        let main = make_task("main", "Main", Status::InProgress);

        setup_workgraph(dir_path, vec![dep_a, dep_b, main]);

        let result = run(dir_path, "main", "task:dep-a=done,task:dep-b=done", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();

        if let Some(WaitSpec::All(conditions)) = &task.wait_condition {
            assert_eq!(conditions.len(), 2);
        } else {
            panic!("Expected WaitSpec::All with 2 conditions");
        }
    }

    #[test]
    fn test_wg_wait_or_conditions() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let dep = make_task("dep-a", "Dep A", Status::Open);
        let main = make_task("main", "Main", Status::InProgress);

        setup_workgraph(dir_path, vec![dep, main]);

        let result = run(dir_path, "main", "task:dep-a=done|timer:5m", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();

        if let Some(WaitSpec::Any(conditions)) = &task.wait_condition {
            assert_eq!(conditions.len(), 2);
        } else {
            panic!("Expected WaitSpec::Any with 2 conditions");
        }
    }

    #[test]
    fn test_wg_wait_mixed_and_or_rejected() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let dep_a = make_task("dep-a", "Dep A", Status::Open);
        let main = make_task("main", "Main", Status::InProgress);

        setup_workgraph(dir_path, vec![dep_a, main]);

        let result = run(dir_path, "main", "task:dep-a=done,timer:5m|message", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot mix"));
    }

    #[test]
    fn test_wg_wait_invalid_condition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("main", "Main", Status::InProgress)],
        );

        let result = run(dir_path, "main", "invalid-condition", None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unknown condition")
        );
    }

    #[test]
    fn test_wg_wait_creates_log_entry() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("main", "Main", Status::InProgress)],
        );

        let result = run(dir_path, "main", "message", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();

        let last_log = task.log.last().unwrap();
        assert!(last_log.message.contains("Agent parked"));
        assert!(last_log.message.contains("message"));
    }

    #[test]
    fn test_wg_wait_file_condition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Create a file to watch
        let watch_file = dir.path().join("watched.txt");
        std::fs::write(&watch_file, "initial").unwrap();

        setup_workgraph(
            dir_path,
            vec![make_task("main", "Main", Status::InProgress)],
        );

        let result = run(
            dir_path,
            "main",
            &format!("file:{}", watch_file.display()),
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();

        if let Some(WaitSpec::All(conditions)) = &task.wait_condition {
            match &conditions[0] {
                WaitCondition::FileChanged {
                    path,
                    mtime_at_wait,
                } => {
                    assert!(path.contains("watched.txt"));
                    assert!(*mtime_at_wait > 0);
                }
                _ => panic!("Expected FileChanged condition"),
            }
        } else {
            panic!("Expected WaitSpec::All");
        }
    }

    #[test]
    fn test_wg_wait_without_checkpoint() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("main", "Main", Status::InProgress)],
        );

        let result = run(dir_path, "main", "message", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("main").unwrap();

        assert_eq!(task.status, Status::Waiting);
        assert!(task.checkpoint.is_none());
    }
}
