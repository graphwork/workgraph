use crate::graph::WorkGraph;
use serde::Serialize;
use std::collections::HashSet;

/// Result of checking the graph for issues
#[derive(Debug, Clone, Default, Serialize)]
pub struct CheckResult {
    pub cycles: Vec<Vec<String>>,
    pub orphan_refs: Vec<OrphanRef>,
    pub stale_assignments: Vec<StaleAssignment>,
    pub stuck_blocked: Vec<StuckBlocked>,
    pub ok: bool,
}

/// A reference to a non-existent node
#[derive(Debug, Clone, Serialize)]
pub struct OrphanRef {
    pub from: String,
    pub to: String,
    pub relation: String,
}

/// A task with status=open but an agent assigned (may indicate a dead agent)
#[derive(Debug, Clone, Serialize)]
pub struct StaleAssignment {
    pub task_id: String,
    pub assigned: String,
}

/// A task with status=Blocked where all after tasks have terminal status
/// (done/failed/abandoned). These tasks should have been transitioned to Open but weren't.
#[derive(Debug, Clone, Serialize)]
pub struct StuckBlocked {
    pub task_id: String,
    pub after_ids: Vec<String>,
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
        // Follow after edges (A after B means A depends on B)
        for dep_id in &task.after {
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

/// Check for tasks with status=open but an agent assigned (stale assignments)
pub fn check_stale_assignments(graph: &WorkGraph) -> Vec<StaleAssignment> {
    let mut stale = Vec::new();

    for task in graph.tasks() {
        if task.status == crate::graph::Status::Open
            && let Some(assigned) = &task.assigned
        {
            stale.push(StaleAssignment {
                task_id: task.id.clone(),
                assigned: assigned.clone(),
            });
        }
    }

    stale
}

/// Check for tasks with status=Blocked where all after tasks have terminal status.
/// These tasks should have been transitioned to Open but weren't — they're stuck.
pub fn check_stuck_blocked(graph: &WorkGraph) -> Vec<StuckBlocked> {
    let mut stuck = Vec::new();

    for task in graph.tasks() {
        if task.status != crate::graph::Status::Blocked {
            continue;
        }
        if task.after.is_empty() {
            continue;
        }
        let all_terminal = task.after.iter().all(|dep_id| {
            graph
                .get_task(dep_id)
                .is_some_and(|dep| dep.status.is_terminal())
        });
        if all_terminal {
            stuck.push(StuckBlocked {
                task_id: task.id.clone(),
                after_ids: task.after.clone(),
            });
        }
    }

    stuck
}

/// Check for references to non-existent nodes
pub fn check_orphans(graph: &WorkGraph) -> Vec<OrphanRef> {
    let mut orphans = Vec::new();

    for task in graph.tasks() {
        for after in &task.after {
            if graph.get_node(after).is_none() {
                orphans.push(OrphanRef {
                    from: task.id.clone(),
                    to: after.clone(),
                    relation: "after".to_string(),
                });
            }
        }

        for blocks in &task.before {
            if graph.get_node(blocks).is_none() {
                orphans.push(OrphanRef {
                    from: task.id.clone(),
                    to: blocks.clone(),
                    relation: "before".to_string(),
                });
            }
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
    let stale_assignments = check_stale_assignments(graph);
    let stuck_blocked = check_stuck_blocked(graph);

    // Cycles, stale assignments, and stuck blocked are warnings, not errors —
    // only orphan refs make the graph invalid
    let ok = orphan_refs.is_empty();

    CheckResult {
        cycles,
        orphan_refs,
        stale_assignments,
        stuck_blocked,
        ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Node, Status};
    use crate::test_helpers::make_task;

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
        t2.after = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.after = vec!["t2".to_string()];

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
        t1.after = vec!["t2".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let cycles = check_cycles(&graph);
        assert!(!cycles.is_empty());
    }

    #[test]
    fn test_detects_three_node_cycle() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.after = vec!["t3".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];

        let mut t3 = make_task("t3", "Task 3");
        t3.after = vec!["t2".to_string()];

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

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let orphans = check_orphans(&graph);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_detects_orphan_after() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.after = vec!["nonexistent".to_string()];

        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].to, "nonexistent");
        assert_eq!(orphans[0].relation, "after");
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
        t1.after = vec!["t2".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let result = check_all(&graph);
        assert!(result.ok);
        assert!(!result.cycles.is_empty());
    }

    // --- Orphan detection tests for blocks, requires, and edge cases ---

    use crate::graph::Resource;

    #[test]
    fn test_detects_orphan_blocks() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.before = vec!["nonexistent".to_string()];

        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].from, "t1");
        assert_eq!(orphans[0].to, "nonexistent");
        assert_eq!(orphans[0].relation, "before");
    }

    #[test]
    fn test_no_orphan_blocks_with_valid_ref() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.before = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let orphans = check_orphans(&graph);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_detects_orphan_requires() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.requires = vec!["nonexistent-resource".to_string()];

        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].from, "t1");
        assert_eq!(orphans[0].to, "nonexistent-resource");
        assert_eq!(orphans[0].relation, "requires");
    }

    #[test]
    fn test_no_orphan_requires_with_valid_resource() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.requires = vec!["gpu".to_string()];

        let resource = Resource {
            id: "gpu".to_string(),
            name: Some("GPU Compute".to_string()),
            resource_type: Some("compute".to_string()),
            available: Some(4.0),
            unit: Some("GPUs".to_string()),
        };

        graph.add_node(Node::Task(task));
        graph.add_node(Node::Resource(resource));

        let orphans = check_orphans(&graph);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_requires_task_id_is_orphan() {
        // requires uses get_resource, so a task ID in requires is an orphan
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.requires = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].from, "t2");
        assert_eq!(orphans[0].to, "t1");
        assert_eq!(orphans[0].relation, "requires");
    }

    #[test]
    fn test_multiple_orphans_in_same_task() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.after = vec!["ghost-a".to_string()];
        task.before = vec!["ghost-b".to_string()];
        task.requires = vec!["ghost-resource".to_string()];

        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 3);

        let relations: Vec<&str> = orphans.iter().map(|o| o.relation.as_str()).collect();
        assert!(relations.contains(&"after"));
        assert!(relations.contains(&"before"));
        assert!(relations.contains(&"requires"));

        // All orphans come from t1
        assert!(orphans.iter().all(|o| o.from == "t1"));
    }

    #[test]
    fn test_bidirectional_orphans() {
        // t1 blocks nonexistent, t2 after nonexistent — both are orphans
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.before = vec!["phantom".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["phantom".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 2);

        let from_ids: Vec<&str> = orphans.iter().map(|o| o.from.as_str()).collect();
        assert!(from_ids.contains(&"t1"));
        assert!(from_ids.contains(&"t2"));
    }

    #[test]
    fn test_blocks_referencing_resource_is_valid() {
        // blocks uses get_node, so a Resource ID in blocks is NOT an orphan
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.before = vec!["budget".to_string()];

        let resource = Resource {
            id: "budget".to_string(),
            name: Some("Budget".to_string()),
            resource_type: Some("budget".to_string()),
            available: Some(1000.0),
            unit: Some("USD".to_string()),
        };

        graph.add_node(Node::Task(task));
        graph.add_node(Node::Resource(resource));

        let orphans = check_orphans(&graph);
        assert!(orphans.is_empty());
    }

    // --- Stale assignment tests ---

    #[test]
    fn test_no_stale_assignments_in_empty_graph() {
        let graph = WorkGraph::new();
        let stale = check_stale_assignments(&graph);
        assert!(stale.is_empty());
    }

    #[test]
    fn test_no_stale_when_open_and_unassigned() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        let stale = check_stale_assignments(&graph);
        assert!(stale.is_empty());
    }

    #[test]
    fn test_stale_when_open_and_assigned() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.assigned = Some("agent-abc".to_string());
        graph.add_node(Node::Task(t1));

        let stale = check_stale_assignments(&graph);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].task_id, "t1");
        assert_eq!(stale[0].assigned, "agent-abc");
    }

    #[test]
    fn test_no_stale_when_in_progress_and_assigned() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.status = Status::InProgress;
        t1.assigned = Some("agent-abc".to_string());
        graph.add_node(Node::Task(t1));

        let stale = check_stale_assignments(&graph);
        assert!(stale.is_empty());
    }

    #[test]
    fn test_no_stale_when_done_and_assigned() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.status = Status::Done;
        t1.assigned = Some("agent-abc".to_string());
        graph.add_node(Node::Task(t1));

        let stale = check_stale_assignments(&graph);
        assert!(stale.is_empty());
    }

    #[test]
    fn test_stale_assignments_are_warnings_not_errors() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.assigned = Some("agent-abc".to_string());
        graph.add_node(Node::Task(t1));

        let result = check_all(&graph);
        assert!(!result.stale_assignments.is_empty());
        // Stale assignments should not make the graph invalid
        assert!(result.ok);
    }

    // --- Stuck blocked tests ---

    #[test]
    fn test_no_stuck_blocked_in_empty_graph() {
        let graph = WorkGraph::new();
        let stuck = check_stuck_blocked(&graph);
        assert!(stuck.is_empty());
    }

    #[test]
    fn test_stuck_blocked_all_deps_done() {
        let mut graph = WorkGraph::new();
        let mut dep = make_task("dep", "Dependency");
        dep.status = Status::Done;
        let mut blocked = make_task("blocked", "Blocked task");
        blocked.status = Status::Blocked;
        blocked.after = vec!["dep".to_string()];

        graph.add_node(Node::Task(dep));
        graph.add_node(Node::Task(blocked));

        let stuck = check_stuck_blocked(&graph);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].task_id, "blocked");
        assert_eq!(stuck[0].after_ids, vec!["dep".to_string()]);
    }

    #[test]
    fn test_stuck_blocked_mixed_terminal_deps() {
        let mut graph = WorkGraph::new();
        let mut dep1 = make_task("dep1", "Done dep");
        dep1.status = Status::Done;
        let mut dep2 = make_task("dep2", "Failed dep");
        dep2.status = Status::Failed;
        let mut dep3 = make_task("dep3", "Abandoned dep");
        dep3.status = Status::Abandoned;
        let mut blocked = make_task("blocked", "Blocked task");
        blocked.status = Status::Blocked;
        blocked.after = vec!["dep1".to_string(), "dep2".to_string(), "dep3".to_string()];

        graph.add_node(Node::Task(dep1));
        graph.add_node(Node::Task(dep2));
        graph.add_node(Node::Task(dep3));
        graph.add_node(Node::Task(blocked));

        let stuck = check_stuck_blocked(&graph);
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].task_id, "blocked");
    }

    #[test]
    fn test_not_stuck_when_dep_still_open() {
        let mut graph = WorkGraph::new();
        let dep1 = make_task("dep1", "Open dep");
        let mut dep2 = make_task("dep2", "Done dep");
        dep2.status = Status::Done;
        let mut blocked = make_task("blocked", "Blocked task");
        blocked.status = Status::Blocked;
        blocked.after = vec!["dep1".to_string(), "dep2".to_string()];

        graph.add_node(Node::Task(dep1));
        graph.add_node(Node::Task(dep2));
        graph.add_node(Node::Task(blocked));

        let stuck = check_stuck_blocked(&graph);
        assert!(stuck.is_empty());
    }

    #[test]
    fn test_not_stuck_when_status_is_open() {
        let mut graph = WorkGraph::new();
        let mut dep = make_task("dep", "Done dep");
        dep.status = Status::Done;
        let mut task = make_task("task", "Open task");
        task.after = vec!["dep".to_string()];
        // status is Open (default), not Blocked

        graph.add_node(Node::Task(dep));
        graph.add_node(Node::Task(task));

        let stuck = check_stuck_blocked(&graph);
        assert!(stuck.is_empty());
    }

    #[test]
    fn test_stuck_blocked_are_warnings_not_errors() {
        let mut graph = WorkGraph::new();
        let mut dep = make_task("dep", "Done dep");
        dep.status = Status::Done;
        let mut blocked = make_task("blocked", "Blocked task");
        blocked.status = Status::Blocked;
        blocked.after = vec!["dep".to_string()];

        graph.add_node(Node::Task(dep));
        graph.add_node(Node::Task(blocked));

        let result = check_all(&graph);
        assert!(!result.stuck_blocked.is_empty());
        // Stuck blocked should not make the graph invalid
        assert!(result.ok);
    }
}
