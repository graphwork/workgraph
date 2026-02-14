use crate::graph::{LoopGuard, WorkGraph};
use std::collections::HashSet;

/// Result of checking the graph for issues
#[derive(Debug, Clone, Default)]
pub struct CheckResult {
    pub cycles: Vec<Vec<String>>,
    pub orphan_refs: Vec<OrphanRef>,
    pub loop_edge_issues: Vec<LoopEdgeIssue>,
    pub ok: bool,
}

/// A reference to a non-existent node
#[derive(Debug, Clone)]
pub struct OrphanRef {
    pub from: String,
    pub to: String,
    pub relation: String,
}

/// An issue with a loop edge
#[derive(Debug, Clone)]
pub struct LoopEdgeIssue {
    pub from: String,
    pub target: String,
    pub kind: LoopEdgeIssueKind,
}

/// Types of loop edge issues
#[derive(Debug, Clone, PartialEq)]
pub enum LoopEdgeIssueKind {
    /// Target task does not exist
    TargetNotFound,
    /// max_iterations is 0 (loop will never fire)
    ZeroMaxIterations,
    /// Guard references a task that does not exist
    GuardTaskNotFound(String),
    /// Self-loop: a task loops_to itself (immediate re-open on done)
    SelfLoop,
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

/// Check loop edges for validity and safety issues
pub fn check_loop_edges(graph: &WorkGraph) -> Vec<LoopEdgeIssue> {
    let mut issues = Vec::new();

    for task in graph.tasks() {
        for edge in &task.loops_to {
            // Self-loop: a task cannot loops_to itself
            if edge.target == task.id {
                issues.push(LoopEdgeIssue {
                    from: task.id.clone(),
                    target: edge.target.clone(),
                    kind: LoopEdgeIssueKind::SelfLoop,
                });
            }

            // Target must exist
            if graph.get_task(&edge.target).is_none() {
                issues.push(LoopEdgeIssue {
                    from: task.id.clone(),
                    target: edge.target.clone(),
                    kind: LoopEdgeIssueKind::TargetNotFound,
                });
            }

            // max_iterations must be > 0
            if edge.max_iterations == 0 {
                issues.push(LoopEdgeIssue {
                    from: task.id.clone(),
                    target: edge.target.clone(),
                    kind: LoopEdgeIssueKind::ZeroMaxIterations,
                });
            }

            // Guard task references must exist
            if let Some(LoopGuard::TaskStatus {
                task: guard_task, ..
            }) = &edge.guard
            {
                if graph.get_task(guard_task).is_none() {
                    issues.push(LoopEdgeIssue {
                        from: task.id.clone(),
                        target: edge.target.clone(),
                        kind: LoopEdgeIssueKind::GuardTaskNotFound(guard_task.clone()),
                    });
                }
            }
        }
    }

    issues
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
    let loop_edge_issues = check_loop_edges(graph);

    // loops_to cycles are INTENTIONAL — only blocked_by cycles and orphan refs
    // and loop edge issues make the graph invalid
    let ok = cycles.is_empty() && orphan_refs.is_empty() && loop_edge_issues.is_empty();

    CheckResult {
        cycles,
        orphan_refs,
        loop_edge_issues,
        ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Node, Status, Task};

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
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
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

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

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

    // --- Loop edge validation tests ---

    use crate::graph::LoopEdge;

    #[test]
    fn test_no_loop_issues_for_valid_edges() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let issues = check_loop_edges(&graph);
        assert!(issues.is_empty());
    }

    #[test]
    fn test_loop_target_not_found() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.loops_to = vec![LoopEdge {
            target: "nonexistent".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));

        let issues = check_loop_edges(&graph);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, LoopEdgeIssueKind::TargetNotFound);
    }

    #[test]
    fn test_loop_zero_max_iterations() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: None,
            max_iterations: 0,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let issues = check_loop_edges(&graph);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, LoopEdgeIssueKind::ZeroMaxIterations);
    }

    #[test]
    fn test_loop_guard_task_not_found() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: Some(crate::graph::LoopGuard::TaskStatus {
                task: "nonexistent".to_string(),
                status: Status::Done,
            }),
            max_iterations: 3,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let issues = check_loop_edges(&graph);
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0].kind,
            LoopEdgeIssueKind::GuardTaskNotFound("nonexistent".to_string())
        );
    }

    #[test]
    fn test_loop_self_loop() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));

        let issues = check_loop_edges(&graph);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, LoopEdgeIssueKind::SelfLoop);
    }

    #[test]
    fn test_loop_multiple_issues_same_edge() {
        let mut graph = WorkGraph::new();
        // Self-loop with zero max_iterations — should report both issues
        let mut t1 = make_task("t1", "Task 1");
        t1.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: None,
            max_iterations: 0,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));

        let issues = check_loop_edges(&graph);
        assert_eq!(issues.len(), 2);
        let kinds: Vec<&LoopEdgeIssueKind> = issues.iter().map(|i| &i.kind).collect();
        assert!(kinds.contains(&&LoopEdgeIssueKind::SelfLoop));
        assert!(kinds.contains(&&LoopEdgeIssueKind::ZeroMaxIterations));
    }

    #[test]
    fn test_check_all_ok_with_valid_loop_edges() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let result = check_all(&graph);
        assert!(result.ok);
        assert!(result.loop_edge_issues.is_empty());
    }

    #[test]
    fn test_check_all_not_ok_with_loop_edge_issues() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.loops_to = vec![LoopEdge {
            target: "nonexistent".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));

        let result = check_all(&graph);
        assert!(!result.ok);
        assert!(!result.loop_edge_issues.is_empty());
    }

    // --- Orphan detection tests for blocks, requires, and edge cases ---

    use crate::graph::Resource;

    #[test]
    fn test_detects_orphan_blocks() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.blocks = vec!["nonexistent".to_string()];

        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].from, "t1");
        assert_eq!(orphans[0].to, "nonexistent");
        assert_eq!(orphans[0].relation, "blocks");
    }

    #[test]
    fn test_no_orphan_blocks_with_valid_ref() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocks = vec!["t1".to_string()];

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
        task.blocked_by = vec!["ghost-a".to_string()];
        task.blocks = vec!["ghost-b".to_string()];
        task.requires = vec!["ghost-resource".to_string()];

        graph.add_node(Node::Task(task));

        let orphans = check_orphans(&graph);
        assert_eq!(orphans.len(), 3);

        let relations: Vec<&str> = orphans.iter().map(|o| o.relation.as_str()).collect();
        assert!(relations.contains(&"blocked_by"));
        assert!(relations.contains(&"blocks"));
        assert!(relations.contains(&"requires"));

        // All orphans come from t1
        assert!(orphans.iter().all(|o| o.from == "t1"));
    }

    #[test]
    fn test_bidirectional_orphans() {
        // t1 blocks nonexistent, t2 blocked_by nonexistent — both are orphans
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.blocks = vec!["phantom".to_string()];

        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["phantom".to_string()];

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
        task.blocks = vec!["budget".to_string()];

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

    #[test]
    fn test_valid_guard_task_reference() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let t2 = make_task("t2", "Task 2");
        let mut t3 = make_task("t3", "Task 3");
        t3.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: Some(crate::graph::LoopGuard::TaskStatus {
                task: "t2".to_string(),
                status: Status::Done,
            }),
            max_iterations: 5,
            delay: None,
        }];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let issues = check_loop_edges(&graph);
        assert!(issues.is_empty());
    }
}
