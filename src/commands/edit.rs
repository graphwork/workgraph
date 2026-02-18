//! Edit command for modifying existing tasks

use anyhow::{Context, Result};
use std::path::Path;
use workgraph::graph::{LoopEdge, parse_delay};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Edit a task's fields
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    task_id: &str,
    title: Option<&str>,
    description: Option<&str>,
    add_blocked_by: &[String],
    remove_blocked_by: &[String],
    add_tag: &[String],
    remove_tag: &[String],
    model: Option<&str>,
    add_skill: &[String],
    remove_skill: &[String],
    add_loops_to: Option<&str>,
    loop_max: Option<u32>,
    loop_guard: Option<&str>,
    loop_delay: Option<&str>,
    remove_loops_to: Option<&str>,
    loop_iteration: Option<u32>,
) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Load the graph
    let mut graph = load_graph(&path).context("Failed to load graph")?;

    // Validate task exists
    graph.get_task_or_err(task_id)?;

    // Validate self-blocking
    for dep in add_blocked_by {
        if dep == task_id {
            anyhow::bail!("Task '{}' cannot block itself", task_id);
        }
    }

    // Validate self-loop
    if let Some(target) = add_loops_to
        && target == task_id
    {
        anyhow::bail!("Task '{}' cannot loop to itself", task_id);
    }

    let mut changed = false;
    let mut field_changes: Vec<serde_json::Value> = Vec::new();

    // Modify the task in a block so the mutable borrow is released afterwards
    {
        let task = graph.get_task_mut_or_err(task_id)?;

        // Update title
        if let Some(new_title) = title {
            let old = task.title.clone();
            task.title = new_title.to_string();
            field_changes.push(serde_json::json!({"field": "title", "old": old, "new": new_title}));
            println!("Updated title: {}", new_title);
            changed = true;
        }

        // Update description
        if let Some(new_description) = description {
            let old = task.description.clone();
            task.description = Some(new_description.to_string());
            field_changes.push(serde_json::json!({"field": "description", "old": old, "new": new_description}));
            println!("Updated description");
            changed = true;
        }

        // Add blocked_by dependencies
        for dep in add_blocked_by {
            if !task.blocked_by.contains(dep) {
                task.blocked_by.push(dep.clone());
                println!("Added blocked_by: {}", dep);
                changed = true;
            } else {
                println!("Already blocked by: {}", dep);
            }
        }

        // Remove blocked_by dependencies
        for dep in remove_blocked_by {
            if let Some(pos) = task.blocked_by.iter().position(|x| x == dep) {
                task.blocked_by.remove(pos);
                println!("Removed blocked_by: {}", dep);
                changed = true;
            } else {
                println!("Not blocked by: {}", dep);
            }
        }

        // Add tags
        for tag in add_tag {
            if !task.tags.contains(tag) {
                task.tags.push(tag.clone());
                println!("Added tag: {}", tag);
                changed = true;
            } else {
                println!("Already has tag: {}", tag);
            }
        }

        // Remove tags
        for tag in remove_tag {
            if let Some(pos) = task.tags.iter().position(|x| x == tag) {
                task.tags.remove(pos);
                println!("Removed tag: {}", tag);
                changed = true;
            } else {
                println!("Does not have tag: {}", tag);
            }
        }

        // Update model
        if let Some(new_model) = model {
            task.model = Some(new_model.to_string());
            println!("Updated model: {}", new_model);
            changed = true;
        }

        // Add skills
        for skill in add_skill {
            if !task.skills.contains(skill) {
                task.skills.push(skill.clone());
                println!("Added skill: {}", skill);
                changed = true;
            } else {
                println!("Already has skill: {}", skill);
            }
        }

        // Remove skills
        for skill in remove_skill {
            if let Some(pos) = task.skills.iter().position(|x| x == skill) {
                task.skills.remove(pos);
                println!("Removed skill: {}", skill);
                changed = true;
            } else {
                println!("Does not have skill: {}", skill);
            }
        }

        // Add loops_to edge
        if let Some(target) = add_loops_to {
            let max_iterations = loop_max.ok_or_else(|| {
                anyhow::anyhow!("--loop-max is required when using --add-loops-to")
            })?;
            let guard = match loop_guard {
                Some(expr) => Some(crate::commands::add::parse_guard_expr(expr)?),
                None => None,
            };
            let delay = match loop_delay {
                Some(d) => {
                    parse_delay(d).ok_or_else(|| {
                        anyhow::anyhow!("Invalid delay '{}'. Use format: 30s, 5m, 1h, 24h, 7d", d)
                    })?;
                    Some(d.to_string())
                }
                None => None,
            };
            // Check for duplicate target
            if task.loops_to.iter().any(|e| e.target == target) {
                println!("Already has loops_to edge targeting: {}", target);
            } else {
                task.loops_to.push(LoopEdge {
                    target: target.to_string(),
                    guard,
                    max_iterations,
                    delay,
                });
                println!(
                    "Added loops_to: {} (max_iterations: {})",
                    target, max_iterations
                );
                changed = true;
            }
        } else if loop_max.is_some() || loop_guard.is_some() || loop_delay.is_some() {
            anyhow::bail!("--loop-max, --loop-guard, and --loop-delay require --add-loops-to");
        }

        // Remove loops_to edge
        if let Some(target) = remove_loops_to {
            if let Some(pos) = task.loops_to.iter().position(|e| e.target == target) {
                task.loops_to.remove(pos);
                println!("Removed loops_to: {}", target);
                changed = true;
            } else {
                println!("No loops_to edge targeting: {}", target);
            }
        }

        // Set loop_iteration directly
        if let Some(iter) = loop_iteration {
            task.loop_iteration = iter;
            println!("Set loop_iteration: {}", iter);
            changed = true;
        }
    } // task borrow released here

    // Maintain bidirectional consistency: update `blocks` on referenced tasks
    let task_id_owned = task_id.to_string();
    for dep in add_blocked_by {
        if let Some(blocker) = graph.get_task_mut(dep)
            && !blocker.blocks.contains(&task_id_owned)
        {
            blocker.blocks.push(task_id_owned.clone());
        }
    }
    for dep in remove_blocked_by {
        if let Some(blocker) = graph.get_task_mut(dep) {
            blocker.blocks.retain(|b| b != &task_id_owned);
        }
    }

    // Save if changes were made
    if changed {
        save_graph(&graph, &path).context("Failed to save graph")?;
        super::notify_graph_changed(dir);

        // Record operation
        let config = workgraph::config::Config::load_or_default(dir);
        let _ = workgraph::provenance::record(
            dir,
            "edit",
            Some(task_id),
            None,
            serde_json::json!({ "fields": field_changes }),
            config.log.rotation_threshold,
        );

        println!("\nTask '{}' updated successfully", task_id);
    } else {
        println!("No changes made to task '{}'", task_id);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_graph(dir: &Path) -> Result<()> {
        // Create the workgraph directory if it doesn't exist
        fs::create_dir_all(dir)?;

        // Create an empty graph.jsonl file
        let graph_path = graph_path(dir);
        fs::write(&graph_path, "")?;

        // Add a test task using the add command
        crate::commands::add::run(
            dir,
            "Test Task",
            Some("test-task"),
            Some("Original description"),
            &["dep1".to_string()],
            None,
            None,
            None,
            &["tag1".to_string()],
            &["skill1".to_string()],
            &[],
            &[],
            None,
            Some("sonnet"),
            None,
            None,
            None,
            None,
            None,
        )?;

        Ok(())
    }

    fn create_test_graph_with_two_tasks(dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        let graph_path = graph_path(dir);
        fs::write(&graph_path, "")?;

        // Add two independent tasks (no initial dependency between them)
        crate::commands::add::run(
            dir,
            "Blocker Task",
            Some("blocker-task"),
            None,
            &[],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )?;

        crate::commands::add::run(
            dir,
            "Test Task",
            Some("test-task"),
            Some("Original description"),
            &[],
            None,
            None,
            None,
            &["tag1".to_string()],
            &["skill1".to_string()],
            &[],
            &[],
            None,
            Some("sonnet"),
            None,
            None,
            None,
            None,
            None,
        )?;

        Ok(())
    }

    #[test]
    fn test_edit_title() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            Some("New Title"),
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert_eq!(task.title, "New Title");
    }

    #[test]
    fn test_edit_description() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            Some("New description"),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert_eq!(task.description, Some("New description".to_string()));
    }

    #[test]
    fn test_add_blocked_by() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["dep2".to_string()],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(task.blocked_by.contains(&"dep2".to_string()));
        assert!(task.blocked_by.contains(&"dep1".to_string()));
    }

    #[test]
    fn test_remove_blocked_by() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &["dep1".to_string()],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(!task.blocked_by.contains(&"dep1".to_string()));
    }

    #[test]
    fn test_add_tag() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &["tag2".to_string()],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(task.tags.contains(&"tag2".to_string()));
        assert!(task.tags.contains(&"tag1".to_string()));
    }

    #[test]
    fn test_remove_tag() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &["tag1".to_string()],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(!task.tags.contains(&"tag1".to_string()));
    }

    #[test]
    fn test_edit_model() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            Some("opus"),
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert_eq!(task.model, Some("opus".to_string()));
    }

    #[test]
    fn test_add_skill() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            &["skill2".to_string()],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(task.skills.contains(&"skill2".to_string()));
        assert!(task.skills.contains(&"skill1".to_string()));
    }

    #[test]
    fn test_remove_skill() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
            &["skill1".to_string()],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(!task.skills.contains(&"skill1".to_string()));
    }

    #[test]
    fn test_task_not_found() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "nonexistent-task",
            Some("New Title"),
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_no_changes() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_self_blocking_rejected() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["test-task".to_string()],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot block itself")
        );
    }

    #[test]
    fn test_self_loop_rejected() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            Some("test-task"),
            Some(3),
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot loop to itself")
        );
    }

    #[test]
    fn test_add_blocked_by_updates_blocker_blocks() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph_with_two_tasks(temp_dir.path()).unwrap();

        // Add a new blocked_by edge
        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["blocker-task".to_string()],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();

        // Verify bidirectional consistency
        let blocker = graph.get_task("blocker-task").unwrap();
        assert!(
            blocker.blocks.contains(&"test-task".to_string()),
            "blocker-task.blocks should contain test-task"
        );
    }

    #[test]
    fn test_remove_blocked_by_updates_blocker_blocks() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph_with_two_tasks(temp_dir.path()).unwrap();

        // First add the dependency, then remove it
        run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["blocker-task".to_string()],
            &[],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // Remove the blocked_by edge
        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &["blocker-task".to_string()],
            &[],
            &[],
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();

        // Verify bidirectional consistency
        let blocker = graph.get_task("blocker-task").unwrap();
        assert!(
            !blocker.blocks.contains(&"test-task".to_string()),
            "blocker-task.blocks should NOT contain test-task after removal"
        );
    }
}
