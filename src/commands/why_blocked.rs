use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;
use workgraph::WorkGraph;
use workgraph::graph::{Status, Task};

/// Information about a blocking chain node
#[derive(Debug, Clone)]
struct BlockingNode {
    id: String,
    status: Status,
    children: Vec<BlockingNode>,
}

/// Root blocker information
#[derive(Debug, Clone)]
struct RootBlocker<'a> {
    task: &'a Task,
    is_ready: bool,
}

pub fn run(dir: &Path, id: &str, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let task = graph.get_task_or_err(id)?;

    // Build the blocking chain tree (resolves remote deps via federation)
    let mut visited = HashSet::new();
    let blocking_tree = build_blocking_tree(&graph, id, &mut visited, dir);

    // Find root blockers (tasks with no blockers of their own, and not done)
    let mut root_blocker_ids = HashSet::new();
    collect_root_blockers(&graph, &blocking_tree, &mut root_blocker_ids);

    let root_blockers: Vec<RootBlocker> = root_blocker_ids
        .iter()
        .filter_map(|rid| {
            // For remote refs, we can't get a &Task, but the blocking tree already
            // has the status. Root blockers from remote peers are only shown in the
            // tree; they won't appear here (since graph.get_task won't find them).
            graph.get_task(rid).map(|t| {
                let is_ready = is_task_ready(&graph, t, dir);
                RootBlocker { task: t, is_ready }
            })
        })
        .collect();

    // Count total blocking tasks
    let total_blockers = count_blockers(&blocking_tree);

    if json {
        print_json(task, &blocking_tree, &root_blockers, total_blockers)?;
    } else {
        print_human(task, &blocking_tree, &root_blockers, total_blockers);
    }

    Ok(())
}

fn build_blocking_tree(
    graph: &WorkGraph,
    task_id: &str,
    visited: &mut HashSet<String>,
    dir: &Path,
) -> BlockingNode {
    let task = graph.get_task(task_id);
    let status = task.map(|t| t.status).unwrap_or(Status::Open);

    let mut node = BlockingNode {
        id: task_id.to_string(),
        status,
        children: vec![],
    };

    if visited.contains(task_id) {
        return node; // Avoid cycles
    }
    visited.insert(task_id.to_string());

    if let Some(task) = task {
        for blocker_id in &task.blocked_by {
            // Skip if already visited (cycle detection)
            if visited.contains(blocker_id) {
                continue;
            }

            if let Some((_peer_name, _remote_task_id)) =
                workgraph::federation::parse_remote_ref(blocker_id)
            {
                // Cross-repo dependency — resolve remote status
                let remote = workgraph::federation::resolve_remote_task_status(
                    _peer_name,
                    _remote_task_id,
                    dir,
                );
                if !remote.status.is_terminal() {
                    let child = BlockingNode {
                        id: blocker_id.clone(),
                        status: remote.status,
                        children: vec![], // Don't recurse into remote graphs
                    };
                    node.children.push(child);
                }
            } else if let Some(blocker) = graph.get_task(blocker_id) {
                // Local dependency — only include if still actively blocking
                if !blocker.status.is_terminal() {
                    let child = build_blocking_tree(graph, blocker_id, visited, dir);
                    node.children.push(child);
                }
            }
        }
    }

    node
}

fn collect_root_blockers(graph: &WorkGraph, node: &BlockingNode, roots: &mut HashSet<String>) {
    if node.children.is_empty() {
        // This node has no blockers - check if it's actually a blocker (not the root task)
        if let Some(task) = graph.get_task(&node.id) {
            // It's a root blocker if it's not terminal (still open, in-progress, or blocked)
            if !task.status.is_terminal() {
                roots.insert(node.id.clone());
            }
        }
    } else {
        for child in &node.children {
            collect_root_blockers(graph, child, roots);
        }
    }
}

fn is_task_ready(graph: &WorkGraph, task: &Task, dir: &Path) -> bool {
    if task.status != Status::Open {
        return false;
    }
    task.blocked_by.iter().all(|blocker_id| {
        workgraph::query::is_blocker_satisfied(blocker_id, graph, Some(dir))
    })
}

fn count_blockers(node: &BlockingNode) -> usize {
    let mut count = 0;
    let mut visited = HashSet::new();
    count_blockers_recursive(node, &mut count, &mut visited);
    count
}

