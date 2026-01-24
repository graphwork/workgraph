use crate::graph::WorkGraph;
use std::collections::HashSet;

/// Result of checking the graph for issues
#[derive(Debug, Clone, Default)]
pub struct CheckResult {
    pub cycles: Vec<Vec<String>>,
    pub orphan_refs: Vec<OrphanRef>,
    pub ok: bool,
}

/// A reference to a non-existent node
#[derive(Debug, Clone)]
pub struct OrphanRef {
    pub from: String,
    pub to: String,
    pub relation: String,
}

/// Check for cycles in task dependencies
pub fn check_cycles(graph: &WorkGraph) -> Vec<Vec<String>> {
    let mut cycles = Vec::new();
    let mut visited = HashSet::new();
    let mut rec_stack = HashSet::new();
    let mut path = Vec::new();

    for task in graph.tasks() {
        if !visited.contains(&task.id) {
            find_cycles(
                graph,
                &task.id,
                &mut visited,
                &mut rec_stack,
                &mut path,
                &mut cycles,
            );
        }
    }

    cycles
}

fn find_cycles(
    graph: &WorkGraph,
    node_id: &str,
    visited: &mut HashSet<String>,
    rec_stack: &mut HashSet<String>,
    path: &mut Vec<String>,
    cycles: &mut Vec<Vec<String>>,
) {
    visited.insert(node_id.to_string());
    rec_stack.insert(node_id.to_string());
    path.push(node_id.to_string());

    if let Some(task) = graph.get_task(node_id) {
        // Follow blocked_by edges (A blocked_by B means A depends on B)
        for dep_id in &task.blocked_by {
            if !visited.contains(dep_id) {
                find_cycles(graph, dep_id, visited, rec_stack, path, cycles);
            } else if rec_stack.contains(dep_id) {
                // Found a cycle - extract the cycle from path
                if let Some(pos) = path.iter().position(|x| x == dep_id) {
                    let cycle: Vec<String> = path[pos..].to_vec();
                    cycles.push(cycle);
                }
            }
        }
    }

    path.pop();
    rec_stack.remove(node_id);
}

/// Check for references to non-existent nodes
pub fn check_orphans(graph: &WorkGraph) -> Vec<OrphanRef> {
    let mut orphans = Vec::new();

    for task in graph.tasks() {
        for blocked_by in &task.blocked_by {
            if graph.get_node(blocked_by).is_none() {
                orphans.push(OrphanRef {
                    from: task.id.clone(),
                    to: blocked_by.clone(),
                    relation: "blocked_by".to_string(),
                });
            }
        }

        for blocks in &task.blocks {
            if graph.get_node(blocks).is_none() {
                orphans.push(OrphanRef {
                    from: task.id.clone(),
                    to: blocks.clone(),
                    relation: "blocks".to_string(),
                });
            }
        }

        if let Some(ref assigned) = task.assigned
            && graph.get_actor(assigned).is_none()
        {
            orphans.push(OrphanRef {
                from: task.id.clone(),
                to: assigned.clone(),
                relation: "assigned".to_string(),
            });
        }

        for requires in &task.requires {
            if graph.get_resource(requires).is_none() {
                orphans.push(OrphanRef {
                    from: task.id.clone(),
                    to: requires.clone(),
                    relation: "requires".to_string(),
                });
            }
        }
    }

    orphans
}

/// Run all checks and return a summary
pub fn check_all(graph: &WorkGraph) -> CheckResult {
    let cycles = check_cycles(graph);
    let orphan_refs = check_orphans(graph);
    let ok = cycles.is_empty() && orphan_refs.is_empty();

    CheckResult {
        cycles,
        orphan_refs,
        ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Actor, Node, Status, Task, TrustLevel};

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

    fn make_actor(id: &str) -> Actor {
        Actor {
            id: id.to_string(),
            name: None,
            role: None,
            rate: None,
            capacity: None,
            capabilities: vec![],
            context_limit: None,
            trust_level: TrustLevel::Provisional,
            last_seen: None,
        }
    }

    #[test]
    fn test_no_cycles_in_empty_graph() {
        let graph = WorkGraph::new();
        let cycles = check_cycles(&graph);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_no_cycles_in_linear_chain() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t2".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let cycles = check_cycles(&graph);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_detects_simple_cycle() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["t2".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let cycles = check_cycles(&graph);
        assert!(!cycles.is_empty());
    }

    #[test]
    fn test_detects_three_node_cycle() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["t3".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t2".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let cycles = check_cycles(&graph);
        assert!(!cycles.is_empty());
    }

    #[test]
    fn test_no_orphans_in_empty_graph() {
        let graph = WorkGraph::new();
        let orphans = check_orphans(&graph);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_no_orphans_with_valid_refs() {
        let mut graph = WorkGraph::new();

        let actor = make_actor("erik");
        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("erik".to_string());

        graph.add_node(Node::Actor(actor));
        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_detects_orphan_blocked_by() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.blocked_by = vec!["nonexistent".to_string()];

        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].to, "nonexistent");
        assert_eq!(orphans[0].relation, "blocked_by");
    }

    #[test]
    fn test_detects_orphan_assigned() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("ghost".to_string());

        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].to, "ghost");
        assert_eq!(orphans[0].relation, "assigned");
    }

    #[test]
    fn test_check_all_returns_ok_for_valid_graph() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));

        let result = check_all(&graph);
        assert!(result.ok);
        assert!(result.cycles.is_empty());
        assert!(result.orphan_refs.is_empty());
    }

    #[test]
    fn test_check_all_returns_not_ok_for_invalid_graph() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["t2".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let result = check_all(&graph);
        assert!(!result.ok);
        assert!(!result.cycles.is_empty());
    }
}
