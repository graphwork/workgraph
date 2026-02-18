use anyhow::{Context, Result};
use std::path::Path;
use workgraph::agency;
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// `wg assign <task-id> <agent-hash>`  — explicitly assign agent to task
/// `wg assign <task-id> --clear`       — remove agent assignment
pub fn run(dir: &Path, task_id: &str, agent_hash: Option<&str>, clear: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    if clear {
        return run_clear(dir, &path, task_id);
    }

    match agent_hash {
        Some(hash) => run_explicit_assign(dir, &path, task_id, hash),
        None => {
            anyhow::bail!(
                "Usage: wg assign <task-id> <agent-hash>\n\
                 Or use --clear to remove assignment."
            );
        }
    }
}

/// Explicitly assign an agent (by hash or prefix) to a task.
fn run_explicit_assign(dir: &Path, path: &Path, task_id: &str, agent_hash: &str) -> Result<()> {
    let agency_dir = dir.join("agency");
    let agents_dir = agency_dir.join("agents");

    // Resolve agent by prefix
    let agent = agency::find_agent_by_prefix(&agents_dir, agent_hash).with_context(|| {
        let available = list_available_agent_ids(&agents_dir);
        let hint = if available.is_empty() {
            "No agents defined. Use 'wg agent create' to create one.".to_string()
        } else {
            format!("Available agents: {}", available.join(", "))
        };
        format!("No agent matching '{}'. {}", agent_hash, hint)
    })?;

    let mut graph = load_graph(path).context("Failed to load graph")?;

    let task = graph.get_task_mut_or_err(task_id)?;

    task.agent = Some(agent.id.clone());
    save_graph(&graph, path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "assign",
        Some(task_id),
        None,
        serde_json::json!({ "agent_hash": agent.id, "role_id": agent.role_id }),
        config.log.rotation_threshold,
    );

    // Resolve role/motivation names for display
    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");

    let role_name = agency::find_role_by_prefix(&roles_dir, &agent.role_id)
        .map(|r| r.name)
        .unwrap_or_else(|_| "(not found)".to_string());
    let motivation_name = agency::find_motivation_by_prefix(&motivations_dir, &agent.motivation_id)
        .map(|m| m.name)
        .unwrap_or_else(|_| "(not found)".to_string());

    println!("Assigned agent to task '{}':", task_id);
    println!(
        "  Agent:      {} ({})",
        agent.name,
        agency::short_hash(&agent.id)
    );
    println!(
        "  Role:       {} ({})",
        role_name,
        agency::short_hash(&agent.role_id)
    );
    println!(
        "  Motivation: {} ({})",
        motivation_name,
        agency::short_hash(&agent.motivation_id)
    );

    Ok(())
}

/// Clear the agent assignment from a task.
fn run_clear(dir: &Path, path: &Path, task_id: &str) -> Result<()> {
    let mut graph = load_graph(path).context("Failed to load graph")?;

    let task = graph.get_task_mut_or_err(task_id)?;

    let prev_agent = task.agent.clone();
    task.agent = None;
    save_graph(&graph, path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "assign",
        Some(task_id),
        None,
        serde_json::json!({ "action": "clear", "prev_agent": prev_agent }),
        config.log.rotation_threshold,
    );

    if prev_agent.is_some() {
        println!("Cleared agent from task '{}'", task_id);
    } else {
        println!("Task '{}' had no agent assigned (no change)", task_id);
    }
    Ok(())
}

/// List available agent short IDs from the agents directory.
fn list_available_agent_ids(dir: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                ids.push(agency::short_hash(stem).to_string());
            }
        }
    }
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::agency::{Lineage, PerformanceRecord, SkillRef};
    use workgraph::graph::{Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_workgraph(dir: &Path, tasks: Vec<Task>) {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
    }

    /// Set up agency with test entities, returning (agent_id, role_id, motivation_id).
    fn setup_agency(dir: &Path) -> (String, String, String) {
        let agency_dir = dir.join("agency");
        agency::init(&agency_dir).unwrap();

        let role = agency::build_role(
            "Implementer",
            "Writes code",
            vec![SkillRef::Name("rust".to_string())],
            "Working code",
        );
        let role_id = role.id.clone();
        agency::save_role(&role, &agency_dir.join("roles")).unwrap();

        let mut motivation = agency::build_motivation(
            "Quality First",
            "Prioritise correctness",
            vec!["Slower delivery".to_string()],
            vec!["Skipping tests".to_string()],
        );
        motivation.performance.task_count = 2;
        motivation.performance.avg_score = Some(0.9);
        let mot_id = motivation.id.clone();
        agency::save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

        // Create an agent for this role+motivation pair
        let agent_id = agency::content_hash_agent(&role_id, &mot_id);
        let agent = agency::Agent {
            id: agent_id.clone(),
            role_id: role_id.clone(),
            motivation_id: mot_id.clone(),
            name: "test-agent".to_string(),
            performance: PerformanceRecord {
                task_count: 0,
                avg_score: None,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "claude".to_string(),
        };
        agency::save_agent(&agent, &agency_dir.join("agents")).unwrap();

        (agent_id, role_id, mot_id)
    }

    #[test]
    fn test_assign_explicit_agent_hash() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (agent_id, _role_id, _mot_id) = setup_agency(dir_path);

        let result = run(dir_path, "t1", Some(&agent_id), false);
        assert!(result.is_ok(), "assign failed: {:?}", result.err());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.agent, Some(agent_id));
    }

    #[test]
    fn test_assign_by_prefix() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        let (agent_id, _role_id, _mot_id) = setup_agency(dir_path);

        // Use 8-char prefix instead of full hash
        let prefix = &agent_id[..8];
        let result = run(dir_path, "t1", Some(prefix), false);
        assert!(
            result.is_ok(),
            "assign by prefix failed: {:?}",
            result.err()
        );

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.agent, Some(agent_id));
    }

    #[test]
    fn test_assign_clear() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task");
        task.agent = Some("some-agent-hash".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", None, true);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.agent.is_none());
    }

    #[test]
    fn test_assign_nonexistent_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);
        let (agent_id, _, _) = setup_agency(dir_path);

        let result = run(dir_path, "nonexistent", Some(&agent_id), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_assign_nonexistent_agent() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);
        setup_agency(dir_path);

        let result = run(dir_path, "t1", Some("nonexistent"), false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No agent matching 'nonexistent'"));
    }

    #[test]
    fn test_assign_no_args_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);

        let result = run(dir_path, "t1", None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Usage:"));
    }

    #[test]
    fn test_clear_no_agent_is_noop() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task")]);

        let result = run(dir_path, "t1", None, true);
        assert!(result.is_ok());
    }
}