fn count_blockers_recursive(node: &BlockingNode, count: &mut usize, visited: &mut HashSet<String>) {
    for child in &node.children {
        if !visited.contains(&child.id) {
            visited.insert(child.id.clone());
            *count += 1;
            count_blockers_recursive(child, count, visited);
        }
    }
}

fn print_human(task: &Task, tree: &BlockingNode, root_blockers: &[RootBlocker], total: usize) {
    println!("Task: {}", task.id);

    if tree.children.is_empty() {
        println!("Status: {:?}", task.status);
        println!();
        println!("{} has no blockers.", task.id);
        return;
    }

    println!("Status: blocked (transitively)");
    println!();
    println!("Blocking chain:");
    println!();
    print_tree(tree, "", 0);

    if !root_blockers.is_empty() {
        println!();
        println!("Root blockers (actionable now):");
        for rb in root_blockers {
            let assigned = rb
                .task
                .assigned
                .as_ref()
                .map(|a| format!(", assigned to {}", a))
                .unwrap_or_else(|| ", unassigned".to_string());
            let ready_str = if rb.is_ready { ", ready to start" } else { "" };
            println!(
                "  - {}: {:?}{}{}",
                rb.task.id, rb.task.status, assigned, ready_str
            );
        }
    }

    println!();
    if root_blockers.len() == 1 {
        println!(
            "Summary: {} is blocked by {} task{}; unblock {} to make progress.",
            task.id,
            total,
            if total == 1 { "" } else { "s" },
            root_blockers[0].task.id
        );
    } else if root_blockers.is_empty() {
        println!(
            "Summary: {} is blocked by {} task{}.",
            task.id,
            total,
            if total == 1 { "" } else { "s" }
        );
    } else {
        let ids: Vec<&str> = root_blockers.iter().map(|rb| rb.task.id.as_str()).collect();
        println!(
            "Summary: {} is blocked by {} task{}; unblock {} to make progress.",
            task.id,
            total,
            if total == 1 { "" } else { "s" },
            ids.join(" or ")
        );
    }
}

fn print_tree(node: &BlockingNode, prefix: &str, depth: usize) {
    if depth == 0 {
        // Root node - just print the ID
        println!("{}", node.id);
    } else {
        // Child node - print with tree connector and status
        let status_str = format!("(status: {:?})", node.status);
        let root_marker = if node.children.is_empty() && !node.status.is_terminal() {
            " <-- ROOT CAUSE"
        } else {
            ""
        };
        println!(
            "{} \\-- blocked by: {} {}{}",
            prefix, node.id, status_str, root_marker
        );
    }

    // Calculate the prefix for children
    let child_prefix = if depth == 0 {
        "".to_string()
    } else {
        format!("{}     ", prefix)
    };

    for child in &node.children {
        print_tree(child, &child_prefix, depth + 1);
    }
}

