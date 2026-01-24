use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use workgraph::parser::load_graph;

use super::graph_path;

/// JSON output for skill listing
#[derive(Debug, Serialize)]
struct SkillSummary {
    skill: String,
    task_count: usize,
    tasks: Vec<String>,
}

/// JSON output for task skills
#[derive(Debug, Serialize)]
struct TaskSkills {
    id: String,
    title: String,
    skills: Vec<String>,
}

/// List all skills used across tasks
pub fn run_list(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Build a map of skill -> tasks that require it
    let mut skill_map: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for task in graph.tasks() {
        for skill in &task.skills {
            skill_map
                .entry(skill.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    if json {
        let output: Vec<SkillSummary> = skill_map
            .into_iter()
            .map(|(skill, tasks)| SkillSummary {
                skill,
                task_count: tasks.len(),
                tasks,
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if skill_map.is_empty() {
            println!("No skills defined in any tasks");
        } else {
            println!("Skills used across tasks:\n");
            for (skill, tasks) in &skill_map {
                println!("  {} ({} tasks)", skill, tasks.len());
            }
        }
    }

    Ok(())
}

/// Show skills for a specific task
pub fn run_task(dir: &Path, id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    if json {
        let output = TaskSkills {
            id: task.id.clone(),
            title: task.title.clone(),
            skills: task.skills.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Task: {} - {}", task.id, task.title);
        if task.skills.is_empty() {
            println!("Skills: (none)");
        } else {
            println!("Skills: {}", task.skills.join(", "));
        }
    }

    Ok(())
}

/// Find tasks requiring a specific skill
pub fn run_find(dir: &Path, skill: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let matching_tasks: Vec<_> = graph
        .tasks()
        .filter(|t| t.skills.iter().any(|s| s == skill))
        .collect();

    if json {
        let output: Vec<TaskSkills> = matching_tasks
            .iter()
            .map(|t| TaskSkills {
                id: t.id.clone(),
                title: t.title.clone(),
                skills: t.skills.clone(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if matching_tasks.is_empty() {
            println!("No tasks require skill '{}'", skill);
        } else {
            println!("Tasks requiring skill '{}':\n", skill);
            for task in matching_tasks {
                println!("  {} - {}", task.id, task.title);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Status, Task, WorkGraph};

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

    #[test]
    fn test_skill_collection() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.skills = vec!["rust".to_string(), "testing".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.skills = vec!["rust".to_string(), "documentation".to_string()];

        let t3 = make_task("t3", "Task 3");

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        // Build skill map similar to run_list
        let mut skill_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for task in graph.tasks() {
            for skill in &task.skills {
                skill_map
                    .entry(skill.clone())
                    .or_default()
                    .push(task.id.clone());
            }
        }

        assert_eq!(skill_map.len(), 3);
        assert_eq!(skill_map.get("rust").unwrap().len(), 2);
        assert_eq!(skill_map.get("testing").unwrap().len(), 1);
        assert_eq!(skill_map.get("documentation").unwrap().len(), 1);
    }

    #[test]
    fn test_find_tasks_by_skill() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.skills = vec!["rust".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.skills = vec!["python".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let skill = "rust";
        let matching: Vec<_> = graph
            .tasks()
            .filter(|t| t.skills.iter().any(|s| s == skill))
            .collect();

        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].id, "t1");
    }
}
