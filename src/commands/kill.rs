//! Kill running agents
//!
//! Terminates agent processes and cleans up their registry entries.
//! By default, kills also pause the agent's task to prevent re-dispatch.
//!
//! Usage:
//!   wg kill agent-1              # Kill + pause task (default)
//!   wg kill agent-1 --redispatch # Kill but leave task open for re-dispatch
//!   wg kill agent-1 --force      # Force kill (SIGKILL immediately)
//!   wg kill --all                # Kill all agents + pause their tasks
//!   wg kill --tree <task-id>     # Kill agent + all downstream tasks
//!   wg kill --tree <task> --dry-run  # Show what would be killed

use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::HashSet;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::modify_graph;
use workgraph::query::build_reverse_index;
use workgraph::service::{AgentRegistry, AgentStatus};

use super::{collect_transitive_dependents, graph_path, kill_process_force, kill_process_graceful};

/// Default wait time between SIGTERM and SIGKILL
const DEFAULT_WAIT_SECS: u64 = 5;

/// Kill a single agent
pub fn run(dir: &Path, agent_id: &str, force: bool, redispatch: bool, json: bool) -> Result<()> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    let agent = locked_registry
        .get_agent(agent_id)
        .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found", agent_id))?;

    let pid = agent.pid;
    let task_id = agent.task_id.clone();

    // Kill the process
    if force {
        kill_process_force(pid)?;
    } else {
        kill_process_graceful(pid, DEFAULT_WAIT_SECS)?;
    }

    // Update registry
    locked_registry.update_status(agent_id, AgentStatus::Stopping)?;
    locked_registry.save_ref()?;

    // Unclaim the task (and pause unless --redispatch)
    unclaim_task(dir, &task_id, agent_id, !redispatch)?;

    // Remove agent from registry
    locked_registry.unregister_agent(agent_id);
    locked_registry.save()?;

    let paused = !redispatch;
    if json {
        let output = serde_json::json!({
            "killed": agent_id,
            "pid": pid,
            "task_id": task_id,
            "force": force,
            "paused": paused,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if force {
            println!("Force killed {} (PID {})", agent_id, pid);
        } else {
            println!("Killed {} (PID {})", agent_id, pid);
        }
        if paused {
            println!("Task '{}' paused (use 'wg resume {}' to re-enable dispatch)", task_id, task_id);
        } else {
            println!("Task '{}' unclaimed (will be re-dispatched)", task_id);
        }
    }

    Ok(())
}

/// Kill all running agents
pub fn run_all(dir: &Path, force: bool, redispatch: bool, json: bool) -> Result<()> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Get all alive agents
    let alive_agents: Vec<_> = locked_registry
        .list_alive_agents()
        .iter()
        .map(|a| (a.id.clone(), a.pid, a.task_id.clone()))
        .collect();

    if alive_agents.is_empty() {
        if json {
            let output = serde_json::json!({
                "killed": [],
                "count": 0,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("No running agents to kill.");
        }
        return Ok(());
    }

    let pause = !redispatch;
    let mut killed = Vec::new();
    let mut errors = Vec::new();

    for (agent_id, pid, task_id) in &alive_agents {
        // Kill the process
        let kill_result = if force {
            kill_process_force(*pid)
        } else {
            kill_process_graceful(*pid, DEFAULT_WAIT_SECS)
        };

        if let Err(e) = kill_result {
            errors.push(format!("{}: {}", agent_id, e));
            continue;
        }

        // Update status
        if let Err(e) = locked_registry.update_status(agent_id, AgentStatus::Stopping) {
            eprintln!(
                "Warning: failed to update status for agent {}: {}",
                agent_id, e
            );
        }

        // Unclaim task (and pause unless --redispatch)
        if let Err(e) = unclaim_task(dir, task_id, agent_id, pause) {
            errors.push(format!("Failed to unclaim task '{}': {}", task_id, e));
            continue;
        }

        // Remove from registry only after successful unclaim
        locked_registry.unregister_agent(agent_id);

        killed.push((agent_id.clone(), *pid, task_id.clone()));
    }

    locked_registry.save()?;

    if json {
        let output = serde_json::json!({
            "killed": killed.iter().map(|(id, pid, task)| {
                serde_json::json!({
                    "id": id,
                    "pid": pid,
                    "task_id": task,
                })
            }).collect::<Vec<_>>(),
            "count": killed.len(),
            "paused": pause,
            "errors": errors,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if killed.is_empty() {
            println!("No agents were killed.");
        } else {
            println!("Killed {} agent(s){}:", killed.len(), if pause { " (tasks paused)" } else { "" });
            for (id, pid, task) in &killed {
                println!("  {} (PID {}) - task '{}'", id, pid, task);
            }
        }

        if !errors.is_empty() {
            eprintln!();
            eprintln!("Errors:");
            for err in &errors {
                eprintln!("  {}", err);
            }
        }
    }

    Ok(())
}

/// Kill agents and abandon tasks for a task and all its downstream dependents.
///
/// 1. Finds the target task and all transitive dependents
/// 2. Kills agents working on any of those tasks
/// 3. Abandons all those tasks (unless --no-abandon)
/// 4. Prints summary
pub fn run_tree(
    dir: &Path,
    task_id: &str,
    force: bool,
    dry_run: bool,
    no_abandon: bool,
    json: bool,
) -> Result<()> {
    let path = graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Load graph to find all downstream tasks
    let (graph, _path) = super::load_workgraph(dir)?;

    // Verify the root task exists
    graph
        .get_task(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Build reverse index and find all transitive dependents
    let reverse_index = build_reverse_index(&graph);
    let mut downstream: HashSet<String> = HashSet::new();
    collect_transitive_dependents(&reverse_index, task_id, &mut downstream);

    // The full set: root task + all downstream
    let mut all_task_ids: Vec<String> = vec![task_id.to_string()];
    all_task_ids.extend(downstream);
    all_task_ids.sort();

    // Find agents working on any of these tasks
    let locked_registry = AgentRegistry::load_locked(dir);
    let mut agents_to_kill: Vec<(String, u32, String)> = Vec::new(); // (agent_id, pid, task_id)

    if let Ok(ref registry) = locked_registry {
        for tid in &all_task_ids {
            // Check alive agents for this task
            for agent in registry.list_alive_agents() {
                if agent.task_id == *tid {
                    agents_to_kill.push((agent.id.clone(), agent.pid, agent.task_id.clone()));
                }
            }
        }
    }
    // Drop the registry lock before dry-run output
    drop(locked_registry);

    // Determine which tasks to abandon (non-terminal ones)
    let tasks_to_abandon: Vec<String> = all_task_ids
        .iter()
        .filter(|tid| {
            if let Some(t) = graph.get_task(tid) {
                !t.status.is_terminal()
            } else {
                false
            }
        })
        .cloned()
        .collect();

    if dry_run {
        if json {
            let output = serde_json::json!({
                "dry_run": true,
                "root_task": task_id,
                "agents_to_kill": agents_to_kill.iter().map(|(id, pid, tid)| {
                    serde_json::json!({ "id": id, "pid": pid, "task_id": tid })
                }).collect::<Vec<_>>(),
                "tasks_to_abandon": if no_abandon { vec![] } else { tasks_to_abandon.clone() },
                "total_tasks_in_tree": all_task_ids.len(),
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("Dry run — no changes will be made\n");
            println!(
                "Tree rooted at '{}' ({} tasks total):",
                task_id,
                all_task_ids.len()
            );
            for tid in &all_task_ids {
                let status = graph
                    .get_task(tid)
                    .map(|t| format!("{:?}", t.status))
                    .unwrap_or_else(|| "unknown".to_string());
                let agent_info = agents_to_kill
                    .iter()
                    .find(|(_, _, atid)| atid == tid)
                    .map(|(aid, pid, _)| format!(" [agent: {} PID {}]", aid, pid))
                    .unwrap_or_default();
                println!("  {} ({}){}", tid, status, agent_info);
            }
            println!();
            println!("Would kill {} agent(s)", agents_to_kill.len());
            if no_abandon {
                println!("Would NOT abandon tasks (--no-abandon)");
            } else {
                println!("Would abandon {} task(s)", tasks_to_abandon.len());
            }
        }
        return Ok(());
    }

    // Actually kill agents
    let mut killed_agents: Vec<(String, u32, String)> = Vec::new();
    let mut kill_errors: Vec<String> = Vec::new();

    if !agents_to_kill.is_empty() {
        let mut locked_registry = AgentRegistry::load_locked(dir)?;
        for (agent_id, pid, tid) in &agents_to_kill {
            let kill_result = if force {
                kill_process_force(*pid)
            } else {
                kill_process_graceful(*pid, DEFAULT_WAIT_SECS)
            };

            match kill_result {
                Ok(()) => {
                    if let Err(e) = locked_registry.update_status(agent_id, AgentStatus::Stopping) {
                        eprintln!(
                            "Warning: failed to update status for agent {}: {}",
                            agent_id, e
                        );
                    }
                    locked_registry.unregister_agent(agent_id);
                    killed_agents.push((agent_id.clone(), *pid, tid.clone()));
                }
                Err(e) => {
                    // Agent may have already exited — skip gracefully
                    kill_errors.push(format!("{} (PID {}): {}", agent_id, pid, e));
                    // Still unregister if the process is gone
                    if !super::is_process_alive(*pid) {
                        locked_registry.unregister_agent(agent_id);
                        killed_agents.push((agent_id.clone(), *pid, tid.clone()));
                    }
                }
            }
        }
        locked_registry.save()?;
    }

    // Abandon tasks (unless --no-abandon)
    let mut abandoned_tasks: Vec<String> = Vec::new();
    if !no_abandon && !tasks_to_abandon.is_empty() {
        modify_graph(&path, |graph| {
            let mut changed = false;
            for tid in &tasks_to_abandon {
                if let Some(task) = graph.get_task_mut(tid)
                    && !task.status.is_terminal()
                {
                    task.status = Status::Abandoned;
                    task.assigned = None;
                    task.failure_reason = Some(format!("Tree-killed from root task '{}'", task_id));
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: None,
                        user: Some(workgraph::current_user()),
                        message: format!(
                            "Tree-killed: abandoned as part of cascade from '{}'",
                            task_id
                        ),
                    });
                    abandoned_tasks.push(tid.clone());
                    changed = true;
                }
            }
            changed
        })
        .context("Failed to modify graph")?;

        super::notify_graph_changed(dir);
    }

    // Print summary
    if json {
        let output = serde_json::json!({
            "root_task": task_id,
            "killed_agents": killed_agents.iter().map(|(id, pid, tid)| {
                serde_json::json!({ "id": id, "pid": pid, "task_id": tid })
            }).collect::<Vec<_>>(),
            "abandoned_tasks": abandoned_tasks,
            "total_tasks_in_tree": all_task_ids.len(),
            "errors": kill_errors,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "Killed {} agent(s), abandoned {} task(s)",
            killed_agents.len(),
            abandoned_tasks.len()
        );

        if !killed_agents.is_empty() {
            println!("\nKilled agents:");
            for (id, pid, tid) in &killed_agents {
                println!("  {} (PID {}) — task '{}'", id, pid, tid);
            }
        }

        if !abandoned_tasks.is_empty() {
            println!("\nAbandoned tasks:");
            for tid in &abandoned_tasks {
                println!("  {}", tid);
            }
        }

        if !kill_errors.is_empty() {
            eprintln!("\nWarnings:");
            for err in &kill_errors {
                eprintln!("  {}", err);
            }
        }
    }

    Ok(())
}

/// Unclaim the task that was being worked on by the killed agent.
/// When `pause` is true, also sets the task's paused flag to prevent re-dispatch.
fn unclaim_task(dir: &Path, task_id: &str, agent_id: &str, pause: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        return Ok(()); // No graph, nothing to unclaim
    }

    modify_graph(&path, |graph| {
        if let Some(task) = graph.get_task_mut(task_id)
            && task.status == Status::InProgress
        {
            task.status = Status::Open;
            task.assigned = None;

            if pause {
                task.paused = true;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    user: Some(workgraph::current_user()),
                    message: format!(
                        "Agent '{}' killed — task auto-paused (use 'wg resume' to re-enable dispatch)",
                        agent_id
                    ),
                });
            } else {
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    user: Some(workgraph::current_user()),
                    message: format!("Task unclaimed: agent '{}' was killed (--redispatch)", agent_id),
                });
            }

            return true;
        }
        false
    })
    .context("Failed to modify graph")?;

    super::notify_graph_changed(dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};
    use workgraph::service::is_process_alive;

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Task::default()
        }
    }

    fn setup_with_agent_and_task() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        // Create graph with a task assigned to the agent
        let mut graph = WorkGraph::new();
        let mut task = make_task("task-1", "Test Task", Status::InProgress);
        task.assigned = Some("test-agent".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        // Register an agent with a fake PID (use PID 1 which should always exist on Unix)
        let mut registry = AgentRegistry::new();
        registry.register_agent(1, "task-1", "claude", "/tmp/output.log");
        registry.save(temp_dir.path()).unwrap();

        temp_dir
    }

    #[test]
    fn test_is_process_alive() {
        // Current process should always be running
        #[cfg(unix)]
        {
            let pid = std::process::id();
            assert!(is_process_alive(pid));
        }

        // Random high PID likely doesn't exist
        #[cfg(unix)]
        assert!(!is_process_alive(999999999));
    }

    #[test]
    fn test_unclaim_task_with_pause() {
        let temp_dir = setup_with_agent_and_task();

        // Unclaim with pause (default behavior)
        let result = unclaim_task(temp_dir.path(), "task-1", "agent-1", true);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert!(task.paused, "Task should be paused after kill");
        assert!(task.log.last().unwrap().message.contains("auto-paused"));
    }

    #[test]
    fn test_unclaim_task_with_redispatch() {
        let temp_dir = setup_with_agent_and_task();

        // Unclaim without pause (--redispatch behavior)
        let result = unclaim_task(temp_dir.path(), "task-1", "agent-1", false);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert!(!task.paused, "Task should NOT be paused with --redispatch");
        assert!(task.log.last().unwrap().message.contains("--redispatch"));
    }

    #[test]
    fn test_unclaim_task_not_in_progress() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        // Create graph with a done task
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("task-1", "Done Task", Status::Done)));
        save_graph(&graph, &path).unwrap();

        // Unclaim should succeed but not change anything (task already terminal)
        let result = unclaim_task(temp_dir.path(), "task-1", "agent-1", true);
        assert!(result.is_ok());

        // Verify task is still done and NOT paused
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert_eq!(task.status, Status::Done);
        assert!(!task.paused);
    }

    #[test]
    fn test_run_all_empty() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // No agents registered
        let result = run_all(temp_dir.path(), false, false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_kill_agent_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "agent-999", false, false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // Note: Can't easily test actual process killing in unit tests
    // as it would require spawning real processes. The kill functions
    // are tested manually or in integration tests.

    // --- Tree kill tests ---

    fn setup_tree_graph(dir: &Path) {
        let path = dir.join("graph.jsonl");

        // Build a tree:  root -> child-a -> grandchild
        //                root -> child-b
        let mut graph = WorkGraph::new();

        let root = make_task("root", "Root Task", Status::InProgress);
        graph.add_node(Node::Task(root));

        let mut child_a = make_task("child-a", "Child A", Status::InProgress);
        child_a.after = vec!["root".to_string()];
        graph.add_node(Node::Task(child_a));

        let mut child_b = make_task("child-b", "Child B", Status::Open);
        child_b.after = vec!["root".to_string()];
        graph.add_node(Node::Task(child_b));

        let mut grandchild = make_task("grandchild", "Grandchild", Status::Open);
        grandchild.after = vec!["child-a".to_string()];
        graph.add_node(Node::Task(grandchild));

        save_graph(&graph, &path).unwrap();
    }

    #[test]
    fn test_kill_tree_dry_run_shows_tree() {
        let temp_dir = TempDir::new().unwrap();
        setup_tree_graph(temp_dir.path());

        let result = run_tree(temp_dir.path(), "root", false, true, false, false);
        assert!(result.is_ok());

        // Verify nothing was changed (dry run)
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        assert_eq!(graph.get_task("root").unwrap().status, Status::InProgress);
        assert_eq!(
            graph.get_task("child-a").unwrap().status,
            Status::InProgress
        );
        assert_eq!(graph.get_task("child-b").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("grandchild").unwrap().status, Status::Open);
    }

    #[test]
    fn test_kill_tree_abandons_all_downstream() {
        let temp_dir = TempDir::new().unwrap();
        setup_tree_graph(temp_dir.path());

        // No agents registered, so no kills — just abandon
        let result = run_tree(temp_dir.path(), "root", false, false, false, false);
        assert!(result.is_ok());

        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        assert_eq!(graph.get_task("root").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("child-a").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("child-b").unwrap().status, Status::Abandoned);
        assert_eq!(
            graph.get_task("grandchild").unwrap().status,
            Status::Abandoned
        );

        // Check failure reason
        assert!(
            graph
                .get_task("child-a")
                .unwrap()
                .failure_reason
                .as_ref()
                .unwrap()
                .contains("root")
        );
    }

    #[test]
    fn test_kill_tree_no_abandon_leaves_tasks_unchanged() {
        let temp_dir = TempDir::new().unwrap();
        setup_tree_graph(temp_dir.path());

        let result = run_tree(temp_dir.path(), "root", false, false, true, false);
        assert!(result.is_ok());

        // Tasks should NOT be abandoned with --no-abandon
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        assert_eq!(graph.get_task("root").unwrap().status, Status::InProgress);
        assert_eq!(
            graph.get_task("child-a").unwrap().status,
            Status::InProgress
        );
        assert_eq!(graph.get_task("child-b").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("grandchild").unwrap().status, Status::Open);
    }

    #[test]
    fn test_kill_tree_no_downstream_deps() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        // Single task, no dependents
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "solo",
            "Solo Task",
            Status::InProgress,
        )));
        save_graph(&graph, &path).unwrap();

        let result = run_tree(temp_dir.path(), "solo", false, false, false, false);
        assert!(result.is_ok());

        let graph = load_graph(&path).unwrap();
        assert_eq!(graph.get_task("solo").unwrap().status, Status::Abandoned);
    }

    #[test]
    fn test_kill_tree_skips_already_terminal_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let root = make_task("root", "Root", Status::InProgress);
        graph.add_node(Node::Task(root));

        let mut done_child = make_task("done-child", "Done Child", Status::Done);
        done_child.after = vec!["root".to_string()];
        graph.add_node(Node::Task(done_child));

        let mut open_child = make_task("open-child", "Open Child", Status::Open);
        open_child.after = vec!["root".to_string()];
        graph.add_node(Node::Task(open_child));

        save_graph(&graph, &path).unwrap();

        let result = run_tree(temp_dir.path(), "root", false, false, false, false);
        assert!(result.is_ok());

        let graph = load_graph(&path).unwrap();
        assert_eq!(graph.get_task("root").unwrap().status, Status::Abandoned);
        // Done task should remain Done
        assert_eq!(graph.get_task("done-child").unwrap().status, Status::Done);
        // Open task should be abandoned
        assert_eq!(
            graph.get_task("open-child").unwrap().status,
            Status::Abandoned
        );
    }

    #[test]
    fn test_kill_tree_task_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run_tree(temp_dir.path(), "nonexistent", false, false, false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_kill_tree_json_output() {
        let temp_dir = TempDir::new().unwrap();
        setup_tree_graph(temp_dir.path());

        // JSON dry run should succeed
        let result = run_tree(temp_dir.path(), "root", false, true, false, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_kill_tree_diamond_dependency() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        // Diamond: root -> a, b -> merge
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("root", "Root", Status::InProgress)));

        let mut a = make_task("a", "A", Status::Open);
        a.after = vec!["root".to_string()];
        graph.add_node(Node::Task(a));

        let mut b = make_task("b", "B", Status::Open);
        b.after = vec!["root".to_string()];
        graph.add_node(Node::Task(b));

        let mut merge = make_task("merge", "Merge", Status::Open);
        merge.after = vec!["a".to_string(), "b".to_string()];
        graph.add_node(Node::Task(merge));

        save_graph(&graph, &path).unwrap();

        let result = run_tree(temp_dir.path(), "root", false, false, false, false);
        assert!(result.is_ok());

        let graph = load_graph(&path).unwrap();
        assert_eq!(graph.get_task("root").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("a").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("b").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("merge").unwrap().status, Status::Abandoned);
    }
}