fn print_json(
    task: &Task,
    tree: &BlockingNode,
    root_blockers: &[RootBlocker],
    total: usize,
) -> Result<()> {
    let output = serde_json::json!({
        "task": {
            "id": task.id,
            "title": task.title,
            "status": task.status,
        },
        "is_blocked": !tree.children.is_empty(),
        "blocking_chain": tree_to_json(tree),
        "root_blockers": root_blockers.iter().map(|rb| {
            serde_json::json!({
                "id": rb.task.id,
                "title": rb.task.title,
                "status": rb.task.status,
                "assigned": rb.task.assigned,
                "is_ready": rb.is_ready,
            })
        }).collect::<Vec<_>>(),
        "total_blockers": total,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn tree_to_json(node: &BlockingNode) -> serde_json::Value {
    serde_json::json!({
        "id": node.id,
        "status": format!("{:?}", node.status),
        "blocked_by": node.children.iter().map(tree_to_json).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Task};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn test_build_blocking_tree_no_blockers() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));

        let mut visited = HashSet::new();
        let dir = Path::new("/tmp");
        let tree = build_blocking_tree(&graph, "t1", &mut visited, dir);

        assert_eq!(tree.id, "t1");
        assert!(tree.children.is_empty());
    }

    #[test]
    fn test_build_blocking_tree_single_blocker() {
        let mut graph = WorkGraph::new();

        let blocker = make_task("blocker", "Blocker");
        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let mut visited = HashSet::new();
        let dir = Path::new("/tmp");
        let tree = build_blocking_tree(&graph, "blocked", &mut visited, dir);

        assert_eq!(tree.id, "blocked");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].id, "blocker");
    }

    #[test]
    fn test_build_blocking_tree_chain() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t2".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let mut visited = HashSet::new();
        let dir = Path::new("/tmp");
        let tree = build_blocking_tree(&graph, "t3", &mut visited, dir);

        assert_eq!(tree.id, "t3");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].id, "t2");
        assert_eq!(tree.children[0].children.len(), 1);
        assert_eq!(tree.children[0].children[0].id, "t1");
    }

    #[test]
    fn test_build_blocking_tree_excludes_done() {
        let mut graph = WorkGraph::new();

        let mut blocker = make_task("blocker", "Blocker");
        blocker.status = Status::Done;

        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let mut visited = HashSet::new();
        let dir = Path::new("/tmp");
        let tree = build_blocking_tree(&graph, "blocked", &mut visited, dir);

        assert_eq!(tree.id, "blocked");
        assert!(tree.children.is_empty()); // Done blocker excluded
    }

    #[test]
    fn test_build_blocking_tree_handles_cycles() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["t2".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let mut visited = HashSet::new();
        let dir = Path::new("/tmp");
        let tree = build_blocking_tree(&graph, "t1", &mut visited, dir);

        // Should not infinite loop - t2 will be a child but t1 won't be repeated
        assert_eq!(tree.id, "t1");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].id, "t2");
        // t2's children should be empty because t1 was already visited
        assert!(tree.children[0].children.is_empty());
    }

    #[test]
    fn test_collect_root_blockers() {
        let mut graph = WorkGraph::new();

        let root = make_task("root", "Root");
        let mut mid = make_task("mid", "Middle");
        mid.blocked_by = vec!["root".to_string()];
        let mut leaf = make_task("leaf", "Leaf");
        leaf.blocked_by = vec!["mid".to_string()];

        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(mid));
        graph.add_node(Node::Task(leaf));

        let mut visited = HashSet::new();
        let dir = Path::new("/tmp");
        let tree = build_blocking_tree(&graph, "leaf", &mut visited, dir);

        let mut roots = HashSet::new();
        collect_root_blockers(&graph, &tree, &mut roots);

        assert_eq!(roots.len(), 1);
        assert!(roots.contains("root"));
    }

    #[test]
    fn test_count_blockers() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t2".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let mut visited = HashSet::new();
        let dir = Path::new("/tmp");
        let tree = build_blocking_tree(&graph, "t3", &mut visited, dir);

        assert_eq!(count_blockers(&tree), 2);
    }

    #[test]
    fn test_is_task_ready() {
        let mut graph = WorkGraph::new();

        let mut blocker = make_task("blocker", "Blocker");
        blocker.status = Status::Done;

        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked.clone()));

        let dir = Path::new("/tmp");

        // blocked task is ready because blocker is done
        assert!(is_task_ready(&graph, &blocked, dir));

        // Now test with an open blocker
        let mut graph2 = WorkGraph::new();
        let blocker2 = make_task("blocker", "Blocker");
        let mut blocked2 = make_task("blocked", "Blocked");
        blocked2.blocked_by = vec!["blocker".to_string()];

        graph2.add_node(Node::Task(blocker2));
        graph2.add_node(Node::Task(blocked2.clone()));

        assert!(!is_task_ready(&graph2, &blocked2, dir));
    }

    #[test]
    fn test_collect_root_blockers_includes_in_progress() {
        let mut graph = WorkGraph::new();

        let mut root = make_task("root", "Root");
        root.status = Status::InProgress;
        let mut leaf = make_task("leaf", "Leaf");
        leaf.blocked_by = vec!["root".to_string()];

        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(leaf));

        let mut visited = HashSet::new();
        let dir = Path::new("/tmp");
        let tree = build_blocking_tree(&graph, "leaf", &mut visited, dir);

        let mut roots = HashSet::new();
        collect_root_blockers(&graph, &tree, &mut roots);

        assert_eq!(roots.len(), 1);
        assert!(roots.contains("root"));
    }
}
